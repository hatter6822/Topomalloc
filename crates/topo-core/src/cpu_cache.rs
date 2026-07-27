// SPDX-License-Identifier: MIT
//! Per-CPU cache with locked mode (W6-4, plan 05).
//!
//! Each logical CPU has a set of per-size-class slots holding object addresses.
//! This is the RSEQ-free correct baseline: every operation acquires a per-CPU
//! spinlock before touching the slot, so correctness is trivially serialized.
//! Plan 06 (W7) replaces the lock with an RSEQ critical section for the fast
//! path; this module remains the fallback for platforms without RSEQ.
//!
//! **Structure.** `MAX_CPUS` [`PerCpu`] entries, each containing a spinlock and
//! `NUM_SIZE_CLASSES` [`CpuSlot`]s. A slot holds a metadata-allocated array of
//! `usize` addresses (lazily initialized on first use) plus a length, a soft
//! capacity, a hard capacity, and miss/overflow counters.
//!
//! **Lock ordering (SS27.2).** The per-CPU lock is the *outermost* lock in the
//! cache hierarchy. It serializes all operations on that CPU's slots. The
//! transfer lock (rank 3) and central lock (rank 4) are never held while the
//! per-CPU lock is held; the `cache_ops` module enforces hand-over-hand.
//!
//! **Hard capacity invariant (SS11.5).** A slot's `len` never exceeds its
//! `hard_capacity`; push operations that would breach it return
//! [`FeOutcome::Full`].

use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};

use topo_arch::rseq;

use crate::bootstrap::MetadataAlloc;
use crate::fe::{CoreId, FeOutcome};
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::{ArenaId, SizeClassId};
use crate::lock::{LockRank, RankedLock};
use crate::size_class;

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

/// Maximum logical CPUs supported. This bounds the `CpuCache` array size.
pub const MAX_CPUS: usize = 128;

/// Bound on RSEQ abort retries before the fast path falls back to the locked
/// path (W7). Aborts are rare (only on real preemption/migration), so the loop
/// almost always exits on the first iteration; the bound only prevents a
/// pathological livelock under extreme scheduler churn.
const RSEQ_ABORT_RETRY: u32 = 128;

/// Front-end fast-path mode (W7). Selected at runtime: `Locked` is the always-
/// correct baseline (W6-4); `Rseq` is the Linux restartable fast path (W7-2/3);
/// `PinnedCore` is the seLe4n pinned-thread per-core path (W7-5). The modes are
/// mutually exclusive deployment choices (Linux uses `Rseq`, seLe4n `PinnedCore`).
const MODE_LOCKED: u8 = 0;
const MODE_RSEQ: u8 = 1;
const MODE_PINNED: u8 = 2;
/// Per-size-class slot within a [`PerCpu`]: a lazily-allocated LIFO stack of
/// object addresses.
///
/// In the **locked** baseline (W6-4) the slot is not self-synchronizing; all
/// access is serialized by the owning [`PerCpu`]'s spinlock. In **RSEQ** mode
/// (W7) the per-architecture restartable sequence reads `len`/`buf`/
/// `soft_capacity` directly, so the layout is `#[repr(C)]` with those fields
/// first and their offsets fed to the assembly via `offset_of!`. The asm does a
/// single committing store of `len`; aligned word access is atomic on x86-64 and
/// AArch64, so it never tears against the `Relaxed` reads here.
#[repr(C)]
pub struct CpuSlot {
    /// Pointer to the metadata-allocated array of `usize` addresses.
    /// Null (0) until initialized — the RSEQ sequence treats null as "take the
    /// locked path" (it cannot allocate the buffer itself).
    buf: AtomicUsize,
    /// Number of valid entries in the buffer. The RSEQ commit store targets this.
    len: AtomicU32,
    /// Soft capacity -- the budget controller may grow or shrink this within
    /// `[batch_size, hard_capacity]` (W6-5). The RSEQ push bounds against it.
    soft_capacity: AtomicU32,
    /// Hard capacity -- the absolute ceiling for this slot (SS11.5).
    hard_capacity: AtomicU32,
    /// Whether the slot has been initialized (buffer allocated).
    initialized: AtomicBool,
    /// Cache miss count (pop from empty slot). Incremented lock-free (Relaxed)
    /// by the fast path; read and reset by the budget controller (W6-5).
    misses: AtomicU64,
    /// Cache overflow count (push to full slot). Incremented lock-free (Relaxed).
    overflows: AtomicU64,
}

impl CpuSlot {
    /// The canonical initial slot. Retained as the **specification** of the all-zero
    /// pattern `CpuCache::ensure_cpus` relies on (see
    /// `a_zeroed_per_cpu_block_is_the_const_initialiser`); the live carve zeroes rather
    /// than materialising a 360 KiB temporary, so nothing constructs one at run time.
    #[cfg(test)]
    const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            buf: AtomicUsize::new(0),
            len: AtomicU32::new(0),
            soft_capacity: AtomicU32::new(0),
            hard_capacity: AtomicU32::new(0),
            misses: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
        }
    }

    /// Whether the slot has been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Current number of addresses in the slot.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    /// Whether the slot is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Soft capacity (the budget controller's target).
    #[inline]
    pub fn soft_capacity(&self) -> u32 {
        self.soft_capacity.load(Ordering::Relaxed)
    }

    /// Hard capacity (the absolute ceiling, SS11.5).
    #[inline]
    pub fn hard_capacity(&self) -> u32 {
        self.hard_capacity.load(Ordering::Relaxed)
    }

    /// Cache miss count (approximate, lock-free).
    #[inline]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Cache overflow count (approximate, lock-free).
    #[inline]
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Reset the miss counter, returning the old value.
    #[inline]
    pub fn reset_misses(&self) -> u64 {
        self.misses.swap(0, Ordering::Relaxed)
    }

    /// Reset the overflow counter, returning the old value.
    #[inline]
    pub fn reset_overflows(&self) -> u64 {
        self.overflows.swap(0, Ordering::Relaxed)
    }

    /// Set the soft capacity. Clamped to `[1, hard_capacity]` to maintain the
    /// buffer bounds invariant (`cur_len < soft_capacity <= hard_capacity`).
    #[inline]
    pub fn set_soft_capacity(&self, cap: u32) {
        let hard = self.hard_capacity.load(Ordering::Relaxed);
        self.soft_capacity
            .store(cap.min(hard).max(1), Ordering::Relaxed);
    }

    /// Appendix B.2 (cache invariants, W19-1b): this slot is within its capacity
    /// bounds (§11.5). Total + side-effect-free; the `debug-checks`/test oracle
    /// for one per-CPU slot. An **uninitialized** slot is vacuously well-formed
    /// (it holds nothing). For an initialized slot:
    ///
    /// * `len <= hard_capacity` — the §11.5 absolute ceiling is never breached;
    /// * `1 <= soft_capacity <= hard_capacity` — the budget controller (W6-5)
    ///   keeps the soft target inside the legal window, so a push always has a
    ///   meaningful bound and the buffer (sized at `hard_capacity`) never tears;
    /// * `hard_capacity == max_local_capacity(sc)` — the geometry the slot was
    ///   initialized with, so a corrupted ceiling (which would mis-size the
    ///   buffer-bounds reasoning) is caught.
    ///
    /// Reads are lock-free relaxed snapshots; exact when the slot is quiescent.
    pub fn check_invariants(&self, sc: SizeClassId) -> bool {
        if !self.is_initialized() {
            return true;
        }
        let len = self.len();
        let soft = self.soft_capacity();
        let hard = self.hard_capacity();
        len <= hard
            && soft >= 1
            && soft <= hard
            && hard == size_class::max_local_capacity(sc) as u32
    }

    /// Initialize the slot: allocate the address buffer from `meta`.
    /// Must be called under the per-CPU lock. Returns `false` on metadata
    /// exhaustion.
    fn init(&self, meta: &dyn MetadataAlloc, hard_cap: u32, soft_cap: u32) -> bool {
        if self.is_initialized() {
            return true;
        }
        let cap = hard_cap as usize;
        let bytes = match cap.checked_mul(core::mem::size_of::<usize>()) {
            Some(b) if b > 0 => b,
            _ => return false,
        };
        let ptr = match meta.alloc(bytes, core::mem::align_of::<usize>()) {
            Some(p) => p,
            None => return false,
        };
        // SAFETY: ptr is a fresh, exclusively-owned region of `bytes` bytes from
        // MetadataAlloc. Zeroing yields valid `usize(0)` entries.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0, bytes) };
        self.buf.store(ptr.as_ptr() as usize, Ordering::Release);
        self.hard_capacity.store(hard_cap, Ordering::Release);
        // Clamp soft_cap: must be in [1, hard_cap] to guarantee the buffer
        // bounds invariant (cur_len < soft_cap <= hard_cap).
        let clamped_soft = soft_cap.min(hard_cap).max(1);
        self.soft_capacity.store(clamped_soft, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
        true
    }

    /// Returns the buffer as a raw pointer to `usize` elements. Only valid
    /// when initialized and under the per-CPU lock.
    #[inline]
    fn buf_ptr(&self) -> *mut usize {
        self.buf.load(Ordering::Acquire) as *mut usize
    }

    /// Appendix B.2 (W19-1b) distinctness: the `len` cached addresses are pairwise
    /// **distinct** and non-null. A duplicate is a double-listed (double-freed)
    /// object (§29.3, S-009); catching it here pins the corruption to the exact
    /// CPU/slot, earlier than the central double-insert check at flush time.
    ///
    /// # Safety
    /// The caller MUST hold the owning per-CPU lock (so this thread is the sole
    /// accessor of `buf`/`len`). O(len²), debug-cadence. The scan is bounded by the
    /// **buffer capacity**, not the (possibly-corrupted) `len`, so it can never read
    /// past the `hard_capacity`-sized buffer even if `len` has been corrupted past
    /// it — the capacity overshoot is caught separately by [`check_invariants`].
    unsafe fn entries_distinct(&self) -> bool {
        if !self.is_initialized() {
            return true;
        }
        // Defensive bound: never index past the allocated buffer (`hard_capacity`
        // elements), regardless of what `len` claims. In the well-formed case
        // `len <= hard_capacity`, so this scans every resident object.
        let len = (self.len() as usize).min(self.hard_capacity() as usize);
        let buf = self.buf_ptr();
        for i in 0..len {
            // SAFETY: `i < len <= hard_capacity`; `buf` is a valid array of
            // `hard_capacity` `usize` from `MetadataAlloc` (never freed); the
            // per-CPU lock is held by the caller, so there is no concurrent writer.
            let a = unsafe { *buf.add(i) };
            if a == 0 {
                return false;
            }
            for j in (i + 1)..len {
                // SAFETY: `j < len <= hard_capacity`, as above.
                if a == unsafe { *buf.add(j) } {
                    return false;
                }
            }
        }
        true
    }

    /// Visit every address this slot currently holds (W19-1b residency cross-check).
    ///
    /// # Safety
    /// The caller MUST hold the owning per-CPU lock. The scan is bounded by the **buffer
    /// capacity**, not by the (possibly-corrupted) `len`, so it can never read past the
    /// `hard_capacity`-sized buffer.
    pub(crate) unsafe fn for_each_entry(&self, mut f: impl FnMut(usize)) {
        if !self.is_initialized() {
            return;
        }
        let len = (self.len() as usize).min(self.hard_capacity() as usize);
        let buf = self.buf_ptr();
        if buf.is_null() {
            return;
        }
        for i in 0..len {
            // SAFETY: `i < len <= hard_capacity`; `buf` is a valid array of
            // `hard_capacity` `usize` from `MetadataAlloc` (never freed) and the caller
            // holds the per-CPU lock, so there is no concurrent writer.
            f(unsafe { *buf.add(i) });
        }
    }

    /// Test-only: force `len` past the capacity bound, to construct a
    /// deliberately-inconsistent slot and prove
    /// [`check_invariants`](Self::check_invariants) catches it (W19-1 negative
    /// test). Never compiled into a shipping build.
    #[cfg(test)]
    pub(crate) fn corrupt_len_for_test(&self, n: u32) {
        self.len.store(n, Ordering::Relaxed);
    }
}

/// Per-CPU state: a spinlock and per-size-class slots.
///
/// `#[repr(C)]` with `locked` first (offset 0): the RSEQ sequence reads the lock
/// byte at `&cpus[cpu]` to divert to the locked path when a non-owner holds it
/// (W7-4), and indexes `slots` by the kernel-reported CPU. The offsets and the
/// `size_of::<PerCpu>()` stride are fed to the assembly via `offset_of!`. The lock
/// is a [`RankedLock`] at rank [`LockRank::FRONT_END`] (the outermost data-path
/// lock, §27.2), routed through the W16-1b checker; it is a single-`AtomicBool`
/// struct, so the lock byte stays at offset 0 exactly as the assembly requires.
/// The RSEQ fast path peeks that byte directly (no checker); only the locked
/// baseline / non-owner drain take it through the per-CPU `lock()`.
#[repr(C)]
pub struct PerCpu {
    /// Spinlock protecting all slots for this CPU. Offset 0 (the RSEQ lock byte).
    locked: RankedLock<{ LockRank::FRONT_END }>,
    /// Per-size-class slots.
    slots: [CpuSlot; NUM_SIZE_CLASSES],
}

impl PerCpu {
    /// The canonical initial per-CPU entry — see [`CpuSlot::new`] for why it is
    /// test-only.
    #[cfg(test)]
    const fn new() -> Self {
        Self {
            locked: RankedLock::new(),
            slots: [const { CpuSlot::new() }; NUM_SIZE_CLASSES],
        }
    }

    /// Acquire the per-CPU lock (rank `FRONT_END`, routed through the checker).
    #[inline]
    pub(crate) fn lock(&self) -> CpuGuard<'_> {
        self.locked.acquire();
        CpuGuard { cpu: self }
    }

    /// The slot for a size class (bounds-checked).
    #[inline]
    pub fn slot(&self, sc: SizeClassId) -> Option<&CpuSlot> {
        self.slots.get(sc.index())
    }
}

/// RAII guard for a locked [`PerCpu`].
pub(crate) struct CpuGuard<'a> {
    cpu: &'a PerCpu,
}

impl Drop for CpuGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.cpu.locked.release();
    }
}

// SAFETY: all shared state in CpuSlot is behind atomics. The raw pointer (buf)
// reaches monotonic metadata (never freed, MetadataAlloc contract), and is only
// dereferenced under the per-CPU lock.
unsafe impl Sync for CpuSlot {}
// SAFETY: CpuSlot's interior state is atomics; the buffer pointer reaches
// monotonic metadata that is never freed.
unsafe impl Send for CpuSlot {}

// SAFETY: all shared state in PerCpu is behind atomics or the spinlock.
unsafe impl Sync for PerCpu {}
// SAFETY: PerCpu's fields are atomics and a spinlock.
unsafe impl Send for PerCpu {}

// ---------------------------------------------------------------------------
// RSEQ layout constants (W7). These describe the per-CPU array to the
// per-architecture assembly: a slot's `len`/`buf`/`soft_capacity` offsets, the
// per-CPU stride, and the offset of the `slots` array. `locked` must be at
// offset 0 so `&cpus[cpu]` is both the lock byte and the per-CPU base.

const SLOT_LEN_OFF: usize = core::mem::offset_of!(CpuSlot, len);
const SLOT_BUF_OFF: usize = core::mem::offset_of!(CpuSlot, buf);
const SLOT_CAP_OFF: usize = core::mem::offset_of!(CpuSlot, soft_capacity);
const SLOT_STRIDE: usize = core::mem::size_of::<CpuSlot>();
const PERCPU_STRIDE: usize = core::mem::size_of::<PerCpu>();
const PERCPU_SLOTS_OFF: usize = core::mem::offset_of!(PerCpu, slots);

const _: () = {
    // The RSEQ lock-byte / per-CPU base coincide only if `locked` is first.
    assert!(core::mem::offset_of!(PerCpu, locked) == 0);
    // The committing store writes a `u32` length; the buffer pointer is a word.
    assert!(SLOT_BUF_OFF + core::mem::size_of::<usize>() <= SLOT_STRIDE);
    assert!(SLOT_LEN_OFF + core::mem::size_of::<u32>() <= SLOT_STRIDE);
    assert!(SLOT_CAP_OFF + core::mem::size_of::<u32>() <= SLOT_STRIDE);
};

/// Pop up to `max` (and `out.len()`) addresses off the top of an **already-locked**
/// slot into `out`, returning the count. The single point that moves objects out
/// of a slot under the per-CPU lock; shared by `pop_batch` and `drain_cpu`.
///
/// # Safety
/// The caller must hold the slot's per-CPU lock (so this thread is the sole
/// accessor) and have run the RSEQ fence for a non-owner drain.
unsafe fn pop_slot(slot: &CpuSlot, out: &mut [usize], max: usize) -> usize {
    let cur_len = slot.len.load(Ordering::Relaxed) as usize;
    if cur_len == 0 {
        return 0;
    }
    let pop_count = max.min(cur_len).min(out.len());
    if pop_count == 0 {
        return 0;
    }
    let buf = slot.buf_ptr();
    let new_len = cur_len - pop_count;
    for (i, dst) in out[..pop_count].iter_mut().enumerate() {
        // SAFETY: `buf` points to a valid array of `hard_capacity` `usize`s;
        // `new_len + i < cur_len <= hard_capacity`. The lock is held.
        *dst = unsafe { *buf.add(new_len + i) };
    }
    slot.len.store(new_len as u32, Ordering::Relaxed);
    pop_count
}

/// Adapts a `fn() -> i32` core oracle to the [`CoreProvider`](crate::pinned::CoreProvider)
/// trait for the pinned-mode dispatch (W7-5). A returned `-1` (unknown core) maps
/// to a sentinel that cannot equal any in-range expected core, so the pinned
/// sequence aborts with no change rather than committing to a wrong slot.
struct FnCoreProvider(fn() -> i32);

impl crate::pinned::CoreProvider for FnCoreProvider {
    #[inline]
    fn current_core(&self) -> CoreId {
        let c = (self.0)();
        if c < 0 {
            CoreId(u32::MAX)
        } else {
            CoreId(c as u32)
        }
    }
}

/// The per-CPU cache: `MAX_CPUS` [`PerCpu`] entries (W6-4).
///
/// Thread-safe by construction: each CPU has its own spinlock. The fast-path
/// operations `fe_pop` and `fe_push` lock the target CPU, operate on the slot,
/// and unlock -- the transfer and central locks are never held.
pub struct CpuCache {
    /// The `MAX_CPUS`-entry per-CPU array, carved from monotonic metadata on first
    /// use and never freed; null until then.
    ///
    /// **Not inline.** The array is ~360 KiB (`MAX_CPUS` × [`PerCpu`], each holding a
    /// slot per size class), which is far too large to sit by value inside an engine
    /// that is constructed and moved as a value, and is pure waste for a program that
    /// never touches the front end (a `no_std` profile, an arena-only embedding, a test
    /// that exercises only the central path). Carving it lazily makes the cost
    /// proportional to use, exactly as the span/large descriptor pools and the free
    /// bitmaps already are.
    ///
    /// A failed carve is **not** an error: every accessor treats a null array as "no
    /// slots", so the front end simply declines and the caller falls back to the central
    /// path (§2.4 — a policy layer may degrade, it may never fail an allocation).
    cpus: AtomicPtr<PerCpu>,
    /// Serialises the one-shot array carve so a race cannot leak a second 360 KiB block.
    /// Rank [`LockRank::FRONT_END`], like the per-CPU locks it guards the creation of;
    /// it is always released before any per-CPU lock is taken, so the §27.2 order (which
    /// requires each acquisition be *strictly* rank-increasing) still holds.
    cpus_init: RankedLock<{ LockRank::FRONT_END }>,
    /// Number of active (online) CPUs. Operations on a core beyond this
    /// count are valid but will always miss (no slots initialized).
    active_cpus: AtomicU32,
    /// Front-end fast-path mode (`MODE_LOCKED`/`MODE_RSEQ`/`MODE_PINNED`, W7).
    /// `MODE_LOCKED` is the baseline (W6-4); [`enable_rseq`](Self::enable_rseq)
    /// and [`enable_pinned_core`](Self::enable_pinned_core) select the fast
    /// paths; [`disable_rseq`](Self::disable_rseq) reverts to conservative
    /// (locked) mode (e.g. the child fork handler, §28.1).
    ///
    /// **This is a dispatch selector and nothing more.** It says which path an
    /// operation should take; it is deliberately *not* consulted to decide whether a
    /// non-owner drain must fence — that is [`rseq_ever_enabled`](Self::rseq_ever_enabled).
    /// Keeping those two questions apart is what makes concurrent mode changes
    /// uninteresting: every value of this field dispatches correctly, so racing writers
    /// can produce any interleaving without threatening safety.
    mode: AtomicU8,
    /// Whether the RSEQ fast path has **ever** been enabled in this process — a
    /// monotonic latch, set before `mode` ever becomes `MODE_RSEQ` and never cleared.
    ///
    /// This, not `mode`, is what arms the W7-4 non-owner fence. The question a drainer
    /// actually has to answer is *"could a restartable sequence be in flight?"*, and a
    /// sequence runs only in `MODE_RSEQ`, which is only reachable after this latch is
    /// set. So `false` here is a sound proof that no sequence has ever started, and
    /// `true` is a conservative "fence to be sure".
    ///
    /// Deriving the answer from the *current* mode instead is what produced three
    /// consecutive rounds of concurrency bugs. It made `MODE_LOCKED` mean two different
    /// things — "dispatch to the locked path", which any thread may assert, and "no
    /// sequence is in flight", which only a thread that has fenced may assert — so every
    /// pair of concurrent mode writers had to be reasoned about separately, and each fix
    /// covered one pair while leaving the next. A monotonic latch has no writers to order
    /// against: it is set once, before the fact it describes can become true, so no
    /// interleaving of enable/disable/pinned can make it lie.
    ///
    /// The cost is one membarrier per drained CPU in a process that enabled RSEQ and then
    /// turned it off — a fork child, on a host-driven maintenance path. The common cases
    /// are unchanged: RSEQ on fences exactly as before, and a build that never enabled it
    /// (non-Linux, sanitizer, pinned, locked-baseline) skips exactly as before.
    rseq_ever_enabled: AtomicBool,
    /// Count of W7-4 non-owner fences that were required but could not be issued
    /// (`membarrier` refused — e.g. a thread-local seccomp filter denying the syscall).
    /// Each one caused its locked operation to **decline** rather than risk racing an
    /// in-flight sequence, so this is an observability counter for a degraded-but-safe
    /// condition, not an error tally.
    fence_failures: AtomicU64,
    /// In `MODE_PINNED`, a `fn() -> i32` (cast to `usize`) that returns the
    /// calling thread's current core (the seLe4n runtime's per-core identity, the
    /// analogue of `rseq`'s `cpu_id`), or `-1` if unknown. `0` when unset.
    pinned_core_fn: AtomicUsize,
}

impl CpuCache {
    /// A fresh, empty CPU cache (no slots initialized), in the locked baseline
    /// mode. Call [`enable_rseq`](Self::enable_rseq) (Linux) or
    /// [`enable_pinned_core`](Self::enable_pinned_core) (seLe4n) to opt into a
    /// fast path after the cache is wired up.
    pub const fn new() -> Self {
        Self {
            cpus: AtomicPtr::new(core::ptr::null_mut()),
            cpus_init: RankedLock::new(),
            active_cpus: AtomicU32::new(0),
            mode: AtomicU8::new(MODE_LOCKED),
            rseq_ever_enabled: AtomicBool::new(false),
            fence_failures: AtomicU64::new(0),
            pinned_core_fn: AtomicUsize::new(0),
        }
    }

    /// The per-CPU array if it has been carved, else `None`. Lock-free (one acquire
    /// load), and the accessor every read-only path uses — a `None` simply means the
    /// front end holds nothing.
    #[inline]
    fn cpus(&self) -> Option<&[PerCpu; MAX_CPUS]> {
        let p = self.cpus.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: a non-null `cpus` was published by `ensure_cpus` with a release store
        // after the block was fully zero-initialised, and it points at `MAX_CPUS`
        // `PerCpu`s in monotonic metadata (never freed), so the reference is valid for
        // the process lifetime and this acquire load synchronises with that release.
        Some(unsafe { &*p.cast::<[PerCpu; MAX_CPUS]>() })
    }

    /// The per-CPU entry for `core`, carving the array from `meta` if this is its first
    /// use. `None` when the carve fails (an exhausted metadata arena) — the caller then
    /// declines to the central path.
    fn cpu_for(&self, core: CoreId, meta: &dyn MetadataAlloc) -> Option<&PerCpu> {
        if let Some(cpus) = self.cpus() {
            return cpus.get(core.index());
        }
        self.ensure_cpus(meta)?.get(core.index())
    }

    /// Carve the per-CPU array from `meta` (idempotent). An all-zero block **is** the
    /// correct initial state — every field of [`PerCpu`] is an atomic whose zero pattern
    /// is its `new()` value (an unlocked [`RankedLock`], an uninitialised slot with a
    /// null buffer and zero length/capacities/counters) — so this zeroes rather than
    /// materialising a 360 KiB temporary, the same discipline `SpanPool::new` uses.
    #[cold]
    fn ensure_cpus(&self, meta: &dyn MetadataAlloc) -> Option<&[PerCpu; MAX_CPUS]> {
        self.cpus_init.acquire();
        let existing = self.cpus.load(Ordering::Acquire);
        if existing.is_null() {
            let bytes = core::mem::size_of::<[PerCpu; MAX_CPUS]>();
            match meta.alloc(bytes, core::mem::align_of::<PerCpu>()) {
                Some(block) => {
                    // SAFETY: `block` is a fresh, exclusively-owned, correctly-aligned
                    // region of exactly `bytes` bytes; zeroing it yields `MAX_CPUS` valid
                    // `PerCpu`s (see the zero-pattern argument above). Nothing else can
                    // observe the block until the release store below publishes it.
                    unsafe { core::ptr::write_bytes(block.as_ptr(), 0, bytes) };
                    self.cpus
                        .store(block.as_ptr().cast::<PerCpu>(), Ordering::Release);
                }
                None => {
                    self.cpus_init.release();
                    return None;
                }
            }
        }
        self.cpus_init.release();
        self.cpus()
    }

    /// Enable the RSEQ fast path if the platform supports it (W7). Idempotent:
    /// detects/initialises RSEQ for the process (registration model + the
    /// non-owner fence) and, on success, switches `fe_pop`/`fe_push` to the
    /// restartable sequences. Returns whether RSEQ mode is now active; on
    /// `false` the cache stays on the correct locked baseline (P-003).
    ///
    /// Each thread that will use the fast path must additionally call
    /// [`register_current_thread`](Self::register_current_thread) at start-up
    /// (§27.6) — a no-op beyond a presence check in glibc mode.
    pub fn enable_rseq(&self) -> bool {
        let ok = rseq::enable();
        if ok {
            // Arm the fence **before** publishing the mode that lets sequences start, so
            // the latch is never behind the fact it describes. Both orderings are release
            // stores; a drainer's acquire load of the latch therefore cannot observe
            // `false` after any sequence has begun.
            self.rseq_ever_enabled.store(true, Ordering::Release);
        }
        let target = if ok { MODE_RSEQ } else { MODE_LOCKED };
        // **Never transition out of `MODE_PINNED` here.** Leaving pinned mode is the one
        // change that needs the §36.10 quiescence obligation — pinned sequences take no
        // per-CPU lock, so publishing any other mode under a running one lets the next
        // operation touch that slot concurrently, and no fence can drain it. That is why
        // it lives in the `unsafe` [`disable_pinned_core`](Self::disable_pinned_core); a
        // safe method must not do it by the back door, and `disable_rseq` already
        // doesn't. Enabling RSEQ on a pinned cache therefore declines and reports the
        // fast path as not-enabled, which is the truth: pinned is still the active path.
        loop {
            let cur = self.mode.load(Ordering::Acquire);
            if cur == MODE_PINNED {
                return false;
            }
            if self
                .mode
                .compare_exchange_weak(cur, target, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        ok
    }

    /// Enable the seLe4n pinned-thread per-core fast path (W7-5, §36.10 option 1).
    /// `current_core` is the runtime's per-core identity oracle: it returns the
    /// calling thread's current core, or `-1` if unknown. `fe_pop`/`fe_push` then
    /// run the pinned restartable sequence (abort-with-no-change on a core
    /// mismatch), falling back to the locked path when the oracle reports an
    /// invalid core.
    ///
    /// **Non-owner coordination in pinned mode is the §36.10 hand-off contract**
    /// (a cache is flushed or made unreachable before core ownership changes) —
    /// *not* the RSEQ membarrier fence. A non-owner [`drain`](Self::drain_cpu) of
    /// an *active* pinned cache is therefore the caller's responsibility to
    /// serialize (the idle-flush path operates on quiesced cores).
    ///
    /// # Safety
    ///
    /// That hand-off contract is a real obligation, and it cannot be checked here, which
    /// is why this is `unsafe`. The pinned sequences deliberately take **no per-CPU
    /// lock** — their soundness rests entirely on the §36.10 guarantee that the pinned
    /// thread is the sole accessor of its core's slot — and the non-owner fence does not
    /// cover them: it is armed for `MODE_RSEQ`/`MODE_DRAINING` only, because a pinned
    /// sequence is ordinary code with no kernel-visible critical section for
    /// `membarrier` to abort. A drain of a core whose pinned thread is running would
    /// therefore read and rewrite that slot's *non-atomic* buffer concurrently with it:
    /// undefined behaviour, and an object lost or double-vended.
    ///
    /// The caller must guarantee, for as long as pinned mode is active, that:
    ///
    /// * each core's slot is accessed by at most one pinned thread (the §36.10 pinning
    ///   contract itself), and
    /// * no [`drain_cpu`](Self::drain_cpu) — including the
    ///   [`flush_front_end_core`](crate::Allocator::flush_front_end_core) /
    ///   [`flush_front_end_all`](crate::Allocator::flush_front_end_all) /
    ///   [`check_invariants`](crate::Allocator::check_invariants) paths that reach it —
    ///   runs against a core whose pinned thread is active. Draining a *quiesced* core
    ///   (ownership already handed off) is exactly what the contract permits.
    ///
    /// A seLe4n runtime can discharge both: it owns core assignment and knows when a
    /// cache has been handed off. A general POSIX embedding that cannot should use the
    /// RSEQ path ([`enable_rseq`](Self::enable_rseq)) or the locked baseline, both of
    /// which are safe against a concurrent drain.
    pub unsafe fn enable_pinned_core(&self, current_core: fn() -> i32) {
        self.pinned_core_fn
            .store(current_core as usize, Ordering::Release);
        self.mode.store(MODE_PINNED, Ordering::Release);
    }

    /// Disable any fast path, reverting to the locked baseline (§28.1 child fork
    /// handler / conservative mode). Already-cached objects are unaffected.
    ///
    /// A plain store, deliberately. Leaving RSEQ mode used to be a two-step transition
    /// with a transitional mode, a fence, and compare-exchange choreography to decide who
    /// was allowed to publish the terminal state — all of it in service of keeping
    /// `MODE_LOCKED` honest, because that value doubled as the licence for a non-owner
    /// drain to skip its fence. That coupling is gone: the fence is armed by a monotonic
    /// "RSEQ was ever enabled" latch, which no mode change can falsify. `mode` now only selects a dispatch path, every value of it
    /// dispatches correctly, and concurrent callers — two disablers, a disabler racing an
    /// enabler, an atfork child racing anything — can interleave in any order without
    /// producing an unsafe state.
    ///
    /// It also cannot block, which matters: this runs from the `pthread_atfork` child
    /// handler, where a child that inherited a transitional state (or a lock) from a
    /// thread that no longer exists would have no way to make progress.
    ///
    /// **It leaves `MODE_PINNED` alone**, and not merely because of the name. Moving
    /// *out* of pinned mode is the one mode change that is genuinely unsafe on its own:
    /// the pinned sequences deliberately ignore the per-CPU lock (§36.10 makes the pinned
    /// thread the slot's sole accessor), so publishing `MODE_LOCKED` under a running
    /// pinned operation lets the very next locked operation take that lock and touch the
    /// slot concurrently — a data race no fence can repair, because pinned mode has no
    /// kernel-visible critical section to abort. Leaving pinned mode therefore carries the
    /// same quiescence obligation as entering it and lives in
    /// [`disable_pinned_core`](Self::disable_pinned_core), which is `unsafe` for exactly
    /// that reason. A fork child inherits pinned mode and is safe by construction: it has
    /// one thread, so no pinned operation can be in flight on another.
    pub fn disable_rseq(&self) {
        // RSEQ → locked only. A failure is a no-op and needs no retry: every mode this
        // can observe already dispatches correctly, and the fence latch is independent of
        // all of them.
        let _ = self.mode.compare_exchange(
            MODE_RSEQ,
            MODE_LOCKED,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    /// Reset the front end for a freshly forked **child** (§28.1): locked baseline, the
    /// "RSEQ has ever run" latch cleared, and the architecture layer's registration
    /// decision dropped.
    ///
    /// Clearing the latch is sound here for a reason that holds nowhere else: `fork`
    /// creates a process with exactly one thread, so no restartable sequence from the
    /// parent can be in flight in the child — the fact the latch records is *provably*
    /// false again, rather than merely believed to be.
    ///
    /// It is also necessary, not just permitted. `fence_rseq` is
    /// `membarrier(PRIVATE_EXPEDITED_RSEQ)`, and membarrier's registration of intent
    /// lives in the mm and is **not inherited across `fork`** — the child's mm starts
    /// unregistered, so the call fails with `EPERM`. A child that inherited the latch as
    /// `true` would therefore fence on every non-owner drain and fail every time, which
    /// the W7-4 `debug_assert` in the non-owner fence
    /// correctly reports as a kernel anomaly — it just is not one here. Re-enabling RSEQ
    /// in the child re-registers the intent and re-arms the latch together, so the two
    /// can never drift apart.
    ///
    /// The same argument applies one layer down, which is why this also calls
    /// [`rseq::reset_after_fork`]: `rseq::enable` is idempotent and would otherwise
    /// short-circuit on the *parent's* decided mode, re-registering neither the
    /// membarrier intent nor the child thread's rseq area. Resetting only this layer
    /// would leave the child able to re-enter RSEQ mode over kernel state that no longer
    /// exists. Both halves of the fork-invalidated RSEQ state are dropped here, together,
    /// so neither can be reset without the other.
    ///
    /// # Safety
    ///
    /// The caller must guarantee this runs in a **quiesced single-threaded context** —
    /// in practice, a `fork` child, from the `pthread_atfork` child handler. Every claim
    /// above rests on "no restartable sequence can be in flight", which only `fork`'s
    /// single-threadedness establishes. Called with an RSEQ operation in flight it
    /// publishes `MODE_LOCKED` *and* clears the very latch that would have made the next
    /// non-owner locked access fence (W7-4), so that access can mutate a slot while the
    /// in-flight sequence commits — losing an object or vending one twice. This is the
    /// same obligation, for the same reason, as
    /// [`disable_pinned_core`](Self::disable_pinned_core); `disable_rseq` is by contrast
    /// safe precisely because it leaves the latch armed.
    pub unsafe fn reset_after_fork(&self) {
        self.mode.store(MODE_LOCKED, Ordering::Release);
        self.rseq_ever_enabled.store(false, Ordering::Release);
        rseq::reset_after_fork();
    }

    /// Leave the seLe4n pinned-thread fast path for the locked baseline (§36.10), the
    /// counterpart to [`enable_pinned_core`](Self::enable_pinned_core).
    ///
    /// # Safety
    ///
    /// The caller must guarantee that **no pinned operation is in flight on any core**
    /// when this is called. A pinned sequence takes no per-CPU lock, so the moment
    /// `MODE_LOCKED` is published the next locked operation may acquire that lock and
    /// access the same non-atomic slot buffer the pinned sequence is still reading or
    /// writing — losing an object or vending one twice. Unlike the RSEQ path there is no
    /// fence that can drain them: `membarrier` aborts kernel-registered critical sections,
    /// and a pinned sequence is ordinary code.
    ///
    /// A seLe4n runtime discharges this the same way it discharges
    /// [`enable_pinned_core`](Self::enable_pinned_core)'s obligation — it owns core
    /// assignment, so it knows when a core's pinned worker is parked.
    pub unsafe fn disable_pinned_core(&self) {
        let _ = self.mode.compare_exchange(
            MODE_PINNED,
            MODE_LOCKED,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    /// Whether the RSEQ fast path is currently active.
    #[inline]
    pub fn rseq_mode(&self) -> bool {
        self.mode.load(Ordering::Acquire) == MODE_RSEQ
    }

    /// Whether the seLe4n pinned-core fast path is currently active.
    #[inline]
    pub fn pinned_mode(&self) -> bool {
        self.mode.load(Ordering::Acquire) == MODE_PINNED
    }

    /// Register the calling thread for the RSEQ fast path (§27.6, W7-1). Returns
    /// whether this thread can use it; in the locked/pinned modes this is a no-op
    /// returning `false` (pinned mode needs no per-thread RSEQ registration).
    #[inline]
    pub fn register_current_thread(&self) -> bool {
        self.rseq_mode() && rseq::register_current_thread()
    }

    /// Set the number of active CPUs.
    #[inline]
    pub fn set_active_cpus(&self, n: u32) {
        self.active_cpus
            .store(n.min(MAX_CPUS as u32), Ordering::Release);
    }

    /// Number of active CPUs.
    #[inline]
    pub fn active_cpus(&self) -> u32 {
        self.active_cpus.load(Ordering::Relaxed)
    }

    /// The [`CoreId`] the calling thread should use for its front-end operations
    /// (W6-4): the **running CPU** when a cheap per-CPU id is readable
    /// (`rseq::current_cpu()`, available once [`enable_rseq`](Self::enable_rseq) has
    /// run), else a stable per-thread spreading key, else core 0.
    ///
    /// **Any** value is *correct* — a slot is just a keyed object pool, and every
    /// cached object of a class is interchangeable, so a thread that migrates (or
    /// shares a core id with another) still pops and pushes valid objects. The choice
    /// only decides *which* slot is touched, i.e. how well the front-end lock spreads;
    /// in [`rseq_mode`](Self::rseq_mode) it is a hint the restartable sequence ignores
    /// in favour of the hardware CPU (§27.4).
    ///
    /// The per-thread fallback matters: without it a build whose `rseq::enable()`
    /// failed (no kernel support, a sanitizer build, a non-Linux target) would land
    /// every thread on core 0 and turn the front end into one process-wide spinlock —
    /// strictly worse than the per-size-class-binned central path it fronts. This is
    /// the same reasoning (and the same golden-ratio key) as the fork gate's shard
    /// selection; see [`crate::fork`].
    ///
    /// **The fallback spreads over the whole array, never over
    /// [`active_cpus`](Self::active_cpus).** `active_cpus` is a *population count*, not
    /// an index bound — the same distinction [`CacheBudget::adapt`](crate::budget::CacheBudget::adapt)
    /// already turns on, since Linux does not promise online CPU ids are dense. Keying
    /// the fallback to it made this mapping **change under the host's feet**: an
    /// embedding that allocates before publishing a count (or that later publishes a
    /// smaller one) spreads over `MAX_CPUS` first, and the publication then re-maps every
    /// thread into `0..cpus`, leaving whatever those threads had cached at indices
    /// `>= cpus` unreachable to ordinary allocation. Those objects were *removed from
    /// central* to get there, so on an exhausted backend the allocator returns null with
    /// reusable objects sitting in slots nothing will look at — a spurious OOM. Draining
    /// the excluded slots at the publication point would only narrow the window (a thread
    /// that sampled the old mapping can push after the drain); making the mapping
    /// independent of the count closes it, and costs nothing: the `[PerCpu; MAX_CPUS]`
    /// array is carved whole either way, and total residency is bounded by the §11.5
    /// *global* budget, which is still sized from the CPU count.
    #[inline]
    pub fn current_core(&self) -> CoreId {
        let cpu = rseq::current_cpu();
        if cpu >= 0 && (cpu as usize) < MAX_CPUS {
            return CoreId(cpu as u32);
        }
        // No per-CPU id: spread by thread instead.
        match crate::fork::shard::thread_key() {
            Some(k) => Self::fallback_core(k),
            None => CoreId::DEFAULT,
        }
    }

    /// The no-RSEQ slot for a thread key.
    ///
    /// **Deliberately an associated function with no `&self`**, so it cannot read
    /// `active_cpus` — or any other mutable allocator state — even by accident. That is
    /// the invariant, not an implementation detail: a slot mapping that moves under a
    /// host's feet orphans everything the previous mapping cached (see
    /// [`current_core`](Self::current_core)), and the type signature is what keeps it
    /// from coming back.
    #[inline]
    #[must_use]
    pub fn fallback_core(key: usize) -> CoreId {
        CoreId((key % MAX_CPUS) as u32)
    }

    /// The per-CPU entry for a core (bounds-checked).
    #[inline]
    pub fn per_cpu(&self, core: CoreId) -> Option<&PerCpu> {
        self.cpus()?.get(core.index())
    }

    /// Appendix B.2 (cache invariants, W19-1b): **every** per-CPU slot is within
    /// its capacity bounds (§11.5). Total + side-effect-free, O(`MAX_CPUS` × sc);
    /// the `debug-checks`/test oracle for the per-CPU front-end, and the runtime
    /// counterpart of the "per-CPU cache counts do not exceed capacity / bytes do
    /// not exceed the hard budget" Appendix-B clauses (each slot's bytes are
    /// bounded by `len × object_size <= hard_capacity × object_size`).
    ///
    /// Iterates **all** `MAX_CPUS`, not just the active count, so a slot
    /// initialized on a core that later went offline is still checked. Acquires
    /// each per-CPU lock (rank `FRONT_END`, the outermost, so taking it first from
    /// an unlocked context respects the §27.2 order) for a consistent snapshot of
    /// the slot buffers, then is read-only — total + side-effect-free. The
    /// capacity bounds plus the per-slot **distinctness** check (a duplicate is a
    /// double-freed object, §29.3) are the B.2 per-CPU clauses.
    pub fn check_invariants(&self) -> bool {
        let Some(cpus) = self.cpus() else {
            return true; // never used ⇒ nothing to violate
        };
        for (cpu_idx, cpu) in cpus.iter().enumerate() {
            let _g = cpu.lock();
            // W7-4: the sweep reads another CPU's slots, so drain any in-flight sequence
            // there before reading `len`/`buf` — otherwise the checker can observe a
            // half-committed sequence and report a spurious violation.
            if !self.fence_if_non_owner(CoreId(cpu_idx as u32)) {
                // Cannot drain this CPU's in-flight sequences, so its slots cannot be
                // read consistently. Skipping is right for a checker: reporting a
                // violation it cannot substantiate would be a false alarm.
                continue;
            }
            for (sc_idx, slot) in cpu.slots.iter().enumerate() {
                if !slot.check_invariants(SizeClassId::new(sc_idx)) {
                    return false;
                }
                // SAFETY: the per-CPU lock (`_g`) is held for this CPU's slots.
                if !unsafe { slot.entries_distinct() } {
                    return false;
                }
            }
        }
        true
    }

    /// Initialize a slot for `(core, sc)` with the given initial capacity.
    /// The hard capacity is `max_local_capacity` for the size class. The
    /// soft capacity starts at `initial_soft_cap` (typically `batch_size`).
    pub fn init_slot(
        &self,
        core: CoreId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
        initial_soft_cap: u32,
    ) -> bool {
        let cpu = match self.cpu_for(core, meta) {
            Some(c) => c,
            None => return false,
        };
        let _guard = cpu.lock();
        // W7-4: as the locked pop/push — publishing a slot buffer for a CPU the caller
        // may not be running on must drain any in-flight sequence on that CPU first.
        if !self.fence_if_non_owner(core) {
            return false; // cannot drain the target CPU: do not publish into its slot
        }
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return false,
        };
        let hard_cap = size_class::max_local_capacity(sc) as u32;
        slot.init(meta, hard_cap, initial_soft_cap)
    }

    /// Pop an address from the per-CPU slot for `(core, arena, sc)`.
    ///
    /// Returns `FeOutcome::Success(addr)` on success, `FeOutcome::Empty` if
    /// the slot is empty (needs refill). The slot is lazily initialized on
    /// first use via `meta`.
    ///
    /// In RSEQ mode (W7) this first attempts the lock-free restartable sequence
    /// on the **current** CPU's slot (the `core` argument is a hint there, the
    /// hardware CPU is authoritative); on `Abort` it retries, and on a genuine
    /// `Empty` it returns. Any condition the sequence cannot handle (lock held
    /// by a non-owner, an uninitialised slot, or the abort bound) falls through
    /// to the locked path, which is behaviourally identical (proven by the
    /// forced-migration equivalence tests, G-fast).
    #[inline]
    pub fn fe_pop(
        &self,
        core: CoreId,
        arena: ArenaId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<usize> {
        self.fe_pop_tracked(core, arena, sc, meta).0
    }

    /// [`fe_pop`](Self::fe_pop), additionally reporting **which core's slot it actually
    /// operated on** — the authoritative answer, resolved inside the pop rather than
    /// guessed by the caller beforehand — or `None` if it touched no slot at all.
    ///
    /// A caller that reacts to `Empty` by refilling needs this. `core` is only a *hint*
    /// in RSEQ mode (§27.4): the sequence ignores it and uses the hardware CPU, so a
    /// thread that migrated between the caller's `current_core()` and the pop pops from
    /// its new CPU while a refill keyed to the hint fills the old one. That refill
    /// *moves* objects — central loses a batch, a slot the caller will never read gains
    /// it — so on an exhausted backend the allocation can report OOM with free objects
    /// parked on another core. Refilling the core reported here keeps the two halves
    /// keyed to the same CPU, so a migration costs at most a retry instead of a spurious
    /// failure. (Migrating again after this read is harmless: the objects land in a real
    /// slot either way, and the caller's loop is bounded.)
    ///
    /// **`None` means "the front end declined; do not follow up on any core"** — see the
    /// `MODE_PINNED` arm. A caller must not substitute a hint of its own: the whole point
    /// of the answer being `None` is that no core is safe to touch.
    #[inline]
    pub fn fe_pop_tracked(
        &self,
        core: CoreId,
        arena: ArenaId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
    ) -> (FeOutcome<usize>, Option<CoreId>) {
        match self.mode.load(Ordering::Acquire) {
            MODE_RSEQ => {
                if let Some((out, served)) = self.fe_pop_rseq(sc) {
                    return (out, Some(served));
                }
                let eff = self.effective_core(core);
                (self.fe_pop_locked(eff, arena, sc, meta), Some(eff))
            }
            // **In pinned mode the pinned sequence is the only thing that may touch a
            // slot, so a declined dispatch declines the front end** (§2.4) rather than
            // falling back to a locked access keyed on `core`.
            //
            // `core` comes from [`current_core`](Self::current_core), which knows nothing
            // about the pinned oracle: with no RSEQ it is a per-thread spreading key (core
            // 0 in the `no_std` build), so it names an essentially arbitrary slot — one
            // that under the §36.10 contract belongs to *another* core's pinned worker.
            // That worker takes no per-CPU lock and no fence can drain it, so the locked
            // fallback would read and rewrite its non-atomic slot buffer concurrently:
            // undefined behaviour, and an object lost or double-vended. The allocator must
            // itself honour the contract [`enable_pinned_core`](Self::enable_pinned_core)
            // asks its caller to uphold.
            //
            // Nothing is lost by declining. The dispatch only gives up when the oracle
            // cannot name this thread's core or the thread keeps migrating — exactly the
            // states in which guessing is unsound — and the lazy slot init lives inside
            // `fe_pop_pinned` itself, so it is not what the locked path was there for.
            // The caller serves from central instead.
            MODE_PINNED => match self.fe_pop_pinned_dispatch(arena, sc, meta) {
                Some((out, served)) => (out, Some(served)),
                None => (FeOutcome::Empty, None),
            },
            _ => (self.fe_pop_locked(core, arena, sc, meta), Some(core)),
        }
    }

    /// Pop from **a specific core's slot**, whatever core this thread is running on.
    ///
    /// The ordinary [`fe_pop`](Self::fe_pop) cannot do this: in RSEQ mode the hardware
    /// CPU is authoritative and the argument is only a hint, by design. This takes the
    /// locked path unconditionally, so the core argument is honoured — which is what a
    /// caller needs when it must reach objects it *itself* placed in a known slot and has
    /// since migrated away from, rather than whatever the current CPU happens to hold.
    ///
    /// Safe against a concurrent RSEQ sequence on that core for the usual reason
    /// (`fe_pop_locked` issues the W7-4 non-owner fence before touching the slot), so
    /// this is the same discipline drains and flushes already use — not a new hazard.
    #[inline]
    pub fn fe_pop_on_core(
        &self,
        core: CoreId,
        arena: ArenaId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<usize> {
        self.fe_pop_locked(core, arena, sc, meta)
    }

    /// The locked (spinlock) pop — the RSEQ-free correct baseline (W6-4) and the
    /// RSEQ-mode fallback.
    fn fe_pop_locked(
        &self,
        core: CoreId,
        _arena: ArenaId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<usize> {
        let cpu = match self.cpu_for(core, meta) {
            Some(c) => c,
            None => return FeOutcome::Empty,
        };
        let _guard = cpu.lock();
        // W7-4 non-owner fence: `core` was sampled *before* the lock, so the thread may
        // have migrated since; and in RSEQ mode an in-flight sequence on `core` does not
        // abort merely because another CPU took the lock (the kernel aborts a critical
        // section on preempt/migrate/signal only). Draining it here is what makes the
        // locked fallback exclusive — without it a migrated thread and an in-flight
        // sequence can both commit `len`, double-vending an object. A no-op off RSEQ mode
        // and on the owning CPU. Same discipline as `pop_batch`/`push_batch`/`drain_cpu`.
        if !self.fence_if_non_owner(core) {
            // Cannot prove no sequence is in flight on `core`; touching the slot would
            // race it. Report empty and let the caller serve from central (§2.4).
            return FeOutcome::Empty;
        }
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return FeOutcome::Empty,
        };

        // Lazy init: ensure the slot has a buffer.
        if !slot.is_initialized() {
            let hard_cap = size_class::max_local_capacity(sc) as u32;
            let soft_cap = size_class::batch(sc) as u32;
            if !slot.init(meta, hard_cap, soft_cap) {
                return FeOutcome::Empty;
            }
        }

        let cur_len = slot.len.load(Ordering::Relaxed);
        if cur_len == 0 {
            slot.misses.fetch_add(1, Ordering::Relaxed);
            return FeOutcome::Empty;
        }

        let new_len = cur_len - 1;
        let buf = slot.buf_ptr();
        // SAFETY: buf points to a valid array of `hard_capacity` usize elements
        // allocated from MetadataAlloc (never freed). `new_len` < cur_len
        // <= hard_capacity (the hard capacity invariant), so the read is in bounds.
        // We hold the per-CPU lock, so no concurrent access.
        let addr = unsafe { *buf.add(new_len as usize) };
        slot.len.store(new_len, Ordering::Relaxed);
        FeOutcome::Success(addr)
    }

    /// Push an address into the per-CPU slot for `(core, arena, sc)`.
    ///
    /// Returns `FeOutcome::Success(())` on success, `FeOutcome::Full` if the
    /// slot is at soft capacity (needs flush). The slot is lazily initialized
    /// on first use via `meta`.
    ///
    /// In RSEQ mode (W7) this first attempts the restartable sequence on the
    /// current CPU's slot, falling through to the locked path on any condition
    /// it cannot handle (see [`fe_pop`](Self::fe_pop)).
    #[inline]
    pub fn fe_push(
        &self,
        core: CoreId,
        arena: ArenaId,
        sc: SizeClassId,
        addr: usize,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<()> {
        self.fe_push_tracked(core, arena, sc, addr, meta).0
    }

    /// [`fe_push`](Self::fe_push), additionally reporting **which core's slot it actually
    /// operated on**, or `None` if it touched none — the push-side counterpart of
    /// [`fe_pop_tracked`](Self::fe_pop_tracked), and needed for the same reason.
    ///
    /// A caller that reacts to `Full` by flushing needs this. `core` is only a hint on
    /// both fast paths, and the push resolves the real core itself: in RSEQ mode the
    /// hardware CPU, in pinned mode the §36.10 oracle. Flushing the hint instead is wrong
    /// twice over — it drains a slot that is not the full one (so the retry fails and the
    /// object falls to central for no reason), and in pinned mode it drains a core this
    /// thread does not own, racing that core's pinned worker on its non-atomic slot
    /// buffer. The pop side already learned this (see `fe_pop_tracked`); the push side
    /// went on guessing.
    #[inline]
    pub fn fe_push_tracked(
        &self,
        core: CoreId,
        arena: ArenaId,
        sc: SizeClassId,
        addr: usize,
        meta: &dyn MetadataAlloc,
    ) -> (FeOutcome<()>, Option<CoreId>) {
        match self.mode.load(Ordering::Acquire) {
            MODE_RSEQ => {
                if let Some((out, served)) = self.fe_push_rseq(sc, addr) {
                    return (out, Some(served));
                }
                let eff = self.effective_core(core);
                (self.fe_push_locked(eff, arena, sc, addr, meta), Some(eff))
            }
            // Decline rather than guess a core — see `fe_pop_tracked`'s `MODE_PINNED` arm
            // for why a locked fallback keyed on `core` is undefined behaviour here.
            MODE_PINNED => match self.fe_push_pinned_dispatch(arena, sc, addr, meta) {
                Some((out, served)) => (out, Some(served)),
                None => (FeOutcome::Full, None),
            },
            _ => (self.fe_push_locked(core, arena, sc, addr, meta), Some(core)),
        }
    }

    /// The locked (spinlock) push — the RSEQ-free correct baseline (W6-4) and the
    /// RSEQ-mode fallback.
    fn fe_push_locked(
        &self,
        core: CoreId,
        _arena: ArenaId,
        sc: SizeClassId,
        addr: usize,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<()> {
        let cpu = match self.cpu_for(core, meta) {
            Some(c) => c,
            None => return FeOutcome::Full,
        };
        let _guard = cpu.lock();
        // W7-4 non-owner fence: `core` was sampled *before* the lock, so the thread may
        // have migrated since; and in RSEQ mode an in-flight sequence on `core` does not
        // abort merely because another CPU took the lock (the kernel aborts a critical
        // section on preempt/migrate/signal only). Draining it here is what makes the
        // locked fallback exclusive — without it a migrated thread and an in-flight
        // sequence can both commit `len`, double-vending an object. A no-op off RSEQ mode
        // and on the owning CPU. Same discipline as `pop_batch`/`push_batch`/`drain_cpu`.
        if !self.fence_if_non_owner(core) {
            // As the pop: decline rather than race. `Full` routes the object to the
            // central free list, which is always correct, just slower.
            return FeOutcome::Full;
        }
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return FeOutcome::Full,
        };

        // Lazy init.
        if !slot.is_initialized() {
            let hard_cap = size_class::max_local_capacity(sc) as u32;
            let soft_cap = size_class::batch(sc) as u32;
            if !slot.init(meta, hard_cap, soft_cap) {
                return FeOutcome::Full;
            }
        }

        let cur_len = slot.len.load(Ordering::Relaxed);
        let soft = slot.soft_capacity.load(Ordering::Relaxed);
        if cur_len >= soft {
            slot.overflows.fetch_add(1, Ordering::Relaxed);
            return FeOutcome::Full;
        }

        let buf = slot.buf_ptr();
        // SAFETY: buf points to a valid array of `hard_capacity` elements.
        // `cur_len` < `soft_capacity` <= `hard_capacity` (init clamps soft
        // to hard, set_soft_capacity clamps likewise), so the write is in
        // bounds. We hold the per-CPU lock, so no concurrent access.
        unsafe { *buf.add(cur_len as usize) = addr };
        slot.len.store(cur_len + 1, Ordering::Relaxed);
        FeOutcome::Success(())
    }

    // --- RSEQ fast path (W7) ---

    /// The base address of the per-CPU array (`&cpus[0]`), which is both the
    /// per-CPU lock-byte base (offset 0) and the per-CPU stride base for the asm.
    #[inline]
    fn cpus_base(&self) -> *const u8 {
        self.cpus.load(Ordering::Acquire).cast::<u8>()
    }

    /// The effective core for the locked fallback in RSEQ mode: the hardware CPU
    /// the thread runs on, or the caller's hint if RSEQ cannot report it.
    /// The core an RSEQ sequence ran on: the area's current `cpu_id`, falling back to
    /// `at_entry` (the value read before the sequence) if that read is out of range.
    /// Both are valid slot keys — a slot is a keyed object pool, so *any* core id is
    /// correct — this just picks the one most likely to match the next pop.
    #[inline]
    fn area_core(area: *mut rseq::Rseq, at_entry: i32) -> CoreId {
        let now = rseq::cpu_of(area);
        let cpu = if now >= 0 && (now as usize) < MAX_CPUS {
            now
        } else {
            at_entry
        };
        CoreId(cpu.max(0) as u32)
    }

    #[inline]
    fn effective_core(&self, hint: CoreId) -> CoreId {
        let cpu = rseq::current_cpu();
        if cpu >= 0 && (cpu as usize) < MAX_CPUS {
            CoreId(cpu as u32)
        } else {
            hint
        }
    }

    /// Increment the miss counter for `(cpu, sc)` (approximate stats; Relaxed).
    #[inline]
    fn bump_miss(&self, cpu: usize, sc: SizeClassId) {
        if let Some(s) = self
            .cpus()
            .and_then(|c| c.get(cpu))
            .and_then(|c| c.slots.get(sc.index()))
        {
            s.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Increment the overflow counter for `(cpu, sc)` (approximate; Relaxed).
    #[inline]
    fn bump_overflow(&self, cpu: usize, sc: SizeClassId) {
        if let Some(s) = self
            .cpus()
            .and_then(|c| c.get(cpu))
            .and_then(|c| c.slots.get(sc.index()))
        {
            s.overflows.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether a non-owner drain must issue the W7-4 fence.
    ///
    /// The question this has to answer is **"could a restartable sequence be in flight
    /// right now?"**, and the honest answer is derived from the monotonic
    /// [`rseq_ever_enabled`](Self::rseq_ever_enabled) latch rather than from the current
    /// mode. A sequence runs only in `MODE_RSEQ`, which is reachable only after that latch
    /// is set, so `false` here *proves* no sequence has ever started and `true` is a
    /// conservative "fence to be sure".
    ///
    /// Reading the live mode instead is what made this fragile. It answered a subtly
    /// different question — "is the fast path on *at this instant*" — and the gap between
    /// the two is every in-flight sequence belonging to a mode that has just been changed
    /// out from under it. Patching that gap mode-by-mode (a transitional draining state,
    /// then serialising two disablers, then serialising an enabler against a disabler)
    /// treated each pair of racing writers as its own problem, and there was always
    /// another pair. A monotonic latch has no writers to order against.
    ///
    /// The precision lost is narrow and cold: a process that enabled RSEQ and later turned
    /// it off now fences on a non-owner drain where it previously would not. That is a
    /// fork child on a host-driven maintenance path. Nothing changes for RSEQ-on (it
    /// fenced already) or for a build that never enabled it (it skipped already, and still
    /// does — the latch is false).
    #[inline]
    fn non_owner_fence_armed(&self) -> bool {
        self.rseq_ever_enabled.load(Ordering::Acquire)
    }

    /// Test-only accessor for [`non_owner_fence_armed`](Self::non_owner_fence_armed).
    #[cfg(test)]
    pub(crate) fn non_owner_fence_armed_for_test(&self) -> bool {
        self.non_owner_fence_armed()
    }

    /// W7-4 non-owner fence: in RSEQ mode, if `core` is not the caller's current
    /// CPU (a non-owner drain), abort any in-flight RSEQ sequence on `core`. Must
    /// be called with `core`'s per-CPU lock held (so new sequences see the lock
    /// and divert); the fence then drains the in-flight ones (§27.4).
    /// Returns `false` when the fence was **required but could not be issued** — the
    /// caller must then decline the operation rather than touch the slot.
    ///
    /// The fence being validated once at `enable_rseq` time does not make a later failure
    /// impossible: `membarrier` is a syscall, and a thread that installs a seccomp filter
    /// denying it fails here while the process-wide RSEQ mode stays enabled. Treating
    /// that as a `debug_assert` leaves the release build proceeding to read and mutate the
    /// non-atomic slot buffer with an RSEQ sequence potentially still in flight on the
    /// target CPU — a real data race that loses or double-vends an object. The safe
    /// response is the one the front end already has for every other "cannot serve this
    /// here" case: decline, and let the caller fall back to the central path (§2.4).
    #[inline]
    #[must_use]
    pub(crate) fn fence_if_non_owner(&self, core: CoreId) -> bool {
        if self.non_owner_fence_armed() && (core.0 as i32) != rseq::current_cpu() {
            let ok = rseq::fence_rseq();
            // Still loud in debug — an unexplained failure is worth catching in tests —
            // but the release build now acts on it instead of ignoring it.
            debug_assert!(ok, "RSEQ non-owner fence failed unexpectedly (W7-4)");
            if !ok {
                self.fence_failures.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        }
        true
    }

    /// Attempt the restartable pop on the current CPU's slot for `sc`. Returns
    /// `Some(outcome)` when RSEQ handled it (`Success`/`Empty`), or `None` to
    /// signal the caller to take the locked path (uninitialised slot, a
    /// non-owner holding the lock, or the abort bound exceeded).
    #[inline]
    fn fe_pop_rseq(&self, sc: SizeClassId) -> Option<(FeOutcome<usize>, CoreId)> {
        if sc.index() >= NUM_SIZE_CLASSES {
            return None;
        }
        // Resolve the thread's rseq area once; derive the CPU from it (a null
        // area yields -1). This avoids a second thread-pointer read on the hot
        // path. The asm re-reads `cpu_id` inside the CS (the authoritative read);
        // `cpu` here only guards bounds and tags the approximate miss stat.
        let area = rseq::current_area();
        let cpu = rseq::cpu_of(area);
        if cpu < 0 || (cpu as usize) >= MAX_CPUS {
            return None;
        }
        let base = self.cpus_base();
        // The per-CPU array is carved lazily from metadata, and the restartable
        // sequence cannot allocate: divert to the locked path, which carves it (and
        // the slot buffer) before this class is ever pushed to.
        if base.is_null() {
            return None;
        }
        // SAFETY: `base` is the non-null per-CPU array, and
        // `slots_off + sc*slot_stride` lies within one `PerCpu`, so this is the base of
        // the per-`sc` slot column the asm indexes by CPU.
        let slot_base = unsafe { base.add(PERCPU_SLOTS_OFF + sc.index() * SLOT_STRIDE) };
        for _ in 0..RSEQ_ABORT_RETRY {
            // SAFETY: `area` is this thread's registered area; `base`/`slot_base`/
            // `PERCPU_STRIDE` describe this live per-CPU cache, with `len` at
            // `SLOT_LEN_OFF` and `buf` at `SLOT_BUF_OFF`.
            match unsafe {
                rseq::pop::<SLOT_LEN_OFF, SLOT_BUF_OFF>(
                    area,
                    slot_base,
                    base,
                    PERCPU_STRIDE,
                    MAX_CPUS,
                )
            } {
                rseq::Pop::Success(addr) => {
                    return Some((FeOutcome::Success(addr), Self::area_core(area, cpu)))
                }
                rseq::Pop::Empty => {
                    // Re-read the area's `cpu_id` rather than reuse the pre-loop `cpu`:
                    // the kernel updates it on migration, and this is the CPU whose slot
                    // the sequence just found empty — the one a refill must target.
                    // `cpu` stays the stat key (approximate by construction).
                    self.bump_miss(cpu as usize, sc);
                    return Some((FeOutcome::Empty, Self::area_core(area, cpu)));
                }
                rseq::Pop::Abort => continue,
                rseq::Pop::Fallback => return None,
            }
        }
        None
    }

    /// Attempt the restartable push on the current CPU's slot for `sc`, reporting the CPU
    /// it ran on. See [`fe_pop_rseq`](Self::fe_pop_rseq) for the `None` semantics.
    #[inline]
    fn fe_push_rseq(&self, sc: SizeClassId, addr: usize) -> Option<(FeOutcome<()>, CoreId)> {
        if sc.index() >= NUM_SIZE_CLASSES {
            return None;
        }
        // Resolve the area once (see `fe_pop_rseq`).
        let area = rseq::current_area();
        let cpu = rseq::cpu_of(area);
        if cpu < 0 || (cpu as usize) >= MAX_CPUS {
            return None;
        }
        let base = self.cpus_base();
        // As `fe_pop_rseq`: an uncarved array diverts to the locked path.
        if base.is_null() {
            return None;
        }
        // SAFETY: as `fe_pop_rseq`.
        let slot_base = unsafe { base.add(PERCPU_SLOTS_OFF + sc.index() * SLOT_STRIDE) };
        for _ in 0..RSEQ_ABORT_RETRY {
            // SAFETY: as `fe_pop_rseq`, plus `soft_capacity` at `SLOT_CAP_OFF`
            // and a buffer of at least `soft_capacity` entries (init clamps
            // `soft_capacity <= hard_capacity`, the buffer length).
            match unsafe {
                rseq::push::<SLOT_LEN_OFF, SLOT_BUF_OFF, SLOT_CAP_OFF>(
                    area,
                    slot_base,
                    base,
                    PERCPU_STRIDE,
                    MAX_CPUS,
                    addr,
                )
            } {
                rseq::Push::Success => {
                    return Some((FeOutcome::Success(()), Self::area_core(area, cpu)))
                }
                rseq::Push::Full => {
                    // Re-read the area's `cpu_id` rather than reuse the pre-loop `cpu`, as
                    // the `Empty` pop does: the kernel updates it on migration, and this is
                    // the CPU whose slot the sequence found full — the one a flush must
                    // target. `cpu` stays the stat key (approximate by construction).
                    self.bump_overflow(cpu as usize, sc);
                    return Some((FeOutcome::Full, Self::area_core(area, cpu)));
                }
                rseq::Push::Abort => continue,
                rseq::Push::Fallback => return None,
            }
        }
        None
    }

    // --- seLe4n pinned-thread per-core fast path (W7-5, §36.10 option 1) ---

    /// The pinned-core oracle as a typed function pointer, or `None` if unset.
    #[inline]
    fn pinned_oracle(&self) -> Option<fn() -> i32> {
        let f = self.pinned_core_fn.load(Ordering::Acquire);
        if f == 0 {
            None
        } else {
            // SAFETY: `f` was produced by `enable_pinned_core` as `(fn() -> i32)
            // as usize`; transmuting it back to the same `fn` type is sound.
            Some(unsafe { core::mem::transmute::<usize, fn() -> i32>(f) })
        }
    }

    /// `MODE_PINNED` dispatch for `fe_pop`: run the pinned sequence on the
    /// thread's current core (from the oracle), retrying the abort-on-migration
    /// case on the (now-current) core. Returns `Some(Success/Empty/Full)` or
    /// `None` to fall back to the locked path (no oracle / invalid core / the
    /// abort bound).
    #[inline]
    fn fe_pop_pinned_dispatch(
        &self,
        arena: ArenaId,
        sc: SizeClassId,
        meta: &dyn MetadataAlloc,
    ) -> Option<(FeOutcome<usize>, CoreId)> {
        let oracle = self.pinned_oracle()?;
        let provider = FnCoreProvider(oracle);
        for _ in 0..RSEQ_ABORT_RETRY {
            let cpu = oracle();
            if cpu < 0 || (cpu as usize) >= MAX_CPUS {
                return None;
            }
            let core = CoreId(cpu as u32);
            match self.fe_pop_pinned(core, arena, sc, &provider, meta) {
                FeOutcome::Abort => continue,
                other => return Some((other, core)),
            }
        }
        None
    }

    /// `MODE_PINNED` dispatch for `fe_push`, reporting the core it ran on so the caller's
    /// overflow flush targets the slot that is actually full — and, more to the point, one
    /// this thread owns. See [`fe_pop_pinned_dispatch`](Self::fe_pop_pinned_dispatch).
    #[inline]
    fn fe_push_pinned_dispatch(
        &self,
        arena: ArenaId,
        sc: SizeClassId,
        addr: usize,
        meta: &dyn MetadataAlloc,
    ) -> Option<(FeOutcome<()>, CoreId)> {
        let oracle = self.pinned_oracle()?;
        let provider = FnCoreProvider(oracle);
        for _ in 0..RSEQ_ABORT_RETRY {
            let cpu = oracle();
            if cpu < 0 || (cpu as usize) >= MAX_CPUS {
                return None;
            }
            let core = CoreId(cpu as u32);
            match self.fe_push_pinned(core, arena, sc, addr, &provider, meta) {
                FeOutcome::Abort => continue,
                other => return Some((other, core)),
            }
        }
        None
    }

    /// Pinned-thread per-core pop (W7-5). A software restartable sequence behind
    /// the same [`FeOutcome`] contract: it reads the current core from
    /// `provider`, **aborts with no state change** if the thread is not on
    /// `expected` (migration / violated pinning contract), and commits with a
    /// single store only when the core is stable across the read — mirroring
    /// the RSEQ abort contract (`per_core_cache_abort_no_change`, plan 02 W1-12d).
    ///
    /// Under the §36.10 pinned-thread contract the calling thread is the sole
    /// accessor of `expected`'s slot, so the read-modify-write is race-free; the
    /// allocating lazy-init is done under the per-CPU lock (it cannot live in the
    /// restartable section). See [`crate::pinned`].
    pub fn fe_pop_pinned(
        &self,
        expected: CoreId,
        _arena: ArenaId,
        sc: SizeClassId,
        provider: &dyn crate::pinned::CoreProvider,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<usize> {
        // Abort if not on the expected core (no state has been touched).
        if provider.current_core() != expected {
            return FeOutcome::Abort;
        }
        let cpu = match self.cpu_for(expected, meta) {
            Some(c) => c,
            None => return FeOutcome::Empty,
        };
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return FeOutcome::Empty,
        };
        // Lazy init (allocating) under the lock — outside the restartable part.
        if !slot.is_initialized() {
            let _guard = cpu.lock();
            let hard_cap = size_class::max_local_capacity(sc) as u32;
            let soft_cap = size_class::batch(sc) as u32;
            if !slot.init(meta, hard_cap, soft_cap) {
                return FeOutcome::Empty;
            }
        }
        let cur_len = slot.len.load(Ordering::Relaxed);
        if cur_len == 0 {
            slot.misses.fetch_add(1, Ordering::Relaxed);
            return FeOutcome::Empty;
        }
        let new_len = cur_len - 1;
        let buf = slot.buf_ptr();
        // SAFETY: `new_len < cur_len <= hard_capacity`; under the pinned-thread
        // contract this thread is the sole accessor, so the read is race-free.
        let addr = unsafe { *buf.add(new_len as usize) };
        // Abort-no-change: if the core changed before the commit, do not commit.
        if provider.current_core() != expected {
            return FeOutcome::Abort;
        }
        slot.len.store(new_len, Ordering::Relaxed); // the single committing store
        FeOutcome::Success(addr)
    }

    /// Pinned-thread per-core push (W7-5). The object is staged into the
    /// logically-free `buf[len]` and published only by the committing `len`
    /// store, so an abort before the commit is invisible. See
    /// [`fe_pop_pinned`](Self::fe_pop_pinned).
    pub fn fe_push_pinned(
        &self,
        expected: CoreId,
        _arena: ArenaId,
        sc: SizeClassId,
        addr: usize,
        provider: &dyn crate::pinned::CoreProvider,
        meta: &dyn MetadataAlloc,
    ) -> FeOutcome<()> {
        if provider.current_core() != expected {
            return FeOutcome::Abort;
        }
        let cpu = match self.cpu_for(expected, meta) {
            Some(c) => c,
            None => return FeOutcome::Full,
        };
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return FeOutcome::Full,
        };
        if !slot.is_initialized() {
            let _guard = cpu.lock();
            let hard_cap = size_class::max_local_capacity(sc) as u32;
            let soft_cap = size_class::batch(sc) as u32;
            if !slot.init(meta, hard_cap, soft_cap) {
                return FeOutcome::Full;
            }
        }
        let cur_len = slot.len.load(Ordering::Relaxed);
        let soft = slot.soft_capacity.load(Ordering::Relaxed);
        if cur_len >= soft {
            slot.overflows.fetch_add(1, Ordering::Relaxed);
            return FeOutcome::Full;
        }
        let buf = slot.buf_ptr();
        // SAFETY: `cur_len < soft_capacity <= hard_capacity`; sole accessor under
        // the pinned-thread contract. The stage targets logically-free space.
        unsafe { *buf.add(cur_len as usize) = addr };
        // Abort-no-change: the staged value is unpublished until `len` commits.
        if provider.current_core() != expected {
            return FeOutcome::Abort;
        }
        slot.len.store(cur_len + 1, Ordering::Relaxed); // the single committing store
        FeOutcome::Success(())
    }

    /// Pop up to `max` addresses from the per-CPU slot for `(core, sc)` into
    /// `out`. Returns the number of addresses popped. Used by flush operations
    /// (cache_ops W6-3b).
    ///
    /// **W7-4 non-owner coordination.** In RSEQ mode, when this is called for a
    /// CPU other than the caller's current one (a non-owner drain, e.g. idle-CPU
    /// flush), it takes the per-CPU lock and then issues the RSEQ fence so any
    /// in-flight sequence on that CPU is aborted before the slot is read — the
    /// lock makes new sequences divert, the fence drains the in-flight ones
    /// (§27.4). Owner-side batch ops (the common refill/flush on the current CPU)
    /// need no fence: only a thread running on that CPU can mutate its slot via
    /// RSEQ, and it is this thread.
    pub fn pop_batch(&self, core: CoreId, sc: SizeClassId, out: &mut [usize], max: usize) -> usize {
        let cpu = match self.cpus().and_then(|c| c.get(core.index())) {
            Some(c) => c,
            None => return 0,
        };
        let _guard = cpu.lock();
        if !self.fence_if_non_owner(core) {
            return 0; // cannot drain `core`: moved nothing, which every caller handles
        }
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return 0,
        };
        if !slot.is_initialized() {
            return 0;
        }
        // SAFETY: the per-CPU lock is held (and, for a non-owner, the RSEQ fence
        // has run), so this thread is the sole accessor of the slot.
        unsafe { pop_slot(slot, out, max) }
    }

    /// Push addresses from `addrs` into the per-CPU slot for `(core, sc)`.
    /// Returns the number of addresses pushed (may be less than `addrs.len()`
    /// if the slot hits hard capacity). Used by refill operations (cache_ops
    /// W6-3a).
    pub fn push_batch(&self, core: CoreId, sc: SizeClassId, addrs: &[usize]) -> usize {
        let cpu = match self.cpus().and_then(|c| c.get(core.index())) {
            Some(c) => c,
            None => return 0,
        };
        let _guard = cpu.lock();
        if !self.fence_if_non_owner(core) {
            return 0; // cannot drain `core`: moved nothing, which every caller handles
        }
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return 0,
        };
        if !slot.is_initialized() {
            return 0;
        }

        let cur_len = slot.len.load(Ordering::Relaxed) as usize;
        let hard = slot.hard_capacity.load(Ordering::Relaxed) as usize;
        let space = hard.saturating_sub(cur_len);
        let push_count = addrs.len().min(space);
        if push_count == 0 {
            return 0;
        }

        let buf = slot.buf_ptr();
        for (i, &addr) in addrs[..push_count].iter().enumerate() {
            // SAFETY: buf points to a valid array of `hard_capacity` elements.
            // `cur_len + i` < hard (the space check). We hold the per-CPU lock.
            unsafe { *buf.add(cur_len + i) = addr };
        }
        slot.len
            .store((cur_len + push_count) as u32, Ordering::Relaxed);
        push_count
    }

    /// Drain every initialized slot of `core` for an idle-CPU flush / hand-off.
    /// For each non-empty slot it repeatedly pops a chunk (up to `buf.len()`)
    /// **under the per-CPU lock + a non-owner RSEQ fence**, then **releases the
    /// lock before** invoking `sink(sc, &buf[..n])` — strict **hand-over-hand**:
    /// the per-CPU lock is never held while the sink takes a middle-end
    /// (transfer/central) lock. Returns the total drained.
    ///
    /// **Lock discipline (W7-4 / §27.2).** Each chunk re-acquires the per-CPU
    /// lock and re-issues the fence (the lock makes new sequences on `core`
    /// divert; the fence aborts any in-flight one), so the read is race-free; the
    /// lock is then dropped, and only afterwards does the sink lock the middle
    /// end. This avoids holding the per-CPU lock across a transfer/central
    /// acquisition entirely, so it cannot form a cycle with the refill path
    /// (which acquires transfer then — separately — the per-CPU lock). A
    /// membarrier per chunk is acceptable on the *idle* path; correctness over a
    /// fence-count optimization.
    ///
    /// **Bounded.** Each slot is drained by at most its residency when that slot's turn
    /// came, so the call is finite even under sustained concurrent frees onto `core` —
    /// see the loop body. Objects pushed during the drain are left for the next flush,
    /// which is what "approximate under concurrency" means here; under quiescence the
    /// bound is the whole slot and nothing is left behind.
    pub fn drain_cpu<F>(&self, core: CoreId, buf: &mut [usize], mut sink: F) -> usize
    where
        F: FnMut(SizeClassId, &[usize]),
    {
        let cpu = match self.cpus().and_then(|c| c.get(core.index())) {
            Some(c) => c,
            None => return 0,
        };
        let max = buf.len();
        let mut total = 0usize;
        for i in 0..NUM_SIZE_CLASSES {
            let slot = match cpu.slots.get(i) {
                Some(s) => s,
                None => continue,
            };
            // Lock-free pre-check: an uninitialised slot has no buffer (the RSEQ
            // sequence diverts on a null `buf`), so it needs neither the lock nor
            // the (expensive, all-CPU-IPI) fence.
            if !slot.is_initialized() {
                continue;
            }
            let sc = SizeClassId::new(i);
            // Pop one chunk under the lock + fence, then drop the lock *before*
            // sinking — strict hand-over-hand (the per-CPU lock is never held
            // while the sink takes a middle-end lock). The lock-free `len != 0`
            // guard skips the fence on an empty slot; a push racing it merely
            // defers that object to a later flush (conservation still holds).
            //
            // **Bounded by the residency at entry.** Because the lock is dropped around
            // each `sink`, sustained frees on this CPU can refill the slot in that window,
            // and an "until empty" loop has no reason to ever observe zero — a maintenance
            // call that is documented as approximate-under-concurrency would instead not
            // return. Draining at most what was resident when this slot's turn came makes
            // the work per call finite and leaves racing pushes for the next flush, which
            // is the same bound (and the same rationale) as the transfer-cache half.
            let mut budget = slot.len.load(Ordering::Relaxed) as usize;
            while budget != 0 && slot.len.load(Ordering::Relaxed) != 0 {
                let n = {
                    let _guard = cpu.lock();
                    if !self.fence_if_non_owner(core) {
                        break; // cannot drain safely; leave the rest for a later flush
                    }
                    // SAFETY: the per-CPU lock is held and the fence has run, so
                    // this thread is the sole accessor of the slot.
                    unsafe { pop_slot(slot, buf, max) }
                }; // <-- per-CPU lock released here, before the sink
                if n == 0 {
                    break;
                }
                budget = budget.saturating_sub(n);
                total = total.saturating_add(n);
                sink(sc, &buf[..n]); // hand-over-hand: no per-CPU lock held
            }
        }
        total
    }
}

impl Default for CpuCache {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: CpuCache is an array of PerCpu, each Sync.
unsafe impl Sync for CpuCache {}
// SAFETY: CpuCache is an array of PerCpu, each Send.
unsafe impl Send for CpuCache {}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::ids::ArenaId;

    const A: ArenaId = ArenaId::DEFAULT;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    /// W6 maintenance liveness: `drain_cpu` must return even while frees keep landing on
    /// the CPU being drained. The lock is dropped around each `sink`, so a producer can
    /// refill the slot in that window; an "until empty" loop then has no reason to ever
    /// observe zero, and `topomalloc_cache_flush_all` — documented as approximate under
    /// concurrency — would instead never return.
    ///
    /// The sink here refills the slot it was just handed, which is the adversarial shape
    /// of that race made deterministic and single-threaded: every drained chunk is pushed
    /// straight back. Bounded by the entry residency, the call terminates and reports the
    /// snapshot; unbounded, it spins forever.
    #[test]
    fn a_drain_terminates_even_when_the_slot_is_refilled_underneath_it() {
        let m = meta(8 * 1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let cap = size_class::max_local_capacity(sc) as u32;
        cc.init_slot(core, sc, &m, cap);

        // Seed the slot with a few objects.
        const SEEDED: usize = 4;
        for i in 0..SEEDED {
            cc.fe_push(core, A, sc, 0x1000 + i * 0x100, &m);
        }

        let mut buf = [0usize; 8];
        let mut pushed_back = 0usize;
        let drained = cc.drain_cpu(core, &mut buf, |sc2, objs| {
            // Put everything straight back — a producer that always wins the window.
            for &o in objs {
                cc.fe_push(core, A, sc2, o, &m);
                pushed_back += 1;
            }
        });

        assert_eq!(
            drained, SEEDED,
            "the drain is bounded by the slot's residency at entry"
        );
        assert_eq!(pushed_back, SEEDED, "every drained object was refilled");
        // The refills are still resident: they are the next flush's work, not this one's.
        assert_eq!(
            cc.per_cpu(core).unwrap().slot(sc).unwrap().len(),
            SEEDED as u32
        );
    }

    /// §36.10: **no safe method may take the cache out of pinned mode.**
    ///
    /// Pinned sequences take no per-CPU lock, so publishing any other mode while one is
    /// running lets the next operation touch that slot concurrently — and unlike the RSEQ
    /// path there is no fence that can drain it, because a pinned sequence is ordinary
    /// code with no kernel-visible critical section. That makes leaving pinned mode a
    /// quiescence obligation, which is why it lives in the `unsafe`
    /// [`disable_pinned_core`](CpuCache::disable_pinned_core).
    ///
    /// The obligation is only worth anything if *every* safe route respects it. It was
    /// established for `disable_rseq` first and `enable_rseq` still wrote the mode
    /// unconditionally, so a safe `enable_rseq` could do exactly what the unsafe method
    /// exists to gate. Both are pinned here so the next one cannot regress alone.
    #[test]
    fn no_safe_transition_leaves_pinned_mode() {
        let cc = CpuCache::new();
        // SAFETY: §36.10 — test-local cache, single-threaded, never drained concurrently.
        unsafe { cc.enable_pinned_core(|| 0) };
        assert!(cc.pinned_mode());

        cc.disable_rseq();
        assert!(
            cc.pinned_mode(),
            "disable_rseq must leave pinned mode alone — it is not RSEQ's to end"
        );

        let enabled = cc.enable_rseq();
        assert!(
            cc.pinned_mode(),
            "enable_rseq must not take the cache out of pinned mode either"
        );
        assert!(
            !enabled,
            "and it must report the RSEQ fast path as not enabled, which is the truth"
        );
        assert!(!cc.rseq_mode());

        // Only the unsafe, quiesced transition ends it.
        // SAFETY: nothing is in flight — no pinned operation has ever run on this cache.
        unsafe { cc.disable_pinned_core() };
        assert!(!cc.pinned_mode());
    }

    /// W7-4, the invariant the fence exists for: **once a restartable sequence could have
    /// run in this process, a non-owner drain fences — whatever the mode does afterwards.**
    ///
    /// This replaces two tests that pinned the *mechanism* instead (a transitional
    /// draining mode; which of two concurrent disablers was entitled to publish the
    /// terminal state). Both were faithful to the implementation of the day, and both
    /// missed the next mode pair, because the fragility was never in any particular pair —
    /// it was that `MODE_LOCKED` meant two different things: "dispatch to the locked path",
    /// which any thread may assert, and "no sequence is in flight", which only a thread
    /// that has fenced may assert. Deriving the fence from a monotonic latch separates
    /// them, so what is worth pinning is the property, not the choreography.
    ///
    /// Every transition is exercised, including the ones that were the last three rounds'
    /// bugs — enable racing disable, disable racing disable, pinned in the middle — and
    /// the fence must stay armed through all of them.
    #[test]
    fn the_non_owner_fence_stays_armed_once_rseq_has_ever_run() {
        let cc = CpuCache::new();

        // A cache that has never enabled RSEQ owes no fence: no sequence can ever have
        // started, so this is a proof, not an optimism.
        assert!(!cc.non_owner_fence_armed_for_test());
        cc.disable_rseq();
        assert!(!cc.non_owner_fence_armed_for_test(), "still never enabled");

        // `enable_rseq` returns false where the platform has no RSEQ (CI runs both), and
        // then nothing can be in flight and the latch must stay clear.
        if !cc.enable_rseq() {
            assert!(
                !cc.non_owner_fence_armed_for_test(),
                "a failed enable starts no sequences, so it must not arm the fence"
            );
            return;
        }
        assert!(cc.rseq_mode());
        assert!(cc.non_owner_fence_armed_for_test());

        // The transition that was round 5's bug: the fast path goes off, but a sequence
        // that started before it may still be committing, so the fence stays armed.
        cc.disable_rseq();
        assert!(!cc.rseq_mode(), "no new sequence may start");
        assert!(
            cc.non_owner_fence_armed_for_test(),
            "leaving RSEQ mode does not retire the sequences already in flight"
        );

        // Round 6's: a second disabler, arriving in any state, cannot clear it.
        cc.disable_rseq();
        cc.disable_rseq();
        assert!(cc.non_owner_fence_armed_for_test());

        // Round 8's: an enable racing a disable, in either order. Every ordering leaves a
        // mode that dispatches correctly and a fence that is still armed.
        cc.enable_rseq();
        cc.disable_rseq();
        assert!(cc.non_owner_fence_armed_for_test());
        cc.disable_rseq();
        cc.enable_rseq();
        assert!(cc.non_owner_fence_armed_for_test());

        // And pinned mode, which runs no RSEQ sequences of its own but cannot un-run the
        // ones that already happened.
        // SAFETY: §36.10 — this cache is test-local, single-threaded, never drained.
        unsafe { cc.enable_pinned_core(|| 0) };
        assert!(
            cc.non_owner_fence_armed_for_test(),
            "switching to pinned mode does not retire in-flight RSEQ sequences either"
        );
    }

    /// `ensure_cpus` carves the per-CPU array by **zeroing** a metadata block rather
    /// than materialising a 360 KiB `[PerCpu; MAX_CPUS]` temporary, which is sound only
    /// because the all-zero bit pattern *is* `PerCpu::new()`. That is a property of the
    /// field set, not of the language, so it is pinned here: adding a field whose
    /// initial value is not zero (a non-zero capacity, a `true` flag, a sentinel
    /// pointer) silently breaks the carve, and this test is what catches it.
    #[test]
    fn a_zeroed_per_cpu_block_is_the_const_initialiser() {
        let fresh = PerCpu::new();
        // SAFETY: `PerCpu` is `#[repr(C)]` over a `RankedLock` (one `AtomicBool`) and an
        // array of `CpuSlot`s, each of which is entirely atomics — every field's all-zero
        // pattern is a valid value, which is exactly the claim under test.
        let zeroed: PerCpu = unsafe { core::mem::zeroed() };
        // The lock byte: a zeroed lock must be *unlocked*, or the first acquisition on a
        // freshly carved array would spin forever.
        let g = zeroed.lock();
        drop(g);
        assert_eq!(fresh.slots.len(), zeroed.slots.len());
        for (a, b) in fresh.slots.iter().zip(zeroed.slots.iter()) {
            assert_eq!(a.is_initialized(), b.is_initialized());
            assert_eq!(a.len(), b.len());
            assert_eq!(a.soft_capacity(), b.soft_capacity());
            assert_eq!(a.hard_capacity(), b.hard_capacity());
            assert_eq!(a.misses(), b.misses());
            assert_eq!(a.overflows(), b.overflows());
            assert_eq!(
                a.buf.load(Ordering::Relaxed),
                b.buf.load(Ordering::Relaxed),
                "a zeroed slot must carry the same (null) buffer as a fresh one"
            );
        }
    }

    /// The whole point of the lazy carve: a cache nobody has used holds no metadata, so
    /// an engine embedding that never touches the front end pays nothing for it.
    #[test]
    fn an_unused_cache_carves_no_metadata_and_reads_as_empty() {
        let cc = CpuCache::new();
        assert!(cc.per_cpu(CoreId::DEFAULT).is_none());
        assert_eq!(
            cc.pop_batch(CoreId::DEFAULT, SizeClassId::new(0), &mut [0; 4], 4),
            0
        );
        assert_eq!(
            cc.push_batch(CoreId::DEFAULT, SizeClassId::new(0), &[1, 2]),
            0
        );
        assert!(cc.check_invariants());
        // ... and a starved metadata arena degrades to "declines", never to a panic or a
        // wrong answer (§2.4): the caller falls back to the central path.
        let starved = meta(0);
        assert!(cc
            .fe_pop(CoreId::DEFAULT, A, SizeClassId::new(0), &starved)
            .is_empty());
        assert!(cc
            .fe_push(CoreId::DEFAULT, A, SizeClassId::new(0), 0x1000, &starved)
            .is_full());
        assert!(cc.per_cpu(CoreId::DEFAULT).is_none());
    }

    #[test]
    fn pop_from_empty_returns_empty() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        assert!(cc.fe_pop(core, A, sc, &m).is_empty());
    }

    #[test]
    fn push_pop_round_trip() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);

        assert!(cc.fe_push(core, A, sc, 0xDEAD, &m).is_success());
        assert!(cc.fe_push(core, A, sc, 0xBEEF, &m).is_success());

        // LIFO: last pushed first popped.
        assert_eq!(cc.fe_pop(core, A, sc, &m).unwrap(), 0xBEEF);
        assert_eq!(cc.fe_pop(core, A, sc, &m).unwrap(), 0xDEAD);
        assert!(cc.fe_pop(core, A, sc, &m).is_empty());
    }

    #[test]
    fn push_beyond_soft_capacity_returns_full() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;

        // Lazy init sets soft_cap = batch_size. Fill to that limit.
        for i in 0..batch {
            let result = cc.fe_push(core, A, sc, i as usize + 1, &m);
            assert!(result.is_success(), "push {i} of {batch} failed");
        }

        // Next push hits soft_capacity -- returns Full.
        assert!(cc.fe_push(core, A, sc, 999, &m).is_full());
    }

    #[test]
    fn push_beyond_hard_capacity_returns_full() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Init with soft_cap = hard_cap so fe_push fills to the absolute ceiling.
        cc.init_slot(core, sc, &m, hard_cap);

        for i in 0..hard_cap {
            let result = cc.fe_push(core, A, sc, i as usize + 1, &m);
            assert!(result.is_success(), "push {i} of {hard_cap} failed");
        }

        // Next push should return Full.
        assert!(cc.fe_push(core, A, sc, 999, &m).is_full());
    }

    #[test]
    fn hard_capacity_invariant_holds() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Init with soft_cap = hard_cap to fill to hard ceiling.
        cc.init_slot(core, sc, &m, hard_cap);

        for i in 0..hard_cap {
            cc.fe_push(core, A, sc, i as usize + 1, &m);
        }

        // Verify len does not exceed hard_capacity.
        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert!(slot.len() <= slot.hard_capacity());
        assert_eq!(slot.len(), hard_cap);
    }

    /// `fe_pop_tracked` reports the core whose slot it actually touched — the value a
    /// refill must be keyed to, so the batch it moves out of central lands where the
    /// consuming pop will look for it.
    ///
    /// In locked and pinned modes the served core is the requested one, which is what
    /// this asserts exactly. In RSEQ mode the hardware CPU is authoritative and the
    /// argument is only a hint, which is precisely why the caller cannot compute this
    /// itself: the reported value comes from inside the pop, after any migration that
    /// preceded it. The forced-migration battery (`tests/rseq_equivalence.rs`) covers
    /// that mode; here the point is that the reported core is a real slot key and that
    /// the plain `fe_pop` wrapper still agrees with it.
    #[test]
    fn a_tracked_pop_reports_the_core_it_served() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let sc = SizeClassId::new(0);
        let core = CoreId(3);

        // Empty slot: the outcome is `Empty` and the served core is still reported, since
        // that is the case a refill reacts to.
        let (out, served) = cc.fe_pop_tracked(core, A, sc, &m);
        assert!(out.is_empty());
        assert_eq!(
            served,
            Some(core),
            "the locked path serves the requested core"
        );

        // And on a hit: push to a *different* core, then confirm each pop is attributed
        // to the slot it actually drained rather than to whichever core was asked last.
        let other = CoreId(5);
        assert!(cc.fe_push(other, A, sc, 0xAB, &m).is_success());
        let (out, served) = cc.fe_pop_tracked(other, A, sc, &m);
        assert_eq!(out.unwrap(), 0xAB);
        assert_eq!(served, Some(other));
        let (out, served) = cc.fe_pop_tracked(core, A, sc, &m);
        assert!(out.is_empty(), "core 3's slot was never filled");
        assert_eq!(
            served,
            Some(core),
            "an empty pop is attributed to its own core"
        );

        // The wrapper is the tracked pop's first component, so the two cannot drift.
        assert!(cc.fe_push(core, A, sc, 0xCD, &m).is_success());
        assert_eq!(cc.fe_pop(core, A, sc, &m).unwrap(), 0xCD);
    }

    /// §36.10: a pinned push lands on the **oracle's** core and reports it, not the
    /// caller's hint.
    ///
    /// `current_core()` — the hint's source — consults `rseq::current_cpu()` and a
    /// per-thread spreading key, neither of which knows about the pinned oracle, so in a
    /// no-RSEQ pinned deployment the hint names an essentially arbitrary slot. The caller
    /// reacts to `Full` by flushing, and flushing the hint drains a core this thread does
    /// not own: that core's pinned worker takes no per-CPU lock and no fence can drain it,
    /// so the flush races its non-atomic slot buffer — an object lost or double-vended.
    ///
    /// Both halves are asserted, because only the second is the memory-safety one: the
    /// object lands in the oracle's slot (not the hint's), and the *reported* core — the
    /// value the flush is keyed on — is the oracle's too. Fails with the pinned arm
    /// reporting `core`, which is what the push side did while the pop side had already
    /// been fixed to report the core it served.
    #[test]
    fn a_pinned_push_lands_and_reports_the_oracle_core_not_the_hint() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let sc = SizeClassId::new(0);
        // A constant oracle: no real CPU pinning needed, since `fe_push_pinned` only
        // compares the oracle against itself.
        const PINNED: u32 = 3;
        // SAFETY: §36.10 hand-off contract — this cache is local to the test, only this
        // thread ever touches it, and nothing drains it concurrently.
        unsafe { cc.enable_pinned_core(|| PINNED as i32) };
        let hint = CoreId(0); // deliberately not the oracle's core

        let (out, served) = cc.fe_push_tracked(hint, A, sc, 0x1000, &m);
        assert!(out.is_success());
        assert_eq!(
            served,
            Some(CoreId(PINNED)),
            "the push reported the hint, so an overflow flush would drain a core this \
             thread does not own"
        );
        // The object really is on the oracle's core, not the hint's.
        assert!(
            cc.fe_pop_on_core(hint, A, sc, &m).is_empty(),
            "nothing may land in the hint's slot"
        );
        assert_eq!(
            cc.fe_pop_on_core(CoreId(PINNED), A, sc, &m).unwrap(),
            0x1000
        );
    }

    /// §36.10 + §2.4: when the oracle cannot name this thread's core, a pinned operation
    /// **declines** — it must not fall back to a locked access keyed on the caller's hint.
    ///
    /// The locked fallback was the hazard hiding behind the hint. Under the pinning
    /// contract the hint's slot belongs to *another* core's pinned worker, which takes no
    /// per-CPU lock and cannot be fenced, so a locked read-modify-write of its buffer is
    /// undefined behaviour. The allocator has to honour the contract `enable_pinned_core`
    /// asks its caller to uphold; declining costs one trip to the central path.
    ///
    /// Fails with the `fe_*_locked(core, ..)` fallback restored: the object lands in the
    /// hint's slot, and the pop reports that slot as served.
    #[test]
    fn a_pinned_operation_with_no_resolvable_core_touches_no_slot() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let sc = SizeClassId::new(0);
        // SAFETY: as above — a test-local cache, single-threaded, never drained.
        unsafe { cc.enable_pinned_core(|| -1) };
        let hint = CoreId(0);

        let (out, served) = cc.fe_push_tracked(hint, A, sc, 0x2000, &m);
        assert!(out.is_full(), "an unresolvable core declines the push");
        assert_eq!(served, None, "no slot was touched, so none may be reported");

        let (out, served) = cc.fe_pop_tracked(hint, A, sc, &m);
        assert!(out.is_empty(), "an unresolvable core declines the pop");
        assert_eq!(served, None);

        // Nothing was written into the hint's slot — nor anywhere else the fallback key
        // could have reached.
        for c in 0..8u32 {
            assert!(
                cc.fe_pop_on_core(CoreId(c), A, sc, &m).is_empty(),
                "core {c} received an object from a declined pinned push"
            );
        }
    }

    /// `fe_pop_on_core` reaches a *named* slot regardless of the running CPU — the
    /// capability the allocation loop needs to recover a batch it parked on a core it has
    /// since migrated away from. The ordinary `fe_pop` cannot do this in RSEQ mode, where
    /// the hardware CPU is authoritative and the argument is only a hint.
    /// W7-4: a required non-owner fence that cannot be issued must make the locked
    /// operation **decline**, not proceed.
    ///
    /// `membarrier` is a syscall, so its availability is not settled for the whole process
    /// by the one-time probe at `enable_rseq`: a thread that installs a seccomp filter
    /// denying it fails the fence while the process-wide RSEQ mode stays on. Before this,
    /// the failure was a bare `debug_assert!` — so a release build read and mutated the
    /// non-atomic slot buffer with a sequence possibly still in flight on the target CPU.
    ///
    /// Declining is always safe: an empty pop and a full push both route the caller to
    /// the central path (§2.4). What this pins is that the decision is *observable* —
    /// `fence_if_non_owner` reports it rather than swallowing it.
    #[test]
    fn a_failed_non_owner_fence_is_reported_so_the_caller_can_decline() {
        let cc = CpuCache::new();
        // Off RSEQ mode the fence is not required, so it must never report failure —
        // otherwise every locked operation on a non-RSEQ host would decline and the
        // front end would be dead weight.
        assert!(
            cc.fence_if_non_owner(CoreId(0)),
            "no fence is required when RSEQ was never enabled"
        );
        assert!(
            cc.fence_if_non_owner(CoreId(9)),
            "a non-owner core still needs no fence off RSEQ mode"
        );
        assert_eq!(
            cc.fence_failures.load(Ordering::Relaxed),
            0,
            "nothing failed, so nothing is counted"
        );
    }

    #[test]
    fn a_core_specific_pop_reaches_that_cores_slot() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let sc = SizeClassId::new(0);
        let parked = CoreId(7);
        let here = CoreId(2);

        assert!(cc.fe_push(parked, A, sc, 0x1234, &m).is_success());
        // The running core's slot is empty, so an ordinary pop finds nothing...
        assert!(cc.fe_pop(here, A, sc, &m).is_empty());
        // ...but the batch is still reachable where it was actually left.
        assert_eq!(cc.fe_pop_on_core(parked, A, sc, &m).unwrap(), 0x1234);
        assert!(cc.fe_pop_on_core(parked, A, sc, &m).is_empty());
    }

    #[test]
    fn lazy_init_on_push() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);

        // Nothing is carved before first use — neither the per-CPU array nor the slot
        // buffer inside it.
        assert!(cc.per_cpu(core).is_none());

        // The first push carves the array *and* initializes the slot's buffer.
        assert!(cc.fe_push(core, A, sc, 42, &m).is_success());
        let cpu = cc
            .per_cpu(core)
            .expect("the push must have carved the array");
        assert!(cpu.slot(sc).unwrap().is_initialized());
        // Only the size class that was used has a buffer; the rest stay uninitialized.
        assert!(!cpu.slot(SizeClassId::new(1)).unwrap().is_initialized());
    }

    #[test]
    fn miss_and_overflow_counters() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Init with soft_cap = hard_cap so we can fill to the absolute ceiling.
        cc.init_slot(core, sc, &m, hard_cap);

        // Pop from empty -> miss
        cc.fe_pop(core, A, sc, &m);
        cc.fe_pop(core, A, sc, &m);
        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert_eq!(slot.misses(), 2);

        // Fill to capacity then overflow
        for i in 0..hard_cap {
            cc.fe_push(core, A, sc, i as usize + 1, &m);
        }
        cc.fe_push(core, A, sc, 999, &m);
        cc.fe_push(core, A, sc, 998, &m);
        assert_eq!(slot.overflows(), 2);

        // Reset counters
        assert_eq!(slot.reset_misses(), 2);
        assert_eq!(slot.misses(), 0);
        assert_eq!(slot.reset_overflows(), 2);
        assert_eq!(slot.overflows(), 0);
    }

    #[test]
    fn different_cpus_independent() {
        let m = meta(2 * 1024 * 1024);
        let cc = CpuCache::new();
        let core0 = CoreId(0);
        let core1 = CoreId(1);
        let sc = SizeClassId::new(0);

        cc.fe_push(core0, A, sc, 100, &m);
        cc.fe_push(core1, A, sc, 200, &m);

        assert_eq!(cc.fe_pop(core0, A, sc, &m).unwrap(), 100);
        assert_eq!(cc.fe_pop(core1, A, sc, &m).unwrap(), 200);

        // Each CPU's slot is independent.
        assert!(cc.fe_pop(core0, A, sc, &m).is_empty());
        assert!(cc.fe_pop(core1, A, sc, &m).is_empty());
    }

    #[test]
    fn push_pop_batch() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);

        // Init slot first.
        cc.init_slot(core, sc, &m, 32);

        // Push a batch.
        let addrs: Vec<usize> = (100..108).collect();
        let pushed = cc.push_batch(core, sc, &addrs);
        assert_eq!(pushed, 8);

        // Pop a batch.
        let mut out = [0usize; 16];
        let popped = cc.pop_batch(core, sc, &mut out, 8);
        assert_eq!(popped, 8);

        let popped_set: std::collections::BTreeSet<usize> = out[..8].iter().copied().collect();
        let orig_set: std::collections::BTreeSet<usize> = addrs.iter().copied().collect();
        assert_eq!(popped_set, orig_set);
    }

    #[test]
    fn concurrent_push_pop_from_different_cpus() {
        let m = meta(4 * 1024 * 1024);
        let cc = CpuCache::new();
        let sc = SizeClassId::new(0);

        let cc_ref = &cc;
        let m_ref = &m;

        std::thread::scope(|s| {
            // Each thread uses its own CPU.
            for t in 0..4u32 {
                s.spawn(move || {
                    let core = CoreId(t);
                    for i in 0..100u32 {
                        let addr = (t * 10000 + i) as usize;
                        cc_ref.fe_push(core, A, sc, addr, m_ref);
                    }
                    for _ in 0..50 {
                        cc_ref.fe_pop(core, A, sc, m_ref);
                    }
                });
            }
        });

        // No crash, no data race.
        for t in 0..4u32 {
            let core = CoreId(t);
            let cpu = cc.per_cpu(core).unwrap();
            let slot = cpu.slot(sc).unwrap();
            assert!(slot.len() <= slot.hard_capacity());
        }
    }

    #[test]
    fn out_of_range_core_returns_empty_or_full() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let bad_core = CoreId(MAX_CPUS as u32);
        let sc = SizeClassId::new(0);
        assert!(cc.fe_pop(bad_core, A, sc, &m).is_empty());
        assert!(cc.fe_push(bad_core, A, sc, 42, &m).is_full());
    }

    #[test]
    fn init_slot_sets_capacities() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch_size = size_class::batch(sc) as u32;
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        assert!(cc.init_slot(core, sc, &m, batch_size));

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert!(slot.is_initialized());
        assert_eq!(slot.soft_capacity(), batch_size);
        assert_eq!(slot.hard_capacity(), hard_cap);
    }

    #[test]
    fn init_clamps_soft_to_hard() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let sc2 = SizeClassId::new(1);
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Pass soft_cap > hard_cap: should be clamped to hard_cap.
        assert!(cc.init_slot(core, sc, &m, hard_cap + 100));

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert_eq!(slot.soft_capacity(), hard_cap);

        // Pass soft_cap = 0: should be clamped to 1.
        assert!(cc.init_slot(core, sc2, &m, 0));
        let slot2 = cpu.slot(sc2).unwrap();
        assert_eq!(slot2.soft_capacity(), 1);
    }

    #[test]
    fn set_soft_capacity_clamps() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        cc.init_slot(core, sc, &m, hard_cap);

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();

        // Setting above hard_cap clamps to hard_cap.
        slot.set_soft_capacity(hard_cap + 50);
        assert_eq!(slot.soft_capacity(), hard_cap);

        // Setting to 0 clamps to 1 (minimum).
        slot.set_soft_capacity(0);
        assert_eq!(slot.soft_capacity(), 1);

        // Normal value within range works.
        slot.set_soft_capacity(64);
        assert_eq!(slot.soft_capacity(), 64);
    }

    /// Publishing (or shrinking) the host's CPU count must not move a single thread's
    /// slot. It used to: the no-RSEQ fallback took its key modulo `active_cpus`, so an
    /// embedding that allocated before announcing a count spread over `MAX_CPUS` and the
    /// announcement then re-mapped every thread into `0..cpus`. Everything the earlier
    /// mapping had cached at a higher index became unreachable to ordinary allocation —
    /// and those objects had been *removed from central* to get there, so on an exhausted
    /// backend the allocator returns null with reusable objects parked in slots nothing
    /// will look at.
    ///
    /// Draining the excluded slots at the publication point would only narrow the window
    /// (a thread that sampled the old mapping can push after the drain, and a shrink can
    /// happen again); this pins the mapping's *independence* from the count instead,
    /// which is what closes it. `active_cpus` is a population, not an index bound — the
    /// same distinction `CacheBudget::adapt` turns on, since Linux does not promise online CPU
    /// ids are dense.
    #[test]
    fn the_fallback_slot_never_moves_when_the_cpu_count_changes() {
        let cc = CpuCache::new();
        // Every count the host could publish, including "none yet" (0), a shrink, and a
        // value larger than the array.
        for count in [
            0u32,
            1,
            2,
            7,
            64,
            MAX_CPUS as u32,
            MAX_CPUS as u32 + 9,
            3,
            0,
        ] {
            cc.set_active_cpus(count);
            for key in 0..512usize {
                assert_eq!(
                    CpuCache::fallback_core(key),
                    CoreId((key % MAX_CPUS) as u32),
                    "thread key {key} moved slots after publishing {count} CPUs"
                );
            }
        }

        // ...and the mapping really does reach the whole array, so a future "clamp to
        // the active count" cannot pass the invariance check above by collapsing
        // everything onto core 0.
        let mut seen = [false; MAX_CPUS];
        for key in 0..MAX_CPUS {
            seen[CpuCache::fallback_core(key).index()] = true;
        }
        assert!(seen.iter().all(|s| *s), "the fallback must span every slot");

        // The live entry point agrees whenever it takes the fallback branch (it takes
        // the hardware CPU when RSEQ is active, which is count-independent by
        // construction).
        if rseq::current_cpu() < 0 {
            if let Some(k) = crate::fork::shard::thread_key() {
                cc.set_active_cpus(1);
                assert_eq!(cc.current_core(), CpuCache::fallback_core(k));
                cc.set_active_cpus(0);
                assert_eq!(cc.current_core(), CpuCache::fallback_core(k));
            }
        }
    }

    #[test]
    fn push_respects_soft_capacity() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;

        cc.init_slot(core, sc, &m, batch);

        // Fill to soft capacity.
        for i in 0..batch {
            let r = cc.fe_push(core, A, sc, i as usize + 1, &m);
            assert!(r.is_success(), "push {i} should succeed");
        }

        // Next push should return Full.
        let r = cc.fe_push(core, A, sc, 999, &m);
        assert!(r.is_full(), "push beyond soft capacity should return Full");

        // Reduce soft capacity and try pushing (already at old soft cap).
        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        slot.set_soft_capacity(batch / 2);

        // Slot len is still `batch` which is > new soft_capacity.
        // fe_push should return Full.
        let r = cc.fe_push(core, A, sc, 888, &m);
        assert!(r.is_full());
    }
}

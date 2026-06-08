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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use topo_arch::rseq;

use crate::bootstrap::MetadataAlloc;
use crate::fe::{CoreId, FeOutcome};
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::{ArenaId, SizeClassId};
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
}

/// Per-CPU state: a spinlock and per-size-class slots.
///
/// `#[repr(C)]` with `locked` first (offset 0): the RSEQ sequence reads the lock
/// byte at `&cpus[cpu]` to divert to the locked path when a non-owner holds it
/// (W7-4), and indexes `slots` by the kernel-reported CPU. The offsets and the
/// `size_of::<PerCpu>()` stride are fed to the assembly via `offset_of!`.
#[repr(C)]
pub struct PerCpu {
    /// Spinlock protecting all slots for this CPU. Offset 0 (the RSEQ lock byte).
    locked: AtomicBool,
    /// Per-size-class slots.
    slots: [CpuSlot; NUM_SIZE_CLASSES],
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            slots: [const { CpuSlot::new() }; NUM_SIZE_CLASSES],
        }
    }

    /// Acquire the per-CPU lock.
    #[inline]
    fn lock(&self) -> CpuGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        CpuGuard { cpu: self }
    }

    /// The slot for a size class (bounds-checked).
    #[inline]
    pub fn slot(&self, sc: SizeClassId) -> Option<&CpuSlot> {
        self.slots.get(sc.index())
    }
}

/// RAII guard for a locked [`PerCpu`].
struct CpuGuard<'a> {
    cpu: &'a PerCpu,
}

impl Drop for CpuGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.cpu.locked.store(false, Ordering::Release);
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

/// The per-CPU cache: `MAX_CPUS` [`PerCpu`] entries (W6-4).
///
/// Thread-safe by construction: each CPU has its own spinlock. The fast-path
/// operations `fe_pop` and `fe_push` lock the target CPU, operate on the slot,
/// and unlock -- the transfer and central locks are never held.
pub struct CpuCache {
    cpus: [PerCpu; MAX_CPUS],
    /// Number of active (online) CPUs. Operations on a core beyond this
    /// count are valid but will always miss (no slots initialized).
    active_cpus: AtomicU32,
    /// Whether the RSEQ fast path is in use (W7). `false` is the locked baseline
    /// (W6-4); set by [`enable_rseq`](Self::enable_rseq) when the platform
    /// supports RSEQ, cleared by [`disable_rseq`](Self::disable_rseq) (e.g. the
    /// child fork handler, which must go conservative until per-CPU state is
    /// safe, §28.1).
    rseq_enabled: AtomicBool,
}

impl CpuCache {
    /// A fresh, empty CPU cache (no slots initialized), in the locked baseline
    /// mode. Call [`enable_rseq`](Self::enable_rseq) to opt into the RSEQ fast
    /// path after the cache is wired up.
    pub const fn new() -> Self {
        Self {
            cpus: [const { PerCpu::new() }; MAX_CPUS],
            active_cpus: AtomicU32::new(0),
            rseq_enabled: AtomicBool::new(false),
        }
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
        self.rseq_enabled.store(ok, Ordering::Release);
        ok
    }

    /// Disable the RSEQ fast path, reverting to the locked baseline (§28.1 child
    /// fork handler / conservative mode). Already-cached objects are unaffected;
    /// subsequent operations take the spinlock path.
    pub fn disable_rseq(&self) {
        self.rseq_enabled.store(false, Ordering::Release);
    }

    /// Whether the RSEQ fast path is currently active.
    #[inline]
    pub fn rseq_mode(&self) -> bool {
        self.rseq_enabled.load(Ordering::Acquire)
    }

    /// Register the calling thread for the RSEQ fast path (§27.6, W7-1). Returns
    /// whether this thread can use it; in the locked baseline this is a no-op
    /// returning `false`.
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

    /// The per-CPU entry for a core (bounds-checked).
    #[inline]
    pub fn per_cpu(&self, core: CoreId) -> Option<&PerCpu> {
        self.cpus.get(core.index())
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
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return false,
        };
        let _guard = cpu.lock();
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
        if self.rseq_mode() {
            if let Some(out) = self.fe_pop_rseq(sc) {
                return out;
            }
            return self.fe_pop_locked(self.effective_core(core), arena, sc, meta);
        }
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
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return FeOutcome::Empty,
        };
        let _guard = cpu.lock();
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
        if self.rseq_mode() {
            if let Some(out) = self.fe_push_rseq(sc, addr) {
                return out;
            }
            return self.fe_push_locked(self.effective_core(core), arena, sc, addr, meta);
        }
        self.fe_push_locked(core, arena, sc, addr, meta)
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
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return FeOutcome::Full,
        };
        let _guard = cpu.lock();
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
        core::ptr::addr_of!(self.cpus).cast::<u8>()
    }

    /// The effective core for the locked fallback in RSEQ mode: the hardware CPU
    /// the thread runs on, or the caller's hint if RSEQ cannot report it.
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
        if let Some(s) = self.cpus.get(cpu).and_then(|c| c.slots.get(sc.index())) {
            s.misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Increment the overflow counter for `(cpu, sc)` (approximate; Relaxed).
    #[inline]
    fn bump_overflow(&self, cpu: usize, sc: SizeClassId) {
        if let Some(s) = self.cpus.get(cpu).and_then(|c| c.slots.get(sc.index())) {
            s.overflows.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// W7-4 non-owner fence: in RSEQ mode, if `core` is not the caller's current
    /// CPU (a non-owner drain), abort any in-flight RSEQ sequence on `core`. Must
    /// be called with `core`'s per-CPU lock held (so new sequences see the lock
    /// and divert); the fence then drains the in-flight ones (§27.4).
    #[inline]
    fn fence_if_non_owner(&self, core: CoreId) {
        if self.rseq_mode() && (core.0 as i32) != rseq::current_cpu() {
            rseq::fence_rseq();
        }
    }

    /// Attempt the restartable pop on the current CPU's slot for `sc`. Returns
    /// `Some(outcome)` when RSEQ handled it (`Success`/`Empty`), or `None` to
    /// signal the caller to take the locked path (uninitialised slot, a
    /// non-owner holding the lock, or the abort bound exceeded).
    #[inline]
    fn fe_pop_rseq(&self, sc: SizeClassId) -> Option<FeOutcome<usize>> {
        if sc.index() >= NUM_SIZE_CLASSES {
            return None;
        }
        let cpu = rseq::current_cpu();
        if cpu < 0 || (cpu as usize) >= MAX_CPUS {
            return None;
        }
        let area = rseq::current_area();
        if area.is_null() {
            return None;
        }
        let base = self.cpus_base();
        // SAFETY: `slots_off + sc*slot_stride` lies within one `PerCpu`, so this
        // is the base of the per-`sc` slot column the asm indexes by CPU.
        let slot_base = unsafe { base.add(PERCPU_SLOTS_OFF + sc.index() * SLOT_STRIDE) };
        for _ in 0..RSEQ_ABORT_RETRY {
            // SAFETY: `area` is this thread's registered area; `base`/`slot_base`/
            // `PERCPU_STRIDE` describe this live per-CPU cache, with `len` at
            // `SLOT_LEN_OFF` and `buf` at `SLOT_BUF_OFF`.
            match unsafe {
                rseq::pop::<SLOT_LEN_OFF, SLOT_BUF_OFF>(area, slot_base, base, PERCPU_STRIDE)
            } {
                rseq::Pop::Success(addr) => return Some(FeOutcome::Success(addr)),
                rseq::Pop::Empty => {
                    self.bump_miss(cpu as usize, sc);
                    return Some(FeOutcome::Empty);
                }
                rseq::Pop::Abort => continue,
                rseq::Pop::Fallback => return None,
            }
        }
        None
    }

    /// Attempt the restartable push on the current CPU's slot for `sc`. See
    /// [`fe_pop_rseq`](Self::fe_pop_rseq) for the `None` semantics.
    #[inline]
    fn fe_push_rseq(&self, sc: SizeClassId, addr: usize) -> Option<FeOutcome<()>> {
        if sc.index() >= NUM_SIZE_CLASSES {
            return None;
        }
        let cpu = rseq::current_cpu();
        if cpu < 0 || (cpu as usize) >= MAX_CPUS {
            return None;
        }
        let area = rseq::current_area();
        if area.is_null() {
            return None;
        }
        let base = self.cpus_base();
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
                    addr,
                )
            } {
                rseq::Push::Success => return Some(FeOutcome::Success(())),
                rseq::Push::Full => {
                    self.bump_overflow(cpu as usize, sc);
                    return Some(FeOutcome::Full);
                }
                rseq::Push::Abort => continue,
                rseq::Push::Fallback => return None,
            }
        }
        None
    }

    // --- seLe4n pinned-thread per-core fast path (W7-5, §36.10 option 1) ---

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
        let cpu = match self.cpus.get(expected.index()) {
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
        let cpu = match self.cpus.get(expected.index()) {
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
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return 0,
        };
        let _guard = cpu.lock();
        self.fence_if_non_owner(core);
        let slot = match cpu.slots.get(sc.index()) {
            Some(s) => s,
            None => return 0,
        };
        if !slot.is_initialized() {
            return 0;
        }

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
        for (i, slot) in out[..pop_count].iter_mut().enumerate() {
            // SAFETY: buf points to a valid array of `hard_capacity` usize elements.
            // `new_len + i` < cur_len <= hard_capacity. We hold the per-CPU lock.
            *slot = unsafe { *buf.add(new_len + i) };
        }
        slot.len.store(new_len as u32, Ordering::Relaxed);
        pop_count
    }

    /// Push addresses from `addrs` into the per-CPU slot for `(core, sc)`.
    /// Returns the number of addresses pushed (may be less than `addrs.len()`
    /// if the slot hits hard capacity). Used by refill operations (cache_ops
    /// W6-3a).
    pub fn push_batch(&self, core: CoreId, sc: SizeClassId, addrs: &[usize]) -> usize {
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return 0,
        };
        let _guard = cpu.lock();
        self.fence_if_non_owner(core);
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

    #[test]
    fn lazy_init_on_push() {
        let m = meta(1024 * 1024);
        let cc = CpuCache::new();
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);

        // Slot is not initialized before first use.
        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert!(!slot.is_initialized());

        // First push triggers lazy init.
        assert!(cc.fe_push(core, A, sc, 42, &m).is_success());
        assert!(slot.is_initialized());
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

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

use crate::bootstrap::MetadataAlloc;
use crate::fe::{CoreId, FeOutcome};
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::{ArenaId, SizeClassId};
use crate::size_class;

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

/// Maximum logical CPUs supported. This bounds the `CpuCache` array size.
pub const MAX_CPUS: usize = 128;

/// Per-size-class slot within a [`PerCpu`]: a lazily-allocated LIFO stack of
/// object addresses.
///
/// The slot is **not self-synchronizing**; all access is serialized by the
/// owning [`PerCpu`]'s spinlock.
pub struct CpuSlot {
    /// Whether the slot has been initialized (buffer allocated).
    initialized: AtomicBool,
    /// Pointer to the metadata-allocated array of `usize` addresses.
    /// Null (0) until initialized.
    buf: AtomicUsize,
    /// Number of valid entries in the buffer.
    len: AtomicU32,
    /// Soft capacity -- the budget controller may grow or shrink this within
    /// `[batch_size, hard_capacity]` (W6-5).
    soft_capacity: AtomicU32,
    /// Hard capacity -- the absolute ceiling for this slot (SS11.5).
    hard_capacity: AtomicU32,
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
pub struct PerCpu {
    /// Spinlock protecting all slots for this CPU.
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
}

impl CpuCache {
    /// A fresh, empty CPU cache (no slots initialized).
    pub const fn new() -> Self {
        Self {
            cpus: [const { PerCpu::new() }; MAX_CPUS],
            active_cpus: AtomicU32::new(0),
        }
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
    pub fn fe_pop(
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
    pub fn fe_push(
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

    /// Pop up to `max` addresses from the per-CPU slot for `(core, sc)` into
    /// `out`. Returns the number of addresses popped. Used by flush operations
    /// (cache_ops W6-3b).
    pub fn pop_batch(&self, core: CoreId, sc: SizeClassId, out: &mut [usize], max: usize) -> usize {
        let cpu = match self.cpus.get(core.index()) {
            Some(c) => c,
            None => return 0,
        };
        let _guard = cpu.lock();
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
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Pass soft_cap > hard_cap: should be clamped to hard_cap.
        assert!(cc.init_slot(core, sc, &m, hard_cap + 100));

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert_eq!(slot.soft_capacity(), hard_cap);
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

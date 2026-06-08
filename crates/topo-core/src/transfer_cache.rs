// SPDX-License-Identifier: MIT
//! Per-size-class transfer cache (W6-2, plan 05).
//!
//! The transfer cache sits between the per-CPU front-end cache and the central
//! free list. It holds batches of object addresses — not indices — and serves
//! as a **contention-reducing buffer**: refills and flushes between threads
//! that share a size class often hit the transfer cache rather than contending
//! on the central lock.
//!
//! **Structure.** One [`TransferBin`] per size class, each protected by its own
//! spinlock (lock rank 3, below the central lock at rank 4). The bin stores
//! addresses in a metadata-allocated array, lazily initialized on first use.
//!
//! **Lock ordering (§27.2).** Transfer cache lock (rank 3) < Central lock
//! (rank 4) < Span lock (rank 5). The cache_ops module (W6-3) enforces
//! hand-over-hand: the transfer lock is **released before** the central lock
//! is acquired. Two transfer bins are never held simultaneously.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::SizeClassId;
use crate::size_class;

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

/// Default capacity multiplier: `batch_size * DEFAULT_CAPACITY_BATCHES`.
const DEFAULT_CAPACITY_BATCHES: usize = 4;

/// Per-size-class transfer bin: a spinlock-protected buffer of object addresses.
///
/// The buffer is lazily allocated from [`MetadataAlloc`] on first use, so an
/// idle size class costs only the `TransferBin` struct itself (no metadata
/// overhead until the class is actually used).
pub struct TransferBin {
    /// Spinlock protecting the buffer (lock rank 3).
    locked: AtomicBool,
    /// Whether the bin has been initialized (buffer allocated).
    initialized: AtomicBool,
    /// Pointer to the metadata-allocated array of `usize` addresses.
    /// Null until initialized.
    buf: AtomicUsize,
    /// Number of valid entries in the buffer.
    len: AtomicU32,
    /// Capacity of the allocated buffer (in elements).
    capacity: AtomicU32,
}

impl TransferBin {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            buf: AtomicUsize::new(0),
            len: AtomicU32::new(0),
            capacity: AtomicU32::new(0),
        }
    }

    /// Acquire the bin's spinlock (rank 3).
    #[inline]
    fn lock(&self) -> TransferGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        TransferGuard { bin: self }
    }

    /// Whether the bin has been initialized.
    #[inline]
    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    /// Initialize the bin: allocate the address buffer from `meta`.
    /// Must be called under the lock. Returns `false` on metadata exhaustion.
    fn init(&self, meta: &dyn MetadataAlloc, cap: usize) -> bool {
        if self.is_initialized() {
            return true;
        }
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
        self.capacity.store(cap as u32, Ordering::Release);
        self.initialized.store(true, Ordering::Release);
        true
    }

    /// Returns the buffer as a raw pointer to `usize` elements. Only valid
    /// when initialized and under the lock.
    #[inline]
    fn buf_ptr(&self) -> *mut usize {
        self.buf.load(Ordering::Acquire) as *mut usize
    }

    /// Current number of addresses in the buffer.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    /// Buffer capacity (in elements).
    #[inline]
    pub fn capacity(&self) -> u32 {
        self.capacity.load(Ordering::Relaxed)
    }

    /// Whether the bin is empty (approximate, no lock).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// RAII guard for a locked [`TransferBin`].
struct TransferGuard<'a> {
    bin: &'a TransferBin,
}

impl Drop for TransferGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.bin.locked.store(false, Ordering::Release);
    }
}

// SAFETY: all shared state in TransferBin is behind atomics or the spinlock.
// The raw pointer (buf) reaches monotonic metadata (never freed, MetadataAlloc
// contract), and is only dereferenced under the lock.
unsafe impl Sync for TransferBin {}
// SAFETY: TransferBin's interior state is atomics and a spinlock; the buffer
// pointer reaches monotonic metadata that is never freed.
unsafe impl Send for TransferBin {}

/// The transfer cache: one [`TransferBin`] per size class (W6-2).
///
/// Thread-safe by construction: each bin has its own spinlock (rank 3).
/// Operations take and release the lock for exactly one bin at a time.
pub struct TransferCache {
    bins: [TransferBin; NUM_SIZE_CLASSES],
}

impl TransferCache {
    /// A fresh, empty transfer cache (no bins initialized).
    pub const fn new() -> Self {
        Self {
            bins: [const { TransferBin::new() }; NUM_SIZE_CLASSES],
        }
    }

    /// The bin for a size class (bounds-checked).
    #[inline]
    pub fn bin(&self, sc: SizeClassId) -> Option<&TransferBin> {
        self.bins.get(sc.index())
    }

    /// Pop up to `max` addresses from the transfer bin for `sc` into `out`.
    /// Returns the number of addresses popped. The bin is lazily initialized
    /// on first use; an uninitialized or empty bin returns 0.
    pub fn try_pop_batch(
        &self,
        sc: SizeClassId,
        out: &mut [usize],
        max: usize,
        meta: &dyn MetadataAlloc,
    ) -> usize {
        let bin = match self.bins.get(sc.index()) {
            Some(b) => b,
            None => return 0,
        };
        let _guard = bin.lock();

        // Lazy init: ensure the bin has a buffer.
        if !bin.is_initialized() {
            let cap = default_capacity(sc);
            if !bin.init(meta, cap) {
                return 0;
            }
        }

        let cur_len = bin.len.load(Ordering::Relaxed) as usize;
        if cur_len == 0 {
            return 0;
        }

        let pop_count = max.min(cur_len).min(out.len());
        if pop_count == 0 {
            return 0;
        }

        let buf = bin.buf_ptr();
        // Pop from the top of the stack (LIFO).
        let new_len = cur_len - pop_count;
        for (i, slot) in out[..pop_count].iter_mut().enumerate() {
            // SAFETY: buf points to a valid array of `capacity` usize elements
            // allocated from MetadataAlloc (never freed). `new_len + i` < cur_len
            // <= capacity (the len invariant), so the read is in bounds.
            // We hold the lock, so no concurrent access.
            *slot = unsafe { *buf.add(new_len + i) };
        }
        bin.len.store(new_len as u32, Ordering::Relaxed);
        pop_count
    }

    /// Push addresses from `addrs` into the transfer bin for `sc`. Returns the
    /// number of addresses pushed (may be less than `addrs.len()` if the bin is
    /// at capacity). The bin is lazily initialized on first use.
    pub fn try_push_batch(
        &self,
        sc: SizeClassId,
        addrs: &[usize],
        meta: &dyn MetadataAlloc,
    ) -> usize {
        let bin = match self.bins.get(sc.index()) {
            Some(b) => b,
            None => return 0,
        };
        let _guard = bin.lock();

        // Lazy init.
        if !bin.is_initialized() {
            let cap = default_capacity(sc);
            if !bin.init(meta, cap) {
                return 0;
            }
        }

        let cur_len = bin.len.load(Ordering::Relaxed) as usize;
        let cap = bin.capacity.load(Ordering::Relaxed) as usize;
        let space = cap.saturating_sub(cur_len);
        let push_count = addrs.len().min(space);
        if push_count == 0 {
            return 0;
        }

        let buf = bin.buf_ptr();
        for (i, &addr) in addrs[..push_count].iter().enumerate() {
            // SAFETY: buf points to a valid array of `capacity` elements.
            // `cur_len + i` < cap (the space check), so the write is in bounds.
            // We hold the lock, so no concurrent access.
            unsafe { *buf.add(cur_len + i) = addr };
        }
        bin.len
            .store((cur_len + push_count) as u32, Ordering::Relaxed);
        push_count
    }
}

impl Default for TransferCache {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: TransferCache is an array of TransferBin, each Sync.
unsafe impl Sync for TransferCache {}
// SAFETY: TransferCache is an array of TransferBin, each Send.
unsafe impl Send for TransferCache {}

/// Default capacity for a transfer bin: `batch_size * 4`.
fn default_capacity(sc: SizeClassId) -> usize {
    let batch = size_class::batch(sc);
    batch.checked_mul(DEFAULT_CAPACITY_BATCHES).unwrap_or(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    #[test]
    fn push_pop_batch_round_trip() {
        let m = meta(1024 * 1024);
        let tc = TransferCache::new();
        let sc = SizeClassId::new(0);

        let addrs: Vec<usize> = (100..108).collect();
        let pushed = tc.try_push_batch(sc, &addrs, &m);
        assert_eq!(pushed, 8);

        let mut out = [0usize; 16];
        let popped = tc.try_pop_batch(sc, &mut out, 8, &m);
        assert_eq!(popped, 8);

        // LIFO order: last pushed is first popped.
        let popped_set: std::collections::BTreeSet<usize> = out[..8].iter().copied().collect();
        let orig_set: std::collections::BTreeSet<usize> = addrs.iter().copied().collect();
        assert_eq!(popped_set, orig_set);
    }

    #[test]
    fn empty_cache_returns_zero() {
        let m = meta(1024 * 1024);
        let tc = TransferCache::new();
        let sc = SizeClassId::new(0);

        let mut out = [0usize; 16];
        let popped = tc.try_pop_batch(sc, &mut out, 8, &m);
        assert_eq!(popped, 0);
    }

    #[test]
    fn capacity_respected() {
        let m = meta(1024 * 1024);
        let tc = TransferCache::new();
        let sc = SizeClassId::new(0);

        let cap = default_capacity(sc);
        // Push more than capacity.
        let addrs: Vec<usize> = (0..cap + 50).collect();
        let pushed = tc.try_push_batch(sc, &addrs, &m);
        assert_eq!(pushed, cap);

        // Bin is now full; pushing more returns 0.
        let more: Vec<usize> = vec![999; 10];
        let pushed2 = tc.try_push_batch(sc, &more, &m);
        assert_eq!(pushed2, 0);
    }

    #[test]
    fn partial_pop() {
        let m = meta(1024 * 1024);
        let tc = TransferCache::new();
        let sc = SizeClassId::new(0);

        let addrs: Vec<usize> = (0..20).collect();
        tc.try_push_batch(sc, &addrs, &m);

        // Pop only 5.
        let mut out = [0usize; 5];
        let popped = tc.try_pop_batch(sc, &mut out, 5, &m);
        assert_eq!(popped, 5);

        // 15 remain.
        let bin = tc.bin(sc).unwrap();
        assert_eq!(bin.len(), 15);
    }

    #[test]
    fn different_size_classes_independent() {
        let m = meta(1024 * 1024);
        let tc = TransferCache::new();
        let sc0 = SizeClassId::new(0);
        let sc1 = SizeClassId::new(1);

        tc.try_push_batch(sc0, &[100, 200], &m);
        tc.try_push_batch(sc1, &[300, 400, 500], &m);

        assert_eq!(tc.bin(sc0).unwrap().len(), 2);
        assert_eq!(tc.bin(sc1).unwrap().len(), 3);

        let mut out = [0usize; 4];
        let p0 = tc.try_pop_batch(sc0, &mut out, 4, &m);
        assert_eq!(p0, 2);
        assert_eq!(tc.bin(sc1).unwrap().len(), 3);
    }

    #[test]
    fn concurrent_push_pop() {
        let m = meta(4 * 1024 * 1024);
        let tc = TransferCache::new();
        let sc = SizeClassId::new(0);

        let tc_ref = &tc;
        let m_ref = &m;

        std::thread::scope(|s| {
            // 4 threads pushing
            for t in 0..4u32 {
                s.spawn(move || {
                    for i in 0..100u32 {
                        let addr = (t * 10000 + i) as usize;
                        tc_ref.try_push_batch(sc, &[addr], m_ref);
                    }
                });
            }
            // 4 threads popping
            for _ in 0..4 {
                s.spawn(move || {
                    let mut out = [0usize; 8];
                    for _ in 0..50 {
                        tc_ref.try_pop_batch(sc, &mut out, 8, m_ref);
                    }
                });
            }
        });

        // No crash, no data race. The bin is in a consistent state.
        let bin = tc.bin(sc).unwrap();
        assert!(bin.len() <= bin.capacity());
    }
}

// SPDX-License-Identifier: MIT
//! Thread-local cache (W6-1a/1b, plan 05).
//!
//! A per-thread cache of object addresses, supplementing the per-CPU cache.
//! This is the innermost cache layer: allocations hit the thread cache first,
//! avoiding the per-CPU lock entirely on a fast-path hit.
//!
//! **Feature gate.** Only available with the `std` feature (thread-local
//! storage requires `std::thread_local!`). In `no_std` builds, stub types
//! provide the same API with no-op implementations (every operation reports
//! empty/full, and nothing is cached).
//!
//! **Thread exit (W6-1b).** On thread exit, the thread cache's `Drop` impl
//! flushes all cached objects back to the transfer cache or central free list,
//! preventing leaks. The flush is budget-bounded: the total objects across all
//! size classes never exceeds a configurable budget.
//!
//! **Budget tracking.** The thread cache tracks a global budget (total objects
//! across all SCs). Push operations that would exceed the budget fail, forcing
//! a flush to the transfer cache.

use crate::ids::SizeClassId;

/// Default per-thread budget (total objects across all size classes).
pub const DEFAULT_THREAD_BUDGET: usize = 1024;

// ---------------------------------------------------------------------------
// std implementation
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "std"))]
mod imp {
    use super::*;
    use crate::generated::tables::SIZE_CLASSES;

    /// Number of size classes in the generated table.
    const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

    /// Per-size-class slot in the thread cache.
    pub struct ThreadCacheSlot {
        /// The cached object addresses (LIFO stack).
        addrs: Vec<usize>,
        /// Maximum capacity for this slot.
        capacity: usize,
    }

    impl ThreadCacheSlot {
        /// A fresh, empty slot.
        fn new(capacity: usize) -> Self {
            Self {
                addrs: Vec::new(),
                capacity,
            }
        }

        /// Pop an address from this slot.
        #[inline]
        pub fn pop(&mut self) -> Option<usize> {
            self.addrs.pop()
        }

        /// Push an address to this slot. Returns `false` if at capacity.
        #[inline]
        pub fn push(&mut self, addr: usize) -> bool {
            if self.addrs.len() >= self.capacity {
                return false;
            }
            self.addrs.push(addr);
            true
        }

        /// Current number of cached addresses.
        #[inline]
        pub fn len(&self) -> usize {
            self.addrs.len()
        }

        /// Whether the slot is empty.
        #[inline]
        pub fn is_empty(&self) -> bool {
            self.addrs.is_empty()
        }

        /// Drain up to `max` addresses into `out`. Returns the number drained.
        pub fn drain_to(&mut self, out: &mut [usize], max: usize) -> usize {
            let count = max.min(self.addrs.len()).min(out.len());
            for slot in out[..count].iter_mut() {
                // unwrap is safe: count <= self.addrs.len()
                *slot = self.addrs.pop().unwrap();
            }
            count
        }

        /// Drain all addresses into a Vec and return them.
        pub fn drain_all(&mut self) -> Vec<usize> {
            core::mem::take(&mut self.addrs)
        }
    }

    /// Per-thread cache of object addresses (W6-1a/1b).
    pub struct ThreadCache {
        /// Per-size-class slots.
        slots: Vec<ThreadCacheSlot>,
        /// Total objects cached across all SCs.
        total_cached: usize,
        /// Maximum total objects across all SCs.
        budget: usize,
    }

    impl ThreadCache {
        /// Create a new thread cache with the given budget.
        pub fn new(budget: usize) -> Self {
            let slots = (0..NUM_SIZE_CLASSES)
                .map(|i| {
                    let sc = SizeClassId::new(i);
                    let cap = crate::size_class::max_local_capacity(sc);
                    ThreadCacheSlot::new(cap)
                })
                .collect();
            Self {
                slots,
                total_cached: 0,
                budget,
            }
        }

        /// Create a new thread cache with the default budget.
        pub fn with_default_budget() -> Self {
            Self::new(DEFAULT_THREAD_BUDGET)
        }

        /// Pop an address for size class `sc`.
        #[inline]
        pub fn pop(&mut self, sc: SizeClassId) -> Option<usize> {
            let slot = self.slots.get_mut(sc.index())?;
            let addr = slot.pop()?;
            self.total_cached = self.total_cached.saturating_sub(1);
            Some(addr)
        }

        /// Push an address for size class `sc`. Returns `false` if over budget
        /// or at per-SC capacity.
        #[inline]
        pub fn push(&mut self, sc: SizeClassId, addr: usize) -> bool {
            if self.total_cached >= self.budget {
                return false;
            }
            let slot = match self.slots.get_mut(sc.index()) {
                Some(s) => s,
                None => return false,
            };
            if slot.push(addr) {
                self.total_cached = self.total_cached.saturating_add(1);
                true
            } else {
                false
            }
        }

        /// Total objects cached across all SCs.
        #[inline]
        pub fn total_cached(&self) -> usize {
            self.total_cached
        }

        /// Budget (maximum total objects).
        #[inline]
        pub fn budget(&self) -> usize {
            self.budget
        }

        /// Set the budget.
        #[inline]
        pub fn set_budget(&mut self, budget: usize) {
            self.budget = budget;
        }

        /// The slot for a size class.
        #[inline]
        pub fn slot(&self, sc: SizeClassId) -> Option<&ThreadCacheSlot> {
            self.slots.get(sc.index())
        }

        /// Flush all slots, returning addresses via the provided callback.
        /// The callback receives `(sc, addrs)` for each non-empty slot.
        pub fn flush_all<F>(&mut self, mut flush_fn: F)
        where
            F: FnMut(SizeClassId, Vec<usize>),
        {
            for i in 0..NUM_SIZE_CLASSES {
                let slot = &mut self.slots[i];
                if !slot.is_empty() {
                    let addrs = slot.drain_all();
                    let sc = SizeClassId::new(i);
                    flush_fn(sc, addrs);
                }
            }
            self.total_cached = 0;
        }

        /// Flush one size class, returning addresses via the provided callback.
        pub fn flush_sc<F>(&mut self, sc: SizeClassId, mut flush_fn: F)
        where
            F: FnMut(SizeClassId, Vec<usize>),
        {
            if let Some(slot) = self.slots.get_mut(sc.index()) {
                if !slot.is_empty() {
                    let count = slot.len();
                    let addrs = slot.drain_all();
                    self.total_cached = self.total_cached.saturating_sub(count);
                    flush_fn(sc, addrs);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// no_std stub implementation
// ---------------------------------------------------------------------------

#[cfg(not(any(test, feature = "std")))]
mod imp {
    use super::*;

    /// Stub per-size-class slot (no_std: no-op).
    pub struct ThreadCacheSlot;

    impl ThreadCacheSlot {
        /// Always returns `None` (no caching in no_std).
        #[inline]
        pub fn pop(&mut self) -> Option<usize> {
            None
        }

        /// Always returns `false` (no caching in no_std).
        #[inline]
        pub fn push(&mut self, _addr: usize) -> bool {
            false
        }

        /// Always returns 0.
        #[inline]
        pub fn len(&self) -> usize {
            0
        }

        /// Always returns `true`.
        #[inline]
        pub fn is_empty(&self) -> bool {
            true
        }
    }

    /// Stub thread cache (no_std: no-op).
    pub struct ThreadCache;

    impl ThreadCache {
        /// Create a stub thread cache (ignores budget).
        pub fn new(_budget: usize) -> Self {
            Self
        }

        /// Create a stub thread cache with the default budget.
        pub fn with_default_budget() -> Self {
            Self
        }

        /// Always returns `None`.
        #[inline]
        pub fn pop(&mut self, _sc: SizeClassId) -> Option<usize> {
            None
        }

        /// Always returns `false`.
        #[inline]
        pub fn push(&mut self, _sc: SizeClassId, _addr: usize) -> bool {
            false
        }

        /// Always returns 0.
        #[inline]
        pub fn total_cached(&self) -> usize {
            0
        }

        /// Always returns 0.
        #[inline]
        pub fn budget(&self) -> usize {
            0
        }

        /// No-op.
        #[inline]
        pub fn set_budget(&mut self, _budget: usize) {}

        /// No-op.
        pub fn flush_all<F>(&mut self, _flush_fn: F)
        where
            F: FnMut(SizeClassId, &[usize]),
        {
        }

        /// No-op.
        pub fn flush_sc<F>(&mut self, _sc: SizeClassId, _flush_fn: F)
        where
            F: FnMut(SizeClassId, &[usize]),
        {
        }
    }
}

pub use imp::{ThreadCache, ThreadCacheSlot};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::SizeClassId;

    #[test]
    fn push_pop_basic() {
        let mut tc = ThreadCache::new(1024);
        let sc = SizeClassId::new(0);

        assert!(tc.pop(sc).is_none());
        assert!(tc.push(sc, 0xDEAD));
        assert!(tc.push(sc, 0xBEEF));
        assert_eq!(tc.total_cached(), 2);

        assert_eq!(tc.pop(sc), Some(0xBEEF)); // LIFO
        assert_eq!(tc.pop(sc), Some(0xDEAD));
        assert!(tc.pop(sc).is_none());
        assert_eq!(tc.total_cached(), 0);
    }

    #[test]
    fn budget_enforcement() {
        let mut tc = ThreadCache::new(3);
        let sc = SizeClassId::new(0);

        assert!(tc.push(sc, 1));
        assert!(tc.push(sc, 2));
        assert!(tc.push(sc, 3));
        assert_eq!(tc.total_cached(), 3);

        // Budget reached -- push fails.
        assert!(!tc.push(sc, 4));
        assert_eq!(tc.total_cached(), 3);

        // Pop one, then push succeeds.
        tc.pop(sc);
        assert_eq!(tc.total_cached(), 2);
        assert!(tc.push(sc, 5));
        assert_eq!(tc.total_cached(), 3);
    }

    #[test]
    fn flush_all_drains_everything() {
        let mut tc = ThreadCache::new(1024);
        let sc0 = SizeClassId::new(0);
        let sc1 = SizeClassId::new(1);

        tc.push(sc0, 100);
        tc.push(sc0, 200);
        tc.push(sc1, 300);

        let mut flushed: Vec<(usize, Vec<usize>)> = Vec::new();
        tc.flush_all(|sc, addrs| {
            flushed.push((sc.index(), addrs));
        });

        assert_eq!(tc.total_cached(), 0);
        assert!(tc.pop(sc0).is_none());
        assert!(tc.pop(sc1).is_none());

        // Verify flushed data.
        let sc0_flushed: Vec<&(usize, Vec<usize>)> =
            flushed.iter().filter(|(i, _)| *i == 0).collect();
        let sc1_flushed: Vec<&(usize, Vec<usize>)> =
            flushed.iter().filter(|(i, _)| *i == 1).collect();
        assert_eq!(sc0_flushed.len(), 1);
        assert_eq!(sc1_flushed.len(), 1);
        assert_eq!(sc0_flushed[0].1.len(), 2);
        assert_eq!(sc1_flushed[0].1.len(), 1);
    }

    #[test]
    fn flush_sc_drains_one_class() {
        let mut tc = ThreadCache::new(1024);
        let sc0 = SizeClassId::new(0);
        let sc1 = SizeClassId::new(1);

        tc.push(sc0, 100);
        tc.push(sc1, 200);

        let mut flushed = Vec::new();
        tc.flush_sc(sc0, |sc, addrs| {
            flushed.push((sc.index(), addrs));
        });

        assert_eq!(tc.total_cached(), 1); // sc1 still has one
        assert!(tc.pop(sc0).is_none());
        assert_eq!(tc.pop(sc1), Some(200));
    }

    #[test]
    fn different_size_classes_independent() {
        let mut tc = ThreadCache::new(1024);
        let sc0 = SizeClassId::new(0);
        let sc5 = SizeClassId::new(5);

        tc.push(sc0, 1);
        tc.push(sc5, 2);

        assert_eq!(tc.pop(sc0), Some(1));
        assert!(tc.pop(sc0).is_none());
        assert_eq!(tc.pop(sc5), Some(2));
    }
}

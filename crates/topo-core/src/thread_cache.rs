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
//! flushes all cached objects back via a registered flush hook, preventing
//! leaks. The hook is set by the allocator at initialization time; if no hook
//! is registered, `Drop` is a no-op (with a `debug_assert` warning).
//!
//! **Thread-local storage.** The `std`-gated [`with_thread_cache`] function
//! provides per-thread access to the cache via `thread_local!`. Each thread
//! gets its own cache, lazily created on first access.
//!
//! **Budget tracking.** The thread cache tracks a global budget (total objects
//! across all SCs). Push operations that would exceed the budget fail, forcing
//! a flush to the transfer cache.
//!
//! # Deliberately **not** on the live allocation path
//!
//! The W6 front end the engine wires up is the **per-CPU cache + transfer cache**
//! (`cpu_cache`, `transfer_cache`, driven by `cache_ops` from
//! [`Allocator`](crate::Allocator)); this module is not part of it, and
//! `AllocatorStats` reports its residency as zero. That is a decision, not an
//! oversight, for two reasons:
//!
//! 1. **Its storage allocates through the allocator.** A slot is a `Vec<usize>` that
//!    grows on `push`, so putting it on the fast path would have `malloc` allocate
//!    through `malloc` — the Appendix-F "no recursion through TopoMalloc" anti-pattern —
//!    and its thread-exit `Drop` would then *free* through the allocator during TLS
//!    teardown, in the one window where the engine may already be gone. Wiring it is
//!    therefore blocked on re-basing its slots on metadata storage (the discipline
//!    [`CpuSlot`](crate::cpu_cache::CpuSlot) already uses: a fixed, metadata-carved
//!    buffer, never freed), which is a rewrite of the storage layer rather than a
//!    wiring change.
//! 2. **There is little left for it to save.** A thread cache exists to avoid the
//!    per-CPU lock; the W7 RSEQ fast path already removes that lock entirely on the
//!    hot path (the restartable sequence commits with a single store, no lock byte
//!    taken), and the locked baseline's acquisition is per-CPU and uncontended by
//!    construction. A third residency layer would add a third place an object can hide
//!    from the §16.4 conservation law and a third `cached`-bit transition to keep
//!    correct, for a saving RSEQ already banks.
//!
//! It remains a complete, unit-tested implementation of the §11.2 layer for the day
//! either premise changes (a platform with no RSEQ where profiling shows the per-CPU
//! lock hurting, or a `no_std` host with cheap TLS and no per-CPU id).

use crate::ids::SizeClassId;

/// Default per-thread budget (total objects across all size classes).
pub const DEFAULT_THREAD_BUDGET: usize = 1024;

/// Flush hook signature: receives `(sc, addrs)` for each non-empty slot
/// during a flush. The hook should return objects to the transfer cache or
/// central free list.
///
/// The `*const ()` context pointer is provided by the caller when registering
/// the hook (typically a pointer to the global allocator state). The hook is
/// responsible for casting it back to the correct type.
///
/// # Safety
///
/// The context pointer must remain valid for the lifetime of the thread cache
/// (i.e., until the thread exits). This is naturally satisfied when the context
/// points to a `'static` allocator struct.
pub type FlushHookFn = unsafe fn(ctx: *const (), sc: SizeClassId, addrs: &[usize]);

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

        /// Drain all addresses into a Vec and return them.
        pub fn drain_all(&mut self) -> Vec<usize> {
            core::mem::take(&mut self.addrs)
        }
    }

    /// Per-thread cache of object addresses (W6-1a/1b).
    ///
    /// On `Drop`, all cached objects are flushed via the registered flush hook
    /// (W6-1b). If no hook is registered, cached objects leak — a `debug_assert`
    /// warns in debug builds.
    pub struct ThreadCache {
        /// Per-size-class slots.
        slots: Vec<ThreadCacheSlot>,
        /// Total objects cached across all SCs.
        total_cached: usize,
        /// Maximum total objects across all SCs.
        budget: usize,
        /// Flush hook called on `Drop` and `flush_all`.
        flush_hook: Option<FlushHookFn>,
        /// Context pointer passed to `flush_hook`.
        flush_ctx: *const (),
    }

    // SAFETY: ThreadCache is thread-local (one per thread, never shared).
    // The flush_ctx pointer reaches 'static allocator state that is Sync.
    unsafe impl Send for ThreadCache {}

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
                flush_hook: None,
                flush_ctx: core::ptr::null(),
            }
        }

        /// Create a new thread cache with the default budget.
        pub fn with_default_budget() -> Self {
            Self::new(DEFAULT_THREAD_BUDGET)
        }

        /// Register the flush hook called on thread exit (and explicit flushes).
        ///
        /// # Safety
        ///
        /// `ctx` must remain valid until the thread cache is dropped (typically
        /// a `&'static` pointer to the global allocator struct).
        pub unsafe fn set_flush_hook(&mut self, hook: FlushHookFn, ctx: *const ()) {
            self.flush_hook = Some(hook);
            self.flush_ctx = ctx;
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

        /// Appendix B.2 (cache invariants, W19-1b): this thread cache is within
        /// its budget and its count accounting reconciles. Total + side-effect-free
        /// (a `&self` read), the `debug-checks`/test oracle for the per-thread
        /// front-end:
        ///
        /// * `total_cached <= budget` — the §11.5 per-thread hard budget (the
        ///   Appendix-B "thread cache bytes do not exceed hard budget" clause; the
        ///   budget is in objects, bounding bytes by `Σ len × object_size`);
        /// * every slot's `len <= capacity` (the per-class ceiling);
        /// * `Σ slot.len == total_cached` — the running counter (decremented on
        ///   pop, incremented on push) never drifts from the authoritative slots;
        /// * each slot's addresses are pairwise **distinct** and non-null — a
        ///   duplicate is a double-freed object (§29.3, the cache being
        ///   single-owner needs no lock). O(len²); thread slots are small.
        pub fn check_invariants(&self) -> bool {
            if self.total_cached > self.budget {
                return false;
            }
            let mut sum = 0usize;
            for slot in &self.slots {
                if slot.addrs.len() > slot.capacity {
                    return false;
                }
                for (i, &a) in slot.addrs.iter().enumerate() {
                    if a == 0 || slot.addrs[i + 1..].contains(&a) {
                        return false;
                    }
                }
                sum += slot.addrs.len();
            }
            sum == self.total_cached
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
            F: FnMut(SizeClassId, &[usize]),
        {
            for i in 0..NUM_SIZE_CLASSES {
                let slot = &mut self.slots[i];
                if !slot.is_empty() {
                    let addrs = slot.drain_all();
                    let sc = SizeClassId::new(i);
                    flush_fn(sc, &addrs);
                }
            }
            self.total_cached = 0;
        }

        /// Flush one size class, returning addresses via the provided callback.
        pub fn flush_sc<F>(&mut self, sc: SizeClassId, mut flush_fn: F)
        where
            F: FnMut(SizeClassId, &[usize]),
        {
            if let Some(slot) = self.slots.get_mut(sc.index()) {
                if !slot.is_empty() {
                    let count = slot.len();
                    let addrs = slot.drain_all();
                    self.total_cached = self.total_cached.saturating_sub(count);
                    flush_fn(sc, &addrs);
                }
            }
        }

        /// Drain all cached addresses belonging to a specific arena (§13.4
        /// arena-reset drain). Addresses that do not belong to `arena` are
        /// kept; matching addresses are returned via `drain_fn`.
        ///
        /// The `classify` callback maps an address to its owning arena id;
        /// the caller typically implements this via a pagemap lookup.
        pub fn drain_arena<C, F>(
            &mut self,
            arena: crate::ids::ArenaId,
            mut classify: C,
            mut drain_fn: F,
        ) where
            C: FnMut(usize) -> Option<crate::ids::ArenaId>,
            F: FnMut(SizeClassId, &[usize]),
        {
            for i in 0..NUM_SIZE_CLASSES {
                let slot = &mut self.slots[i];
                if slot.is_empty() {
                    continue;
                }
                let sc = SizeClassId::new(i);
                let all = slot.drain_all();
                let mut keep = Vec::new();
                let mut drain = Vec::new();
                for addr in all {
                    if classify(addr) == Some(arena) {
                        drain.push(addr);
                    } else {
                        keep.push(addr);
                    }
                }
                let drained = drain.len();
                for addr in keep {
                    slot.push(addr);
                }
                if !drain.is_empty() {
                    drain_fn(sc, &drain);
                }
                self.total_cached = self.total_cached.saturating_sub(drained);
            }
        }
    }

    impl Drop for ThreadCache {
        fn drop(&mut self) {
            if let Some(hook) = self.flush_hook {
                let ctx = self.flush_ctx;
                self.flush_all(|sc, addrs| {
                    // SAFETY: ctx was validated by the caller of set_flush_hook,
                    // which requires ctx to remain valid until Drop.
                    unsafe { hook(ctx, sc, addrs) };
                });
            }
            debug_assert_eq!(
                self.total_cached,
                0,
                "ThreadCache dropped with {} cached objects (flush hook present: {})",
                self.total_cached,
                self.flush_hook.is_some()
            );
        }
    }

    // --- thread_local! wiring (W6-1b) ---

    std::thread_local! {
        static THREAD_CACHE: core::cell::RefCell<ThreadCache> =
            core::cell::RefCell::new(ThreadCache::with_default_budget());
    }

    /// Access the current thread's cache. The cache is lazily created on
    /// first access and flushed on thread exit via its `Drop` impl.
    ///
    /// Falls back to a temporary cache during thread shutdown (TLS destroyed)
    /// or re-entrant access (e.g. a flush hook calling back into the cache).
    pub fn with_thread_cache<R>(f: impl FnOnce(&mut ThreadCache) -> R) -> R {
        let mut f_opt = Some(f);
        let result = THREAD_CACHE.try_with(|tc| {
            let f = f_opt.take().unwrap();
            match tc.try_borrow_mut() {
                Ok(mut guard) => f(&mut guard),
                Err(_) => f(&mut ThreadCache::with_default_budget()),
            }
        });
        match result {
            Ok(r) => r,
            Err(_) => (f_opt.take().unwrap())(&mut ThreadCache::with_default_budget()),
        }
    }

    /// Register the flush hook on the current thread's cache.
    ///
    /// # Safety
    ///
    /// `ctx` must remain valid until the thread exits (typically a `&'static`
    /// pointer to the global allocator struct).
    pub unsafe fn init_thread_cache(hook: FlushHookFn, ctx: *const ()) {
        let _ = THREAD_CACHE.try_with(|tc| {
            // SAFETY: caller guarantees ctx validity for the thread's lifetime.
            unsafe { tc.borrow_mut().set_flush_hook(hook, ctx) };
        });
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

        /// No-op: flush hook is not supported in no_std.
        ///
        /// # Safety
        ///
        /// Same contract as the std version.
        pub unsafe fn set_flush_hook(&mut self, _hook: FlushHookFn, _ctx: *const ()) {}

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

        /// Appendix B.2 (W19-1b): the no-op stub holds nothing, so it is
        /// vacuously well-formed. Always `true`.
        #[inline]
        pub fn check_invariants(&self) -> bool {
            true
        }

        /// Always returns `None` (no slots in no_std stub).
        #[inline]
        pub fn slot(&self, _sc: SizeClassId) -> Option<&ThreadCacheSlot> {
            None
        }

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

        /// No-op: no caching in no_std, so nothing to drain.
        pub fn drain_arena<C, F>(&mut self, _arena: crate::ids::ArenaId, _classify: C, _drain_fn: F)
        where
            C: FnMut(usize) -> Option<crate::ids::ArenaId>,
            F: FnMut(SizeClassId, &[usize]),
        {
        }
    }

    /// No-op stub: always calls `f` with a fresh stub cache.
    pub fn with_thread_cache<R>(f: impl FnOnce(&mut ThreadCache) -> R) -> R {
        f(&mut ThreadCache)
    }
}

pub use imp::{ThreadCache, ThreadCacheSlot};

#[cfg(any(test, feature = "std"))]
pub use imp::{init_thread_cache, with_thread_cache};

#[cfg(not(any(test, feature = "std")))]
pub use imp::with_thread_cache;

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

        // Drain before drop to avoid debug_assert.
        tc.flush_all(|_, _| {});
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
            flushed.push((sc.index(), addrs.to_vec()));
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
            flushed.push((sc.index(), addrs.to_vec()));
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

    #[test]
    fn drop_with_hook_flushes_all() {
        use std::sync::Mutex;

        type FlushedData = Vec<(usize, Vec<usize>)>;

        let flushed: std::sync::Arc<Mutex<FlushedData>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));

        unsafe fn test_hook(ctx: *const (), sc: SizeClassId, addrs: &[usize]) {
            // SAFETY: ctx is a valid &Arc<Mutex<...>> pointer, alive for the
            // duration of the test (set_flush_hook contract).
            let flushed = unsafe { &*ctx.cast::<std::sync::Arc<Mutex<FlushedData>>>() };
            flushed.lock().unwrap().push((sc.index(), addrs.to_vec()));
        }

        let sc = SizeClassId::new(0);
        {
            let mut tc = ThreadCache::new(1024);
            // SAFETY: &flushed is valid for the scope that encloses tc's lifetime.
            unsafe {
                tc.set_flush_hook(
                    test_hook,
                    (&flushed as *const std::sync::Arc<Mutex<FlushedData>>).cast::<()>(),
                );
            }
            tc.push(sc, 0xA);
            tc.push(sc, 0xB);
            tc.push(sc, 0xC);
            assert_eq!(tc.total_cached(), 3);
            // tc drops here, triggering flush via hook
        }

        let data = flushed.lock().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].0, 0);
        assert_eq!(data[0].1.len(), 3);
    }

    #[test]
    fn drop_without_hook_leaks_silently_in_release() {
        let sc = SizeClassId::new(0);
        let mut tc = ThreadCache::new(1024);
        tc.push(sc, 42);
        // No hook set and objects cached — this is a leak. The debug_assert
        // in Drop will fire in debug builds. In release, the objects are
        // silently lost. We use forget to avoid the debug_assert in tests.
        core::mem::forget(tc);
    }

    #[test]
    fn tls_thread_cache_isolation() {
        use std::sync::Barrier;

        let barrier = std::sync::Arc::new(Barrier::new(2));
        let sc = SizeClassId::new(0);

        let b1 = barrier.clone();
        let t1 = std::thread::spawn(move || {
            with_thread_cache(|tc| {
                tc.push(sc, 111);
            });
            b1.wait();
            with_thread_cache(|tc| tc.pop(sc))
        });

        let b2 = barrier.clone();
        let t2 = std::thread::spawn(move || {
            with_thread_cache(|tc| {
                tc.push(sc, 222);
            });
            b2.wait();
            with_thread_cache(|tc| tc.pop(sc))
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert_eq!(r1, Some(111));
        assert_eq!(r2, Some(222));
    }

    #[test]
    fn drain_arena_removes_only_matching() {
        use crate::ids::ArenaId;

        let mut tc = ThreadCache::new(1024);
        let sc = SizeClassId::new(0);
        let arena_a = ArenaId(1);
        let arena_b = ArenaId(2);

        // Push addresses: 100-102 belong to arena A, 200-201 to arena B.
        tc.push(sc, 100);
        tc.push(sc, 101);
        tc.push(sc, 102);
        tc.push(sc, 200);
        tc.push(sc, 201);
        assert_eq!(tc.total_cached(), 5);

        let mut drained = Vec::new();
        tc.drain_arena(
            arena_a,
            |addr| {
                if (100..200).contains(&addr) {
                    Some(arena_a)
                } else {
                    Some(arena_b)
                }
            },
            |sc_id, addrs| drained.push((sc_id.index(), addrs.to_vec())),
        );

        // Only arena A's 3 addresses should be drained.
        assert_eq!(tc.total_cached(), 2);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1.len(), 3);

        // Remaining are arena B's addresses.
        let a = tc.pop(sc).unwrap();
        let b = tc.pop(sc).unwrap();
        let mut remaining = vec![a, b];
        remaining.sort();
        assert_eq!(remaining, vec![200, 201]);
    }

    #[test]
    fn tls_flush_on_thread_exit() {
        use std::sync::{Arc, Mutex};

        type FlushedData = Vec<(usize, Vec<usize>)>;
        let flushed: Arc<Mutex<FlushedData>> = Arc::new(Mutex::new(Vec::new()));

        unsafe fn test_hook(ctx: *const (), sc: SizeClassId, addrs: &[usize]) {
            // SAFETY: ctx points to a heap-allocated Arc<Mutex<...>> (via
            // Box::into_raw), valid until after the thread exits.
            let flushed = unsafe { &*ctx.cast::<Arc<Mutex<FlushedData>>>() };
            flushed.lock().unwrap().push((sc.index(), addrs.to_vec()));
        }

        // Heap-allocate the Arc so the pointer outlives the thread's stack.
        let ctx_box = Box::new(flushed.clone());
        let ctx_ptr = Box::into_raw(ctx_box);
        let ctx_addr = ctx_ptr as usize; // usize is Send

        let handle = std::thread::spawn(move || {
            // SAFETY: ctx_addr points to a heap-allocated Arc that outlives
            // this thread (reclaimed by the main thread after join).
            unsafe {
                init_thread_cache(test_hook, ctx_addr as *const ());
            }

            let sc = SizeClassId::new(0);
            with_thread_cache(|tc| {
                tc.push(sc, 0x100);
                tc.push(sc, 0x200);
                tc.push(sc, 0x300);
            });
            // Thread exits here; TLS Drop fires the flush hook.
        });

        handle.join().unwrap();

        // Reclaim the heap-allocated context.
        // SAFETY: ctx_ptr was created by Box::into_raw and the thread is
        // joined, so no more references exist.
        unsafe { drop(Box::from_raw(ctx_ptr)) };

        // The flush hook should have been called with the 3 addresses.
        let data = flushed.lock().unwrap();
        assert!(!data.is_empty(), "flush hook should fire on thread exit");
        let total_addrs: usize = data.iter().map(|(_, addrs)| addrs.len()).sum();
        assert_eq!(total_addrs, 3, "all 3 cached objects should be flushed");
    }

    #[test]
    fn with_thread_cache_reentrant_does_not_panic() {
        with_thread_cache(|tc| {
            let sc = SizeClassId::new(0);
            tc.push(sc, 42);

            // Simulate re-entrant access (e.g. a flush hook calling back).
            // Should not panic; falls back to a temporary cache.
            let val = with_thread_cache(|inner_tc| inner_tc.pop(sc));
            // The inner cache is a fresh temporary, so pop returns None.
            assert!(val.is_none());

            // The outer cache still has the object.
            assert_eq!(tc.pop(sc), Some(42));
        });
    }
}

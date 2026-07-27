// SPDX-License-Identifier: MIT
//! The §11 front end is **live in the shipped artifact** (plan 05 W6/W7).
//!
//! `topo-core`'s own tests construct an engine directly, so they prove the per-CPU and
//! transfer caches work — not that `topo-abi`'s `global()` actually wires them into the
//! process allocator. A regression there (the §35.4 phase-4 block dropped, an init step
//! reordered away, a cacheability predicate that stops matching) would leave every
//! `topo-core` test green while the shipped `malloc`/`free` quietly ran the pre-W6
//! central path at full central-lock contention. These tests close that gap by observing
//! the front end **through the public C surface only** — `topomalloc_malloc`/`_free`, the
//! `topomalloc_cache_*` controls, and the stats snapshot — so they also pin that surface.
//!
//! **Its own binary, and serialized.** The engine is process-wide, so a sibling test
//! allocating concurrently would move the residency counters under an assertion about
//! them. Every test here takes [`SERIAL`] and drains the front end on entry, so each one
//! observes a heap only it is perturbing.

use std::sync::Mutex;

use topo_abi::{
    topomalloc_cache_budget_tick, topomalloc_cache_flush_all, topomalloc_cache_rseq_active,
    topomalloc_debug_check_now, topomalloc_free, topomalloc_malloc, topomalloc_stats_snapshot,
    topomalloc_stats_t,
};

/// Serializes this binary's tests: they assert on process-wide residency counters.
/// Poison-tolerant (a panicking test must not wedge the rest).
static SERIAL: Mutex<()> = Mutex::new(());

/// A size class no other test in this binary uses, so its residency is unambiguous.
const SIZE: usize = 96;

/// A stats snapshot through the public C entry point.
fn stats() -> topomalloc_stats_t {
    let mut out = topomalloc_stats_t::default();
    // SAFETY: `out` is a valid, writable `topomalloc_stats_t`; flags `0` is the default
    // (summary-only) view.
    let ok = unsafe { topomalloc_stats_snapshot(&mut out, 0) };
    assert_eq!(
        ok, 0,
        "topomalloc_stats_snapshot must succeed for flags = 0"
    );
    out
}

/// Take the lock and drain the front end, so the test starts from a known-empty cache.
fn quiesce(_g: &std::sync::MutexGuard<'_, ()>) {
    topomalloc_cache_flush_all();
}

/// The core claim: a `free` through the real C entry point leaves the object in the front
/// end, and the next same-class `malloc` takes it straight back out. `cache_bytes` is
/// identically zero unless the cache is wired into the live path, so this cannot pass
/// vacuously.
#[test]
fn a_freed_object_is_held_in_the_front_end_and_vended_back() {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);
    assert_eq!(stats().cache_bytes, 0, "the drain emptied the front end");

    let p = topomalloc_malloc(SIZE);
    assert!(!p.is_null());
    // SAFETY: `p` was just returned by `topomalloc_malloc` and is still owned here.
    unsafe { topomalloc_free(p) };

    let held = stats().cache_bytes;
    assert!(
        held >= SIZE as u64,
        "a freed small object must land in the front end (cache_bytes = {held})"
    );

    // LIFO: the same object comes back.
    let q = topomalloc_malloc(SIZE);
    assert_eq!(p, q, "the front end vends the most recently freed object");
    // SAFETY: as above.
    unsafe { topomalloc_free(q) };
    topomalloc_cache_flush_all();
}

/// §29.3 through the real entry points: a double free of a **cache-resident** object is
/// rejected, and the object is still vended exactly once afterwards. The central bitmap
/// never saw it, so only the W6 per-object residency bit can catch this.
#[test]
fn a_double_free_of_a_cached_object_is_caught_by_the_abi() {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);

    let p = topomalloc_malloc(SIZE);
    assert!(!p.is_null());
    // SAFETY: `p` is live and owned here.
    unsafe { topomalloc_free(p) };
    let cached = stats().cache_bytes;
    // SAFETY: deliberately freeing an already-freed pointer — the property under test is
    // exactly that the allocator detects this and mutates nothing. `topomalloc_free` is
    // total on a double free (it reports and returns).
    unsafe { topomalloc_free(p) };
    assert_eq!(
        stats().cache_bytes,
        cached,
        "a rejected double free must not enter the object into the cache a second time"
    );

    let a = topomalloc_malloc(SIZE);
    let b = topomalloc_malloc(SIZE);
    assert_ne!(a, b, "the double-freed object was vended exactly once");
    // SAFETY: two distinct live allocations owned here.
    unsafe {
        topomalloc_free(a);
        topomalloc_free(b);
    }
    topomalloc_cache_flush_all();
    assert_eq!(topomalloc_debug_check_now(), 1);
}

/// §35.4 phase 4 published the machine to the front end. The CPU count is what sizes the
/// §11.5 budget and spreads the slots; without it the W6-5 controller has nothing to
/// visit and returns 0, which is what makes this check non-vacuous.
#[test]
fn phase_four_published_the_machine_to_the_front_end() {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);

    let mut ptrs = Vec::new();
    for _ in 0..400 {
        ptrs.push(topomalloc_malloc(SIZE));
    }
    for p in ptrs {
        // SAFETY: live allocations owned here.
        unsafe { topomalloc_free(p) };
    }
    assert!(
        topomalloc_cache_budget_tick() > 0,
        "phase 4 must publish the CPU count — without it the W6-5 controller is inert"
    );

    // The RSEQ fast path is platform-dependent, so its absence is not a failure: the
    // locked baseline is the always-correct alternative and the tests above already
    // exercise whichever one this machine selected. Assert only that the query is stable
    // and that allocation works in the reported mode.
    let mode = topomalloc_cache_rseq_active();
    assert_eq!(
        mode,
        topomalloc_cache_rseq_active(),
        "the mode query is stable"
    );
    let p = topomalloc_malloc(SIZE);
    assert!(!p.is_null(), "allocation works with rseq_active={mode}");
    // SAFETY: live allocation owned here.
    unsafe { topomalloc_free(p) };
    topomalloc_cache_flush_all();
}

/// The drain is complete: after it, **both** front-end layers are empty. A drain that
/// only emptied the per-CPU slots would push their contents one layer down into the
/// transfer cache and report success while the spans stayed non-empty — so their backing
/// stayed unreclaimable and the §21.3 "drain caches" rung would be a no-op in effect.
#[test]
fn draining_the_front_end_empties_both_layers() {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);

    let mut ptrs = Vec::new();
    for _ in 0..2000 {
        ptrs.push(topomalloc_malloc(SIZE));
    }
    for p in ptrs {
        // SAFETY: live allocations owned here.
        unsafe { topomalloc_free(p) };
    }
    // 2000 objects far exceed one slot's soft capacity, so this residency spans *both*
    // layers — the per-CPU slots and what they overflowed into the transfer cache.
    assert!(
        stats().cache_bytes > 0,
        "the burst must have left residency in the front end"
    );

    let moved = topomalloc_cache_flush_all();
    assert!(moved > 0, "the drain reports what it returned to central");
    let d = stats();
    assert_eq!(
        d.cache_bytes, 0,
        "both front-end layers are empty after a drain"
    );
    assert_eq!(d.live_bytes, 0);
    assert_eq!(topomalloc_debug_check_now(), 1);
}

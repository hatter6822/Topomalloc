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

use std::ffi::CStr;

use topo_abi::{
    topo_dallocx, topo_mallocx, topo_sdallocx, topomalloc_cache_budget_tick,
    topomalloc_cache_flush_all, topomalloc_cache_rseq_active, topomalloc_debug_check_now,
    topomalloc_free, topomalloc_malloc, topomalloc_malloc_usable_size, topomalloc_stats_json,
    topomalloc_stats_snapshot, topomalloc_stats_t, TOPO_TCACHE_NONE,
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

/// §10.3 `TOPO_TCACHE_NONE` means what it says on **both** sides: an allocation carrying
/// it is served from the central free list rather than a per-CPU slot, and a free carrying
/// it returns the object straight to central rather than parking it in one.
///
/// This is the flag's whole observable contract — both routes are equally correct, so
/// nothing but a test distinguishes "honoured" from "decoded and discarded", which is what
/// it was before there was a front end to bypass.
///
/// Asserted as **deltas**, never absolutes: a `malloc` that misses refills a whole batch
/// into the slot, so front-end residency after any single operation is a batch-sized
/// number, not zero. What the flag controls is whether an operation moves one object
/// across the front-end boundary — exactly what a delta measures.
#[test]
fn tcache_none_bypasses_the_front_end_on_both_sides() {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);
    assert_eq!(stats().cache_bytes, 0);

    // Prime the slot so both directions have somewhere to move an object to or from, and
    // learn the class's usable size — residency is counted in usable bytes, not requested.
    let prime = topomalloc_malloc(SIZE);
    assert!(!prime.is_null());
    let obj = topomalloc_malloc_usable_size(prime) as u64;
    assert!(obj >= SIZE as u64);

    // --- free side ----------------------------------------------------------------
    let before = stats().cache_bytes;
    assert!(
        before > 0,
        "the priming miss refilled a batch into the slot"
    );
    // SAFETY: `prime` is live and owned here; the flag word is valid.
    unsafe { topo_dallocx(prime.cast(), TOPO_TCACHE_NONE) };
    assert_eq!(
        stats().cache_bytes,
        before,
        "a TOPO_TCACHE_NONE free must go straight to central, adding no residency"
    );

    // Not vacuous: the *unflagged* free of the same shape does add exactly one object.
    let p = topomalloc_malloc(SIZE); // pops one from the slot
    let mid = stats().cache_bytes;
    // SAFETY: `p` is live and owned here.
    unsafe { topo_dallocx(p.cast(), 0) };
    assert_eq!(
        stats().cache_bytes,
        mid + obj,
        "an unflagged free is absorbed by the front end"
    );

    // --- alloc side ---------------------------------------------------------------
    let held = stats().cache_bytes;
    let q = topo_mallocx(SIZE, TOPO_TCACHE_NONE);
    assert!(!q.is_null());
    assert_eq!(
        stats().cache_bytes,
        held,
        "a TOPO_TCACHE_NONE allocation must come from central, not from the slot"
    );
    // Not vacuous: the *unflagged* allocation right after it does pop one.
    let r = topomalloc_malloc(SIZE);
    assert!(!r.is_null());
    assert_eq!(
        stats().cache_bytes,
        held - obj,
        "an unflagged allocation is served from the front end"
    );

    // --- sized free carries the flag too (`topo_sdallocx`) ------------------------
    let before_sized = stats().cache_bytes;
    // SAFETY: `q` is a live allocation owned here, freed once with a truthful size hint.
    unsafe { topo_sdallocx(q.cast(), SIZE, TOPO_TCACHE_NONE) };
    assert_eq!(
        stats().cache_bytes,
        before_sized,
        "a TOPO_TCACHE_NONE sized free must go straight to central"
    );

    // SAFETY: `r` is a live allocation owned here.
    unsafe { topomalloc_free(r) };
    topomalloc_cache_flush_all();
    assert_eq!(topomalloc_debug_check_now(), 1);
}

/// §31.2 `BY_CPU` renders the **real** per-core front-end residency. The flag predates the
/// front end and used to emit a hard-coded empty array, which was honest only while no
/// cache existed; now the array is empty exactly when the front end is, and otherwise
/// names the cores holding objects.
#[test]
fn by_cpu_detail_reconciles_with_the_front_end_total() {
    /// `StatsFlags::BY_CPU` (bit 2).
    const BY_CPU: u64 = 1 << 2;

    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    quiesce(&g);

    let json = |flags: u64| -> String {
        let mut buf = vec![0i8; 64 * 1024];
        // SAFETY: `buf` is a valid writable buffer of `len` bytes; the renderer
        // NUL-terminates within it.
        let n = unsafe { topomalloc_stats_json(buf.as_mut_ptr(), buf.len(), flags) };
        assert!(n > 0, "the renderer must produce output");
        // SAFETY: the renderer wrote a NUL-terminated string into `buf`.
        unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };

    // Drained: no core holds anything, so the array is empty — and present, not missing
    // (the flag's shape is additive, §35.3).
    let empty = json(BY_CPU);
    assert!(
        empty.contains("\"by_cpu\": []"),
        "a drained front end renders an empty by_cpu array:\n{empty}"
    );

    // Populated: a burst leaves residency, and the per-core lines must sum to the
    // `cache_bytes` total minus whatever the transfer cache holds.
    let mut ptrs = Vec::new();
    for _ in 0..500 {
        ptrs.push(topomalloc_malloc(SIZE));
    }
    for p in ptrs {
        // SAFETY: live allocations owned here.
        unsafe { topomalloc_free(p) };
    }
    let live = json(BY_CPU);
    assert!(
        live.contains("\"cpu\":") && live.contains("\"objects\":") && live.contains("\"bytes\":"),
        "a populated front end names the cores holding objects:\n{live}"
    );
    let v: serde_json::Value = serde_json::from_str(&live).expect("valid JSON");
    let lines = v["by_cpu"].as_array().expect("by_cpu is an array");
    assert!(!lines.is_empty(), "some core holds the burst's residency");
    let per_cpu_total: u64 = lines.iter().map(|l| l["bytes"].as_u64().unwrap()).sum();
    assert!(
        per_cpu_total > 0 && per_cpu_total <= stats().cache_bytes,
        "the per-core lines are a real decomposition of the front-end total \
         ({per_cpu_total} vs {})",
        stats().cache_bytes
    );

    topomalloc_cache_flush_all();
    assert!(
        json(BY_CPU).contains("\"by_cpu\": []"),
        "the drain empties it again"
    );
    assert_eq!(topomalloc_debug_check_now(), 1);
}

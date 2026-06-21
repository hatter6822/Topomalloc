// SPDX-License-Identifier: MIT
//! End-to-end statistics/observability tests (§31, §8.6, plan 07 W17).
//!
//! These exercise the whole stats stack over a **live** allocator: the §31.1 byte classes
//! flow out of `Allocator::stats`, map through `topo_stats::Stats::record_allocator`, and
//! render to the Appendix-D JSON; the §8.6 reconciliation identities hold (the W17-1b
//! acceptance, "a snapshot reconciles to managed VM modulo the documented convention"); the
//! §31.1 `arena.destroyed_count`, §31.5 fragmentation, §31.6 explanation, and §36.12
//! label-scoped redaction all behave. The pure renderer is unit-tested in `topo-stats`; here
//! it is driven by real traffic, sequential and concurrent.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use topo_backend_posix::PosixBackingProvider;
use topo_core::generated::tables::HUGE_THRESHOLD;
use topo_core::ids::ArenaId;
use topo_core::{Allocator, AllocatorConfig, ArenaPolicy, MetaArena, PageMap, RequestFlags};
use topo_stats::{redact_arenas, ArenaLine, Stats, StatsDetail, StatsFlags};

type Engine = Allocator<'static, PosixBackingProvider>;

/// A leaked engine over POSIX (the M1 extent path), valid for the process. Leaking keeps the
/// `'static` borrow simple and lets the engine be shared `&'static` across scoped threads.
fn engine() -> &'static Engine {
    let cfg = AllocatorConfig::small();
    let meta: &'static MetaArena<PosixBackingProvider> = Box::leak(Box::new(
        MetaArena::reserve(
            PosixBackingProvider::new(),
            ArenaId::DEFAULT,
            8 * 1024 * 1024,
        )
        .expect("meta arena"),
    ));
    let pagemap: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    Box::leak(Box::new(
        Allocator::new(
            PosixBackingProvider::new(),
            PosixBackingProvider::new(),
            meta,
            meta,
            pagemap,
            ArenaId::DEFAULT,
            cfg,
        )
        .expect("engine"),
    ))
}

/// Take a `topo_stats::Stats` from the live engine (the composition the C ABI does).
fn snapshot(eng: &Engine) -> Stats {
    let mut s = Stats::default();
    s.record_allocator(&eng.stats());
    s
}

/// Assert the §8.6 reconciliation identities. The algebraic two always hold (even under a
/// torn concurrent read); the byte-covering one (`live + central_free <= active`) and the
/// application identity (`live == allocated - freed`) hold exactly at a quiescent point.
fn assert_reconciles(s: &Stats, quiescent: bool) {
    // Algebraic — true for any `StateBytes` value, so robust under concurrency:
    assert_eq!(
        s.pageheap_free_bytes,
        s.retained_bytes + s.dirty_bytes + s.muzzy_bytes + s.released_bytes,
        "pageheap_free == retained + dirty + muzzy + released (§8.6)"
    );
    assert_eq!(
        s.virtual_bytes,
        s.active_bytes + s.pageheap_free_bytes,
        "virtual == active + pageheap_free (§8.6)"
    );
    if quiescent {
        // The active backing covers every live + central-resident object byte; the slack is
        // slab-packing residual (the documented §8.6 convention).
        assert!(
            s.live_bytes + s.central_free_bytes <= s.active_bytes,
            "live ({}) + central_free ({}) must fit in active backing ({})",
            s.live_bytes,
            s.central_free_bytes,
            s.active_bytes
        );
        assert_eq!(
            s.live_bytes,
            s.allocated_bytes_total - s.freed_bytes_total,
            "live == allocated - freed (§8.6)"
        );
    }
}

#[test]
fn reconciliation_holds_for_a_sequential_live_allocator() {
    let eng = engine();
    // A mix of sizes spanning small / medium / large, with some freed back.
    let sizes = [16usize, 64, 100, 4096, 32 * 1024, 80 * 1024, 7, 4095];
    let mut live: Vec<*mut u8> = Vec::new();
    for &sz in sizes.iter().cycle().take(400) {
        let p = eng.allocate(sz, 16, RequestFlags::NONE);
        if !p.is_null() {
            live.push(p);
        }
        // Free roughly half back as we go, so central lists + backend states are non-trivial.
        if live.len() >= 8 {
            let p = live.swap_remove(live.len() / 2);
            // SAFETY: `p` is a live allocation from this engine we still own.
            unsafe { eng.free(p) };
        }
    }
    let s = snapshot(eng);
    assert_reconciles(&s, true);
    // Every byte class is present and non-negative in the JSON (u64 ⇒ non-negative).
    let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
    for key in [
        "active_bytes",
        "retained_bytes",
        "dirty_bytes",
        "released_bytes",
    ] {
        assert!(v["backend"][key].is_u64(), "backend.{key} present");
    }
    assert!(
        v["quarantine"]["bytes"].is_u64(),
        "quarantine class present (§31.1)"
    );
    assert!(
        v["arenas"]["destroyed"].is_u64(),
        "arena.destroyed present (§31.1)"
    );
    assert!(
        v["fragmentation"]["external_bytes"].is_u64(),
        "fragmentation present (§31.5)"
    );

    // Free the rest; live drops to zero and the application identity is exact.
    for p in live {
        // SAFETY: live pointers we still own.
        unsafe { eng.free(p) };
    }
    let s = snapshot(eng);
    assert_eq!(s.live_bytes, 0, "all freed ⇒ no live bytes");
    assert_reconciles(&s, true);
}

#[test]
fn reconciliation_holds_under_concurrent_load_then_quiesces() {
    let eng = engine();
    const WORKERS: usize = 4;
    // Deterministic completion signal (no timing hack): each worker bumps `finished` when it
    // returns; the reader spins until all `WORKERS` are done.
    let finished = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        // Workers churn allocations while a reader repeatedly snapshots — the algebraic
        // identities must never be violated mid-flight (W17-1b).
        for t in 0..WORKERS {
            let finished = Arc::clone(&finished);
            scope.spawn(move || {
                let mut live: Vec<*mut u8> = Vec::new();
                for i in 0..2000 {
                    let sz = 16 + ((i * 7 + t) % 4096);
                    let p = eng.allocate(sz, 16, RequestFlags::NONE);
                    if !p.is_null() {
                        live.push(p);
                    }
                    if live.len() > 16 {
                        let p = live.swap_remove(0);
                        // SAFETY: a live pointer this thread owns.
                        unsafe { eng.free(p) };
                    }
                }
                for p in live {
                    // SAFETY: a live pointer this thread owns.
                    unsafe { eng.free(p) };
                }
                finished.fetch_add(1, Ordering::Release);
            });
        }
        // Reader thread: snapshot under load; the algebraic identities hold even torn. Runs
        // until every worker has finished (then a few more, to catch the post-quiesce tail).
        let finished_r = Arc::clone(&finished);
        scope.spawn(move || {
            while finished_r.load(Ordering::Acquire) < WORKERS {
                assert_reconciles(&snapshot(eng), false);
            }
            for _ in 0..64 {
                assert_reconciles(&snapshot(eng), false);
            }
        });
    });
    // All threads joined ⇒ quiescent: the full reconciliation is exact, and everything freed.
    let s = snapshot(eng);
    assert_reconciles(&s, true);
    assert_eq!(s.live_bytes, 0, "every thread freed its allocations");
}

/// W18-3 / §8.6 (#23): the reconciliation identities hold with the **quarantine
/// enabled** under concurrent load. A quarantined object is accounted as app-freed
/// (excluded from `live_bytes`) and reported separately as `quarantine.bytes` — never
/// double-counted — so `live == allocated - freed` still holds while objects are held,
/// and disabling (draining) returns `quarantine.bytes` to zero with reconciliation
/// intact. Gated on the `quarantine` feature (the `hardened` profile).
#[cfg(feature = "quarantine")]
#[test]
fn reconciliation_holds_with_quarantine_under_concurrent_load() {
    let eng = engine();
    eng.set_quarantine_enabled(true);
    eng.set_quarantine_policy(topo_core::QuarantinePolicy {
        max_bytes: 1 << 20,
        max_objects: 4096,
        ..topo_core::QuarantinePolicy::DEFAULT
    });
    const WORKERS: usize = 4;
    let finished = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for t in 0..WORKERS {
            let finished = Arc::clone(&finished);
            scope.spawn(move || {
                let mut live: Vec<*mut u8> = Vec::new();
                for i in 0..2000 {
                    let sz = 16 + ((i * 7 + t) % 2048);
                    let p = eng.allocate(sz, 16, RequestFlags::NONE);
                    if !p.is_null() {
                        live.push(p);
                    }
                    if live.len() > 16 {
                        let p = live.swap_remove(0);
                        // SAFETY: a live pointer this thread owns (may be held in quarantine).
                        unsafe { eng.free(p) };
                    }
                }
                for p in live {
                    // SAFETY: a live pointer this thread owns.
                    unsafe { eng.free(p) };
                }
                finished.fetch_add(1, Ordering::Release);
            });
        }
        let finished_r = Arc::clone(&finished);
        scope.spawn(move || {
            while finished_r.load(Ordering::Acquire) < WORKERS {
                // The algebraic identities hold mid-flight even with objects held.
                assert_reconciles(&snapshot(eng), false);
            }
        });
    });
    // Quiescent, quarantine still holding: every app-free happened, so `live_bytes == 0`
    // (held objects are app-freed, not live) and the reconciliation is exact. The held
    // bytes show up only as `quarantine.bytes`, never as live or central-free.
    let s = snapshot(eng);
    assert_reconciles(&s, true);
    assert_eq!(
        s.live_bytes, 0,
        "quarantined objects are app-freed, not live"
    );
    // Drain by disabling: `quarantine.bytes` returns to zero, reconciliation intact.
    eng.set_quarantine_enabled(false);
    let s2 = snapshot(eng);
    assert_eq!(s2.quarantine_bytes, 0, "disable drains the quarantine");
    assert_reconciles(&s2, true);
    assert_eq!(s2.live_bytes, 0, "every allocation ultimately freed");
}

#[test]
fn exact_large_fragmentation_and_peak_track_the_engine() {
    // W17-4: exact medium/large internal fragmentation = usable − requested, live-set accurate.
    // W17-2/3: the peak high-water tracks the maximum live, and RESET_PEAKS clears it.
    let eng = engine();
    let req = HUGE_THRESHOLD + 100; // large, not a page multiple ⇒ genuine waste
    let p = eng.allocate(req, 16, RequestFlags::NONE);
    assert!(!p.is_null());
    let usable = eng.usable_size(p).expect("live large");
    assert!(usable > req, "page-rounding wastes the tail");
    let s = eng.stats();
    assert_eq!(
        s.live_internal_fragmentation_bytes,
        (usable - req) as u64,
        "exact frag = usable − requested"
    );
    assert!(s.peak_live_bytes >= s.live_bytes, "peak ≥ live");
    assert!(
        s.peak_live_bytes >= usable as u64,
        "peak captured the large alloc"
    );

    // A second, larger request grows the exact frag by its own waste (live-set additive).
    let req2 = 2 * HUGE_THRESHOLD + 7;
    let q = eng.allocate(req2, 16, RequestFlags::NONE);
    assert!(!q.is_null());
    let usable2 = eng.usable_size(q).expect("live large 2");
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        (usable - req) as u64 + (usable2 - req2) as u64
    );

    // Freeing returns the waste to the live set exactly (robust to the free path).
    // SAFETY: live pointers we own.
    unsafe {
        eng.free(p);
        eng.free(q);
    }
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        0,
        "no live large ⇒ no internal fragmentation"
    );
    // The peak persists above the now-zero live until reset, then collapses.
    assert!(eng.stats().peak_live_bytes >= usable2 as u64);
    eng.reset_peak_live();
    assert_eq!(
        eng.stats().peak_live_bytes,
        0,
        "RESET_PEAKS clears the high-water"
    );
}

#[test]
fn realloc_in_place_keeps_exact_fragmentation_current() {
    // W17-4: an in-place large grow/shrink updates the recorded request, so the exact frag
    // never goes stale (the over-/under-count the naïve approach would leave).
    let eng = engine();
    let p = eng.allocate(HUGE_THRESHOLD + 100, 16, RequestFlags::NONE);
    assert!(!p.is_null());
    // Grow in place toward a much larger request: frag must reflect the NEW request, not the
    // old tiny one (else it would over-report by ~the growth).
    // SAFETY: `p` is the live base pointer we own and do not concurrently free/realloc.
    let grown = unsafe { eng.resize_in_place(p, 4 * HUGE_THRESHOLD, 16, RequestFlags::NONE) };
    if let Some(new_usable) = grown {
        if new_usable >= 4 * HUGE_THRESHOLD {
            let frag = eng.stats().live_internal_fragmentation_bytes;
            assert!(
                frag <= (new_usable - 4 * HUGE_THRESHOLD) as u64 + PAGE_TAIL_SLACK,
                "frag {frag} reflects the grown request, not the original"
            );
        }
    }
    // SAFETY: live pointer we own.
    unsafe { eng.free(p) };
    assert_eq!(eng.stats().live_internal_fragmentation_bytes, 0);
}

/// A page of slack to keep the in-place-grow assertion robust to page rounding.
const PAGE_TAIL_SLACK: u64 = 64 * 1024;

#[test]
fn exact_fragmentation_tracks_a_same_page_request_change() {
    // W17-4 exactness regression: a resize that changes the request but lands in the **same**
    // page count (`rounded == usable`, so the backend neither shrinks nor grows) must still
    // update the recorded request — else `usable − requested` reads the *stale* pre-resize
    // request and mis-states the exact fragmentation (over-count when the request grew within
    // the page, under-count when it shrank). The allocation is the only live large, so the
    // engine's exact frag equals this descriptor's waste.
    let eng = engine();
    let req0 = HUGE_THRESHOLD + 100; // 513 pages; the request tail-wastes within the last page
    let p = eng.allocate(req0, 16, RequestFlags::NONE);
    assert!(!p.is_null());
    let usable = eng.usable_size(p).expect("live large");
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        (usable - req0) as u64
    );
    // Grow the request within the same page (+200 still rounds to 513 pages): `rounded ==
    // usable`, no backend move — frag must DROP to reflect the larger request, not stay stale.
    let req_up = HUGE_THRESHOLD + 200;
    // SAFETY: `p` is the live base pointer we own and do not concurrently free/realloc.
    let kept = unsafe { eng.resize_in_place(p, req_up, 16, RequestFlags::NONE) };
    assert_eq!(
        kept,
        Some(usable),
        "same page count ⇒ kept whole at the same usable"
    );
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        (usable - req_up) as u64,
        "frag reflects the grown (same-page) request, not the stale original"
    );
    // Shrink the request within the same page (+50): frag must RISE to reflect the smaller
    // request — the under-count direction the stale-request bug would hide.
    let req_down = HUGE_THRESHOLD + 50;
    // SAFETY: live pointer we own.
    let kept2 = unsafe { eng.resize_in_place(p, req_down, 16, RequestFlags::NONE) };
    assert_eq!(kept2, Some(usable), "still the same page count");
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        (usable - req_down) as u64,
        "frag reflects the shrunk (same-page) request"
    );
    // SAFETY: live pointer we own.
    unsafe { eng.free(p) };
    assert_eq!(eng.stats().live_internal_fragmentation_bytes, 0);
}

#[test]
fn destroyed_arena_count_reconciles_through_stats() {
    let eng = engine();
    assert_eq!(snapshot(eng).arenas_destroyed, 0);
    // Create and destroy a few explicit arenas; the cumulative count tracks the destroys.
    let a = eng.arena_create(&ArenaPolicy::explicit()).expect("arena a");
    let b = eng.arena_create(&ArenaPolicy::explicit()).expect("arena b");
    assert_eq!(snapshot(eng).arenas_destroyed, 0, "no destroy yet");
    // SAFETY: the arenas are quiesced (no outstanding allocations) before destroy.
    unsafe {
        eng.arena_destroy(a).expect("destroy a");
        eng.arena_destroy(b).expect("destroy b");
    }
    let s = snapshot(eng);
    assert_eq!(s.arenas_destroyed, 2, "two destroys counted (§31.1)");
    let v: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
    assert_eq!(v["arenas"]["destroyed"], 2);
}

#[test]
fn exact_fragmentation_is_robust_to_the_arena_bulk_free_path() {
    // W17-4: the exact-frag walk reads the live set, so it is correct across the *bulk* free
    // path too — a large allocation in an arena contributes waste, and destroying the arena
    // (which frees it via `free_arena`, not `free`) returns the waste to zero.
    let eng = engine();
    let arena = eng.arena_create(&ArenaPolicy::explicit()).expect("arena");
    let req = HUGE_THRESHOLD + 100;
    let p = eng.allocate_in(arena, req, 16, RequestFlags::NONE);
    assert!(!p.is_null());
    let usable = eng.usable_size(p).expect("live large");
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        (usable - req) as u64
    );
    // Destroy the arena (bulk-frees its large allocation via the free_arena chokepoint).
    // SAFETY: the arena is quiesced — we hold no other live pointer into it.
    unsafe { eng.arena_destroy(arena).expect("destroy") };
    assert_eq!(
        eng.stats().live_internal_fragmentation_bytes,
        0,
        "arena bulk-free returns the large waste to the live set"
    );
}

#[test]
fn by_size_class_detail_sums_to_the_central_free_total() {
    let eng = engine();
    // Allocate then free a batch of one small class so its central list is non-empty.
    let ptrs: Vec<*mut u8> = (0..64)
        .map(|_| eng.allocate(64, 16, RequestFlags::NONE))
        .filter(|p| !p.is_null())
        .collect();
    for p in ptrs {
        // SAFETY: live pointers we own.
        unsafe { eng.free(p) };
    }
    // The per-class central-free decomposition (the §31.2 BY_SIZE_CLASS detail) sums to the
    // total `central_free_bytes` the summary reports — the detail reconciles with the summary.
    let mut per_class_sum = 0u64;
    let mut lines = Vec::new();
    eng.for_each_size_class_central_free(|class, size, free| {
        per_class_sum += free;
        if free > 0 {
            lines.push((class, size, free));
        }
    });
    let s = snapshot(eng);
    assert_eq!(
        per_class_sum, s.central_free_bytes,
        "Σ per-class central-free == central.free_bytes total"
    );
    // At least the 64-byte class shows non-zero central-free after the free batch.
    assert!(
        lines.iter().any(|&(_, size, free)| size == 64 && free > 0),
        "the freed 64-byte objects are central-resident"
    );
}

#[test]
fn explanation_and_full_report_render_for_a_live_snapshot() {
    let eng = engine();
    let live: Vec<*mut u8> = (0..200)
        .map(|i| eng.allocate(64 + i % 256, 16, RequestFlags::NONE))
        .filter(|p| !p.is_null())
        .collect();
    let s = snapshot(eng);
    // §31.6 explanation names the live contributor.
    let e = s.explain();
    assert!(e.starts_with("RSS is attributed to: "));
    assert!(e.contains("live"));
    // The full report with detail flags is valid JSON and includes the detail blocks.
    let arenas = [ArenaLine {
        id: 0,
        label: 0,
        used: s.live_bytes,
        reserved: 0,
    }];
    let detail = StatsDetail {
        arenas: &arenas,
        size_classes: &[],
        numa_nodes: &[],
    };
    let json = s.to_json_with(StatsFlags::BY_ARENA | StatsFlags::BY_CPU, &detail);
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid detailed JSON");
    assert!(v["by_arena"].is_array());
    assert!(
        v["by_cpu"].is_array(),
        "by_cpu is an (empty) array until M2"
    );
    for p in live {
        // SAFETY: live pointers we own.
        unsafe { eng.free(p) };
    }
}

#[test]
fn redaction_is_label_noninterference_end_to_end() {
    // W17-6 / §36.12: the Rust analogue of `stats_observation_noninterference`. A low (label
    // 0) observer's redacted per-arena view is a pure function of the low-labelled arenas, so
    // any high-domain activity is invisible — a low domain cannot infer a high domain's
    // allocation pattern.
    let low = ArenaLine {
        id: 1,
        label: 0,
        used: 64 * 1024,
        reserved: 0,
    };
    // Two different high-domain (label 9) states.
    let high_a = ArenaLine {
        id: 2,
        label: 9,
        used: 4 * 1024 * 1024,
        reserved: 0,
    };
    let high_b = ArenaLine {
        id: 2,
        label: 9,
        used: 512 * 1024 * 1024,
        reserved: 1 << 20,
    };
    let view_a = redact_arenas(&[low, high_a], 0);
    let view_b = redact_arenas(&[low, high_b], 0);
    assert_eq!(
        view_a, view_b,
        "low view is invariant under high-domain activity"
    );
    assert_eq!(view_a, vec![low]);
    // The rendered low-domain JSON never leaks a high arena's bytes.
    let s = Stats::default();
    let json_a = s.to_json_with(
        StatsFlags::BY_ARENA,
        &StatsDetail {
            arenas: &view_a,
            size_classes: &[],
            numa_nodes: &[],
        },
    );
    let json_b = s.to_json_with(
        StatsFlags::BY_ARENA,
        &StatsDetail {
            arenas: &view_b,
            size_classes: &[],
            numa_nodes: &[],
        },
    );
    assert_eq!(json_a, json_b, "the redacted low-domain JSON is identical");
    assert!(!json_a.contains("4194304") && !json_a.contains("536870912"));
    // A high observer (label 9) dominates both and sees everything.
    assert_eq!(redact_arenas(&[low, high_a], 9).len(), 2);
}

#[test]
fn c_abi_stats_snapshot_reconciles_and_advances_epoch() {
    // The live C ABI path (process-global engine) composes a snapshot that reconciles and
    // carries a monotonic epoch (§8.6, W17-1b/2).
    let p = topo_abi::topomalloc_malloc(4096);
    let mut st = topo_abi::topomalloc_stats_t::default();
    // SAFETY: `&mut st` is a valid writable struct.
    let rc = unsafe { topo_abi::topomalloc_stats_snapshot(&mut st, 0) };
    assert_eq!(rc, 0);
    assert!(st.epoch >= 1);
    assert_eq!(
        st.pageheap_free_bytes,
        st.retained_bytes + st.dirty_bytes + st.muzzy_bytes + st.released_bytes
    );
    assert_eq!(st.virtual_bytes, st.active_bytes + st.pageheap_free_bytes);
    let mut st2 = topo_abi::topomalloc_stats_t::default();
    // SAFETY: valid writable struct.
    unsafe { topo_abi::topomalloc_stats_snapshot(&mut st2, 0) };
    assert!(st2.epoch > st.epoch, "epoch advances per snapshot");
    if !p.is_null() {
        // SAFETY: `p` is a live allocation we own.
        unsafe { topo_abi::topomalloc_free(p) };
    }
}

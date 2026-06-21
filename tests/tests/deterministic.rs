// SPDX-License-Identifier: MIT
//! Deterministic test mode — behavioural integration tests (§30.4, plan 08 W19-3).
//!
//! The `force_slow_path` / `force_purge` toggles are process-global flags read on
//! allocator hot paths (the central empty-span cache reuse, the extent free), so a
//! test that flips them would race any concurrent allocator test in the *same*
//! process. This is its own integration binary (a separate process from the
//! topo-core unit tests), and its tests serialize on [`SERIAL`] and reset every
//! flag they touch, so flipping a global here is contained and race-free.
//!
//! The pure deterministic machinery (seed derivation, trace ids, the seeded
//! guard/quarantine RNGs) is unit-tested in `topo-core` (`deterministic`,
//! `harden`); here we prove the two *behavioural* hot-path hooks actually fire.

use std::sync::Mutex;

use topo_backend_posix::PosixBackingProvider;
use topo_core::deterministic;
use topo_core::{
    ArenaId, BumpArena, CentralCache, ExtentManager, Fit, Label, NodeId, PageMap, RemoveResult,
    RetainPolicy, SizeClassId, SpanDescriptor, SpanId, ANY_PLACE_CLASS,
};

/// Serializes the tests in this binary: they flip process-global flags read on
/// allocator hot paths, so they must not run concurrently with one another.
/// Poison-tolerant (a panicking test must not wedge the rest).
static SERIAL: Mutex<()> = Mutex::new(());

const PAGE: usize = 16 * 1024;

fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: a leaked, owned allocation of `len` bytes — valid for the process.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

/// Drain every central-free object of `sc` out of the cache into a `Vec` of
/// indices, so the span can then be returned in one batch (making it empty).
fn drain_all(cache: &CentralCache, sc: SizeClassId) -> Vec<u16> {
    let mut all = Vec::new();
    while let RemoveResult::Ok(batch) = cache.remove_batch(
        NodeId::DEFAULT,
        ArenaId::DEFAULT,
        Label::PUBLIC,
        sc,
        ANY_PLACE_CLASS,
        64,
    ) {
        for i in 0..batch.len() {
            all.push(batch.index(i));
        }
    }
    all
}

#[test]
fn force_slow_path_declines_empty_span_cache_reuse() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    deterministic::set_force_slow_path(false); // clean baseline

    let m = meta(4 * 1024 * 1024);
    let pm = PageMap::new();
    let cache = CentralCache::new();
    let sc = SizeClassId::new(3); // 64-byte class
    let row = topo_core::size_class::row(sc);
    let span = SpanDescriptor::new(
        SpanId(1),
        ArenaId::DEFAULT,
        sc,
        0x4000_0000,
        row.slab_pages,
        row.objects_per_slab,
        0,
        m,
    )
    .expect("span");

    // Build an empty-cached span: activate, drain every object, return them all.
    cache.activate_span(&span, &pm, m).expect("activate");
    let all = drain_all(&cache, sc);
    cache.insert_batch(&span, &all, all.len());
    let bin = cache.bin(sc).unwrap();
    assert_eq!(bin.empty_count(), 1, "the drained span is cached as empty");
    assert_eq!(bin.partial_count(), 0);

    // Forced slow path: the empty-cache reuse is declined → NeedSpan, and the
    // empty span stays cached (the backend would supply a fresh span).
    deterministic::set_force_slow_path(true);
    assert!(
        matches!(
            cache.remove_batch(
                NodeId::DEFAULT,
                ArenaId::DEFAULT,
                Label::PUBLIC,
                sc,
                ANY_PLACE_CLASS,
                4
            ),
            RemoveResult::NeedSpan
        ),
        "force_slow_path must decline the empty-span cache reuse"
    );
    assert_eq!(
        cache.bin(sc).unwrap().empty_count(),
        1,
        "the empty span is untouched"
    );
    assert!(
        cache.check_invariants(),
        "the bin stays well-formed (B.1/B.3)"
    );

    // Fast path restored: the same request now reuses the cached empty span.
    deterministic::set_force_slow_path(false);
    assert!(
        matches!(
            cache.remove_batch(
                NodeId::DEFAULT,
                ArenaId::DEFAULT,
                Label::PUBLIC,
                sc,
                ANY_PLACE_CLASS,
                4
            ),
            RemoveResult::Ok(_)
        ),
        "with the fast path, the empty-cached span is reused"
    );
    assert_eq!(
        cache.bin(sc).unwrap().empty_count(),
        0,
        "the empty span was reused"
    );
    assert!(cache.check_invariants());

    deterministic::set_force_slow_path(false); // leave the global clean
}

#[test]
fn force_purge_releases_freed_backing_eagerly() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    deterministic::set_force_purge(false); // clean baseline

    let mut mgr = ExtentManager::new(
        PosixBackingProvider::new(),
        meta(1 << 20),
        ArenaId::DEFAULT,
        64 * PAGE,
        PAGE,
        4096,
    )
    .expect("manager");
    // Pin the retain policy so the contrast is force_purge, not the build profile.
    mgr.set_retain_policy(RetainPolicy::Retain);

    // Baseline: a freed extent is *retained* (Dirty), not released.
    let r = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc");
    mgr.free(r).expect("free");
    let sb = mgr.state_bytes();
    assert!(
        sb.dirty > 0,
        "retain policy keeps freed backing Dirty: {sb:?}"
    );
    assert_eq!(sb.released, 0, "nothing released under the retain policy");
    assert!(mgr.check_invariants());

    // Forced purge: a freed extent is released to the OS eagerly (decommitted).
    deterministic::set_force_purge(true);
    let r2 = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc2");
    mgr.free(r2).expect("free2");
    let sb2 = mgr.state_bytes();
    assert!(
        sb2.released > 0,
        "force_purge must release freed backing eagerly: {sb2:?}"
    );
    assert!(mgr.check_invariants());

    deterministic::set_force_purge(false); // leave the global clean
}

#[test]
fn deterministic_seed_and_trace_ids_are_reproducible() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Same seed ⇒ the same derived per-domain streams (the basis of replay).
    deterministic::set_seed(0x0BADC0DE_0BADC0DE);
    let g = deterministic::domain_seed(deterministic::salt::GUARD);
    let q = deterministic::domain_seed(deterministic::salt::QUARANTINE);
    deterministic::set_seed(0x0BADC0DE_0BADC0DE);
    assert_eq!(deterministic::domain_seed(deterministic::salt::GUARD), g);
    assert_eq!(
        deterministic::domain_seed(deterministic::salt::QUARANTINE),
        q
    );
    assert_ne!(g, q, "distinct domains get decorrelated streams");

    // Trace ids are monotonic and re-base on reset (single-thread reproducibility).
    deterministic::reset_trace_ids();
    let a = deterministic::next_trace_id();
    let b = deterministic::next_trace_id();
    assert!(b > a);
    deterministic::reset_trace_ids();
    assert_eq!(deterministic::next_trace_id(), 1);

    deterministic::set_seed(deterministic::DEFAULT_SEED); // leave the global clean
}

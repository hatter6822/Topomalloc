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
    refill, ArenaId, BumpArena, CentralCache, CoreId, CpuCache, ExtentManager, Fit, Label, NodeId,
    PageMap, RemoveResult, RetainPolicy, SizeClassId, SpanDescriptor, SpanId, TransferCache,
    ANY_PLACE_CLASS,
};

/// Serializes the tests in this binary: they flip process-global flags read on
/// allocator hot paths, so they must not run concurrently with one another.
/// Poison-tolerant (a panicking test must not wedge the rest).
static SERIAL: Mutex<()> = Mutex::new(());

const PAGE: usize = 16 * 1024;

/// A SplitMix64 step — a local copy of the `deterministic` module's mixer (which is
/// private), used to advance a reproducible per-run RNG stream in the replay test
/// below, standing in for the seed-driven decisions the guard/heap samplers make.
fn splitmix_step(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

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

/// §30.4 (W19-3) × W6: `force_slow_path` must bypass the **front end** too, so a
/// deterministic replay exercises the central path on every allocation and free rather
/// than whichever objects a per-CPU slot happened to be holding — the front end's
/// slot selection depends on the running CPU, which is exactly the nondeterminism the
/// mode exists to remove.
///
/// Lives in this (serialized, separate-process) binary because the flag is a
/// process-global read on the allocation hot path.
#[test]
fn force_slow_path_bypasses_the_front_end() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let meta_provider = PosixBackingProvider::new();
    let meta = topo_core::MetaArena::reserve(meta_provider, ArenaId::DEFAULT, 32 * 1024 * 1024)
        .expect("metadata arena");
    let pm = PageMap::new();
    let a = topo_core::Allocator::new(
        PosixBackingProvider::new(),
        PosixBackingProvider::new(),
        &meta,
        &meta,
        &pm,
        ArenaId::DEFAULT,
        topo_core::AllocatorConfig::small(),
    )
    .expect("allocator");

    // Baseline: with the flag off, a freed small object is held in the front end.
    let p = a.malloc(64);
    assert!(!p.is_null());
    // SAFETY: `p` was just returned by this allocator and is still owned here.
    assert_eq!(unsafe { a.free(p) }, topo_core::FreeOutcome::Freed);
    assert_eq!(
        a.stats().per_cpu_bytes,
        64,
        "with the flag off the free is absorbed by the front end"
    );
    a.flush_front_end_all();

    // Forced: the same sequence goes straight to the central free list.
    deterministic::set_force_slow_path(true);
    let before = a.stats();
    let q = a.malloc(64);
    assert!(!q.is_null());
    // SAFETY: as above.
    assert_eq!(unsafe { a.free(q) }, topo_core::FreeOutcome::Freed);
    let after = a.stats();
    deterministic::set_force_slow_path(false); // leave the global clean

    assert_eq!(
        after.per_cpu_bytes, 0,
        "the forced free bypassed the front end"
    );
    assert_eq!(after.transfer_bytes, 0);
    assert_eq!(
        after.central_free_bytes, before.central_free_bytes,
        "…and the object went back to central, where it came from"
    );
    assert!(a.check_invariants());
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
fn force_slow_path_bypasses_the_transfer_cache_in_refill() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    deterministic::set_force_slow_path(false); // clean baseline

    let m = meta(4 * 1024 * 1024);
    let pm = PageMap::new();
    let cpu = CpuCache::new();
    cpu.set_active_cpus(1);
    let transfer = TransferCache::new();
    let central = CentralCache::new();
    let sc = SizeClassId::new(3);
    let core = CoreId::DEFAULT;
    cpu.init_slot(core, sc, m, topo_core::size_class::batch(sc) as u32);

    // Activate a span in central so a forced refill has somewhere to pull from.
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
    .unwrap();
    central.activate_span(&span, &pm, m).unwrap();

    // Populate the transfer cache with dummy objects (the fast-path source).
    transfer.try_push_batch(ArenaId::DEFAULT, sc, &[111, 222, 333], m);
    let before = transfer.bin(sc).unwrap().len();
    assert!(before > 0);

    // Forced slow path: refill pulls from *central*, leaving the transfer cache
    // untouched (the fast-path source is bypassed).
    deterministic::set_force_slow_path(true);
    let mut no_retire = |_: &topo_core::SpanDescriptor| {};
    let r = refill(
        core,
        NodeId::DEFAULT,
        ArenaId::DEFAULT,
        Label::PUBLIC,
        sc,
        ANY_PLACE_CLASS,
        &cpu,
        &transfer,
        &central,
        &pm,
        m,
        &mut no_retire,
    );
    assert!(r.filled > 0, "the forced refill pulled from central");
    assert!(!r.need_span);
    assert_eq!(
        transfer.bin(sc).unwrap().len(),
        before,
        "force_slow_path leaves the transfer cache untouched (bypassed)"
    );

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
fn trace_emission_is_reproducible_under_deterministic_ids() {
    // §30.4 / §33.7 (W19-3): with the deterministic monotonic trace-id source, the
    // trace grammar an operation sequence emits is byte-identical run-to-run — the
    // reproducibility the differential runner (W21-2) rides on. (W19-3 supplies the
    // id source + the emitters; wiring live per-op emission into the allocator is
    // W21-2a's job, which now has everything it needs.)
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let emit_run = || -> String {
        deterministic::reset_trace_ids();
        let mut buf = String::new();
        // A fixed op sequence, each line stamped with the next deterministic id.
        for (size, align) in [(24usize, 16usize), (64, 16), (100_000, 16)] {
            let id = deterministic::next_trace_id();
            // A deterministic (id-derived) pointer stands in for the real address,
            // which is not itself reproducible (ASLR) — the model replay compares
            // abstract outcomes, and the *trace* is what must reproduce here.
            topo_core::trace::emit_alloc(
                &mut buf,
                id,
                size,
                align,
                0,
                0,
                0x1_0000 * id as usize,
                size,
                None,
                None,
            )
            .unwrap();
        }
        buf
    };

    let first = emit_run();
    assert_eq!(
        first,
        emit_run(),
        "the §33.7 trace replays byte-identically"
    );
    // The ids are the reproducible 1, 2, 3 sequence (re-based by reset).
    assert!(first.starts_with("ALLOC 1 24 16 "));
    assert!(first.contains("\nALLOC 2 64 16 "));
    assert!(first.contains("\nALLOC 3 100000 16 "));

    deterministic::reset_trace_ids(); // leave the global clean
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

#[test]
fn a_seeded_op_trace_captures_and_replays_identically() {
    // §30.4 / W19-3 acceptance ("a trace replays identically; the differential
    // runner rides on it"): an end-to-end self-consistency replay over the *whole*
    // deterministic pipeline. A fixed op script is captured as a §33.7 trace whose
    // every field is a reproducible projection — the deterministic trace id, the
    // **real classifier outcome** (size class / extent bytes, from `classify`), and
    // a seed-derived placement draw (the same `domain_seed` stream the guard/heap
    // samplers draw from). With the same seed the capture replays byte-for-byte;
    // with a *different* seed it changes — so the seed genuinely drives the
    // randomized part (not a constant masquerading as reproducible). The Lean
    // executable-model differential is W21-2b; this proves the W19-3 prerequisite
    // with only W19-3 machinery.
    use topo_core::classify::{classify, RequestKind};

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // A fixed op script: (size, align). Spans the small, medium, and large paths.
    const SCRIPT: [(usize, usize); 6] = [
        (1, 16),
        (24, 16),
        (64, 64),
        (4096, 16),
        (100_000, 16),
        (2_500_000, 16),
    ];

    // The §33.7 trace a run with `seed` emits — the reproducible projection of each
    // op's real outcome, one line per op.
    let capture = |seed: u64| -> String {
        deterministic::set_deterministic(true);
        deterministic::set_seed(seed);
        deterministic::reset_trace_ids();
        // The per-run RNG stream a randomized placement/sampling decision draws from,
        // derived from the just-reset global seed — reproducible per seed.
        let mut rng = deterministic::domain_seed(deterministic::salt::SAMPLER);
        let mut buf = String::new();
        for (size, align) in SCRIPT {
            let id = deterministic::next_trace_id();
            let req = classify(size, align, 0).expect("valid request");
            let (usable, sc) = match req.kind {
                RequestKind::Small { sc, usable } => (usable, Some(sc.index() as u64)),
                RequestKind::Medium { bytes } | RequestKind::Large { bytes } => (bytes, None),
            };
            // Advance the seeded stream — the randomized decision — so its value
            // rides in the captured trace and the seed participates in the replay.
            rng = splitmix_step(rng);
            // A real address is not reproducible (ASLR); the model replay compares
            // abstract outcomes (§33.7), so the captured pointer mixes the
            // deterministic id with the seeded draw — reproducible per seed.
            let pseudo_addr = (0x1_0000usize.wrapping_mul(id as usize)) ^ (rng as usize & 0xfff);
            topo_core::trace::emit_alloc(
                &mut buf,
                id,
                size,
                align,
                0, // flags=0 ⇒ the default arena for every op
                0,
                pseudo_addr,
                usable,
                sc,
                None,
            )
            .expect("emit");
        }
        buf
    };

    const SEED: u64 = 0x7E57_5EED_1234_ABCD;
    let first = capture(SEED);
    assert_eq!(
        first,
        capture(SEED),
        "the seeded op-trace replays byte-identically under the same seed"
    );
    assert_ne!(
        first,
        capture(SEED ^ u64::MAX),
        "a different seed must change the trace (the seed genuinely drives the replay)"
    );
    // The real classifier outcomes are captured (trace line:
    // `ALLOC id size align arena flags -> ptr usable sc span`, `sc`/`span` "-" when
    // absent): the 1-byte request is small — class 0, usable 16 — and the 2.5 MB
    // request takes the large path (no size class).
    assert!(
        first.contains("ALLOC 1 1 16 0 0 -> "),
        "op 1 present with id 1, arena 0"
    );
    assert!(
        first.lines().next().unwrap().ends_with(" 16 0 -"),
        "the 1-byte request records class 0 (usable 16) in the trace's sc field"
    );
    assert!(
        first.lines().last().unwrap().ends_with(" - -"),
        "the 2.5 MB request takes the large path — no size class recorded"
    );

    // Leave every global clean for the rest of the binary.
    deterministic::set_seed(deterministic::DEFAULT_SEED);
    deterministic::reset_trace_ids();
    deterministic::set_deterministic(false);
}

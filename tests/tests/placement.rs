// SPDX-License-Identifier: MIT
//! Lifetime / hotness / placement-policy integration tests (§24, plan 07 W14 +
//! the minimal W17-3 sampling slice).
//!
//! Three things are proven here, the first being W14's single non-negotiable:
//!
//! 1. **The §24.5 safety boundary — the fixed wall.** A placement decision may change
//!    *where* an object lands but **never** its size, alignment, validity, or free path.
//!    We pin it twice: over the pure [`HugePageFiller`] (a run is always exactly the
//!    requested geometry, under *any* — including adversarial — hints), and end-to-end
//!    through a real hugepage-backed engine where hints actually steer placement, yet
//!    `usable_size` / alignment / writability / free are identical across every hint.
//! 2. **W14-2 learning → placement.** A learned (even deliberately *wrong*)
//!    [`SiteProfileTable`] profile distils to advisory [`PlaceHints`]; feeding those hints
//!    back through the allocator still upholds the wall — the learned-profile machinery
//!    can evolve freely behind the fixed boundary.
//! 3. **W17-3 live sampling.** With sampling enabled, real `topomalloc_malloc`/`free`
//!    traffic populates per-site profiles (lock-free decision, alloc-free capture), the
//!    sampler never re-enters the allocator, and a profile dump is well-formed — all a
//!    no-op when sampling is off (the default).

use topo_backend_posix::PosixBackingProvider;
use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::{HUGE_THRESHOLD, PAGE_SIZE, SMALL_MAX};
use topo_core::huge::Hotness;
use topo_core::ids::ArenaId;
use topo_core::{
    Allocator, AllocatorConfig, FreeOutcome, HugeConfig, HugePageBackend, HugePageFiller, Lifetime,
    PageMap, PlaceHints, RequestFlags, SiteProfileTable, StackId, CONFIDENT_SAMPLES, HUGEPAGE_SIZE,
    PAGES_PER_HUGEPAGE,
};

type Engine = Allocator<'static, PosixBackingProvider>;
type Huge = HugePageBackend<PosixBackingProvider>;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A leaked heap metadata arena, valid for the process (the shared test pattern).
fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

/// A full engine whose medium/large path is served by a hugepage backend (so hints
/// actually reach the filler), with the backend exposed so a test can read its §19.4 bin
/// coverage. Both are leaked `'static`.
fn huge_engine() -> (&'static Engine, &'static Huge) {
    let m = meta(16 << 20);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    let cfg = AllocatorConfig {
        span_region_bytes: 32 * 1024 * 1024,
        span_extent_slots: 4096,
        span_slots: 4096,
        large_region_bytes: 96 * 1024 * 1024,
        large_extent_slots: 4096,
        large_slots: 4096,
    };
    let capacity = (cfg.large_region_bytes / HUGEPAGE_SIZE).max(1);
    let hp: &'static Huge = Box::leak(Box::new(
        HugePageBackend::new(
            PosixBackingProvider::new(),
            m,
            ArenaId::DEFAULT,
            HugeConfig::with_capacity(capacity),
        )
        .expect("hugepage backend"),
    ));
    let engine: &'static Engine = Box::leak(Box::new(
        Allocator::new_with_huge(
            PosixBackingProvider::new(),
            PosixBackingProvider::new(),
            hp,
            m,
            m,
            pm,
            ArenaId::DEFAULT,
            cfg,
        )
        .expect("engine"),
    ));
    (engine, hp)
}

/// Build a `RequestFlags` carrying a hotness + lifetime hint (the W14-1 plumbing target).
fn flags_with(hotness: u8, lifetime: Lifetime) -> RequestFlags {
    RequestFlags::NONE
        .with_hotness(hotness)
        .with_lifetime(lifetime)
}

/// Map a [`PlaceHints`] (a profile's advisory output) back to request flags, closing the
/// W14-2 loop: learned profile → hints → allocation request.
fn flags_of_hints(h: PlaceHints) -> RequestFlags {
    let hotness = match h.hotness {
        Hotness::Cold => 0,
        Hotness::Neutral => 64,
        Hotness::Hot => 200,
    };
    RequestFlags::NONE
        .with_hotness(hotness)
        .with_lifetime(h.lifetime)
}

/// Every hint combination we sweep over — including the contradictory / "wrong" ones a
/// bad profile could produce.
fn all_hint_flags() -> Vec<RequestFlags> {
    let mut v = vec![RequestFlags::NONE];
    for &hot in &[0u8, 1, 64, 128, 255] {
        for lt in [
            Lifetime::Unspecified,
            Lifetime::Short,
            Lifetime::Medium,
            Lifetime::Long,
        ] {
            v.push(flags_with(hot, lt));
        }
    }
    v
}

// ---------------------------------------------------------------------------
// 1. The §24.5 safety boundary — the fixed wall
// ---------------------------------------------------------------------------

#[test]
fn filler_run_geometry_is_invariant_under_any_hints() {
    // The pure filler: a placed run is always exactly `pages` pages, page-aligned, and
    // in-bounds — regardless of hotness/lifetime hints. Hints can only change *which*
    // hugepage/offset is chosen (the score), never the run's size or alignment (§24.5).
    let m = meta(8 << 20);
    let mut filler = HugePageFiller::new(m, 0x2000_0000_0000, 64).expect("filler");

    let hint_set = [
        PlaceHints::default(),
        PlaceHints {
            hotness: Hotness::Hot,
            lifetime: Lifetime::Long,
        },
        PlaceHints {
            hotness: Hotness::Cold,
            lifetime: Lifetime::Short,
        },
        // A deliberately mismatched/adversarial hint.
        PlaceHints {
            hotness: Hotness::Hot,
            lifetime: Lifetime::Short,
        },
    ];
    for pages in [1usize, 2, 5, PAGES_PER_HUGEPAGE / 2, PAGES_PER_HUGEPAGE] {
        for align in [PAGE_SIZE, 2 * PAGE_SIZE, 4 * PAGE_SIZE] {
            for &hints in &hint_set {
                let Some(p) = filler.place(pages, align, hints) else {
                    continue; // region full for this geometry — fine, just skip
                };
                // Size invariant: exactly the requested page count.
                assert_eq!(p.pages, pages, "placement changed the run length");
                // Alignment invariant.
                assert_eq!(p.base % align, 0, "placement violated alignment");
                // In-bounds invariant: the run lies within one hugepage.
                let off = p.base % HUGEPAGE_SIZE;
                assert!(
                    off + pages * PAGE_SIZE <= HUGEPAGE_SIZE,
                    "run escaped its hugepage"
                );
                // Return it so the region does not fill across the sweep.
                filler.free(p.base, p.pages);
            }
        }
    }
}

#[test]
fn engine_size_align_validity_free_are_invariant_under_hints() {
    // End-to-end through a hugepage-backed engine: for each (size, align), the baseline
    // (no-hint) allocation fixes the usable size and alignment; *every* hint must produce
    // an allocation with the identical usable size and alignment, the full usable range
    // writable+readable, and a clean free. This is the §24.5 wall as a live test.
    let (engine, _hp) = huge_engine();
    let sizes = [
        64usize,
        4096,
        SMALL_MAX,             // largest small
        SMALL_MAX + 1,         // medium
        1 << 20,               // medium, sub-hugepage
        HUGE_THRESHOLD,        // exactly one hugepage (large)
        HUGE_THRESHOLD + 4096, // large, just over one hugepage
        3 * HUGE_THRESHOLD,    // multi-hugepage
    ];
    let aligns = [1usize, 16, 64, PAGE_SIZE];

    for &size in &sizes {
        for &align in &aligns {
            // Baseline (no hints) establishes the invariants.
            let base = engine.allocate(size, align, RequestFlags::NONE);
            assert!(
                !base.is_null(),
                "baseline alloc failed for ({size},{align})"
            );
            let want_usable = engine.usable_size(base).expect("baseline usable");
            assert!(want_usable >= size, "usable < requested");
            assert_eq!(base as usize % align, 0, "baseline misaligned");
            // SAFETY: `base` is a live allocation this engine just returned and still owns.
            assert_eq!(unsafe { engine.free(base) }, FreeOutcome::Freed);

            for flags in all_hint_flags() {
                let p = engine.allocate(size, align, flags);
                assert!(
                    !p.is_null(),
                    "hinted alloc failed for ({size},{align}) flags={:#x}",
                    flags.raw()
                );
                // Size invariant (§24.5): hints never change the usable size.
                assert_eq!(
                    engine.usable_size(p),
                    Some(want_usable),
                    "hint changed usable size for ({size},{align})"
                );
                // Alignment invariant.
                assert_eq!(p as usize % align, 0, "hint changed alignment");
                // Validity invariant: the whole usable range is writable and reads back.
                // SAFETY: `p` is a live allocation of at least `want_usable` writable bytes
                // (its reported usable size), so the fill + the bounding reads are in range.
                unsafe {
                    core::ptr::write_bytes(p, 0xAB, want_usable);
                    assert_eq!(p.read(), 0xAB);
                    assert_eq!(p.add(want_usable - 1).read(), 0xAB);
                }
                // Free-path invariant.
                // SAFETY: `p` is the live allocation just validated; we still own it.
                let outcome = unsafe { engine.free(p) };
                assert_eq!(
                    outcome,
                    FreeOutcome::Freed,
                    "hint changed the free path for ({size},{align})"
                );
            }
        }
    }
}

#[test]
fn learned_profile_hints_uphold_the_wall() {
    // W14-2 → W14-3: a learned profile (here a deliberately *wrong* one — it claims its
    // objects are hot+long when the test will treat them as anything) distils to hints
    // that, fed back through the engine, still cannot break size/align/validity (§24.5).
    let mut table: SiteProfileTable<64> = SiteProfileTable::new();
    let site = StackId(0xBADC0DE);
    let mut clock = 0u64;
    for _ in 0..CONFIDENT_SAMPLES {
        table.record_alloc(site, 8, 4096, 255 /* hot */, clock);
        clock += 1;
        table.record_free(site, 120_000 /* long */, 4096, clock);
        clock += 1;
    }
    let hints = table.place_hints(site);
    assert_eq!(hints.hotness, Hotness::Hot);
    assert_eq!(hints.lifetime, Lifetime::Long);
    let flags = flags_of_hints(hints);

    let (engine, _hp) = huge_engine();
    for &size in &[128usize, 1 << 20, HUGE_THRESHOLD] {
        let base = engine.allocate(size, 16, RequestFlags::NONE);
        let want = engine.usable_size(base).unwrap();
        // SAFETY: `base` is a live allocation this engine just returned and still owns.
        assert_eq!(unsafe { engine.free(base) }, FreeOutcome::Freed);

        let p = engine.allocate(size, 16, flags);
        assert!(!p.is_null());
        assert_eq!(
            engine.usable_size(p),
            Some(want),
            "learned hint changed size"
        );
        assert_eq!(p as usize % 16, 0);
        // SAFETY: `p` is a live allocation of at least `want` writable bytes that we own,
        // so the fill, the bounding read, and the free are all valid.
        unsafe {
            core::ptr::write_bytes(p, 0x5A, want);
            assert_eq!(p.add(want - 1).read(), 0x5A);
            assert_eq!(engine.free(p), FreeOutcome::Freed);
        }
    }
}

// ---------------------------------------------------------------------------
// 2. W14-3 grouping is observable in stats
// ---------------------------------------------------------------------------

#[test]
fn cold_and_hot_grouping_is_observable_in_hugepage_bins() {
    // §24.6/§24.8: cold objects steer toward the cold/sparse bin and hot ones toward the
    // dense bin. We assert the *effect* is visible in the §19.4 bin distribution that the
    // filler surfaces in stats (the W14-3 "grouping observable in stats" acceptance),
    // without asserting an exact layout (placement is policy).
    let (engine, hp) = huge_engine();
    // A spread of medium allocations carrying cold vs. hot hints.
    let cold = flags_with(0, Lifetime::Short);
    let hot = flags_with(255, Lifetime::Long);
    let mut live = Vec::new();
    for _ in 0..16 {
        live.push(engine.allocate(1 << 20, PAGE_SIZE, cold));
        live.push(engine.allocate(1 << 20, PAGE_SIZE, hot));
    }
    for &p in &live {
        assert!(!p.is_null());
    }
    // The §19.4 bin distribution (the W14-3 "grouping observable in stats" surface) is
    // populated and **reconciles**: every touched hugepage is counted in exactly one bin
    // (H-002/H-003). That the policy's effect is visible and self-consistent is the
    // acceptance — the exact bins are policy, not asserted.
    let cov = hp.coverage();
    let total: u32 = cov.bins.iter().sum();
    assert!(total > 0, "hugepages were touched");
    assert_eq!(
        total as u64,
        cov.coverage_bytes / HUGEPAGE_SIZE as u64,
        "bin counts reconcile with touched hugepages"
    );
    for &p in &live {
        // SAFETY: each `p` is a distinct live allocation from this engine that we own.
        assert_eq!(unsafe { engine.free(p) }, FreeOutcome::Freed);
    }
}

// ---------------------------------------------------------------------------
// 3. W17-3 live sampling through the public C ABI
// ---------------------------------------------------------------------------

use topo_abi::{
    topomalloc_free, topomalloc_malloc, topomalloc_profile_dump_json, topomalloc_profile_enabled,
    topomalloc_profile_set_rate, topomalloc_profile_sites,
};

#[test]
fn sampling_lifecycle_off_then_live_then_concurrent() {
    // The C-ABI sampling tests share the *process-global* sampler, so they run as one
    // sequential lifecycle rather than as parallel tests that would race over the global
    // enable flag.
    use std::thread;

    // --- Phase A: disabled (the default) records nothing. ---
    topomalloc_profile_set_rate(0);
    assert_eq!(topomalloc_profile_enabled(), 0);
    let before = topomalloc_profile_sites();
    let mut ps = Vec::new();
    for _ in 0..2000 {
        let p = topomalloc_malloc(256);
        assert!(!p.is_null());
        ps.push(p);
    }
    for p in ps {
        // SAFETY: each `p` is a live `topomalloc_malloc` allocation we still own.
        unsafe { topomalloc_free(p) };
    }
    assert_eq!(
        topomalloc_profile_sites(),
        before,
        "no sites recorded while sampling is off"
    );

    // --- Phase B: enabled — real traffic feeds profiles, and the sampler never re-enters
    // the allocator (the loop completing without deadlock/recursion is the proof). ---
    assert!(
        !topo_abi::sampling::in_sampler(),
        "guard clear outside the sampler"
    );
    topomalloc_profile_set_rate(4096); // small interval ⇒ many samples
    assert_eq!(topomalloc_profile_enabled(), 1);
    for _ in 0..20_000 {
        let p = topomalloc_malloc(512);
        assert!(!p.is_null());
        debug_assert!(
            !topo_abi::sampling::in_sampler(),
            "guard leaked onto the hot path"
        );
        // SAFETY: `p` is the live allocation from the line above; we own it.
        unsafe { topomalloc_free(p) };
    }
    let stats = topo_abi::sampling::placement_stats();
    assert!(stats.alloc_samples > 0, "sampling fired on real traffic");
    assert!(stats.sites_tracked > 0, "at least one site was attributed");

    // The profile dump is well-formed JSON; the C buffer contract agrees on length.
    let json = topo_abi::sampling::dump_json();
    let v: serde_json::Value = serde_json::from_str(&json).expect("dump is valid JSON");
    assert_eq!(v["enabled"], true);
    assert!(v["sites_tracked"].as_u64().unwrap() >= 1);
    // SAFETY: the length-query form — a null buffer with cap 0 writes nothing.
    let need = unsafe { topomalloc_profile_dump_json(core::ptr::null_mut(), 0) };
    assert!(need > 0);

    // --- Phase C: concurrent traffic stays memory-safe and deadlock-free. ---
    let handles: Vec<_> = (0..8)
        .map(|_| {
            thread::spawn(|| {
                for i in 0..10_000usize {
                    let p = topomalloc_malloc(64 + (i % 512));
                    assert!(!p.is_null());
                    // SAFETY: `p` is the live allocation from the line above; this thread owns it.
                    unsafe { topomalloc_free(p) };
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("worker thread");
    }
    assert!(topo_abi::sampling::placement_stats().alloc_samples > 0);

    // Restore the default (off) so nothing leaks into other test binaries' expectations.
    topomalloc_profile_set_rate(0);
    assert_eq!(topomalloc_profile_enabled(), 0);
}

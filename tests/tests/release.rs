// SPDX-License-Identifier: MIT
//! Release-controller live-wiring integration tests (§20–§21, plan 04 W12). The
//! in-crate `topo_core::release` tests cover the pure controller (inputs → pressure
//! mode → ladder → demand reserve) in isolation; this file proves the **live W11→W12
//! handoff**: [`HugePageBackend::release_tick`] driving the controller against a real
//! hugepage backend over a real POSIX backing, so the §21.4 demand reserve actually
//! governs whether empty hugepages are returned to the OS.
//!
//! What is exercised:
//!
//! * **idle ⇒ release:** with no recent allocation the reserve is zero, so a pressured
//!   tick returns the backend's empty-backed hugepages to the OS (RSS drops);
//! * **churn ⇒ hold (§21.1 R2):** with a high allocation rate the reserve covers the
//!   empty supply, so the *same* pressure releases strictly less — no release-then-
//!   refault oscillation, proven on the live backend, not just the model;
//! * **emergency:** an allocation-failure tick disables the HugeCache reserve and
//!   drains every empty hugepage regardless of the rate cap.

use topo_backend_posix::PosixBackingProvider;
use topo_core::ids::ArenaId;
use topo_core::{
    DecayConfig, HugeConfig, HugePageBackend, PlaceHints, PressureMode, ReleaseController,
    ReleaseInputs, HUGEPAGE_SIZE,
};

const PAGE: usize = topo_core::generated::tables::PAGE_SIZE;

/// A leaked heap metadata arena, valid for the process (the shared test pattern).
fn meta(bytes: usize) -> &'static topo_core::bootstrap::BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe {
        topo_core::bootstrap::BumpArena::new(ptr, len)
    }))
}

fn huge_backend(capacity: usize) -> HugePageBackend<PosixBackingProvider> {
    HugePageBackend::new(
        PosixBackingProvider::new(),
        meta(1 << 20),
        ArenaId::DEFAULT,
        HugeConfig::with_capacity(capacity),
    )
    .expect("hugepage backend")
}

/// Reserve then free `n` whole hugepages, leaving them empty-backed in the HugeCache
/// (committed, cached for reuse) — the supply the controller may release.
fn build_empty_backed_supply(hp: &HugePageBackend<PosixBackingProvider>, n: usize) {
    let regions: Vec<_> = (0..n)
        .map(|_| {
            hp.allocate(HUGEPAGE_SIZE, PAGE, PlaceHints::default())
                .expect("hugepage alloc")
        })
        .collect();
    for r in regions {
        assert!(hp.free_region(r), "free leaves the hugepage empty-backed");
    }
    assert_eq!(
        hp.coverage().empty_backed_bytes,
        (n * HUGEPAGE_SIZE) as u64,
        "n empty-backed hugepages cached"
    );
}

/// Inputs at a fixed Soft-pressure cgroup utilization (80%), with `allocated_total`
/// cumulative bytes — the live oscillation harness (same pressure, varying churn).
fn soft_inputs(allocated_total: u64) -> ReleaseInputs {
    ReleaseInputs {
        cgroup_current: 80,
        cgroup_max: 100,
        allocated_bytes_total: allocated_total,
        ..ReleaseInputs::default()
    }
}

/// A Normal-pressure baseline tick (50% utilization): it preserves hugepages (rung 2
/// does not fire), so it establishes the rate/peak clock **without** releasing the
/// supply the measured tick will act on.
fn normal_baseline() -> ReleaseInputs {
    ReleaseInputs {
        cgroup_current: 50,
        cgroup_max: 100,
        allocated_bytes_total: 0,
        ..ReleaseInputs::default()
    }
}

#[test]
fn idle_release_tick_returns_empty_hugepages_to_the_os() {
    // With no recent allocation the §21.4 reserve is zero, so a Soft-pressure tick
    // releases the whole empty-backed supply live (RSS drops).
    let hp = huge_backend(8);
    build_empty_backed_supply(&hp, 4);
    let mut ctl = ReleaseController::new(DecayConfig::default());

    // Baseline tick establishes the rate clock (no allocation).
    let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
    // 10 s later, still no allocation ⇒ reserve 0 ⇒ release the empties.
    let (plan, released) = hp.release_tick(&mut ctl, 10_000, soft_inputs(0));

    assert_eq!(plan.mode, PressureMode::Soft);
    assert_eq!(plan.demand_reserve_bytes, 0, "idle ⇒ no reserve");
    assert_eq!(
        released,
        4 * HUGEPAGE_SIZE as u64,
        "all empty hugepages released"
    );
    assert_eq!(
        hp.coverage().empty_backed_bytes,
        0,
        "the HugeCache empty-backed supply was returned to the OS"
    );
    assert!(hp.check_invariants());
}

#[test]
fn churning_release_tick_holds_memory_back_against_refault() {
    // The live §21.1 R2 property: at the *same* Soft pressure, a churning workload's
    // reserve covers the empty supply, so the tick releases strictly fewer hugepages
    // than the idle case — it does not release memory it is about to refault.
    let hp = huge_backend(16);
    build_empty_backed_supply(&hp, 4); // 4 hugepages = 8 MiB empty-backed
    let mut ctl = ReleaseController::new(DecayConfig::default());

    let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
    // 1 s later, a large allocation burst ⇒ a reserve that covers the empty supply.
    let churn = soft_inputs(64 * 1024 * 1024); // 64 MiB/s
    let (plan, released) = hp.release_tick(&mut ctl, 1_000, churn);

    assert!(plan.demand_reserve_bytes > 0, "churn ⇒ a reserve is held");
    assert!(
        released < 4 * HUGEPAGE_SIZE as u64,
        "churn releases strictly less than the full idle supply (held back, §21.1 R2)"
    );
    // Some empty-backed supply is retained for the imminent re-allocation.
    assert!(
        hp.coverage().empty_backed_bytes > 0,
        "empty hugepages retained for fast burst reuse, not refaulted"
    );
    assert!(hp.check_invariants());
}

#[test]
fn emergency_release_tick_drains_all_empty_hugepages() {
    // An allocation-failure tick forces Emergency: the HugeCache reserve is disabled
    // and every empty hugepage is drained, even under a tight rate cap (§21.5/§36.5).
    let cfg = DecayConfig {
        release_rate_bytes_per_sec: 1, // 1 B/s — would otherwise grant ~nothing
        ..DecayConfig::default()
    };
    let hp = huge_backend(8);
    build_empty_backed_supply(&hp, 3);
    let mut ctl = ReleaseController::new(cfg);

    let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
    let emergency = ReleaseInputs {
        alloc_failed: true,
        ..soft_inputs(0)
    };
    let (plan, released) = hp.release_tick(&mut ctl, 1_000, emergency);

    assert_eq!(plan.mode, PressureMode::Emergency);
    assert!(plan.disable_hugecache_reserve);
    assert_eq!(
        released,
        3 * HUGEPAGE_SIZE as u64,
        "emergency drains the whole empty supply despite the 1 B/s rate cap"
    );
    assert_eq!(hp.coverage().empty_backed_bytes, 0);
    assert!(hp.check_invariants());
}

#[test]
fn controller_driven_release_preserves_live_objects() {
    // §21.6 end-to-end — the W12 analogue of W14's fixed-wall safety test. With live
    // objects AND a releasable empty-backed supply on the *same* backend, executing the
    // controller's plan returns the free supply to the OS while EVERY live object stays
    // intact and writable. The mechanism theorems prove this in the abstract
    // (`release_to_os_preserves_live_objects`, `subrelease_preserves_live_backing`); the
    // other W12 live-wiring tests release only *empty* supply, so this pins the missing
    // case: that the controller, driven as hard as it goes, never reclaims a live
    // object's pages (the §2.4 "safety before policy" boundary, end to end).
    let hp = huge_backend(64);

    // Live objects, each stamped with a distinct pattern and KEPT live across the
    // release: two whole hugepages plus six sub-hugepage objects (packed onto shared
    // partial hugepages — the cold-sparse subrelease candidates rung 5 may touch, where
    // H-005 must keep the live pages while returning only the free ones).
    let mut live: Vec<(topo_core::Region, u8)> = Vec::new();
    let alloc_live = |size: usize, pat: u8| -> (topo_core::Region, u8) {
        let r = hp
            .allocate(size, PAGE, PlaceHints::default())
            .expect("live alloc");
        assert!(r.len >= size);
        // SAFETY: `r` is a live committed region of `r.len` bytes the backend just vended.
        unsafe { core::ptr::write_bytes(r.base, pat, r.len) };
        (r, pat)
    };
    live.push(alloc_live(HUGEPAGE_SIZE, 0x11));
    live.push(alloc_live(HUGEPAGE_SIZE, 0x22));
    for i in 0..6u8 {
        live.push(alloc_live(64 * 1024, 0x30 + i));
    }

    // Releasable supply: empty-backed hugepages (allocated then freed).
    build_empty_backed_supply(&hp, 6);

    // Drive the controller hard (Emergency: alloc-failure forces it, §21.5/O-007) across
    // several ticks so it reclaims everything it legitimately can.
    let mut ctl = ReleaseController::new(DecayConfig::default());
    let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
    let emergency = ReleaseInputs {
        alloc_failed: true,
        ..soft_inputs(0)
    };
    for t in 1..=8 {
        let _ = hp.release_tick(&mut ctl, t * 1_000, emergency);
    }

    // The §21.6 safety property, end to end: every live object survived untouched.
    for (r, pat) in &live {
        // SAFETY: each `r` is still live (never freed); `r.len` committed bytes.
        unsafe {
            assert_eq!(
                *r.base, *pat,
                "live object head clobbered by a release tick"
            );
            assert_eq!(
                *r.base.add(r.len - 1),
                *pat,
                "live object tail clobbered by a release tick"
            );
        }
    }
    // The free supply was returned to the OS; the backend stays well-formed.
    assert_eq!(
        hp.coverage().empty_backed_bytes,
        0,
        "the releasable empty supply was drained while the live objects were untouched"
    );
    assert!(hp.check_invariants());

    // Clean teardown: every live object frees exactly once.
    for (r, _) in live {
        assert!(hp.free_region(r), "live object frees cleanly after release");
    }
    assert!(hp.check_invariants());
}

#[test]
fn release_stats_reconcile_into_the_stats_snapshot() {
    // W12 stats wiring: the controller's running counters record into the topo_stats
    // snapshot and render in the JSON `release` block with the live pressure mode.
    let hp = huge_backend(8);
    build_empty_backed_supply(&hp, 2);
    let mut ctl = ReleaseController::new(DecayConfig::default());
    let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
    let _ = hp.release_tick(&mut ctl, 5_000, soft_inputs(0));

    let mut s = topo_stats::Stats::default();
    s.record_release(ctl.stats());
    let json = s.to_json();
    assert!(json.contains("\"pressure_mode\": \"soft\""), "{json}");
    assert!(json.contains("\"ticks\": 2"), "{json}");
}

/// W12 / §36.9 backend-agnosticism: the controller is pure policy, so the **identical**
/// release workload driven through [`HugePageBackend::release_tick`] yields the
/// **identical** decision (plan + bytes released + resulting coverage) over the POSIX
/// provider and the seLe4n simulator — the G-sim slice for the release controller.
#[cfg(feature = "sele4n-sim")]
mod sele4n {
    use super::*;
    use topo_backend_sele4n::Sele4nSim;
    use topo_core::TopoBackingProvider;

    fn build_supply<P: TopoBackingProvider>(hp: &HugePageBackend<P>, n: usize) {
        let regions: Vec<_> = (0..n)
            .map(|_| {
                hp.allocate(HUGEPAGE_SIZE, PAGE, PlaceHints::default())
                    .expect("hugepage alloc")
            })
            .collect();
        for r in regions {
            assert!(hp.free_region(r));
        }
    }

    /// The address-independent abstract outcome of an idle-release workload: the plan,
    /// the bytes released, and the resulting coverage. Deterministic, so the two
    /// providers must agree exactly.
    fn release_workload<P: TopoBackingProvider>(
        hp: &HugePageBackend<P>,
    ) -> (topo_core::ReleasePlan, u64, topo_core::HugeStats) {
        build_supply(hp, 4);
        let mut ctl = ReleaseController::new(DecayConfig::default());
        let _ = hp.release_tick(&mut ctl, 0, normal_baseline());
        let (plan, released) = hp.release_tick(&mut ctl, 10_000, soft_inputs(0));
        (plan, released, hp.coverage())
    }

    fn sim_backend(capacity: usize) -> HugePageBackend<Sele4nSim> {
        let pool = (capacity + 1) * HUGEPAGE_SIZE;
        HugePageBackend::new(
            Sele4nSim::new(pool),
            meta(1 << 20),
            ArenaId::DEFAULT,
            HugeConfig::with_capacity(capacity),
        )
        .expect("sim hugepage backend")
    }

    #[test]
    fn release_outcome_is_identical_over_posix_and_the_sele4n_simulator() {
        let posix = huge_backend(8);
        let sim = sim_backend(8);
        let posix_outcome = release_workload(&posix);
        let sim_outcome = release_workload(&sim);
        assert_eq!(
            posix_outcome, sim_outcome,
            "the release controller must decide identically over POSIX and the seLe4n \
             simulator (W12 backend-agnostic, §36.9)"
        );
        // And the decision actually released the idle supply on both.
        assert_eq!(posix_outcome.1, 4 * HUGEPAGE_SIZE as u64);
        assert!(posix.check_invariants() && sim.check_invariants());
    }
}

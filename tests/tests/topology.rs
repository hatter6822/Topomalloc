// SPDX-License-Identifier: MIT
//! Topology-awareness integration tests (§15, plan 04 W13). The in-crate
//! `topo_core::topology` tests cover the pure snapshot / placement / rebalancer in
//! isolation; this file proves the **cross-crate wiring** no single crate can:
//!
//! * **discovery → stats → control:** the real `topo_backend_posix::discover_topology`
//!   feeds a `topo_stats::Stats` snapshot whose `topo.numa.*` keys read back through the
//!   `topo_control` namespace — the full §15.2 observability path;
//! * **placement modes (§15.5):** all five NUMA modes over a built multi-node snapshot;
//! * **no permanent stranding (§15.4):** the rebalancer driven to a fixpoint, asserting
//!   every move preserves the donor (never strands it) and the iteration converges (no
//!   churn) — the system-level property a single `plan` call cannot show.
//! * **the live router over the real POSIX provider:** per-node backends route a request
//!   by policy, free routes home, and the router's §15.4/§15.5 counters reconcile through
//!   stats into the control namespace — the end-to-end placement path.

use topo_backend_posix::{discover_topology, PosixBackingProvider};
use topo_control::Control;
use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{
    topology::MAX_NODES, CoreId, FixedCore, Hints, HugeConfig, HugePageBackend, NodeId,
    NodePressure, NodeRouter, NumaPolicy, Rebalancer, RegionCacheHook, Topology, TopologyBuilder,
    HUGEPAGE_SIZE,
};
use topo_stats::{Profile, Stats};

/// A built two-node topology (CPUs 0,1 on node 0/LLC 0; 2,3 on node 1/LLC 1).
fn two_node() -> Topology {
    let mut b = TopologyBuilder::new(4);
    b.set_cpu(0, 0, 0)
        .set_cpu(1, 0, 0)
        .set_cpu(2, 1, 1)
        .set_cpu(3, 1, 1);
    b.set_distance(0, 1, 21).set_distance(1, 0, 21);
    b.build()
}

#[test]
fn discovered_topology_reconciles_through_stats_into_the_control_namespace() {
    // §15.2 end-to-end: the real platform discovery yields a usable snapshot whose
    // node/LLC counts flow into the stats snapshot and then the Appendix-E control
    // namespace — the observability path that ties topo-backend-posix → topo-core →
    // topo-stats → topo-control together (no single crate's unit tests span it).
    let topo = discover_topology();
    assert!(topo.cpu_count() >= 1);
    assert!(topo.node_count() >= 1);
    assert!(topo.llc_count() >= 1);
    // The discovered snapshot is always within the bounded model (never truncated).
    assert!(topo.node_count() as usize <= MAX_NODES);

    let mut stats = Stats::default();
    stats.record_topology(&topo);
    assert_eq!(stats.numa_nodes, topo.node_count());
    assert_eq!(stats.llc_domains, topo.llc_count());

    let mut control = Control::new(Profile::Performance);
    control.set_stats(stats);
    // The control namespace echoes the discovered counts exactly.
    assert_eq!(
        control.get("topo.numa.nodes").as_deref(),
        Some(topo.node_count().to_string().as_str())
    );
    assert_eq!(
        control.get("topo.numa.llc_domains").as_deref(),
        Some(topo.llc_count().to_string().as_str())
    );
    // …and the same counts appear in the stats JSON topology block.
    let json = control.get("topo.stats.json").expect("stats json");
    assert!(json.contains("\"topology\""));
    assert!(json.contains(&format!("\"numa_nodes\": {}", topo.node_count())));
    assert!(json.contains(&format!("\"llc_domains\": {}", topo.llc_count())));
}

#[test]
fn a_built_multi_node_topology_reconciles_exactly() {
    // A deterministic two-node snapshot reconciles to exactly (2, 2) through the same
    // path, so the discovery test above is exercising real wiring, not a single-domain
    // degenerate that would pass trivially.
    let topo = two_node();
    assert_eq!(topo.node_count(), 2);
    assert_eq!(topo.llc_count(), 2);

    let mut stats = Stats::default();
    stats.record_topology(&topo);
    let mut control = Control::new(Profile::Performance);
    control.set_stats(stats);
    assert_eq!(control.get("topo.numa.nodes").as_deref(), Some("2"));
    assert_eq!(control.get("topo.numa.llc_domains").as_deref(), Some("2"));
}

#[test]
fn placement_covers_every_numa_mode() {
    // §15.5: all five NUMA modes behave over a real multi-node snapshot.
    let topo = two_node();
    let mut il = 0u32;
    // local → the running CPU's node.
    assert_eq!(
        topo.preferred_node(NumaPolicy::Local, 2, &mut il),
        Some(NodeId(1))
    );
    // bind(valid) honored; bind(stale) clamps to the default node, never out of range.
    assert_eq!(
        topo.preferred_node(NumaPolicy::Bind(NodeId(1)), 0, &mut il),
        Some(NodeId(1))
    );
    assert_eq!(
        topo.preferred_node(NumaPolicy::Bind(NodeId(99)), 0, &mut il),
        Some(NodeId::DEFAULT)
    );
    // interleave → deterministic round-robin across the two nodes.
    let mut seen = Vec::new();
    for _ in 0..4 {
        seen.push(topo.preferred_node(NumaPolicy::Interleave, 0, &mut il));
    }
    assert_eq!(
        seen,
        vec![
            Some(NodeId(0)),
            Some(NodeId(1)),
            Some(NodeId(0)),
            Some(NodeId(1))
        ]
    );
    // os_default / arena_policy defer to the caller.
    assert_eq!(topo.preferred_node(NumaPolicy::OsDefault, 0, &mut il), None);
    assert_eq!(
        topo.preferred_node(NumaPolicy::ArenaPolicy, 0, &mut il),
        None
    );
}

#[test]
fn rebalancer_converges_without_ever_stranding_a_donor() {
    // §15.4 system-level: drive the rebalancer to a fixpoint. A donor (node 0) holds
    // free memory **and** its own live demand; two starved nodes pull from it across
    // rounds. Every move must (a) be bounded by the donor's surplus and (b) leave the
    // donor still able to satisfy itself — so memory is relieved with no node ever
    // stranded — and the iteration must converge (no churn).
    let mut b = TopologyBuilder::new(3);
    b.set_cpu(0, 0, 0).set_cpu(1, 1, 1).set_cpu(2, 2, 2);
    let topo = b.build();

    let mut nodes = [
        NodePressure {
            free_bytes: 10 << 20,
            demand_bytes: 2 << 20, // node 0 keeps 2 MiB for itself (surplus 8 MiB)
        },
        NodePressure {
            free_bytes: 0,
            demand_bytes: 4 << 20, // node 1: starved
        },
        NodePressure {
            free_bytes: 0,
            demand_bytes: 3 << 20, // node 2: starved
        },
    ];

    let mut rounds = 0;
    while let Some(m) = Rebalancer::plan(&nodes, &topo) {
        let s = m.src.0 as usize;
        assert!(
            nodes[s].movable_surplus() >= m.bytes,
            "a move never exceeds the donor's surplus"
        );
        assert!(m.apply(&mut nodes), "the canonical move semantics apply it");
        assert_eq!(
            nodes[s].unmet_need(),
            0,
            "after donating only its surplus, the donor still satisfies itself"
        );
        rounds += 1;
        assert!(rounds < 100, "the rebalancer converges (no infinite churn)");
    }

    // Fixpoint: both starved nodes were relieved and the donor kept its own 2 MiB.
    assert_eq!(nodes[1].unmet_need(), 0);
    assert_eq!(nodes[2].unmet_need(), 0);
    assert_eq!(nodes[0].free_bytes, 3 << 20, "10 MiB − 4 − 3 donated");
    assert!(
        nodes[0].free_bytes >= nodes[0].demand_bytes,
        "donor not stranded"
    );
    assert!(
        Rebalancer::plan(&nodes, &topo).is_none(),
        "nothing left stranded"
    );
}

// --- the live router over the real POSIX backing provider -------------------------------

/// A leaked metadata arena, valid for the process (the shared test pattern), large enough
/// for several per-node hugepage backends.
fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

/// Build a two-node router over the real POSIX provider. Each "node" is a distinct
/// reservation; on a single-node host both are physically node 0 and the node-1 `mbind` is
/// a best-effort miss — but the **routing** is logical, so placement is exercised either way.
fn real_two_node_router(cap: usize) -> NodeRouter<PosixBackingProvider, FixedCore> {
    let m = meta(1 << 21);
    let mut b = TopologyBuilder::new(2);
    b.set_cpu(0, 0, 0).set_cpu(1, 1, 1);
    NodeRouter::build(b.build(), FixedCore(CoreId::DEFAULT), |_node, _os| {
        HugePageBackend::new(
            PosixBackingProvider::new(),
            m,
            topo_core::ids::ArenaId::DEFAULT,
            HugeConfig::with_capacity(cap),
        )
        .ok()
    })
    .expect("router over the real POSIX provider")
}

fn hints(numa: NumaPolicy) -> Hints {
    Hints {
        numa,
        ..Default::default()
    }
}

#[test]
fn live_router_routes_by_policy_and_frees_home_over_real_posix() {
    let r = real_two_node_router(8);
    assert_eq!(r.node_count(), 2);
    // A request bound to node 1 lands on node 1's backend; one bound to node 0 on node 0.
    let p0 = r
        .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
        .expect("alloc node 0");
    let p1 = r
        .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
        .expect("alloc node 1");
    assert_eq!(r.node_of_addr(p0.base as usize), Some(0));
    assert_eq!(r.node_of_addr(p1.base as usize), Some(1));
    // The two nodes are distinct reservations.
    assert_ne!(
        r.node_of_addr(p0.base as usize),
        r.node_of_addr(p1.base as usize)
    );
    // The allocation is real, committed memory.
    // SAFETY: `p1` is a live 64 KiB allocation from the router.
    unsafe {
        p1.base.write(0xab);
        assert_eq!(p1.base.read(), 0xab);
    }
    assert!(r.try_cache(p0), "free routes home to node 0's backend");
    assert!(r.try_cache(p1), "free routes home to node 1's backend");
    assert!(r.check_invariants());
}

#[test]
fn live_router_counters_reconcile_into_stats_and_control() {
    // The router's §15.4/§15.5 counters flow router → stats → control. We drive a rebalance
    // tick so the move counter is exercised, then read it back through every layer.
    let r = real_two_node_router(4);
    // Node 0 holds an idle empty-backed hugepage (free); node 1 takes a demand miss.
    let big = r
        .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
        .expect("whole hugepage on node 0");
    assert!(r.try_cache(big), "freed ⇒ empty-backed on node 0");
    // Fill node 1, then a Bind(1) miss records demand (and spills to node 0).
    let mut held = Vec::new();
    for _ in 0..6 {
        if let Some(p) = r.try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1)))) {
            if r.node_of_addr(p.base as usize) == Some(1) {
                held.push(p);
            } else {
                assert!(r.try_cache(p)); // a spillover to node 0
            }
        }
    }
    let _ = r.rebalance_tick(); // plans 0→1 and releases node 0's idle memory

    let rs = r.stats();
    let mut stats = Stats::default();
    stats.record_node_router(rs);
    let mut control = Control::new(Profile::HugepageOptimized);
    control.set_stats(stats);
    assert_eq!(
        control.get("topo.numa.nodes").as_deref(),
        Some(rs.nodes.to_string().as_str())
    );
    assert_eq!(
        control.get("topo.numa.bind_failures").as_deref(),
        Some(rs.bind_failures.to_string().as_str())
    );
    assert_eq!(
        control.get("topo.numa.rebalance_moves").as_deref(),
        Some(rs.rebalance_moves.to_string().as_str())
    );
    // The JSON topology block carries the same counters.
    let json = control.get("topo.stats.json").expect("stats json");
    assert!(json.contains(&format!("\"rebalance_moves\": {}", rs.rebalance_moves)));

    for p in held {
        assert!(r.try_cache(p));
    }
    assert!(r.check_invariants());
}

#[test]
fn live_router_refresh_cycle_over_real_posix() {
    // The §15.2 host-driven refresh: a moved CPU (detected via detect_mismatch inside
    // refresh_if_mismatch) rebuilds + swaps the snapshot, changing Local placement.
    let r = real_two_node_router(4);
    let a = r
        .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Local))
        .expect("local alloc");
    assert_eq!(r.node_of_addr(a.base as usize), Some(0), "cpu 0 ⇒ node 0");
    assert!(r.try_cache(a));
    // cpu 0 observed on node 1 ⇒ refresh.
    let refreshed = r.refresh_if_mismatch(0, NodeId(1), || {
        let mut b = TopologyBuilder::new(2);
        b.set_cpu(0, 1, 1).set_cpu(1, 0, 0);
        b.build()
    });
    assert!(refreshed, "a moved cpu triggers a refresh");
    let a2 = r
        .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Local))
        .expect("local alloc after refresh");
    assert_eq!(
        r.node_of_addr(a2.base as usize),
        Some(1),
        "Local now follows the refreshed map"
    );
    assert!(r.try_cache(a2));
}

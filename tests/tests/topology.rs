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

use topo_backend_posix::discover_topology;
use topo_control::Control;
use topo_core::{
    topology::MAX_NODES, NodeId, NodePressure, NumaPolicy, Rebalancer, Topology, TopologyBuilder,
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

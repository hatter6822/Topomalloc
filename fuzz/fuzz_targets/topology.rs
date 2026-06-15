// SPDX-License-Identifier: MIT
//! Fuzz target for the §15 topology model + cross-domain rebalancer (W13, §34.8). Two
//! properties hold under *any* input:
//!
//! * **Builder totality + dense renumbering:** an arbitrary `TopologyBuilder` stream
//!   (CPUs / nodes / LLCs / distances, including out-of-range and gap inputs) always
//!   `build()`s to a well-formed snapshot — `1 ≤ node_count ≤ MAX_NODES`, every dense id
//!   `< node_count` has a recoverable OS id, and every query (`node_of_cpu`, `llc_of_cpu`,
//!   `os_node_of`, `distance`, `preferred_node`) is **total** (never panics) on arbitrary
//!   CPU/node ids.
//! * **Rebalancer surplus-safety + convergence:** driving `Rebalancer::plan` to a fixpoint
//!   over an arbitrary per-node pressure array, every move is bounded by the donor's
//!   surplus, never strands the donor, makes strict progress, and converges in `≤ nodes`
//!   moves (the §15.4 "never permanently stranded", with no churn).
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::topology::{MAX_CPUS, MAX_NODES};
use topo_core::{NodeId, NodePressure, NumaPolicy, Rebalancer, TopologyBuilder};

/// A tiny deterministic byte reader (saturates to 0 once the input is exhausted).
struct Bytes<'a> {
    d: &'a [u8],
    i: usize,
}
impl<'a> Bytes<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, i: 0 }
    }
    fn u8(&mut self) -> u8 {
        let b = self.d.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        b
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    fn u64(&mut self) -> u64 {
        (u64::from(self.u32()) << 32) | u64::from(self.u32())
    }
}

fuzz_target!(|data: &[u8]| {
    let mut b = Bytes::new(data);

    // --- Build an arbitrary topology snapshot (exercises dense renumbering + fallback) ---
    let n_cpus = b.u32() % (MAX_CPUS as u32 + 4); // include 0 and > MAX_CPUS (→ fallback)
    let mut builder = TopologyBuilder::new(n_cpus);
    let edits = (b.u8() % 80) as usize;
    for _ in 0..edits {
        if b.u8() & 1 == 0 {
            // Deliberately allow out-of-range cpu/node/llc (the builder must tolerate it).
            let _ = builder.set_cpu(
                b.u32() % (MAX_CPUS as u32 + 4),
                b.u32() % (MAX_NODES as u32 + 4),
                b.u32() % 8,
            );
        } else {
            builder.set_distance(
                b.u32() % (MAX_NODES as u32 + 4),
                b.u32() % (MAX_NODES as u32 + 4),
                b.u8(),
            );
        }
    }
    let t = builder.build();

    // Well-formed bounds.
    assert!(t.cpu_count() >= 1);
    let nodes = t.node_count();
    assert!(nodes >= 1 && nodes as usize <= MAX_NODES, "node_count in 1..=MAX_NODES");
    assert!(t.llc_count() >= 1);
    // Every dense id < node_count has a recoverable OS id (and queries are total).
    for d in 0..nodes {
        assert!((t.os_node_of(NodeId(d)) as usize) < MAX_NODES);
    }
    // Arbitrary CPU/node queries never panic (totality).
    for _ in 0..8 {
        let _ = t.node_of_cpu(b.u32());
        let _ = t.llc_of_cpu(b.u32());
        let _ = t.os_node_of(NodeId(b.u32()));
        let _ = t.distance(NodeId(b.u32()), NodeId(b.u32()));
        let mut il = b.u32();
        let _ = t.preferred_node(NumaPolicy::Interleave, b.u32(), &mut il);
        let _ = t.preferred_node(NumaPolicy::Bind(NodeId(b.u32())), 0, &mut il);
        let _ = t.preferred_node_at(NumaPolicy::Local, b.u32(), b.u32());
    }

    // --- Drive the rebalancer to a fixpoint over arbitrary per-node pressures ---
    let mut pressures = [NodePressure::default(); MAX_NODES];
    for p in pressures.iter_mut().take(nodes as usize) {
        p.free_bytes = b.u64() % (1u64 << 20);
        p.demand_bytes = b.u64() % (1u64 << 20);
    }
    let slice = &mut pressures[..nodes as usize];
    let total = |ns: &[NodePressure]| ns.iter().map(|p| p.unmet_need()).sum::<u64>();
    let mut prev = total(slice);
    let mut moves = 0u32;
    while let Some(m) = Rebalancer::plan(slice, &t) {
        let s = m.src.0 as usize;
        assert!(m.bytes > 0 && m.bytes <= slice[s].movable_surplus(), "bounded by surplus");
        assert!(m.apply(slice), "endpoints in range");
        assert_eq!(slice[s].unmet_need(), 0, "never strands the donor");
        let now = total(slice);
        assert!(now < prev, "strict progress");
        prev = now;
        moves += 1;
        assert!(moves <= nodes, "converges in ≤ nodes moves");
    }
});

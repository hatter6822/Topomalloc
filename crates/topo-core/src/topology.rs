// SPDX-License-Identifier: MIT
//! CPU / LLC / NUMA topology awareness (§15, plan 04 W13).
//!
//! Modern systems have nonuniform memory and cache topology (§15.1); treating every
//! CPU as equivalent strands memory in the wrong domain and adds cross-socket traffic.
//! This module models the §15 hierarchy
//!
//! ```text
//! CPU → LLC domain → NUMA node → process/global
//! ```
//!
//! as a **pure, `no_std`, bounded** [`Topology`] snapshot. *Discovery* (sysfs / CPUID /
//! libc, §15.2) is a host concern — the host fills a [`TopologyBuilder`] and calls
//! [`build`](TopologyBuilder::build), which **falls back to a conservative single-domain
//! model** on any missing or inconsistent data (§15.2, the load-bearing safety
//! property: a wrong topology must never misplace — only under-optimize). The snapshot
//! is read on the placement path ([`preferred_node`](Topology::preferred_node), §15.3/
//! §15.5) and rebuilt on hotplug/affinity/cgroup change (§15.2, W13-4).
//!
//! Like the release controller (W12) and the hugepage filler's score (W11), placement
//! is **policy, not safety** (§2.4): a misread topology degrades fragmentation/locality
//! but can never hand back an unsound pointer. So there is no abstract state-machine
//! transition here and no Lean obligation.

use crate::arena::NumaPolicy;
use crate::ids::NodeId;

/// Maximum CPUs the bounded snapshot models; a host reporting more degrades to the
/// conservative single-domain fallback (§15.2) rather than truncating.
pub const MAX_CPUS: usize = 256;
/// Maximum NUMA nodes the bounded snapshot models (and the distance matrix order).
pub const MAX_NODES: usize = 16;
/// Maximum LLC domains the bounded snapshot models.
pub const MAX_LLC: usize = 64;

/// The §15.4 "local" node distance (the ACPI SLIT convention: 10 = same node). Larger
/// values are more remote; the rebalancer (W13-3) prefers the nearest source.
pub const DISTANCE_LOCAL: u8 = 10;
/// The default distance assumed between two distinct nodes when the host supplies none.
pub const DISTANCE_REMOTE: u8 = 20;

/// A CPU/LLC/NUMA topology snapshot (§15.2). Bounded and `Copy`-free (it owns a few
/// hundred bytes of mapping tables); the host keeps one, refreshing it on hotplug.
/// Every query is total — an out-of-range CPU reads as the default domain, so the
/// placement path never panics on a stale CPU id.
#[derive(Clone)]
pub struct Topology {
    n_cpus: u16,
    n_nodes: u16,
    n_llc: u16,
    /// `cpu_node[c]` = the NUMA node index of CPU `c` (`0` beyond `n_cpus`).
    cpu_node: [u16; MAX_CPUS],
    /// `cpu_llc[c]` = the LLC-domain index of CPU `c` (`0` beyond `n_cpus`).
    cpu_llc: [u16; MAX_CPUS],
    /// `node_dist[a][b]` = the §15.4 distance from node `a` to node `b` (10 = local).
    node_dist: [[u8; MAX_NODES]; MAX_NODES],
}

impl Topology {
    /// The conservative **single-domain fallback** (§15.2): every CPU in one LLC domain
    /// and one NUMA node, `n_cpus` clamped to [`MAX_CPUS`]. Always correct, never
    /// optimal — the model used whenever discovery data is missing or inconsistent.
    pub fn single_domain(n_cpus: u32) -> Topology {
        let n = (n_cpus.max(1) as usize).min(MAX_CPUS) as u16;
        let mut node_dist = [[DISTANCE_REMOTE; MAX_NODES]; MAX_NODES];
        // The diagonal is local distance; only node 0 is populated here.
        let mut i = 0;
        while i < MAX_NODES {
            node_dist[i][i] = DISTANCE_LOCAL;
            i += 1;
        }
        Topology {
            n_cpus: n,
            n_nodes: 1,
            n_llc: 1,
            cpu_node: [0; MAX_CPUS],
            cpu_llc: [0; MAX_CPUS],
            node_dist,
        }
    }

    /// The NUMA node of `cpu` (§15.3). Out-of-range CPUs read as the default node.
    pub fn node_of_cpu(&self, cpu: u32) -> NodeId {
        if (cpu as usize) < self.n_cpus as usize {
            NodeId(self.cpu_node[cpu as usize] as u32)
        } else {
            NodeId::DEFAULT
        }
    }

    /// The LLC-domain index of `cpu` (§15.3). Out-of-range CPUs read as domain `0`.
    pub fn llc_of_cpu(&self, cpu: u32) -> u32 {
        if (cpu as usize) < self.n_cpus as usize {
            self.cpu_llc[cpu as usize] as u32
        } else {
            0
        }
    }

    /// The number of NUMA nodes (≥ 1).
    pub fn node_count(&self) -> u32 {
        self.n_nodes as u32
    }

    /// The number of LLC domains (≥ 1).
    pub fn llc_count(&self) -> u32 {
        self.n_llc as u32
    }

    /// The number of modeled CPUs (≥ 1).
    pub fn cpu_count(&self) -> u32 {
        self.n_cpus as u32
    }

    /// Whether this is the conservative single-domain model (§15.2): one node, one LLC.
    pub fn is_single_domain(&self) -> bool {
        self.n_nodes <= 1 && self.n_llc <= 1
    }

    /// The §15.4 distance between two nodes (10 = local; larger = more remote). Used by
    /// the rebalancer to prefer the nearest source. Out-of-range nodes read as remote.
    pub fn distance(&self, a: NodeId, b: NodeId) -> u8 {
        let (a, b) = (a.0 as usize, b.0 as usize);
        if a < MAX_NODES && b < MAX_NODES {
            self.node_dist[a][b]
        } else if a == b {
            DISTANCE_LOCAL
        } else {
            DISTANCE_REMOTE
        }
    }

    /// **The §15.3/§15.5 placement decision.** The preferred NUMA node for a request
    /// under `policy`, running on `current_cpu`, advancing `interleave` for the
    /// round-robin mode. `None` means "do not override OS/arena placement" — the caller
    /// then leaves backing to the OS (`OsDefault`) or resolves the arena's own policy
    /// (`ArenaPolicy`).
    ///
    /// Placement is **policy, not a modeled transition** (§2.4): it steers locality, so
    /// it carries no `SPEC-transition` tag and no Lean obligation.
    pub fn preferred_node(
        &self,
        policy: NumaPolicy,
        current_cpu: u32,
        interleave: &mut u32,
    ) -> Option<NodeId> {
        match policy {
            // Prefer the current CPU's node (§15.3 "prefer current NUMA node").
            NumaPolicy::Local => Some(self.node_of_cpu(current_cpu)),
            // A specific node, clamped into range (a stale bind degrades to node 0, never
            // an out-of-range read — §15.2 tolerate-inconsistent).
            NumaPolicy::Bind(node) => Some(if (node.0) < self.n_nodes as u32 {
                node
            } else {
                NodeId::DEFAULT
            }),
            // Round-robin across the nodes (§15.5 interleave); deterministic given the
            // counter, so test mode is reproducible.
            NumaPolicy::Interleave => {
                let n = self.n_nodes.max(1) as u32;
                let node = *interleave % n;
                *interleave = interleave.wrapping_add(1);
                Some(NodeId(node))
            }
            // Defer: the OS default, or the arena's own placement, is honored by the
            // caller (§15.5).
            NumaPolicy::OsDefault | NumaPolicy::ArenaPolicy => None,
        }
    }
}

/// A builder the host fills from discovered topology (§15.2) — one
/// [`set_cpu`](Self::set_cpu) per online CPU, optional [`set_distance`](Self::set_distance)
/// edges — then [`build`](Self::build)s the snapshot. Any inconsistency (no CPUs, an
/// out-of-range node/LLC, a CPU never set) collapses to the single-domain fallback, so
/// a partial or contradictory read is always *safe*.
pub struct TopologyBuilder {
    n_cpus: u16,
    cpu_node: [u16; MAX_CPUS],
    cpu_llc: [u16; MAX_CPUS],
    cpu_set: [bool; MAX_CPUS],
    node_dist: [[u8; MAX_NODES]; MAX_NODES],
    max_node: u16,
    max_llc: u16,
    consistent: bool,
}

impl TopologyBuilder {
    /// A builder for `n_cpus` online CPUs. `n_cpus == 0` or `> MAX_CPUS` marks the build
    /// inconsistent up front (it will yield the single-domain fallback).
    pub fn new(n_cpus: u32) -> TopologyBuilder {
        let ok = n_cpus >= 1 && (n_cpus as usize) <= MAX_CPUS;
        let mut node_dist = [[DISTANCE_REMOTE; MAX_NODES]; MAX_NODES];
        let mut i = 0;
        while i < MAX_NODES {
            node_dist[i][i] = DISTANCE_LOCAL;
            i += 1;
        }
        TopologyBuilder {
            n_cpus: if ok { n_cpus as u16 } else { 0 },
            cpu_node: [0; MAX_CPUS],
            cpu_llc: [0; MAX_CPUS],
            cpu_set: [false; MAX_CPUS],
            node_dist,
            max_node: 0,
            max_llc: 0,
            consistent: ok,
        }
    }

    /// Record CPU `cpu`'s NUMA node and LLC domain. An out-of-range argument marks the
    /// build inconsistent (→ single-domain fallback).
    pub fn set_cpu(&mut self, cpu: u32, node: u32, llc: u32) -> &mut Self {
        if (cpu as usize) < self.n_cpus as usize
            && (node as usize) < MAX_NODES
            && (llc as usize) < MAX_LLC
        {
            self.cpu_node[cpu as usize] = node as u16;
            self.cpu_llc[cpu as usize] = llc as u16;
            self.cpu_set[cpu as usize] = true;
            self.max_node = self.max_node.max(node as u16);
            self.max_llc = self.max_llc.max(llc as u16);
        } else {
            self.consistent = false;
        }
        self
    }

    /// Record the §15.4 distance from node `a` to node `b` (10 = local). Out-of-range
    /// nodes are ignored (the default remote distance stands).
    pub fn set_distance(&mut self, a: u32, b: u32, dist: u8) -> &mut Self {
        if (a as usize) < MAX_NODES && (b as usize) < MAX_NODES {
            self.node_dist[a as usize][b as usize] = dist;
        }
        self
    }

    /// Build the snapshot (§15.2). Returns the discovered [`Topology`] only if every
    /// online CPU was set consistently; otherwise the conservative single-domain
    /// fallback over the same CPU count.
    pub fn build(self) -> Topology {
        if !self.consistent || self.n_cpus == 0 {
            return Topology::single_domain(self.n_cpus.max(1) as u32);
        }
        // Every online CPU must have been recorded (no gaps), else fall back.
        let mut c = 0usize;
        while c < self.n_cpus as usize {
            if !self.cpu_set[c] {
                return Topology::single_domain(self.n_cpus as u32);
            }
            c += 1;
        }
        Topology {
            n_cpus: self.n_cpus,
            n_nodes: self.max_node + 1,
            n_llc: self.max_llc + 1,
            cpu_node: self.cpu_node,
            cpu_llc: self.cpu_llc,
            node_dist: self.node_dist,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_domain_is_one_node_one_llc() {
        let t = Topology::single_domain(8);
        assert!(t.is_single_domain());
        assert_eq!(t.node_count(), 1);
        assert_eq!(t.llc_count(), 1);
        assert_eq!(t.cpu_count(), 8);
        assert_eq!(t.node_of_cpu(3), NodeId(0));
        assert_eq!(t.node_of_cpu(999), NodeId::DEFAULT); // out-of-range is total
        assert_eq!(t.distance(NodeId(0), NodeId(0)), DISTANCE_LOCAL);
    }

    #[test]
    fn builder_models_a_two_node_machine() {
        // 4 CPUs: 0,1 on node 0 / LLC 0; 2,3 on node 1 / LLC 1.
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0)
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 1)
            .set_cpu(3, 1, 1);
        b.set_distance(0, 1, 21).set_distance(1, 0, 21);
        let t = b.build();
        assert!(!t.is_single_domain());
        assert_eq!(t.node_count(), 2);
        assert_eq!(t.llc_count(), 2);
        assert_eq!(t.node_of_cpu(1), NodeId(0));
        assert_eq!(t.node_of_cpu(2), NodeId(1));
        assert_eq!(t.llc_of_cpu(3), 1);
        assert_eq!(t.distance(NodeId(0), NodeId(1)), 21);
    }

    #[test]
    fn inconsistent_data_falls_back_to_single_domain() {
        // A gap (CPU 2 never set) ⇒ conservative fallback (§15.2).
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0).set_cpu(1, 0, 0).set_cpu(3, 1, 1);
        let t = b.build();
        assert!(t.is_single_domain(), "a gap ⇒ single-domain fallback");
        assert_eq!(t.cpu_count(), 4);

        // An out-of-range node also falls back.
        let mut b2 = TopologyBuilder::new(2);
        b2.set_cpu(0, 0, 0).set_cpu(1, 99, 0);
        assert!(b2.build().is_single_domain());

        // Zero CPUs ⇒ a valid one-CPU single domain (never empty).
        assert_eq!(TopologyBuilder::new(0).build().cpu_count(), 1);
    }

    #[test]
    fn placement_local_prefers_the_running_cpus_node() {
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0)
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 1)
            .set_cpu(3, 1, 1);
        let t = b.build();
        let mut il = 0;
        assert_eq!(
            t.preferred_node(NumaPolicy::Local, 3, &mut il),
            Some(NodeId(1))
        );
        assert_eq!(
            t.preferred_node(NumaPolicy::Local, 0, &mut il),
            Some(NodeId(0))
        );
    }

    #[test]
    fn placement_bind_clamps_and_interleave_round_robins() {
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0)
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 1)
            .set_cpu(3, 1, 1);
        let t = b.build();
        let mut il = 0;
        // Bind to a valid node is honored; a stale out-of-range bind clamps to default.
        assert_eq!(
            t.preferred_node(NumaPolicy::Bind(NodeId(1)), 0, &mut il),
            Some(NodeId(1))
        );
        assert_eq!(
            t.preferred_node(NumaPolicy::Bind(NodeId(7)), 0, &mut il),
            Some(NodeId(0))
        );
        // Interleave round-robins across the two nodes deterministically.
        let seq: [Option<NodeId>; 4] =
            core::array::from_fn(|_| t.preferred_node(NumaPolicy::Interleave, 0, &mut il));
        assert_eq!(
            seq,
            [
                Some(NodeId(0)),
                Some(NodeId(1)),
                Some(NodeId(0)),
                Some(NodeId(1))
            ]
        );
    }

    #[test]
    fn placement_osdefault_and_arenapolicy_defer() {
        let t = Topology::single_domain(4);
        let mut il = 0;
        assert_eq!(t.preferred_node(NumaPolicy::OsDefault, 0, &mut il), None);
        assert_eq!(t.preferred_node(NumaPolicy::ArenaPolicy, 0, &mut il), None);
    }
}

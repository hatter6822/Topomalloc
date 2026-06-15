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
///
/// **Node *and* LLC ids are dense and internal.** [`build`](TopologyBuilder::build)
/// densely renumbers the OS NUMA node ids — and, by the identical construction, the
/// LLC-domain ids — actually in use to `0..node_count()` / `0..llc_count()`, so every id in
/// those ranges is a real domain: no phantom gaps even when the platform numbers nodes (or
/// LLC domains) sparsely (OS nodes 0 and 2 present, 1 absent). The raw OS *node* id is
/// preserved in `node_os_id` and recovered by [`os_node_of`](Self::os_node_of) for syscalls
/// (`mbind`/`set_mempolicy`), which need the kernel's node number; LLC ids are purely
/// internal (no syscall consumes them), so no raw-id map is kept for them.
#[derive(Clone)]
pub struct Topology {
    n_cpus: u16,
    n_nodes: u16,
    n_llc: u16,
    /// `cpu_node[c]` = the dense NUMA node index of CPU `c` (`0` beyond `n_cpus`).
    cpu_node: [u16; MAX_CPUS],
    /// `cpu_llc[c]` = the LLC-domain index of CPU `c` (`0` beyond `n_cpus`).
    cpu_llc: [u16; MAX_CPUS],
    /// `node_dist[a][b]` = the §15.4 distance from dense node `a` to dense node `b`
    /// (10 = local).
    node_dist: [[u8; MAX_NODES]; MAX_NODES],
    /// `node_os_id[d]` = the **OS** NUMA node number of dense node `d` (the identity map
    /// `node_os_id[d] == d` for the common dense-numbered platform; `0` beyond
    /// `n_nodes`). Used only by the backing provider's `mbind` path (§15.5, W13).
    node_os_id: [u16; MAX_NODES],
}

impl Topology {
    /// The conservative **single-domain fallback** (§15.2): every CPU in one LLC domain
    /// and one NUMA node, `n_cpus` clamped to [`MAX_CPUS`]. Always correct, never
    /// optimal — the model used whenever discovery data is missing or inconsistent.
    pub fn single_domain(n_cpus: u32) -> Topology {
        let n = (n_cpus.max(1) as usize).min(MAX_CPUS) as u16;
        let mut node_dist = [[DISTANCE_REMOTE; MAX_NODES]; MAX_NODES];
        let mut node_os_id = [0u16; MAX_NODES];
        // The diagonal is local distance; the OS-id map is the identity (dense node `i`
        // ⇒ OS node `i`) so a stray `os_node_of` past `n_nodes` reads sanely.
        let mut i = 0;
        while i < MAX_NODES {
            node_dist[i][i] = DISTANCE_LOCAL;
            node_os_id[i] = i as u16;
            i += 1;
        }
        Topology {
            n_cpus: n,
            n_nodes: 1,
            n_llc: 1,
            cpu_node: [0; MAX_CPUS],
            cpu_llc: [0; MAX_CPUS],
            node_dist,
            node_os_id,
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

    /// The **OS** NUMA node number of dense node `node` (§15.5, W13) — the kernel id the
    /// backing provider's `mbind`/`set_mempolicy` needs. The identity (`os == dense`) on
    /// the common dense-numbered platform; differs only when the OS numbers nodes
    /// sparsely. Out-of-range dense ids read as `0` (the default node).
    pub fn os_node_of(&self, node: NodeId) -> u32 {
        let d = node.0 as usize;
        if d < self.n_nodes as usize {
            self.node_os_id[d] as u32
        } else {
            0
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

    /// **§15.2 periodic-mismatch check (W13-4).** Whether a freshly-observed
    /// `(cpu, observed_node)` pair disagrees with this snapshot — the cheap probe a
    /// background action runs when the platform gives no hotplug notification: a `true`
    /// result means the host should rebuild the snapshot (via [`TopologyBuilder`]) and
    /// swap it in. A single-domain snapshot never reports a mismatch (it claims nothing
    /// to contradict).
    pub fn detect_mismatch(&self, cpu: u32, observed_node: NodeId) -> bool {
        !self.is_single_domain() && self.node_of_cpu(cpu) != observed_node
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
        let node = self.preferred_node_at(policy, current_cpu, *interleave);
        if matches!(policy, NumaPolicy::Interleave) {
            *interleave = interleave.wrapping_add(1);
        }
        node
    }

    /// The pure form of [`preferred_node`](Self::preferred_node): the decision for an
    /// explicit `interleave` counter *value*, with no mutation — so a lock-free caller
    /// (the [live router](crate::NodeRouter)) can drive the round-robin from an atomic it
    /// advances itself. Identical otherwise; `Interleave` rotates by `interleave % nodes`.
    pub fn preferred_node_at(
        &self,
        policy: NumaPolicy,
        current_cpu: u32,
        interleave: u32,
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
                Some(NodeId(interleave % n))
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
    ///
    /// Both the OS NUMA node ids and the LLC-domain ids actually in use are **densely
    /// renumbered** to `0..node_count()` / `0..llc_count()`, so a sparsely-numbered platform
    /// (e.g. OS nodes 0 and 2 present, 1 absent) yields exactly two nodes rather than a
    /// three-node model with a phantom node 1 — and likewise for LLC domains. The raw OS
    /// *node* id is kept in `node_os_id` for the `mbind` path ([`Topology::os_node_of`]); LLC
    /// ids are internal only. On a dense-numbered platform the renumbering is the identity,
    /// so the common case is unchanged.
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

        // Which OS node ids are actually used by an online CPU? (`cpu_node[k] < MAX_NODES`
        // is guaranteed by `set_cpu`, and the no-gap check above means every CPU is set.)
        let mut used = [false; MAX_NODES];
        let mut k = 0usize;
        while k < self.n_cpus as usize {
            used[self.cpu_node[k] as usize] = true;
            k += 1;
        }
        // Assign dense ids in increasing OS-id order (so a dense platform is the identity).
        let mut dense_of_os = [0u16; MAX_NODES];
        let mut node_os_id = [0u16; MAX_NODES];
        let mut n_nodes = 0u16;
        let mut os = 0usize;
        while os < MAX_NODES {
            if used[os] {
                dense_of_os[os] = n_nodes;
                node_os_id[n_nodes as usize] = os as u16;
                n_nodes += 1;
            }
            os += 1;
        }
        // At least one CPU is set (n_cpus ≥ 1, no gaps), so at least one node is used.
        debug_assert!(n_nodes >= 1, "a consistent build has at least one node");

        // Remap each CPU's node to its dense id.
        let mut cpu_node = [0u16; MAX_CPUS];
        let mut k = 0usize;
        while k < self.n_cpus as usize {
            cpu_node[k] = dense_of_os[self.cpu_node[k] as usize];
            k += 1;
        }
        // Project the OS-indexed distance matrix onto the dense ids (dropping absent
        // rows/cols, so `distance(a, b)` indexes by dense id like every other query). The
        // full diagonal is local (as `single_domain`/`new` establish), so `distance(d, d)`
        // is `DISTANCE_LOCAL` even for the unused dense slots; the projection below
        // overwrites the in-range submatrix (whose diagonal is also local in `self`).
        let mut node_dist = [[DISTANCE_REMOTE; MAX_NODES]; MAX_NODES];
        let mut i = 0;
        while i < MAX_NODES {
            node_dist[i][i] = DISTANCE_LOCAL;
            i += 1;
        }
        let mut da = 0usize;
        while da < n_nodes as usize {
            let mut db = 0usize;
            while db < n_nodes as usize {
                node_dist[da][db] =
                    self.node_dist[node_os_id[da] as usize][node_os_id[db] as usize];
                db += 1;
            }
            da += 1;
        }
        // Identity-fill the unused dense slots' OS map so a stray `os_node_of` is sane.
        let mut d = n_nodes as usize;
        while d < MAX_NODES {
            node_os_id[d] = d as u16;
            d += 1;
        }

        // Densely renumber the LLC domains actually in use to `0..n_llc`, mirroring the
        // node renumbering above, so a sparsely-numbered LLC space yields no phantom domain
        // in `llc_count()`. On the discovery path the ids arrive already dense (the sysfs
        // reader renumbers package ids), so this is the identity there; doing it *here* in
        // the builder hardens every direct caller too (defense in depth — §15.2 "no phantom
        // domain"). LLC ids are internal only, so — unlike nodes — no raw-id map is kept.
        // `self.cpu_llc[k] < MAX_LLC` is guaranteed by `set_cpu`, and the no-gap check above
        // means every online CPU is set, so the index and the `≥ 1` count both hold.
        let mut llc_used = [false; MAX_LLC];
        let mut k = 0usize;
        while k < self.n_cpus as usize {
            llc_used[self.cpu_llc[k] as usize] = true;
            k += 1;
        }
        let mut dense_llc_of = [0u16; MAX_LLC];
        let mut n_llc = 0u16;
        let mut l = 0usize;
        while l < MAX_LLC {
            if llc_used[l] {
                dense_llc_of[l] = n_llc;
                n_llc += 1;
            }
            l += 1;
        }
        debug_assert!(n_llc >= 1, "a consistent build has at least one LLC domain");
        let mut cpu_llc = [0u16; MAX_CPUS];
        let mut k = 0usize;
        while k < self.n_cpus as usize {
            cpu_llc[k] = dense_llc_of[self.cpu_llc[k] as usize];
            k += 1;
        }

        Topology {
            n_cpus: self.n_cpus,
            n_nodes,
            n_llc,
            cpu_node,
            cpu_llc,
            node_dist,
            node_os_id,
        }
    }
}

/// The §15.4 rebalancing tier a planned move uses, in preference order (lower is
/// cheaper / more local). The cross-node tiers (4–5) are what the node-granularity
/// [`Rebalancer`] plans; the same-node cache tiers (1–2) are the transfer/central cache
/// layer's job (plan 05 W6) and are named here for completeness.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RebalanceTier {
    /// 1. Move transfer-cache batches within the same NUMA node (cache layer, M2).
    TransferCacheSameNode,
    /// 2. Move central free-list batches within the same NUMA node (cache layer, M2).
    CentralBatchSameNode,
    /// 3. Move empty spans between arenas if policy permits.
    EmptySpanCrossArena,
    /// 4. Return memory to the backend and reallocate on the target node.
    BackendReallocTargetNode,
    /// 5. Remote reuse — only when cheaper than an OS allocation or pressure demands it.
    RemoteReuse,
}

/// One node's movable-free / demand observation, the rebalancer input (§15.4). The two
/// fields are **gross**: the node's spare memory and the node's total local demand. The
/// rebalancer derives the *net* quantities it acts on — [`unmet_need`](Self::unmet_need)
/// (what makes a node a recipient) and [`movable_surplus`](Self::movable_surplus) (what a
/// node may safely donate) — from them, so a node holding both free memory and live demand
/// is read correctly (it donates only its surplus, §15.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodePressure {
    /// Free, movable bytes currently parked on this node (empty spans / cached batches).
    pub free_bytes: u64,
    /// **Gross** allocation demand (pressure) on this node — the bytes it wants live, of
    /// which [`free_bytes`](Self::free_bytes) covers what it can; the shortfall is its
    /// [`unmet_need`](Self::unmet_need).
    pub demand_bytes: u64,
}

impl NodePressure {
    /// The node's **net unmet demand** (§15.4): gross demand beyond the free memory it
    /// holds locally — the shortfall that makes it a rebalance *recipient*. Zero when the
    /// node can satisfy its own demand from its parked free memory.
    #[inline]
    pub const fn unmet_need(&self) -> u64 {
        self.demand_bytes.saturating_sub(self.free_bytes)
    }

    /// The node's **movable surplus** (§15.4): free memory beyond its *own* demand — the
    /// bytes it can donate without stranding itself (donating more would create the very
    /// unmet need the rebalancer exists to relieve). Zero when the node has no spare.
    ///
    /// Complementary to [`unmet_need`](Self::unmet_need): for any node exactly one of the
    /// two is nonzero (or both zero when `free == demand`), since one is `demand − free`
    /// and the other `free − demand`.
    #[inline]
    pub const fn movable_surplus(&self) -> u64 {
        self.free_bytes.saturating_sub(self.demand_bytes)
    }
}

/// A planned cross-domain move (§15.4): take `bytes` of free memory from `src` to relieve
/// pressure on `dst`, via the chosen [`RebalanceTier`]. The host executes it by draining
/// `src` and re-providing on `dst`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RebalanceMove {
    /// The donor node (has movable surplus beyond its own demand).
    pub src: NodeId,
    /// The pressured node (has unmet demand).
    pub dst: NodeId,
    /// Bytes to move — bounded by the donor's [`movable_surplus`](NodePressure::movable_surplus)
    /// and the recipient's [`unmet_need`](NodePressure::unmet_need), so the move never
    /// strands the donor (§15.4).
    pub bytes: u64,
    /// The §15.4 preference tier this move uses.
    pub tier: RebalanceTier,
}

impl RebalanceMove {
    /// Apply this move to a per-node pressure array — the **canonical move semantics**
    /// shared by the rebalancer's tests and the live executor ([`NodeRouter`](crate::NodeRouter)):
    /// the donor parts with `bytes` of its free memory and the recipient's gross demand is
    /// relieved by the same amount. Bounded by construction (`bytes ≤ donor surplus ≤
    /// donor free` and `bytes ≤ recipient need ≤ recipient demand`), so neither field
    /// underflows; out-of-range `src`/`dst` for `nodes` are ignored (total, never panics).
    ///
    /// Returns `true` if it was applied (both endpoints in range). Applying a
    /// [`plan`](Rebalancer::plan) result never strands the donor: afterwards `nodes[src]`
    /// still satisfies its own demand ([`unmet_need`](NodePressure::unmet_need) stays 0).
    pub fn apply(&self, nodes: &mut [NodePressure]) -> bool {
        let (s, d) = (self.src.0 as usize, self.dst.0 as usize);
        if s >= nodes.len() || d >= nodes.len() {
            return false;
        }
        nodes[s].free_bytes = nodes[s].free_bytes.saturating_sub(self.bytes);
        nodes[d].demand_bytes = nodes[d].demand_bytes.saturating_sub(self.bytes);
        true
    }
}

/// **The §15.4 cross-domain rebalancer (W13-3).** A pure policy that, given each node's
/// free/demand and the topology, plans a move from the nearest *surplus* donor to the
/// most-pressured node so memory is **never permanently stranded** (§15.4): if any node has
/// unmet demand it cannot satisfy locally *and* another node has movable surplus,
/// [`plan`](Self::plan) returns a move. Node-granularity (same-node cache moves are the M2
/// cache layer's job).
pub struct Rebalancer;

impl Rebalancer {
    /// Plan one move (the most valuable this round), or `None` if nothing is stranded —
    /// every pressured node can satisfy itself locally, or no node has movable surplus.
    ///
    /// `nodes[i]` is node `i`'s pressure; entries beyond [`Topology::node_count`] are
    /// ignored. The recipient is the node with the greatest
    /// [`unmet_need`](NodePressure::unmet_need) (`demand − local free`); the donor is the
    /// **nearest** (by [`Topology::distance`]) other node with
    /// [`movable_surplus`](NodePressure::movable_surplus) (`free − own demand`), ties
    /// broken toward the **larger** surplus so one move covers more of the need (§15.4
    /// prefer-close). Using *surplus* rather than raw free is what keeps the move from
    /// stranding the donor: it never gives away memory the donor needs for its own demand,
    /// so it can never create the unmet need it exists to relieve (and a round where no
    /// node has spare memory plans nothing rather than churning memory between equally
    /// starved nodes).
    ///
    /// The move size is `min(recipient need, donor surplus)`. The tier is
    /// [`BackendReallocTargetNode`](RebalanceTier::BackendReallocTargetNode) normally, or
    /// [`RemoteReuse`](RebalanceTier::RemoteReuse) when the recipient is under acute
    /// pressure (its net need exceeds the donor's surplus — pressure "demands it", §15.4).
    pub fn plan(nodes: &[NodePressure], topo: &Topology) -> Option<RebalanceMove> {
        let n = (topo.node_count() as usize).min(nodes.len());
        if n < 2 {
            return None; // a single domain cannot strand across nodes
        }
        // Recipient: the greatest net unmet demand (demand beyond what it holds locally).
        let mut dst = None;
        let mut dst_need = 0u64;
        for (i, p) in nodes.iter().enumerate().take(n) {
            let need = p.unmet_need();
            if need > dst_need {
                dst_need = need;
                dst = Some(i);
            }
        }
        let dst = dst?; // nobody has net unmet demand ⇒ nothing stranded

        // Donor: the nearest *other* node with movable surplus — free memory beyond its
        // OWN demand, so donating it never strands the donor (§15.4 prefer-close). Among
        // equidistant donors, the one with the larger surplus wins (covers more in one
        // move). A node with net need has zero surplus, so it is never picked as a donor.
        let mut src = None;
        let mut best: (u8, u64) = (u8::MAX, 0); // (distance, surplus): min dist, then max surplus
        for (i, p) in nodes.iter().enumerate().take(n) {
            let surplus = p.movable_surplus();
            if i == dst || surplus == 0 {
                continue;
            }
            let d = topo.distance(NodeId(i as u32), NodeId(dst as u32));
            if d < best.0 || (d == best.0 && surplus > best.1) {
                best = (d, surplus);
                src = Some(i);
            }
        }
        let src = src?; // no donor has surplus ⇒ cannot rebalance without stranding someone

        let donor_surplus = nodes[src].movable_surplus();
        let bytes = dst_need.min(donor_surplus);
        // Every returned move transfers a positive amount: the recipient has `dst_need > 0`
        // and the donor `donor_surplus > 0` (both gates above), so their min is ≥ 1 — the
        // rebalancer never emits a no-op move.
        debug_assert!(bytes > 0, "a planned move always transfers ≥ 1 byte");
        // Acute pressure (the donor's surplus cannot fully cover the need) justifies remote
        // reuse; otherwise return-to-backend-and-realloc-on-target is the cheaper default.
        let tier = if dst_need > donor_surplus {
            RebalanceTier::RemoteReuse
        } else {
            RebalanceTier::BackendReallocTargetNode
        };
        Some(RebalanceMove {
            src: NodeId(src as u32),
            dst: NodeId(dst as u32),
            bytes,
            tier,
        })
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
    fn build_densely_renumbers_sparse_os_nodes_no_phantom() {
        // A platform that numbers NUMA nodes sparsely — OS nodes 0 and 2 present, node 1
        // absent — must yield exactly TWO dense nodes, not a three-node model with a
        // phantom node 1. The OS ids are recoverable for the mbind path (§15.5).
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0) // OS node 0
            .set_cpu(1, 0, 0)
            .set_cpu(2, 2, 1) // OS node 2 (node 1 never appears)
            .set_cpu(3, 2, 1);
        // Distances are given with OS ids; the diagonal stays local after renumbering.
        b.set_distance(0, 2, 22).set_distance(2, 0, 22);
        let t = b.build();

        assert_eq!(t.node_count(), 2, "two real nodes, no phantom node 1");
        assert!(!t.is_single_domain());
        // CPUs are remapped to dense ids: OS 0 ⇒ dense 0, OS 2 ⇒ dense 1.
        assert_eq!(t.node_of_cpu(1), NodeId(0));
        assert_eq!(t.node_of_cpu(2), NodeId(1));
        // The OS ids round-trip for the syscall path: dense 0 ⇒ OS 0, dense 1 ⇒ OS 2.
        assert_eq!(t.os_node_of(NodeId(0)), 0);
        assert_eq!(t.os_node_of(NodeId(1)), 2);
        // The distance matrix is projected onto the dense ids (OS 0↔2 distance at 0↔1).
        assert_eq!(t.distance(NodeId(0), NodeId(1)), 22);
        assert_eq!(t.distance(NodeId(0), NodeId(0)), DISTANCE_LOCAL);
        // An out-of-range dense id reads as the default OS node (total).
        assert_eq!(t.os_node_of(NodeId(9)), 0);
    }

    #[test]
    fn build_densely_renumbers_sparse_llc_domains_no_phantom() {
        // The LLC axis gets the same no-phantom guarantee as the node axis: a platform
        // whose LLC ids are sparse — LLC domains 0 and 3 present, 1 and 2 absent — must
        // yield exactly TWO dense LLC domains, not `max + 1 == 4` with two phantom ones.
        // This hardens a *direct* builder caller; the sysfs reader already passes dense
        // ids, so the discovery path is unchanged (the renumber is the identity there).
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0) // LLC 0
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 3) // LLC 3 (1 and 2 never appear)
            .set_cpu(3, 1, 3);
        let t = b.build();

        assert_eq!(t.node_count(), 2);
        assert_eq!(
            t.llc_count(),
            2,
            "two real LLC domains, no phantom 1/2 from max+1"
        );
        // CPUs are remapped to dense LLC ids: OS LLC 0 ⇒ dense 0, OS LLC 3 ⇒ dense 1.
        assert_eq!(t.llc_of_cpu(0), 0);
        assert_eq!(t.llc_of_cpu(1), 0);
        assert_eq!(t.llc_of_cpu(2), 1);
        assert_eq!(t.llc_of_cpu(3), 1);
        // An out-of-range CPU still reads as LLC domain 0 (total).
        assert_eq!(t.llc_of_cpu(999), 0);
    }

    #[test]
    fn build_is_identity_on_a_dense_numbered_platform() {
        // The common case (OS nodes 0..k all present) renumbers to the identity, so
        // os_node_of is the identity and nothing about the dense path changes behavior.
        let mut b = TopologyBuilder::new(3);
        b.set_cpu(0, 0, 0).set_cpu(1, 1, 1).set_cpu(2, 2, 2);
        let t = b.build();
        assert_eq!(t.node_count(), 3);
        for d in 0..3 {
            assert_eq!(
                t.os_node_of(NodeId(d)),
                d,
                "identity map on a dense platform"
            );
        }
    }

    #[test]
    fn preferred_node_at_is_the_pure_form_of_preferred_node() {
        let mut b = TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0)
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 1)
            .set_cpu(3, 1, 1);
        let t = b.build();
        // For every policy and counter value, the pure form agrees with what the mutating
        // form returns for that same counter value (the router relies on this).
        for il in 0u32..6 {
            for (pol, cpu) in [
                (NumaPolicy::Local, 3u32),
                (NumaPolicy::Bind(NodeId(1)), 0),
                (NumaPolicy::Interleave, 0),
                (NumaPolicy::OsDefault, 0),
            ] {
                let mut counter = il;
                let mutating = t.preferred_node(pol, cpu, &mut counter);
                assert_eq!(t.preferred_node_at(pol, cpu, il), mutating);
                // Only Interleave advances the counter.
                assert_eq!(
                    counter,
                    il + u32::from(matches!(pol, NumaPolicy::Interleave))
                );
            }
        }
    }

    #[test]
    fn build_distance_diagonal_is_local_for_every_slot() {
        // `distance(d, d)` must be local for every dense slot — including the unused ones
        // past node_count() — as single_domain/new establish. Regression: build left the
        // unused-slot diagonal at REMOTE.
        let mut b = TopologyBuilder::new(2);
        b.set_cpu(0, 0, 0).set_cpu(1, 1, 1);
        let t = b.build();
        assert_eq!(t.node_count(), 2);
        for d in 0..MAX_NODES as u32 {
            assert_eq!(
                t.distance(NodeId(d), NodeId(d)),
                DISTANCE_LOCAL,
                "self-distance is local for slot {d}"
            );
        }
        // A real off-diagonal in-range distance still projects correctly (default remote).
        assert_eq!(t.distance(NodeId(0), NodeId(1)), DISTANCE_REMOTE);
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

    /// A two-node topology helper.
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
    fn rebalancer_moves_free_memory_to_a_stranded_node() {
        // Node 0 holds free memory; node 1 has unmet demand and none free. The §15.4
        // "never permanently stranded" invariant ⇒ a move 0 → 1 is planned.
        let t = two_node();
        let nodes = [
            NodePressure {
                free_bytes: 4 << 20,
                demand_bytes: 0,
            },
            NodePressure {
                free_bytes: 0,
                demand_bytes: 2 << 20,
            },
        ];
        let m = Rebalancer::plan(&nodes, &t).expect("a stranded node ⇒ a move");
        assert_eq!(m.src, NodeId(0));
        assert_eq!(m.dst, NodeId(1));
        assert_eq!(
            m.bytes,
            2 << 20,
            "move the recipient's net need (donor has plenty)"
        );
        assert_eq!(m.tier, RebalanceTier::BackendReallocTargetNode);
    }

    #[test]
    fn rebalancer_uses_remote_reuse_under_acute_pressure() {
        // The recipient needs more than the donor can give ⇒ remote reuse (§15.4 tier 5).
        let t = two_node();
        let nodes = [
            NodePressure {
                free_bytes: 1 << 20,
                demand_bytes: 0,
            },
            NodePressure {
                free_bytes: 0,
                demand_bytes: 8 << 20,
            },
        ];
        let m = Rebalancer::plan(&nodes, &t).unwrap();
        assert_eq!(m.bytes, 1 << 20, "bounded by the donor's surplus");
        assert_eq!(m.tier, RebalanceTier::RemoteReuse);
    }

    #[test]
    fn rebalancer_plans_nothing_when_demand_is_locally_satisfiable() {
        // Every node can satisfy its own demand ⇒ no stranding ⇒ no move.
        let t = two_node();
        let nodes = [
            NodePressure {
                free_bytes: 4 << 20,
                demand_bytes: 1 << 20,
            },
            NodePressure {
                free_bytes: 4 << 20,
                demand_bytes: 2 << 20,
            },
        ];
        assert_eq!(Rebalancer::plan(&nodes, &t), None);
        // A single domain never rebalances across nodes.
        assert_eq!(
            Rebalancer::plan(
                &[NodePressure {
                    free_bytes: 0,
                    demand_bytes: 9
                }],
                &Topology::single_domain(4)
            ),
            None
        );
    }

    #[test]
    fn rebalancer_prefers_the_nearest_donor() {
        // Three nodes: node 2 is pressured; node 1 is nearer than node 0.
        let mut b = TopologyBuilder::new(3);
        b.set_cpu(0, 0, 0).set_cpu(1, 1, 1).set_cpu(2, 2, 2);
        b.set_distance(0, 2, 30).set_distance(1, 2, 15); // node 1 closer to node 2
        let t = b.build();
        let nodes = [
            NodePressure {
                free_bytes: 8 << 20,
                demand_bytes: 0,
            }, // node 0: far donor
            NodePressure {
                free_bytes: 8 << 20,
                demand_bytes: 0,
            }, // node 1: near donor
            NodePressure {
                free_bytes: 0,
                demand_bytes: 4 << 20,
            }, // node 2: pressured
        ];
        let m = Rebalancer::plan(&nodes, &t).unwrap();
        assert_eq!(m.src, NodeId(1), "the nearer donor is chosen (§15.4)");
        assert_eq!(m.dst, NodeId(2));
    }

    #[test]
    fn node_pressure_need_and_surplus_are_complementary() {
        // The two derived quantities are exact complements: at most one is nonzero, and
        // each is the saturating difference in its direction (the rebalancer's algebra).
        for (free, demand) in [(0u64, 0u64), (10, 0), (0, 10), (10, 4), (4, 10), (7, 7)] {
            let p = NodePressure {
                free_bytes: free,
                demand_bytes: demand,
            };
            assert_eq!(p.unmet_need(), demand.saturating_sub(free));
            assert_eq!(p.movable_surplus(), free.saturating_sub(demand));
            // Never both positive — a node is a recipient xor a donor (or neither).
            assert!(p.unmet_need() == 0 || p.movable_surplus() == 0);
        }
    }

    #[test]
    fn rebalance_move_apply_relieves_demand_without_stranding_the_donor() {
        // The canonical move semantics: the donor's free drops by `bytes` and the
        // recipient's demand drops by `bytes`, leaving the donor still self-satisfiable.
        let t = two_node();
        let mut nodes = [
            NodePressure {
                free_bytes: 4 << 20,
                demand_bytes: 1 << 20,
            },
            NodePressure {
                free_bytes: 0,
                demand_bytes: 5 << 20,
            },
        ];
        let m = Rebalancer::plan(&nodes, &t).unwrap();
        assert!(m.apply(&mut nodes), "endpoints in range ⇒ applied");
        assert_eq!(nodes[0].free_bytes, (4 << 20) - m.bytes);
        assert_eq!(nodes[1].demand_bytes, (5 << 20) - m.bytes);
        assert_eq!(nodes[0].unmet_need(), 0, "donor still satisfies itself");
        // An out-of-range endpoint is ignored (total, never panics or underflows).
        let bad = RebalanceMove {
            src: NodeId(9),
            dst: NodeId(0),
            bytes: 1,
            tier: RebalanceTier::RemoteReuse,
        };
        let before = nodes;
        assert!(!bad.apply(&mut nodes), "out-of-range endpoint ⇒ no-op");
        assert_eq!(nodes, before, "no mutation on an out-of-range move");
    }

    #[test]
    fn rebalancer_never_strands_the_donor() {
        // A donor that holds free memory **and** its own demand must give away only its
        // *surplus* (free − demand), never the memory it needs for itself — otherwise the
        // move would create the very stranding §15.4 forbids. Node 0 has 4 MiB free but
        // 1 MiB of its own demand (surplus 3 MiB); node 1 needs 5 MiB.
        let t = two_node();
        let nodes = [
            NodePressure {
                free_bytes: 4 << 20,
                demand_bytes: 1 << 20, // donor keeps this for itself
            },
            NodePressure {
                free_bytes: 0,
                demand_bytes: 5 << 20,
            },
        ];
        let m = Rebalancer::plan(&nodes, &t).expect("a stranded node ⇒ a move");
        assert_eq!(m.src, NodeId(0));
        assert_eq!(m.dst, NodeId(1));
        assert_eq!(
            m.bytes,
            3 << 20,
            "only the 3 MiB surplus moves — the donor's own 1 MiB demand is preserved"
        );
        assert_eq!(
            m.tier,
            RebalanceTier::RemoteReuse,
            "need 5 MiB > surplus 3 MiB"
        );
        // The move leaves the donor able to satisfy itself: free − bytes == its demand.
        assert_eq!(nodes[0].free_bytes - m.bytes, nodes[0].demand_bytes);
    }

    #[test]
    fn rebalancer_prefers_larger_surplus_among_equidistant_donors() {
        // Two equidistant donors (default remote distance) and one starved recipient: the
        // donor with the *larger* surplus is chosen, so a single move covers more (§15.4).
        let mut b = TopologyBuilder::new(3);
        b.set_cpu(0, 0, 0).set_cpu(1, 1, 1).set_cpu(2, 2, 2);
        let t = b.build(); // node 0 and node 1 are equidistant (default) from node 2
        let nodes = [
            NodePressure {
                free_bytes: 2 << 20,
                demand_bytes: 0,
            }, // node 0: small surplus
            NodePressure {
                free_bytes: 5 << 20,
                demand_bytes: 0,
            }, // node 1: larger surplus
            NodePressure {
                free_bytes: 0,
                demand_bytes: 10 << 20,
            }, // node 2: starved
        ];
        assert_eq!(
            t.distance(NodeId(0), NodeId(2)),
            t.distance(NodeId(1), NodeId(2))
        );
        let m = Rebalancer::plan(&nodes, &t).unwrap();
        assert_eq!(
            m.src,
            NodeId(1),
            "the larger-surplus equidistant donor wins"
        );
        assert_eq!(m.bytes, 5 << 20, "its full surplus moves");
    }

    #[test]
    fn rebalancer_plans_nothing_when_no_node_has_surplus() {
        // Both nodes are under-supplied (free < demand): neither has surplus to give.
        // Moving one's small free to the other would only worsen the donor (churn without
        // progress), so the rebalancer plans **nothing** — surplus, not raw free, gates it.
        let t = two_node();
        let nodes = [
            NodePressure {
                free_bytes: 2 << 20,
                demand_bytes: 5 << 20,
            },
            NodePressure {
                free_bytes: 1 << 20,
                demand_bytes: 4 << 20,
            },
        ];
        assert_eq!(Rebalancer::plan(&nodes, &t), None);
    }

    #[test]
    fn detect_mismatch_flags_a_moved_cpu_and_ignores_single_domain() {
        let t = two_node();
        // CPU 2 is modeled on node 1; an observation of node 0 is a mismatch (W13-4).
        assert!(t.detect_mismatch(2, NodeId(0)));
        assert!(!t.detect_mismatch(2, NodeId(1)));
        // A single-domain snapshot claims nothing, so never reports a mismatch.
        assert!(!Topology::single_domain(4).detect_mismatch(0, NodeId(3)));
    }
}

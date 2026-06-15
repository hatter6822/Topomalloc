// SPDX-License-Identifier: MIT
//! Statistics snapshot and JSON rendering (§31, Appendix D).
//!
//! The cardinal stats question is "where is the memory?" (§31.1). M0 ships the
//! snapshot shape and the additive JSON renderer, with `topomalloc_version`
//! wired from the crate version (W0-13) so the reported version and the ABI
//! series can never drift. Real counters are populated as each subsystem lands
//! (plan 07); fields are only ever *added* within a release series (§35.3).
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};

use topo_core::VERSION;

/// An epoch-consistent statistics snapshot (§31.2). All counters are unsigned,
/// so the "stats nonnegative" property (§34.3) holds by construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Monotonic epoch the snapshot was taken at.
    pub epoch: u64,
    /// Active profile name (e.g. `"performance"`).
    pub profile: Profile,
    /// Bytes currently live in the application.
    pub live_bytes: u64,
    /// Cumulative bytes ever allocated.
    pub allocated_bytes_total: u64,
    /// Cumulative bytes ever freed.
    pub freed_bytes_total: u64,
    /// Bytes held in per-CPU caches.
    pub per_cpu_bytes: u64,
    /// Bytes held in thread caches.
    pub thread_cache_bytes: u64,
    /// Bytes held in transfer caches.
    pub transfer_bytes: u64,
    /// Bytes held in central free lists.
    pub central_free_bytes: u64,
    /// Bytes of allocator metadata.
    pub metadata_bytes: u64,

    // --- back-end physical state (§20.1/§21.2, plan 04 W4-3a) ---------------
    /// Free, physically-backed bytes that may hold old data (§20.1 *dirty*).
    pub dirty_bytes: u64,
    /// Free, lazily-purged/scrubbed bytes (§20.1 *muzzy*).
    pub muzzy_bytes: u64,
    /// Free bytes returned to the OS, needing recommit (§20.1 *released*).
    pub released_bytes: u64,
    /// All free back-end bytes (§21.2 `pageheap_free_bytes`): reserved + dirty +
    /// muzzy + released.
    pub pageheap_free_bytes: u64,
    /// Total virtual bytes the back-end manages (§21.2 `virtual_bytes`).
    pub virtual_bytes: u64,

    // --- arenas (§22/§36.4, plan 06 W9) -------------------------------------
    /// Arenas currently registered, including the always-present default.
    pub live_arenas: u64,
    /// Cumulative NUMA binding failures across all arenas (§15.5).
    pub numa_bind_failures: u64,
    /// Cumulative custom-backing (extent-hook) failures across all hooked arenas,
    /// per kind (§23, plan 06 W10).
    pub hook_failures: topo_core::HookFailureStats,

    // --- hugepage coverage (§19.7, plan 04 W11-5) ---------------------------
    /// The §19.7 hugepage coverage metrics, summed over the hugepage backend(s).
    /// All zero unless a hugepage backend is in use; populated via
    /// [`record_huge`](Self::record_huge).
    pub hugepage: topo_core::HugeStats,

    // --- release controller (§20.3/§21, plan 04 W12) ------------------------
    /// The release-controller / background-purge running counters (pressure mode,
    /// backlog, planned bytes, demand reserve). All zero unless a release controller
    /// is in use; populated via [`record_release`](Self::record_release).
    pub release: topo_core::ReleaseStats,

    // --- topology (§15, plan 04 W13) ----------------------------------------
    /// Discovered NUMA node count (§15.2); `1` for the single-domain fallback.
    /// Populated via [`record_topology`](Self::record_topology).
    pub numa_nodes: u32,
    /// Discovered LLC-domain count (§15.2); `1` for the single-domain fallback.
    pub llc_domains: u32,
    /// Live NUMA router: the cumulative count of per-node `mbind` failures (§15.5
    /// "NUMA binding failures MUST be visible in stats"). Populated via
    /// [`record_node_router`](Self::record_node_router); `0` when no router is wired.
    pub numa_router_bind_failures: u64,
    /// Live NUMA router: cumulative §15.4 rebalancer moves executed.
    pub numa_rebalance_moves: u64,
    /// Live NUMA router: cumulative bytes returned to the OS by rebalancer moves.
    pub numa_rebalance_released_bytes: u64,
    /// Live NUMA router: cumulative spillover allocations (served off the preferred node
    /// because it was full — §15.4 remote reuse at allocation time).
    pub numa_spillovers: u64,
}

/// The active build/runtime profile (§30.1). Profiles are features, not forks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    /// Optimized, the M0 default.
    #[default]
    Performance,
    /// Security-hardened.
    Hardened,
    /// Debug invariant checks enabled.
    Debug,
    /// Deterministic test mode (§30.4).
    DeterministicTest,
    /// RSS-minimizing.
    LowRss,
    /// Hugepage-optimized.
    HugepageOptimized,
}

impl Profile {
    /// The stable string used in stats/control output.
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Performance => "performance",
            Profile::Hardened => "hardened",
            Profile::Debug => "debug",
            Profile::DeterministicTest => "deterministic_test",
            Profile::LowRss => "low_rss",
            Profile::HugepageOptimized => "hugepage_optimized",
        }
    }

    /// The profile selected by the compiled-in Cargo features (§30.1), so stats
    /// and the control plane report the actual build, not a hard-coded default.
    pub fn active() -> Profile {
        match topo_core::active_profile() {
            "hardened" => Profile::Hardened,
            "debug" => Profile::Debug,
            "deterministic_test" => Profile::DeterministicTest,
            "low_rss" => Profile::LowRss,
            "hugepage_optimized" => Profile::HugepageOptimized,
            _ => Profile::Performance,
        }
    }
}

impl Stats {
    /// Record the back-end physical-state byte breakdown (§20.1/§21.2) from the
    /// extent manager's [`StateBytes`](topo_core::StateBytes) — the W4-3a "states
    /// reconcile in stats" path. `dirty`/`muzzy`/`released` map directly;
    /// `pageheap_free_bytes` is all free back-end bytes and `virtual_bytes` the
    /// total the back-end manages. The invariant
    /// `virtual_bytes == live + dirty + muzzy + released + (reserved/active backing)`
    /// is the extent manager's `StateBytes::total()`.
    pub fn record_backend(&mut self, sb: topo_core::StateBytes) {
        self.dirty_bytes = sb.dirty as u64;
        self.muzzy_bytes = sb.muzzy as u64;
        self.released_bytes = sb.released as u64;
        self.pageheap_free_bytes = sb.free() as u64;
        self.virtual_bytes = sb.total() as u64;
    }

    /// Record an [`Allocator`](topo_core::Allocator) snapshot (plan 06 W8 —
    /// the "state exposes stats and reconciles" DoD): the application-side
    /// counters and central-list bytes map directly; the two back-end regions'
    /// §20.1 state breakdowns are summed into the single back-end view
    /// [`record_backend`](Self::record_backend) renders; the pagemap's radix
    /// nodes are the metadata overhead measured so far.
    pub fn record_allocator(&mut self, a: &topo_core::AllocatorStats) {
        self.live_bytes = a.live_bytes;
        self.allocated_bytes_total = a.allocated_bytes_total;
        self.freed_bytes_total = a.freed_bytes_total;
        self.central_free_bytes = a.central_free_bytes;
        self.metadata_bytes = a.pagemap_metadata_bytes;
        self.live_arenas = a.live_arenas;
        self.numa_bind_failures = a.numa_bind_failures;
        self.hook_failures = a.hook_failures;
        let combined = topo_core::StateBytes {
            reserved: a.span_backend.reserved + a.large_backend.reserved,
            active: a.span_backend.active + a.large_backend.active,
            dirty: a.span_backend.dirty + a.large_backend.dirty,
            muzzy: a.span_backend.muzzy + a.large_backend.muzzy,
            released: a.span_backend.released + a.large_backend.released,
        };
        self.record_backend(combined);
    }

    /// Record the §19.7 hugepage coverage metrics (plan 04 W11-5) from a hugepage
    /// backend's [`HugeStats`](topo_core::HugeStats) (its
    /// [`coverage`](topo_core::HugePageBackend::coverage) snapshot). The backend is a
    /// separate component from the central path, so its coverage is recorded
    /// alongside [`record_allocator`](Self::record_allocator); several backends'
    /// coverage sums with [`HugeStats::add`](topo_core::HugeStats::add) before being
    /// recorded. The §19.7 `coverage_ratio` is rendered (computed, not stored) from
    /// the recorded fields.
    pub fn record_huge(&mut self, hs: topo_core::HugeStats) {
        self.hugepage = hs;
    }

    /// Record the release-controller running counters (§20.3/§21, plan 04 W12) from a
    /// [`ReleaseController::stats`](topo_core::ReleaseController::stats) snapshot. The
    /// controller is a host-owned sibling of the allocator (like the hugepage backend),
    /// so its stats are recorded alongside [`record_allocator`](Self::record_allocator).
    pub fn record_release(&mut self, rs: topo_core::ReleaseStats) {
        self.release = rs;
    }

    /// Record the §15.2 topology summary (plan 04 W13) from a discovered
    /// [`Topology`](topo_core::Topology): its NUMA-node and LLC-domain counts (both `1`
    /// for the conservative single-domain fallback). The host records it once at
    /// startup and on a refresh.
    pub fn record_topology(&mut self, t: &topo_core::Topology) {
        self.numa_nodes = t.node_count();
        self.llc_domains = t.llc_count();
    }

    /// Record the live NUMA router's running counters (§15.4/§15.5, plan 04 W13) from a
    /// [`NodeRouterStats`](topo_core::NodeRouterStats) snapshot — the bind-failure count
    /// (the §15.5 visibility requirement), executed rebalancer moves and bytes released,
    /// and spillover allocations. The router is a host-owned sibling like the hugepage
    /// backend, so its stats are composed alongside [`record_topology`](Self::record_topology).
    /// Also pins `numa_nodes` to the router's node count (= the placed-across node count).
    pub fn record_node_router(&mut self, r: topo_core::NodeRouterStats) {
        self.numa_nodes = r.nodes;
        self.numa_router_bind_failures = r.bind_failures;
        self.numa_rebalance_moves = r.rebalance_moves;
        self.numa_rebalance_released_bytes = r.rebalance_released_bytes;
        self.numa_spillovers = r.spillovers;
    }

    /// Render the snapshot as JSON in the Appendix-D shape. The renderer is
    /// additive: new fields may be added in later milestones, never removed or
    /// renamed within a release series (§35.3). Strings here are fixed ASCII
    /// identifiers, so no escaping is required.
    pub fn to_json(&self) -> String {
        // §19.4 bin distribution (W11-4a), rendered as a compact JSON array. Built
        // here rather than inline in the template so it tracks `HugeBin::COUNT`
        // automatically; this is the slow stats surface, so the small per-element
        // allocations are immaterial.
        let mut hp_bins = String::from("[");
        for (i, count) in self.hugepage.bins.iter().enumerate() {
            if i > 0 {
                hp_bins.push(',');
            }
            hp_bins.push_str(&count.to_string());
        }
        hp_bins.push(']');
        format!(
            concat!(
                "{{\n",
                "  \"topomalloc_version\": \"{version}\",\n",
                "  \"epoch\": {epoch},\n",
                "  \"profile\": \"{profile}\",\n",
                "  \"application\": {{\n",
                "    \"live_bytes\": {live},\n",
                "    \"allocated_bytes_total\": {alloc_total},\n",
                "    \"freed_bytes_total\": {freed_total}\n",
                "  }},\n",
                "  \"cache\": {{\n",
                "    \"per_cpu_bytes\": {per_cpu},\n",
                "    \"thread_cache_bytes\": {thread},\n",
                "    \"transfer_bytes\": {transfer}\n",
                "  }},\n",
                "  \"central\": {{\n",
                "    \"free_bytes\": {central}\n",
                "  }},\n",
                "  \"arenas\": {{\n",
                "    \"count\": {live_arenas},\n",
                "    \"numa_bind_failures\": {numa_bind_failures},\n",
                "    \"hook_failures\": {{\n",
                "      \"commit\": {hf_commit},\n",
                "      \"release\": {hf_release},\n",
                "      \"split\": {hf_split},\n",
                "      \"merge\": {hf_merge}\n",
                "    }}\n",
                "  }},\n",
                "  \"backend\": {{\n",
                "    \"dirty_bytes\": {dirty},\n",
                "    \"muzzy_bytes\": {muzzy},\n",
                "    \"released_bytes\": {released},\n",
                "    \"pageheap_free_bytes\": {pageheap},\n",
                "    \"virtual_bytes\": {virtual_b}\n",
                "  }},\n",
                "  \"hugepage\": {{\n",
                "    \"coverage_bytes\": {hp_coverage},\n",
                "    \"live_bytes_on_intact_hugepages\": {hp_intact},\n",
                "    \"live_bytes_on_partial_hugepages\": {hp_partial},\n",
                "    \"empty_backed_bytes\": {hp_empty_backed},\n",
                "    \"empty_released_bytes\": {hp_empty_released},\n",
                "    \"partial_subreleased_bytes\": {hp_subreleased},\n",
                "    \"fragmentation_bytes\": {hp_fragmentation},\n",
                "    \"coverage_ratio_bp\": {hp_ratio_bp},\n",
                "    \"bin_counts\": {hp_bins}\n",
                "  }},\n",
                "  \"release\": {{\n",
                "    \"pressure_mode\": \"{rel_mode}\",\n",
                "    \"backlog_bytes\": {rel_backlog},\n",
                "    \"demand_reserve_bytes\": {rel_reserve},\n",
                "    \"planned_bytes_total\": {rel_planned},\n",
                "    \"ticks\": {rel_ticks},\n",
                "    \"active_ticks\": {rel_active},\n",
                "    \"alloc_rate_bps\": {rel_alloc_rate},\n",
                "    \"free_rate_bps\": {rel_free_rate}\n",
                "  }},\n",
                "  \"topology\": {{\n",
                "    \"numa_nodes\": {numa_nodes},\n",
                "    \"llc_domains\": {llc_domains},\n",
                "    \"router_bind_failures\": {numa_router_bind_failures},\n",
                "    \"rebalance_moves\": {numa_rebalance_moves},\n",
                "    \"rebalance_released_bytes\": {numa_rebalance_released_bytes},\n",
                "    \"spillovers\": {numa_spillovers}\n",
                "  }},\n",
                "  \"metadata\": {{\n",
                "    \"bytes\": {metadata}\n",
                "  }}\n",
                "}}"
            ),
            version = VERSION,
            epoch = self.epoch,
            profile = self.profile.as_str(),
            live = self.live_bytes,
            alloc_total = self.allocated_bytes_total,
            freed_total = self.freed_bytes_total,
            per_cpu = self.per_cpu_bytes,
            thread = self.thread_cache_bytes,
            transfer = self.transfer_bytes,
            central = self.central_free_bytes,
            live_arenas = self.live_arenas,
            numa_bind_failures = self.numa_bind_failures,
            hf_commit = self.hook_failures.commit,
            hf_release = self.hook_failures.release,
            hf_split = self.hook_failures.split,
            hf_merge = self.hook_failures.merge,
            dirty = self.dirty_bytes,
            muzzy = self.muzzy_bytes,
            released = self.released_bytes,
            pageheap = self.pageheap_free_bytes,
            virtual_b = self.virtual_bytes,
            hp_coverage = self.hugepage.coverage_bytes,
            hp_intact = self.hugepage.live_bytes_on_intact,
            hp_partial = self.hugepage.live_bytes_on_partial,
            hp_empty_backed = self.hugepage.empty_backed_bytes,
            hp_empty_released = self.hugepage.empty_released_bytes,
            hp_subreleased = self.hugepage.partial_subreleased_bytes,
            hp_fragmentation = self.hugepage.fragmentation_bytes,
            hp_ratio_bp = self.hugepage.coverage_ratio_bp(),
            hp_bins = hp_bins,
            rel_mode = self.release.mode.as_str(),
            rel_backlog = self.release.backlog_bytes,
            rel_reserve = self.release.demand_reserve_bytes,
            rel_planned = self.release.planned_bytes_total,
            rel_ticks = self.release.ticks,
            rel_active = self.release.active_ticks,
            rel_alloc_rate = self.release.alloc_rate_bps,
            rel_free_rate = self.release.free_rate_bps,
            numa_nodes = self.numa_nodes,
            llc_domains = self.llc_domains,
            numa_router_bind_failures = self.numa_router_bind_failures,
            numa_rebalance_moves = self.numa_rebalance_moves,
            numa_rebalance_released_bytes = self.numa_rebalance_released_bytes,
            numa_spillovers = self.numa_spillovers,
            metadata = self.metadata_bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_wellformed_and_carries_version() {
        let s = Stats {
            epoch: 128931,
            profile: Profile::Performance,
            live_bytes: 3493224448,
            allocated_bytes_total: 991234883584,
            freed_bytes_total: 987741659136,
            per_cpu_bytes: 268435456,
            ..Stats::default()
        };
        let json = s.to_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["topomalloc_version"], VERSION);
        assert_eq!(v["epoch"], 128931);
        assert_eq!(v["profile"], "performance");
        assert_eq!(v["application"]["live_bytes"], 3493224448u64);
        assert_eq!(v["cache"]["per_cpu_bytes"], 268435456u64);
    }

    #[test]
    fn backend_state_reconciles_into_stats() {
        // W4-3a: the extent manager's StateBytes feeds the §21.2 backend fields, and
        // the JSON carries them. The reconciliation identities hold by construction.
        let sb = topo_core::StateBytes {
            reserved: 4096,
            active: 8192,
            dirty: 2048,
            muzzy: 1024,
            released: 512,
        };
        let mut s = Stats::default();
        s.record_backend(sb);
        assert_eq!(s.dirty_bytes, 2048);
        assert_eq!(s.muzzy_bytes, 1024);
        assert_eq!(s.released_bytes, 512);
        assert_eq!(s.pageheap_free_bytes, (4096 + 2048 + 1024 + 512) as u64);
        assert_eq!(s.virtual_bytes, (4096 + 8192 + 2048 + 1024 + 512) as u64);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["backend"]["dirty_bytes"], 2048);
        assert_eq!(v["backend"]["released_bytes"], 512);
        assert_eq!(v["backend"]["virtual_bytes"], 15872);
    }

    #[test]
    fn hugepage_coverage_reconciles_into_stats_and_json() {
        // W11-5: the §19.7 coverage metrics flow into the stats snapshot and the JSON,
        // and the coverage ratio is computed from the recorded fields.
        let hs = topo_core::HugeStats {
            coverage_bytes: 4 * 2 * 1024 * 1024, // 4 hugepages
            live_bytes_on_intact: 3 * 1024 * 1024,
            live_bytes_on_partial: 1024 * 1024,
            empty_backed_bytes: 2 * 1024 * 1024,
            empty_released_bytes: 512 * 1024,
            partial_subreleased_bytes: 256 * 1024,
            fragmentation_bytes: 128 * 1024,
            live_total_bytes: 4 * 1024 * 1024,
            // Two empty-backed + one full + one partial-subreleased hugepage: a
            // distribution that must reconcile to the 4-hugepage coverage above.
            bins: [2, 0, 0, 0, 0, 1, 1, 0, 0],
        };
        let mut s = Stats::default();
        s.record_huge(hs);
        assert_eq!(s.hugepage.coverage_bytes, 4 * 2 * 1024 * 1024);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["hugepage"]["coverage_bytes"], 4u64 * 2 * 1024 * 1024);
        assert_eq!(
            v["hugepage"]["live_bytes_on_intact_hugepages"],
            3u64 * 1024 * 1024
        );
        assert_eq!(
            v["hugepage"]["live_bytes_on_partial_hugepages"],
            1024u64 * 1024
        );
        assert_eq!(v["hugepage"]["empty_backed_bytes"], 2u64 * 1024 * 1024);
        assert_eq!(v["hugepage"]["empty_released_bytes"], 512u64 * 1024);
        assert_eq!(v["hugepage"]["partial_subreleased_bytes"], 256u64 * 1024);
        assert_eq!(v["hugepage"]["fragmentation_bytes"], 128u64 * 1024);
        // ratio = intact / total = 3 MiB / 4 MiB = 7500 bp.
        assert_eq!(v["hugepage"]["coverage_ratio_bp"], 7500);
        // The §19.4 bin distribution renders as a compact array in HugeBin order and
        // sums to the touched-hugepage count (4).
        assert_eq!(
            v["hugepage"]["bin_counts"],
            serde_json::json!([2, 0, 0, 0, 0, 1, 1, 0, 0])
        );
        let bin_sum: u64 = hs.bins.iter().map(|&c| c as u64).sum();
        assert_eq!(bin_sum, hs.coverage_bytes / (2 * 1024 * 1024));
    }

    #[test]
    fn release_controller_stats_reconcile_into_stats_and_json() {
        // W12: the release-controller running counters flow into the snapshot and the
        // JSON `release` block, with the pressure mode rendered as its stable string.
        let rs = topo_core::ReleaseStats {
            mode: topo_core::PressureMode::Hard,
            backlog_bytes: 4096,
            planned_bytes_total: 1_048_576,
            ticks: 42,
            demand_reserve_bytes: 65536,
            active_ticks: 7,
            alloc_rate_bps: 12_345,
            free_rate_bps: 6_789,
        };
        let mut s = Stats::default();
        s.record_release(rs);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["release"]["pressure_mode"], "hard");
        assert_eq!(v["release"]["backlog_bytes"], 4096);
        assert_eq!(v["release"]["demand_reserve_bytes"], 65536);
        assert_eq!(v["release"]["planned_bytes_total"], 1_048_576);
        assert_eq!(v["release"]["ticks"], 42);
        assert_eq!(v["release"]["active_ticks"], 7);
        assert_eq!(v["release"]["alloc_rate_bps"], 12_345);
        assert_eq!(v["release"]["free_rate_bps"], 6_789);
        // The default snapshot renders the no-pressure normal mode.
        let d: serde_json::Value = serde_json::from_str(&Stats::default().to_json()).unwrap();
        assert_eq!(d["release"]["pressure_mode"], "normal");
    }

    #[test]
    fn topology_summary_reconciles_into_stats_and_json() {
        // W13: the discovered §15.2 node/LLC counts flow into the snapshot + JSON.
        let mut b = topo_core::TopologyBuilder::new(4);
        b.set_cpu(0, 0, 0)
            .set_cpu(1, 0, 0)
            .set_cpu(2, 1, 1)
            .set_cpu(3, 1, 1);
        let t = b.build();
        let mut s = Stats::default();
        s.record_topology(&t);
        assert_eq!(s.numa_nodes, 2);
        assert_eq!(s.llc_domains, 2);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["topology"]["numa_nodes"], 2);
        assert_eq!(v["topology"]["llc_domains"], 2);
        // The default snapshot reports the conservative single domain.
        let d: serde_json::Value = serde_json::from_str(&Stats::default().to_json()).unwrap();
        assert_eq!(d["topology"]["numa_nodes"], 0); // unrecorded ⇒ 0 (no topology yet)
    }

    #[test]
    fn node_router_counters_reconcile_into_stats_and_json() {
        // W13: the live router's §15.4/§15.5 counters flow into the snapshot + JSON, and
        // its node count overrides the discovered count (it is what we place across).
        let mut s = Stats::default();
        s.record_node_router(topo_core::NodeRouterStats {
            nodes: 2,
            bind_failures: 3,
            rebalance_moves: 5,
            rebalance_released_bytes: 4096,
            spillovers: 9,
        });
        assert_eq!(s.numa_nodes, 2);
        assert_eq!(s.numa_router_bind_failures, 3);
        assert_eq!(s.numa_rebalance_moves, 5);
        assert_eq!(s.numa_rebalance_released_bytes, 4096);
        assert_eq!(s.numa_spillovers, 9);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["topology"]["numa_nodes"], 2);
        assert_eq!(v["topology"]["router_bind_failures"], 3);
        assert_eq!(v["topology"]["rebalance_moves"], 5);
        assert_eq!(v["topology"]["rebalance_released_bytes"], 4096);
        assert_eq!(v["topology"]["spillovers"], 9);
        // Default (no router wired) ⇒ all zero.
        let d: serde_json::Value = serde_json::from_str(&Stats::default().to_json()).unwrap();
        assert_eq!(d["topology"]["router_bind_failures"], 0);
        assert_eq!(d["topology"]["spillovers"], 0);
    }

    #[test]
    fn default_snapshot_is_all_zero_performance() {
        let s = Stats::default();
        assert_eq!(s.profile, Profile::Performance);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).unwrap();
        assert_eq!(v["application"]["live_bytes"], 0);
    }

    #[test]
    fn record_allocator_maps_and_sums_the_regions() {
        let snap = topo_core::AllocatorStats {
            live_bytes: 1000,
            allocated_bytes_total: 1500,
            freed_bytes_total: 500,
            central_free_bytes: 256,
            span_backend: topo_core::StateBytes {
                reserved: 10,
                active: 20,
                dirty: 30,
                muzzy: 40,
                released: 50,
            },
            large_backend: topo_core::StateBytes {
                reserved: 1,
                active: 2,
                dirty: 3,
                muzzy: 4,
                released: 5,
            },
            pagemap_metadata_bytes: 8192,
            live_spans: 2,
            live_large: 1,
            live_arenas: 3,
            numa_bind_failures: 7,
            hook_failures: topo_core::HookFailureStats {
                commit: 12,
                release: 13,
                split: 14,
                merge: 15,
            },
        };
        let mut s = Stats::default();
        s.record_allocator(&snap);
        // Arena summary (plan 06 W9) maps through and renders.
        assert_eq!(s.live_arenas, 3);
        assert_eq!(s.numa_bind_failures, 7);
        // Hook-failure counts (plan 06 W10) map through and render in the JSON.
        assert_eq!(s.hook_failures.commit, 12);
        assert_eq!(s.hook_failures.merge, 15);
        assert_eq!(s.live_bytes, 1000);
        assert_eq!(s.allocated_bytes_total, 1500);
        assert_eq!(s.freed_bytes_total, 500);
        assert_eq!(s.central_free_bytes, 256);
        assert_eq!(s.metadata_bytes, 8192);
        // The two regions sum into the single back-end view (§20.1/§21.2).
        assert_eq!(s.dirty_bytes, 33);
        assert_eq!(s.muzzy_bytes, 44);
        assert_eq!(s.released_bytes, 55);
        assert_eq!(
            s.virtual_bytes,
            (10 + 20 + 30 + 40 + 50) + (1 + 2 + 3 + 4 + 5)
        );
        // The application identity §8.6: live == allocated - freed.
        assert_eq!(s.live_bytes, s.allocated_bytes_total - s.freed_bytes_total);
        // JSON renders the recorded values (additive schema, §35.3).
        let json = s.to_json();
        assert!(json.contains("\"live_bytes\": 1000"));
        assert!(json.contains("\"allocated_bytes_total\": 1500"));
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["arenas"]["count"], 3);
        assert_eq!(v["arenas"]["numa_bind_failures"], 7);
        assert_eq!(v["arenas"]["hook_failures"]["commit"], 12);
        assert_eq!(v["arenas"]["hook_failures"]["release"], 13);
        assert_eq!(v["arenas"]["hook_failures"]["split"], 14);
        assert_eq!(v["arenas"]["hook_failures"]["merge"], 15);
    }
}

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
    /// The high-water mark of `live_bytes` since the last `RESET_PEAKS` (§31.2/§31.3
    /// sampled peak heap, W17-2): the maximum live bytes the allocator has charged.
    /// A true high-water mark maintained on the allocation path, not a poll-time sample.
    pub peak_live_bytes: u64,
    /// Cumulative bytes ever allocated.
    pub allocated_bytes_total: u64,
    /// Cumulative bytes ever freed.
    pub freed_bytes_total: u64,
    /// The process resident-set size in bytes (§31.6), read from the OS (`/proc/self/statm`
    /// on Linux) at snapshot time; `0` when unavailable. This is the *actual* RSS the
    /// `explain` endpoint attributes (W17-5) — distinct from `virtual_bytes` (managed VM).
    pub rss_bytes: u64,
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
    /// In-use backing bytes — the §20.1 *active* state, i.e. virtual memory carved
    /// into live spans / large mappings. The application classes (`live_bytes`,
    /// `central_free_bytes`, the caches, `quarantine_bytes`) partition this modulo
    /// slab-packing residual; the §8.6 reconciliation identity is
    /// `virtual_bytes == active_bytes + pageheap_free_bytes` (W17-1a).
    pub active_bytes: u64,
    /// Virtual address range retained for future use but **not** physically backed
    /// (§20.1 *Retained*; the extent manager's `reserved`). §31.1 `backend.retained_bytes`
    /// (W17-1a) — distinct from `released_bytes` (advised-away, needs recommit).
    pub retained_bytes: u64,
    /// Free, physically-backed bytes that may hold old data (§20.1 *dirty*).
    pub dirty_bytes: u64,
    /// Free, lazily-purged/scrubbed bytes (§20.1 *muzzy*).
    pub muzzy_bytes: u64,
    /// Free bytes advised away / decommitted, needing recommit (§20.1 *Released*).
    pub released_bytes: u64,
    /// All free back-end bytes (§21.2 `pageheap_free_bytes`): retained + dirty +
    /// muzzy + released.
    pub pageheap_free_bytes: u64,
    /// Total virtual bytes the back-end manages (§21.2 `virtual_bytes`).
    pub virtual_bytes: u64,

    // --- quarantine (§17.5/§31.1, plan 08 W18) ------------------------------
    /// Bytes held in quarantine (delayed-reuse freed memory, §17.5). §31.1
    /// `quarantine.bytes`. Present and non-negative (W17-1a); `0` until the
    /// hardened/debug quarantine byte accounting lands (plan 08 W18) — TopoMalloc
    /// keeps quarantine *flags* per span today, not a live byte total.
    pub quarantine_bytes: u64,

    // --- arenas (§22/§36.4, plan 06 W9) -------------------------------------
    /// Arenas currently registered, including the always-present default.
    pub live_arenas: u64,
    /// Cumulative arenas destroyed over the engine's lifetime (§31.1
    /// `arena.destroyed_count`, plan 07 W17-1a). Monotonic; a destroyed id may be
    /// recycled (§36.13), so this is an event count, not `created − live`.
    pub arenas_destroyed: u64,
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

    // --- placement / lifetime profiling (§24/§31.3, plan 07 W14 + W17-3) -----
    /// The allocation-site learning policy's running counters (sites tracked, confident
    /// sites, sampled alloc/free/censored counts, evictions, sampled live bytes). All zero
    /// unless heap/lifetime sampling has been enabled; populated via
    /// [`record_placement`](Self::record_placement). These are *profiling* estimates, not a
    /// managed-memory byte class, so they do not enter the §8.6 VM reconciliation.
    pub placement: topo_core::PlacementStats,
    /// The §31.5 **sampled internal fragmentation** for *small* objects: the sum of
    /// `usable − requested` over the live sampled small objects (W17-4). A profiling estimate
    /// (`0` unless sampling is on), so — like `placement` — it sits outside the §8.6 byte
    /// reconciliation. Set via [`record_fragmentation`](Self::record_fragmentation).
    pub sampled_internal_fragmentation_bytes: u64,
    /// The §31.5 **exact internal fragmentation** for *medium/large* allocations: the sum of
    /// `usable − requested` over every live extent-backed allocation (W17-4). Tracked exactly
    /// and always-on (the large path records its requested size), since page-rounding is where
    /// internal waste dominates and is cheap to track there. `0` when no medium/large
    /// allocations are live. Set via [`record_fragmentation`](Self::record_fragmentation).
    pub exact_internal_fragmentation_bytes: u64,
}

/// Stats selection flags (§31.2) — which views a snapshot / JSON / print renders. The
/// always-present summary blocks answer "where is the memory?" (§31.1); the `BY_*` flags
/// add per-entity detail; `CONSISTENT_SNAPSHOT` / `RESET_PEAKS` are read modes. A `u64`
/// newtype (no external `bitflags` dep — the kernel allows only `libc`/dev-only crates),
/// laid out so the C ABI (`TOPOMALLOC_STATS_*`) and Rust agree bit-for-bit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct StatsFlags(pub u64);

impl StatsFlags {
    /// The summary view (`0`) — always rendered; the other flags are additive.
    pub const SUMMARY: StatsFlags = StatsFlags(0);
    /// Per-arena breakdown (§22): one line per live arena (id, label, used, reserved).
    pub const BY_ARENA: StatsFlags = StatsFlags(1 << 0);
    /// Per-size-class breakdown: central-free bytes per class.
    pub const BY_SIZE_CLASS: StatsFlags = StatsFlags(1 << 1);
    /// Per-CPU cache breakdown (front-end caches, M2 — empty until then).
    pub const BY_CPU: StatsFlags = StatsFlags(1 << 2);
    /// Per-NUMA-node breakdown (§15): the topology / router counters block.
    pub const BY_NUMA: StatsFlags = StatsFlags(1 << 3);
    /// Per-hugepage breakdown (§19.4): the occupancy-bin distribution.
    pub const BY_HUGEPAGE: StatsFlags = StatsFlags(1 << 4);
    /// Consistent-snapshot read mode (§8.6): read cumulative counters skew-free.
    pub const CONSISTENT_SNAPSHOT: StatsFlags = StatsFlags(1 << 5);
    /// Reset peak gauges after reading (§31.2 `RESET_PEAKS`).
    pub const RESET_PEAKS: StatsFlags = StatsFlags(1 << 6);

    /// Every defined flag bit OR-ed together — the validity mask (reserved bits must be 0).
    pub const ALL: StatsFlags = StatsFlags(
        Self::BY_ARENA.0
            | Self::BY_SIZE_CLASS.0
            | Self::BY_CPU.0
            | Self::BY_NUMA.0
            | Self::BY_HUGEPAGE.0
            | Self::CONSISTENT_SNAPSHOT.0
            | Self::RESET_PEAKS.0,
    );

    /// The raw bits.
    #[inline]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether **every** bit in `other` is set (so `SUMMARY`/`0` is always contained).
    #[inline]
    pub const fn contains(self, other: StatsFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit outside [`ALL`](Self::ALL) is set — an invalid flag word (§10.4).
    #[inline]
    pub const fn has_unknown_bits(self) -> bool {
        self.0 & !Self::ALL.0 != 0
    }
}

impl core::ops::BitOr for StatsFlags {
    type Output = StatsFlags;
    #[inline]
    fn bitor(self, rhs: StatsFlags) -> StatsFlags {
        StatsFlags(self.0 | rhs.0)
    }
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
        self.active_bytes = sb.active as u64;
        // §20.1 *Retained* (the extent manager's `reserved`): virtual range kept for reuse
        // but not physically backed — §31.1 `backend.retained_bytes`, distinct from
        // `released_bytes` (advised-away). The §8.6 identities then hold by construction:
        // `pageheap_free == retained + dirty + muzzy + released` and
        // `virtual == active + pageheap_free`.
        self.retained_bytes = sb.reserved as u64;
        self.dirty_bytes = sb.dirty as u64;
        self.muzzy_bytes = sb.muzzy as u64;
        self.released_bytes = sb.released as u64;
        self.pageheap_free_bytes = sb.free() as u64;
        self.virtual_bytes = sb.total() as u64;
    }

    /// Record the §31.5 **sampled** small-object internal-fragmentation estimate (W17-4):
    /// `Σ(usable − requested)` over the live sampled small objects. A profiling estimate
    /// (`0` unless heap sampling is on), recorded alongside
    /// [`record_placement`](Self::record_placement). The medium/large exact figure arrives
    /// through [`record_allocator`](Self::record_allocator).
    pub fn record_fragmentation(&mut self, sampled_internal_bytes: u64) {
        self.sampled_internal_fragmentation_bytes = sampled_internal_bytes;
    }

    /// Record the process resident-set size (§31.6, W17-5) read from the OS at snapshot
    /// time. `0` leaves it unknown (the `explain` endpoint then omits the RSS figure).
    pub fn record_rss(&mut self, rss_bytes: u64) {
        self.rss_bytes = rss_bytes;
    }

    /// Record an [`Allocator`](topo_core::Allocator) snapshot (plan 06 W8 —
    /// the "state exposes stats and reconciles" DoD): the application-side
    /// counters and central-list bytes map directly; the two back-end regions'
    /// §20.1 state breakdowns are summed into the single back-end view
    /// [`record_backend`](Self::record_backend) renders; the pagemap's radix
    /// nodes are the metadata overhead measured so far.
    pub fn record_allocator(&mut self, a: &topo_core::AllocatorStats) {
        self.live_bytes = a.live_bytes;
        self.peak_live_bytes = a.peak_live_bytes;
        self.allocated_bytes_total = a.allocated_bytes_total;
        self.freed_bytes_total = a.freed_bytes_total;
        self.central_free_bytes = a.central_free_bytes;
        self.metadata_bytes = a.pagemap_metadata_bytes;
        self.exact_internal_fragmentation_bytes = a.live_internal_fragmentation_bytes;
        self.live_arenas = a.live_arenas;
        self.arenas_destroyed = a.arenas_destroyed;
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

    /// Record the placement learning policy's running counters (§24/§31.3, plan 07 W14)
    /// from a [`SiteProfileTable::stats`](topo_core::SiteProfileTable::stats) snapshot. The
    /// profiler is a host-owned sibling (like the release controller / hugepage backend),
    /// so its summary is composed alongside [`record_allocator`](Self::record_allocator).
    pub fn record_placement(&mut self, ps: topo_core::PlacementStats) {
        self.placement = ps;
    }

    /// Render the snapshot as summary JSON in the Appendix-D shape (the §31.1 "where is
    /// the memory?" view). Equivalent to [`to_json_with`](Self::to_json_with) with
    /// [`StatsFlags::SUMMARY`] and no detail. The renderer is additive: new fields may be
    /// added in later milestones, never removed or renamed within a release series (§35.3).
    pub fn to_json(&self) -> String {
        self.to_json_with(StatsFlags::SUMMARY, &StatsDetail::EMPTY)
    }

    /// Render the snapshot as JSON (§31.2), honoring the selection `flags` (§31.2): the
    /// always-present summary blocks answer "where is the memory?" (§31.1); the `BY_*`
    /// flags append per-entity detail blocks from `detail` (which the live composer in
    /// `topo-abi` populates — `topo-stats` is a pure renderer). Strings here are fixed
    /// ASCII identifiers, so no escaping is required. Additive across a release series
    /// (§35.3): callers parsing a known field are never broken by a newer field.
    pub fn to_json_with(&self, flags: StatsFlags, detail: &StatsDetail) -> String {
        use core::fmt::Write as _;
        // §19.4 bin distribution (W11-4a), rendered as a compact JSON array. Built here
        // rather than inline so it tracks `HugeBin::COUNT` automatically; this is the slow
        // stats surface, so the small per-element allocations are immaterial.
        let mut hp_bins = String::from("[");
        for (i, count) in self.hugepage.bins.iter().enumerate() {
            if i > 0 {
                hp_bins.push(',');
            }
            hp_bins.push_str(&count.to_string());
        }
        hp_bins.push(']');
        // §31.5 fragmentation, derived from the byte classes (W17-4). `external` is the free
        // **physically-backed** back-end memory not immediately useful (dirty + muzzy;
        // `released` is unbacked, so excluded); `cache` is the bytes stranded in front-end
        // caches (per-CPU + thread + transfer, 0 until M2).
        let frag_external = self.dirty_bytes.saturating_add(self.muzzy_bytes);
        let frag_cache = self
            .per_cpu_bytes
            .saturating_add(self.thread_cache_bytes)
            .saturating_add(self.transfer_bytes);
        // The always-present summary object, *without* its closing brace, so the flag-gated
        // detail blocks (and the brace) can be appended below.
        let mut out = format!(
            concat!(
                "{{\n",
                "  \"topomalloc_version\": \"{version}\",\n",
                "  \"epoch\": {epoch},\n",
                "  \"profile\": \"{profile}\",\n",
                "  \"application\": {{\n",
                "    \"live_bytes\": {live},\n",
                "    \"peak_live_bytes\": {peak_live},\n",
                "    \"allocated_bytes_total\": {alloc_total},\n",
                "    \"freed_bytes_total\": {freed_total},\n",
                "    \"rss_bytes\": {rss}\n",
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
                "    \"destroyed\": {arenas_destroyed},\n",
                "    \"numa_bind_failures\": {numa_bind_failures},\n",
                "    \"hook_failures\": {{\n",
                "      \"commit\": {hf_commit},\n",
                "      \"release\": {hf_release},\n",
                "      \"split\": {hf_split},\n",
                "      \"merge\": {hf_merge}\n",
                "    }}\n",
                "  }},\n",
                "  \"backend\": {{\n",
                "    \"active_bytes\": {active},\n",
                "    \"retained_bytes\": {retained},\n",
                "    \"dirty_bytes\": {dirty},\n",
                "    \"muzzy_bytes\": {muzzy},\n",
                "    \"released_bytes\": {released},\n",
                "    \"pageheap_free_bytes\": {pageheap},\n",
                "    \"virtual_bytes\": {virtual_b},\n",
                "    \"total_managed_vm_bytes\": {total_vm}\n",
                "  }},\n",
                "  \"quarantine\": {{\n",
                "    \"bytes\": {quarantine}\n",
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
                "  \"placement\": {{\n",
                "    \"sites_tracked\": {pl_sites},\n",
                "    \"confident_sites\": {pl_confident},\n",
                "    \"alloc_samples\": {pl_alloc},\n",
                "    \"free_samples\": {pl_free},\n",
                "    \"censored_samples\": {pl_censored},\n",
                "    \"evictions\": {pl_evict},\n",
                "    \"sampled_live_bytes\": {pl_live}\n",
                "  }},\n",
                "  \"fragmentation\": {{\n",
                "    \"internal_exact_bytes\": {frag_internal_exact},\n",
                "    \"internal_sampled_bytes\": {frag_internal_sampled},\n",
                "    \"external_bytes\": {frag_external},\n",
                "    \"cache_bytes\": {frag_cache},\n",
                "    \"hugepage_bytes\": {hp_fragmentation},\n",
                "    \"metadata_overhead_bytes\": {metadata}\n",
                "  }},\n",
                "  \"metadata\": {{\n",
                "    \"bytes\": {metadata}\n",
                "  }}"
            ),
            version = VERSION,
            epoch = self.epoch,
            profile = self.profile.as_str(),
            live = self.live_bytes,
            peak_live = self.peak_live_bytes,
            alloc_total = self.allocated_bytes_total,
            freed_total = self.freed_bytes_total,
            rss = self.rss_bytes,
            per_cpu = self.per_cpu_bytes,
            thread = self.thread_cache_bytes,
            transfer = self.transfer_bytes,
            central = self.central_free_bytes,
            live_arenas = self.live_arenas,
            arenas_destroyed = self.arenas_destroyed,
            numa_bind_failures = self.numa_bind_failures,
            hf_commit = self.hook_failures.commit,
            hf_release = self.hook_failures.release,
            hf_split = self.hook_failures.split,
            hf_merge = self.hook_failures.merge,
            active = self.active_bytes,
            retained = self.retained_bytes,
            dirty = self.dirty_bytes,
            muzzy = self.muzzy_bytes,
            released = self.released_bytes,
            pageheap = self.pageheap_free_bytes,
            virtual_b = self.virtual_bytes,
            total_vm = self.virtual_bytes.saturating_add(self.metadata_bytes),
            quarantine = self.quarantine_bytes,
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
            pl_sites = self.placement.sites_tracked,
            pl_confident = self.placement.confident_sites,
            pl_alloc = self.placement.alloc_samples,
            pl_free = self.placement.free_samples,
            pl_censored = self.placement.censored_samples,
            pl_evict = self.placement.evictions,
            pl_live = self.placement.sampled_live_bytes,
            frag_internal_exact = self.exact_internal_fragmentation_bytes,
            frag_internal_sampled = self.sampled_internal_fragmentation_bytes,
            frag_external = frag_external,
            frag_cache = frag_cache,
            metadata = self.metadata_bytes,
        );
        // --- flag-gated detail blocks (§31.2) -------------------------------------------
        if flags.contains(StatsFlags::BY_ARENA) {
            out.push_str(",\n  \"by_arena\": [");
            for (i, a) in detail.arenas.iter().enumerate() {
                let _ = write!(
                    out,
                    "{}\n    {{\"id\": {}, \"label\": {}, \"used_bytes\": {}, \"reserved_bytes\": {}}}",
                    if i > 0 { "," } else { "" },
                    a.id,
                    a.label,
                    a.used,
                    a.reserved,
                );
            }
            out.push_str(if detail.arenas.is_empty() {
                "]"
            } else {
                "\n  ]"
            });
        }
        if flags.contains(StatsFlags::BY_SIZE_CLASS) {
            out.push_str(",\n  \"by_size_class\": [");
            for (i, c) in detail.size_classes.iter().enumerate() {
                let _ = write!(
                    out,
                    "{}\n    {{\"class\": {}, \"size\": {}, \"free_bytes\": {}}}",
                    if i > 0 { "," } else { "" },
                    c.class,
                    c.size,
                    c.free_bytes,
                );
            }
            out.push_str(if detail.size_classes.is_empty() {
                "]"
            } else {
                "\n  ]"
            });
        }
        if flags.contains(StatsFlags::BY_CPU) {
            // Front-end per-CPU caches land at M2; the breakdown is an empty array until
            // then (the flag resolves to a defined, additive shape, never a missing key).
            out.push_str(",\n  \"by_cpu\": []");
        }
        if flags.contains(StatsFlags::BY_NUMA) {
            // Per-node §19.7 hugepage coverage (empty in the default build / single-node host;
            // populated under the live NUMA router). The aggregate is always in `topology`.
            out.push_str(",\n  \"by_numa_node\": [");
            for (i, n) in detail.numa_nodes.iter().enumerate() {
                let _ = write!(
                    out,
                    "{}\n    {{\"node\": {}, \"coverage_bytes\": {}, \"empty_backed_bytes\": {}, \"live_bytes\": {}}}",
                    if i > 0 { "," } else { "" },
                    n.node,
                    n.coverage_bytes,
                    n.empty_backed_bytes,
                    n.live_bytes,
                );
            }
            out.push_str(if detail.numa_nodes.is_empty() {
                "]"
            } else {
                "\n  ]"
            });
        }
        if flags.contains(StatsFlags::BY_HUGEPAGE) {
            // The §19.4 occupancy-bin distribution, as a labeled {bin, count} array. (The same
            // counts are the always-present `hugepage.bin_counts`; this is the labeled detail.)
            out.push_str(",\n  \"by_hugepage_bin\": [");
            for (i, count) in self.hugepage.bins.iter().enumerate() {
                let _ = write!(
                    out,
                    "{}\n    {{\"bin\": {}, \"count\": {}}}",
                    if i > 0 { "," } else { "" },
                    i,
                    count,
                );
            }
            out.push_str("\n  ]");
        }
        out.push_str("\n}");
        out
    }

    /// A human-readable, single-paragraph RSS attribution string (§31.6 `topo_explain_memory`,
    /// W17-5): "RSS is 2.5 GiB: 1.8 GiB live, 700.0 MiB per-CPU cache, …", leading with the
    /// **actual** resident-set size (read from the OS, [`record_rss`](Self::record_rss)) and
    /// naming the largest contributors to resident memory in descending order. Adoption hinges
    /// on RSS being *explainable*, not just measurable. The contributors form a complete
    /// partition of the allocator's *physically-backed* footprint — `live + caches + central +
    /// quarantine + slab/page overhead` (= the §20.1 *active* backing) `+ dirty + muzzy +
    /// empty hugepages + metadata` — so they reconcile; decommitted (`released`) bytes are
    /// noted apart as managed-VM-not-resident (§8.6), and any RSS beyond the allocator's
    /// footprint is attributed to non-heap mappings (code, stacks, …). Integer-only binary
    /// units (§6).
    pub fn explain(&self) -> String {
        use core::fmt::Write as _;
        let caches = self
            .per_cpu_bytes
            .saturating_add(self.thread_cache_bytes)
            .saturating_add(self.transfer_bytes);
        // Slab/page overhead = the §20.1 active backing not accounted by app-visible objects:
        // partially-filled slab pages, per-slab metadata. Completes the partition of `active`.
        let slab_overhead = self.active_bytes.saturating_sub(
            self.live_bytes
                .saturating_add(self.central_free_bytes)
                .saturating_add(caches)
                .saturating_add(self.quarantine_bytes),
        );
        // (label, bytes) for every resident contributor, sorted descending; their sum is the
        // allocator's physically-backed footprint. A fixed small set, so a tiny `Vec`.
        let mut parts: alloc::vec::Vec<(&'static str, u64)> = alloc::vec![
            ("live", self.live_bytes),
            ("per-CPU cache", self.per_cpu_bytes),
            ("thread cache", self.thread_cache_bytes),
            ("transfer cache", self.transfer_bytes),
            ("central free lists", self.central_free_bytes),
            ("slab/page overhead", slab_overhead),
            ("dirty retained", self.dirty_bytes),
            ("muzzy (lazily purged)", self.muzzy_bytes),
            ("empty hugepages retained", self.hugepage.empty_backed_bytes),
            ("metadata", self.metadata_bytes),
            ("quarantine", self.quarantine_bytes),
        ];
        // Stable, deterministic order: bytes descending, then first-listed wins ties.
        parts.sort_by(|a, b| b.1.cmp(&a.1));
        let allocator_resident: u64 = parts.iter().map(|&(_, b)| b).sum();

        let mut out = String::new();
        if self.rss_bytes > 0 {
            let _ = write!(out, "RSS is {}: ", human_bytes(self.rss_bytes));
        } else {
            out.push_str("RSS is attributed to: ");
        }
        let mut first = true;
        for (label, bytes) in parts {
            if bytes == 0 {
                continue;
            }
            if !first {
                out.push_str(", ");
            }
            first = false;
            let _ = write!(out, "{} {}", human_bytes(bytes), label);
        }
        if first {
            // Nothing resident anywhere — be explicit rather than emit a dangling colon.
            out.push_str("no resident allocator memory (idle)");
        }
        out.push('.');
        // The released (unbacked) bytes are managed VM but not RSS; note them so an operator
        // reconciling RSS against `virtual_bytes` understands the difference (§8.6).
        if self.released_bytes > 0 {
            let _ = write!(
                out,
                " Additionally {} is decommitted (retained in the address space, not resident).",
                human_bytes(self.released_bytes)
            );
        }
        // If the OS RSS exceeds the allocator's resident footprint, the remainder is non-heap
        // (code, stacks, thread-local storage, other mappings) — the gap operators ask about.
        if let Some(outside) = self.rss_bytes.checked_sub(allocator_resident) {
            if outside > 0 && self.rss_bytes > 0 {
                let _ = write!(
                    out,
                    " {} of RSS is outside the allocator (code, stacks, other mappings).",
                    human_bytes(outside)
                );
            }
        }
        out
    }
}

/// Render `bytes` in binary units (B/KiB/MiB/GiB/TiB) with one decimal place for the
/// fractional units — the §31.6 explanation's human sizes. Integer-only (the kernel is
/// floating-point-free, §6): the tenths are computed as `(rem * 10) / divisor`.
fn human_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
    ];
    for &(name, div) in UNITS {
        if bytes >= div {
            let whole = bytes / div;
            let tenths = (bytes % div).saturating_mul(10) / div;
            return format!("{whole}.{tenths} {name}");
        }
    }
    format!("{bytes} B")
}

/// One line of the §31.2 `BY_ARENA` per-arena breakdown (W17-2): a single arena's id, its
/// §36.12 security label, and its live/reserved byte accounting. The unit the W17-6
/// label-scoped redaction filters over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArenaLine {
    /// The arena id (`TOPO_ARENA(id)` routes to it).
    pub id: u32,
    /// The arena's §36.12 information-flow label (`0` = the POSIX `PUBLIC` domain).
    pub label: u32,
    /// Live (own) bytes charged to the arena (§36.4 quota accounting).
    pub used: u64,
    /// Reserved child-delegation quota (§36.4).
    pub reserved: u64,
}

/// One line of the §31.2 `BY_SIZE_CLASS` per-class breakdown (W17-2): central-resident free
/// bytes for a size class.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeClassLine {
    /// The size-class index.
    pub class: u32,
    /// The class's object size in bytes.
    pub size: u32,
    /// Free object bytes resident in the class's central list.
    pub free_bytes: u64,
}

/// One line of the §31.2 `BY_NUMA` per-node breakdown (W17-2): a NUMA node's §19.7 hugepage
/// coverage. Empty (all backends absent) in the default build; populated under the live NUMA
/// router (`hugepage-optimized`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NumaNodeLine {
    /// The dense NUMA node id.
    pub node: u32,
    /// §19.7 hugepage coverage bytes placed on this node.
    pub coverage_bytes: u64,
    /// Empty-but-backed hugepage bytes retained on this node.
    pub empty_backed_bytes: u64,
    /// Live bytes on this node's hugepages.
    pub live_bytes: u64,
}

/// The per-entity detail the `BY_*` flags render (§31.2). `topo-stats` is a pure renderer:
/// the live composer (`topo-abi`) fills these from the running allocator/registry and passes
/// them to [`Stats::to_json_with`]. Borrowed slices so the renderer allocates nothing extra.
#[derive(Clone, Copy, Debug)]
pub struct StatsDetail<'a> {
    /// `BY_ARENA` lines (already label-redacted by the composer if a low domain is reading).
    pub arenas: &'a [ArenaLine],
    /// `BY_SIZE_CLASS` lines.
    pub size_classes: &'a [SizeClassLine],
    /// `BY_NUMA` per-node lines.
    pub numa_nodes: &'a [NumaNodeLine],
}

impl StatsDetail<'_> {
    /// No detail — the summary-only view.
    pub const EMPTY: StatsDetail<'static> = StatsDetail {
        arenas: &[],
        size_classes: &[],
        numa_nodes: &[],
    };
}

/// **Label-scoped stats redaction (§36.12, W17-6).** Keep only the arena lines an observer
/// at `observer_label` is authorized to see — those whose label the observer dominates
/// (`label <= observer_label`, the POSIX/total-order projection of the §36.12 lattice) —
/// and **drop** every higher-domain line. Mirrors the Lean `stats_observation_noninterference`
/// (`lean/TopoMalloc/SeLe4n/InformationFlow.lean`): a low observer's view is a pure function
/// of the low-labelled arenas, so high-domain allocation activity is invisible to it — a low
/// domain cannot infer a high domain's patterns. Pure and total.
///
/// On POSIX every arena carries the single `PUBLIC` (`0`) label, so a `PUBLIC` observer sees
/// everything (the identity case); the redaction has teeth only under the seLe4n multi-label
/// profile (plan 09), where it is the Rust-side analogue of the proved non-interference.
pub fn redact_arenas(arenas: &[ArenaLine], observer_label: u32) -> alloc::vec::Vec<ArenaLine> {
    arenas
        .iter()
        .copied()
        .filter(|a| a.label <= observer_label)
        .collect()
}

/// **Summary-level label redaction (§36.12, W17-6).** Produce the stats *summary* a low
/// observer is authorized to see — the piece `redact_arenas` (which only filters the
/// per-arena detail) leaves open. When `all_visible` is true (the observer dominates every
/// arena — always the POSIX single-`PUBLIC`-label case), the full summary is returned
/// unchanged (a `PUBLIC` observer legitimately sees everything). Otherwise every **cross-domain
/// aggregate** — the global cumulative counters, the *shared* backend / cache / central /
/// hugepage / release / placement / fragmentation state, the metadata, the peak — is redacted
/// to zero (it mixes all domains' activity, so a low observer must not see it), and the
/// observer-visible `live_bytes` is recomputed from the visible arenas' `used` (sound because
/// `Σ arena.used == live_bytes`, §8.6/§36.17).
///
/// The result is a **pure function of `(full.epoch, full.profile, visible_arenas)`**, so —
/// modulo the non-sensitive snapshot sequence number `epoch` — any higher-domain activity is
/// invisible to a low observer: the summary analogue of `redact_arenas`. Together they make
/// the JSON a low observer receives the Rust-side analogue of the proved Lean
/// `stats_observation_noninterference`. Pure and total.
pub fn redact_summary(full: &Stats, visible_arenas: &[ArenaLine], all_visible: bool) -> Stats {
    if all_visible {
        return *full;
    }
    let visible_live: u64 = visible_arenas.iter().map(|a| a.used).sum();
    Stats {
        // Non-sensitive identity (a snapshot sequence number + the build profile).
        epoch: full.epoch,
        profile: full.profile,
        // The only disclosed figure: this domain's live bytes, with a consistent
        // application identity (`live == allocated − freed`).
        live_bytes: visible_live,
        allocated_bytes_total: visible_live,
        freed_bytes_total: 0,
        // The visible arena count (the per-arena lines are redacted by `redact_arenas`).
        live_arenas: visible_arenas.len() as u64,
        // Every other field is a cross-domain aggregate ⇒ redacted to zero.
        ..Stats::default()
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
    fn placement_stats_reconcile_into_stats_and_json() {
        // W14 / W17-3: the learning policy's running counters flow into the snapshot and
        // the JSON `placement` block.
        let ps = topo_core::PlacementStats {
            sites_tracked: 12,
            confident_sites: 5,
            alloc_samples: 1000,
            free_samples: 900,
            censored_samples: 40,
            evictions: 3,
            sampled_live_bytes: 4_194_304,
        };
        let mut s = Stats::default();
        s.record_placement(ps);
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        assert_eq!(v["placement"]["sites_tracked"], 12);
        assert_eq!(v["placement"]["confident_sites"], 5);
        assert_eq!(v["placement"]["alloc_samples"], 1000);
        assert_eq!(v["placement"]["free_samples"], 900);
        assert_eq!(v["placement"]["censored_samples"], 40);
        assert_eq!(v["placement"]["evictions"], 3);
        assert_eq!(v["placement"]["sampled_live_bytes"], 4_194_304u64);
        // The default snapshot (profiling never enabled) is all-zero.
        let d: serde_json::Value = serde_json::from_str(&Stats::default().to_json()).unwrap();
        assert_eq!(d["placement"]["sites_tracked"], 0);
        assert_eq!(d["placement"]["alloc_samples"], 0);
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
            arenas_destroyed: 4,
            peak_live_bytes: 2000,
            live_internal_fragmentation_bytes: 512,
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
        // The cumulative destroyed-arena count maps through (§31.1, W17-1a).
        assert_eq!(s.arenas_destroyed, 4);
        // The two regions sum into the single back-end view (§20.1/§21.2).
        assert_eq!(s.active_bytes, 22); // 20 + 2
        assert_eq!(s.retained_bytes, 11); // reserved: 10 + 1 (§20.1 Retained)
        assert_eq!(s.dirty_bytes, 33);
        assert_eq!(s.muzzy_bytes, 44);
        assert_eq!(s.released_bytes, 55);
        assert_eq!(
            s.virtual_bytes,
            (10 + 20 + 30 + 40 + 50) + (1 + 2 + 3 + 4 + 5)
        );
        // §8.6 reconciliation identities hold by construction (W17-1b):
        //   pageheap_free == retained + dirty + muzzy + released, and
        //   virtual       == active + pageheap_free.
        assert_eq!(
            s.pageheap_free_bytes,
            s.retained_bytes + s.dirty_bytes + s.muzzy_bytes + s.released_bytes
        );
        assert_eq!(s.virtual_bytes, s.active_bytes + s.pageheap_free_bytes);
        // The application identity §8.6: live == allocated - freed.
        assert_eq!(s.live_bytes, s.allocated_bytes_total - s.freed_bytes_total);
        // JSON renders the recorded values (additive schema, §35.3).
        let json = s.to_json();
        assert!(json.contains("\"live_bytes\": 1000"));
        assert!(json.contains("\"allocated_bytes_total\": 1500"));
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(v["arenas"]["count"], 3);
        assert_eq!(v["arenas"]["destroyed"], 4);
        assert_eq!(v["arenas"]["numa_bind_failures"], 7);
        assert_eq!(v["arenas"]["hook_failures"]["commit"], 12);
        assert_eq!(v["arenas"]["hook_failures"]["release"], 13);
        assert_eq!(v["arenas"]["hook_failures"]["split"], 14);
        assert_eq!(v["arenas"]["hook_failures"]["merge"], 15);
        assert_eq!(v["backend"]["active_bytes"], 22);
        assert_eq!(v["backend"]["retained_bytes"], 11);
        // W17-2/3/4: the peak high-water, exact medium/large fragmentation, and total managed
        // VM (= virtual + metadata) all map through and render.
        assert_eq!(s.peak_live_bytes, 2000);
        assert_eq!(s.exact_internal_fragmentation_bytes, 512);
        assert_eq!(v["application"]["peak_live_bytes"], 2000);
        assert_eq!(v["fragmentation"]["internal_exact_bytes"], 512);
        assert_eq!(
            v["backend"]["total_managed_vm_bytes"],
            s.virtual_bytes + s.metadata_bytes
        );
    }

    #[test]
    fn quarantine_and_fragmentation_blocks_are_present_and_nonnegative() {
        // W17-1a: the §31.1 `quarantine.bytes` class is present (0 until plan 08 wires it).
        // W17-4: the §31.5 fragmentation block is present and derived from the byte classes.
        let s = Stats {
            dirty_bytes: 4096,
            muzzy_bytes: 2048,
            released_bytes: 1024,
            per_cpu_bytes: 512,
            metadata_bytes: 8192,
            sampled_internal_fragmentation_bytes: 333,
            hugepage: topo_core::HugeStats {
                fragmentation_bytes: 65536,
                ..topo_core::HugeStats::default()
            },
            ..Stats::default()
        };
        let v: serde_json::Value = serde_json::from_str(&s.to_json()).expect("valid JSON");
        // Quarantine class present.
        assert_eq!(v["quarantine"]["bytes"], 0);
        // §31.5 fragmentation derived correctly: external = dirty + muzzy (backed, free, not
        // immediately useful — `released` is unbacked so excluded); cache = the cache bytes;
        // hugepage from HugeStats; internal is the sampled estimate; metadata overhead.
        assert_eq!(v["fragmentation"]["internal_sampled_bytes"], 333);
        assert_eq!(v["fragmentation"]["external_bytes"], 4096 + 2048);
        assert_eq!(v["fragmentation"]["cache_bytes"], 512);
        assert_eq!(v["fragmentation"]["hugepage_bytes"], 65536);
        assert_eq!(v["fragmentation"]["metadata_overhead_bytes"], 8192);
    }

    #[test]
    fn flags_contains_bitor_and_validity() {
        // SUMMARY (0) is contained in everything; BY_* compose by bitor; reserved bits flag.
        let f = StatsFlags::BY_ARENA | StatsFlags::BY_NUMA;
        assert!(f.contains(StatsFlags::BY_ARENA));
        assert!(f.contains(StatsFlags::BY_NUMA));
        assert!(f.contains(StatsFlags::SUMMARY));
        assert!(!f.contains(StatsFlags::BY_CPU));
        assert!(!StatsFlags::ALL.has_unknown_bits());
        assert!(StatsFlags(1 << 40).has_unknown_bits());
        assert_eq!(StatsFlags::default(), StatsFlags::SUMMARY);
    }

    #[test]
    fn by_arena_detail_block_renders_when_flagged() {
        // W17-2: BY_ARENA appends a per-arena array; absent without the flag (additive).
        let s = Stats::default();
        let arenas = [
            ArenaLine {
                id: 0,
                label: 0,
                used: 1000,
                reserved: 0,
            },
            ArenaLine {
                id: 3,
                label: 0,
                used: 4096,
                reserved: 256,
            },
        ];
        let detail = StatsDetail {
            arenas: &arenas,
            size_classes: &[],
            numa_nodes: &[],
        };
        // Without the flag: no by_arena key.
        let summary: serde_json::Value =
            serde_json::from_str(&s.to_json_with(StatsFlags::SUMMARY, &detail)).unwrap();
        assert!(summary.get("by_arena").is_none());
        // With the flag: the array is present and carries both lines.
        let v: serde_json::Value =
            serde_json::from_str(&s.to_json_with(StatsFlags::BY_ARENA, &detail)).expect("valid");
        let by = v["by_arena"].as_array().expect("by_arena array");
        assert_eq!(by.len(), 2);
        assert_eq!(by[1]["id"], 3);
        assert_eq!(by[1]["used_bytes"], 4096);
        assert_eq!(by[1]["reserved_bytes"], 256);
        // An empty BY_ARENA still renders a valid empty array (not a missing key).
        let e: serde_json::Value =
            serde_json::from_str(&s.to_json_with(StatsFlags::BY_ARENA, &StatsDetail::EMPTY))
                .expect("valid");
        assert_eq!(e["by_arena"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn explain_names_the_largest_contributors() {
        // W17-5: a human-readable RSS attribution, largest first, with released noted apart.
        let s = Stats {
            live_bytes: 2 * (1 << 30) + (1 << 29), // 2.5 GiB
            per_cpu_bytes: 700 * (1 << 20),        // 700 MiB
            dirty_bytes: 1400 * (1 << 20),         // ~1.4 GiB
            released_bytes: 256 * (1 << 20),       // 256 MiB decommitted
            ..Stats::default()
        };
        let e = s.explain();
        assert!(e.starts_with("RSS is attributed to: "));
        // Largest (live, 2.5 GiB) is named before per-CPU (700 MiB).
        let live_at = e.find("live").expect("live named");
        let cpu_at = e.find("per-CPU cache").expect("cpu named");
        assert!(live_at < cpu_at, "contributors are ordered largest-first");
        assert!(e.contains("GiB") && e.contains("MiB"));
        // Released is decommitted, not resident — called out separately.
        assert!(e.contains("decommitted"));
        // The idle case is explicit, never a dangling colon.
        assert_eq!(
            Stats::default().explain(),
            "RSS is attributed to: no resident allocator memory (idle)."
        );
    }

    #[test]
    fn redaction_is_label_noninterference() {
        // W17-6 (§36.12): the Rust analogue of `stats_observation_noninterference`. A low
        // observer's redacted view is a pure function of the low-labelled arenas, so adding
        // or changing any *higher*-labelled arena leaves the low view bit-for-bit identical.
        let low = ArenaLine {
            id: 1,
            label: 0,
            used: 4096,
            reserved: 0,
        };
        let high1 = ArenaLine {
            id: 2,
            label: 5,
            used: 1 << 20,
            reserved: 0,
        };
        let high2 = ArenaLine {
            id: 2,
            label: 5,
            used: 999 << 20, // different high-domain activity
            reserved: 4096,
        };
        // Observer at label 0 sees only the low arena, regardless of the high one's bytes.
        let view_a = redact_arenas(&[low, high1], 0);
        let view_b = redact_arenas(&[low, high2], 0);
        assert_eq!(
            view_a, view_b,
            "high-domain activity is invisible to a low observer"
        );
        assert_eq!(view_a, alloc::vec![low]);
        // A high observer (label 5) dominates both labels and sees everything.
        assert_eq!(redact_arenas(&[low, high1], 5).len(), 2);
        // The default JSON for a redacted (low) view never leaks a high arena's line.
        let s = Stats::default();
        let redacted = redact_arenas(&[low, high1], 0);
        let json = s.to_json_with(
            StatsFlags::BY_ARENA,
            &StatsDetail {
                arenas: &redacted,
                size_classes: &[],
                numa_nodes: &[],
            },
        );
        assert!(!json.contains("1048576")); // the high arena's bytes never appear
    }

    #[test]
    fn summary_redaction_is_noninterference_for_the_whole_view() {
        // W17-6 correctness fix: the *summary* (not just the per-arena detail) a low observer
        // receives is a pure function of the visible arenas + epoch/profile — so ALL
        // cross-domain aggregate activity (global live/backend/placement, a high arena's bytes)
        // is invisible. Two states that differ only in high-domain activity redact identically.
        let low = ArenaLine {
            id: 1,
            label: 0,
            used: 4096,
            reserved: 0,
        };
        let high = ArenaLine {
            id: 2,
            label: 7,
            used: 64 << 20,
            reserved: 0,
        };
        // State A: modest globals. State B: very different globals + a much bigger high arena.
        let full_a = Stats {
            epoch: 9,
            profile: Profile::Performance,
            live_bytes: 4096 + (64 << 20),
            allocated_bytes_total: 10 << 20,
            freed_bytes_total: 1 << 20,
            dirty_bytes: 5 << 20,
            active_bytes: 80 << 20,
            metadata_bytes: 1 << 20,
            sampled_internal_fragmentation_bytes: 12345,
            ..Stats::default()
        };
        let high_b = ArenaLine {
            used: 900 << 20,
            ..high
        };
        let full_b = Stats {
            live_bytes: 4096 + (900 << 20),
            allocated_bytes_total: 999 << 20,
            freed_bytes_total: 7 << 20,
            dirty_bytes: 123 << 20,
            active_bytes: 950 << 20,
            metadata_bytes: 9 << 20,
            sampled_internal_fragmentation_bytes: 6_7890,
            ..full_a // same epoch + profile
        };
        // The visible (low) set is identical in both; redacting summaries must match exactly.
        let vis_a = redact_arenas(&[low, high], 0);
        let vis_b = redact_arenas(&[low, high_b], 0);
        let red_a = redact_summary(&full_a, &vis_a, false);
        let red_b = redact_summary(&full_b, &vis_b, false);
        assert_eq!(red_a, red_b, "the redacted low-domain summary is invariant");
        // It discloses only the visible domain's live bytes, with a consistent identity.
        assert_eq!(red_a.live_bytes, 4096);
        assert_eq!(red_a.allocated_bytes_total, 4096);
        assert_eq!(red_a.freed_bytes_total, 0);
        assert_eq!(red_a.dirty_bytes, 0, "shared backend state is redacted");
        assert_eq!(red_a.sampled_internal_fragmentation_bytes, 0);
        assert_eq!(red_a.live_arenas, 1);
        // The rendered JSON differs nowhere a high domain could be inferred.
        let json_a = red_a.to_json();
        let json_b = red_b.to_json();
        assert_eq!(json_a, json_b);
        // `all_visible` (a top observer / the POSIX PUBLIC case) is the identity: full stats.
        assert_eq!(redact_summary(&full_a, &[low, high], true), full_a);
    }

    #[test]
    fn explain_is_rss_aware() {
        // W17-5: with a real RSS, the explanation leads with it and attributes the resident
        // footprint, calling out the non-heap remainder (code/stacks) and decommitted bytes.
        let s = Stats {
            rss_bytes: 3 * (1 << 30),  // 3.0 GiB resident
            live_bytes: 1 << 30,       // 1.0 GiB live
            active_bytes: 1 << 30,     // (no slab overhead beyond live here)
            dirty_bytes: 512 << 20,    // 512 MiB dirty retained
            metadata_bytes: 64 << 20,  // 64 MiB metadata
            released_bytes: 256 << 20, // 256 MiB decommitted
            ..Stats::default()
        };
        let e = s.explain();
        assert!(
            e.starts_with("RSS is 3.0 GiB: "),
            "leads with the real RSS: {e}"
        );
        assert!(e.contains("1.0 GiB live"));
        assert!(e.contains("512.0 MiB dirty retained"));
        // allocator resident ≈ 1 GiB live + 512 MiB dirty + 64 MiB metadata ≈ 1.56 GiB; the
        // remaining ~1.44 GiB of the 3 GiB RSS is outside the allocator.
        assert!(e.contains("outside the allocator"));
        assert!(e.contains("decommitted"));
        // Without an RSS reading, it falls back to the attribution phrasing (no "outside").
        let no_rss = Stats {
            live_bytes: 1 << 20,
            ..Stats::default()
        };
        let e2 = no_rss.explain();
        assert!(e2.starts_with("RSS is attributed to: "));
        assert!(!e2.contains("outside the allocator"));
    }

    #[test]
    fn human_bytes_renders_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 << 20), "3.0 MiB");
        assert_eq!(human_bytes((1 << 30) + (1 << 29)), "1.5 GiB");
    }
}

// SPDX-License-Identifier: MIT
//! Configuration and the control namespace (§32, Appendix E).
//!
//! M0 ships a read-only slice of the control namespace — enough to prove the
//! plumbing (`topo.version`, `topo.profile`, `topo.stats.json`) and to give the
//! governance/docs a concrete surface to reference. Mutating knobs (cache
//! budgets, decay, arena lifecycle) and their validation (§32.4) arrive with
//! plan 07 (W20).
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};

use topo_core::VERSION;
use topo_stats::{Profile, Stats};

/// Render a boolean control value as the stable `"true"`/`"false"` strings.
#[inline]
const fn bool_str(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// The control plane (§32). Holds the active profile and the latest stats
/// snapshot, and answers reads against the Appendix-E namespace.
pub struct Control {
    profile: Profile,
    stats: Stats,
}

impl Control {
    /// Create a control plane for `profile`.
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            stats: Stats {
                profile,
                ..Stats::default()
            },
        }
    }

    /// Replace the current stats snapshot (called by the stats subsystem).
    pub fn set_stats(&mut self, stats: Stats) {
        self.stats = stats;
    }

    /// Read a control key (Appendix E). Returns `None` for unknown or
    /// not-yet-implemented keys, so callers can probe the surface safely.
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "topo.version" => Some(VERSION.to_string()),
            "topo.profile" => Some(self.profile.as_str().to_string()),
            "topo.stats.json" => Some(self.stats.to_json()),
            // The §31.6 human-readable RSS attribution and the §8.6 snapshot epoch
            // (plan 07 W17-5/1b), read from the latest snapshot.
            "topo.stats.explain" => Some(self.stats.explain()),
            "topo.stats.epoch" => Some(self.stats.epoch.to_string()),
            // §31.3 peak heap high-water + §31.6 RSS (W17-2/5), read from the latest snapshot.
            "topo.stats.peak_live_bytes" => Some(self.stats.peak_live_bytes.to_string()),
            "topo.stats.rss_bytes" => Some(self.stats.rss_bytes.to_string()),
            // §31.1 byte classes added by W17-1a (retained/active distinction,
            // quarantine, destroyed-arena count) and the §31.5 internal fragmentation
            // (sampled small + exact medium/large, W17-4), read from the latest snapshot.
            "topo.backend.active_bytes" => Some(self.stats.active_bytes.to_string()),
            "topo.backend.retained_bytes" => Some(self.stats.retained_bytes.to_string()),
            "topo.backend.total_managed_vm_bytes" => Some(
                self.stats
                    .virtual_bytes
                    .saturating_add(self.stats.metadata_bytes)
                    .to_string(),
            ),
            "topo.quarantine.bytes" => Some(self.stats.quarantine_bytes.to_string()),
            "topo.arena.destroyed" => Some(self.stats.arenas_destroyed.to_string()),
            "topo.fragmentation.internal_sampled_bytes" => {
                Some(self.stats.sampled_internal_fragmentation_bytes.to_string())
            }
            "topo.fragmentation.internal_exact_bytes" => {
                Some(self.stats.exact_internal_fragmentation_bytes.to_string())
            }
            // The W8-4 zero-size policy (§9.6, Appendix E `compat.*`): read
            // from the core's process-wide knob, so this agrees with what the
            // allocation entry points actually do. Writable via the mutating
            // control surface when plan 07 W20 lands it.
            "topo.compat.zero_size" => Some(topo_core::zero_size_policy().as_str().to_string()),
            // Deterministic test mode (§30.4, plan 08 W19-3, Appendix E
            // `deterministic.*`): read from the core's process-wide control block,
            // so this agrees with what the allocator actually does. Writable via the
            // `topomalloc_deterministic_*` C control surface (§10.5).
            "topo.deterministic.enabled" => {
                Some(bool_str(topo_core::is_deterministic()).to_string())
            }
            "topo.deterministic.seed" => {
                Some(topo_core::deterministic::seed().to_string())
            }
            "topo.deterministic.force_slow_path" => {
                Some(bool_str(topo_core::force_slow_path()).to_string())
            }
            "topo.deterministic.force_purge" => {
                Some(bool_str(topo_core::force_purge()).to_string())
            }
            // Arena summary (§22/§36.4, plan 06 W9): the number of registered
            // arenas and the cumulative NUMA binding-failure count (§15.5), read
            // from the latest stats snapshot. Per-arena introspection and the
            // mutating arena-lifecycle control surface land with plan 07 W20.
            "topo.arena.count" => Some(self.stats.live_arenas.to_string()),
            "topo.arena.numa_bind_failures" => Some(self.stats.numa_bind_failures.to_string()),
            // Release controller (§20.3/§21, plan 04 W12): the live pressure mode and
            // the backlog/demand-reserve counters, read from the latest stats snapshot.
            // The mutating decay knobs (`topo.dirty_decay_ms`, …) land with plan 07 W20.
            "topo.release.pressure_mode" => Some(self.stats.release.mode.as_str().to_string()),
            "topo.release.backlog_bytes" => Some(self.stats.release.backlog_bytes.to_string()),
            "topo.release.demand_reserve_bytes" => {
                Some(self.stats.release.demand_reserve_bytes.to_string())
            }
            // Topology summary (§15, plan 04 W13): the discovered NUMA-node / LLC-domain
            // counts from the latest stats snapshot (`1`/`0` for the single-domain case),
            // plus the live router's §15.4/§15.5 counters (bind failures, rebalancer moves
            // and bytes released, spillovers) — all `0` when no router is wired.
            "topo.numa.nodes" => Some(self.stats.numa_nodes.to_string()),
            "topo.numa.llc_domains" => Some(self.stats.llc_domains.to_string()),
            "topo.numa.bind_failures" => Some(self.stats.numa_router_bind_failures.to_string()),
            "topo.numa.rebalance_moves" => Some(self.stats.numa_rebalance_moves.to_string()),
            "topo.numa.rebalance_released_bytes" => {
                Some(self.stats.numa_rebalance_released_bytes.to_string())
            }
            "topo.numa.spillovers" => Some(self.stats.numa_spillovers.to_string()),
            // Placement / lifetime profiling (§24/§31.3, plan 07 W14 + W17-3): the learning
            // policy's summary, read from the latest stats snapshot. All `0` until heap
            // sampling is enabled (the default). Starting/stopping sampling is the
            // `topomalloc_profile_set_rate` C control (`profile.heap.start`/`stop`, §10.5).
            "topo.placement.sites_tracked" => Some(self.stats.placement.sites_tracked.to_string()),
            "topo.placement.confident_sites" => {
                Some(self.stats.placement.confident_sites.to_string())
            }
            "topo.placement.alloc_samples" => Some(self.stats.placement.alloc_samples.to_string()),
            "topo.placement.free_samples" => Some(self.stats.placement.free_samples.to_string()),
            "topo.placement.sampled_live_bytes" => {
                Some(self.stats.placement.sampled_live_bytes.to_string())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_and_profile() {
        let c = Control::new(Profile::Hardened);
        assert_eq!(c.get("topo.version").as_deref(), Some(VERSION));
        assert_eq!(c.get("topo.profile").as_deref(), Some("hardened"));
    }

    #[test]
    fn stats_json_reflects_updates() {
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            live_bytes: 4096,
            ..Stats::default()
        });
        let json = c.get("topo.stats.json").expect("stats json");
        assert!(json.contains("4096"));
    }

    #[test]
    fn unknown_keys_are_none() {
        let c = Control::new(Profile::Performance);
        assert_eq!(c.get("topo.nonexistent"), None);
    }

    #[test]
    fn reads_the_w17_observability_keys() {
        // Plan 07 W17: the new §31.1 byte classes, the §8.6 epoch, the §31.5 sampled
        // fragmentation, and the §31.6 explanation surface in the control namespace.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            epoch: 7,
            active_bytes: 4096,
            retained_bytes: 2048,
            quarantine_bytes: 0,
            arenas_destroyed: 3,
            sampled_internal_fragmentation_bytes: 128,
            exact_internal_fragmentation_bytes: 4000,
            peak_live_bytes: 2 << 20,
            rss_bytes: 5 << 20,
            virtual_bytes: 10000,
            metadata_bytes: 2000,
            live_bytes: 1 << 20,
            ..Stats::default()
        });
        assert_eq!(c.get("topo.stats.epoch").as_deref(), Some("7"));
        assert_eq!(
            c.get("topo.stats.peak_live_bytes").as_deref(),
            Some((2 << 20).to_string().as_str())
        );
        assert_eq!(
            c.get("topo.stats.rss_bytes").as_deref(),
            Some((5 << 20).to_string().as_str())
        );
        assert_eq!(c.get("topo.backend.active_bytes").as_deref(), Some("4096"));
        assert_eq!(
            c.get("topo.backend.retained_bytes").as_deref(),
            Some("2048")
        );
        assert_eq!(
            c.get("topo.backend.total_managed_vm_bytes").as_deref(),
            Some("12000")
        );
        assert_eq!(
            c.get("topo.fragmentation.internal_exact_bytes").as_deref(),
            Some("4000")
        );
        assert_eq!(c.get("topo.quarantine.bytes").as_deref(), Some("0"));
        assert_eq!(c.get("topo.arena.destroyed").as_deref(), Some("3"));
        assert_eq!(
            c.get("topo.fragmentation.internal_sampled_bytes")
                .as_deref(),
            Some("128")
        );
        // The §31.6 explanation leads with the real RSS (we set one) and names the live
        // contributor.
        let explain = c.get("topo.stats.explain").expect("explain present");
        assert!(
            explain.starts_with("RSS is 5.0 MiB: "),
            "explain: {explain}"
        );
        assert!(explain.contains("live"));
    }

    #[test]
    fn reads_the_arena_summary() {
        // Plan 06 W9: the arena count + NUMA-failure summary surface in the
        // control namespace, sourced from the stats snapshot.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            live_arenas: 4,
            numa_bind_failures: 2,
            ..Stats::default()
        });
        assert_eq!(c.get("topo.arena.count").as_deref(), Some("4"));
        assert_eq!(c.get("topo.arena.numa_bind_failures").as_deref(), Some("2"));
    }

    #[test]
    fn reads_the_release_controller_summary() {
        // Plan 04 W12: the release controller's pressure mode + backlog/reserve surface
        // in the control namespace, sourced from the stats snapshot.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            release: topo_core::ReleaseStats {
                mode: topo_core::PressureMode::Soft,
                backlog_bytes: 8192,
                demand_reserve_bytes: 4096,
                ..topo_core::ReleaseStats::default()
            },
            ..Stats::default()
        });
        assert_eq!(c.get("topo.release.pressure_mode").as_deref(), Some("soft"));
        assert_eq!(c.get("topo.release.backlog_bytes").as_deref(), Some("8192"));
        assert_eq!(
            c.get("topo.release.demand_reserve_bytes").as_deref(),
            Some("4096")
        );
    }

    #[test]
    fn reads_the_topology_summary() {
        // Plan 04 W13: the §15.2 node/LLC counts surface in the control namespace.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            numa_nodes: 2,
            llc_domains: 4,
            ..Stats::default()
        });
        assert_eq!(c.get("topo.numa.nodes").as_deref(), Some("2"));
        assert_eq!(c.get("topo.numa.llc_domains").as_deref(), Some("4"));
    }

    #[test]
    fn reads_the_live_router_counters() {
        // Plan 04 W13: the live NUMA router's §15.4/§15.5 counters surface in the control
        // namespace, and default to "0" when no router is wired.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            numa_router_bind_failures: 3,
            numa_rebalance_moves: 5,
            numa_rebalance_released_bytes: 4096,
            numa_spillovers: 9,
            ..Stats::default()
        });
        assert_eq!(c.get("topo.numa.bind_failures").as_deref(), Some("3"));
        assert_eq!(c.get("topo.numa.rebalance_moves").as_deref(), Some("5"));
        assert_eq!(
            c.get("topo.numa.rebalance_released_bytes").as_deref(),
            Some("4096")
        );
        assert_eq!(c.get("topo.numa.spillovers").as_deref(), Some("9"));
        // No router wired ⇒ "0".
        let d = Control::new(Profile::Performance);
        assert_eq!(d.get("topo.numa.bind_failures").as_deref(), Some("0"));
    }

    #[test]
    fn reads_the_placement_profiler_summary() {
        // Plan 07 W14 / W17-3: the placement learning policy's summary surfaces in the
        // control namespace, sourced from the stats snapshot.
        let mut c = Control::new(Profile::Performance);
        c.set_stats(Stats {
            placement: topo_core::PlacementStats {
                sites_tracked: 7,
                confident_sites: 3,
                alloc_samples: 500,
                free_samples: 480,
                sampled_live_bytes: 8192,
                ..topo_core::PlacementStats::default()
            },
            ..Stats::default()
        });
        assert_eq!(c.get("topo.placement.sites_tracked").as_deref(), Some("7"));
        assert_eq!(
            c.get("topo.placement.confident_sites").as_deref(),
            Some("3")
        );
        assert_eq!(
            c.get("topo.placement.alloc_samples").as_deref(),
            Some("500")
        );
        assert_eq!(c.get("topo.placement.free_samples").as_deref(), Some("480"));
        assert_eq!(
            c.get("topo.placement.sampled_live_bytes").as_deref(),
            Some("8192")
        );
        // No profiling enabled ⇒ "0".
        let d = Control::new(Profile::Performance);
        assert_eq!(d.get("topo.placement.sites_tracked").as_deref(), Some("0"));
    }

    #[test]
    fn reads_the_deterministic_mode_keys() {
        // Plan 08 W19-3: the §30.4 deterministic-mode state surfaces in the control
        // namespace, sourced from the core's process-wide control block. The seed
        // and force flags follow runtime changes (restored afterwards: process-global).
        let c = Control::new(Profile::Performance);
        // Seed reads back what the core surface set.
        topo_core::set_deterministic_seed(0x1234_5678);
        assert_eq!(
            c.get("topo.deterministic.seed").as_deref(),
            Some("305419896") // 0x1234_5678
        );
        // The force flags default false and reflect runtime toggles.
        assert_eq!(
            c.get("topo.deterministic.force_slow_path").as_deref(),
            Some("false")
        );
        topo_core::deterministic::set_force_slow_path(true);
        assert_eq!(
            c.get("topo.deterministic.force_slow_path").as_deref(),
            Some("true")
        );
        topo_core::deterministic::set_force_slow_path(false);
        // `enabled` is a real read that never panics and matches the module.
        assert_eq!(
            c.get("topo.deterministic.enabled").as_deref(),
            Some(if topo_core::is_deterministic() {
                "true"
            } else {
                "false"
            })
        );
        // Restore the default seed so the process-global state does not leak.
        topo_core::set_deterministic_seed(topo_core::deterministic::DEFAULT_SEED);
    }

    #[test]
    fn reads_the_zero_size_compat_policy() {
        let c = Control::new(Profile::Performance);
        // Default reflects the core knob (zero_unique, §9.6)…
        assert_eq!(c.get("topo.compat.zero_size").as_deref(), Some("unique"));
        // …and follows runtime changes (restored afterwards: process-global).
        topo_core::set_zero_size_policy(topo_core::ZeroSizePolicy::Null);
        assert_eq!(c.get("topo.compat.zero_size").as_deref(), Some("null"));
        topo_core::set_zero_size_policy(topo_core::ZeroSizePolicy::Unique);
    }
}

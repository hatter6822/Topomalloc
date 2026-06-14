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
            // The W8-4 zero-size policy (§9.6, Appendix E `compat.*`): read
            // from the core's process-wide knob, so this agrees with what the
            // allocation entry points actually do. Writable via the mutating
            // control surface when plan 07 W20 lands it.
            "topo.compat.zero_size" => Some(topo_core::zero_size_policy().as_str().to_string()),
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
            // counts from the latest stats snapshot (`1`/`0` for the single-domain case).
            "topo.numa.nodes" => Some(self.stats.numa_nodes.to_string()),
            "topo.numa.llc_domains" => Some(self.stats.llc_domains.to_string()),
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

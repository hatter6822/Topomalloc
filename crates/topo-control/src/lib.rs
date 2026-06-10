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

/// The control plane (§32). Holds the active profile, the latest stats
/// snapshot, and the latest per-arena snapshots (plan 06 W9), and answers
/// reads against the Appendix-E namespace.
pub struct Control {
    profile: Profile,
    stats: Stats,
    arenas: alloc::vec::Vec<topo_core::ArenaSnapshot>,
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
            arenas: alloc::vec::Vec::new(),
        }
    }

    /// Replace the current stats snapshot (called by the stats subsystem).
    pub fn set_stats(&mut self, stats: Stats) {
        self.stats = stats;
    }

    /// Replace the per-arena snapshots (plan 06 W9 — the `ArenaSet::snapshot_all`
    /// output, collected by the stats subsystem).
    pub fn set_arenas(&mut self, arenas: alloc::vec::Vec<topo_core::ArenaSnapshot>) {
        self.arenas = arenas;
    }

    /// Read a control key (Appendix E). Returns `None` for unknown or
    /// not-yet-implemented keys, so callers can probe the surface safely.
    pub fn get(&self, key: &str) -> Option<String> {
        match key {
            "topo.version" => Some(VERSION.to_string()),
            "topo.profile" => Some(self.profile.as_str().to_string()),
            "topo.stats.json" => Some(self.stats.to_json()),
            // The W9 arena surface (§22/§36.4): the live count and the
            // per-arena JSON array (authority, quota, lifecycle, NUMA).
            "topo.arenas.count" => Some(format_count(self.arenas.len())),
            "topo.arenas.json" => Some(Stats::arenas_to_json(&self.arenas)),
            // The W8-4 zero-size policy (§9.6, Appendix E `compat.*`): read
            // from the core's process-wide knob, so this agrees with what the
            // allocation entry points actually do. Writable via the mutating
            // control surface when plan 07 W20 lands it.
            "topo.compat.zero_size" => Some(topo_core::zero_size_policy().as_str().to_string()),
            _ => None,
        }
    }
}

/// Render a count (no_std-friendly).
fn format_count(n: usize) -> String {
    let mut s = String::new();
    core::fmt::Write::write_fmt(&mut s, format_args!("{n}")).expect("infallible");
    s
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
    fn arena_keys_reflect_snapshots() {
        let mut c = Control::new(Profile::Performance);
        assert_eq!(c.get("topo.arenas.count").as_deref(), Some("0"));
        assert_eq!(c.get("topo.arenas.json").as_deref(), Some("[]"));
        c.set_arenas(alloc::vec![topo_core::ArenaSnapshot {
            id: topo_core::ArenaId(0),
            name: topo_core::ArenaName::new("default").unwrap(),
            state: topo_core::ArenaState::Active,
            label: topo_core::Label::PUBLIC,
            rights: topo_core::CapRights::ALL,
            parent: None,
            quota_limit: u64::MAX,
            quota_used: 42,
            quota_delegated: 0,
            numa: topo_core::NumaPolicy::OsDefault,
            resets: 0,
            reset_generation: 0,
            numa_binding_failures: 0,
            scrubs: 0,
            scrubs_skipped_label: 0,
            engine: topo_core::AllocatorStats {
                live_bytes: 42,
                ..Default::default()
            },
        }]);
        assert_eq!(c.get("topo.arenas.count").as_deref(), Some("1"));
        let json = c.get("topo.arenas.json").unwrap();
        assert!(json.contains("\"name\": \"default\""));
        assert!(json.contains("\"state\": \"active\""));
        assert!(json.contains("\"used\": 42"));
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

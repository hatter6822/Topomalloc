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
}

// SPDX-License-Identifier: MIT
//! Build-profile reflection (§30.1). Profiles are Cargo features, not forks
//! (overview principle 8); this module reports which one is active so stats
//! (`topomalloc` `profile` field) and the control plane (`topo.profile`) agree
//! with the actual build. The feature→profile mapping is documented in
//! `profiles/README.md`.

/// The active build profile name, derived from the compiled-in features.
///
/// Precedence (most-specific first) keeps a single canonical answer when several
/// features are enabled together.
pub const fn active_profile() -> &'static str {
    if cfg!(feature = "hardened") {
        "hardened"
    } else if cfg!(feature = "debug") {
        "debug"
    } else if cfg!(feature = "deterministic-test") {
        "deterministic_test"
    } else if cfg!(feature = "low-rss") {
        "low_rss"
    } else if cfg!(feature = "hugepage-optimized") {
        "hugepage_optimized"
    } else {
        "performance"
    }
}

/// Whether Appendix-B invariant checks are compiled in (the `debug-checks`
/// feature, implied by `debug`/`hardened`). Hot paths gate expensive checks on
/// this so `performance` pays nothing (docs/CONVENTIONS.md §2).
pub const fn debug_checks_enabled() -> bool {
    cfg!(feature = "debug-checks")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_build_is_performance() {
        // The test harness builds with default features, so this is the
        // performance profile with no debug checks.
        assert_eq!(active_profile(), "performance");
        assert!(!debug_checks_enabled());
    }
}

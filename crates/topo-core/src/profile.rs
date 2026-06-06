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
    // Calls are `super::`-qualified rather than `use super::*` so the module carries
    // no unused import when every test below is `cfg`-gated out (e.g. under
    // `--features low-rss`, where neither applies).

    #[test]
    #[cfg(not(any(
        feature = "debug-checks",
        feature = "low-rss",
        feature = "deterministic-test",
        feature = "hugepage-optimized"
    )))]
    fn default_build_is_performance() {
        // With no profile feature the build is `performance` and pays no debug
        // checks. Gated off whenever *any* profile feature is active (debug-checks
        // covers the `hardened`/`debug` passes; the other profiles are listed
        // explicitly), where the assertion deliberately would not hold — so the suite
        // stays green under, e.g., `--features low-rss` (W4-3b).
        assert_eq!(super::active_profile(), "performance");
        assert!(!super::debug_checks_enabled());
    }

    #[test]
    #[cfg(feature = "debug-checks")]
    fn debug_checks_feature_enables_the_checks() {
        // The hardened/debug pass: the §17.3/Appendix-B checks are compiled in.
        assert!(super::debug_checks_enabled());
    }
}

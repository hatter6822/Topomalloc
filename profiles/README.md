<!-- SPDX-License-Identifier: MIT -->
# `profiles/` — profile definitions & feature wiring

Profiles are **features, not forks** (overview principle 8): the same code base
ships every profile, selected by Cargo features rather than separate builds.
This directory is the canonical map from a SPEC profile name (§30.1) to the
Cargo feature set that realizes it.

| Profile (§30.1) | Intent | Cargo features (current → planned) |
|-----------------|--------|------------------------------------|
| `performance` | optimized; sampled checks off; debug fills off (the default) | *(none)* → release-tuned defaults |
| `hardened` | metadata protection, double/invalid-free detection, quarantine, guard pages (§29) | `topo-core/debug-checks` → `hardened` (plan 08 W18) |
| `debug` | the Appendix-B invariant checklist as runtime assertions (§30.2) | `topo-core/debug-checks` → `debug` (plan 08 W19) |
| `deterministic_test` | seeded randomness, deterministic refill, force-slow-path, trace IDs (§30.4) | `topo-test-support` deterministic harness → `deterministic_test` (plan 08 W19-3) |
| `low_rss` | aggressive release / unmap (§20.5) | *(planned)* plan 04 W4-3b |
| `hugepage_optimized` | hugepage-aware placement tuned up (§19) | *(planned)* plan 04 W11 |

## Status at M0

The profile *names* and the stats/`topo.profile` plumbing exist (`topo_stats::Profile`),
and `topo-core` exposes the `debug-checks` feature. The feature wiring above is
filled in as each subsystem lands (caches/hardening/release in plans 04, 05, 08).
The build profile is reported in stats JSON (`profile`) and via `topo.profile`.

The cargo *build* profiles (`dev`/`release`) are orthogonal: `release` is used
for the `performance` build, but debug assertions are gated by **feature**, not
by the cargo profile, so a `release` build can still run hardened checks.

<!-- SPDX-License-Identifier: MIT -->
# `profiles/` — feature-composed build profiles

Profiles are Cargo feature combinations, not forks. The same source tree builds
performance, hardened, debug, deterministic, low-RSS, and hugepage-oriented
variants.

| Profile | Intent | Feature shape |
|---------|--------|---------------|
| `performance` | Optimized default with sampled checks and expensive diagnostics off. | Default feature set / release profile. |
| `hardened` | Metadata protection, invariant checks, junk fill, quarantine, guard pages, and scrub-before-downgrade. | `topo-abi/hardened` / `topo-core/hardened`. |
| `debug` | Runtime invariant checks plus debug-oriented fill behavior. | `topo-abi/debug` / `topo-core/debug`. |
| `deterministic_test` | Seeded randomness, deterministic refill/purge controls, and trace identifiers. | Deterministic test/control features. |
| `low_rss` | More aggressive release/unmap behavior. | Low-RSS policy features. |
| `hugepage_optimized` | Route eligible large allocations through hugepage-aware placement. | `topo-abi/hugepage-optimized` / `topo-core/hugepage-optimized`. |

Runtime knobs still default to conservative behavior: quarantine and guard-page
sampling must be enabled through the C control surface or environment variables
even when their features are compiled in.

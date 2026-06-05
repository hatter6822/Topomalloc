<!-- SPDX-License-Identifier: MIT -->
# `tests/` — cross-crate integration tests

The `topo-tests` crate holds tests that exercise several crates together (the
per-crate unit tests live inside each crate). Integration tests are in
`tests/tests/`:

| File | Covers |
|------|--------|
| `abi.rs` | the prefixed C ABI (`topomalloc_*`): alignment, distinctness, overflow safety, `free(NULL)` |
| `walking_skeleton.rs` | the M0 vertical slice — classify → allocate through the seam → emit §33.7 trace → parse → replay against the executable model (W0-14) |
| `property.rs` | property tests (`proptest`, §34.3): request classification soundness (small/medium/large split, over-alignment routing, overflow safety), alignment, no duplicate live pointer, bounded usage, calloc overflow/zeroing |
| `dual_backend.rs` | the G-sim seed — identical behaviour over POSIX and `Sele4nSim`. Compiled only with `--features sele4n-sim` (GPL test binary) |

The default build is MIT/POSIX-only; `--features sele4n-sim` additionally links
the GPL seLe4n backend. This directory will also grow concurrency, fork, and
fuzz-corpus tests (plan 08).

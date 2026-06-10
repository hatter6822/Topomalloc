<!-- SPDX-License-Identifier: MIT -->
# `tests/` — cross-crate integration tests

The `topo-tests` crate holds tests that exercise several crates together (the
per-crate unit tests live inside each crate). Integration tests are in
`tests/tests/`:

| File | Covers |
|------|--------|
| `abi.rs` | the full prefixed C ABI (plan 06 W8): core/aligned/C23/extended entry points, recycle-on-free, usable-size ↔ `nallocx` coherence, errno discipline, realloc contract, concurrent use |
| `walking_skeleton.rs` | the M0 vertical slice — classify → allocate through the seam → emit §33.7 trace → parse → replay against the executable model (W0-14) |
| `property.rs` | property tests (`proptest`, §34.3): request classification soundness, alignment, no duplicate live pointer, calloc overflow/zeroing, **realloc content-preservation + failure-safety** (W8/W15 DoD), and an engine allocate/free stream replayed against the `LiveModel` ownership oracle |
| `dual_backend.rs` | the G-sim gate — identical abstract behaviour over POSIX and `Sele4nSim`, for the M0 skeleton **and** the M1 engine. Compiled only with `--features sele4n-sim` (GPL test binary) |

C/C++ ABI harnesses live next door: `tests/c/abi_smoke.c` (C11) and
`tests/cpp/abi_smoke.cpp` (C++17, including the opt-in operator new/delete
header) — compiled, linked against the staticlib, and run by
`cargo xtask abi-test`, which also cross-checks the exported symbol set
against `include/topomalloc.h` (W8-8).

The default build is MIT/POSIX-only; `--features sele4n-sim` additionally links
the GPL seLe4n backend. This directory will also grow concurrency, fork, and
fuzz-corpus tests (plan 08).

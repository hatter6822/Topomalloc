<!-- SPDX-License-Identifier: MIT -->
# `tests/` — cross-crate integration tests

The `topo-tests` crate exercises behavior that spans crates. Per-crate unit tests
remain next to their implementations.

| Area | Coverage |
|------|----------|
| ABI smoke and integration | Prefixed C API, aligned/POSIX allocation, C23 sized frees, `topo_*x`, errno rules, usable-size coherence, stats/control surfaces, and concurrent use. |
| Properties | Classification, alignment, calloc overflow/zeroing, realloc content/failure safety, allocation-stream ownership, release accounting, and placement invariants. |
| Dual backend | POSIX and `Sele4nSim` equivalence for allocator-visible behavior; enabled with the GPL `sele4n-sim` feature. |
| Runtime modes | Zero-size policy isolation, deterministic replay, debug invariant sweeps, hardening behavior, fork/TLS/concurrency paths, and sanitizer-oriented harnesses. |
| C/C++ harnesses | `tests/c/abi_smoke.c` and `tests/cpp/abi_smoke.cpp`, compiled and run by `cargo xtask abi-test`. |

Use `cargo xtask test` for the full suite or `cargo xtask test --kind <kind>` for
focused runs. The default build is MIT/POSIX-only; enabling `sele4n-sim` links the
GPL seLe4n backend into the test binary.

<!-- SPDX-License-Identifier: MIT -->
# Introduction

TopoMalloc is a general-purpose allocator built around three constraints:

- **Safety first:** correctness invariants are gating; policy can be conservative,
  but allocator safety must not regress.
- **One provider seam:** platform interaction is isolated behind
  `TopoBackingProvider`, allowing the POSIX and seLe4n backends to share the same
  allocator core.
- **Model-backed development:** generated tables, trace oracles, differential
  tests, and Lean proofs are maintained with the implementation.

## Current status

The workspace version is **0.3.0**. The tree includes the central allocation
path, C/C++/Rust ABI surfaces, arena and extent-hook support, hugepage and NUMA
placement infrastructure, stats/observability, opt-in hardening, debug invariant
checkers, deterministic replay support, and sanitizer/fuzz/loom gates.

## Authoritative references

- Project overview: [`README.md`](https://github.com/hatter6822/topomalloc/blob/main/README.md)
- Specification: [`planning/SPEC.md`](https://github.com/hatter6822/topomalloc/blob/main/planning/SPEC.md)
- Implementation plan: [`planning/plans/README.md`](https://github.com/hatter6822/topomalloc/blob/main/planning/plans/README.md)
- Decisions: [`docs/DECISIONS.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/DECISIONS.md)
- Coding conventions: [`docs/CONVENTIONS.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/CONVENTIONS.md)
- Versioning and ABI: [`docs/ABI.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/ABI.md)

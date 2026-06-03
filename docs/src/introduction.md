<!-- SPDX-License-Identifier: MIT -->
# Introduction

TopoMalloc is a general-purpose memory allocator built on three commitments:

- **Safety before policy.** A build may ship a dumb-but-correct policy; it may
  never ship an unsafe fast path. The unconditional safety invariants stay green
  on every change.
- **A formal model in lockstep.** A Lean model defines the abstract state
  machine and proves its invariants; the size-class tables are *generated* from
  a single source of truth and machine-checked.
- **One seam, two backends.** All OS/kernel interaction goes through one
  `TopoBackingProvider` trait. POSIX and the seLe4n capability microkernel are
  co-equal behind it.

This book is the operator/contributor site. The authoritative documents are:

- The specification: [`planning/SPEC.md`](https://github.com/hatter6822/topomalloc/blob/main/planning/SPEC.md)
- The implementation plan: [`planning/plans/README.md`](https://github.com/hatter6822/topomalloc/blob/main/planning/plans/README.md)
- Decisions D3–D8: [`docs/DECISIONS.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/DECISIONS.md)
- Coding conventions: [`docs/CONVENTIONS.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/CONVENTIONS.md)
- Versioning & ABI: [`docs/ABI.md`](https://github.com/hatter6822/topomalloc/blob/main/docs/ABI.md)

## Status

The project is at milestone **M0** — the walking skeleton. Every tool in the
pipeline (Rust workspace, the codegen single-source-of-truth, the Lean model,
the dual backends, and the trace/replay differential spine) is wired and runs
end to end. The real allocator (front/middle/back ends, caches, RSEQ, hugepages,
arenas, hardening) is built milestone by milestone on top of this substrate.

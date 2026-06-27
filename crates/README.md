<!-- SPDX-License-Identifier: MIT -->
# `crates/` — Rust workspace crates

The workspace is organized around a small allocator core plus adapters for ABI,
backing providers, architecture support, stats/control plumbing, and tests.
`topo-core` is the owner of allocator policy and metadata; platform memory comes
through `TopoBackingProvider` implementations instead of direct OS calls.

| Crate | Role | License |
|-------|------|---------|
| `topo-core` | classifier, generated size classes, metadata, pagemap, extents, central allocator path, arenas, hardening/debug modules | MIT |
| `topo-abi` | exported C ABI, Rust `GlobalAlloc`, C-facing controls, stats, profiling, hardening, and deterministic/debug entry points | MIT |
| `topo-backend-posix` | default POSIX provider over `mmap`, `madvise`, and `mprotect` | MIT |
| `topo-backend-sele4n` | seLe4n simulator and optional real ABI provider | GPL-3.0-or-later |
| `topo-arch` | architecture-specific fast-path/RSEQ support and fallback selection | MIT |
| `topo-stats` | snapshot structs, additive JSON rendering, memory explanations, and redaction helpers | MIT |
| `topo-control` | configuration sources and control namespace plumbing | MIT |
| `topo-test-support` | trace grammar, deterministic PRNG, live model, and test generators | MIT |

The default MIT artifact does not link `topo-backend-sele4n`. See
[`../NOTICE`](../NOTICE) for the split-license policy.

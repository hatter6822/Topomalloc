<!-- SPDX-License-Identifier: MIT -->
# `crates/` — the Rust workspace

The allocator, split into focused crates along the central seam. `topo-core` is
`no_std`-capable and OS-agnostic — it never calls `mmap`/`retype`; all backing
memory comes through the `TopoBackingProvider` trait it defines. Everything else
either implements that seam (the backends), exposes the allocator (the ABI), or
supports it (arch, stats, control, test-support).

| Crate | Role | License | Plan |
|-------|------|---------|------|
| `topo-core` | classifier, size classes, the seam, the M0 skeleton allocator | MIT | 03, 05 |
| `topo-abi` | C ABI exports + Rust `GlobalAlloc` adapter | MIT | 06 |
| `topo-backend-posix` | `PosixBackingProvider` (single-authority case) | MIT | 04 |
| `topo-backend-sele4n` | `Sele4nSim` + (M1) `Sele4nBackingProvider` | GPL-3.0-or-later | 09 |
| `topo-arch` | per-arch RSEQ / restartable sections | MIT | 05 |
| `topo-stats` | stats / profiling / explain | MIT | 07 |
| `topo-control` | config + control namespace | MIT | 07 |
| `topo-test-support` | trace grammar, deterministic harness, generators | MIT | 08 |

`topo-backend-sele4n` is the one crate here that is GPL-3.0-or-later (D5); the
default MIT artifact never links it. See [`../NOTICE`](../NOTICE).

<!-- SPDX-License-Identifier: MIT -->

<h1 align="center">TopoMalloc</h1>

<p align="center">
  A safety-first, formally grounded, topology-aware memory allocator.
</p>

<p align="center">
  <a href="https://github.com/hatter6822/topomalloc/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/hatter6822/topomalloc/ci.yml?branch=main&label=CI" />
  </a>
  <img alt="Version" src="https://img.shields.io/badge/version-v0.4.3-blue" />
  <img alt="Rust" src="https://img.shields.io/badge/Rust-1.94-dea584" />
  <img alt="Lean" src="https://img.shields.io/badge/Lean-4.28.0-10b981" />
  <img alt="License" src="https://img.shields.io/badge/license-MIT-informational" />
</p>

TopoMalloc is a Rust allocator workspace with a C/C++ ABI, POSIX and seLe4n-backed
providers, generated size-class tables, and a Lean 4 model that tracks the Rust
implementation. The default build is MIT-licensed and does not link GPL code;
seLe4n integration is isolated behind explicit GPL-3.0-or-later artifacts.

## Status at a glance

| Attribute | Value |
|-----------|-------|
| Project version | `0.4.3` |
| Rust toolchain | `1.94` stable, pinned by `rust-toolchain.toml` |
| Lean toolchain | `v4.28.0`, pinned by `lean-toolchain` |
| Primary platforms | x86-64 and AArch64 |
| Default artifact | MIT POSIX allocator (`libtopomalloc`) |
| Optional GPL artifact | seLe4n simulator / real ABI integration |

The current tree contains the central allocator path, the per-CPU/transfer front
end with its restartable-sequence fast path, extended C ABI, arena and
extent-hook surfaces, hugepage-aware backing, topology routing, observability,
hardening features, deterministic/debug modes, and sanitizer/test harnesses. The
formal model and generated tables are part of the normal development workflow,
not after-the-fact documentation.

## What is implemented

- **Allocator core:** request classification, generated size classes, central
  free lists, extent-backed large allocations, `free`, `realloc`, aligned
  allocation, usable-size queries, and zero-size/errno semantics.
- **Front end:** a per-CPU object cache backed by a transfer cache, served by
  restartable sequences (`rseq`) where the platform supports them and by a
  ranked spinlock otherwise, so a small allocation and free stay off the
  contended central lock. Exact double-free detection is preserved by a
  per-object residency marker, and cached memory is reported as its own byte
  class and reclaimable on demand (`topomalloc_cache_flush_all`).
- **Public ABI:** prefixed C symbols (`topomalloc_*`, `topo_*`), C23 sized frees,
  `topo_*x` flags, opt-in C++ `operator new`/`delete`, and a Rust `GlobalAlloc`
  adapter.
- **Backing providers:** a POSIX `mmap`/`madvise`/`mprotect` provider and an
  optional seLe4n provider/simulator sharing the same `TopoBackingProvider` seam.
- **Arenas and placement:** capability-aware arenas, custom extent hooks,
  topology-aware routing, NUMA controls, hugepage bins, release control, and
  lifetime/hotness placement inputs.
- **Observability:** additive stats JSON, fixed C snapshot struct, memory
  explanation output, peak/reset controls, fragmentation metrics, sampling, and
  label-scoped redaction.
- **Hardening and debug modes:** junk fill, quarantine, sampled guard pages,
  scrub-before-downgrade, double/invalid-free detection, Appendix-B invariant
  checkers, deterministic replay controls, and sanitizer integrations.
- **Formal checks:** a Lean 4 model with generated-table gates, trace/provider/
  extent/arena differentials, WellFormed preservation theorems, and seLe4n
  refinement/non-interference proofs.

## Quick start

```sh
cargo xtask setup     # install or verify pinned Rust, Lean, and cross targets
cargo xtask ci        # run the same build, lint, generated-table, test, and Lean gates as CI
```

For focused work, use the narrower commands below:

```sh
cargo xtask fmt --check
cargo xtask gen --check
cargo xtask lint
cargo xtask test --kind unit
cargo xtask abi-test
cargo xtask lean --check
cargo xtask bench      # non-gating micro-benchmarks
```

`cargo xtask` is the single supported entry point for automation. See
[`CONTRIBUTING.md`](CONTRIBUTING.md) and [`xtask/README.md`](xtask/README.md) for
command details.

## Architecture

```text
Public API
  C ABI + C++ header + Rust GlobalAlloc                         topo-abi, include/
        │
        ▼
Allocator engine
  classification, arenas, stats, hardening, extents, pagemap     topo-core
        │
        ▼
Provider seam
  TopoBackingProvider: allocate, release, purge, protect, stats   topo-core
        │
        ├── POSIX mmap/madvise/mprotect                           topo-backend-posix
        └── seLe4n capability-backed frames                        topo-backend-sele4n

Formal and verification sidecars
  Lean model, generated table checks, trace replay, fuzz/loom/sanitizers
```

The important boundary is `TopoBackingProvider`: allocator policy and metadata do
not call platform APIs directly. That keeps the core portable, makes POSIX and
seLe4n comparable in tests, and lets the Lean model reason about provider state
explicitly.

## Workspace guide

| Path | Purpose |
|------|---------|
| `crates/topo-core` | allocator engine, generated tables, metadata, extents, arenas, hardening/debug modules |
| `crates/topo-abi` | exported C ABI, Rust `GlobalAlloc`, and C-facing control/stat surfaces |
| `crates/topo-backend-posix` | default MIT POSIX backing provider |
| `crates/topo-backend-sele4n` | optional GPL seLe4n simulator / ABI backend |
| `crates/topo-arch` | architecture-specific RSEQ and fast-path selection support |
| `crates/topo-stats` | snapshots, additive JSON, explanations, redaction helpers |
| `crates/topo-control` | configuration and control namespace plumbing |
| `crates/topo-test-support` | trace grammar, deterministic PRNG, live-model test support |
| `include/` | public C/C++ headers and generated size-class header |
| `lean/` | Lean model and seLe4n proof bridge |
| `tests/` | cross-crate Rust/C/C++ integration tests |
| `tools/` | size-class generator and trace replay utility |
| `xtask/` | build, lint, codegen, test, and CI driver |
| `docs/` | conventions, ABI policy, decisions, and mdBook sources |
| `planning/` | long-form specification and roadmap plans |

## Documentation map

- [`docs/ABI.md`](docs/ABI.md) — stable C ABI, versioning policy, symbol/header checks.
- [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md) — coding, safety, profile, and generated-file conventions.
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — ratified architecture decisions and audit notes.
- [`docs/src/`](docs/src/) — mdBook operator/contributor guide.
- [`planning/SPEC.md`](planning/SPEC.md) — full design specification.
- [`planning/plans/README.md`](planning/plans/README.md) — implementation plan index.

## Testing and verification

`cargo xtask ci` is the recommended pre-PR gate. It composes formatting,
generated-table drift checks, lints, workspace builds, integration tests, ABI
harnesses, and Lean checks. Additional targeted gates include fuzzing, loom,
TSan/ASan/MSan/LSan modes, RSEQ equivalence, differential trace replay, and
non-gating Criterion benchmarks.

Generated files are checked rather than trusted. Do not hand-edit
`crates/topo-core/src/generated/tables.rs`, `include/topomalloc_tables.h`, or
`lean/TopoMalloc/Generated/SizeClasses.lean`; edit the golden input and run
`cargo xtask gen`.

## Licensing

TopoMalloc is split-licensed:

- The standalone allocator core and default POSIX artifact are **MIT**.
- The seLe4n integration layer is **GPL-3.0-or-later** because it links/models
  the GPL seLe4n ABI.

See [`LICENSE`](LICENSE), [`sele4n/LICENSE`](sele4n/LICENSE), and [`NOTICE`](NOTICE)
for the precise SPDX policy.

## Contributing and security

Read [`CONTRIBUTING.md`](CONTRIBUTING.md) before changing code. Security scope,
supported versions, hardening profiles, and reporting instructions are in
[`SECURITY.md`](SECURITY.md).

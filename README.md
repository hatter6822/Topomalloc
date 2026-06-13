<!-- SPDX-License-Identifier: MIT -->

<h1 align="center">TopoMalloc</h1>

<p align="center">
  A safety-first, formally-grounded, capability-aware general-purpose memory allocator.
</p>

<p align="center">
  <a href="https://github.com/hatter6822/topomalloc/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/hatter6822/topomalloc/ci.yml?branch=main&label=CI" />
  </a>
  <img alt="Version" src="https://img.shields.io/badge/version-v0.1.0-blue" />
  <img alt="Rust" src="https://img.shields.io/badge/Rust-1.94.1-dea584" />
  <img alt="Lean" src="https://img.shields.io/badge/Lean-4.28.0-10b981" />
  <img alt="License" src="https://img.shields.io/badge/license-MIT-informational" />
</p>

Rust core (`no_std`-capable hot path) + per-arch assembly + a Lean 4 formal
model, with POSIX and the [seLe4n](https://github.com/hatter6822/seLe4n)
capability microkernel co-equal behind one backing-provider seam.

## What TopoMalloc provides

- **Per-CPU caching** with RSEQ restartable sequences (x86-64, AArch64) and locked/pinned-core fallbacks.
- **Topology-aware transfer layers** and jemalloc-style policy arenas.
- **Temeraire-style hugepage-aware backing** with dirty/muzzy/released extent tracking.
- **A Lean 4 formal model** — 14-clause `WellFormed` predicate, sorry-free theorems, executable trace oracle — in lockstep with the Rust implementation.
- **seLe4n microkernel integration** — capability-backed arenas, exact-byte quota accounting, SMP non-interference proofs, coupled alloc/free simulation.
- **Full C/C++/Rust ABI** — standard `malloc`/`free`, C23 sized frees, extended `topo_*x` API, opt-in C++ `operator new`/`delete`, Rust `GlobalAlloc` adapter.
- **Split licensing** — the core is MIT; the seLe4n integration is GPL-3.0-or-later. The default artifact links no GPL code.

## Current state

| Attribute | Value |
|-----------|-------|
| Version | `v0.1.0` |
| Rust toolchain | `1.94.1` stable (pinned in `rust-toolchain.toml`) |
| Lean toolchain | `v4.28.0` (pinned in `lean-toolchain`) |
| Milestone | M0 closed; **M1 (central-path allocator) under way** |
| Cross targets | x86-64 + AArch64 (co-primary in CI) |
| Lean model | sorry-free; single-core + SMP theorem sets complete |

The public API runs over the real central-path allocator: classify → central
free lists / extent-backed large path, with genuine `free`/`realloc`/`malloc_usable_size`,
errno semantics, C23 sized frees, the extended `topo_*x` API, opt-in C++
operators, and the Rust `GlobalAlloc` adapter — identical over POSIX and the
seLe4n simulator (G-sim).

**Capability-backed arenas (W9)** ride on top: a live multi-arena data path with
per-arena isolation (§22.7), the full §22.3/§36.13 lifecycle
(create / delegate / reset / destroy / revocation), attenuation-only delegation
of rights / quota / label (§36.4) — with each child's quota **reserved** on its
parent so a delegated subtree's live bytes stay within the root's quota (proved
in Lean by `subtree_used_le_quota`) — NUMA policy modes (§15.5), and a C arena API
(`topo_arena_create` / `delegate` / `reset` / `destroy`). The arena lifecycle
state machine is modeled and proof-checked in Lean. Front-end caches (M2) and
the remaining M1 pieces land per the plan.

## Quick start

```sh
cargo xtask setup     # install the pinned Rust + Lean toolchains and cross targets
cargo xtask ci        # build (x86-64 + AArch64), lint, gen-check, test, Lean
```

A fresh clone is green with just `cargo xtask setup && cargo xtask ci`.

`cargo xtask` is the single entry point developers and CI both use, so a build
is never "Rust only". See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full
command list and the Definition of Done.

## Architecture

```text
┌──────────────────────────────────────────────────────────────────┐
│ Public API:  C ABI (malloc/free/…) + Rust GlobalAlloc            │  topo-abi
├──────────────────────────────────────────────────────────────────┤
│ Request classifier: size class, alignment, arena, label, hints   │  topo-core
├──────────────────────────────────────────────────────────────────┤
│ Front-end: per-CPU (RSEQ / pinned-core) or thread cache          │  topo-core + topo-arch
├──────────────────────────────────────────────────────────────────┤
│ Middle-end: transfer caches + central free lists                 │  topo-core
├──────────────────────────────────────────────────────────────────┤
│ Back-end: extent manager, pagemap, hugepage-aware backing        │  topo-core
├──────────────────────────────────────────────────────────────────┤
│            TopoBackingProvider seam (the central abstraction)     │
├─────────────────────────┬────────────────────────────────────────┤
│ PosixBackingProvider    │ Sele4nSim / SeLe4nBackingProvider      │
│ mmap / madvise / mprotect│ capability-typed frames + quotas       │
│ (MIT)                   │ (GPL-3.0-or-later)                     │
└─────────────────────────┴────────────────────────────────────────┘
          ↕ differential lockstep ↕
┌──────────────────────────────────────────────────────────────────┐
│ Lean 4 formal model: State, WellFormed, Transitions, Theorems    │
│ + seLe4n bridge: coupled simulation, SMP non-interference        │
└──────────────────────────────────────────────────────────────────┘
```

Everything hangs off one interface — `TopoBackingProvider`. POSIX and seLe4n
are co-equal behind it from M1, so the core allocator is OS-agnostic and
`no_std`-capable.

## Workspace

| Crate | Role | License |
|-------|------|---------|
| `topo-core` | classifier, size classes, seam, metadata, pagemap, extents, central-path allocator, capability-backed arena registry | MIT |
| `topo-abi` | C/C++/Rust ABI surface (malloc, free, GlobalAlloc, C23, `topo_*x`) | MIT |
| `topo-backend-posix` | POSIX backend — mmap/madvise/mprotect | MIT |
| `topo-backend-sele4n` | seLe4n simulator + (M1) real seLe4n ABI backend | GPL-3.0-or-later |
| `topo-arch` | per-arch RSEQ assembly (x86-64, AArch64), fast-path mode selector | MIT |
| `topo-stats` | statistics, JSON snapshots, version wiring | MIT |
| `topo-control` | configuration sources, control namespace | MIT |
| `topo-test-support` | trace grammar, `LiveModel` oracle, deterministic PRNG | MIT |

Supporting crates: `xtask` (build driver), `tools/size-class-gen` (single
source of truth for size-class tables), `tools/trace-replay` (§33.7 trace
replay), `tests` (cross-crate integration tests), `fuzz` (cargo-fuzz targets,
nightly-only).

## Lean formal model

The Lean 4 model defines the allocator's abstract states, the 14-clause
`WellFormed` predicate, all transitions as total functions, and the theorem
families — it is not on the production hot path. It is built with `lake` and
driven by `cargo xtask lean`.

**Soundness:** No `sorry`, no `admit`, no `native_decide`. The only postulated
axioms are the four §33.5 RSEQ primitives. Every theorem rests only on Lean's
standard axioms (`propext`/`Quot.sound`/`Classical.choice`).

**Lean gates (`lake exe check`):**
- Size-class table gate (72 classes, `small_max` = 32 KiB, `huge_threshold` = 2 MiB)
- Trace oracle gate (§33.7 replay + injected-violation detection)
- Pagemap differential (Lean model ↔ Rust radix)
- Provider state machine differential (§36.6)
- Extent state machine differential (§20.1)
- Arena lifecycle differential (§22.3/§36.13 transitions + revocation chain)

**Selected headline theorems:**

| Property | Module |
|----------|--------|
| Size-class table covers all small requests | `Theorems/SizeClass.lean` |
| 14-clause WellFormed preservation (per transition) | `Theorems/*.lean` |
| Arena lifecycle: alloc only in Active; partial failure never Destroyed | `ArenaLifecycle.lean` |
| Capability delegation is attenuation-only (`DelegatesFrom`) | `SeLe4n/CapBackedArena.lean` |
| Delegated subtree's live bytes stay within the root quota (`subtree_used_le_quota`) | `SeLe4n/CapBackedArena.lean` |
| Coupled alloc/free preserves combined invariants | `SeLe4n/Refinement.lean` |
| Exact byte accounting (`ArenaQuotaExact`) | `SeLe4n/Refinement.lean` |
| SMP correctness (every interleaving) | `SeLe4n/SMP.lean` |
| RSEQ abort safety | `SeLe4n/ClientRuntime.lean` |
| Stats non-interference | `SeLe4n/InformationFlow.lean` |
| Bundle inhabitation (non-vacuity) | `SeLe4n/Refinement.lean` |

See [`lean/README.md`](lean/README.md) for the full model charter and the
seLe4n bridge details.

## Testing

```sh
cargo xtask test                           # all test kinds
cargo xtask test --kind unit               # per-crate unit tests
cargo xtask test --kind prop               # proptest (property-based)
cargo xtask test --kind diff               # differential: trace replay vs Lean oracle
cargo xtask test --kind fuzz               # cargo-fuzz (nightly)
cargo xtask test --kind loom               # loom model-checking (nightly)
cargo xtask test --kind tsan               # ThreadSanitizer (nightly)
cargo xtask test --kind rseq               # RSEQ equivalence (native arm64)
cargo xtask abi-test                       # C/C++ ABI harness
cargo xtask lean                           # lake build + lake exe check
cargo xtask bench                          # criterion micro-benchmarks (non-gating)
```

Cross-crate integration tests cover the full C ABI, C23 sized frees, the
extended API, errno discipline, property-based classification/alignment,
`LiveModel` stream checking, the G-sim dual-backend gate, and the zero-size
policy matrix.

## Repository layout

```text
crates/          the Rust workspace (core, ABI, backends, arch, stats, control, test-support)
xtask/           the build/codegen/CI driver (dependency-free)
tools/           size-class-gen (the single source of truth) + trace-replay
lean/            the Lean 4 formal model + the seLe4n bridge (GPL-3.0-or-later)
tests/           cross-crate integration tests + C/C++ ABI harness
fuzz/            cargo-fuzz targets (nightly, standalone workspace)
include/         public + generated C/C++ headers
vendor/sele4n/   pinned seLe4n ABI mirror (GPL-3.0-or-later)
sele4n/          the seLe4n resource-server component (GPL-3.0-or-later)
bench/           benchmark config + results schema
profiles/        profile definitions (features, not forks)
docs/            CONVENTIONS.md, DECISIONS.md, ABI.md, mdbook
planning/        SPEC.md + 10 domain plans (24 workstreams, milestones M0–M9)
scripts/         setup_lean.sh, vendor_sele4n.sh
```

Each top-level directory carries a one-paragraph charter README.

## Licensing (D5)

Split-licensed. The standalone allocator **core is MIT** (see [`LICENSE`](LICENSE));
the **seLe4n-integration layer is GPL-3.0-or-later** (see [`sele4n/LICENSE`](sele4n/LICENSE)),
because it links/models the GPLv3 seLe4n ABI. The default `libtopomalloc`
artifact links no GPL code and is MIT; building with the `sele4n-sim` feature
produces a GPL combined work. The full split and SPDX policy are in
[`NOTICE`](NOTICE).

## Contributing

1. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`CLAUDE.md`](CLAUDE.md) first.
2. A fresh clone is green with `cargo xtask setup && cargo xtask ci`.
3. Every change must pass the [Definition of Done](CONTRIBUTING.md#definition-of-done-every-change) checklist.
4. Run `cargo xtask ci` before opening a PR — it is the exact sequence CI runs.

## Planning & design

- **Specification:** [`planning/SPEC.md`](planning/SPEC.md) — the full design spec (~100 sections).
- **Implementation plan:** [`planning/plans/README.md`](planning/plans/README.md) — overview + ten domain plans (24 workstreams, M0–M9).
- **Decisions, conventions, ABI:** [`docs/`](docs/)

## Security

TopoMalloc is pre-1.0 and under active development. See [`SECURITY.md`](SECURITY.md)
for the vulnerability reporting policy, scope, hardening profiles, and
security-review cadence.

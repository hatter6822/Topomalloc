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
state machine is modeled and proof-checked in Lean.

**Extent hooks & custom backing (W10)** ride on the same provider seam: the §23.2
`ExtentHooks` interface and the `HookProvider` adapter run the whole central path
over a user-supplied memory source / OS policy, with the §23.3 contracts enforced
(alignment / size / sub-range, no-overlap, dealloc-pairing, reentrancy — rejected,
never trusted) and the §23.4 "allocator correctness assumes hook correctness"
assumption modeled and proof-checked in Lean (`ExtentHooks.lean`). The full §22.2
per-arena `hooks` field is wired: `topo_arena_create_hooked` gives an arena its
**own** custom-backed region, isolated from every other arena's by construction
(§22.7), reachable from C through the `topo_extent_hooks_t` ABI.

**Hugepage-aware backend (W11)** rides on the §18.6 region-cache seam: the four
§19.2 components (HugeAllocator / HugeCache / HugePageFiller / RegionCache) as a
real, backend-agnostic placement subsystem over the provider seam. A
`HugePageFiller` packs sub-hugepage page-runs into hugepages over the nine §19.4
occupancy bins (each hugepage in **exactly one**, H-003) with packing-ordered,
scored placement carrying the request's hotness **and** lifetime hints (§19.3/
§19.5, no full scan); a provider-driven `HugePageBackend` implements the
`RegionCacheHook` the large path consults, so a medium/large request is packed or
served as a whole-hugepage run. It is wired **live** through the engine —
`Allocator::new_with_huge` (the `hugepage_optimized` configuration) routes every
medium/large allocation through the filler with the small/free paths unchanged —
and a §36.9 G-sim slice proves the *identical* outcome over POSIX and the seLe4n
simulator. Partial subrelease is guarded so it can **never** intersect a live
object (H-005, proved in Lean both over the `Range` geometry and as a per-page
state machine), gated by a real §19.6 cost/benefit test and §36.6
revoke-before-decommit; an empty-hugepage demand-reserve (`release_empty_excess`)
returns excess RSS. The §19.7 coverage metrics — plus the §19.4 per-bin
distribution (`hugepage.bin_counts`) — reconcile into the stats JSON, and
the §19.4 bin classification is pinned to the Lean model by a `lake exe check`
differential gate. Under the `hugepage-optimized` feature the live C
`malloc`/`free` already run over a `HugePageBackend`-backed engine
(`topo-abi`'s `build_posix_allocator`), gated so the default MIT artifact is
byte-for-byte the M1 extent path.

**Release controller (W12)** rides on the W11 mechanisms: a **pure, `no_std`,
host-driven** `ReleaseController` decides when and how much unused memory returns to
the OS (§20–§21). A `tick(now_ms, inputs)` pump samples the §21.2 observation vector,
classifies the §21.5 pressure mode (Normal/Soft/Hard/Emergency, with hysteresis;
allocation failure or cgroup-critical forces Emergency), computes the §21.4 demand
reserve — the anti-oscillation brake that withholds release proportional to recent
demand so freed memory is not faulted straight back (§21.1 R2), capped at the
`recent_peak` of releasable-free memory (a leaky peak-hold that relaxes toward current
free over a fixed horizon, decayed by age so it is tick-cadence independent, so a
transient free spike does not pin the reserve high) — and plans the §21.3
priority ladder (drain caches → release empty hugepages beyond the reserve → purge aged
dirty-not-on-hot → convert aged dirty→muzzy → subrelease cold-sparse → release aged
muzzy → emergency shrink) — where dirty and muzzy are each retained for reuse until
their `dirty_decay_ms`/`muzzy_decay_ms` interval elapses — each rung gated by mode and
the §36.11 latency class, rate-capped (§20.2) with a backlog (the max of carried-vs-
current desire, so a rate-capped persistent supply cannot make it diverge). It is wired
**live** through `HugePageBackend::release_tick`, which drives the
W11 `release_empty_excess` demand-reserve hook — so an idle backend returns its empty
hugepages to the OS while a churning one holds them back — identical over POSIX and the
seLe4n simulator (§36.9). The controller adds **no abstract transition**: it sequences
mechanisms already certified by the §21.6 release-safety theorem, so the proof stays
discharged. Pressure mode, backlog, and demand reserve reconcile into the stats JSON
and the `topo.release.*` control namespace.

**Topology awareness (W13)** completes plan 04's "Backend, Hugepages, Release &
Topology", and is **live**: a pure, `no_std` `Topology` snapshot models the §15
`CPU → LLC → NUMA node` hierarchy, built from Linux sysfs
(`topo-backend-posix::discover_topology`), **always falling back to a conservative single
domain** on inconsistent data and densely renumbering sparse OS node ids *and* LLC-domain ids
(no phantom node or LLC domain; the raw node id is kept for `mbind`) (§15.2). `preferred_node` is the §15.3/§15.5 placement
decision over the NUMA policy (local / bind / interleave / OS-default / arena), and a
`Rebalancer` plans nearest-donor → most-pressured-node moves that strand no one — moving
only a donor's **surplus** (free beyond its own demand) (§15.4). A **`NodeRouter`** makes
it all act on real allocations: one `mbind`-bound hugepage backend per node serving explicit
local/bind/interleave, **plus an unbound default backend** so `OS-default` allocations land
**first-touch** on the using thread's node (never pinned to node 0). `Local` tracks the real
running CPU (`sched_getcpu`), a full node spills to the **nearest** other node, and the host
drives the rebalancer, the §15.2 refresh, and W12 idle-release through a C
`topomalloc_numa_*` control surface. It is installed into the `hugepage-optimized` build
through the existing region-cache seam, so the **default path and a single-node host are
byte-for-byte unchanged**. Placement is policy, not a modeled transition, so it adds no
proof obligation. The router's bind-failure / rebalance / spillover counters and the
node/LLC counts reconcile into the stats JSON and the `topo.numa.*` control namespace.

**Lifetime, hotness & placement policy (W14)** completes plan 07's placement track. The
six §24.2 `LifetimeClass`es, the §24.4 `AllocationSiteProfile` record (stack id, a bounded
Space-Saving size-class summary, a right-censored lifetime histogram, a recency-weighted
hotness with a stability gate, EWMA alloc/free rates, sampled live bytes, confidence), and
the `SiteProfileTable` learning policy live in `crates/topo-core/src/placement.rs` — a
**pure, `no_std`, host-driven** object (the W12 `ReleaseController` pattern) that distils a
confident profile into the advisory `PlaceHints` (hotness + lifetime) the placement layers
group by (cold spans, short-lived together, long-lived-hot densely packed; §24.6–§24.8). The
**learn → place loop is closed live**: confident, consistent per-bucket consensus is
published into a lock-free `LearnedHints` table the allocation path reads (one atomic load),
so a *placement-unhinted* allocation adopts its site's learned profile — an explicit hint
always winning, and the placement unchanged when nothing is learned.
Grouping acts at **two layers**: the W11 hugepage filler (medium/large), and new §24.6/§24.7
`PlaceClass`-tagged **span pools** for small objects (cold / hot / short-lived spans, with an
availability fallback so grouping never causes a spurious OOM). To feed the policy from
*real* traffic, the **minimal W17-3 sampling slice** rides alongside (`sampling.rs` + the
`topo-abi` glue): a lock-free per-thread Poisson decision, an allocation-free
`libc::backtrace` capture into a fixed buffer, a `SampleBloom`-gated sampled-object lifecycle
with right-censored lifetimes, and a re-entrancy guard — **off by default**, enabled by
`$TOPOMALLOC_SAMPLE_RATE` or `topomalloc_profile_set_rate`. A counting-allocator test proves
the sampled path makes **zero** heap allocations (it never re-enters the allocator, §31.4),
and a criterion bench bounds its overhead. The **single non-negotiable** — the §24.5 safety
boundary — is a fixed, tested wall (over POSIX *and* the seLe4n simulator): a placement
decision (even from a wrong or adversarial learned profile) may change *where* an object
lands but **never** its size, alignment, validity, or free path. Placement is policy, not a
modeled transition (§2.4), so it adds **no Lean obligation**; the profiler's counters
reconcile into the stats JSON (`placement` block) and the `topo.placement.*` control
namespace. Front-end caches (M2) and the remaining M1 pieces land per the plan.

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
- Extent-hook contract differential (§23.3 alignment/size/sub-range checks)
- Hugepage-bin differential (§19.4 `classifyBin` ↔ Rust `classify_bin`)

**Selected headline theorems:**

| Property | Module |
|----------|--------|
| Size-class table covers all small requests | `Theorems/SizeClass.lean` |
| 14-clause WellFormed preservation (per transition) | `Theorems/*.lean` |
| Partial subrelease never strands a live object (H-005) | `HugePageFiller.lean` |
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

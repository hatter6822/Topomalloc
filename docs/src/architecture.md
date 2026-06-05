<!-- SPDX-License-Identifier: MIT -->
# Architecture

The allocator is a stack of layers over a single backing seam. The full diagram
and rationale are in the plan overview (§3); the M0 skeleton wires a thin
vertical slice through it.

```text
Public API:  C ABI (topomalloc_*) + Rust GlobalAlloc        topo-abi
Request classifier: small/medium/large, size class,         topo-core (classify)
                    align, arena, label, hints
Front/middle/back ends (M1+)                                 topo-core, plans 03/05
        ─────────────────  S E A M  ─────────────────
trait TopoBackingProvider                                    topo-core (backend)
  ├─ PosixBackingProvider  (ambient authority)               topo-backend-posix
  └─ Sele4nSim / Sele4nBackingProvider (capabilities)        topo-backend-sele4n
Formal: Lean model + seLe4n bridge                           lean/
```

## Crates

| Crate | Role | License |
|-------|------|---------|
| `topo-core` | classifier, size classes, the seam, the skeleton allocator (`no_std`) | MIT |
| `topo-abi` | C ABI + `GlobalAlloc` + runtime backend selector | MIT |
| `topo-backend-posix` | `PosixBackingProvider` (degenerate single-authority case) | MIT |
| `topo-backend-sele4n` | `Sele4nSim` + (M1) `Sele4nBackingProvider` | GPL-3.0-or-later |
| `topo-arch` | per-arch identity + fast-path mode (RSEQ lands in plan 05) | MIT |
| `topo-stats` | stats snapshot + additive JSON (`topomalloc_version`) | MIT |
| `topo-control` | configuration + control namespace | MIT |
| `topo-test-support` | trace parser, deterministic PRNG, executable model | MIT |

## Request classification (plan 03 W2)

`classify(size, align, flags)` (§A.1) is the first step on every allocation path.
It is total and overflow-safe (§9.7): it returns a `Request` or `None` (→ null /
`bad_alloc`), never panicking or wrapping. The decision is three-way (§9.2):

- **Small** — a size-class slab (`size_class`, a branch-light direct-mapped
  granule lookup). Over-aligned requests route to a sufficiently-aligned class or
  out to medium/large — never widening a shared slab's stride (§9.3 / §25.5).
- **Medium** — a page-rounded extent above the small path.
- **Large** — at/above the hugepage threshold (`HUGE_THRESHOLD`), hugepage-backed
  (plan 04). The medium/large split is decided on `max(size, align)`.

Advisory flags (§10.4) are decoded and **validated** by `RequestFlags` into a
structured `Hints` (zero, cache-bypass, guard, hugepage preference, lifetime,
hotness) plus arena routing; reserved bits and contradictory combinations fail
deterministically. The public C `TOPO_*` values map onto this internal layout at
the plan-06 API boundary. The Lean model mirrors the classifier and proves the
size-coverage and over-alignment-sufficiency invariants (plan 02 W1-4).

## Single source of truth (DD-1)

`tools/size-class-gen` reads the committed golden
`tools/size-class-gen/size-classes.json` and emits the Rust table, the C header,
and the Lean table — including the size-regime constants `HUGE_THRESHOLD` (the
medium/large boundary, authored) and `MAX_ALIGN` (the widest class alignment,
*derived* so over-alignment routing cannot drift) — all checked byte-for-byte in
CI (G-table). No table value is ever hand-edited. On the Lean side, `lake exe
check` replays the *generated* table through the well-formedness predicate
(`tableOk`) and the decidable lookup-coverage and §9.4-spacing gates
(`coversAllB`, `spacingOkB`); the §9.5/§33.4 coverage and §9.4 spacing theorems
are proved generally and discharged on the emitted table through those
predicates, so the shipped artifact provably cannot drift from a sound table.

## Formal model (`lean/`, plan 02 W1)

Lean 4 carries the allocator's abstract state machine (§33) and the seLe4n bridge
(§36). It is not on the hot path; it makes conformance real. The single-core
theorem set is complete:

- **State & invariants** — an ownership-map `State`, the 12-clause `WellFormed`
  predicate (§33.3), and the transitions (malloc/free/cache/central/release/arena)
  as total functions whose frame conditions are definitional.
- **§33.4 theorems** — every required theorem, proved and named exactly per the
  SPEC: well-formedness preservation, ownership conservation (consuming the §33.5
  RSEQ frame contract), span split/merge disjointness, pagemap soundness, release
  safety, the arena lifecycle, and size-class coverage.
- **§9.4/§9.5 size classes** — the spacing-dominated ratio bound, the
  alignment-dominated waste caveat, the slab-layout lemmas, and lookup coverage.
- **seLe4n bridge (§36, GPL-3.0-or-later)** — the abstraction relation and
  `TopoSeLe4nWellFormed`, the §36.6 backing-provider state machine, the §36.17
  families (authority/quota, provenance/release, destroy/label/scrub,
  per-core/stats/non-interference), and a *coupled* TopoMalloc↔seLe4n step whose
  whole invariant bundle is preserved together. The SMP/multicore forms are
  **proved** by interleaving semantics over all schedules (not staged).
- **Executable model (§33.7)** — a proof-grade trace-replay oracle that checks
  well-formedness at trace boundaries and flags injected violations; `lake exe
  check` runs it as a CI gate alongside the G-table check.

There are no `sorry`s; the only trusted axioms are the four §33.5 RSEQ primitives.
See [`lean/README.md`](../../lean/README.md) for the module map.

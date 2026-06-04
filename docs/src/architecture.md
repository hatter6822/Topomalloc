<!-- SPDX-License-Identifier: MIT -->
# Architecture

The allocator is a stack of layers over a single backing seam. The full diagram
and rationale are in the plan overview (§3); the M0 skeleton wires a thin
vertical slice through it.

```text
Public API:  C ABI (topomalloc_*) + Rust GlobalAlloc        topo-abi
Request classifier: size class, align, arena, label         topo-core (classify)
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

## Single source of truth (DD-1)

`tools/size-class-gen` reads the committed golden
`tools/size-class-gen/size-classes.json` and emits the Rust table, the C header,
and the Lean table — all checked byte-for-byte in CI (G-table). No table value
is ever hand-edited. The Lean model additionally proves `buildTable_eq_generated`:
its own parameterized table builder reproduces the emitted golden exactly, so the
generated artifact provably cannot drift from the model.

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
  `TopoSeLe4nWellFormed`, the §36.6 backing-provider state machine, and the
  single-core §36.17 families (authority/quota, provenance/release,
  destroy/label/scrub, per-core/stats/non-interference). SMP forms are staged as
  tracked V-004 debt.
- **Executable model (§33.7)** — a proof-grade trace-replay oracle that checks
  well-formedness at trace boundaries and flags injected violations; `lake exe
  check` runs it as a CI gate alongside the G-table check.

There are no `sorry`s; the only trusted axioms are the four §33.5 RSEQ primitives.
See [`lean/README.md`](../../lean/README.md) for the module map.

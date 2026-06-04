<!-- SPDX-License-Identifier: MIT -->
# `lean/` — the formal model (plan 02, workstream W1)

The Lean 4 abstract state machine and its proofs (SPEC §33, §36). Lean defines the
allocator's states, the well-formedness predicate, the transitions, and the theorems;
it is **not** on the production hot path (§33.1). It is built with `lake` (pinned by
`../lean-toolchain`) and driven by `cargo xtask lean` / `cargo xtask ci`.

The single-core theorem set is **complete**: every §33.4 theorem and every single-core
§36.17 bridge family is proved (named exactly per the SPEC for traceability). The
SMP/per-core extensions (W1-14) are staged as tracked V-004 refinement debt, which
§36.17 explicitly permits.

## Core model (MIT)

| Module | Charter | WU |
|--------|---------|----|
| `TopoMalloc/Types.lean` | `Range` geometry, `Owner` (all SPEC owners), ids (§33.2) | W1-1 |
| `TopoMalloc/SizeClass.lean` | `SizeClassRow` predicate; `Params`/`buildTable`; the §9.4/§9.5 proofs | W1-4a–d |
| `TopoMalloc/Generated/SizeClasses.lean` | **generated** table (DO NOT EDIT) — single source of truth (DD-1) | — |
| `TopoMalloc/State.lean` | abstract `State`, ownership map, the `setOwner` frame primitive | W1-5 |
| `TopoMalloc/WellFormed.lean` | the 12 named `WellFormed` clauses (§33.3) + their preservation lemmas | W1-3 |
| `TopoMalloc/Transitions.lean` | malloc/free/cache/central/release/arena as **total** functions | W1-5 |
| `TopoMalloc/Rseq.lean` | the RSEQ contract — trusted primitive + frame condition (§33.5) | W1-7 |
| `TopoMalloc/Theorems/*.lean` | one file per §33.4 theorem family | W1-4e, W1-6, W1-8, W1-9 |
| `TopoMalloc/Exec.lean` | executable model + trace replay (§33.7); flags injected violations | W1-10 |
| `Check.lean` | `lake exe check`: the G-table gate + the trace-oracle gate | — |

### The §33.4 theorem set (`Theorems/`)

`SizeClass.lean` (`size_class_table_covers_all_small_requests` + `buildTable_eq_generated`,
the machine-checked single-source tie), `Malloc.lean`, `Free.lean`, `Cache.lean`
(refill/flush ownership conservation, consuming the W1-7 frame), `Central.lean`,
`Span.lean`, `Pagemap.lean`, `Release.lean`, `Arena.lean`.

## seLe4n bridge (GPL-3.0-or-later, D5)

The bridge models the GPLv3 seLe4n system, so it is GPL-3.0-or-later, kept separate
from the MIT core. It imports TopoMalloc's model and seLe4n's *public* shapes only, and
**builds without seLe4n** (no seLe4n imports). `TopoMalloc/SeLe4n.lean` is the umbrella.

| Module | Charter | WU |
|--------|---------|----|
| `SeLe4n/CapBackedArena.lean` | capability-backed arenas, rights/quota/label attenuation (§36.4) | W1-11/12a |
| `SeLe4n/UntypedProvider.lean` | the §36.6 backing-provider state machine + provenance | W1-11b |
| `SeLe4n/VSpaceProvider.lean` | VSpace window map/unmap contract (§36.8.1) | W1-11 |
| `SeLe4n/CSpaceProvider.lean` | CSlot reservation contract (§36.8.2) | W1-11 |
| `SeLe4n/Bridge.lean` | the abstraction relation `R` + `TopoSeLe4nWellFormed` (§36.3.3) | W1-11a/c |
| `SeLe4n/ResourceServer.lean` | `arena_cap_authorizes_alloc`, `arena_quota_preserved` | W1-12a |
| `SeLe4n/ClientRuntime.lean` | `client_cache_refines_server_authority`, `per_core_cache_abort_no_change` | W1-12a/d |
| `SeLe4n/InformationFlow.lean` | non-interference: `stats_observation_noninterference`, low-equivalence (§36.12) | W1-12d/13 |
| `SeLe4n/Refinement.lean` | provenance/release/destroy/label/scrub + `topo_step_preserves_sele4n_invariants` | W1-12b/c/d |
| `SeLe4n/SMP.lean` | per-core core-isolation lemma + the staged V-004 SMP debt list | W1-14 |

## Soundness

There are no `sorry`s and no `native_decide`. The only postulated axioms are the four
RSEQ primitives/contracts (§33.5) — the trusted hardware boundary, consistent (the
always-`abort` interpretation models them). Every §33.4 theorem rests only on Lean's
standard axioms (`propext`/`Quot.sound`/`Classical.choice`); `buildTable_eq_generated`
is fully constructive (no axioms). Verify with `#print axioms <thm>`.

## Building

```sh
cargo xtask lean       # lake build + lake exe check (best-effort if lake absent)
lake build && lake exe check
```

The setup script `../scripts/setup_lean.sh` installs the pinned toolchain (it needs
`curl` and `zstd`).

<!-- SPDX-License-Identifier: MIT -->
# `lean/` — the formal model (plan 02, workstream W1)

The Lean 4 abstract state machine and its proofs (SPEC §33, §36). Lean defines the
allocator's states, the well-formedness predicate, the transitions, and the theorems;
it is **not** on the production hot path (§33.1). It is built with `lake` (pinned by
`../lean-toolchain`) and driven by `cargo xtask lean` / `cargo xtask ci`.

The single-core **and** SMP theorem sets are complete: every §33.4 theorem, every
§36.17 bridge family (single-core *and* the SMP/multicore extensions), the §9.4/§9.5
size-class proofs for the real tuned 32 KiB table, and the §33.7 executable oracle.

## Soundness

No `sorry`, no `admit`, no `native_decide`. The only postulated axioms are the four
§33.5 RSEQ primitives/contracts (the trusted hardware boundary, consistent — the
always-`abort` interpretation models them). Every §33.4/§36.17 theorem rests only on
Lean's standard axioms (`propext`/`Quot.sound`/`Classical.choice`); the size-class
single-source check is fully constructive. Verify with `#print axioms <thm>`.

## Core model (MIT)

| Module | Charter | WU |
|--------|---------|----|
| `TopoMalloc/Types.lean` | `Range` geometry, `Owner` (all SPEC owners), ids (§33.2) | W1-1 |
| `TopoMalloc/SizeClass.lean` | `SizeClassRow` predicate; `Params`/`buildTable`; the §9.4/§9.5 proofs | W1-4 |
| `TopoMalloc/Generated/SizeClasses.lean` | **generated** tuned table (72 classes to 32 KiB) — single source (DD-1) | W1-4e |
| `TopoMalloc/State.lean` | abstract `State`, ownership map, the `setOwner` frame primitive | W1-5 |
| `TopoMalloc/WellFormed.lean` | the **14** named `WellFormed` clauses (§33.3 + §9.5/§16 backbones) + preservation | W1-3 |
| `TopoMalloc/Transitions.lean` | malloc/free/cache/central/release/arena as **total** functions | W1-5 |
| `TopoMalloc/ExtentState.lean` | the §20.1 extent physical-backing state machine + `canTransition` (pinned 1:1 to Rust) | W4-2d |
| `TopoMalloc/ExtentHooks.lean` | the §23.4 hook assumption: §23.3 contracts ⇒ alloc/split/merge/subrange preserve disjointness + tiling (tied to the real `WfRangesDisjoint`); §22.7 per-arena-region isolation; `hookContractGate` decidable checks | W10 |
| `TopoMalloc/Rseq.lean` | the RSEQ contract — trusted primitive + frame condition (§33.5) | W1-7 |
| `TopoMalloc/Theorems/*.lean` | one file per §33.4 family, incl. `Demo.lean` (a concrete non-empty witness) | W1-4e/6/8/9 |
| `TopoMalloc/Exec.lean` | executable model + §33.7 **text-grammar** trace replay; flags injected violations | W1-10 |
| `Check.lean` | `lake exe check`: the G-table gate, the trace-oracle gate, and the pagemap (W3-3d), §36.6 provider (`providerChainGate`) and §20.1 extent-state (`extentStateGate`, W4-2d) differentials | — |

### The §33.4 theorem set (`Theorems/`)

`SizeClass.lean` (`size_class_table_covers_all_small_requests` for the emitted tuned
table), `Malloc.lean`, `Free.lean`, `Cache.lean` (refill/flush conservation **preserving
the live set**, capacity-under-filling, and the fast paths consuming the W1-7 frame),
`Central.lean`, `Span.lean` (split/merge lifted to `State`, preserving the span-range
disjointness clause), `Pagemap.lean`, `Release.lean`, `Arena.lean`, and `Demo.lean` (all
14 clauses jointly satisfiable on a real state + a malloc→free round-trip).

## seLe4n bridge (GPL-3.0-or-later, D5)

The bridge models the GPLv3 seLe4n system, so it is GPL-3.0-or-later, kept separate
from the MIT core. It imports TopoMalloc's model and seLe4n's *public* shapes only, and
**builds without seLe4n**. `TopoMalloc/SeLe4n.lean` is the umbrella.

| Module | Charter | WU |
|--------|---------|----|
| `SeLe4n/CapBackedArena.lean` | capability-backed arenas, rights/quota/label attenuation (§36.4); `ArenaTree` reservation model + `subtree_used_le_quota` (tree-wide quota bound) | W1-11/12a |
| `SeLe4n/UntypedProvider.lean` | the §36.6 backing-provider state machine + provenance | W1-11b |
| `SeLe4n/VSpaceProvider.lean` / `CSpaceProvider.lean` | VSpace/CSlot provider contracts (§36.8) | W1-11 |
| `SeLe4n/Bridge.lean` | the abstraction relation `R` (real provenance) + `TopoSeLe4nWellFormed` (§36.3.3) | W1-11a/c |
| `SeLe4n/ResourceServer.lean` | `arena_cap_authorizes_alloc`, `arena_quota_preserved` | W1-12a |
| `SeLe4n/ClientRuntime.lean` | `client_cache_refines_server_authority`, `per_core_cache_abort_no_change` | W1-12a/d |
| `SeLe4n/InformationFlow.lean` | non-interference: `stats_observation_noninterference`, low-equivalence (§36.12) | W1-12d/13 |
| `SeLe4n/Refinement.lean` | **coupled** alloc/free steps (topo+sys move together), destroy revokes backings, label partition (incl. `free`), provenance/release/scrub families | W1-12b/c/d |
| `SeLe4n/SMP.lean` | the **multicore** model: conservation/isolation/abort/non-interference over *every* interleaving | W1-14 |

The coupled `allocStep`/`freeStep` make the TopoMalloc malloc/free and the seLe4n quota
accounting one step (`allocStep_preserves_invariants`/`freeStep_preserves_invariants` each
preserve the whole `TopoSeLe4nWellFormed` bundle — well-formedness, quota, the abstraction
relation, the label partition, **and exact byte accounting** together). The accounting is
not merely bounded (`used ≤ quota`): `ArenaQuotaExact` pins each arena's `used` to the
*actual* live bytes (`State.arenaLiveBytes`), and the steps preserve it because each ties
the charge/credit to the allocated slot's own arena and size (and pops/frees a
non-live/live slot) — so the model cannot under- or over-charge, nor double-credit a free.
`topoSeLe4nWellFormed_empty` witnesses that this whole bundle is *inhabited* (the empty
combined state satisfies it), so the preservation theorems are not vacuous. This is the
simulation the bridge is about. `SMP.lean`
proves the §36.17 SMP forms by interleaving semantics: with the RSEQ contract giving atomic
per-core steps, correctness is "the invariant holds for every schedule", by induction over
the schedule.

## What is proved vs. deliberately abstracted

The model is faithful but, by design, abstract. Documented simplifications (sound, not
gaps — each is a stated modelling choice, not an unproven claim):

- **Ownership = membership.** Caches/lists are owners in one block list, so clauses 3/4/6
  ("caches/bitmaps agree with ownership") are rendered as size-class/arena consistency and
  occupancy counts rather than reconciliation against a separate bitmap structure. Block↔span
  class consistency *is* enforced (clause 14, `WfBlockSpanClass`).
- **Pagemap/hugepage (clauses 7/8)** are modelled as containment + entry agreement; a full
  hugepage occupancy-count metric (§19.7) is not modelled. `arenaDestroy` filters the
  pagemap so no entry dangles to a removed span.
- **RSEQ is a trusted primitive (§33.5).** Its success contract pins the successor to the
  exact owner relabel (`s' = setOwner p …`), so it frames *all* non-owner geometry; the
  remaining trust is the hardware↔model refinement (the asm itself) — open question 7 /
  W1-14 per-arch verification. The plan 05 **W7** implementation
  ([`topo-arch/src/rseq/`](../crates/topo-arch/src/rseq/)) discharges this contract
  *empirically*: a forced-migration differential test asserts the per-arch assembly makes
  exactly the object movements the axiom permits (no lost/duplicated object vs. the locked
  baseline), and the abort path leaves allocator-visible state unchanged
  (`per_core_cache_abort_no_change`, exercised by the pinned-core mode too).
- **The executable oracle** (`Exec.lean`) checks live **range-disjointness** on the §33.7
  trace — an allocation whose `[ptr, ptr+usable_size)` range overlaps a live object is
  rejected (`replay_disjoint`), not merely an equal base address. The host-side Rust oracle
  (`topo_test_support::LiveModel`) performs the *same* range check, so the two replayers stay
  in differential lockstep (plan 08). A full State-level decidable `WellFormed` over a State
  reconstructed from the trace is future work (the trace grammar does not carry full block
  structure). The structural `WellFormed` clauses are decidable and `decide`-checked on
  `demoState`.

## Building

```sh
cargo xtask lean       # lake build + lake exe check (best-effort if lake absent)
lake build && lake exe check
```

The setup script `../scripts/setup_lean.sh` installs the pinned toolchain (`curl` + `zstd`).

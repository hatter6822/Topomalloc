-- SPDX-License-Identifier: MIT
/-
§25.4 / §33.4 theorems for the realloc **move** path (plan 06 W8-1a, W15-2).

* `realloc_move_window_keeps_old_live`
* `realloc_move_preserves_wellformed`

`reallocMove` is the composition `free (malloc s bNew) bOld o` — allocate
first, free last (§25.4). The two theorems pin the properties the runtime
relies on:

1. **The copy window is sound.** Between the allocation and the free the old
   block is still live while the new block is already live, so the §8.3
   live-disjointness clause of well-formedness applies to the pair — the
   `memcpy` in the runtime reads a live object into a *disjoint* live object.

2. **The composition preserves well-formedness**, by chaining the proved
   `malloc` and `free` preservation theorems through the intermediate state.

The third §25.1 obligation — *failure preserves the original* — needs no
theorem: the runtime's failure path returns before any transition runs, so
the abstract state is literally unchanged (`reallocMove` is only invoked once
the new slot exists). The definition's ordering is what makes that argument
purely syntactic; see `Transitions.lean`.
-/
import TopoMalloc.Theorems.Malloc
import TopoMalloc.Theorems.Free

namespace TopoMalloc

open State

/-- **`realloc_move_window_keeps_old_live` (§25.4).** At the instant the new
block has become live (after `malloc`, before `free`), the original block is
*still* live. With `WellFormed.rangesDisjoint` (§8.3) over the intermediate
state, the two live blocks are range-disjoint — exactly the precondition the
runtime's `copy_nonoverlapping` needs. The allocate-before-free order is
load-bearing: the opposite order has no state in which both are live. -/
theorem realloc_move_window_keeps_old_live (s : State) (bNew bOld : BlockId)
    (hne : bOld ≠ bNew) (hold : s.ownerOf bOld = some Owner.live)
    (hnew : (s.blockById bNew).isSome) :
    (malloc s bNew).ownerOf bOld = some Owner.live ∧
      (malloc s bNew).ownerOf bNew = some Owner.live := by
  constructor
  · rw [malloc, setOwner_ownerOf_ne s bNew Owner.live hne]
    exact hold
  · exact setOwner_ownerOf_self s bNew Owner.live hnew

/-- **`realloc_move_preserves_wellformed` (§33.4 / §25.4).** The move-realloc
composition preserves every well-formedness clause: `malloc` preservation
carries the state to the intermediate point, `free` preservation carries it
home. The free-side consistency hypotheses are stated against the
intermediate state (`malloc s bNew`), which is where that transition runs. -/
theorem realloc_move_preserves_wellformed (s : State) (bNew bOld : BlockId) (o : Owner)
    (hwf : WellFormed s)
    (hnonlive : s.ownerOf bNew ≠ some Owner.live)
    (hcommitted : ∀ blk ∈ s.blocks, blk.id = bNew → ∀ r ∈ s.released,
      Range.Disjoint r blk.range)
    (hne_live : o ≠ Owner.live)
    (hne_cpu : ∀ cpu sc, o ≠ Owner.cpuCache cpu sc)
    (hcache : ∀ blk ∈ (malloc s bNew).blocks, blk.id = bOld →
      ∀ sc, o.cacheSizeClass? = some sc → blk.sc = sc)
    (hcentral : ∀ blk ∈ (malloc s bNew).blocks, blk.id = bOld →
      ∀ a, o.arena? = some a →
        ∃ d ∈ (malloc s bNew).spans, d.id = blk.span ∧ d.arena = a) :
    WellFormed (reallocMove s bNew bOld o) := by
  unfold reallocMove
  exact free_preserves_wellformed_for_valid_pointer (malloc s bNew) bOld o
    (malloc_preserves_wellformed s bNew hwf hnonlive hcommitted)
    hne_live hne_cpu hcache hcentral

end TopoMalloc

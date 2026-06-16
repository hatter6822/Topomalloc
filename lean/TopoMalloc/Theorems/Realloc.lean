-- SPDX-License-Identifier: MIT
/-
§25.3/§25.4 / §33.4 theorems for the realloc **move** path (plan 06 W8-1a, W15-2)
and the in-place **shrink** obligation (W15-3b).

* `realloc_move_window_keeps_old_live`
* `realloc_move_preserves_wellformed`
* `realloc_shrink_inplace_tail_tiles_disjointly` — the W15-3b in-place-shrink
  geometric obligation, discharged against the certified `extent_split` geometry
  (`span_split_preserves_disjointness`). See its docstring for why the in-place
  large shrink introduces no new abstract transition (it sequences the certified
  extent split + free, the W12 pattern — not a `reallocMove`, whose copy window
  needs two disjoint live ranges that two same-base ranges can never be).

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
import TopoMalloc.Theorems.Span

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

/-- **`realloc_shrink_inplace_tail_tiles_disjointly` (§25.3, plan 06 W15-3b) — the
in-place-shrink geometric obligation.** The runtime returns a medium/large
allocation's tail pages by *splitting its backing extent* at the page-rounded new
size `k ≤ len` and freeing the upper half (`LargeAllocator::shrink`).

This adds **no new abstract transition**, and the reason is specific — not a
hand-wave. A large allocation is **not** a core `Block`: clause 12
(`WfSlabLayout`) keys every block to a size class, so the §33.3 block state
machine (malloc/free/realloc-move over `s.blocks`) models only the small-object
slab path. A large allocation is modelled in the **extent backend**, where the
split is the certified `extent_split` (§18.3) and the tail-free is the certified
`extent free` (§20.1 `ExtentState`, pinned by the `extent state machine` gate).
The in-place shrink simply **sequences** those two — the same shape as the W12
release controller sequencing the certified release transition (`release_to_os_*`),
**not** the W13/W14 case (placement policy that is invisible to the abstract
state). So crucially it is *not* `reallocMove`: that needs the old and new blocks
**simultaneously live and disjoint** (the copy window), which two same-base ranges
can never be — the shrink instead frees the tail, so nothing overlapping is ever
live at once.

The one geometric fact the sequencing rests on is discharged here against the
certified `span_split_preserves_disjointness`: the **kept prefix** `splitLeft`
stays disjoint from every range `x` already disjoint from the whole extent (so the
still-live front of the allocation never aliases its neighbours), **and** the kept
prefix and the **returned tail** `splitRight` never overlap (so a later
reallocation of the freed tail can never alias the live prefix — the
retire-before-free ordering's geometric premise). Naming it makes the
"composition of certified mechanisms" claim a machine-checked artifact rather than
a prose assertion. -/
theorem realloc_shrink_inplace_tail_tiles_disjointly
    (extent x : Range) (k : Nat) (hk : k ≤ extent.len) (hx : Range.Disjoint x extent) :
    Range.Disjoint x (splitLeft extent k) ∧
      Range.Disjoint (splitLeft extent k) (splitRight extent k) :=
  let h := span_split_preserves_disjointness extent x k hk hx
  ⟨h.1, h.2.2⟩

end TopoMalloc

-- SPDX-License-Identifier: MIT
/-
The §22.5/§22.6 arena-lifecycle theorems (plan 02 W1-9, mirroring plan 06 W9-4).

* `arena_reset_invalidates_only_target_arena`
* `arena_destroy_preserves_other_arenas`

A block's arena is read through its span (`spanArena`). Reset relabels exactly the
target arena's slots to a non-live backend owner and leaves every other slot
verbatim; destroy additionally drops the target's spans and slots, leaving other
arenas' spans and slots present (the §22.7 isolation invariant in action).
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`arena_reset_invalidates_only_target_arena` (§33.4 / §22.5).** Reset leaves every
slot of a *different* arena untouched, and makes every slot of the target arena
non-live (it becomes a backend free slot). -/
theorem arena_reset_invalidates_only_target_arena (s : State) (a : ArenaId) :
    (∀ blk ∈ s.blocks, spanArena s blk.span ≠ some a → blk ∈ (arenaReset s a).blocks) ∧
      (∀ blk ∈ (arenaReset s a).blocks, spanArena s blk.span = some a →
        blk.owner ≠ Owner.live) := by
  constructor
  · intro blk hblk hcond
    rw [arenaReset_blocks, List.mem_map]
    exact ⟨blk, hblk, by rw [if_neg hcond]⟩
  · intro blk' hblk' hcond
    rw [arenaReset_blocks, List.mem_map] at hblk'
    obtain ⟨blk0, _, rfl⟩ := hblk'
    -- the span field is unchanged by the relabel, so the target test is on blk0
    by_cases h0 : spanArena s blk0.span = some a
    · rw [if_pos h0]; simp
    · rw [if_neg h0] at hcond ⊢; exact absurd hcond h0

/-- **`arena_destroy_preserves_other_arenas` (§33.4 / §22.6, §22.7).** Destroying arena
`a` keeps every block and span belonging to a *different* arena. -/
theorem arena_destroy_preserves_other_arenas (s : State) (a : ArenaId) :
    (∀ blk ∈ s.blocks, spanArena s blk.span ≠ some a → blk ∈ (arenaDestroy s a).blocks) ∧
      (∀ d ∈ s.spans, d.arena ≠ a → d ∈ (arenaDestroy s a).spans) := by
  constructor
  · intro blk hblk hcond
    rw [arenaDestroy_blocks, List.mem_filter]
    exact ⟨hblk, by simp [hcond]⟩
  · intro d hd hcond
    rw [arenaDestroy_spans, List.mem_filter]
    exact ⟨hd, by simp [hcond]⟩

end TopoMalloc

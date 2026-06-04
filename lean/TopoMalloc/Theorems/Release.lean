-- SPDX-License-Identifier: MIT
/-
The §21.6 release-safety theorem (plan 02 W1-8c).

* `release_to_os_preserves_live_objects`

`releaseToOs` only records a range as released; it touches no block, so every live
slot stays live and keeps its geometry. `release_to_os_preserves_wellformed` adds
that, when the released range avoids every live block (its §21.6 precondition),
well-formedness is preserved (clause 10 now also covers the new range).
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`release_to_os_preserves_live_objects` (§21.6 / §33.4).** Releasing a range to
the OS leaves every previously-live slot live (and at the same address). -/
theorem release_to_os_preserves_live_objects (s : State) (r : Range) :
    ∀ b, s.IsLive b → (releaseToOs s r).IsLive b := by
  intro b hlive
  -- `releaseToOs` changes only `released`; `ownerOf` reads `blocks`, unchanged.
  simpa [releaseToOs, State.IsLive, State.ownerOf, State.blockById] using hlive

/-- Release also preserves well-formedness when the released range is disjoint from
every live slot (the §21.6 precondition: you do not release live memory). -/
theorem release_to_os_preserves_wellformed (s : State) (r : Range) (hwf : WellFormed s)
    (hsafe : ∀ blk ∈ s.blocks, blk.owner = Owner.live → Range.Disjoint r blk.range) :
    WellFormed (releaseToOs s r) := by
  -- `releaseToOs` changes only `released`; every other clause reads unchanged
  -- (definitionally equal) fields, and clause 10 is re-established for `r :: …`.
  exact
    { rangesDisjoint := hwf.rangesDisjoint
      uniqueOwner := hwf.uniqueOwner
      cachesConsistent := hwf.cachesConsistent
      centralConsistent := hwf.centralConsistent
      objectsFitSpans := hwf.objectsFitSpans
      bitmapsAgree := hwf.bitmapsAgree
      pagemapAgrees := hwf.pagemapAgrees
      hugepagesAgree := hwf.hugepagesAgree
      arenaUnique := hwf.arenaUnique
      releasedNoLive := by
        intro r' hr' blk hblk hlive
        rcases List.mem_cons.mp hr' with rfl | hr'
        · exact hsafe blk hblk hlive
        · exact hwf.releasedNoLive r' hr' blk hblk hlive
      cacheCapacities := hwf.cacheCapacities
      slabLayout := hwf.slabLayout
      spansDisjoint := hwf.spansDisjoint
      blockSpanClass := hwf.blockSpanClass
      uniqueIds := hwf.uniqueIds }

end TopoMalloc

-- SPDX-License-Identifier: MIT
/-
§33.4 theorems for `free` (plan 02 W1-6b).

* `free_preserves_wellformed_for_valid_pointer`
* `free_removes_liveness_and_adds_exactly_one_free_owner`

`free` returns a live slot to a free owner `o` (its arena's central list). `o` is
neither `live` (so clause 10 holds — the live set only shrinks) nor a per-CPU cache
(so clause 11's count cannot rise); the cache/central consistency obligations carry
`o`'s size class and arena, as the central-list-per-(arena, class) discipline
guarantees.
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`free_preserves_wellformed_for_valid_pointer` (§33.4).** Returning a slot to a
free owner preserves well-formedness, given that `o` is a genuine free owner (not
`live`, not a per-CPU cache) consistent with the slot's class and arena. -/
theorem free_preserves_wellformed_for_valid_pointer (s : State) (b : BlockId) (o : Owner)
    (hwf : WellFormed s) (hne_live : o ≠ Owner.live)
    (hne_cpu : ∀ cpu sc, o ≠ Owner.cpuCache cpu sc)
    (hcache : ∀ blk ∈ s.blocks, blk.id = b → ∀ sc, o.cacheSizeClass? = some sc → blk.sc = sc)
    (hcentral : ∀ blk ∈ s.blocks, blk.id = b → ∀ a, o.arena? = some a →
      ∃ d ∈ s.spans, d.id = blk.span ∧ d.arena = a) :
    WellFormed (free s b o) := by
  unfold free
  exact
    { rangesDisjoint := WfRangesDisjoint.setOwner s b o hwf.rangesDisjoint
      uniqueOwner := WfUniqueOwner.setOwner s b o hwf.uniqueOwner
      cachesConsistent := WfCachesConsistent.setOwner s b o hwf.cachesConsistent hcache
      centralConsistent := WfCentralConsistent.setOwner s b o hwf.centralConsistent hcentral
      objectsFitSpans := WfObjectsFitSpans.setOwner s b o hwf.objectsFitSpans
      bitmapsAgree := WfBitmapsAgree.setOwner s b o hwf.bitmapsAgree
      pagemapAgrees := WfPagemapAgrees.setOwner s b o hwf.pagemapAgrees
      hugepagesAgree := WfHugepagesAgree.setOwner s b o hwf.hugepagesAgree
      arenaUnique := WfArenaUnique.setOwner s b o hwf.arenaUnique
      releasedNoLive := WfReleasedNoLive.setOwner_of_ne_live s b o hne_live hwf.releasedNoLive
      cacheCapacities := fun cpu sc =>
        Nat.le_trans (countOwned_setOwner_le_of_ne s b o (hne_cpu cpu sc).symm)
          (hwf.cacheCapacities cpu sc)
      slabLayout := WfSlabLayout.setOwner s b o hwf.slabLayout
      spansDisjoint := WfSpansDisjoint.setOwner s b o hwf.spansDisjoint }

/-- **`free_removes_liveness_and_adds_exactly_one_free_owner` (§33.4).** After `free`,
the slot is no longer live, it is owned by the free owner `o`, and **only** that slot
changes owner (exactly one free owner is added; the frame condition). -/
theorem free_removes_liveness_and_adds_exactly_one_free_owner (s : State) (b : BlockId) (o : Owner)
    (hb : (s.blockById b).isSome) (hne_live : o ≠ Owner.live) :
    (free s b o).ownerOf b = some o ∧ ¬ (free s b o).IsLive b ∧
      (∀ q, q ≠ b → (free s b o).ownerOf q = s.ownerOf q) := by
  unfold free
  refine ⟨setOwner_ownerOf_self s b o hb, ?_, fun q hq => setOwner_ownerOf_ne s b o hq⟩
  rw [State.IsLive, setOwner_ownerOf_self s b o hb]
  intro hcon; injection hcon with hcon; exact hne_live hcon

end TopoMalloc

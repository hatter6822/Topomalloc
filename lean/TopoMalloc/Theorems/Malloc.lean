-- SPDX-License-Identifier: MIT
/-
§33.4 theorems for `malloc` (plan 02 W1-6a).

* `malloc_preserves_wellformed`
* `malloc_success_returns_aligned_sufficient_disjoint_object`

`malloc` relabels one cached slot to `live`. Preservation of the eight
owner-independent clauses is automatic; the four owner-dependent clauses are
discharged from: `live` is neither a cache owner (clause 3) nor arena-qualified
(clause 4), `live` is not a per-CPU cache so capacity cannot rise (clause 11), and
the popped slot is allocated from *committed* memory — disjoint from every released
range (clause 10, the `hcommitted` hypothesis the implementation guarantees, §20).
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`malloc_preserves_wellformed` (§33.4).** Popping a free slot to `live`
preserves every well-formedness clause, given that the slot's range is disjoint
from all released memory (it is committed). -/
theorem malloc_preserves_wellformed (s : State) (b : BlockId) (hwf : WellFormed s)
    (hcommitted : ∀ blk ∈ s.blocks, blk.id = b → ∀ r ∈ s.released, Range.Disjoint r blk.range) :
    WellFormed (malloc s b) := by
  unfold malloc
  exact
    { rangesDisjoint := WfRangesDisjoint.setOwner s b Owner.live hwf.rangesDisjoint
      uniqueOwner := WfUniqueOwner.setOwner s b Owner.live hwf.uniqueOwner
      cachesConsistent := WfCachesConsistent.setOwner s b Owner.live hwf.cachesConsistent
          (by intro blk _ _ sc hsc; simp [Owner.cacheSizeClass?] at hsc)
      centralConsistent := WfCentralConsistent.setOwner s b Owner.live hwf.centralConsistent
          (by intro blk _ _ a ha; simp [Owner.arena?] at ha)
      objectsFitSpans := WfObjectsFitSpans.setOwner s b Owner.live hwf.objectsFitSpans
      bitmapsAgree := WfBitmapsAgree.setOwner s b Owner.live hwf.bitmapsAgree
      pagemapAgrees := WfPagemapAgrees.setOwner s b Owner.live hwf.pagemapAgrees
      hugepagesAgree := WfHugepagesAgree.setOwner s b Owner.live hwf.hugepagesAgree
      arenaUnique := WfArenaUnique.setOwner s b Owner.live hwf.arenaUnique
      releasedNoLive := WfReleasedNoLive.setOwner_live s b hwf.releasedNoLive hcommitted
      cacheCapacities := fun cpu sc =>
        Nat.le_trans (countOwned_setOwner_le_of_ne s b Owner.live (by simp))
          (hwf.cacheCapacities cpu sc)
      slabLayout := WfSlabLayout.setOwner s b Owner.live hwf.slabLayout }

/-- **`malloc_success_returns_aligned_sufficient_disjoint_object` (§33.4).** The
slot handed out is live; its range is at least as large as the request and aligned
to the request's alignment; and it is disjoint from every other block (hence every
other live object). The size/alignment bounds are stated against the slot's own
size-class row, which the classifier guarantees the request fits. -/
theorem malloc_success_returns_aligned_sufficient_disjoint_object
    (s : State) (b : BlockId) (hwf : WellFormed s)
    (blk : Block) (hb : s.blockById b = some blk)
    (req reqAlign : Nat)
    (hreq : ∀ row, sizeClassRow blk.sc = some row → req ≤ row.size)
    (hralign : ∀ row, sizeClassRow blk.sc = some row → reqAlign ∣ row.align) :
    (malloc s b).ownerOf b = some Owner.live ∧
      req ≤ blk.range.len ∧
      reqAlign ∣ blk.range.base ∧
      (∀ blk', blk' ∈ s.blocks → blk'.id ≠ b → Range.Disjoint blk.range blk'.range) := by
  have hmem : blk ∈ s.blocks := List.mem_of_find?_eq_some hb
  have hbid : blk.id = b := by
    have h := List.find?_some hb; simp only [decide_eq_true_eq] at h; exact h
  obtain ⟨row, hrow, hlen, hdvd⟩ := hwf.slabLayout blk hmem
  refine ⟨?_, ?_, ?_, ?_⟩
  · -- live
    unfold malloc
    exact setOwner_ownerOf_self s b Owner.live (by rw [hb]; rfl)
  · -- sufficient: req ≤ row.size = blk.range.len
    rw [hlen]; exact hreq row hrow
  · -- aligned: reqAlign ∣ row.align ∣ blk.range.base
    exact Nat.dvd_trans (hralign row hrow) hdvd
  · -- disjoint from every other block
    intro blk' hblk' hne'
    exact hwf.disjoint_of_id_ne hmem hblk' (by rw [hbid]; exact fun h => hne' h.symm)

end TopoMalloc

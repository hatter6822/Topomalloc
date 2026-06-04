-- SPDX-License-Identifier: MIT
/-
§33.4 well-formedness theorems for the central-list batch operations (plan 02 W1-6).

* `central_batch_remove_preserves_wellformed` (central → transfer cache)
* `central_batch_insert_preserves_wellformed` (transfer cache → central)

Both move a batch between the central list and a transfer cache, neither of which is
a per-CPU cache, so the capacity clause is untouched. Remove targets a transfer cache
keyed by class `sc` (clause 3 needs the batch to be class `sc`); insert targets a
central list keyed by arena `a` (clause 4 needs every batch block to belong to `a`).
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`central_batch_remove_preserves_wellformed` (§33.4).** Moving a class-`sc` batch
from the central list into a transfer cache preserves well-formedness. -/
theorem central_batch_remove_preserves_wellformed
    (s : State) (batch : List BlockId) (domain : Nat) (sc : SizeClassId) (hwf : WellFormed s)
    (hsc : ∀ blk ∈ s.blocks, blk.id ∈ batch → blk.sc = sc) :
    WellFormed (centralBatchRemove s batch domain sc) := by
  unfold centralBatchRemove
  exact
    { rangesDisjoint := relabelAll_rangesDisjoint s batch _ hwf.rangesDisjoint
      uniqueOwner := relabelAll_uniqueOwner s batch _ hwf.uniqueOwner
      cachesConsistent := relabelAll_cachesConsistent _ s batch hwf.cachesConsistent
          (by intro scT hscT blk hblk hmem
              simp only [Owner.cacheSizeClass?, Option.some.injEq] at hscT
              subst hscT; exact hsc blk hblk hmem)
      centralConsistent := relabelAll_centralConsistent _ s batch hwf.centralConsistent
          (by intro a ha; simp [Owner.arena?] at ha)
      objectsFitSpans := relabelAll_objectsFitSpans s batch _ hwf.objectsFitSpans
      bitmapsAgree := relabelAll_bitmapsAgree s batch _ hwf.bitmapsAgree
      pagemapAgrees := relabelAll_pagemapAgrees s batch _ hwf.pagemapAgrees
      hugepagesAgree := relabelAll_hugepagesAgree s batch _ hwf.hugepagesAgree
      arenaUnique := relabelAll_arenaUnique s batch _ hwf.arenaUnique
      releasedNoLive := relabelAll_releasedNoLive s batch _ (by simp) hwf.releasedNoLive
      cacheCapacities := fun cpu' sc' =>
        Nat.le_trans (relabelAll_countOwned_le_of_ne _ (by simp) s batch)
          (hwf.cacheCapacities cpu' sc')
      slabLayout := relabelAll_slabLayout s batch _ hwf.slabLayout
      spansDisjoint := relabelAll_spansDisjoint s batch _ hwf.spansDisjoint }

/-- **`central_batch_insert_preserves_wellformed` (§33.4).** Moving a batch belonging
to arena `a` from a transfer cache back into the central list preserves
well-formedness. -/
theorem central_batch_insert_preserves_wellformed
    (s : State) (batch : List BlockId) (a : ArenaId) (sc : SizeClassId) (hwf : WellFormed s)
    (hspan : ∀ blk ∈ s.blocks, blk.id ∈ batch → ∃ d ∈ s.spans, d.id = blk.span ∧ d.arena = a) :
    WellFormed (centralBatchInsert s batch a sc) := by
  unfold centralBatchInsert
  exact
    { rangesDisjoint := relabelAll_rangesDisjoint s batch _ hwf.rangesDisjoint
      uniqueOwner := relabelAll_uniqueOwner s batch _ hwf.uniqueOwner
      cachesConsistent := relabelAll_cachesConsistent _ s batch hwf.cachesConsistent
          (by intro scT hscT; simp [Owner.cacheSizeClass?] at hscT)
      centralConsistent := relabelAll_centralConsistent _ s batch hwf.centralConsistent
          (by intro a' ha'
              simp only [Owner.arena?, Option.some.injEq] at ha'
              subst ha'; exact hspan)
      objectsFitSpans := relabelAll_objectsFitSpans s batch _ hwf.objectsFitSpans
      bitmapsAgree := relabelAll_bitmapsAgree s batch _ hwf.bitmapsAgree
      pagemapAgrees := relabelAll_pagemapAgrees s batch _ hwf.pagemapAgrees
      hugepagesAgree := relabelAll_hugepagesAgree s batch _ hwf.hugepagesAgree
      arenaUnique := relabelAll_arenaUnique s batch _ hwf.arenaUnique
      releasedNoLive := relabelAll_releasedNoLive s batch _ (by simp) hwf.releasedNoLive
      cacheCapacities := fun cpu' sc' =>
        Nat.le_trans (relabelAll_countOwned_le_of_ne _ (by simp) s batch)
          (hwf.cacheCapacities cpu' sc')
      slabLayout := relabelAll_slabLayout s batch _ hwf.slabLayout
      spansDisjoint := relabelAll_spansDisjoint s batch _ hwf.spansDisjoint }

end TopoMalloc

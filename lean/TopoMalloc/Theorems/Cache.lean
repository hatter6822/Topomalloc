-- SPDX-License-Identifier: MIT
/-
§33.4 theorems for the per-CPU cache (plan 02 W1-6c, DD-2).

* `cache_refill_preserves_ownership_conservation`  (strengthened: live set preserved)
* `cache_flush_preserves_ownership_conservation`
* `cache_refill_preserves_wellformed`              (capacity preserved under *filling*)
* `malloc_fast_path_*` / `free_fast_path_*`         (consume the W1-7 RSEQ frame)

**Conservation (strengthened).** Refill/flush move a batch between the central list and
the per-CPU cache. Given that the batch was genuinely free (non-live), the operation (a)
creates/destroys no slot, (b) frames every block outside the batch, and (c) changes *no
address's liveness* — `relabelAll_isLive_iff`. (c) is the real safety content: a refill
cannot silently swallow a live object.

**Capacity under filling.** `cache_refill_preserves_wellformed` is the one transition that
*increases* a per-CPU cache; it preserves clause 11 given the implementation's capacity
guard (`hcap`), discharging the obligation the other transitions sidestep.

**Where W1-7 is load-bearing.** Refill/flush are the *slow* path (central ↔ cache), proved
from the batch frame. The W1-7 RSEQ contract governs the *fast* path (cache ↔ live), and is
consumed — non-removably — by `malloc_fast_path_*`/`free_fast_path_*` below: their
well-formedness and frame guarantees come straight from `rseq_pop_contract`/`rseq_push_contract`.
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`cache_refill_preserves_ownership_conservation` (§33.4).** Refilling the per-CPU
cache from a *free* batch conserves ownership: no slot is created or destroyed, every
block outside the batch keeps its owner, and **no address changes liveness**. -/
theorem cache_refill_preserves_ownership_conservation
    (s : State) (batch : List BlockId) (cpu : CpuId) (sc : SizeClassId)
    (hfree : ∀ q ∈ batch, ¬ s.IsLive q) :
    (cacheRefill s batch cpu sc).blocks.map (·.id) = s.blocks.map (·.id) ∧
      (∀ q, q ∉ batch → (cacheRefill s batch cpu sc).ownerOf q = s.ownerOf q) ∧
      (∀ q, (cacheRefill s batch cpu sc).IsLive q ↔ s.IsLive q) := by
  unfold cacheRefill
  exact ⟨relabelAll_map_id s batch _, fun q hq => relabelAll_ownerOf_not_mem _ s batch hq,
    relabelAll_isLive_iff (Owner.cpuCache cpu sc) (by simp) s batch hfree⟩

/-- **`cache_flush_preserves_ownership_conservation` (§33.4).** Flushing a *free* batch from
the per-CPU cache back to the central list conserves ownership symmetrically. -/
theorem cache_flush_preserves_ownership_conservation
    (s : State) (batch : List BlockId) (arena : ArenaId) (sc : SizeClassId)
    (hfree : ∀ q ∈ batch, ¬ s.IsLive q) :
    (cacheFlush s batch arena sc).blocks.map (·.id) = s.blocks.map (·.id) ∧
      (∀ q, q ∉ batch → (cacheFlush s batch arena sc).ownerOf q = s.ownerOf q) ∧
      (∀ q, (cacheFlush s batch arena sc).IsLive q ↔ s.IsLive q) := by
  unfold cacheFlush
  exact ⟨relabelAll_map_id s batch _, fun q hq => relabelAll_ownerOf_not_mem _ s batch hq,
    relabelAll_isLive_iff (Owner.centralFree arena sc) (by simp) s batch hfree⟩

/-- **`cache_refill_preserves_wellformed` (§33.4).** The refill is the one transition that
*raises* a per-CPU cache count; it preserves every clause — including clause 11 — given that
the batch is class `sc` (clause 3) and the implementation's capacity guard holds after the
refill (`hcap`). This discharges the capacity-under-filling obligation. -/
theorem cache_refill_preserves_wellformed
    (s : State) (batch : List BlockId) (cpu : CpuId) (sc : SizeClassId) (hwf : WellFormed s)
    (hsc : ∀ blk ∈ s.blocks, blk.id ∈ batch → blk.sc = sc)
    (hcap : (cacheRefill s batch cpu sc).countOwned (Owner.cpuCache cpu sc) ≤ maxLocalCapacity sc) :
    WellFormed (cacheRefill s batch cpu sc) := by
  unfold cacheRefill
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
      cacheCapacities := by
        intro cpu' sc'
        by_cases h : Owner.cpuCache cpu' sc' = Owner.cpuCache cpu sc
        · injection h with hc hs; subst hc; subst hs; exact hcap
        · exact Nat.le_trans (relabelAll_countOwned_le_of_ne (Owner.cpuCache cpu sc) h s batch)
            (hwf.cacheCapacities cpu' sc')
      slabLayout := relabelAll_slabLayout s batch _ hwf.slabLayout
      spansDisjoint := relabelAll_spansDisjoint s batch _ hwf.spansDisjoint
      blockSpanClass := relabelAll_blockSpanClass s batch _ hwf.blockSpanClass
      uniqueIds := relabelAll_uniqueIds s batch _ hwf.uniqueIds }

/- ----------------------------------------------------------------------- -/
/- The per-CPU fast paths — where the W1-7 RSEQ contract is load-bearing.   -/
/- ----------------------------------------------------------------------- -/

/-- **`malloc_fast_path_preserves_wellformed` (§33.4 fast path, §11.3).** A successful
per-CPU pop preserves well-formedness — straight from the RSEQ contract (W1-7). -/
theorem malloc_fast_path_preserves_wellformed (s : State) (cpu : CpuId) (sc : SizeClassId)
    (hwf : WellFormed s) {p : BlockId} {s' : State}
    (h : rseqPop s cpu sc = RseqPop.success p s') : WellFormed s' :=
  (rseq_pop_success_frame s cpu sc hwf h).2.2.1

/-- **`malloc_fast_path_conserves_ownership` (§33.4 fast path).** A successful per-CPU pop
moves exactly the popped block from the cache to `live`, framing every other block — the
W1-7 frame condition, consumed directly. -/
theorem malloc_fast_path_conserves_ownership (s : State) (cpu : CpuId) (sc : SizeClassId)
    (hwf : WellFormed s) {p : BlockId} {s' : State}
    (h : rseqPop s cpu sc = RseqPop.success p s') :
    s.ownerOf p = some (Owner.cpuCache cpu sc) ∧ s'.ownerOf p = some Owner.live ∧
      (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q) := by
  obtain ⟨h1, h2, _, h4⟩ := rseq_pop_success_frame s cpu sc hwf h
  exact ⟨h1, h2, h4⟩

/-- **`free_fast_path_preserves_wellformed` (§33.4 fast path, §11.4).** A successful per-CPU
push preserves well-formedness — straight from the RSEQ push contract (W1-7). -/
theorem free_fast_path_preserves_wellformed (s : State) (cpu : CpuId) (sc : SizeClassId)
    (p : BlockId) (hwf : WellFormed s) (hlive : s.ownerOf p = some Owner.live) {s' : State}
    (h : rseqPush s cpu sc p = RseqPush.success s') : WellFormed s' :=
  (rseq_push_success_frame s cpu sc p hwf hlive h).2.1

/-- **`free_fast_path_conserves_ownership` (§33.4 fast path).** A successful per-CPU push
moves exactly the freed block from `live` to the cache, framing every other block. -/
theorem free_fast_path_conserves_ownership (s : State) (cpu : CpuId) (sc : SizeClassId)
    (p : BlockId) (hwf : WellFormed s) (hlive : s.ownerOf p = some Owner.live) {s' : State}
    (h : rseqPush s cpu sc p = RseqPush.success s') :
    s'.ownerOf p = some (Owner.cpuCache cpu sc) ∧ (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q) := by
  obtain ⟨h1, _, h3⟩ := rseq_push_success_frame s cpu sc p hwf hlive h
  exact ⟨h1, h3⟩

end TopoMalloc

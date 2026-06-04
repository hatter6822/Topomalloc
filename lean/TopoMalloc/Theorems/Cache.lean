-- SPDX-License-Identifier: MIT
/-
§33.4 conservation theorems for the per-CPU cache batch operations (plan 02 W1-6c).

* `cache_refill_preserves_ownership_conservation`
* `cache_flush_preserves_ownership_conservation`

"Ownership conservation" = no slot is created or destroyed (the block-id multiset is
preserved) and **only** the moved batch changes owner (every block outside the batch
is framed). This is the batch generalisation of the W1-7 RSEQ frame condition, which
`rseq_pop_conserves_ownership` below derives from `rseq_pop_contract` directly —
making explicit that W1-6c *consumes* the load-bearing frame clause (§33.5, DD-2).
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- **`cache_refill_preserves_ownership_conservation` (§33.4).** Refilling the per-CPU
cache from the central/transfer list conserves ownership: block ids are unchanged and
every block outside the refilled batch keeps its owner. -/
theorem cache_refill_preserves_ownership_conservation
    (s : State) (batch : List BlockId) (cpu : CpuId) (sc : SizeClassId) :
    (cacheRefill s batch cpu sc).blocks.map (·.id) = s.blocks.map (·.id) ∧
      (∀ q, q ∉ batch → (cacheRefill s batch cpu sc).ownerOf q = s.ownerOf q) := by
  unfold cacheRefill
  exact ⟨relabelAll_map_id s batch _, fun q hq => relabelAll_ownerOf_not_mem _ s batch hq⟩

/-- **`cache_flush_preserves_ownership_conservation` (§33.4).** Flushing the per-CPU
cache back to the central list conserves ownership symmetrically. -/
theorem cache_flush_preserves_ownership_conservation
    (s : State) (batch : List BlockId) (arena : ArenaId) (sc : SizeClassId) :
    (cacheFlush s batch arena sc).blocks.map (·.id) = s.blocks.map (·.id) ∧
      (∀ q, q ∉ batch → (cacheFlush s batch arena sc).ownerOf q = s.ownerOf q) := by
  unfold cacheFlush
  exact ⟨relabelAll_map_id s batch _, fun q hq => relabelAll_ownerOf_not_mem _ s batch hq⟩

/-- The single-object fast-path conservation the batch theorems generalise — derived
*directly* from the W1-7 frame clause (§33.5). A successful per-CPU pop changes the
owner of only the popped block. -/
theorem rseq_pop_conserves_ownership (s : State) (cpu : CpuId) (sc : SizeClassId)
    (hwf : WellFormed s) {p : BlockId} {s' : State} (h : rseqPop s cpu sc = RseqPop.success p s') :
    ∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q :=
  (rseq_pop_success_frame s cpu sc hwf h).2.2.2

end TopoMalloc

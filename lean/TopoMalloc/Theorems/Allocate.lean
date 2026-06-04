-- SPDX-License-Identifier: MIT
/-
End-to-end small allocation (plan 02 W1, gap closure for composition): the §33.4
per-transition theorems prove the *pop* (`malloc`) and the *classification coverage*
(`size_class_table_covers_all_small_requests`) separately. This file **composes** them
into a single `allocate(reqSize)` guarantee — classify the request to a size class, pop a
free slot of that class, and the returned object is live, at least `reqSize` bytes, aligned
to its class, and disjoint from every other live object.
-/
import TopoMalloc.Theorems.SizeClass
import TopoMalloc.Theorems.Malloc

namespace TopoMalloc

open TopoMalloc.Generated

/-- Classify a request size to a size-class id via the emitted granule lookup (§9.2): the
granule `(reqSize-1)/quantum` indexes the precomputed `sizeToClass` map. -/
def classify (reqSize : Nat) : Option SizeClassId := sizeToClass[(reqSize - 1) / quantum]?

/-- **End-to-end small allocation (§33.4 = §9.2 classification ∘ the §33.4 pop).** For an
in-range request, if `b` is a *non-live* slot of the class that `classify` selects, then
popping it (`malloc`) yields an object that is (1) live, (2) at least `reqSize` bytes,
(3) disjoint from every other block, and (4) aligned to its class alignment for a class
whose size genuinely covers the request. This threads the lookup coverage
(`size_class_table_covers_all_small_requests`) into the per-slot pop guarantee
(`malloc_success_…`) — the end-to-end `allocate` the per-transition theorems left implicit.
The `coversAllB` hypothesis is the same `lake exe check` gate the coverage theorem uses. -/
theorem allocate_small_sound (s : State) (hwf : WellFormed s) (hcov : coversAllB = true)
    (reqSize : Nat) (h1 : 1 ≤ reqSize) (h2 : reqSize ≤ smallMax)
    (b : BlockId) (blk : Block) (hb : s.blockById b = some blk)
    (hnonlive : blk.owner ≠ Owner.live)
    (ci : SizeClassId) (hcl : classify reqSize = some ci) (hbcl : blk.sc = ci) :
    (malloc s b).ownerOf b = some Owner.live ∧
      reqSize ≤ blk.range.len ∧
      (∀ blk', blk' ∈ s.blocks → blk'.id ≠ b → Range.Disjoint blk.range blk'.range) ∧
      ∃ row, sizeClasses[ci]? = some row ∧ reqSize ≤ row.size ∧ row.align ∣ blk.range.base := by
  obtain ⟨ci', row, hlut, hrow, hsize⟩ :=
    size_class_table_covers_all_small_requests hcov reqSize h1 h2
  -- the classified id is exactly `ci`
  have hci : ci' = ci := (Option.some.inj (hcl.symm.trans hlut)).symm
  rw [hci] at hrow
  -- `b`'s class row covers the request and supplies its alignment
  have hreq : ∀ r, sizeClassRow blk.sc = some r → reqSize ≤ r.size := by
    intro r hr; rw [hbcl] at hr; unfold sizeClassRow at hr; rw [hrow] at hr
    rw [← Option.some.inj hr]; exact hsize
  have hral : ∀ r, sizeClassRow blk.sc = some r → row.align ∣ r.align := by
    intro r hr; rw [hbcl] at hr; unfold sizeClassRow at hr; rw [hrow] at hr
    rw [← Option.some.inj hr]; exact Nat.dvd_refl _
  obtain ⟨_, hlive, hsuff, halign, hdisj⟩ :=
    malloc_success_returns_aligned_sufficient_disjoint_object s b hwf blk hb hnonlive
      reqSize row.align hreq hral
  exact ⟨hlive, hsuff, hdisj, row, hrow, hsize, halign⟩

end TopoMalloc

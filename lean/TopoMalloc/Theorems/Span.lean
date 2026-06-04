-- SPDX-License-Identifier: MIT
/-
The §33.4 span split/merge disjointness theorems (plan 02 W1-8a, mirroring plan 04
W4-2b/c extent split/merge).

* `span_split_preserves_disjointness`
* `span_merge_preserves_disjointness`

Stated at the geometric core: splitting a span's range at an offset yields two
sub-ranges that tile it, so anything disjoint from the whole stays disjoint from
each half; merging two adjacent sub-ranges yields their union, so anything disjoint
from both halves is disjoint from the merge. These are the obligations the
implementation's extent split/merge discharge.
-/
import TopoMalloc.Types

namespace TopoMalloc

open Range

/-- The lower half of a range split at offset `k`: `[base, base + k)`. -/
def splitLeft (r : Range) (k : Nat) : Range := ⟨r.base, k⟩

/-- The upper half of a range split at offset `k`: `[base + k, stop)`. -/
def splitRight (r : Range) (k : Nat) : Range := ⟨r.base + k, r.len - k⟩

theorem splitLeft_subset (r : Range) (k : Nat) (hk : k ≤ r.len) :
    (splitLeft r k).Subset r := by
  simp only [splitLeft, Range.Subset, Range.stop]; omega

theorem splitRight_subset (r : Range) (k : Nat) (hk : k ≤ r.len) :
    (splitRight r k).Subset r := by
  simp only [splitRight, Range.Subset, Range.stop]; omega

/-- The two halves of a split tile the range with no gap or overlap. -/
theorem split_halves_disjoint (r : Range) (k : Nat) :
    Range.Disjoint (splitLeft r k) (splitRight r k) := by
  simp only [Range.Disjoint, splitLeft, splitRight, Range.stop]; omega

/-- **`span_split_preserves_disjointness` (§33.4).** Splitting a span's range at offset
`k ≤ len` preserves disjointness: anything disjoint from the whole range is disjoint
from each half, and the two halves are disjoint from each other. -/
theorem span_split_preserves_disjointness (r x : Range) (k : Nat) (hk : k ≤ r.len)
    (hx : Range.Disjoint x r) :
    Range.Disjoint x (splitLeft r k) ∧ Range.Disjoint x (splitRight r k) ∧
      Range.Disjoint (splitLeft r k) (splitRight r k) :=
  ⟨hx.of_subset_right (splitLeft_subset r k hk),
   hx.of_subset_right (splitRight_subset r k hk),
   split_halves_disjoint r k⟩

/-- The merge of two adjacent ranges `r1 = [b, b+k)` and `r2 = [b+k, …)`: their union
`[b, stop r2)`. -/
def merge (r1 r2 : Range) : Range := ⟨r1.base, r1.len + r2.len⟩

theorem merge_subset_left (r1 r2 : Range) : r1.Subset (merge r1 r2) := by
  simp only [merge, Range.Subset, Range.stop]; omega

theorem merge_subset_right (r1 r2 : Range) (hadj : r1.stop = r2.base) :
    r2.Subset (merge r1 r2) := by
  simp only [merge, Range.Subset, Range.stop] at *; omega

/-- **`span_merge_preserves_disjointness` (§33.4).** Merging two adjacent ranges
preserves disjointness: anything (non-empty) disjoint from both halves is disjoint
from their union. -/
theorem span_merge_preserves_disjointness (r1 r2 x : Range) (hadj : r1.stop = r2.base)
    (hvalid : x.Valid) (hx1 : Range.Disjoint x r1) (hx2 : Range.Disjoint x r2) :
    Range.Disjoint x (merge r1 r2) := by
  simp only [Range.Disjoint, Range.Valid, Range.stop, merge] at *
  omega

end TopoMalloc

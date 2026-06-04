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
import TopoMalloc.WellFormed

namespace TopoMalloc

open Range State

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

/- ----------------------------------------------------------------------- -/
/- Lifting split/merge to `State`: they preserve the §16/§22.7 span-range    -/
/- disjointness invariant (clause 13), not just disjointness of free ranges. -/
/- ----------------------------------------------------------------------- -/

/-- Split the span `sp` in a state into two descriptors `d1`, `d2` (the caller
supplies fresh, tiling descriptors): drop `sp`, add `d1` and `d2`. -/
def State.spanSplit (s : State) (sp : SpanId) (d1 d2 : SpanDesc) : State :=
  { s with spans := (s.spans.filter (fun e => decide (e.id ≠ sp))) ++ [d1, d2] }

/-- Two spans (other than the one being split) keep their ranges, and the split is a
sublist, so the surviving spans stay pairwise disjoint. -/
theorem spans_filter_pairwise (s : State) (sp : SpanId) (hwf : WfSpansDisjoint s) :
    ((s.spans.filter (fun e => decide (e.id ≠ sp))).map (·.range)).Pairwise Range.Disjoint :=
  List.Pairwise.sublist ((List.filter_sublist).map _) hwf

/-- A surviving span (id `≠ sp`) is disjoint from the split span `d`'s range. -/
theorem span_disjoint_of_id_ne (s : State) (hwf : WfSpansDisjoint s) {d e : SpanDesc}
    (hd : d ∈ s.spans) (he : e ∈ s.spans) (hne : e.id ≠ d.id) :
    Range.Disjoint e.range d.range := by
  have hpw : s.spans.Pairwise (fun a b => Range.Disjoint a.range b.range) := by
    have h := hwf; rwa [WfSpansDisjoint, List.pairwise_map] at h
  refine pairwise_rel_of_ne (fun _ _ h => h.symm) hpw he hd ?_
  intro hc; subst hc; exact hne rfl

/-- **`span_split_preserves_disjointness` lifted to `State` (§33.4 / §22.7).** Splitting
span `sp` (range `d.range`) into two tiling sub-spans preserves the span-range
disjointness invariant of the whole state. -/
theorem spanSplit_preserves_spansDisjoint (s : State) (sp : SpanId) (d1 d2 : SpanDesc)
    (hwf : WfSpansDisjoint s) (d : SpanDesc) (hd : d ∈ s.spans) (hdid : d.id = sp)
    (h1 : d1.range.Subset d.range) (h2 : d2.range.Subset d.range)
    (h12 : Range.Disjoint d1.range d2.range) :
    WfSpansDisjoint (s.spanSplit sp d1 d2) := by
  unfold State.spanSplit WfSpansDisjoint
  simp only [List.map_append, List.map_cons, List.map_nil, List.pairwise_append]
  refine ⟨spans_filter_pairwise s sp hwf, ?_, ?_⟩
  · -- the two new halves are mutually disjoint
    refine List.Pairwise.cons (fun y hy => ?_) (by simp)
    rw [List.mem_singleton] at hy; subst hy; exact h12
  · -- every surviving span is disjoint from each half
    intro x hx y hy
    simp only [List.mem_map, List.mem_filter, decide_eq_true_eq] at hx
    obtain ⟨e, ⟨hes, heid⟩, rfl⟩ := hx
    have hed : Range.Disjoint e.range d.range :=
      span_disjoint_of_id_ne s hwf hd hes (by rw [hdid]; exact heid)
    rcases List.mem_cons.mp hy with rfl | hy2
    · exact hed.of_subset_right h1
    · rw [List.mem_singleton] at hy2; subst hy2; exact hed.of_subset_right h2

/-- Merge the spans `sp1`, `sp2` in a state into one descriptor `d`: drop both, add
`d`. -/
def State.spanMerge (s : State) (sp1 sp2 : SpanId) (d : SpanDesc) : State :=
  { s with spans := (s.spans.filter (fun e => decide (e.id ≠ sp1 ∧ e.id ≠ sp2))) ++ [d] }

/-- **`span_merge_preserves_disjointness` lifted to `State` (§33.4 / §22.7).** Merging
two adjacent spans into their union preserves the state's span-range disjointness, when
the surviving spans are non-empty. -/
theorem spanMerge_preserves_spansDisjoint (s : State) (sp1 sp2 : SpanId) (d : SpanDesc)
    (hwf : WfSpansDisjoint s) (d1 d2 : SpanDesc)
    (hd1 : d1 ∈ s.spans) (hd2 : d2 ∈ s.spans) (hid1 : d1.id = sp1) (hid2 : d2.id = sp2)
    (hadj : d1.range.stop = d2.range.base) (hmerge : d.range = merge d1.range d2.range)
    (hvalid : ∀ e ∈ s.spans, Range.Valid e.range) :
    WfSpansDisjoint (s.spanMerge sp1 sp2 d) := by
  unfold State.spanMerge WfSpansDisjoint
  simp only [List.map_append, List.map_cons, List.map_nil, List.pairwise_append]
  refine ⟨List.Pairwise.sublist ((List.filter_sublist).map _) hwf, by simp, ?_⟩
  intro x hx y hy
  simp only [List.mem_map, List.mem_filter, decide_eq_true_eq] at hx
  obtain ⟨e, ⟨hes, heid1, heid2⟩, rfl⟩ := hx
  rw [List.mem_singleton] at hy
  subst hy
  rw [hmerge]
  refine span_merge_preserves_disjointness d1.range d2.range e.range hadj (hvalid e hes) ?_ ?_
  · exact span_disjoint_of_id_ne s hwf hd1 hes (by rw [hid1]; exact heid1)
  · exact span_disjoint_of_id_ne s hwf hd2 hes (by rw [hid2]; exact heid2)

end TopoMalloc

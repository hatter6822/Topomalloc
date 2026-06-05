-- SPDX-License-Identifier: MIT
/-
`size_class_table_covers_all_small_requests` (§33.4) for the **generated** tuned table
(plan 02 W1-4e, DD-1).

Two complementary results:

* `TopoMalloc.buildTable_covers` (in `TopoMalloc.SizeClass`) proves coverage, the §9.4
  spacing/waste bounds, and the §9.5 layout lemmas for the *parameterized* uniform table
  — the W1-4b..d obligations, dischargeable for any `Params`.
* Here, `size_class_table_covers_all_small_requests` proves coverage and
  `generated_table_spacing` proves the §9.4 per-range geometric spacing for the actual
  **emitted** tuned table (72 classes to 32 KiB, non-uniform) — so §9.4 is checked on the
  *shipped* table, not only on the uniform builder. The emitted table is too large to
  reduce in the kernel (`decide` overflows), so each property is proved *generally* from a
  decidable predicate (`coversAllB`, `spacingOkB`) that the `check` executable (the `lean`
  CI gate) evaluates on the generated table — the DD-1 single-source verification. This
  keeps every theorem on Lean's standard axioms (no `native_decide`).
-/
import TopoMalloc.SizeClass
import TopoMalloc.Generated.SizeClasses

namespace TopoMalloc

open TopoMalloc.Generated

/-- Granule `g` is sound when the lookup class covers the whole granule: its size is at
least the granule's largest request `(g+1)·quantum`. -/
def granuleOk (g : Nat) : Bool :=
  match sizeToClass[g]? with
  | some ci => match sizeClasses[ci]? with
    | some row => decide ((g + 1) * quantum ≤ row.size)
    | none => false
  | none => false

/-- The whole emitted lookup is sound: every granule up to `small_max` is covered. This
Bool is *evaluated* by `lake exe check`; it is not `decide`-reduced in the kernel. -/
def coversAllB : Bool := (List.range (smallMax / quantum)).all granuleOk

/-- The emitted table satisfies the §9.4 per-range spacing policy. Like `coversAllB`, this
Bool is *evaluated* by `lake exe check` (the products would strain in-kernel `decide`). -/
def spacingOkB : Bool := spacingOk sizeClasses

/-- The generated `maxAlign` equals the widest class alignment. The Rust runtime rejects an
over-aligned request in O(1) when `align > MAX_ALIGN`; that is sound only if `maxAlign` is the
true maximum, so the `lake exe check` gate re-derives it from the emitted rows (DD-1). -/
def maxAlignOkB : Bool := maxAlign == maxAlignOf sizeClasses

/-- The generated `hugeThreshold` (medium/large boundary, §9.2/§A.1) is well-formed: strictly
above the small path and a whole number of allocator pages, so the medium range is non-empty
and a page-rounded medium request can never reach the large regime. -/
def hugeThresholdOkB : Bool :=
  decide (smallMax < hugeThreshold) && decide (hugeThreshold % pageSize = 0)

/-- Granule `g`'s lookup is *minimal*, not merely covering: the class before the chosen one is
too small for the granule's smallest request `g*quantum + 1`, so no smaller class fits. With
`coversAllB` (coverage) this pins the emitted lookup to the unique smallest-fitting class —
the same property the Rust `lookup_matches_linear_scan` test checks, so both languages agree. -/
def granuleMinimalB (g : Nat) : Bool :=
  match sizeToClass[g]? with
  | none => false
  | some 0 => true
  | some (p + 1) =>
    match sizeClasses[p]? with
    | none => false
    | some prev => decide (prev.size < g * quantum + 1)

/-- The whole emitted lookup is minimal: every granule up to `small_max` maps to the smallest
fitting class. Evaluated by `lake exe check`, like `coversAllB`. -/
def minimalLookupB : Bool := (List.range (smallMax / quantum)).all granuleMinimalB

/-- The model's *own* lookup, computed from the row sizes alone (not read off the emitted
table): the first class index whose size covers `req`. -/
def firstFitIdx (req : Nat) : Option Nat :=
  (List.range sizeClasses.length).find? (fun i =>
    match sizeClasses[i]? with
    | some r => Nat.ble req r.size
    | none => false)

/-- **Model-vs-emitted lookup differential (DD-1).** For every granule the *emitted*
`sizeToClass` entry equals the index the model independently computes from the class sizes
(`firstFitIdx` of the granule's smallest request). This is a genuine cross-source check — the
emitted table provably *is* the model's lookup, not merely "both came from one generator". On
the Rust side `lookup_matches_generated_granule_map` ties `size_class` to the same emitted
table, closing the Rust ↔ emitted ↔ model loop. Evaluated by `lake exe check`. -/
def lookupMatchesModelB : Bool :=
  (List.range (smallMax / quantum)).all (fun g =>
    sizeToClass[g]? == firstFitIdx (g * quantum + 1))

/-- A sound granule yields its covering class and row. -/
theorem granuleOk_elim {g : Nat} (h : granuleOk g = true) :
    ∃ ci row, sizeToClass[g]? = some ci ∧ sizeClasses[ci]? = some row ∧
      (g + 1) * quantum ≤ row.size := by
  unfold granuleOk at h
  split at h
  · next ci hc =>
    split at h
    · next row hr => exact ⟨ci, row, hc, hr, by simpa using h⟩
    · next => simp at h
  · next => simp at h

/-- **`size_class_table_covers_all_small_requests` (§33.4).** Given the emitted lookup is
sound (`coversAllB = true`, verified by the `lean` CI gate), every request size up to
`small_max` maps through the lookup to a class whose size is at least the request. -/
theorem size_class_table_covers_all_small_requests (hcov : coversAllB = true)
    (req : Nat) (h1 : 1 ≤ req) (h2 : req ≤ smallMax) :
    ∃ ci row, sizeToClass[(req - 1) / quantum]? = some ci ∧
      sizeClasses[ci]? = some row ∧ req ≤ row.size := by
  have hq : 0 < quantum := by decide
  have hdvd : quantum ∣ smallMax := by decide
  -- the granule index is in range
  have hg : (req - 1) / quantum < smallMax / quantum := by
    have hle : (req - 1) / quantum ≤ (smallMax - 1) / quantum :=
      Nat.div_le_div_right (by omega)
    have hpred := pred_div_lt_of_dvd hdvd (show 0 < smallMax by decide)
    omega
  -- the granule is sound
  have hgok : granuleOk ((req - 1) / quantum) = true := by
    rw [coversAllB, List.all_eq_true] at hcov
    exact hcov _ (List.mem_range.mpr hg)
  obtain ⟨ci, row, hlut, hrow, hsize⟩ := granuleOk_elim hgok
  refine ⟨ci, row, hlut, hrow, ?_⟩
  -- req ≤ (g+1)·quantum ≤ row.size
  have hdm := Nat.div_add_mod (req - 1) quantum
  have hlt := Nat.mod_lt (req - 1) hq
  have hexp : ((req - 1) / quantum + 1) * quantum
      = quantum * ((req - 1) / quantum) + quantum := by
    rw [Nat.add_one_mul, Nat.mul_comm]
  omega

/- ------------------------------------------------------------------------- -/
/- W2-1 — the evaluated gates, lifted to theorems (matching the coverage one). -/
/- ------------------------------------------------------------------------- -/

/-- From a minimal granule plus the chosen class's predecessor, the predecessor is
strictly below the granule's smallest request. -/
theorem granuleMinimalB_elim {g p : Nat} {prev : SizeClassRow} (ci : Nat)
    (h : granuleMinimalB g = true)
    (hlut : sizeToClass[g]? = some ci) (hci : ci = p + 1)
    (hprev : sizeClasses[p]? = some prev) :
    prev.size < g * quantum + 1 := by
  unfold granuleMinimalB at h
  simp only [hlut, hci, hprev, decide_eq_true_eq] at h
  exact h

/-- **§9.5 lookup minimality**, lifted from the evaluated `minimalLookupB` gate: the class the
emitted lookup selects for `req` has no smaller class that also fits — its immediate
predecessor is strictly below `req`. With `size_class_table_covers_all_small_requests`
(coverage) this pins the lookup to the unique smallest fitting class, matching the Rust
`lookup_matches_linear_scan` test. -/
theorem size_class_lookup_minimal (hmin : minimalLookupB = true)
    (req : Nat) (h1 : 1 ≤ req) (h2 : req ≤ smallMax)
    {ci p : Nat} {prev : SizeClassRow}
    (hlut : sizeToClass[(req - 1) / quantum]? = some ci)
    (hci : ci = p + 1) (hprev : sizeClasses[p]? = some prev) :
    prev.size < req := by
  have hq : 0 < quantum := by decide
  have hdvd : quantum ∣ smallMax := by decide
  have hg : (req - 1) / quantum < smallMax / quantum := by
    have hle : (req - 1) / quantum ≤ (smallMax - 1) / quantum :=
      Nat.div_le_div_right (by omega)
    have hpred := pred_div_lt_of_dvd hdvd (show 0 < smallMax by decide)
    omega
  have hgmin : granuleMinimalB ((req - 1) / quantum) = true := by
    rw [minimalLookupB, List.all_eq_true] at hmin
    exact hmin _ (List.mem_range.mpr hg)
  have hlt := granuleMinimalB_elim ci hgmin hlut hci hprev
  have hle2 : (req - 1) / quantum * quantum + 1 ≤ req := by
    have := Nat.div_mul_le_self (req - 1) quantum
    omega
  omega

/-- **`MAX_ALIGN` soundness**, lifted from the evaluated `maxAlignOkB` gate: `maxAlign` is an
upper bound on every class's alignment, so the runtime `align > MAX_ALIGN` fast-reject can
never discard a request some class could actually serve. -/
theorem maxAlign_is_upper_bound (h : maxAlignOkB = true)
    {i : Nat} {r : SizeClassRow} (hr : sizeClasses[i]? = some r) :
    r.align ≤ maxAlign := by
  have hmem : r ∈ sizeClasses := List.mem_of_getElem? hr
  have heq : maxAlign = maxAlignOf sizeClasses := eq_of_beq h
  rw [heq]
  exact mem_align_le_maxAlignOf hmem

/-- **`huge_threshold` well-formedness**, lifted from the `hugeThresholdOkB` gate: the
medium/large boundary lies strictly above the small path and is a whole number of pages. -/
theorem huge_threshold_wellformed (h : hugeThresholdOkB = true) :
    smallMax < hugeThreshold ∧ hugeThreshold % pageSize = 0 := by
  unfold hugeThresholdOkB at h
  simp only [Bool.and_eq_true, decide_eq_true_eq] at h
  exact h

/- ------------------------------------------------------------------------- -/
/- §9.4 spacing for the *shipped* tuned table (not only the uniform builder). -/
/- ------------------------------------------------------------------------- -/

/-- Every adjacent pair of a list satisfies `R` — a self-contained `List.Chain'` (this
project is Mathlib-free). -/
def Adjacent {α : Type _} (R : α → α → Prop) : List α → Prop
  | [] => True
  | [_] => True
  | a :: b :: rest => R a b ∧ Adjacent R (b :: rest)

/-- Weaken the relation of an `Adjacent` chain pointwise. -/
theorem Adjacent.imp {α : Type _} {R S : α → α → Prop} (h : ∀ x y, R x y → S x y) :
    ∀ {l : List α}, Adjacent R l → Adjacent S l := by
  intro l
  induction l with
  | nil => exact fun _ => trivial
  | cons a rest ih =>
    cases rest with
    | nil => exact fun _ => trivial
    | cons b rest' => exact fun hp => ⟨h a b hp.1, ih hp.2⟩

/-- The §9.4 spacing relation as a `Prop` on an adjacent class pair, matching `spacingStepB`. -/
def spacingStep (a b : SizeClassRow) : Prop :=
  if b.size ≤ 48 then b.size ≤ 2 * a.size
  else if b.size ≤ 128 then b.size * 3 ≤ 4 * a.size
  else if b.size ≤ 1024 then b.size * 5 ≤ 6 * a.size
  else b.size * 8 ≤ 9 * a.size

theorem spacingStepB_iff {a b : SizeClassRow} : spacingStepB a b = true ↔ spacingStep a b := by
  unfold spacingStepB spacingStep
  by_cases h1 : b.size ≤ 48
  · simp [h1]
  · by_cases h2 : b.size ≤ 128
    · simp [h1, h2]
    · by_cases h3 : b.size ≤ 1024
      · simp [h1, h2, h3]
      · simp [h1, h2, h3]

/-- The Bool table predicate `spacingOk` is exactly the adjacent-pair chain of `spacingStep`. -/
theorem spacingOk_iff_adjacent (rows : List SizeClassRow) :
    spacingOk rows = true ↔ Adjacent spacingStep rows := by
  induction rows with
  | nil => simp [spacingOk, Adjacent]
  | cons a rest ih =>
    cases rest with
    | nil => simp [spacingOk, Adjacent]
    | cons b rest' =>
      rw [spacingOk, Bool.and_eq_true, ih, spacingStepB_iff]; exact Iff.rfl

/-- A bounded §9.4 ratio implies the global geometric bound: no class exceeds **twice** the
previous, so size-class rounding wastes < 50% (the headline §9.4 fragmentation guarantee). -/
theorem spacingStep_le_two {a b : SizeClassRow} (h : spacingStep a b) : b.size ≤ 2 * a.size := by
  unfold spacingStep at h
  by_cases h1 : b.size ≤ 48
  · rw [if_pos h1] at h; omega
  · rw [if_neg h1] at h
    by_cases h2 : b.size ≤ 128
    · rw [if_pos h2] at h; omega
    · rw [if_neg h2] at h
      by_cases h3 : b.size ≤ 1024
      · rw [if_pos h3] at h; omega
      · rw [if_neg h3] at h; omega

/-- **§9.4 holds for the emitted tuned table.** Given the generator-checked spacing gate
(`spacingOkB`, evaluated by `lake exe check`), every adjacent pair of *shipped* classes
satisfies its per-range geometric ratio — the §9.4 policy, re-verified in Lean on the real
72-class table rather than only on the parameterized uniform builder. -/
theorem generated_table_spacing (hsp : spacingOkB = true) :
    Adjacent spacingStep sizeClasses := (spacingOk_iff_adjacent _).mp hsp

/-- Corollary: on the shipped table no class is more than twice its predecessor. -/
theorem generated_table_spacing_le_two (hsp : spacingOkB = true) :
    Adjacent (fun a b => b.size ≤ 2 * a.size) sizeClasses :=
  (generated_table_spacing hsp).imp (fun _ _ => spacingStep_le_two)

end TopoMalloc

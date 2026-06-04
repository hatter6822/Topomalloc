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

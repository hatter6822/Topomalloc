-- SPDX-License-Identifier: MIT
/-
`size_class_table_covers_all_small_requests` (§33.4) for the **generated** tuned table
(plan 02 W1-4e, DD-1).

Two complementary results:

* `TopoMalloc.buildTable_covers` (in `TopoMalloc.SizeClass`) proves coverage, the §9.4
  spacing/waste bounds, and the §9.5 layout lemmas for the *parameterized* uniform table
  — the W1-4b..d obligations, dischargeable for any `Params`.
* Here, `size_class_table_covers_all_small_requests` proves coverage for the actual
  **emitted** tuned table (72 classes to 32 KiB, non-uniform). The emitted lookup is too
  large to reduce in the kernel (`decide` overflows), so coverage is proved *generally*
  from a decidable per-granule soundness predicate `coversAllB`, which the `check`
  executable (the `lean` CI gate) evaluates on the generated table — the DD-1 single-source
  verification. This keeps every theorem on Lean's standard axioms (no `native_decide`).
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

end TopoMalloc

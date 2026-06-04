-- SPDX-License-Identifier: MIT
/-
`size_class_table_covers_all_small_requests` (§33.4) for the **generated** table, and
the single-source-of-truth tie (plan 02 W1-4e, DD-1).

The general coverage theorem `buildTable_covers` (in `TopoMalloc.SizeClass`) is proved
once for any valid parameterization. Here we (a) exhibit the D4 build constants the
committed golden derives from, (b) machine-check that the Lean-derived `buildTable`
*equals* the generated golden — so the emitted table cannot drift from the model — and
(c) instantiate coverage at those constants, naming it exactly per §33.4.
-/
import TopoMalloc.SizeClass
import TopoMalloc.Generated.SizeClasses

namespace TopoMalloc

/-- The D4 build constants the committed golden derives from (Appendix C / §9.4). -/
def genParams : Params :=
  { page := 16384, quantum := 16, tinyMin := 16, smallMax := 128, batch := 32, cap := 256 }

/-- The build constants are internally consistent. -/
theorem genParams_valid : genParams.Valid := by decide

/-- **Single source of truth (DD-1).** The Lean-derived parameterized table is
*exactly* the generated golden, so the emitted artifact cannot drift from the model. -/
theorem buildTable_eq_generated : buildTable genParams = Generated.sizeClasses := by decide

/-- The emitted size→class lookup is the uniform identity-on-granules map. -/
theorem sizeToClass_eq_generated :
    Generated.sizeToClass = List.range (genParams.smallMax / genParams.quantum) := by decide

/-- **`size_class_table_covers_all_small_requests` (§33.4).** Every request size up to
`small_max` maps to an in-bounds class whose size is at least the request and is the
smallest such class. Proved for the generated golden via the general
`buildTable_covers` and the single-source equality. -/
theorem size_class_table_covers_all_small_requests (req : Nat)
    (h1 : 1 ≤ req) (h2 : req ≤ Generated.smallMax) :
    ∃ row, Generated.sizeClasses[uniformClassOf genParams req]? = some row ∧
      req ≤ row.size ∧
      (∀ j, j < uniformClassOf genParams req → ∀ rowj,
        Generated.sizeClasses[j]? = some rowj → rowj.size < req) := by
  rw [← buildTable_eq_generated]
  exact buildTable_covers genParams genParams_valid req h1 h2

end TopoMalloc

-- SPDX-License-Identifier: MIT
/-
The size-class model and its §9.4/§9.5 proofs (plan 02 W1-4, DD-1).

Three layers, smallest blast radius first (so a generator change re-checks
cheaply rather than re-proving the whole table):

* `SizeClassRow` + the decidable well-formedness predicate `tableOk` (§9.3/§9.5).
  The `check` executable replays the *generated tuned table* (the single source of
  truth — 72 non-uniform classes to 32 KiB) through it, closing the DD-1 loop on
  the Lean side.
* `Params` + `buildTable` (W1-4a): a *parameterized uniform* table that derives
  from build constants with no hand-written literals — a pure function of the D4
  parameters. It is the vehicle for the general §9.4/§9.5 obligations below; it is
  **not** the emitted table (the runtime table is the tuned, non-uniform one, whose
  coverage is proved separately — see `TopoMalloc.Theorems.SizeClass`).
* The machine-checked obligations (W1-4b..e) for `buildTable`: the spacing-dominated
  ratio bound (§9.4), the alignment-dominated waste caveat (§9.4), the layout lemmas
  (§9.5), and lookup totality/coverage (`buildTable_covers`, §9.5). The named
  `size_class_table_covers_all_small_requests` (§33.4) for the *generated tuned*
  table lives in `TopoMalloc.Theorems.SizeClass`, discharged from the decidable
  per-granule predicate `coversAllB` that `lake exe check` evaluates (the tuned
  lookup is too large to reduce in the kernel).
-/
import TopoMalloc.Types

namespace TopoMalloc

/-- One size class. Field names match the generated Lean table and the Rust
`SizeClassRow`. -/
structure SizeClassRow where
  size : Nat
  align : Nat
  slabPages : Nat
  objectsPerSlab : Nat
  batch : Nat
  maxLocalCapacity : Nat
  deriving Repr, DecidableEq

/-- A natural number is a power of two. -/
def isPow2 (n : Nat) : Bool :=
  n != 0 && Nat.land n (n - 1) == 0

/-- Per-row invariants (§9.3 alignment, §9.5 slab layout and batch bounds). -/
def rowOk (pageSize quantum : Nat) (r : SizeClassRow) : Bool :=
  r.size != 0
    && r.align != 0
    && isPow2 r.align
    && r.size % r.align == 0          -- §9.3 third constraint (load-bearing)
    && r.size % quantum == 0          -- granule-lookup soundness
    && Nat.ble r.align r.size         -- align ≤ size
    && r.slabPages != 0               -- ≥ 1
    && r.objectsPerSlab != 0          -- ≥ 1
    && r.objectsPerSlab == r.slabPages * pageSize / r.size   -- exact packing
    && Nat.ble (r.objectsPerSlab * r.size) (r.slabPages * pageSize)  -- fits the span
    && r.batch != 0                   -- ≥ 1
    && Nat.ble r.batch r.maxLocalCapacity   -- §9.5 batch ≤ capacity

/-- Sizes are strictly increasing. (Bool-valued comparisons so the predicate is
decidable by evaluation; `Nat.blt`/`Nat.ble` return `Bool`, unlike `<`/`≤`.) -/
def monotone : List SizeClassRow → Bool
  | [] => true
  | [_] => true
  | a :: b :: rest => Nat.blt a.size b.size && monotone (b :: rest)

/-- §9.4 per-range geometric spacing, as a `Bool` on an adjacent pair `(a, b)`: tiny
classes (≤ 48 B) are alignment-dominated (ratio ≤ 2/1); above that the ratio tightens to
4/3 (≤ 128 B), 6/5 (≤ 1024 B), and 9/8 (larger). Mirrors the size-class generator's §9.4
policy check (`tools/size-class-gen/src/model.rs`), so the Lean side re-verifies the
*shipped* table — not only the parameterized one. -/
def spacingStepB (a b : SizeClassRow) : Bool :=
  if b.size ≤ 48 then Nat.ble b.size (2 * a.size)
  else if b.size ≤ 128 then Nat.ble (b.size * 3) (4 * a.size)
  else if b.size ≤ 1024 then Nat.ble (b.size * 5) (6 * a.size)
  else Nat.ble (b.size * 8) (9 * a.size)

/-- §9.4 spacing over a whole table: every adjacent pair is within range ratio. Bool-valued
(decidable by evaluation), like `monotone`. -/
def spacingOk : List SizeClassRow → Bool
  | [] => true
  | [_] => true
  | a :: b :: rest => spacingStepB a b && spacingOk (b :: rest)

/-- Whole-table well-formedness: non-empty, every row sound, strictly
increasing, and the largest class equals `smallMax` (coverage upper bound). -/
def tableOk (pageSize quantum smallMax : Nat) (rows : List SizeClassRow) : Bool :=
  !rows.isEmpty
    && rows.all (rowOk pageSize quantum)
    && monotone rows
    && (match rows.getLast? with
        | some last => last.size == smallMax
        | none => false)

/- ------------------------------------------------------------------------- -/
/- W1-4a — the parameterized builder (no literals: the table derives from D4). -/
/- ------------------------------------------------------------------------- -/

/-- The D4 build constants the table derives from (§9.4 / Appendix C). -/
structure Params where
  /-- Allocator page size in bytes. -/
  page : Nat
  /-- Alignment quantum (minimum class spacing), a power of two. -/
  quantum : Nat
  /-- Smallest class size (`= quantum`). -/
  tinyMin : Nat
  /-- Largest small-path request the table serves. -/
  smallMax : Nat
  /-- Middle→front transfer batch. -/
  batch : Nat
  /-- Per-CPU hard capacity (`batch ≤ cap`). -/
  cap : Nat
  deriving Repr, DecidableEq

/-- The build constants are internally consistent. Every W1-4 obligation takes
this as its hypothesis, so nothing below is proved for a degenerate table.
`@[reducible]` so the concrete generated parameters can be checked by `decide`. -/
@[reducible] def Params.Valid (p : Params) : Prop :=
  0 < p.page ∧ 0 < p.quantum ∧ isPow2 p.quantum = true ∧ p.quantum ∣ p.smallMax ∧
    p.tinyMin = p.quantum ∧ 0 < p.smallMax ∧ p.smallMax ≤ p.page ∧
    0 < p.batch ∧ p.batch ≤ p.cap

/-- Class `k` (1-based): `k` quanta wide, quantum-aligned, exact-packed into one
page. The objects-per-slab count is computed (`floor(page / size)`), never
written by hand. -/
def buildRow (p : Params) (k : Nat) : SizeClassRow :=
  { size := k * p.quantum
    align := p.quantum
    slabPages := 1
    objectsPerSlab := p.page / (k * p.quantum)
    batch := p.batch
    maxLocalCapacity := p.cap }

/-- The whole table: classes `1·quantum, 2·quantum, …, smallMax`. A pure function
of `p` — the W1-4a deliverable. -/
def buildTable (p : Params) : List SizeClassRow :=
  (List.range' 1 (p.smallMax / p.quantum)).map (buildRow p)

@[simp] theorem buildTable_length (p : Params) :
    (buildTable p).length = p.smallMax / p.quantum := by
  simp [buildTable, List.length_range']

/-- The `i`-th row of the table is class `i+1`. -/
theorem buildTable_get? (p : Params) (i : Nat) (h : i < p.smallMax / p.quantum) :
    (buildTable p)[i]? = some (buildRow p (i + 1)) := by
  have hk : (1 : Nat) + 1 * i = i + 1 := by omega
  simp only [buildTable, List.getElem?_map, List.getElem?_range' h, Option.map_some, hk]

@[simp] theorem buildRow_size (p : Params) (k : Nat) : (buildRow p k).size = k * p.quantum := rfl
@[simp] theorem buildRow_align (p : Params) (k : Nat) : (buildRow p k).align = p.quantum := rfl

/- ------------------------------------------------------------------------- -/
/- W1-4b — spacing-dominated regime (§9.4): bound the per-class spacing ratio. -/
/- ------------------------------------------------------------------------- -/

/-- **Spacing-dominated bound (§9.4).** For quantum-spaced classes (each class is
one quantum wider than the previous), once the previous class is at least `Wden`
quanta wide — i.e. we are out of the alignment-dominated regime — the spacing
ratio `r(c) = size(c)/size(prev)` is bounded by `1 + 1/Wden`. Stated as the
integer cross-multiplied inequality so it needs no rationals:
`size(c) · Wden ≤ (Wden + 1) · size(prev)`. With `Wden = 3` this is the SPEC's
`r ≤ 1.33` target for the 49–128 B range; `Wden = 5` gives `1.20`, `8` gives
`1.125`. -/
theorem spacing_dominated_bound (sizePrev sizeC quantum Wden : Nat)
    (hspace : sizeC = sizePrev + quantum) (hregime : quantum * Wden ≤ sizePrev) :
    sizeC * Wden ≤ (Wden + 1) * sizePrev := by
  have hL : sizeC * Wden = sizePrev * Wden + quantum * Wden := by rw [hspace, Nat.add_mul]
  have hR : (Wden + 1) * sizePrev = sizePrev * Wden + sizePrev := by
    rw [Nat.add_one_mul, Nat.mul_comm]
  omega

/-- The uniform table is exactly quantum-spaced: adjacent class sizes differ by
one quantum. This is the hypothesis `spacing_dominated_bound` consumes. -/
theorem buildRow_quantum_spaced (p : Params) (k : Nat) :
    (buildRow p (k + 1)).size = (buildRow p k).size + p.quantum := by
  simp only [buildRow_size]; rw [Nat.add_one_mul]

/- ------------------------------------------------------------------------- -/
/- W1-4c — alignment-dominated regime (§9.4): waste is bounded by the quantum. -/
/- ------------------------------------------------------------------------- -/

/-- The class size a request rounds up to under quantum spacing: the smallest
multiple of `quantum` that is `≥ req`. -/
def roundedSize (quantum req : Nat) : Nat := ((req - 1) / quantum + 1) * quantum

/-- The rounded size never under-serves the request (§9.5 "mapped class size is
at least requested size"). -/
theorem roundedSize_ge (quantum req : Nat) (hq : 0 < quantum) (h1 : 1 ≤ req) :
    req ≤ roundedSize quantum req := by
  unfold roundedSize
  have hdm := Nat.div_add_mod (req - 1) quantum
  have hlt := Nat.mod_lt (req - 1) hq
  have hexp : ((req - 1) / quantum + 1) * quantum
      = quantum * ((req - 1) / quantum) + quantum := by
    rw [Nat.add_one_mul, Nat.mul_comm]
  omega

/-- **Alignment-dominated waste caveat (§9.4).** Below `q/W` a flat per-request
waste target is unattainable; instead waste is bounded by the quantum. The
absolute waste of rounding a request to the next quantum multiple is strictly
less than one quantum, i.e. `waste_ratio(req) ≤ (q-1)/req`. -/
theorem alignment_dominated_waste (quantum req : Nat) (hq : 0 < quantum) (h1 : 1 ≤ req) :
    roundedSize quantum req - req < quantum := by
  unfold roundedSize
  have hdm := Nat.div_add_mod (req - 1) quantum
  have hlt := Nat.mod_lt (req - 1) hq
  have hexp : ((req - 1) / quantum + 1) * quantum
      = quantum * ((req - 1) / quantum) + quantum := by
    rw [Nat.add_one_mul, Nat.mul_comm]
  omega

/-- Minimality of the rounding: the *previous* quantum multiple is below the
request, so `roundedSize` is the smallest class that fits (§9.5). -/
theorem roundedSize_pred_lt (quantum req : Nat) (hq : 0 < quantum) (h1 : 1 ≤ req) :
    ((req - 1) / quantum) * quantum < req := by
  have hdm := Nat.div_add_mod (req - 1) quantum
  have hlt := Nat.mod_lt (req - 1) hq
  have hexp : ((req - 1) / quantum) * quantum = quantum * ((req - 1) / quantum) :=
    Nat.mul_comm _ _
  omega

/- ------------------------------------------------------------------------- -/
/- W1-4d — layout lemmas (§9.5): alignment-multiple, slab-fit, slot disjoint.  -/
/- ------------------------------------------------------------------------- -/

/-- §9.3 third constraint: `size(c)` is an integer multiple of `align(c)`. Under
quantum spacing `align = quantum` and `size = k·quantum`, so divisibility holds
for every class. This is the proof obligation behind "alignment is sufficient". -/
theorem buildRow_align_dvd_size (p : Params) (k : Nat) :
    (buildRow p k).align ∣ (buildRow p k).size := by
  simp only [buildRow_size, buildRow_align]
  exact ⟨k, by rw [Nat.mul_comm]⟩

/-- §9.5 slab layout: the carved objects fit within the slab without overflowing
the span. `objectsPerSlab = floor(slabBytes / size)`, so `objects · size ≤
slabBytes` by the division bound. -/
theorem buildRow_objects_fit (p : Params) (k : Nat) :
    (buildRow p k).objectsPerSlab * (buildRow p k).size ≤ (buildRow p k).slabPages * p.page := by
  simp only [buildRow, Nat.one_mul]
  exact Nat.div_mul_le_self _ _

/-- The byte range of object `i` in a slab based at `base` with stride `size`. -/
def slabSlot (base size i : Nat) : Range := ⟨base + i * size, size⟩

/-- Distinct slab slots are disjoint (§9.5 "object ranges in a slab are pairwise
disjoint"): consecutive objects are laid at `base + i·size`, so slot `i` ends
exactly where slot `i+1` begins. -/
theorem slabSlot_disjoint_of_lt (base size i j : Nat) (hlt : i < j) :
    Range.Disjoint (slabSlot base size i) (slabSlot base size j) := by
  apply Range.disjoint_of_adjacent
  simp only [slabSlot, Range.stop]
  have : (i + 1) * size ≤ j * size := Nat.mul_le_mul_right size hlt
  rw [Nat.add_one_mul] at this
  omega

/-- The full list of slab slots is pairwise disjoint. -/
theorem slabSlots_pairwise_disjoint (base size n : Nat) :
    (((List.range n).map (slabSlot base size))).Pairwise Range.Disjoint := by
  apply List.Pairwise.map
  · intro a b hab
    exact slabSlot_disjoint_of_lt base size a b hab
  · exact List.pairwise_lt_range

/- ------------------------------------------------------------------------- -/
/- W1-4e — lookup totality + coverage (§9.5 / §33.4).                          -/
/- ------------------------------------------------------------------------- -/

/-- The (0-based) class index a small request maps to under quantum spacing. -/
def uniformClassOf (p : Params) (req : Nat) : Nat := (req - 1) / p.quantum

/-- For a divisor that divides `n`, dropping one byte drops the quotient: this is
exactly what makes the top class `smallMax` cover `smallMax` and no request spill
past the table. -/
theorem pred_div_lt_of_dvd {q n : Nat} (hdvd : q ∣ n) (hn : 0 < n) :
    (n - 1) / q < n / q := by
  have hm : q * (n / q) = n := Nat.mul_div_cancel' hdvd
  have hle : (n - 1) / q * q ≤ n - 1 := Nat.div_mul_le_self (n - 1) q
  have hlt : q * ((n - 1) / q) < q * (n / q) := by
    rw [Nat.mul_comm q ((n - 1) / q), hm]; omega
  exact Nat.lt_of_mul_lt_mul_left hlt

/-- The chosen index is in bounds (lookup totality, §9.5). -/
theorem uniformClassOf_lt_length (p : Params) (hp : p.Valid) (req : Nat)
    (h1 : 1 ≤ req) (h2 : req ≤ p.smallMax) :
    uniformClassOf p req < (buildTable p).length := by
  have hq : 0 < p.quantum := hp.2.1
  have hdvd : p.quantum ∣ p.smallMax := hp.2.2.2.1
  rw [buildTable_length]
  unfold uniformClassOf
  have hle : (req - 1) / p.quantum ≤ (p.smallMax - 1) / p.quantum :=
    Nat.div_le_div_right (by omega)
  have hpred : (p.smallMax - 1) / p.quantum < p.smallMax / p.quantum :=
    pred_div_lt_of_dvd hdvd (by omega)
  omega

/-- **`size_class_table_covers_all_small_requests` (§33.4), general form.** Every
request `1 ≤ req ≤ smallMax` maps to an in-bounds class whose size is at least
the request (sufficient) and is the smallest such class (minimal): the previous
class is too small. The named `size_class_table_covers_all_small_requests` for the
generated tuned table is in `TopoMalloc.Theorems.SizeClass`. -/
theorem buildTable_covers (p : Params) (hp : p.Valid) (req : Nat)
    (h1 : 1 ≤ req) (h2 : req ≤ p.smallMax) :
    ∃ row, (buildTable p)[uniformClassOf p req]? = some row ∧
      req ≤ row.size ∧
      (∀ j, j < uniformClassOf p req → ∀ rowj, (buildTable p)[j]? = some rowj → rowj.size < req) := by
  have hq : 0 < p.quantum := hp.2.1
  have hidx : uniformClassOf p req < (buildTable p).length :=
    uniformClassOf_lt_length p hp req h1 h2
  rw [buildTable_length] at hidx
  refine ⟨buildRow p (uniformClassOf p req + 1), ?_, ?_, ?_⟩
  · exact buildTable_get? p _ hidx
  · -- sufficiency: size = roundedSize ≥ req
    have hsuf := roundedSize_ge p.quantum req hq h1
    simpa [buildRow_size, uniformClassOf, roundedSize] using hsuf
  · -- minimality: every earlier class is below req
    intro j hj rowj hrowj
    have hjlt : j < p.smallMax / p.quantum := Nat.lt_trans hj hidx
    rw [buildTable_get? p j hjlt] at hrowj
    injection hrowj with hrowj'
    subst hrowj'
    simp only [buildRow_size]
    -- (j+1)·quantum ≤ uniformClassOf·quantum = ((req-1)/quantum)·quantum < req
    have hj' : j + 1 ≤ uniformClassOf p req := hj
    have hle : (j + 1) * p.quantum ≤ uniformClassOf p req * p.quantum :=
      Nat.mul_le_mul_right p.quantum hj'
    have hmin := roundedSize_pred_lt p.quantum req hq h1
    unfold uniformClassOf at hle
    omega

/- ------------------------------------------------------------------------- -/
/- W2-3b — over-alignment routing (`align_walk`), modeled and proved.         -/
/- ------------------------------------------------------------------------- -/

/-- Class `i` exists and is naturally aligned enough for `align` (the walk's
per-index predicate). -/
def alignSatAt (classes : List SizeClassRow) (align i : Nat) : Bool :=
  match classes[i]? with
  | some r => Nat.ble align r.align
  | none => false

/-- Mirror of the runtime `align_walk`: the first class index `≥ start` that is
naturally aligned enough for `align`, or `none`. The over-alignment escape
(§9.3/§25.5) advances to a sufficiently-aligned class — it never offset-adjusts an
object inside a shared slab. -/
def alignWalk (classes : List SizeClassRow) (start align : Nat) : Option Nat :=
  (List.range classes.length).find? (fun i => Nat.ble start i && alignSatAt classes align i)

/-- **Over-alignment sufficiency (W2-3b).** Whatever class `alignWalk` selects is
at or after `start` and is naturally aligned enough for the request — so the
request is served without widening any slab's stride (the load-bearing §9.3/§25.5
rule, proved rather than only tested). -/
theorem alignWalk_sufficient {classes : List SizeClassRow} {start align ci : Nat}
    (h : alignWalk classes start align = some ci) :
    start ≤ ci ∧ ∃ r, classes[ci]? = some r ∧ align ≤ r.align := by
  unfold alignWalk at h
  have hpred := List.find?_some h
  simp only [Bool.and_eq_true] at hpred
  obtain ⟨hstart, hsat⟩ := hpred
  refine ⟨Nat.le_of_ble_eq_true hstart, ?_⟩
  unfold alignSatAt at hsat
  split at hsat
  · next r hr => exact ⟨r, hr, Nat.le_of_ble_eq_true hsat⟩
  · next => simp at hsat

/-- The size→class mapping, modeled: granule-lookup then the over-alignment walk
(mirrors the runtime `size_class_in`). -/
def sizeClassOf (classes : List SizeClassRow) (sizeToClass : List Nat)
    (quantum size align : Nat) : Option Nat :=
  match sizeToClass[(size - 1) / quantum]? with
  | some start => alignWalk classes start align
  | none => none

/-- End-to-end: the classified size class is naturally aligned enough for the
request — the W2-3b guarantee on the full modeled lookup. -/
theorem sizeClassOf_sufficient {classes : List SizeClassRow} {sizeToClass : List Nat}
    {quantum size align ci : Nat}
    (h : sizeClassOf classes sizeToClass quantum size align = some ci) :
    ∃ r, classes[ci]? = some r ∧ align ≤ r.align := by
  unfold sizeClassOf at h
  split at h
  · next start _ => exact (alignWalk_sufficient h).2
  · next => simp at h

/- ------------------------------------------------------------------------- -/
/- W2-1 — `maxAlign` upper-bound machinery (the `max`-fold over the table).    -/
/- ------------------------------------------------------------------------- -/

/-- The maximum alignment over a class list. -/
def maxAlignOf (classes : List SizeClassRow) : Nat :=
  classes.foldl (fun acc r => max acc r.align) 0

theorem foldl_maxAlign_ge_init (l : List SizeClassRow) (init : Nat) :
    init ≤ l.foldl (fun acc r => max acc r.align) init := by
  induction l generalizing init with
  | nil => simp
  | cons hd tl ih =>
    simp only [List.foldl_cons]
    exact Nat.le_trans (Nat.le_max_left init hd.align) (ih (max init hd.align))

theorem mem_align_le_foldl (l : List SizeClassRow) (init : Nat) {r : SizeClassRow}
    (hr : r ∈ l) : r.align ≤ l.foldl (fun acc r => max acc r.align) init := by
  induction l generalizing init with
  | nil => simp at hr
  | cons hd tl ih =>
    simp only [List.foldl_cons]
    rcases List.mem_cons.mp hr with rfl | hmem
    · exact Nat.le_trans (Nat.le_max_right init r.align)
        (foldl_maxAlign_ge_init tl (max init r.align))
    · exact ih (max init hd.align) hmem

/-- Every class's alignment is bounded by `maxAlignOf` — the fact behind the
runtime `align > MAX_ALIGN` fast-reject being sound. -/
theorem mem_align_le_maxAlignOf {classes : List SizeClassRow} {r : SizeClassRow}
    (hr : r ∈ classes) : r.align ≤ maxAlignOf classes :=
  mem_align_le_foldl classes 0 hr

end TopoMalloc

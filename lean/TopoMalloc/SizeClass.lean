-- SPDX-License-Identifier: MIT
/-
The size-class row type and a decidable well-formedness predicate (SPEC §9.3 /
§9.5, plan 02 W1-4). The *generated* table (`TopoMalloc.Generated.SizeClasses`,
the single source of truth) is checked against this predicate by the `check`
executable, closing the DD-1 loop on the Lean side. The full machine-checked
spacing-ratio proofs (§9.4) land with plan 02; M0 establishes the row type, the
predicate, and the generated-table import.
-/
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

/-- Whole-table well-formedness: non-empty, every row sound, strictly
increasing, and the largest class equals `smallMax` (coverage upper bound). -/
def tableOk (pageSize quantum smallMax : Nat) (rows : List SizeClassRow) : Bool :=
  !rows.isEmpty
    && rows.all (rowOk pageSize quantum)
    && monotone rows
    && (match rows.getLast? with
        | some last => last.size == smallMax
        | none => false)

end TopoMalloc

-- SPDX-License-Identifier: MIT
/-
Core Lean types for the TopoMalloc abstract model (SPEC §33.2, plan 02 W1-1).
These mirror the runtime newtypes in `crates/topo-core/src/ids.rs`, so the proof
and the implementation name the same things.

Two deliberate, documented simplifications of the §33.2 *schematic*:

* `Range` is plain data (`base`, `len` as `Nat`) rather than carrying the
  dependent `len_pos : len > 0` field, so it has `DecidableEq`/`Repr` and drives
  the executable model (§33.7) directly. Non-emptiness is the separate predicate
  `Range.Valid`, taken as a hypothesis wherever it is needed (nothing is proved
  vacuously). The byte address/length fields are concrete `Nat` (not the `Addr`/
  `Bytes` aliases) because `omega` — the decision procedure the geometry proofs
  rely on — reasons only over literal `Nat`/`Int`, not over type aliases.
* `Addr`/`Bytes`/… remain as documentation aliases naming the SPEC entities; they
  appear in the `Owner` enum and in function signatures as identifiers/keys,
  where no linear-arithmetic reasoning is performed on them.
-/
namespace TopoMalloc

abbrev Addr := Nat
abbrev Bytes := Nat
abbrev CpuId := Nat
abbrev ArenaId := Nat
abbrev SizeClassId := Nat
abbrev BlockId := Nat
abbrev SpanId := Nat
abbrev HugePageId := Nat

/-- A security/information-flow label (§36.12). On POSIX there is the single
`PUBLIC` label (`0`); on seLe4n, labels partition caches and statistics. -/
abbrev Label := Nat

/-- The single label used by the POSIX (single-authority) backend. -/
def Label.public : Label := 0

/-- A half-open byte range `[base, base + len)`. `base` is a byte address, `len`
a byte count (the SPEC `Addr`/`Bytes`); both are `Nat` so the geometry lemmas are
discharged by `omega`. -/
structure Range where
  base : Nat
  len : Nat
  deriving Repr, DecidableEq

namespace Range

/-- One past the last byte of the range. -/
def stop (r : Range) : Nat := r.base + r.len

/-- A range is valid when it is non-empty (the §33.2 `len_pos` side condition). -/
def Valid (r : Range) : Prop := 0 < r.len

instance (r : Range) : Decidable (Valid r) := by unfold Valid; infer_instance

/-- `addr` lies inside the range. -/
def Contains (r : Range) (addr : Nat) : Prop := r.base ≤ addr ∧ addr < r.stop

/-- Two ranges are disjoint when one ends at or before the other begins. -/
def Disjoint (a b : Range) : Prop := a.stop ≤ b.base ∨ b.stop ≤ a.base

/-- `a` is contained in `b` (`a ⊆ b` as byte sets). -/
def Subset (a b : Range) : Prop := b.base ≤ a.base ∧ a.stop ≤ b.stop

instance (a b : Range) : Decidable (Disjoint a b) := by unfold Disjoint stop; infer_instance
instance (a b : Range) : Decidable (Subset a b) := by unfold Subset stop; infer_instance
instance (r : Range) (a : Nat) : Decidable (Contains r a) := by unfold Contains stop; infer_instance

/-- Disjointness is symmetric. -/
theorem Disjoint.symm {a b : Range} (h : Disjoint a b) : Disjoint b a := by
  simp only [Range.Disjoint, Range.stop] at *; omega

/-- A range is never disjoint from itself when it is non-empty. -/
theorem not_disjoint_self {a : Range} (h : Valid a) : ¬ Disjoint a a := by
  simp only [Range.Disjoint, Range.Valid, Range.stop] at *; omega

/-- Disjointness is inherited by subsets: if `c ⊆ b` and `a` is disjoint from
`b`, then `a` is disjoint from `c`. This is the workhorse for span split/merge
and for the released-range safety proofs. -/
theorem Disjoint.of_subset_right {a b c : Range}
    (hsub : Subset c b) (hdisj : Disjoint a b) : Disjoint a c := by
  simp only [Range.Disjoint, Range.Subset, Range.stop] at *; omega

/-- Symmetric form: shrinking the left argument preserves disjointness. -/
theorem Disjoint.of_subset_left {a b c : Range}
    (hsub : Subset c a) (hdisj : Disjoint a b) : Disjoint c b :=
  (hdisj.symm.of_subset_right hsub).symm

/-- A range is a subset of itself. -/
theorem Subset.refl (a : Range) : Subset a a := by
  simp only [Range.Subset, Range.stop]; omega

/-- Two adjacent ranges sharing a cut point are disjoint. -/
theorem disjoint_of_adjacent {a b : Range} (h : a.stop ≤ b.base) : Disjoint a b :=
  Or.inl h

/-- If `addr` is in a valid sub-range `c ⊆ b`, it is in `b`. -/
theorem Contains.of_subset {b c : Range} {addr : Nat}
    (hsub : Subset c b) (h : Contains c addr) : Contains b addr := by
  simp only [Range.Contains, Range.Subset, Range.stop] at *; omega

end Range

/-- Every block has exactly one owner (SPEC §33.2). This enumeration includes
all SPEC owners, including `quarantine` and `released` (W1-1 acceptance). -/
inductive Owner where
  | live
  | cpuCache (cpu : CpuId) (sc : SizeClassId)
  | threadCache (tid : Nat) (sc : SizeClassId)
  | transferCache (domain : Nat) (sc : SizeClassId)
  | centralFree (arena : ArenaId) (sc : SizeClassId)
  | backendFree (arena : ArenaId)
  | quarantine (arena : ArenaId)
  | released (arena : ArenaId)
  deriving Repr, DecidableEq

namespace Owner

/-- The size class a *cache* owner is keyed by, if any (clause 3, §11.2). The
per-CPU, thread, and transfer caches are per-size-class; the other owners are
not keyed by a class here. -/
def cacheSizeClass? : Owner → Option SizeClassId
  | cpuCache _ sc => some sc
  | threadCache _ sc => some sc
  | transferCache _ sc => some sc
  | _ => none

/-- The arena a *backing/central* owner is attributed to, if any (clause 4,
§14.6 / §22.7). `live` and the front-end caches are not arena-qualified in the
ownership enum; a block's arena is recovered from its span (§16.2). -/
def arena? : Owner → Option ArenaId
  | centralFree a _ => some a
  | backendFree a => some a
  | quarantine a => some a
  | released a => some a
  | _ => none

/-- Whether the owner is the `live` state. -/
def isLive : Owner → Bool
  | live => true
  | _ => false

@[simp] theorem isLive_live : isLive live = true := rfl

@[simp] theorem isLive_eq_true {o : Owner} : isLive o = true ↔ o = live := by
  cases o <;> simp [isLive]

end Owner

end TopoMalloc

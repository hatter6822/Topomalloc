-- SPDX-License-Identifier: MIT
/-
Extent hooks & custom backing — the §23.4 formal hook assumption (plan 06 W10).

§23.4: "The Lean model treats hooks as abstract operations with contracts. A
production implementation using unverified hooks remains conditionally correct:
allocator correctness assumes hook correctness."

This module makes that *conditional correctness* precise. It models the §23.2 hook
operations (`alloc`, the sub-range `commit`/`decommit`/`purge`, and `split`/`merge`)
as abstract operations on `Range` geometry, states the §23.3 contracts as
hypotheses, and proves that — **under those contracts** — the operations preserve
the well-formedness core the allocator depends on:

* `alloc` honouring its no-overlap contract keeps the allocator's ranges pairwise
  disjoint (`WellFormed.rangesDisjoint`);
* `split`/`merge` keep the region tiled and preserve disjointness from every other
  extent (the §18.4 / `span_split`/`span_merge` geometry, here for hook-driven
  backing);
* a `commit`/`decommit`/`purge` confined to its sub-range (the §23.3 sub-range
  contract) touches no *other* extent.

Each theorem **consumes the §23.3 contract as a hypothesis and concludes
well-formedness**, so it is literally the §23.4 statement "allocator correctness
assumes hook correctness". The Rust `HookProvider`
(`crates/topo-core/src/hooks.rs`) *enforces* the cheap, load-bearing half of the
contracts at runtime (alignment, size, sub-range) and rejects a violation rather
than trusting it — so for those the conditional correctness here is discharged, and
only the genuinely unverifiable hook behaviour (a custom backing's own bookkeeping)
is assumed, exactly as §23.4 intends.

The model is sorry-free and rests only on the standard axioms (it reuses the
`Range` geometry of `Types.lean`, whose lemmas are `omega`-discharged).
-/
import TopoMalloc.Types
import TopoMalloc.ExtentState

namespace TopoMalloc
namespace ExtentHooks

open Range

/-- The §23.3 contract on an `alloc`-hook result `r`, for a request of `size` bytes
at alignment `align`, against the set `existing` of ranges the allocator already
holds:

* **aligned** — `r.base` is a multiple of `align` (§23.3 "returned ranges are
  aligned");
* **atLeast** — `size ≤ r.len` (§23.3 "at least requested size");
* **disjoint** — `r` overlaps no `existing` range (§23.3 "do not overlap existing
  allocator ranges"). -/
structure AllocContract (r : Range) (size align : Nat) (existing : List Range) : Prop where
  /-- The returned base is `align`-aligned. -/
  aligned : align ∣ r.base
  /-- The returned range is at least the requested size. -/
  atLeast : size ≤ r.len
  /-- The returned range overlaps nothing the allocator already holds. -/
  disjoint : ∀ e ∈ existing, Disjoint e r

/-- **§23.4 conditional correctness for `alloc`.** If the hook honours its contract
— in particular the no-overlap clause — then prepending its range to the allocator's
ranges keeps them **pairwise disjoint** (the `WellFormed.rangesDisjoint` core). The
allocator's disjointness is thus conditional on the hook's no-overlap guarantee:
assume the contract, conclude well-formedness. -/
theorem alloc_preserves_disjoint
    {r : Range} {size align : Nat} {existing : List Range}
    (pairwise : ∀ a ∈ existing, ∀ b ∈ existing, a ≠ b → Disjoint a b)
    (c : AllocContract r size align existing) :
    ∀ a ∈ r :: existing, ∀ b ∈ r :: existing, a ≠ b → Disjoint a b := by
  intro a ha b hb hab
  rcases List.mem_cons.mp ha with rfl | ha'
  · rcases List.mem_cons.mp hb with rfl | hb'
    · exact absurd rfl hab                  -- a = b = r contradicts a ≠ b
    · exact (c.disjoint b hb').symm         -- a = r, b ∈ existing: Disjoint b r → r b
  · rcases List.mem_cons.mp hb with rfl | hb'
    · exact c.disjoint a ha'                -- b = r, a ∈ existing: Disjoint a r
    · exact pairwise a ha' b hb' hab        -- both in existing

/-- The request is satisfiable from a contract-honouring `alloc` (the size half). -/
theorem alloc_satisfies_size {r : Range} {size align : Nat} {existing : List Range}
    (c : AllocContract r size align existing) : size ≤ r.len := c.atLeast

/-- The request's alignment is honoured by a contract-honouring `alloc`. -/
theorem alloc_satisfies_align {r : Range} {size align : Nat} {existing : List Range}
    (c : AllocContract r size align existing) : align ∣ r.base := c.aligned

-- ---------------------------------------------------------------------------
-- Split (§23.2 `split` / §18.4)
-- ---------------------------------------------------------------------------

/-- The prefix half of splitting `r` at the cut point `cut`: `[base, base + cut)`. -/
def splitPrefix (r : Range) (cut : Nat) : Range := ⟨r.base, cut⟩

/-- The suffix half of splitting `r` at `cut`: `[base + cut, base + len)`. -/
def splitSuffix (r : Range) (cut : Nat) : Range := ⟨r.base + cut, r.len - cut⟩

/-- The two halves of a split are disjoint (they share only the cut point). -/
theorem split_disjoint (r : Range) (cut : Nat) :
    Disjoint (splitPrefix r cut) (splitSuffix r cut) := by
  simp only [splitPrefix, splitSuffix, Range.Disjoint, Range.stop]; omega

/-- The two halves **tile** the original: the prefix starts at `r.base` and the
suffix ends at `r.stop` (so their union is exactly `r`). The §18.4 split geometry. -/
theorem split_tiles (r : Range) (cut : Nat) (h : cut ≤ r.len) :
    (splitPrefix r cut).base = r.base ∧ (splitSuffix r cut).stop = r.stop := by
  refine ⟨rfl, ?_⟩
  simp only [splitSuffix, Range.stop]
  omega

/-- Each half is a subset of the original. -/
theorem splitPrefix_subset (r : Range) (cut : Nat) (h : cut ≤ r.len) :
    Subset (splitPrefix r cut) r := by
  simp only [splitPrefix, Range.Subset, Range.stop]; omega

theorem splitSuffix_subset (r : Range) (cut : Nat) (h : cut ≤ r.len) :
    Subset (splitSuffix r cut) r := by
  simp only [splitSuffix, Range.Subset, Range.stop]; omega

/-- **§23.4 conditional correctness for `split`.** Splitting an extent preserves
disjointness from every *other* extent: anything disjoint from the whole extent is
disjoint from each half. So a `split` hook (which only re-describes geometry the
allocator already owns) can never make two live ranges overlap. -/
theorem split_preserves_disjoint_from {r o : Range} (cut : Nat)
    (h : cut ≤ r.len) (hdo : Disjoint o r) :
    Disjoint o (splitPrefix r cut) ∧ Disjoint o (splitSuffix r cut) :=
  ⟨hdo.of_subset_right (splitPrefix_subset r cut h),
   hdo.of_subset_right (splitSuffix_subset r cut h)⟩

-- ---------------------------------------------------------------------------
-- Merge (§23.2 `merge` / §18.4)
-- ---------------------------------------------------------------------------

/-- Merging the address-adjacent `left` and `right` (with `left.stop = right.base`)
into one range `[left.base, left.base + left.len + right.len)`. -/
def merged (left right : Range) : Range := ⟨left.base, left.len + right.len⟩

/-- The merged range **covers** both: it starts at `left.base` and (given adjacency)
ends at `right.stop`. The §18.4 merge geometry. -/
theorem merged_covers {left right : Range} (hadj : left.stop = right.base) :
    (merged left right).base = left.base ∧ (merged left right).stop = right.stop := by
  refine ⟨rfl, ?_⟩
  simp only [merged, Range.stop] at hadj ⊢
  omega

/-- **§23.4 conditional correctness for `merge`.** Merging two adjacent extents
preserves disjointness from every other (non-empty) extent: anything disjoint from
both halves is disjoint from their union. (`Valid o` excludes the degenerate
empty range, which is vacuously "disjoint" from each half yet straddles the cut.)
So a `merge` hook cannot make a live range overlap the coalesced extent. -/
theorem merge_preserves_disjoint_from {left right o : Range}
    (hadj : left.stop = right.base) (hov : Valid o)
    (hl : Disjoint o left) (hr : Disjoint o right) :
    Disjoint o (merged left right) := by
  simp only [merged, Range.Disjoint, Range.Valid, Range.stop] at *; omega

-- ---------------------------------------------------------------------------
-- Sub-range commit / decommit / purge (§23.2 / §23.3 "subranges only")
-- ---------------------------------------------------------------------------

/-- The §23.3 sub-range contract for a `commit`/`decommit`/`purge`: the affected
range `sub` is contained in the extent `region` it acts on. -/
def SubrangeOf (sub region : Range) : Prop := Subset sub region

instance (sub region : Range) : Decidable (SubrangeOf sub region) := by
  unfold SubrangeOf; infer_instance

/-- **§23.4 conditional correctness for a sub-range op.** A `commit`/`decommit`/
`purge` confined to a sub-range of its extent (the §23.3 contract) touches no
*other* extent: anything disjoint from the whole `region` is disjoint from the
affected `sub`-range. So a hook honouring the sub-range contract cannot disturb a
neighbouring live object — the runtime guard `HookProvider::check_subrange`
enforces exactly this `SubrangeOf` premise. -/
theorem subrange_touches_no_other {sub region other : Range}
    (hsub : SubrangeOf sub region) (hdisj : Disjoint other region) :
    Disjoint other sub :=
  hdisj.of_subset_right hsub

-- ---------------------------------------------------------------------------
-- Non-vacuity: the contracts are satisfiable (nothing above is proved vacuously)
-- ---------------------------------------------------------------------------

/-- A concrete contract-honouring `alloc`: a 16-byte, 16-aligned range at address
0x2000 satisfies a `(size = 16, align = 16)` request against a disjoint neighbour at
0x1000, witnessing that `AllocContract` is inhabited (the conditional theorems are
not vacuous). -/
theorem allocContract_inhabited :
    AllocContract ⟨0x2000, 16⟩ 16 16 [⟨0x1000, 16⟩] :=
  { aligned := by decide
    atLeast := by decide
    disjoint := by
      intro e he
      simp only [List.mem_singleton] at he
      subst he
      simp only [Range.Disjoint, Range.stop]; omega }

/-- The split halves of a non-trivial extent are themselves non-empty and tile it —
a concrete witness that `split_tiles`/`split_disjoint` are about a real subdivision. -/
example : Disjoint (splitPrefix ⟨0, 8⟩ 3) (splitSuffix ⟨0, 8⟩ 3)
    ∧ (splitSuffix ⟨0, 8⟩ 3).stop = (⟨0, 8⟩ : Range).stop := by
  refine ⟨split_disjoint _ _, ?_⟩
  exact (split_tiles ⟨0, 8⟩ 3 (by decide)).2

end ExtentHooks
end TopoMalloc

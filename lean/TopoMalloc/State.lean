-- SPDX-License-Identifier: MIT
/-
The abstract allocator `State` and its ownership map (SPEC §33.2, plan 02 W1-5).

The model is an *ownership map*: every block is a slot with a fixed byte `range`,
a `span`, and a size class `sc`; the allocator state is *who owns each slot*. The
front/middle/back ends move a slot between owners (`live`, the per-CPU/thread/
transfer caches, the central/backend free lists, quarantine, released) without
changing its geometry — exactly the §7.2 object state machine.

The load-bearing primitive is `setOwner`, which relabels one block and touches
*nothing else*. Because it preserves every block's id/range/span/sc and all of the
span/pagemap/hugepage structure, the range-based well-formedness clauses are
preserved "for free", and the frame conditions the conservation theorems (§33.4)
need are definitional. Span split/merge, release, and arena destroy are the only
transitions that change geometry, and they are localized.
-/
import TopoMalloc.Types
import TopoMalloc.SizeClass
import TopoMalloc.Generated.SizeClasses

namespace TopoMalloc

/-- One block (object slot). Its geometry (`range`, `span`, `sc`) is fixed; only
`owner` changes across transitions. -/
structure Block where
  id : BlockId
  range : Range
  owner : Owner
  sc : SizeClassId
  span : SpanId
  deriving Repr, DecidableEq

/-- A span descriptor (§16.2): a contiguous run carved into `objCount` slots of
class `sc`, owned by `arena`, optionally backed by a hugepage. -/
structure SpanDesc where
  id : SpanId
  range : Range
  arena : ArenaId
  sc : SizeClassId
  hugepage : Option HugePageId
  objCount : Nat
  deriving Repr, DecidableEq

/-- A hugepage descriptor (§19.2): the backing range a span may be packed into. -/
structure HugePageDesc where
  id : HugePageId
  range : Range
  deriving Repr, DecidableEq

/-- The abstract allocator state (§33.2). `pagemap` maps a page number to the
span covering it (§17.1); `released` records ranges handed back to the OS/provider
(§20.1). -/
structure State where
  blocks : List Block
  spans : List SpanDesc
  hugepages : List HugePageDesc
  pagemap : List (Nat × SpanId)
  released : List Range
  deriving Repr

namespace State

/-- The block with the given id, if present. -/
def blockById (s : State) (b : BlockId) : Option Block :=
  s.blocks.find? (fun blk => decide (blk.id = b))

/-- The owner of a block, if present (the §33.2 `OwnerOf`). -/
def ownerOf (s : State) (b : BlockId) : Option Owner := (s.blockById b).map (·.owner)

/-- The span descriptor with the given id, if present. -/
def spanById (s : State) (sp : SpanId) : Option SpanDesc :=
  s.spans.find? (fun d => decide (d.id = sp))

/-- A block is live when its owner is `Owner.live`. -/
def IsLive (s : State) (b : BlockId) : Prop := s.ownerOf b = some Owner.live

/-- The number of blocks currently owned by `o` (used by the capacity clause and
the conservation theorems). -/
def countOwned (s : State) (o : Owner) : Nat :=
  s.blocks.countP (fun blk => decide (blk.owner = o))

/- ----------------------------------------------------------------------- -/
/- `setOwner`: relabel one block, frame everything else.                   -/
/- ----------------------------------------------------------------------- -/

/-- The relabel kernel: map block `b` to owner `o`, leave every other block and
every non-owner field untouched. -/
def relabel (b : BlockId) (o : Owner) (blk : Block) : Block :=
  if blk.id = b then { blk with owner := o } else blk

@[simp] theorem relabel_id (b o blk) : (relabel b o blk).id = blk.id := by
  unfold relabel; split <;> rfl
@[simp] theorem relabel_range (b o blk) : (relabel b o blk).range = blk.range := by
  unfold relabel; split <;> rfl
@[simp] theorem relabel_sc (b o blk) : (relabel b o blk).sc = blk.sc := by
  unfold relabel; split <;> rfl
@[simp] theorem relabel_span (b o blk) : (relabel b o blk).span = blk.span := by
  unfold relabel; split <;> rfl

/-- Off the target id, `relabel` is the identity (the frame at the element level). -/
theorem relabel_of_ne (b : BlockId) (o : Owner) {blk : Block} (h : blk.id ≠ b) :
    relabel b o blk = blk := by unfold relabel; rw [if_neg h]

/-- On the target id, `relabel` just rewrites the owner. -/
theorem relabel_of_eq (b : BlockId) (o : Owner) {blk : Block} (h : blk.id = b) :
    relabel b o blk = { blk with owner := o } := by unfold relabel; rw [if_pos h]

/-- Relabel block `b` to owner `o`. The only state change is that block's owner. -/
def setOwner (s : State) (b : BlockId) (o : Owner) : State :=
  { s with blocks := s.blocks.map (relabel b o) }

variable (s : State) (b : BlockId) (o : Owner)

@[simp] theorem setOwner_blocks :
    (s.setOwner b o).blocks = s.blocks.map (relabel b o) := rfl
@[simp] theorem setOwner_spans : (s.setOwner b o).spans = s.spans := rfl
@[simp] theorem setOwner_hugepages : (s.setOwner b o).hugepages = s.hugepages := rfl
@[simp] theorem setOwner_pagemap : (s.setOwner b o).pagemap = s.pagemap := rfl
@[simp] theorem setOwner_released : (s.setOwner b o).released = s.released := rfl

/-- The list of block id's is unchanged by `setOwner` (clause 2 backbone). -/
theorem setOwner_map_id :
    (s.setOwner b o).blocks.map (·.id) = s.blocks.map (·.id) := by
  simp only [setOwner_blocks, List.map_map]
  exact List.map_congr_left (fun x _ => by simp only [Function.comp, relabel_id])

/-- The list of block ranges is unchanged by `setOwner` (clause 1 backbone). -/
theorem setOwner_map_range :
    (s.setOwner b o).blocks.map (·.range) = s.blocks.map (·.range) := by
  simp only [setOwner_blocks, List.map_map]
  exact List.map_congr_left (fun x _ => by simp only [Function.comp, relabel_range])

/-- Any owner-independent count is unchanged by `setOwner` (used to preserve the
span-occupancy clause and to do conservation accounting on spans). -/
theorem setOwner_countP_of_owner_indep (p : Block → Bool)
    (hp : ∀ blk (o' : Owner), p { blk with owner := o' } = p blk) :
    (s.setOwner b o).blocks.countP p = s.blocks.countP p := by
  simp only [setOwner_blocks, List.countP_map]
  apply List.countP_congr
  intro x _
  simp only [Function.comp, relabel]
  split
  · rw [hp]
  · rfl

/-- Every block of `setOwner b o` shares its id/range/sc/span with an original
block (only the owner may differ). The structural half of the frame condition. -/
theorem setOwner_mem {blk' : Block} (h : blk' ∈ (s.setOwner b o).blocks) :
    ∃ blk ∈ s.blocks, blk'.id = blk.id ∧ blk'.range = blk.range ∧
      blk'.sc = blk.sc ∧ blk'.span = blk.span := by
  simp only [setOwner_blocks, List.mem_map] at h
  obtain ⟨blk, hblk, rfl⟩ := h
  exact ⟨blk, hblk, by simp, by simp, by simp, by simp⟩

/-- Conversely, every original block has an image under `setOwner` with the same
id/range/sc/span. -/
theorem setOwner_mem' {blk : Block} (h : blk ∈ s.blocks) :
    ∃ blk' ∈ (s.setOwner b o).blocks, blk'.id = blk.id ∧ blk'.range = blk.range ∧
      blk'.sc = blk.sc ∧ blk'.span = blk.span := by
  refine ⟨relabel b o blk, ?_, by simp, by simp, by simp, by simp⟩
  simp only [setOwner_blocks, List.mem_map]
  exact ⟨blk, h, rfl⟩

/-- Looking up any block after `setOwner` is the relabel of the original lookup:
`setOwner` preserves id's, so the same slot is found, only its owner may change. -/
theorem blockById_setOwner (b' : BlockId) :
    (s.setOwner b o).blockById b' = (s.blockById b').map (relabel b o) := by
  have hpred : ((fun blk => decide (blk.id = b')) ∘ relabel b o)
      = (fun blk : Block => decide (blk.id = b')) := by
    funext blk
    show decide ((relabel b o blk).id = b') = decide (blk.id = b')
    rw [relabel_id]
  unfold blockById State.setOwner
  rw [List.find?_map, hpred]

/-- Frame condition: the owner of every block other than `b` is unchanged. -/
theorem setOwner_ownerOf_ne (s : State) (b : BlockId) (o : Owner) {b' : BlockId}
    (h : b' ≠ b) : (s.setOwner b o).ownerOf b' = s.ownerOf b' := by
  simp only [ownerOf, blockById_setOwner, Option.map_map]
  cases hf : s.blockById b' with
  | none => simp
  | some blk =>
    have hid : blk.id = b' := by
      have := List.find?_some hf; simpa [blockById] using this
    have hrel : relabel b o blk = blk := by
      unfold relabel; rw [if_neg (by rw [hid]; exact h)]
    simp [hrel]

/-- After `setOwner b o`, if block `b` is present its owner is exactly `o`. -/
theorem setOwner_ownerOf_self (s : State) (b : BlockId) (o : Owner)
    (h : (s.blockById b).isSome) : (s.setOwner b o).ownerOf b = some o := by
  simp only [ownerOf, blockById_setOwner, Option.map_map]
  cases hf : s.blockById b with
  | none => rw [hf] at h; simp at h
  | some blk =>
    have hid : blk.id = b := by
      have := List.find?_some hf; simpa [blockById] using this
    have hrel : relabel b o blk = { blk with owner := o } := by
      unfold relabel; rw [if_pos hid]
    simp [hrel]

end State

end TopoMalloc

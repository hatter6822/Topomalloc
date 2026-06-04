-- SPDX-License-Identifier: MIT
/-
The `WellFormed` predicate (SPEC §33.3, plan 02 W1-3).

`WellFormed` is a *total* predicate (defined on every `State`) built from fourteen
named clauses, each a standalone `def` cross-referenced to its SPEC bullet so a
transition proof can cite exactly the ones it preserves. Eleven clauses are the
§33.3 bullets; three more — `WfSlabLayout` (the §9.5/§16.3 slab-layout backbone,
"alignment is sufficient"), `WfSpansDisjoint` (the §16.1/§22.7 span-tiling
backbone), and `WfBlockSpanClass` (the §16.2 single-class-per-span backbone) — make
explicit what the §33.4 `malloc_success_…` and span split/merge theorems need and
what the SPEC's eleven-bullet *minimum* leaves implicit.

Block-id uniqueness (`ownerOf` single-valued, §33.3.2) is clause 2 (`WfUniqueOwner`);
the byte-exact quota accounting (`ArenaQuotaExact`, §36.17) reuses *that* clause for
its "a relabel touches exactly one slot" lemma — there is no separate id-uniqueness
clause.

Clause 1 is stated for *all* blocks, not only live ones. This strengthens §33.3
bullet 1 ("live ranges disjoint", §8.3) to the slab-structural truth (§9.5:
"object ranges in a slab are pairwise disjoint") — every slot is disjoint
regardless of liveness. `WellFormed.liveRangesDisjoint` recovers the exact SPEC
statement, so nothing is weakened.

`setOwner` (the relabel primitive) preserves the nine *owner-independent* clauses
unconditionally; the four owner-dependent clauses (caches/central consistency,
released-no-live, capacities) are re-established per transition. Those preservation
lemmas live here so every transition reuses them.
-/
import TopoMalloc.State

namespace TopoMalloc

open State

/-- The generated size-class row for a class id, if in range. -/
def sizeClassRow (sc : SizeClassId) : Option SizeClassRow := Generated.sizeClasses[sc]?

/-- The per-CPU hard capacity for a class (`0` for an out-of-range id, which then
admits no cached blocks). -/
def maxLocalCapacity (sc : SizeClassId) : Nat :=
  match sizeClassRow sc with
  | some r => r.maxLocalCapacity
  | none => 0

/- ----------------------------------------------------------------------- -/
/- The fourteen clauses (§33.3 bullets 1–11 + the §9.5/§16 structural backbones). -/
/- ----------------------------------------------------------------------- -/

/-- Clause 1 (§33.3.1, §8.3, §9.5): block ranges are pairwise disjoint. Stated for
all blocks (strengthening "live ranges disjoint"; see module note). -/
def WfRangesDisjoint (s : State) : Prop := (s.blocks.map (·.range)).Pairwise Range.Disjoint

/-- Clause 2 (§33.3.2, §8.1/§8.2): every block has exactly one owner — equivalently,
block ids are distinct, so `ownerOf` is single-valued. -/
def WfUniqueOwner (s : State) : Prop := (s.blocks.map (·.id)).Nodup

/-- Clause 3 (§33.3.3, §11.2): a per-CPU/thread/transfer cache holds only blocks of
its own size class. -/
def WfCachesConsistent (s : State) : Prop :=
  ∀ blk ∈ s.blocks, ∀ sc, blk.owner.cacheSizeClass? = some sc → blk.sc = sc

/-- Clause 4 (§33.3.4, §14.6/§22.7): a block held by a central/backend owner carries
that owner's arena — recovered from the block's span. -/
def WfCentralConsistent (s : State) : Prop :=
  ∀ blk ∈ s.blocks, ∀ a, blk.owner.arena? = some a →
    ∃ d ∈ s.spans, d.id = blk.span ∧ d.arena = a

/-- Clause 5 (§33.3.5, §16.3): every block's range lies within its span's range
(and the span exists). -/
def WfObjectsFitSpans (s : State) : Prop :=
  ∀ blk ∈ s.blocks, ∃ d ∈ s.spans, d.id = blk.span ∧ blk.range.Subset d.range

/-- Clause 6 (§33.3.6, §16.4): a span's recorded object count agrees with the blocks
attributed to it (the abstract analogue of "bitmaps agree with ownership"), and that
count never exceeds the slab capacity its size class is carved for (§9.5). -/
def WfBitmapsAgree (s : State) : Prop :=
  ∀ d ∈ s.spans,
    s.blocks.countP (fun blk => decide (blk.span = d.id)) = d.objCount ∧
      ∀ r, sizeClassRow d.sc = some r → d.objCount ≤ r.objectsPerSlab

/-- Clause 7 (§33.3.7, §17.2): every pagemap entry names an existing span whose
range covers that whole page. -/
def WfPagemapAgrees (s : State) : Prop :=
  ∀ p sp, (p, sp) ∈ s.pagemap → ∃ d ∈ s.spans, d.id = sp ∧
    d.range.base ≤ p * Generated.pageSize ∧ (p + 1) * Generated.pageSize ≤ d.range.stop

/-- Clause 8 (§33.3.8, §19.8): a span backed by a hugepage lies within that
hugepage's range (occupancy agrees with spans). -/
def WfHugepagesAgree (s : State) : Prop :=
  ∀ d ∈ s.spans, ∀ h, d.hugepage = some h →
    ∃ hp ∈ s.hugepages, hp.id = h ∧ d.range.Subset hp.range

/-- Clause 9 (§33.3.9, §22.7): span ids are distinct, so each block's arena (read
through its span) is well-defined — no span belongs to two arenas. -/
def WfArenaUnique (s : State) : Prop := (s.spans.map (·.id)).Nodup

/-- Clause 10 (§33.3.10, §20.1/§21.6): released ranges contain no live block. -/
def WfReleasedNoLive (s : State) : Prop :=
  ∀ r ∈ s.released, ∀ blk ∈ s.blocks, blk.owner = Owner.live → Range.Disjoint r blk.range

/-- Clause 11 (§33.3.11, §11.5): each per-CPU cache respects its capacity. -/
def WfCacheCapacities (s : State) : Prop :=
  ∀ cpu sc, s.countOwned (Owner.cpuCache cpu sc) ≤ maxLocalCapacity sc

/-- Clause 12 (supplement, §9.5/§16.3): slab layout — a block's length is its class
size and its base is class-aligned, so "alignment is sufficient". -/
def WfSlabLayout (s : State) : Prop :=
  ∀ blk ∈ s.blocks, ∃ r, sizeClassRow blk.sc = some r ∧
    blk.range.len = r.size ∧ r.align ∣ blk.range.base

/-- Clause 13 (supplement, §16.1/§22.7): span ranges are pairwise disjoint — spans
tile the address space without overlap. This is what span split/merge preserve and
what makes the §22.7 isolation invariant geometric, not just id-level. -/
def WfSpansDisjoint (s : State) : Prop := (s.spans.map (·.range)).Pairwise Range.Disjoint

/-- Clause 14 (supplement, §16.2/§16.3): a block's size class matches its span's size
class — a span is carved into slots of a *single* class, so every block in it is that
class. Without this, the occupancy/capacity clause (6) and slab layout (12) could
describe a block by one class while its span's metadata uses another. -/
def WfBlockSpanClass (s : State) : Prop :=
  ∀ blk ∈ s.blocks, ∀ d ∈ s.spans, d.id = blk.span → blk.sc = d.sc

/-- `WellFormed` (§33.3): the conjunction of all clauses, with named projections so a
transition proof cites exactly what it preserves. Clauses 1–11 are the §33.3 bullets;
12 (`slabLayout`), 13 (`spansDisjoint`), and 14 (`blockSpanClass`) are the §9.5/§16
structural backbones the §33.4 theorems consume. -/
structure WellFormed (s : State) : Prop where
  rangesDisjoint : WfRangesDisjoint s
  uniqueOwner : WfUniqueOwner s
  cachesConsistent : WfCachesConsistent s
  centralConsistent : WfCentralConsistent s
  objectsFitSpans : WfObjectsFitSpans s
  bitmapsAgree : WfBitmapsAgree s
  pagemapAgrees : WfPagemapAgrees s
  hugepagesAgree : WfHugepagesAgree s
  arenaUnique : WfArenaUnique s
  releasedNoLive : WfReleasedNoLive s
  cacheCapacities : WfCacheCapacities s
  slabLayout : WfSlabLayout s
  spansDisjoint : WfSpansDisjoint s
  blockSpanClass : WfBlockSpanClass s

/-- A symmetric pairwise relation holds between any two *distinct* members of a
list (no `Nodup` needed: distinct values cannot share an index). -/
theorem pairwise_rel_of_ne {α} {R : α → α → Prop} {l : List α} (hsymm : ∀ a b, R a b → R b a)
    (hp : l.Pairwise R) {x y : α} (hx : x ∈ l) (hy : y ∈ l) (hxy : x ≠ y) : R x y := by
  rw [List.mem_iff_getElem] at hx hy
  obtain ⟨i, hi, rfl⟩ := hx
  obtain ⟨j, hj, rfl⟩ := hy
  rw [List.pairwise_iff_getElem] at hp
  rcases Nat.lt_trichotomy i j with h | h | h
  · exact hp i j hi hj h
  · subst h; exact absurd rfl hxy
  · exact hsymm _ _ (hp j i hj hi h)

/-- Unique ownership (clause 2) is injectivity of `id` on the block list: two listed
blocks with the same id are the same block. -/
theorem WfUniqueOwner.eq_of_id_eq {s : State} (h : WfUniqueOwner s) {b1 b2 : Block}
    (h1 : b1 ∈ s.blocks) (h2 : b2 ∈ s.blocks) (hid : b1.id = b2.id) : b1 = b2 := by
  have hh : (s.blocks.map (·.id)).Pairwise (· ≠ ·) := h
  have hpw : s.blocks.Pairwise (fun a c => a.id ≠ c.id) := List.pairwise_map.mp hh
  by_cases hc : b1 = b2
  · exact hc
  · exact absurd hid (pairwise_rel_of_ne (fun _ _ hh2 => hh2.symm) hpw h1 h2 hc)

/-- Any two blocks with distinct ids have disjoint ranges (the form the
`malloc_success` disjointness obligation consumes). -/
theorem WellFormed.disjoint_of_id_ne {s : State} (hwf : WellFormed s) {blk1 blk2 : Block}
    (h1 : blk1 ∈ s.blocks) (h2 : blk2 ∈ s.blocks) (hne : blk1.id ≠ blk2.id) :
    Range.Disjoint blk1.range blk2.range := by
  have hp : (s.blocks).Pairwise (fun a b => Range.Disjoint a.range b.range) := by
    have hr := hwf.rangesDisjoint
    rwa [WfRangesDisjoint, List.pairwise_map] at hr
  refine pairwise_rel_of_ne (fun _ _ h => h.symm) hp h1 h2 ?_
  intro hcon; rw [hcon] at hne; exact hne rfl

/-- The empty allocator state. -/
def State.empty : State := ⟨[], [], [], [], []⟩

/-- `WellFormed` is **inhabited** — the empty state satisfies every clause (vacuously).
A witness that the model and its preservation theorems are not vacuously true. -/
theorem wellFormed_empty : WellFormed State.empty where
  rangesDisjoint := by simp [WfRangesDisjoint, State.empty]
  uniqueOwner := by simp [WfUniqueOwner, State.empty]
  cachesConsistent := by simp [WfCachesConsistent, State.empty]
  centralConsistent := by simp [WfCentralConsistent, State.empty]
  objectsFitSpans := by simp [WfObjectsFitSpans, State.empty]
  bitmapsAgree := by simp [WfBitmapsAgree, State.empty]
  pagemapAgrees := by simp [WfPagemapAgrees, State.empty]
  hugepagesAgree := by simp [WfHugepagesAgree, State.empty]
  arenaUnique := by simp [WfArenaUnique, State.empty]
  releasedNoLive := by simp [WfReleasedNoLive, State.empty]
  cacheCapacities := by
    intro cpu sc; simp only [State.countOwned, State.empty, List.countP_nil]; exact Nat.zero_le _
  slabLayout := by simp [WfSlabLayout, State.empty]
  spansDisjoint := by simp [WfSpansDisjoint, State.empty]
  blockSpanClass := by simp [WfBlockSpanClass, State.empty]

/-- Recover the exact §33.3 bullet 1: *live* ranges are pairwise disjoint. -/
theorem WellFormed.liveRangesDisjoint {s : State} (h : WellFormed s) :
    ((s.blocks.filter (fun blk => decide (blk.owner = Owner.live))).map (·.range)).Pairwise
      Range.Disjoint := by
  have hsub : (s.blocks.filter (fun blk => decide (blk.owner = Owner.live))).map (·.range)
      |>.Sublist (s.blocks.map (·.range)) :=
    (List.filter_sublist).map _
  exact List.Pairwise.sublist hsub h.rangesDisjoint

/- ----------------------------------------------------------------------- -/
/- Counting under `setOwner` (clause 11 accounting).                        -/
/- ----------------------------------------------------------------------- -/

variable (s : State) (b : BlockId) (o : Owner)

/-- Relabelling to `o` cannot raise the count of any *other* owner: the only owner
that gains a block is `o`. This is what lets the non-refill transitions preserve
the capacity clause for free. -/
theorem countOwned_setOwner_le_of_ne {o' : Owner} (hne : o' ≠ o) :
    (s.setOwner b o).countOwned o' ≤ s.countOwned o' := by
  simp only [countOwned, setOwner_blocks, List.countP_map]
  apply List.countP_mono_left
  intro x _ hx
  simp only [Function.comp, relabel] at hx ⊢
  split at hx
  · -- relabelled to o; but the predicate asks for o' ≠ o, so this branch is false
    simp only [decide_eq_true_eq] at hx
    exact absurd hx.symm hne
  · exact hx

/- ----------------------------------------------------------------------- -/
/- `setOwner` preserves the nine owner-independent clauses.                 -/
/- ----------------------------------------------------------------------- -/

theorem WfRangesDisjoint.setOwner (h : WfRangesDisjoint s) :
    WfRangesDisjoint (s.setOwner b o) := by
  unfold WfRangesDisjoint; rw [setOwner_map_range]; exact h

theorem WfUniqueOwner.setOwner (h : WfUniqueOwner s) :
    WfUniqueOwner (s.setOwner b o) := by
  unfold WfUniqueOwner; rw [setOwner_map_id]; exact h

theorem WfObjectsFitSpans.setOwner (h : WfObjectsFitSpans s) :
    WfObjectsFitSpans (s.setOwner b o) := by
  intro blk' hblk'
  obtain ⟨blk, hblk, _, hrange, _, hspan⟩ := s.setOwner_mem b o hblk'
  obtain ⟨d, hd, hdid, hsub⟩ := h blk hblk
  refine ⟨d, by simpa using hd, ?_, ?_⟩
  · rw [hspan]; exact hdid
  · rw [hrange]; exact hsub

theorem WfBitmapsAgree.setOwner (h : WfBitmapsAgree s) :
    WfBitmapsAgree (s.setOwner b o) := by
  intro d hd
  rw [setOwner_spans] at hd
  rw [s.setOwner_countP_of_owner_indep b o (fun blk => decide (blk.span = d.id))
        (fun _ _ => rfl)]
  exact h d hd

theorem WfSpansDisjoint.setOwner (h : WfSpansDisjoint s) :
    WfSpansDisjoint (s.setOwner b o) := by
  unfold WfSpansDisjoint; rw [setOwner_spans]; exact h

theorem WfBlockSpanClass.setOwner (h : WfBlockSpanClass s) :
    WfBlockSpanClass (s.setOwner b o) := by
  intro blk' hblk' d hd hdid
  rw [setOwner_spans] at hd
  obtain ⟨blk, hblk, _, _, hsc, hspan⟩ := s.setOwner_mem b o hblk'
  rw [hsc]; exact h blk hblk d hd (hdid.trans hspan)

theorem WfPagemapAgrees.setOwner (h : WfPagemapAgrees s) :
    WfPagemapAgrees (s.setOwner b o) := by
  intro p sp hp
  simp only [setOwner_pagemap] at hp
  obtain ⟨d, hd, rest⟩ := h p sp hp
  exact ⟨d, by simpa using hd, rest⟩

theorem WfHugepagesAgree.setOwner (h : WfHugepagesAgree s) :
    WfHugepagesAgree (s.setOwner b o) := by
  intro d hd hh hhh
  rw [setOwner_spans] at hd
  obtain ⟨hp, hpm, rest⟩ := h d hd hh hhh
  exact ⟨hp, by simpa using hpm, rest⟩

theorem WfArenaUnique.setOwner (h : WfArenaUnique s) :
    WfArenaUnique (s.setOwner b o) := by
  unfold WfArenaUnique; rw [setOwner_spans]; exact h

theorem WfSlabLayout.setOwner (h : WfSlabLayout s) :
    WfSlabLayout (s.setOwner b o) := by
  intro blk' hblk'
  obtain ⟨blk, hblk, _, hrange, hsc, _⟩ := s.setOwner_mem b o hblk'
  obtain ⟨r, hrow, hlen, hdvd⟩ := h blk hblk
  refine ⟨r, ?_, ?_, ?_⟩
  · rw [hsc]; exact hrow
  · rw [hrange]; exact hlen
  · rw [hrange]; exact hdvd

/- ----------------------------------------------------------------------- -/
/- The four owner-dependent clauses, re-established per transition.         -/
/- ----------------------------------------------------------------------- -/

/-- Clause 3 under `setOwner`: preserved as long as, *if* the new owner `o` is a
cache keyed by class `sc`, block `b`'s own size class is `sc`. -/
theorem WfCachesConsistent.setOwner (h : WfCachesConsistent s)
    (hb : ∀ blk ∈ s.blocks, blk.id = b → ∀ sc, o.cacheSizeClass? = some sc → blk.sc = sc) :
    WfCachesConsistent (s.setOwner b o) := by
  intro blk' hblk' sc hsc
  simp only [setOwner_blocks, List.mem_map] at hblk'
  obtain ⟨blk, hblk, rfl⟩ := hblk'
  unfold relabel at hsc ⊢
  by_cases hid : blk.id = b
  · rw [if_pos hid] at hsc ⊢; exact hb blk hblk hid sc hsc
  · rw [if_neg hid] at hsc ⊢; exact h blk hblk sc hsc

/-- Clause 4 under `setOwner`: preserved as long as, *if* the new owner `o` is
arena-qualified to `a`, block `b`'s span carries arena `a`. -/
theorem WfCentralConsistent.setOwner (h : WfCentralConsistent s)
    (hb : ∀ blk ∈ s.blocks, blk.id = b → ∀ a, o.arena? = some a →
      ∃ d ∈ s.spans, d.id = blk.span ∧ d.arena = a) :
    WfCentralConsistent (s.setOwner b o) := by
  intro blk' hblk' a ha
  simp only [setOwner_blocks, List.mem_map] at hblk'
  obtain ⟨blk, hblk, rfl⟩ := hblk'
  rw [setOwner_spans]
  unfold relabel at ha ⊢
  by_cases hid : blk.id = b
  · rw [if_pos hid] at ha ⊢; exact hb blk hblk hid a ha
  · rw [if_neg hid] at ha ⊢; exact h blk hblk a ha

/-- Clause 10 under `setOwner` to a **non-live** owner: no new live block appears,
so released ranges still contain no live block. -/
theorem WfReleasedNoLive.setOwner_of_ne_live (hne : o ≠ Owner.live)
    (h : WfReleasedNoLive s) : WfReleasedNoLive (s.setOwner b o) := by
  intro r hr blk' hblk' hlive'
  rw [setOwner_released] at hr
  simp only [setOwner_blocks, List.mem_map] at hblk'
  obtain ⟨blk, hblk, rfl⟩ := hblk'
  unfold relabel at hlive' ⊢
  by_cases hid : blk.id = b
  · rw [if_pos hid] at hlive'; exact absurd hlive' hne
  · rw [if_neg hid] at hlive' ⊢; exact h r hr blk hblk hlive'

/-- Clause 10 under `setOwner` to `live`: preserved when block `b`'s range is
disjoint from every released range (it is allocated from committed memory, §20). -/
theorem WfReleasedNoLive.setOwner_live (h : WfReleasedNoLive s)
    (hb : ∀ blk ∈ s.blocks, blk.id = b → ∀ r ∈ s.released, Range.Disjoint r blk.range) :
    WfReleasedNoLive (s.setOwner b Owner.live) := by
  intro r hr blk' hblk' hlive'
  rw [setOwner_released] at hr
  simp only [setOwner_blocks, List.mem_map] at hblk'
  obtain ⟨blk, hblk, rfl⟩ := hblk'
  unfold relabel at hlive' ⊢
  by_cases hid : blk.id = b
  · rw [if_pos hid]; exact hb blk hblk hid r hr
  · rw [if_neg hid] at hlive' ⊢; exact h r hr blk hblk hlive'

end TopoMalloc

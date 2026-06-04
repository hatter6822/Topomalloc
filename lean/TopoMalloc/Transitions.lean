-- SPDX-License-Identifier: MIT
/-
The abstract transitions (SPEC §33.4 subjects, plan 02 W1-5) as **total functions**
on `State`. Each is tagged with the §7 state-machine transition it models.

`malloc`/`free` relabel one slot; the cache/central batch operations are folds of
`setOwner` over a batch (`relabelAll`); `releaseToOs`, `arenaReset`, and
`arenaDestroy` are the geometry-changing transitions. Span split/merge are modelled
at the range level in `Theorems/Span.lean`.

The reusable engine is `relabelAll` and the lemmas below: a batch of `setOwner`s
preserves the nine owner-independent well-formedness clauses, frames every block
outside the batch, conserves block ids, and cannot raise the count of any owner
other than the target. Every batch transition's §33.4 theorem is then a short
instantiation.
-/
import TopoMalloc.WellFormed
import TopoMalloc.Rseq

namespace TopoMalloc

open State

/- ----------------------------------------------------------------------- -/
/- `relabelAll`: relabel a whole batch to one owner, framing everything else. -/
/- ----------------------------------------------------------------------- -/

/-- Relabel every block id in `batch` to owner `o`, left to right. -/
def relabelAll (s : State) (batch : List BlockId) (o : Owner) : State :=
  batch.foldl (fun st q => st.setOwner q o) s

@[simp] theorem relabelAll_nil (s : State) (o : Owner) : relabelAll s [] o = s := rfl

theorem relabelAll_cons (s : State) (q : BlockId) (rest : List BlockId) (o : Owner) :
    relabelAll s (q :: rest) o = relabelAll (s.setOwner q o) rest o := by
  simp [relabelAll, List.foldl_cons]

/-- Lift any `setOwner`-stable predicate through a batch (the engine behind the
structural-clause preservations below). -/
theorem relabelAll_preserves (o : Owner) (P : State → Prop)
    (hstep : ∀ (s' : State) (b' : BlockId), P s' → P (s'.setOwner b' o)) :
    ∀ (s : State) (batch : List BlockId), P s → P (relabelAll s batch o) := by
  intro s batch
  induction batch generalizing s with
  | nil => intro h; exact h
  | cons b rest ih => intro h; rw [relabelAll_cons]; exact ih _ (hstep s b h)

@[simp] theorem relabelAll_spans (s : State) (batch : List BlockId) (o : Owner) :
    (relabelAll s batch o).spans = s.spans := by
  induction batch generalizing s with
  | nil => rfl
  | cons b rest ih => rw [relabelAll_cons, ih]; rfl

@[simp] theorem relabelAll_released (s : State) (batch : List BlockId) (o : Owner) :
    (relabelAll s batch o).released = s.released := by
  induction batch generalizing s with
  | nil => rfl
  | cons b rest ih => rw [relabelAll_cons, ih]; rfl

/-- Block ids are conserved by a batch relabel: no slot is created or destroyed. -/
theorem relabelAll_map_id (s : State) (batch : List BlockId) (o : Owner) :
    (relabelAll s batch o).blocks.map (·.id) = s.blocks.map (·.id) := by
  induction batch generalizing s with
  | nil => rfl
  | cons b rest ih => rw [relabelAll_cons, ih]; exact s.setOwner_map_id b o

/-- Frame condition for a batch: any block outside the batch keeps its owner. -/
theorem relabelAll_ownerOf_not_mem (o : Owner) {q : BlockId} :
    ∀ (s : State) (batch : List BlockId), q ∉ batch →
      (relabelAll s batch o).ownerOf q = s.ownerOf q := by
  intro s batch
  induction batch generalizing s with
  | nil => intro _; rfl
  | cons b rest ih =>
    intro hq
    simp only [List.mem_cons, not_or] at hq
    rw [relabelAll_cons, ih _ hq.2, setOwner_ownerOf_ne s b o hq.1]

/-- A batch block relabelled to a **non-live** owner is not live afterwards (whether or
not it is present): its owner is `o ≠ live`, or it does not exist. -/
theorem relabelAll_ownerOf_mem_ne_live (o : Owner) (ho : o ≠ Owner.live) {q : BlockId} :
    ∀ (s : State) (batch : List BlockId), q ∈ batch →
      (relabelAll s batch o).ownerOf q ≠ some Owner.live := by
  intro s batch
  induction batch generalizing s with
  | nil => intro h; exact absurd h (by simp)
  | cons b rest ih =>
    intro hq
    rw [relabelAll_cons]
    by_cases hqr : q ∈ rest
    · exact ih _ hqr
    · have hqb : q = b := (List.mem_cons.mp hq).resolve_right hqr
      subst hqb
      rw [relabelAll_ownerOf_not_mem o (s.setOwner q o) rest hqr]
      cases hp : s.blockById q with
      | none => simp [State.ownerOf, blockById_setOwner, hp]
      | some blk =>
        rw [setOwner_ownerOf_self s q o (by rw [hp]; rfl)]
        exact fun hc => ho (Option.some.inj hc)

/-- **Live-set conservation for a free-batch relabel.** If the target owner is not
`live` and every batch block was already non-live, then relabelling the batch changes
*no* address's liveness — the live set is exactly preserved. This is the real content of
"ownership conservation" for cache refill/flush. -/
theorem relabelAll_isLive_iff (o : Owner) (ho : o ≠ Owner.live) (s : State)
    (batch : List BlockId) (hfree : ∀ q ∈ batch, ¬ s.IsLive q) :
    ∀ q, (relabelAll s batch o).IsLive q ↔ s.IsLive q := by
  intro q
  by_cases hq : q ∈ batch
  · exact iff_of_false (relabelAll_ownerOf_mem_ne_live o ho s batch hq) (hfree q hq)
  · show (relabelAll s batch o).ownerOf q = some Owner.live ↔ s.ownerOf q = some Owner.live
    rw [relabelAll_ownerOf_not_mem o s batch hq]

/-- No owner other than the target `o` gains blocks under a batch relabel
(the capacity-clause accounting for non-refill transitions). -/
theorem relabelAll_countOwned_le_of_ne (o : Owner) {o' : Owner} (hne : o' ≠ o) :
    ∀ (s : State) (batch : List BlockId),
      (relabelAll s batch o).countOwned o' ≤ s.countOwned o' := by
  intro s batch
  induction batch generalizing s with
  | nil => exact Nat.le_refl _
  | cons b rest ih =>
    rw [relabelAll_cons]
    exact Nat.le_trans (ih _) (countOwned_setOwner_le_of_ne s b o hne)

/- The nine owner-independent clauses survive a batch relabel. -/

theorem relabelAll_rangesDisjoint (s : State) (batch : List BlockId) (o : Owner)
    (h : WfRangesDisjoint s) : WfRangesDisjoint (relabelAll s batch o) :=
  relabelAll_preserves o WfRangesDisjoint (fun s' b' h => WfRangesDisjoint.setOwner s' b' o h) s batch h

theorem relabelAll_uniqueOwner (s : State) (batch : List BlockId) (o : Owner)
    (h : WfUniqueOwner s) : WfUniqueOwner (relabelAll s batch o) :=
  relabelAll_preserves o WfUniqueOwner (fun s' b' h => WfUniqueOwner.setOwner s' b' o h) s batch h

theorem relabelAll_objectsFitSpans (s : State) (batch : List BlockId) (o : Owner)
    (h : WfObjectsFitSpans s) : WfObjectsFitSpans (relabelAll s batch o) :=
  relabelAll_preserves o WfObjectsFitSpans (fun s' b' h => WfObjectsFitSpans.setOwner s' b' o h) s batch h

theorem relabelAll_bitmapsAgree (s : State) (batch : List BlockId) (o : Owner)
    (h : WfBitmapsAgree s) : WfBitmapsAgree (relabelAll s batch o) :=
  relabelAll_preserves o WfBitmapsAgree (fun s' b' h => WfBitmapsAgree.setOwner s' b' o h) s batch h

theorem relabelAll_pagemapAgrees (s : State) (batch : List BlockId) (o : Owner)
    (h : WfPagemapAgrees s) : WfPagemapAgrees (relabelAll s batch o) :=
  relabelAll_preserves o WfPagemapAgrees (fun s' b' h => WfPagemapAgrees.setOwner s' b' o h) s batch h

theorem relabelAll_hugepagesAgree (s : State) (batch : List BlockId) (o : Owner)
    (h : WfHugepagesAgree s) : WfHugepagesAgree (relabelAll s batch o) :=
  relabelAll_preserves o WfHugepagesAgree (fun s' b' h => WfHugepagesAgree.setOwner s' b' o h) s batch h

theorem relabelAll_arenaUnique (s : State) (batch : List BlockId) (o : Owner)
    (h : WfArenaUnique s) : WfArenaUnique (relabelAll s batch o) :=
  relabelAll_preserves o WfArenaUnique (fun s' b' h => WfArenaUnique.setOwner s' b' o h) s batch h

theorem relabelAll_slabLayout (s : State) (batch : List BlockId) (o : Owner)
    (h : WfSlabLayout s) : WfSlabLayout (relabelAll s batch o) :=
  relabelAll_preserves o WfSlabLayout (fun s' b' h => WfSlabLayout.setOwner s' b' o h) s batch h

theorem relabelAll_spansDisjoint (s : State) (batch : List BlockId) (o : Owner)
    (h : WfSpansDisjoint s) : WfSpansDisjoint (relabelAll s batch o) :=
  relabelAll_preserves o WfSpansDisjoint (fun s' b' h => WfSpansDisjoint.setOwner s' b' o h) s batch h

theorem relabelAll_blockSpanClass (s : State) (batch : List BlockId) (o : Owner)
    (h : WfBlockSpanClass s) : WfBlockSpanClass (relabelAll s batch o) :=
  relabelAll_preserves o WfBlockSpanClass (fun s' b' h => WfBlockSpanClass.setOwner s' b' o h) s batch h

/-- The batch version: relabelling a whole batch to a non-live owner preserves
clause 10 (released ranges contain no live block). -/
theorem relabelAll_releasedNoLive (s : State) (batch : List BlockId) (o : Owner)
    (hne : o ≠ Owner.live) (h : WfReleasedNoLive s) : WfReleasedNoLive (relabelAll s batch o) :=
  relabelAll_preserves o WfReleasedNoLive
    (fun s' b' h => WfReleasedNoLive.setOwner_of_ne_live s' b' o hne h) s batch h

/-- Clause 3 under a batch relabel: preserved when every batch block already has
the size class the (cache) target owner is keyed by. -/
theorem relabelAll_cachesConsistent (o : Owner) :
    ∀ (s : State) (batch : List BlockId), WfCachesConsistent s →
      (∀ scT, o.cacheSizeClass? = some scT → ∀ blk ∈ s.blocks, blk.id ∈ batch → blk.sc = scT) →
      WfCachesConsistent (relabelAll s batch o) := by
  intro s batch
  induction batch generalizing s with
  | nil => intro h _; exact h
  | cons b rest ih =>
    intro h hbatch
    rw [relabelAll_cons]
    apply ih
    · apply WfCachesConsistent.setOwner s b o h
      intro blk hblk hid scT hsc
      exact hbatch scT hsc blk hblk (by rw [hid]; exact List.mem_cons_self ..)
    · intro scT hsc blk' hblk' hmem
      obtain ⟨blk0, hblk0, hid, _, hsc0, _⟩ := s.setOwner_mem b o hblk'
      rw [hsc0]
      exact hbatch scT hsc blk0 hblk0 (by rw [← hid]; exact List.mem_cons_of_mem _ hmem)

/-- Clause 4 under a batch relabel: preserved when every batch block's span carries
the arena the (central) target owner is qualified by. -/
theorem relabelAll_centralConsistent (o : Owner) :
    ∀ (s : State) (batch : List BlockId), WfCentralConsistent s →
      (∀ a, o.arena? = some a → ∀ blk ∈ s.blocks, blk.id ∈ batch →
        ∃ d ∈ s.spans, d.id = blk.span ∧ d.arena = a) →
      WfCentralConsistent (relabelAll s batch o) := by
  intro s batch
  induction batch generalizing s with
  | nil => intro h _; exact h
  | cons b rest ih =>
    intro h hbatch
    rw [relabelAll_cons]
    apply ih
    · apply WfCentralConsistent.setOwner s b o h
      intro blk hblk hid a ha
      exact hbatch a ha blk hblk (by rw [hid]; exact List.mem_cons_self ..)
    · intro a ha blk' hblk' hmem
      obtain ⟨blk0, hblk0, hid, _, _, hspan0⟩ := s.setOwner_mem b o hblk'
      obtain ⟨d, hd, hdid, hda⟩ := hbatch a ha blk0 hblk0 (by rw [← hid]; exact List.mem_cons_of_mem _ hmem)
      exact ⟨d, by rw [setOwner_spans]; exact hd, by rw [hspan0]; exact hdid, hda⟩

/- ----------------------------------------------------------------------- -/
/- The transitions.                                                         -/
/- ----------------------------------------------------------------------- -/

/-- `malloc`: pop a free (cached) slot and hand it out.
SPEC-transition: object `FreeInLocalCache → Live` (§7.2). -/
def malloc (s : State) (b : BlockId) : State := s.setOwner b Owner.live

/-- `free`: return a live slot to a free owner `o` (its arena's central list).
SPEC-transition: object `Live → FreeInLocalCache`/`CentralFree` (§7.2). -/
def free (s : State) (b : BlockId) (o : Owner) : State := s.setOwner b o

/-- `cache_refill`: pull a batch from the central/transfer list into the per-CPU
cache. SPEC-transition: object `CentralFree → FreeInLocalCache` (§11.5/§14.3). -/
def cacheRefill (s : State) (batch : List BlockId) (cpu : CpuId) (sc : SizeClassId) : State :=
  relabelAll s batch (Owner.cpuCache cpu sc)

/-- `cache_flush`: push a batch from the per-CPU cache back to the central list.
SPEC-transition: object `FreeInLocalCache → CentralFree` (§11.5/§14.4). -/
def cacheFlush (s : State) (batch : List BlockId) (arena : ArenaId) (sc : SizeClassId) : State :=
  relabelAll s batch (Owner.centralFree arena sc)

/-- `central_batch_remove`: move a batch from the central list to a transfer cache.
SPEC-transition: object `CentralFree → FreeInTransferCache` (§14.3). -/
def centralBatchRemove (s : State) (batch : List BlockId) (domain : Nat) (sc : SizeClassId) : State :=
  relabelAll s batch (Owner.transferCache domain sc)

/-- `central_batch_insert`: move a batch from a transfer cache to the central list.
SPEC-transition: object `FreeInTransferCache → CentralFree` (§14.4). -/
def centralBatchInsert (s : State) (batch : List BlockId) (arena : ArenaId) (sc : SizeClassId) : State :=
  relabelAll s batch (Owner.centralFree arena sc)

/-- `release_to_os`: record a range as released to the OS/provider.
SPEC-transition: span/range `* → Released` (§20.4/§21). -/
def releaseToOs (s : State) (r : Range) : State := { s with released := r :: s.released }

/-- The arena a block belongs to (read through its span). -/
def spanArena (s : State) (sp : SpanId) : Option ArenaId := (s.spanById sp).map (·.arena)

/-- `arena_reset`: discard every allocation of arena `a`, returning its slots to the
backend free list; other arenas are untouched. SPEC-transition: arena reset (§22.5). -/
def arenaReset (s : State) (a : ArenaId) : State :=
  { s with blocks := s.blocks.map (fun blk =>
      if spanArena s blk.span = some a then { blk with owner := Owner.backendFree a } else blk) }

@[simp] theorem arenaReset_blocks (s : State) (a : ArenaId) :
    (arenaReset s a).blocks = s.blocks.map (fun blk =>
      if spanArena s blk.span = some a then { blk with owner := Owner.backendFree a } else blk) := rfl

/-- `arena_destroy`: reset plus removal of arena `a`'s spans and slots (§22.6), and the
pagemap entries naming those spans — so no entry dangles to a removed span (clause 7). -/
def arenaDestroy (s : State) (a : ArenaId) : State :=
  { s with
    blocks := s.blocks.filter (fun blk => ! decide (spanArena s blk.span = some a))
    spans := s.spans.filter (fun d => ! decide (d.arena = a))
    pagemap := s.pagemap.filter (fun e => ! decide (spanArena s e.2 = some a)) }

@[simp] theorem arenaDestroy_blocks (s : State) (a : ArenaId) :
    (arenaDestroy s a).blocks = s.blocks.filter (fun blk => ! decide (spanArena s blk.span = some a)) :=
  rfl

@[simp] theorem arenaDestroy_spans (s : State) (a : ArenaId) :
    (arenaDestroy s a).spans = s.spans.filter (fun d => ! decide (d.arena = a)) := rfl

@[simp] theorem arenaDestroy_pagemap (s : State) (a : ArenaId) :
    (arenaDestroy s a).pagemap =
      s.pagemap.filter (fun e => ! decide (spanArena s e.2 = some a)) := rfl

end TopoMalloc

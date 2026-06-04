-- SPDX-License-Identifier: MIT
/-
A concrete, non-empty `WellFormed` witness exercised end-to-end (plan 02 W1, gap
closure for the non-vacuity evidence).

`wellFormed_empty` shows the predicate holds *vacuously*; this file shows all fourteen
clauses are *jointly* satisfiable by a real state with actual blocks — a live object and
a cached free object in one span on a page — and that `malloc` then `free` round-trip on
it. This rules out a hidden over-constraint among the clauses and exercises the
transition theorems on a concrete instance.
-/
import TopoMalloc.Theorems.Malloc
import TopoMalloc.Theorems.Free

namespace TopoMalloc

open State

/-- A cached free slot (class 0, the first 16 bytes of the span). -/
def demoBlock0 : Block := ⟨0, ⟨0, 16⟩, Owner.cpuCache 0 0, 0, 0⟩
/-- A live slot (class 0, the next 16 bytes). -/
def demoBlock1 : Block := ⟨1, ⟨16, 16⟩, Owner.live, 0, 0⟩
/-- The span both slots live in: one page, arena 0, class 0, two objects. -/
def demoSpan : SpanDesc := ⟨0, ⟨0, 16384⟩, 0, 0, none, 2⟩
/-- A concrete well-formed state: two class-0 slots in one span, page 0 mapped. -/
def demoState : State := ⟨[demoBlock0, demoBlock1], [demoSpan], [], [(0, 0)], []⟩

/-- Decompose membership in the two-block list. -/
private theorem mem_demoBlocks {blk : Block} (h : blk ∈ [demoBlock0, demoBlock1]) :
    blk = demoBlock0 ∨ blk = demoBlock1 := by
  rcases List.mem_cons.mp h with rfl | h
  · exact Or.inl rfl
  · exact Or.inr (List.mem_singleton.mp h)

/-- **All fourteen `WellFormed` clauses hold jointly on a real, non-empty state.** -/
theorem wellFormed_demoState : WellFormed demoState where
  rangesDisjoint := by unfold WfRangesDisjoint; decide
  uniqueOwner := by unfold WfUniqueOwner; decide
  cachesConsistent := by
    intro blk hblk sc hsc
    rcases mem_demoBlocks hblk with rfl | rfl
    · simp only [demoBlock0, Owner.cacheSizeClass?] at hsc; exact Option.some.inj hsc
    · simp [demoBlock1, Owner.cacheSizeClass?] at hsc
  centralConsistent := by
    intro blk hblk a ha
    rcases mem_demoBlocks hblk with rfl | rfl <;> simp [demoBlock0, demoBlock1, Owner.arena?] at ha
  objectsFitSpans := by unfold WfObjectsFitSpans; decide
  bitmapsAgree := by
    intro d hd
    have hdd : d = demoSpan := List.mem_singleton.mp hd
    subst hdd
    refine ⟨by decide, ?_⟩
    intro r hr
    rw [show sizeClassRow demoSpan.sc = some ⟨16, 16, 1, 1024, 32, 256⟩ from rfl] at hr
    injection hr with hr; subst hr; decide
  pagemapAgrees := by
    intro p sp hmem
    have hpp : (p, sp) = ((0 : Nat), (0 : SpanId)) := List.mem_singleton.mp hmem
    rw [Prod.mk.injEq] at hpp
    obtain ⟨rfl, rfl⟩ := hpp
    exact ⟨demoSpan, by decide, by decide, by decide, by decide⟩
  hugepagesAgree := by
    intro d hd h hh
    have hdd : d = demoSpan := List.mem_singleton.mp hd
    subst hdd; simp [demoSpan] at hh
  arenaUnique := by unfold WfArenaUnique; decide
  releasedNoLive := by unfold WfReleasedNoLive; decide
  cacheCapacities := by
    intro cpu sc
    rcases Decidable.em (cpu = 0 ∧ sc = 0) with ⟨rfl, rfl⟩ | h
    · decide
    · have hzero : demoState.countOwned (Owner.cpuCache cpu sc) = 0 := by
        simp only [demoState, State.countOwned, demoBlock0, demoBlock1, List.countP_cons,
          List.countP_nil]
        have h0 : ¬ (Owner.cpuCache 0 0 = Owner.cpuCache cpu sc) := by
          intro hc; injection hc with hc1 hc2; exact h ⟨hc1.symm, hc2.symm⟩
        simp [h0]
      rw [hzero]; exact Nat.zero_le _
  slabLayout := by
    intro blk hblk
    rcases mem_demoBlocks hblk with rfl | rfl
    · exact ⟨_, rfl, by decide, by decide⟩
    · exact ⟨_, rfl, by decide, by decide⟩
  spansDisjoint := by unfold WfSpansDisjoint; decide
  blockSpanClass := by unfold WfBlockSpanClass; decide

/-- The cached slot `0` is not live in the demo state. -/
theorem demo_block0_not_live : ¬ demoState.IsLive 0 := by unfold State.IsLive; decide

/-- **The fast-path allocation round-trips on a real state.** Popping the cached slot to
`live` preserves well-formedness and makes the slot live; freeing it back to the central
list removes liveness and re-owns it — only that slot changes (the frame). The model does
real work, not nothing. -/
theorem demo_malloc_free_roundtrip :
    WellFormed (malloc demoState 0) ∧
      (malloc demoState 0).ownerOf 0 = some Owner.live ∧
      ¬ (free (malloc demoState 0) 0 (Owner.centralFree 0 0)).IsLive 0 ∧
      (free (malloc demoState 0) 0 (Owner.centralFree 0 0)).ownerOf 0 =
        some (Owner.centralFree 0 0) := by
  have hwf : WellFormed demoState := wellFormed_demoState
  have hb : (demoState.blockById 0).isSome := by decide
  have hwf1 : WellFormed (malloc demoState 0) :=
    malloc_preserves_wellformed demoState 0 hwf demo_block0_not_live
      (by intro blk _ _ r hr; simp [demoState] at hr)
  have hlive : (malloc demoState 0).ownerOf 0 = some Owner.live :=
    setOwner_ownerOf_self demoState 0 Owner.live hb
  have hb1 : ((malloc demoState 0).blockById 0).isSome := by decide
  obtain ⟨ho, hnl, _⟩ :=
    free_removes_liveness_and_adds_exactly_one_free_owner (malloc demoState 0) 0
      (Owner.centralFree 0 0) hb1 (by simp)
  exact ⟨hwf1, hlive, hnl, ho⟩

end TopoMalloc

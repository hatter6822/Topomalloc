-- SPDX-License-Identifier: GPL-3.0-or-later
/-
Information-flow / non-interference (SPEC §36.12, plan 02 W1-12d/W1-13).

* `stats_observation_noninterference`
* `topo_step_preserves_low_equivalence`

A label-scoped statistic `liveLabeled st L` counts the live objects labelled `L`. A
block's label is read through its span and the arena authorities — never through its
owner — so relabelling a block of a *different* label leaves every other label's
statistic untouched. Hence an authorized observation at a low label cannot see
allocation activity in a high label (non-interference), and low-equivalent states stay
low-equivalent across such a step (§36.12).

GPL-3.0-or-later (D5).
-/
import TopoMalloc.SeLe4n.Bridge

namespace TopoMalloc.SeLe4n

open TopoMalloc State

/-- A label-scoped statistic: the number of live objects labelled `L` (§31.1/§36.12). -/
def liveLabeled (st : TopoSeLe4n) (L : Label) : Nat :=
  st.topo.blocks.countP (fun blk => decide (blk.owner = Owner.live ∧ st.blockLabel blk = some L))

/-- A block's label is unchanged by an owner relabel: it is read through the span and
the (unchanged) arena authorities, never through the owner. -/
theorem blockLabel_withTopo_setOwner (st : TopoSeLe4n) (b : BlockId) (o : Owner) (blk0 : Block) :
    (st.withTopo (st.topo.setOwner b o)).blockLabel (relabel b o blk0) = st.blockLabel blk0 := by
  unfold TopoSeLe4n.blockLabel TopoSeLe4n.blockArena
  simp only [withTopo_sys, withTopo_topo, relabel_span, State.spanById, setOwner_spans]

/-- **Core non-interference lemma.** Relabelling a block whose label is *not* `L` leaves
the live-`L` statistic unchanged. -/
theorem liveLabeled_setOwner_eq (st : TopoSeLe4n) (b : BlockId) (o : Owner) (L : Label)
    (hb : ∀ blk ∈ st.topo.blocks, blk.id = b → st.blockLabel blk ≠ some L) :
    liveLabeled (st.withTopo (st.topo.setOwner b o)) L = liveLabeled st L := by
  unfold liveLabeled
  simp only [withTopo_topo, setOwner_blocks, List.countP_map]
  apply List.countP_congr
  intro blk0 hblk0
  simp only [Function.comp, blockLabel_withTopo_setOwner, decide_eq_true_eq]
  by_cases hid : blk0.id = b
  · have hL := hb blk0 hblk0 hid
    simp only [relabel, if_pos hid]
    constructor <;> (rintro ⟨_, h2⟩; exact absurd h2 hL)
  · simp only [relabel, if_neg hid]

/-- **`stats_observation_noninterference` (§36.17 / §36.12).** An authorized observation
of a label `L`'s live statistic is unchanged by any allocation/free activity in a
*different* label: high-domain activity does not perturb a low-domain stat. -/
theorem stats_observation_noninterference (st : TopoSeLe4n) (b : BlockId) (o : Owner) (L : Label)
    (hb : ∀ blk ∈ st.topo.blocks, blk.id = b → st.blockLabel blk ≠ some L) :
    liveLabeled (st.withTopo (st.topo.setOwner b o)) L = liveLabeled st L :=
  liveLabeled_setOwner_eq st b o L hb

/-- Two states are low-equivalent (at a `low` label predicate) when they agree on every
low label's live statistic (§36.12). -/
def LowEquivalent (low : Label → Bool) (st1 st2 : TopoSeLe4n) : Prop :=
  ∀ L, low L = true → liveLabeled st1 L = liveLabeled st2 L

/-- **`topo_step_preserves_low_equivalence` (§36.12, W1-13).** A relabel whose block
carries a *high* (non-low) label preserves low-equivalence: applied to two
low-equivalent states it yields low-equivalent successors. The cache/stats steps the
SPEC schematic targets are instances of this relabel. -/
theorem topo_step_preserves_low_equivalence (low : Label → Bool) (st1 st2 : TopoSeLe4n)
    (b1 b2 : BlockId) (o1 o2 : Owner)
    (hlow : LowEquivalent low st1 st2)
    (hb1 : ∀ blk ∈ st1.topo.blocks, blk.id = b1 → ∀ L, low L = true → st1.blockLabel blk ≠ some L)
    (hb2 : ∀ blk ∈ st2.topo.blocks, blk.id = b2 → ∀ L, low L = true → st2.blockLabel blk ≠ some L) :
    LowEquivalent low (st1.withTopo (st1.topo.setOwner b1 o1))
      (st2.withTopo (st2.topo.setOwner b2 o2)) := by
  intro L hL
  rw [liveLabeled_setOwner_eq st1 b1 o1 L (fun blk hblk hid => hb1 blk hblk hid L hL),
      liveLabeled_setOwner_eq st2 b2 o2 L (fun blk hblk hid => hb2 blk hblk hid L hL)]
  exact hlow L hL

end TopoMalloc.SeLe4n

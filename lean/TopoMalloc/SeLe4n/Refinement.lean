-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The bridge preservation/simulation theorems (SPEC §36.17, plan 02 W1-12b/c/d).

* `backing_descends_from_untyped`, `no_live_object_released`            (W1-12b)
* `destroy_revokes_descendants`, `label_partition_preserved`,
  `scrub_before_downgrade`                                              (W1-12c)
* `topo_step_preserves_sele4n_invariants`                              (W1-12d)

Each is proved single-core. The release/destroy families lift the corresponding §33.4
theorems (`release_to_os_preserves_live_objects`, the arena lifecycle theorems) to the
combined state; the provenance family reads the §36.6 reachability built into `R`; and
`topo_step_preserves_sele4n_invariants` holds because the TopoMalloc fast/slow paths
leave the capability system untouched (`withTopo`).

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Transitions
import TopoMalloc.Theorems.Release
import TopoMalloc.SeLe4n.Bridge
import TopoMalloc.SeLe4n.InformationFlow

namespace TopoMalloc.SeLe4n

open TopoMalloc State

/-- **`topo_step_preserves_sele4n_invariants` (§36.17).** Every TopoMalloc-visible
transition (a `withTopo` step) preserves the seLe4n invariant bundle: the fast/slow
paths never touch capability state. -/
theorem topo_step_preserves_sele4n_invariants (st : TopoSeLe4n) (s' : State)
    (h : SysInvariants st.sys) : SysInvariants (st.withTopo s').sys := by
  rw [withTopo_sys]; exact h

/-- **`backing_descends_from_untyped` (§36.17).** Every backing frame descends from
authorized untyped memory — the §36.6 provenance carried by the abstraction relation. -/
theorem backing_descends_from_untyped (st : TopoSeLe4n) (hR : R st) :
    ∀ bk ∈ st.sys.backings, BackingState.Reaches BackingState.authorizedUntyped bk.state :=
  hR.2

/-- Releasing a range in the combined state. -/
def TopoSeLe4n.release (st : TopoSeLe4n) (r : Range) : TopoSeLe4n :=
  st.withTopo (releaseToOs st.topo r)

/-- **`no_live_object_released` (§36.17 / §21.6).** Release never touches a live object:
every previously-live slot stays live. -/
theorem no_live_object_released (st : TopoSeLe4n) (r : Range) :
    ∀ b, st.topo.IsLive b → (st.release r).topo.IsLive b := by
  intro b h
  unfold TopoSeLe4n.release
  rw [withTopo_topo]
  exact release_to_os_preserves_live_objects st.topo r b h

/-- Destroying an arena in the combined state. -/
def TopoSeLe4n.destroyArena (st : TopoSeLe4n) (a : ArenaId) : TopoSeLe4n :=
  st.withTopo (arenaDestroy st.topo a)

/-- **`destroy_revokes_descendants` (§36.17 / §22.6).** A destroyed arena retains no
slot: every block remaining belongs to a *different* arena, so no derived (live) object
of the destroyed arena survives. -/
theorem destroy_revokes_descendants (st : TopoSeLe4n) (a : ArenaId) :
    ∀ blk ∈ (st.destroyArena a).topo.blocks, ¬ (spanArena st.topo blk.span = some a) := by
  intro blk hblk
  unfold TopoSeLe4n.destroyArena at hblk
  rw [withTopo_topo, arenaDestroy_blocks, List.mem_filter] at hblk
  simpa using hblk.2

/-- **`label_partition_preserved` (§36.17 / §36.12).** A `malloc` (handing a slot to
`live`) preserves the label partition: it only *removes* a slot from a free structure,
never mixing labels, and labels are owner-invariant. -/
theorem label_partition_preserved (st : TopoSeLe4n) (b : BlockId) (h : LabelPartition st) :
    LabelPartition (st.withTopo (st.topo.setOwner b Owner.live)) := by
  intro blk1' h1 blk2' h2 howner hne
  rw [withTopo_topo, setOwner_blocks, List.mem_map] at h1 h2
  obtain ⟨blk01, hblk01, rfl⟩ := h1
  obtain ⟨blk02, hblk02, rfl⟩ := h2
  have e1 : relabel b Owner.live blk01 = blk01 := by
    rcases Decidable.em (blk01.id = b) with hc | hc
    · exact absurd (by simp [relabel, hc] : (relabel b Owner.live blk01).owner = Owner.live) hne
    · simp [relabel, hc]
  have e2 : relabel b Owner.live blk02 = blk02 := by
    rcases Decidable.em (blk02.id = b) with hc | hc
    · exfalso; apply hne; rw [howner]; simp [relabel, hc]
    · simp [relabel, hc]
  rw [e1, e2] at howner
  rw [e1] at hne
  simp only [blockLabel_withTopo_setOwner]
  exact h blk01 hblk01 blk02 hblk02 howner hne

/-- Downgrade a backing frame to a lower label, *only* if it has been scrubbed
(`AllocatorMuzzyOrScrubbed`, §36.7). -/
def Backing.downgrade (bk : Backing) (newLabel : Label) : Option Backing :=
  if bk.state = BackingState.allocatorMuzzyOrScrubbed then some { bk with label := newLabel }
  else none

/-- **`scrub_before_downgrade` (§36.17 / §36.12).** Memory reused at a lower label has
passed the required scrub/revocation protocol: a successful downgrade implies the frame
was scrubbed. -/
theorem scrub_before_downgrade (bk : Backing) (newLabel : Label) (bk' : Backing)
    (h : bk.downgrade newLabel = some bk') :
    bk.state = BackingState.allocatorMuzzyOrScrubbed := by
  unfold Backing.downgrade at h
  by_cases hs : bk.state = BackingState.allocatorMuzzyOrScrubbed
  · exact hs
  · rw [if_neg hs] at h; exact absurd h (by simp)

end TopoMalloc.SeLe4n

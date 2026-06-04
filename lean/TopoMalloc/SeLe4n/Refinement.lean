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
import TopoMalloc.Theorems.Malloc
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

/-- **`backing_descends_from_untyped` (§36.17).** Every backing frame descends from an
*authorized* untyped capability (`origin ∈ authorizedUntypeds`) and sits at a §36.6
lifecycle state reachable from `AuthorizedUntyped`. The provenance clause is a genuine
constraint carried by the abstraction relation `R`, not a tautology. -/
theorem backing_descends_from_untyped (st : TopoSeLe4n) (hR : R st) :
    ∀ bk ∈ st.sys.backings, bk.origin ∈ st.sys.authorizedUntypeds ∧
      BackingState.Reaches BackingState.authorizedUntyped bk.state :=
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

/- ----------------------------------------------------------------------- -/
/- Coupled transitions: TopoMalloc and the capability system move together. -/
/- ----------------------------------------------------------------------- -/

/-- Add `size` bytes to one arena's used count. -/
def ArenaAuth.charge (au : ArenaAuth) (size : Nat) : ArenaAuth := { au with used := au.used + size }
/-- Remove `size` bytes from one arena's used count. -/
def ArenaAuth.credit (au : ArenaAuth) (size : Nat) : ArenaAuth := { au with used := au.used - size }

@[simp] theorem ArenaAuth.charge_arena (au : ArenaAuth) (size : Nat) :
    (au.charge size).arena = au.arena := rfl
@[simp] theorem ArenaAuth.charge_label (au : ArenaAuth) (size : Nat) :
    (au.charge size).label = au.label := rfl

/-- Charge `size` bytes against arena `a`'s quota. -/
def SystemState.chargeArena (sys : SystemState) (a : ArenaId) (size : Nat) : SystemState :=
  { sys with arenas := sys.arenas.map (fun au => if au.arena = a then au.charge size else au) }

/-- Credit `size` bytes back to arena `a`'s quota. -/
def SystemState.creditArena (sys : SystemState) (a : ArenaId) (size : Nat) : SystemState :=
  { sys with arenas := sys.arenas.map (fun au => if au.arena = a then au.credit size else au) }

/-- A **coupled allocation step**: hand slot `b` to `live` *and* charge `size` bytes to
arena `a`. The TopoMalloc malloc and the seLe4n quota accounting happen as one step — the
simulation the bridge is about, not two independent updates. -/
def TopoSeLe4n.allocStep (st : TopoSeLe4n) (b : BlockId) (a : ArenaId) (size : Nat) : TopoSeLe4n :=
  { topo := st.topo.setOwner b Owner.live, sys := st.sys.chargeArena a size }

/-- A **coupled free step**: return slot `b` to the central list *and* credit the bytes back. -/
def TopoSeLe4n.freeStep (st : TopoSeLe4n) (b : BlockId) (a : ArenaId) (sc : SizeClassId)
    (size : Nat) : TopoSeLe4n :=
  { topo := st.topo.setOwner b (Owner.centralFree a sc), sys := st.sys.creditArena a size }

/-- **Simulation.** The allocation's TopoMalloc side is exactly `malloc`. -/
@[simp] theorem allocStep_topo (st : TopoSeLe4n) (b a size) :
    (st.allocStep b a size).topo = malloc st.topo b := rfl

/-- The per-arena update predicate is unaffected by charging (it preserves `arena`). -/
private theorem chargeArena_pred (a size : Nat) (x : ArenaId) :
    ((fun au => decide (au.arena = x)) ∘
      (fun au : ArenaAuth => if au.arena = a then au.charge size else au))
      = (fun au => decide (au.arena = x)) := by
  funext au; simp only [Function.comp]; by_cases hh : au.arena = a <;> simp [hh]

/-- Charging preserves each arena's identity, hence the authority-uniqueness invariant. -/
theorem chargeArena_map_arena (sys : SystemState) (a size : Nat) :
    (sys.chargeArena a size).arenas.map (·.arena) = sys.arenas.map (·.arena) := by
  simp only [SystemState.chargeArena, List.map_map]
  apply List.map_congr_left; intro x _; by_cases h : x.arena = a <;> simp [h]

/-- Charging preserves "arena `x` is a known authority" (it changes `used`, not presence). -/
theorem chargeArena_arenaAuthOf_isSome (sys : SystemState) (a size : Nat) {x : ArenaId}
    (h : (sys.arenaAuthOf x).isSome) : ((sys.chargeArena a size).arenaAuthOf x).isSome := by
  simp only [SystemState.arenaAuthOf, SystemState.chargeArena, List.find?_map, chargeArena_pred]
  simp only [SystemState.arenaAuthOf] at h
  cases hf : sys.arenas.find? (fun au => decide (au.arena = x)) with
  | none => rw [hf] at h; simp at h
  | some au => simp

/-- Charging does not change any arena's label, so block labels are unchanged. -/
theorem chargeArena_label (sys : SystemState) (a size : Nat) (x : ArenaId) :
    ((sys.chargeArena a size).arenaAuthOf x).map (·.label) =
      (sys.arenaAuthOf x).map (·.label) := by
  simp only [SystemState.arenaAuthOf, SystemState.chargeArena, List.find?_map, Option.map_map,
    chargeArena_pred]
  cases hf : sys.arenas.find? (fun au => decide (au.arena = x)) with
  | none => simp
  | some au => by_cases hh : au.arena = a <;> simp [Function.comp, hh]

/-- **Quota preservation under a coupled allocation (§36.17).** If the charged arena had
budget (`used + size ≤ quota`), the allocation keeps every arena's quota sound. -/
theorem chargeArena_preserves_quota (sys : SystemState) (a size : Nat)
    (h : ∀ au ∈ sys.arenas, au.quotaOk)
    (hbudget : ∀ au ∈ sys.arenas, au.arena = a → au.used + size ≤ au.quota) :
    ∀ au ∈ (sys.chargeArena a size).arenas, au.quotaOk := by
  intro au hau
  simp only [SystemState.chargeArena, List.mem_map] at hau
  obtain ⟨au0, hau0, rfl⟩ := hau
  by_cases hc : au0.arena = a
  · rw [if_pos hc]; show au0.used + size ≤ au0.quota; exact hbudget au0 hau0 hc
  · rw [if_neg hc]; exact h au0 hau0

/-- Two combined states with the same TopoMalloc state and the same block labels share the
label partition (used to push charging — which changes no label — through). -/
theorem LabelPartition_of_same {st1 st2 : TopoSeLe4n} (htopo : st1.topo = st2.topo)
    (hlabel : ∀ blk, st1.blockLabel blk = st2.blockLabel blk) (h : LabelPartition st2) :
    LabelPartition st1 := by
  intro blk1 hb1 blk2 hb2 howner hne
  rw [hlabel, hlabel, htopo] at *
  exact h blk1 hb1 blk2 hb2 howner hne

/-- The seLe4n invariant bundle survives a coupled allocation. -/
theorem allocStep_preserves_sysInvariants (st : TopoSeLe4n) (b a size : Nat)
    (h : SysInvariants st.sys)
    (hbudget : ∀ au ∈ st.sys.arenas, au.arena = a → au.used + size ≤ au.quota) :
    SysInvariants (st.allocStep b a size).sys where
  quota := chargeArena_preserves_quota st.sys a size h.quota hbudget
  arenasNodup := by
    have := h.arenasNodup; rw [← chargeArena_map_arena st.sys a size] at this; exact this

/-- **The coupled allocation preserves `TopoSeLe4nWellFormed` (§36.17).** TopoMalloc
well-formedness comes from `malloc`, the seLe4n quota from the budget guard, the
abstraction relation from charging touching neither spans nor presence, and the label
partition because charging changes no label. The two sides move as one. -/
theorem allocStep_preserves_invariants (st : TopoSeLe4n) (b a size : Nat)
    (hwf : TopoSeLe4nWellFormed st)
    (hcommitted : ∀ blk ∈ st.topo.blocks, blk.id = b → ∀ r ∈ st.topo.released,
      Range.Disjoint r blk.range)
    (hbudget : ∀ au ∈ st.sys.arenas, au.arena = a → au.used + size ≤ au.quota) :
    TopoSeLe4nWellFormed (st.allocStep b a size) where
  topoWf := malloc_preserves_wellformed st.topo b hwf.topoWf hcommitted
  sysInv := allocStep_preserves_sysInvariants st b a size hwf.sysInv hbudget
  rel := by
    refine ⟨fun d hd => ?_, fun bk hbk => hwf.rel.2 bk hbk⟩
    exact chargeArena_arenaAuthOf_isSome st.sys a size (hwf.rel.1 d hd)
  labels := by
    -- `allocStep` and `withTopo (setOwner b live)` share a topo, and their block labels
    -- agree because charging changes no label; reduce to the malloc label-partition.
    refine LabelPartition_of_same (st1 := st.allocStep b a size)
      (st2 := st.withTopo (st.topo.setOwner b Owner.live)) rfl ?_
      (label_partition_preserved st b hwf.labels)
    intro blk
    simp only [TopoSeLe4n.blockLabel, TopoSeLe4n.blockArena, TopoSeLe4n.allocStep, withTopo_topo,
      withTopo_sys]
    cases (st.topo.setOwner b Owner.live).spanById blk.span with
    | none => rfl
    | some d => simp only [Option.map_some, Option.bind_some]; exact chargeArena_label st.sys a size d.arena

/-- Mark a backing frame revoked (§36.13). -/
def Backing.revoke (bk : Backing) : Backing := { bk with state := BackingState.revoked }

/-- Revoke every backing frame of arena `a`. -/
def SystemState.revokeArena (sys : SystemState) (a : ArenaId) : SystemState :=
  { sys with backings := sys.backings.map (fun bk => if bk.arena = a then bk.revoke else bk) }

/-- Destroying an arena in the combined state: drop its slots/spans *and* revoke its
backing frames (§36.13). -/
def TopoSeLe4n.destroyArena (st : TopoSeLe4n) (a : ArenaId) : TopoSeLe4n :=
  { topo := arenaDestroy st.topo a, sys := st.sys.revokeArena a }

/-- **`destroy_revokes_descendants` (§36.17 / §22.6/§36.13).** A destroyed arena retains no
slot (every surviving block belongs to a different arena) **and** every one of its backing
frames is revoked — derived capabilities are invalidated before reuse. -/
theorem destroy_revokes_descendants (st : TopoSeLe4n) (a : ArenaId) :
    (∀ blk ∈ (st.destroyArena a).topo.blocks, ¬ (spanArena st.topo blk.span = some a)) ∧
      (∀ bk ∈ (st.destroyArena a).sys.backings, bk.arena = a →
        bk.state = BackingState.revoked) := by
  refine ⟨?_, ?_⟩
  · intro blk hblk
    simp only [TopoSeLe4n.destroyArena, arenaDestroy_blocks, List.mem_filter] at hblk
    simpa using hblk.2
  · intro bk hbk harena
    simp only [TopoSeLe4n.destroyArena, SystemState.revokeArena, List.mem_map] at hbk
    obtain ⟨bk0, _, rfl⟩ := hbk
    by_cases hc : bk0.arena = a
    · rw [if_pos hc]; rfl
    · rw [if_neg hc] at harena; exact absurd harena hc

/-- **`label_partition_preserved` (general, §36.17 / §36.12).** Relabelling block `b` to
*any* owner `o` preserves the label partition, provided the moved block carries the same
label as every other block currently held by `o` (`hmatch`) — i.e. it does not mix a new
label into that cache/list. This is what a `free` into a per-(arena, class) central list
must satisfy. `uniqueOwner` resolves the case where both compared blocks are `b`. -/
theorem label_partition_preserved_setOwner (st : TopoSeLe4n) (b : BlockId) (o : Owner)
    (huniq : WfUniqueOwner st.topo) (h : LabelPartition st)
    (hmatch : ∀ blkb ∈ st.topo.blocks, blkb.id = b →
      ∀ c ∈ st.topo.blocks, c.id ≠ b → c.owner = o → st.blockLabel blkb = st.blockLabel c) :
    LabelPartition (st.withTopo (st.topo.setOwner b o)) := by
  intro blk1' h1 blk2' h2 howner hne
  rw [withTopo_topo, setOwner_blocks, List.mem_map] at h1 h2
  obtain ⟨blk01, hblk01, rfl⟩ := h1
  obtain ⟨blk02, hblk02, rfl⟩ := h2
  rw [blockLabel_withTopo_setOwner st b o blk01, blockLabel_withTopo_setOwner st b o blk02]
  by_cases hc1 : blk01.id = b <;> by_cases hc2 : blk02.id = b
  · rw [huniq.eq_of_id_eq hblk01 hblk02 (by rw [hc1, hc2])]
  · have ho2 : blk02.owner = o := by
      have e1 : (relabel b o blk01).owner = o := by rw [relabel, if_pos hc1]
      have e2 : (relabel b o blk02).owner = blk02.owner := by rw [relabel, if_neg hc2]
      rw [e1, e2] at howner; exact howner.symm
    exact hmatch blk01 hblk01 hc1 blk02 hblk02 hc2 ho2
  · have ho1 : blk01.owner = o := by
      have e1 : (relabel b o blk01).owner = blk01.owner := by rw [relabel, if_neg hc1]
      have e2 : (relabel b o blk02).owner = o := by rw [relabel, if_pos hc2]
      rw [e1, e2] at howner; exact howner
    exact (hmatch blk02 hblk02 hc2 blk01 hblk01 hc1 ho1).symm
  · have ho' : blk01.owner = blk02.owner := by
      have e1 : (relabel b o blk01).owner = blk01.owner := by rw [relabel, if_neg hc1]
      have e2 : (relabel b o blk02).owner = blk02.owner := by rw [relabel, if_neg hc2]
      rw [e1, e2] at howner; exact howner
    have hne' : blk01.owner ≠ Owner.live := by
      have e1 : (relabel b o blk01).owner = blk01.owner := by rw [relabel, if_neg hc1]
      rw [e1] at hne; exact hne
    exact h blk01 hblk01 blk02 hblk02 ho' hne'

/-- **`label_partition_preserved` under `free` (§36.17 / §36.12).** Freeing block `b` into
the central list of `(a, sc)` preserves the label partition when `b` carries the same
label as every block already in that list — the per-(arena, class) single-label
discipline. -/
theorem label_partition_preserved_free (st : TopoSeLe4n) (b : BlockId) (a : ArenaId)
    (sc : SizeClassId) (huniq : WfUniqueOwner st.topo) (h : LabelPartition st)
    (hmatch : ∀ blkb ∈ st.topo.blocks, blkb.id = b →
      ∀ c ∈ st.topo.blocks, c.id ≠ b → c.owner = Owner.centralFree a sc →
        st.blockLabel blkb = st.blockLabel c) :
    LabelPartition (st.withTopo (st.topo.setOwner b (Owner.centralFree a sc))) :=
  label_partition_preserved_setOwner st b (Owner.centralFree a sc) huniq h hmatch

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

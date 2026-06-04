-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The bridge preservation/simulation theorems (SPEC §36.17, plan 02 W1-12b/c/d).

* `backing_descends_from_untyped`, `no_live_object_released`            (W1-12b)
* `destroy_revokes_descendants`, `label_partition_preserved`
  (and its general/`free` forms), `scrub_before_downgrade`             (W1-12c)
* `topo_step_preserves_sele4n_invariants`                              (W1-12d)
* the **coupled** `allocStep`/`freeStep` and their
  `*_preserves_invariants` simulations: TopoMalloc and the seLe4n quota move as one
  step, preserving the whole `TopoSeLe4nWellFormed` bundle (W1-12b)

Each is proved single-core. The release/destroy families lift the corresponding §33.4
theorems (`release_to_os_preserves_live_objects`, the arena lifecycle theorems) to the
combined state; the provenance family reads the §36.6 reachability built into `R`; and
`topo_step_preserves_sele4n_invariants` holds because the TopoMalloc fast/slow paths
leave the capability system untouched (`withTopo`). The coupled steps additionally
charge/credit the arena quota, so they preserve the §36.4 quota accounting too.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Transitions
import TopoMalloc.Theorems.Malloc
import TopoMalloc.Theorems.Free
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

/-- The per-arena update predicate is unaffected by any update that preserves `arena`. -/
private theorem find_if_pred (a x : ArenaId) (f : ArenaAuth → ArenaAuth)
    (hf : ∀ au, (f au).arena = au.arena) :
    ((fun au => decide (au.arena = x)) ∘
      (fun au : ArenaAuth => if au.arena = a then f au else au))
      = (fun au => decide (au.arena = x)) := by
  funext au; simp only [Function.comp]; by_cases hh : au.arena = a <;> simp [hh, hf]

/-- A per-arena authority update preserving `arena` leaves the list of arena ids unchanged
(hence the authority-uniqueness invariant). Shared by `chargeArena`/`creditArena`. -/
private theorem map_if_arena (arenas : List ArenaAuth) (a : ArenaId) (f : ArenaAuth → ArenaAuth)
    (hf : ∀ au, (f au).arena = au.arena) :
    ((arenas.map (fun au => if au.arena = a then f au else au)).map (·.arena)) = arenas.map (·.arena) := by
  simp only [List.map_map]; apply List.map_congr_left; intro x _
  by_cases h : x.arena = a <;> simp [h, hf]

/-- A per-arena update preserving `arena` preserves "arena `x` is a known authority"
(it changes `used`, not presence). -/
private theorem find_if_isSome (arenas : List ArenaAuth) (a : ArenaId) (f : ArenaAuth → ArenaAuth)
    (hf : ∀ au, (f au).arena = au.arena) {x : ArenaId}
    (h : (arenas.find? (fun au => decide (au.arena = x))).isSome) :
    ((arenas.map (fun au => if au.arena = a then f au else au)).find?
      (fun au => decide (au.arena = x))).isSome := by
  rw [List.find?_map, find_if_pred a x f hf]
  cases hfd : arenas.find? (fun au => decide (au.arena = x)) with
  | none => rw [hfd] at h; simp at h
  | some au => simp

/-- A per-arena update preserving `arena` and `label` changes no arena's label. -/
private theorem find_if_label (arenas : List ArenaAuth) (a : ArenaId) (f : ArenaAuth → ArenaAuth)
    (hf_arena : ∀ au, (f au).arena = au.arena) (hf_label : ∀ au, (f au).label = au.label)
    (x : ArenaId) :
    ((arenas.map (fun au => if au.arena = a then f au else au)).find?
      (fun au => decide (au.arena = x))).map (·.label)
      = (arenas.find? (fun au => decide (au.arena = x))).map (·.label) := by
  rw [List.find?_map, Option.map_map, find_if_pred a x f hf_arena]
  cases hfd : arenas.find? (fun au => decide (au.arena = x)) with
  | none => simp
  | some au => by_cases hh : au.arena = a <;> simp [Function.comp, hh, hf_label]

/-- Charging preserves each arena's identity, hence the authority-uniqueness invariant. -/
theorem chargeArena_map_arena (sys : SystemState) (a size : Nat) :
    (sys.chargeArena a size).arenas.map (·.arena) = sys.arenas.map (·.arena) :=
  map_if_arena sys.arenas a (·.charge size) (fun _ => rfl)

/-- Charging preserves "arena `x` is a known authority" (it changes `used`, not presence). -/
theorem chargeArena_arenaAuthOf_isSome (sys : SystemState) (a size : Nat) {x : ArenaId}
    (h : (sys.arenaAuthOf x).isSome) : ((sys.chargeArena a size).arenaAuthOf x).isSome :=
  find_if_isSome sys.arenas a (·.charge size) (fun _ => rfl) h

/-- Charging does not change any arena's label, so block labels are unchanged. -/
theorem chargeArena_label (sys : SystemState) (a size : Nat) (x : ArenaId) :
    ((sys.chargeArena a size).arenaAuthOf x).map (·.label) =
      (sys.arenaAuthOf x).map (·.label) :=
  find_if_label sys.arenas a (·.charge size) (fun _ => rfl) (fun _ => rfl) x

/-- Crediting (the free-side mirror) likewise preserves arena identity, presence, and label. -/
theorem creditArena_map_arena (sys : SystemState) (a size : Nat) :
    (sys.creditArena a size).arenas.map (·.arena) = sys.arenas.map (·.arena) :=
  map_if_arena sys.arenas a (·.credit size) (fun _ => rfl)

theorem creditArena_arenaAuthOf_isSome (sys : SystemState) (a size : Nat) {x : ArenaId}
    (h : (sys.arenaAuthOf x).isSome) : ((sys.creditArena a size).arenaAuthOf x).isSome :=
  find_if_isSome sys.arenas a (·.credit size) (fun _ => rfl) h

theorem creditArena_label (sys : SystemState) (a size : Nat) (x : ArenaId) :
    ((sys.creditArena a size).arenaAuthOf x).map (·.label) =
      (sys.arenaAuthOf x).map (·.label) :=
  find_if_label sys.arenas a (·.credit size) (fun _ => rfl) (fun _ => rfl) x

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

/-- **Quota preservation under a coupled free (§36.17).** Crediting bytes back keeps every
arena's quota sound *unconditionally* — `used` only decreases (`used - size ≤ used ≤
quota`), so no budget guard is needed. -/
theorem creditArena_preserves_quota (sys : SystemState) (a size : Nat)
    (h : ∀ au ∈ sys.arenas, au.quotaOk) :
    ∀ au ∈ (sys.creditArena a size).arenas, au.quotaOk := by
  intro au hau
  simp only [SystemState.creditArena, List.mem_map] at hau
  obtain ⟨au0, hau0, rfl⟩ := hau
  by_cases hc : au0.arena = a
  · rw [if_pos hc]; show au0.used - size ≤ au0.quota
    have := h au0 hau0; unfold ArenaAuth.quotaOk at this; omega
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

/-- **The coupled free preserves `TopoSeLe4nWellFormed` (§36.17).** Symmetric to
`allocStep_preserves_invariants`: TopoMalloc well-formedness comes from `free` into the
central list of `(a, sc)` (which requires the freed slot belong to arena `a` — `hcentral`),
the seLe4n quota stays sound because crediting only lowers `used` (no budget guard), the
abstraction relation survives crediting, and the label partition holds because the freed
slot's label matches the central list (`hmatch`) and crediting changes no label. -/
theorem freeStep_preserves_invariants (st : TopoSeLe4n) (b : BlockId) (a : ArenaId)
    (sc : SizeClassId) (size : Nat) (hwf : TopoSeLe4nWellFormed st)
    (hcentral : ∀ blk ∈ st.topo.blocks, blk.id = b →
      ∃ d ∈ st.topo.spans, d.id = blk.span ∧ d.arena = a)
    (hmatch : ∀ blkb ∈ st.topo.blocks, blkb.id = b → ∀ c ∈ st.topo.blocks, c.id ≠ b →
      c.owner = Owner.centralFree a sc → st.blockLabel blkb = st.blockLabel c) :
    TopoSeLe4nWellFormed (st.freeStep b a sc size) where
  topoWf := free_preserves_wellformed_for_valid_pointer st.topo b (Owner.centralFree a sc)
    hwf.topoWf (by simp) (by intro cpu sc'; simp)
    (by intro blk _ _ sc' hsc'; simp [Owner.cacheSizeClass?] at hsc')
    (by intro blk hblk hid a' ha'
        simp only [Owner.arena?, Option.some.injEq] at ha'; subst ha'
        exact hcentral blk hblk hid)
  sysInv :=
    { quota := creditArena_preserves_quota st.sys a size hwf.sysInv.quota
      arenasNodup := by
        have := hwf.sysInv.arenasNodup
        rw [← creditArena_map_arena st.sys a size] at this; exact this }
  rel := by
    refine ⟨fun d hd => ?_, fun bk hbk => hwf.rel.2 bk hbk⟩
    exact creditArena_arenaAuthOf_isSome st.sys a size (hwf.rel.1 d hd)
  labels := by
    refine LabelPartition_of_same (st1 := st.freeStep b a sc size)
      (st2 := st.withTopo (st.topo.setOwner b (Owner.centralFree a sc))) rfl ?_
      (label_partition_preserved_free st b a sc hwf.topoWf.uniqueOwner hwf.labels hmatch)
    intro blk
    simp only [TopoSeLe4n.blockLabel, TopoSeLe4n.blockArena, TopoSeLe4n.freeStep, withTopo_topo,
      withTopo_sys]
    cases (st.topo.setOwner b (Owner.centralFree a sc)).spanById blk.span with
    | none => rfl
    | some d => simp only [Option.map_some, Option.bind_some]; exact creditArena_label st.sys a size d.arena

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

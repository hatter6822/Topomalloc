-- SPDX-License-Identifier: GPL-3.0-or-later
/-
SMP / per-core multicore model and proofs (SPEC §36.17 SMP forms, plan 02 W1-14).

A genuine multicore treatment by *interleaving semantics*. The RSEQ contract (W1-7) makes
each per-core fast-path step atomic, so a concurrent execution linearises to a sequence of
atomic per-core actions; SMP correctness is then "the invariants hold for **every**
interleaving (every schedule)", proved by induction over the schedule from per-action
lemmas. This is the standard atomic-step concurrency argument at the abstraction level the
SPEC defines (sub-step interleaving is precluded by the hardware RSEQ guarantee).

`SmpState` is the multicore object accounting for one size class: each core's cache count,
the shared central pool, and the live (handed-out) count. The proved SMP forms:

* `runSmp_conserves` — **concurrent conservation**: no object is lost or duplicated under
  *any* interleaving of per-core pop/push/refill/flush and migrations (the SMP extension of
  `cache_*_preserves_ownership_conservation`).
* `applySmp_core_isolation` / `runSmp_core_isolation` — a step on one core never changes
  another core's cache (the SMP extension of `per_core_cache_abort_no_change`).
* `applySmp_abort` — a migrated (aborted) fast path is a no-op (`smp_migration_no_lost_object`).
* `runSmp_low_equivalence` — **non-interference under concurrency**: a schedule of
  high-domain core steps preserves the low-domain observation (the SMP extension of
  `topo_step_preserves_low_equivalence`).

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Types

namespace TopoMalloc.SeLe4n

open TopoMalloc

/-- The multicore accounting state for one size class: per-core cache counts, the shared
central pool, and the live (allocated) count. -/
structure SmpState where
  cache : CpuId → Nat
  central : Nat
  live : Nat

/-- One atomic per-core action (the linearised unit of concurrency). -/
inductive SmpAction where
  /-- Core `i` pops one object from its cache (alloc fast path). -/
  | pop (i : CpuId)
  /-- Core `i` pushes one live object back to its cache (free fast path). -/
  | push (i : CpuId)
  /-- Core `i` refills `k` objects from the central pool. -/
  | refill (i : CpuId) (k : Nat)
  /-- Core `i` flushes `k` objects to the central pool. -/
  | flush (i : CpuId) (k : Nat)
  /-- A preempted/migrated fast path: a no-op (the RSEQ abort case). -/
  | abort

/-- The core an action runs on, if any. -/
def SmpAction.core : SmpAction → Option CpuId
  | pop i => some i
  | push i => some i
  | refill i _ => some i
  | flush i _ => some i
  | abort => none

/-- Execute one atomic action. Each guard (`0 < …`, `k ≤ …`) models the genuine
underflow/overflow that routes to the slow path; on failure the state is unchanged. Each
modified state is an explicit record so the projections reduce directly. -/
def applySmp (s : SmpState) : SmpAction → SmpState
  | .pop i =>
    if 0 < s.cache i then ⟨fun c => if c = i then s.cache i - 1 else s.cache c, s.central, s.live + 1⟩ else s
  | .push i =>
    if 0 < s.live then ⟨fun c => if c = i then s.cache i + 1 else s.cache c, s.central, s.live - 1⟩ else s
  | .refill i k =>
    if k ≤ s.central then ⟨fun c => if c = i then s.cache i + k else s.cache c, s.central - k, s.live⟩ else s
  | .flush i k =>
    if k ≤ s.cache i then ⟨fun c => if c = i then s.cache i - k else s.cache c, s.central + k, s.live⟩ else s
  | .abort => s

/-- **`smp_migration_no_lost_object` (§36.17 SMP).** An aborted (migrated) fast path is a
no-op — the global state is unchanged, so no object is lost. -/
@[simp] theorem applySmp_abort (s : SmpState) : applySmp s .abort = s := rfl

/-- The per-core cache function after atomically setting core `i` to `v`. -/
def upd (g : CpuId → Nat) (i : CpuId) (v : Nat) : CpuId → Nat := fun c => if c = i then v else g c

@[simp] theorem upd_other (g : CpuId → Nat) (i j : CpuId) (v : Nat) (h : j ≠ i) :
    upd g i v j = g j := by simp [upd, h]

/- Each action reduces (definitionally) to either an explicit updated record or the
unchanged state, by the guard. These name the reduced forms for clean rewriting. -/
theorem applySmp_pop_pos (s : SmpState) (i : CpuId) (hg : 0 < s.cache i) :
    applySmp s (.pop i) = ⟨upd s.cache i (s.cache i - 1), s.central, s.live + 1⟩ := if_pos hg
theorem applySmp_pop_neg (s : SmpState) (i : CpuId) (hg : ¬ 0 < s.cache i) :
    applySmp s (.pop i) = s := if_neg hg
theorem applySmp_push_pos (s : SmpState) (i : CpuId) (hg : 0 < s.live) :
    applySmp s (.push i) = ⟨upd s.cache i (s.cache i + 1), s.central, s.live - 1⟩ := if_pos hg
theorem applySmp_push_neg (s : SmpState) (i : CpuId) (hg : ¬ 0 < s.live) :
    applySmp s (.push i) = s := if_neg hg
theorem applySmp_refill_pos (s : SmpState) (i k : CpuId) (hg : k ≤ s.central) :
    applySmp s (.refill i k) = ⟨upd s.cache i (s.cache i + k), s.central - k, s.live⟩ := if_pos hg
theorem applySmp_refill_neg (s : SmpState) (i k : CpuId) (hg : ¬ k ≤ s.central) :
    applySmp s (.refill i k) = s := if_neg hg
theorem applySmp_flush_pos (s : SmpState) (i k : CpuId) (hg : k ≤ s.cache i) :
    applySmp s (.flush i k) = ⟨upd s.cache i (s.cache i - k), s.central + k, s.live⟩ := if_pos hg
theorem applySmp_flush_neg (s : SmpState) (i k : CpuId) (hg : ¬ k ≤ s.cache i) :
    applySmp s (.flush i k) = s := if_neg hg

/-- **Core isolation (§36.17 SMP), per step.** An action running on core `i` never changes
core `j`'s cache, for `j ≠ i`: the SMP extension of `per_core_cache_abort_no_change`. -/
theorem applySmp_core_isolation (s : SmpState) (act : SmpAction) (j : CpuId)
    (h : act.core ≠ some j) : (applySmp s act).cache j = s.cache j := by
  cases act with
  | abort => rfl
  | pop i =>
    have hji : j ≠ i := fun hc => h (by rw [hc]; rfl)
    by_cases hg : 0 < s.cache i
    · rw [applySmp_pop_pos s i hg]; exact upd_other _ i j _ hji
    · rw [applySmp_pop_neg s i hg]
  | push i =>
    have hji : j ≠ i := fun hc => h (by rw [hc]; rfl)
    by_cases hg : 0 < s.live
    · rw [applySmp_push_pos s i hg]; exact upd_other _ i j _ hji
    · rw [applySmp_push_neg s i hg]
  | refill i k =>
    have hji : j ≠ i := fun hc => h (by rw [hc]; rfl)
    by_cases hg : k ≤ s.central
    · rw [applySmp_refill_pos s i k hg]; exact upd_other _ i j _ hji
    · rw [applySmp_refill_neg s i k hg]
  | flush i k =>
    have hji : j ≠ i := fun hc => h (by rw [hc]; rfl)
    by_cases hg : k ≤ s.cache i
    · rw [applySmp_flush_pos s i k hg]; exact upd_other _ i j _ hji
    · rw [applySmp_flush_neg s i k hg]

/-- The multicore conserved quantity: cached (over the machine's `cores`) + central + live.
Total object count, which every action preserves. -/
def SmpState.total (s : SmpState) (cores : List CpuId) : Nat :=
  (cores.map s.cache).sum + s.central + s.live

@[simp] theorem total_mk (f : CpuId → Nat) (c l : Nat) (cores : List CpuId) :
    (SmpState.mk f c l).total cores = (cores.map f).sum + c + l := rfl

/-- Changing one core's count changes the sum over a `Nodup` core list by exactly the
delta (stated additively to stay in `Nat`). -/
theorem sum_update (g : CpuId → Nat) (i : CpuId) (v : Nat) :
    ∀ (cores : List CpuId), i ∈ cores → cores.Nodup →
      (cores.map (upd g i v)).sum + g i = (cores.map g).sum + v := by
  intro cores
  induction cores with
  | nil => intro hi _; exact absurd hi (by simp)
  | cons c rest ih =>
    intro hi hnodup
    rw [List.nodup_cons] at hnodup
    simp only [List.map_cons, List.sum_cons]
    by_cases hc : c = i
    · subst hc
      rw [show upd g c v c = v from by simp [upd]]
      have hrest : rest.map (upd g c v) = rest.map g :=
        List.map_congr_left fun d hd => upd_other g c d v (fun he => hnodup.1 (by rw [← he]; exact hd))
      rw [hrest]; omega
    · rw [upd_other g i c v hc]
      have hir : i ∈ rest := (List.mem_cons.mp hi).resolve_left (fun he => hc he.symm)
      have := ih hir hnodup.2; omega

/-- **Concurrent conservation (§36.17 SMP), per step.** Every atomic action on a core in
`cores` preserves the total object count. -/
theorem applySmp_conserves (cores : List CpuId) (hnodup : cores.Nodup) (s : SmpState)
    (act : SmpAction) (hcore : ∀ i, act.core = some i → i ∈ cores) :
    (applySmp s act).total cores = s.total cores := by
  cases act with
  | abort => rfl
  | pop i =>
    have hsu := sum_update s.cache i (s.cache i - 1) cores (hcore i rfl) hnodup
    by_cases hg : 0 < s.cache i
    · rw [applySmp_pop_pos s i hg, total_mk, SmpState.total]; omega
    · rw [applySmp_pop_neg s i hg]
  | push i =>
    have hsu := sum_update s.cache i (s.cache i + 1) cores (hcore i rfl) hnodup
    by_cases hg : 0 < s.live
    · rw [applySmp_push_pos s i hg, total_mk, SmpState.total]; omega
    · rw [applySmp_push_neg s i hg]
  | refill i k =>
    have hsu := sum_update s.cache i (s.cache i + k) cores (hcore i rfl) hnodup
    by_cases hg : k ≤ s.central
    · rw [applySmp_refill_pos s i k hg, total_mk, SmpState.total]; omega
    · rw [applySmp_refill_neg s i k hg]
  | flush i k =>
    have hsu := sum_update s.cache i (s.cache i - k) cores (hcore i rfl) hnodup
    by_cases hg : k ≤ s.cache i
    · rw [applySmp_flush_pos s i k hg, total_mk, SmpState.total]; omega
    · rw [applySmp_flush_neg s i k hg]

/-- Execute a whole schedule (an interleaving of atomic per-core actions). -/
def runSmp (s : SmpState) (sched : List SmpAction) : SmpState := sched.foldl applySmp s

@[simp] theorem runSmp_nil (s : SmpState) : runSmp s [] = s := rfl

theorem runSmp_cons (s : SmpState) (a : SmpAction) (rest : List SmpAction) :
    runSmp s (a :: rest) = runSmp (applySmp s a) rest := by simp [runSmp, List.foldl_cons]

/-- **`runSmp_conserves` — concurrent conservation (§36.17 SMP).** *Every* interleaving of
per-core pop/push/refill/flush and migrations conserves the total object count: no object
is lost or duplicated under concurrency. -/
theorem runSmp_conserves (cores : List CpuId) (hnodup : cores.Nodup) :
    ∀ (s : SmpState) (sched : List SmpAction),
      (∀ act ∈ sched, ∀ i, act.core = some i → i ∈ cores) →
      (runSmp s sched).total cores = s.total cores := by
  intro s sched
  induction sched generalizing s with
  | nil => intro _; rfl
  | cons a rest ih =>
    intro hsched
    rw [runSmp_cons, ih _ (fun act ha => hsched act (List.mem_cons_of_mem _ ha))]
    exact applySmp_conserves cores hnodup s a (hsched a (List.mem_cons_self ..))

/-- **`runSmp_core_isolation` (§36.17 SMP).** Under any schedule that never touches core
`j`, core `j`'s cache is unchanged — a thread on `j` is unaffected by other cores. -/
theorem runSmp_core_isolation (j : CpuId) :
    ∀ (s : SmpState) (sched : List SmpAction),
      (∀ act ∈ sched, act.core ≠ some j) →
      (runSmp s sched).cache j = s.cache j := by
  intro s sched
  induction sched generalizing s with
  | nil => intro _; rfl
  | cons a rest ih =>
    intro hsched
    rw [runSmp_cons, ih _ (fun act ha => hsched act (List.mem_cons_of_mem _ ha)),
      applySmp_core_isolation s a j (hsched a (List.mem_cons_self ..))]

/- ----------------------------------------------------------------------- -/
/- Non-interference under concurrency (§36.12 SMP).                         -/
/- ----------------------------------------------------------------------- -/

/-- The low-domain observation of the multicore state: the cached counts on the cores a
low observer is permitted to see. -/
def SmpState.lowView (s : SmpState) (lowCores : List CpuId) : List Nat :=
  lowCores.map s.cache

/-- **`runSmp_low_equivalence` — non-interference under concurrency (§36.17/§36.12 SMP).** A
schedule whose actions all run on *high* cores (outside `lowCores`) leaves the low-domain
view unchanged: concurrent high-domain allocation activity is invisible to a low observer. -/
theorem runSmp_low_equivalence (lowCores : List CpuId) (s : SmpState) (sched : List SmpAction)
    (hhigh : ∀ act ∈ sched, ∀ j ∈ lowCores, act.core ≠ some j) :
    (runSmp s sched).lowView lowCores = s.lowView lowCores := by
  unfold SmpState.lowView
  apply List.map_congr_left
  intro j hj
  exact runSmp_core_isolation j s sched (fun act ha => hhigh act ha j hj)

end TopoMalloc.SeLe4n

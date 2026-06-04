-- SPDX-License-Identifier: MIT
/-
The RSEQ abstraction (SPEC §33.5, plan 02 W1-7, DD-2) — the load-bearing axiom.

The restartable-sequence pop/push is modelled first as a *trusted primitive* with
a precise contract, exactly as §33.5 prescribes. `rseqPop`/`rseqPush` are
postulated (not defined): on real hardware they are per-architecture restartable
assembly (refined in W1-14 / open question 7), so within the model they are opaque
functions pinned only by the contract axioms below.

The three outcomes are deliberately **distinct**:

* `abort` — preempted/migrated; the state is unchanged and the caller retries.
* `empty`/`full` — genuine logical underflow/overflow that routes to the slow path.
* `success` — exactly one block changes owner, under a **frame condition**.

The frame clause ("only the popped/pushed block changes owner") is load-bearing:
the refill/flush conservation theorems (§33.4, W1-6c) cannot be proved without it,
and conflating `abort` with `empty`/`full` would let the model "prove" progress the
implementation never makes.

**Consistency.** Postulating `rseqPop`/`rseqPush` plus their contracts is sound:
the interpretation that *always* returns `.abort` satisfies every contract (the
`abort` case is `True`), so the axioms admit a model and add no inconsistency.
-/
import TopoMalloc.WellFormed

namespace TopoMalloc

open State

/-- The three outcomes of a restartable-sequence pop (§33.5). Distinct constructors
keep `abort` (retry) apart from `empty` (slow path). -/
inductive RseqPop where
  /-- Preempted or migrated; state unchanged, caller retries. -/
  | abort
  /-- No object of this class on this CPU; caller takes the slow path. -/
  | empty
  /-- Popped block `p`, producing successor state `s'`. -/
  | success (p : BlockId) (s' : State)

/-- The three outcomes of a restartable-sequence push (§33.5). -/
inductive RseqPush where
  /-- Preempted or migrated; state unchanged, caller retries. -/
  | abort
  /-- Cache at capacity; caller flushes (slow path). -/
  | full
  /-- Pushed the block, producing successor state `s'`. -/
  | success (s' : State)

/-- The trusted RSEQ pop primitive (§33.5). Postulated; pinned only by
`rseq_pop_contract`. -/
axiom rseqPop : State → CpuId → SizeClassId → RseqPop

/-- The trusted RSEQ push primitive (§33.5). Postulated; pinned only by
`rseq_push_contract`. -/
axiom rseqPush : State → CpuId → SizeClassId → BlockId → RseqPush

/-- **The RSEQ pop contract (§33.5).** `abort` ⇒ nothing asserted (state untouched);
`empty` ⇒ the per-CPU cache for this class is genuinely empty; `success p s'` ⇒ `p`
moves from `cpuCache cpu sc` to `live`, `s'` is well-formed, and **only `p` changes
owner** (the frame condition). -/
axiom rseq_pop_contract (s : State) (cpu : CpuId) (sc : SizeClassId) (hwf : WellFormed s) :
    match rseqPop s cpu sc with
    | .abort => True
    | .empty => s.countOwned (Owner.cpuCache cpu sc) = 0
    | .success p s' =>
        s.ownerOf p = some (Owner.cpuCache cpu sc) ∧
        s'.ownerOf p = some Owner.live ∧
        WellFormed s' ∧
        (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q)

/-- **The RSEQ push contract (§33.5).** Symmetric to the pop contract: `full` ⇒ the
cache is genuinely at capacity; `success s'` ⇒ the live block `p` moves to
`cpuCache cpu sc`, `s'` is well-formed, and only `p` changes owner. -/
axiom rseq_push_contract (s : State) (cpu : CpuId) (sc : SizeClassId) (p : BlockId)
    (hwf : WellFormed s) (hlive : s.ownerOf p = some Owner.live) :
    match rseqPush s cpu sc p with
    | .abort => True
    | .full => maxLocalCapacity sc ≤ s.countOwned (Owner.cpuCache cpu sc)
    | .success s' =>
        s'.ownerOf p = some (Owner.cpuCache cpu sc) ∧
        WellFormed s' ∧
        (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q)

/-- Apply the pop primitive: on `success` advance to the successor state; on
`abort`/`empty` the state is unchanged (the caller retries or takes the slow path).
`noncomputable`: it wraps the postulated primitive, which has no runtime code — the
model is not on the production hot path (§33.1). -/
noncomputable def rseqPopStep (s : State) (cpu : CpuId) (sc : SizeClassId) : State :=
  match rseqPop s cpu sc with
  | .success _ s' => s'
  | _ => s

/-- Apply the push primitive, analogous to `rseqPopStep`. -/
noncomputable def rseqPushStep (s : State) (cpu : CpuId) (sc : SizeClassId) (p : BlockId) : State :=
  match rseqPush s cpu sc p with
  | .success s' => s'
  | _ => s

/-- **`per_core_cache_abort_no_change` (§36.17, single-core, W1-12d), pop side.** An
aborted fast path leaves allocator-visible state unchanged. -/
theorem rseqPopStep_abort_no_change (s : State) (cpu : CpuId) (sc : SizeClassId)
    (h : rseqPop s cpu sc = RseqPop.abort) : rseqPopStep s cpu sc = s := by
  unfold rseqPopStep; rw [h]

/-- Push-side abort leaves state unchanged. -/
theorem rseqPushStep_abort_no_change (s : State) (cpu : CpuId) (sc : SizeClassId) (p : BlockId)
    (h : rseqPush s cpu sc p = RseqPush.abort) : rseqPushStep s cpu sc p = s := by
  unfold rseqPushStep; rw [h]

/-- A successful pop conserves ownership: every block other than the popped one is
unchanged (the §33.4 frame condition, the corollary W1-6c consumes). -/
theorem rseq_pop_success_frame (s : State) (cpu : CpuId) (sc : SizeClassId)
    (hwf : WellFormed s) {p : BlockId} {s' : State} (h : rseqPop s cpu sc = RseqPop.success p s') :
    s.ownerOf p = some (Owner.cpuCache cpu sc) ∧ s'.ownerOf p = some Owner.live ∧
      WellFormed s' ∧ (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q) := by
  have hc := rseq_pop_contract s cpu sc hwf
  rw [h] at hc
  exact hc

/-- A successful push conserves ownership symmetrically. -/
theorem rseq_push_success_frame (s : State) (cpu : CpuId) (sc : SizeClassId) (p : BlockId)
    (hwf : WellFormed s) (hlive : s.ownerOf p = some Owner.live) {s' : State}
    (h : rseqPush s cpu sc p = RseqPush.success s') :
    s'.ownerOf p = some (Owner.cpuCache cpu sc) ∧ WellFormed s' ∧
      (∀ q, q ≠ p → s'.ownerOf q = s.ownerOf q) := by
  have hc := rseq_push_contract s cpu sc p hwf hlive
  rw [h] at hc
  exact hc

end TopoMalloc

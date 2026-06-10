-- SPDX-License-Identifier: MIT
/-
The arena lifecycle state machine (SPEC §22.3 / §36.13, plan 06 W9-2/W9-6).

`ArenaState` is the lifecycle of one arena (policy + authority domain, §22), and
`canTransition` is the legal one-step relation the W9 lifecycle operations produce
(publish / reset / destroy / quarantine / retry). It is the single source of truth
the Rust `ArenaState::can_transition` is pinned to — by the
`arena_lifecycle_matches_lean` unit test and the `arenaLifecycleGate` in
`Check.lean` — so the runtime machine and this model cannot drift. This is the
§22.3 analogue of the §36.6 `BackingState` differential (`providerChainGate`) and
the §20.1 `ExtentState` differential (`extentStateGate`).

The two safety spines this relation encodes:

* **Allocations only in `active` (§22.3).** Every teardown state refuses new
  allocations and new delegations (`allowsAlloc` / `allowsDelegation`), which is
  W9-6a's "reject new allocations + delegations" the moment draining begins.
* **Partial failure never reaches `destroyed` (§36.13).** The only edge into
  `destroyed` leaves `draining` — the successful completion of the full
  unmap → revoke → recycle protocol. A failed step lands in `errorQuarantined`,
  whose only exit is back into `draining` (a destroy retry); it can never jump to
  `destroyed` directly (`quarantine_only_retries_draining`).

Self-transitions are not steps (as in `BackingState`, unlike the idempotent-op
reflexivity of `ExtentState`): every lifecycle operation moves the state.

A *failed creation* is deliberately not modeled: §22.4 publishes the arena id only
after complete initialization, so an arena whose construction fails was never
observable — its slot is reclaimed outside the lifecycle, exactly as a span
descriptor slot is recycled outside the span state machine (§16.6).
-/

namespace TopoMalloc

/-- The §22.3 arena lifecycle states, plus §36.13's `ERROR_QUARANTINED`. -/
inductive ArenaState where
  /-- Descriptor under construction; the id is not yet published (§22.4). -/
  | initializing
  /-- Serving allocations — the only state that admits them (§22.3). -/
  | active
  /-- A destroy/revocation in progress (§36.13): new allocations and
  delegations are rejected, caches drain, backing is unmapped/revoked. -/
  | draining
  /-- A reset in progress (§22.5): outstanding allocations are being discarded. -/
  | resetting
  /-- Terminal: reset + metadata removal complete; the id is never reused
  while stale references can exist (§22.6, B.5). -/
  | destroyed
  /-- A reset/destroy step failed partway (§36.13): the arena is quarantined —
  never reported `destroyed` — until a destroy retry completes. -/
  | errorQuarantined
  deriving Repr, DecidableEq, BEq

namespace ArenaState

/-- Whether new allocations are admitted (§22.3: only `active`). This is the
W9-2 gate the runtime checks before every allocation. -/
def allowsAlloc : ArenaState → Bool
  | active => true
  | _ => false

/-- Whether new delegations are admitted (§36.4/§36.13, W9-6a: a draining arena
rejects new delegations; only `active` admits them). -/
def allowsDelegation : ArenaState → Bool
  | active => true
  | _ => false

/-- The canonical legal lifecycle transitions (non-reflexive directed edges) the
W9 operations produce — the single source of truth the Rust
`ArenaState::can_transition` is pinned to:

* **publish** (§22.4): `initializing → active`, only after complete init;
* **reset begin/end** (§22.5): `active → resetting → active` — the arena
  remains `active` unless destroy follows;
* **destroy begin/end** (§36.13/W9-6): `active → draining → destroyed`;
* **partial failure** (§36.13/W9-6e): `resetting | draining → errorQuarantined`
  — never `destroyed`;
* **destroy retry**: `errorQuarantined → draining` (the only exit). -/
def arenaEdges : List (ArenaState × ArenaState) :=
  [ (initializing, active),
    (active, resetting), (resetting, active), (resetting, errorQuarantined),
    (active, draining), (draining, destroyed), (draining, errorQuarantined),
    (errorQuarantined, draining) ]

/-- `(s, t)` is a canonical directed edge (BEq-based, no `Prod` instances needed). -/
def hasEdge (s t : ArenaState) : Bool :=
  arenaEdges.any (fun e => e.1 == s && e.2 == t)

/-- The legal one-step §22.3/§36.13 transition relation: exactly the canonical
edges (no reflexivity — every lifecycle operation moves the state). Total and
decidable; the exact mirror of the Rust `ArenaState::can_transition`. -/
def canTransition : ArenaState → ArenaState → Bool
  -- publish (§22.4)
  | initializing, active => true
  -- reset begin / reset complete / reset failure (§22.5, §36.13)
  | active, resetting => true
  | resetting, active => true
  | resetting, errorQuarantined => true
  -- destroy begin / destroy complete / destroy failure (§36.13, W9-6)
  | active, draining => true
  | draining, destroyed => true
  | draining, errorQuarantined => true
  -- destroy retry: quarantine re-enters the draining protocol (W9-6e)
  | errorQuarantined, draining => true
  | _, _ => false

/-- The six states, for exhaustive gating. -/
def all : List ArenaState :=
  [initializing, active, draining, resetting, destroyed, errorQuarantined]

/-- **W9-2 (§22.3).** Allocations are admitted in exactly the `active` state. -/
theorem allowsAlloc_iff_active : ∀ s, allowsAlloc s = true ↔ s = active := by
  intro s; cases s <;> decide

/-- **W9-6a (§36.13).** Delegations are admitted in exactly the `active` state —
a draining (or resetting, or quarantined) arena rejects new delegations. -/
theorem allowsDelegation_iff_active : ∀ s, allowsDelegation s = true ↔ s = active := by
  intro s; cases s <;> decide

/-- **§22.6.** `destroyed` is terminal: no transition leaves it. A destroyed
arena id is dead forever (B.5: never reused while stale references can exist). -/
theorem destroyed_is_terminal : ∀ t, canTransition destroyed t = false := by
  intro t; cases t <;> decide

/-- **§36.13, the partial-failure spine (W9-6e).** The only edge into `destroyed`
leaves `draining`: an arena can be reported destroyed only by completing the full
draining protocol — a partial failure (which lands in `errorQuarantined`) or any
other state can never reach `destroyed` in one step. -/
theorem only_draining_reaches_destroyed :
    ∀ s, canTransition s destroyed = true → s = draining := by
  intro s; cases s <;> decide

/-- **§36.13 (W9-6e).** Quarantine's only exit is a destroy retry — re-entering
`draining` to re-run the protocol. In particular it cannot jump to `destroyed`
(that would report success for an arena whose revocation never completed). -/
theorem quarantine_only_retries_draining :
    ∀ t, canTransition errorQuarantined t = true → t = draining := by
  intro t; cases t <;> decide

/-- **§22.4.** Nothing re-enters `initializing`: it is the pre-publication state,
and publication is one-way. (A failed construction is reclaimed outside the
lifecycle — the arena was never observable.) -/
theorem nothing_reenters_initializing :
    ∀ s, canTransition s initializing = false := by
  intro s; cases s <;> decide

/-- **W9-2 acceptance ("illegal-state ops rejected").** Every teardown or
pre-publication state refuses allocations. -/
theorem teardown_states_reject_alloc :
    ∀ s, s ≠ active → allowsAlloc s = false := by
  intro s; cases s <;> simp [allowsAlloc]

/-- **§22.5.** A reset that completes returns the arena to `active` (it remains
active unless destroy follows): `resetting` steps only to `active` or to the
quarantine failure state — never directly to `destroyed` or `draining`. -/
theorem resetting_exits_to_active_or_quarantine :
    ∀ t, canTransition resetting t = true → t = active ∨ t = errorQuarantined := by
  intro t; cases t <;> simp [canTransition]

/-- The relation is exactly the canonical edge list (the shape the
`arenaLifecycleGate` in `Check.lean` evaluates over all 36 ordered pairs, and the
Rust `arena_lifecycle_matches_lean` test mirrors). -/
theorem canTransition_iff_hasEdge :
    ∀ s t, canTransition s t = hasEdge s t := by
  intro s t; cases s <;> cases t <;> decide

end ArenaState

end TopoMalloc

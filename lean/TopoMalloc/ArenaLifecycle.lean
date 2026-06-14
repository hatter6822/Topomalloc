-- SPDX-License-Identifier: MIT
/-
The arena lifecycle state machine (SPEC §22.3, extended by §36.13, plan 06 W9-2/
W9-4/W9-6). This models the *states* an arena occupies and the legal transitions
between them, mirroring the runtime `ArenaState`/`RevocationPhase` machines in
`crates/topo-core/src/arena.rs` one-for-one (the Rust differential tests
`state_machine_is_exactly_the_spec_graph` and
`revocation_phases_are_a_linear_chain_*` pin the Rust side to the same edge sets,
so the two cannot drift).

The *transitions on allocator state* (`arenaReset`/`arenaDestroy`) and the
capability-attenuation invariants already live elsewhere — `Transitions.lean` /
`Theorems/Arena.lean` (core) and `SeLe4n/CapBackedArena.lean` /
`SeLe4n/Refinement.lean` (bridge). This module adds the missing piece those
assume: the lifecycle *phase* machine whose safety properties are

* allocations are admitted **only** in `Active` (§22.3);
* a reset completes back to `Active` (§22.5), a destroy to `Destroyed` (§36.13);
* a partial failure quarantines, and a quarantined (or destroyed) arena is
  **terminal** — it can never step to `Destroyed`, so "partial failure MUST leave
  the arena in DRAINING/ERROR_QUARANTINED, never DESTROYED" (§36.13) holds; and
* the revocation protocol is a single linear chain — **unmap before revoke before
  recycle** (§36.13 / plan 06 DD-3).
-/
namespace TopoMalloc

/-- An arena's lifecycle phase (§22.3 + §36.13 `ERROR_QUARANTINED`). Mirrors the
runtime `ArenaState` (`arena.rs`). -/
inductive ArenaPhase where
  /-- Created, initializing; not yet allocatable (§22.4). -/
  | initializing
  /-- Live and serving allocations (the only allocating state, §22.3). -/
  | active
  /-- Reset in progress (§22.5); completes back to `active`. -/
  | resetting
  /-- Destroy in progress (§36.13); completes to `destroyed`. -/
  | draining
  /-- Fully destroyed (§22.6); terminal. -/
  | destroyed
  /-- Partial-failure landing zone (§36.13); terminal. -/
  | errorQuarantined
  deriving Repr, DecidableEq

namespace ArenaPhase

/-- Every phase, for the exhaustive `lake exe check` differential (the G-arena
gate in `Check.lean`). -/
def all : List ArenaPhase :=
  [initializing, active, resetting, draining, destroyed, errorQuarantined]

/-- Whether allocations are admitted in this phase (§22.3: only `active`). -/
def isActive : ArenaPhase → Bool
  | active => true
  | _ => false

/-- Whether the phase is terminal (no successors). -/
def isTerminal : ArenaPhase → Bool
  | destroyed => true
  | errorQuarantined => true
  | _ => false

/-- The legal lifecycle transitions (§22.3 + §36.13). The graph is acyclic except
for `resetting → active` (a completed reset returns the arena to service, §22.5).
Mirrors `ArenaState::can_transition` (`arena.rs`) exactly. -/
def canStep : ArenaPhase → ArenaPhase → Bool
  | initializing, active => true        -- creation completes (§22.4)
  | active, resetting => true           -- reset begins (§22.5)
  | resetting, active => true           -- reset completes (§22.5)
  | active, draining => true            -- destroy begins (§36.13)
  | draining, destroyed => true         -- destroy completes (§36.13)
  | resetting, errorQuarantined => true -- reset partial failure (§36.13)
  | draining, errorQuarantined => true  -- destroy partial failure: a refused revoke
                                        -- OR a custom backing refusing its region
                                        -- return on teardown (§36.13, W10)
  | _, _ => false

/-- **Allocations only in `Active` (§22.3).** A phase admits allocation iff it is
`active`. -/
theorem alloc_admitted_iff_active (p : ArenaPhase) : p.isActive = true ↔ p = active := by
  cases p <;> simp [isActive]

/-- **A reset begins only from `Active` (§22.5 precondition).** Entering
`Resetting` is legal only from `Active`, so the `arenaReset` state transition
(`Transitions.lean`) fires only on a live arena. -/
theorem reset_begins_from_active : ∀ p, canStep p resetting = true → p = active := by
  intro p h; cases p <;> simp_all [canStep]

/-- **A destroy begins only from `Active` (§36.13 step 1 precondition).** -/
theorem destroy_begins_from_active : ∀ p, canStep p draining = true → p = active := by
  intro p h; cases p <;> simp_all [canStep]

/-- **A completed reset returns the arena to service (§22.5).** -/
theorem reset_completes_to_active : canStep resetting active = true := rfl

/-- **A completed destroy reaches `Destroyed` (§36.13).** -/
theorem destroy_completes_to_destroyed : canStep draining destroyed = true := rfl

/-- **The only predecessor of `Destroyed` is `Draining` (§36.13).** An arena can
reach `Destroyed` only through the destroy protocol's final step — never from a
reset, an initialization, or a quarantine. -/
theorem only_draining_reaches_destroyed :
    ∀ p, canStep p destroyed = true → p = draining := by
  intro p h; cases p <;> simp_all [canStep]

/-- **A destroyed arena is terminal.** -/
theorem destroyed_is_terminal (p : ArenaPhase) : canStep destroyed p = false := by
  cases p <;> rfl

/-- **A quarantined arena is terminal (§36.13).** Once a partial failure has
quarantined the arena it has *no* successor — in particular it can never step to
`Destroyed`. This is the heart of "partial failure MUST leave the arena in
DRAINING or ERROR_QUARANTINED, not DESTROYED": the quarantine landing is a dead
end, so a half-revoked arena is never reported as a clean teardown. -/
theorem quarantined_is_terminal (p : ArenaPhase) : canStep errorQuarantined p = false := by
  cases p <;> rfl

/-- Corollary: there is no single step from a partial-failure quarantine to a
clean `Destroyed` (§36.13). -/
theorem quarantine_cannot_reach_destroyed : canStep errorQuarantined destroyed = false :=
  quarantined_is_terminal destroyed

/-- **A destroy that cannot return its backing quarantines, never destroys (§36.13,
extended for the W10 custom-backing teardown).** The destroy protocol's *only*
failure landing is `errorQuarantined`, whatever the partial-failure reason — a
refused capability revoke **or** a custom backing (`ExtentHooks`) refusing its
region return on teardown (`HookProvider::release` failing). Both route through the
same `draining → errorQuarantined` edge, and that landing is terminal, so a destroy
whose backing-release fails is never reported as a clean `Destroyed`. This is the
formal counterpart of the runtime `arena_destroy` returning `Err` and quarantining
when `teardown_hook_backend` surfaces a hook `dealloc` failure. -/
theorem destroy_backing_release_failure_quarantines :
    canStep draining errorQuarantined = true ∧ canStep errorQuarantined destroyed = false :=
  ⟨rfl, quarantine_cannot_reach_destroyed⟩

/-- A terminal phase has no successors (the general statement). -/
theorem terminal_has_no_successor {p : ArenaPhase} (h : p.isTerminal = true) (q : ArenaPhase) :
    canStep p q = false := by
  cases p <;> first | (cases q <;> rfl) | simp [isTerminal] at h

/-- Allocation and the terminal/transition phases are disjoint: an arena admitting
allocation is `active`, which is neither terminal nor a partial-failure landing. -/
theorem active_is_not_terminal : (active : ArenaPhase).isTerminal = false := rfl

end ArenaPhase

/-- A phase in the §36.13 arena-revocation protocol (plan 06 DD-3). Mirrors the
runtime `RevocationPhase` (`arena.rs`). The order is the whole game: **unmap
before revoke before recycle**. -/
inductive RevPhase where
  /-- Enter DRAINING: reject new allocations/delegations (§36.13, W9-6a). -/
  | draining
  /-- Drain local/transfer caches + central lists (§36.13, W9-6b). -/
  | drainCaches
  /-- Unmap client VSpace windows; scrub if needed (§36.13, W9-6c). -/
  | unmap
  /-- Revoke derived frame/mapping caps; delete CSlots (§36.13, W9-6d). -/
  | revoke
  /-- Recycle untyped backing to the free pools (§36.13, W9-6d). -/
  | recycle
  /-- Finalize: mark DESTROYED, generation++ (§36.13, W9-6e). -/
  | finalize
  deriving Repr, DecidableEq

namespace RevPhase

/-- The next phase in the ordered protocol, or `none` at `finalize` — the single
linear chain. Mirrors `RevocationPhase::next` (`arena.rs`). -/
def next : RevPhase → Option RevPhase
  | draining => some drainCaches
  | drainCaches => some unmap
  | unmap => some revoke
  | revoke => some recycle
  | recycle => some finalize
  | finalize => none

/-- The position of a phase in the protocol (its 0-based step index). -/
def rank : RevPhase → Nat
  | draining => 0
  | drainCaches => 1
  | unmap => 2
  | revoke => 3
  | recycle => 4
  | finalize => 5

/-- **The protocol is a single linear chain.** Each non-final phase advances to
exactly the next rank; the chain terminates at `finalize`. -/
theorem next_advances_rank :
    ∀ p q, next p = some q → rank q = rank p + 1 := by
  intro p q h; cases p <;> cases q <;> simp_all [next, rank]

/-- **The chain terminates (acyclicity).** `finalize` has no successor, so the
protocol cannot loop — the recycle-after-revoke-after-unmap ordering is exactly
this acyclicity. -/
theorem chain_terminates : next finalize = none := rfl

/-- **Unmap precedes revoke (§36.13).** The cardinal ordering: a client mapping is
made unreachable before its capabilities are revoked. -/
theorem unmap_before_revoke : rank unmap < rank revoke := by decide

/-- **Revoke precedes recycle (§36.13).** Backing is recycled only *after* its
derived capabilities are revoked — recycling earlier would hand live authority to
another security domain. -/
theorem revoke_before_recycle : rank revoke < rank recycle := by decide

/-- The full safety ordering: drain → unmap → revoke → recycle → finalize, in
strictly increasing rank. -/
theorem protocol_is_strictly_ordered :
    rank draining < rank drainCaches ∧ rank drainCaches < rank unmap ∧
      rank unmap < rank revoke ∧ rank revoke < rank recycle ∧
      rank recycle < rank finalize := by decide

end RevPhase

end TopoMalloc

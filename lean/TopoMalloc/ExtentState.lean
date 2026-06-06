-- SPDX-License-Identifier: MIT
/-
The extent physical-backing state machine (SPEC §20.1 / §18.2, plan 04 W4-2d).

`ExtentState` is the five-state lifecycle of an extent's physical backing, and
`canTransition` is the legal one-step relation the back-end's physical-state
operations produce (allocate / free / recommit / purge / decommit-release). It is the
single source of truth the Rust `ExtentState::can_transition` is pinned to — by the
`extent_state_transition_matches_lean` unit test and the `extentStateGate` in
`Check.lean` — so the runtime machine and this model cannot drift. This is the §20.1
analogue of the §36.6 `BackingState` differential (`providerChainGate`).

`split`/`merge` (§18.4) are *structural* geometry operations that recompute a result
extent's state from the combined backing; they are not lifecycle transitions, so they
are deliberately outside this relation (just as `BackingState` does not model frame
merging).
-/

namespace TopoMalloc

/-- The §20.1 extent physical-backing states. -/
inductive ExtentState where
  /-- Virtual range reserved, no physical backing yet (`committed_len == 0`). -/
  | reserved
  /-- Allocated and handed out; a live object may exist (fully backed). -/
  | active
  /-- Free, physically backed, may hold old data — reuse is cheap (§20.1 dirty). -/
  | dirty
  /-- Free, lazily purged or scrubbed (§20.1 muzzy); still mapped. -/
  | muzzy
  /-- Free, backing returned to the OS; reuse requires a recommit (M-005). -/
  | released
  deriving Repr, DecidableEq, BEq

namespace ExtentState

/-- Whether the extent is free (holds no live object) — anything but `active`. This is
the M-004 precondition: only a free extent may be decommitted/released. Mirrors the
Rust `ExtentState::is_free`. -/
def isFree : ExtentState → Bool
  | active => false
  | _ => true

/-- Whether the extent is fully physically backed (`committed_len == len`). Mirrors the
Rust `ExtentState::is_backed`. -/
def isBacked : ExtentState → Bool
  | active => true
  | dirty => true
  | muzzy => true
  | _ => false

/-- The canonical legal §20.1 transitions (non-reflexive directed edges) the
physical-state operations produce — the single source of truth the Rust
`ExtentState::can_transition` is pinned to:

* **allocate** (carve): any free → `active`;
* **free**: `active` → `dirty` | `released`;
* **recommit** (M-005): `reserved` | `released` → `dirty`;
* **purge**: `dirty` → `muzzy`;
* **decommit/release**: `reserved` | `dirty` | `muzzy` → `released`. -/
def extentEdges : List (ExtentState × ExtentState) :=
  [ (reserved, active), (dirty, active), (muzzy, active), (released, active),
    (active, dirty), (active, released),
    (reserved, dirty), (released, dirty),
    (dirty, muzzy),
    (reserved, released), (dirty, released), (muzzy, released) ]

/-- `(s, t)` is a canonical directed edge (BEq-based, no `Prod` instances needed). -/
def hasEdge (s t : ExtentState) : Bool :=
  extentEdges.any (fun e => e.1 == s && e.2 == t)

/-- The legal one-step §20.1 transition relation: reflexivity (an idempotent op — e.g.
re-purging a muzzy extent — or a state-preserving structural rewrite) together with the
canonical directed edges. Total and decidable; the exact mirror of the Rust
`ExtentState::can_transition`. -/
def canTransition : ExtentState → ExtentState → Bool
  | reserved, reserved => true
  | active, active => true
  | dirty, dirty => true
  | muzzy, muzzy => true
  | released, released => true
  -- allocate (carve): any free → active
  | reserved, active => true
  | dirty, active => true
  | muzzy, active => true
  | released, active => true
  -- free: active → dirty | released
  | active, dirty => true
  | active, released => true
  -- recommit (M-005): reserved | released → dirty
  | reserved, dirty => true
  | released, dirty => true
  -- purge: dirty → muzzy
  | dirty, muzzy => true
  -- decommit/release: reserved | dirty | muzzy → released
  | reserved, released => true
  | dirty, released => true
  | muzzy, released => true
  | _, _ => false

/-- The five states, for exhaustive gating. -/
def all : List ExtentState := [reserved, active, dirty, muzzy, released]

/-- **M-004 evidence.** Freeness is exactly "not active": the runtime predicate that
gates decommit/release. -/
theorem isFree_iff_ne_active : ∀ s, isFree s = true ↔ s ≠ active := by
  intro s; cases s <;> decide

/-- **§20.1.** Nothing transitions *to* `reserved` except reflexively: `reserved` is the
initial, never-backed state — once an extent is touched it never returns. -/
theorem nothing_returns_to_reserved :
    ∀ s, canTransition s reserved = true → s = reserved := by
  intro s; cases s <;> decide

/-- **M-004.** A live (`active`) extent may not step straight to `muzzy`: it must be
*freed* before it can be purged (no purge of a range that may hold a live object). -/
theorem active_cannot_be_purged_directly : canTransition active muzzy = false := by
  decide

end ExtentState

end TopoMalloc

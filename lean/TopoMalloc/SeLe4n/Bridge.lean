-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The seLe4n bridge core (SPEC §36.3.3/§36.7, plan 02 W1-11a/c).

This module relates TopoMalloc's abstract `State` to an abstract seLe4n
`SystemState` (capability-backed arenas §36.4 + backing frames §36.6) and defines:

* the abstraction relation `R` (§36.3.3/§36.7): every span's arena is a known
  authority and every backing descends from authorized untyped (W1-11a);
* the combined state `TopoSeLe4n` and the single-label predicate
  `TopoSeLe4nWellFormed := WellFormed ∧ seLe4n invariants ∧ label partition` (W1-11c).

The bridge imports TopoMalloc's model and seLe4n's *public* shapes only, never
private kernel internals, so the MIT core still builds without seLe4n.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.WellFormed
import TopoMalloc.SeLe4n.CapBackedArena
import TopoMalloc.SeLe4n.UntypedProvider
import TopoMalloc.SeLe4n.VSpaceProvider
import TopoMalloc.SeLe4n.CSpaceProvider

namespace TopoMalloc.SeLe4n

open TopoMalloc State

/-- A backing frame's provenance and lifecycle position (§36.6). `origin` is the untyped
capability it was retyped from; `arena` is the authority domain it backs. -/
structure Backing where
  origin : Nat
  state : BackingState
  label : Label
  arena : ArenaId
  deriving Repr, DecidableEq

/-- An abstract seLe4n system state: the capability-backed arenas (§36.4), the backing
frames (§36.6), and the set of authorized untyped capabilities the frames must descend
from (§36.5 provenance roots). -/
structure SystemState where
  arenas : List ArenaAuth
  backings : List Backing
  authorizedUntypeds : List Nat
  deriving Repr

/-- The authority for arena `a`, if the system knows it. -/
def SystemState.arenaAuthOf (sys : SystemState) (a : ArenaId) : Option ArenaAuth :=
  sys.arenas.find? (fun au => decide (au.arena = a))

/-- The combined TopoMalloc + seLe4n state the bridge reasons about. -/
structure TopoSeLe4n where
  topo : State
  sys : SystemState

/-- Read a block's arena through its span. -/
def TopoSeLe4n.blockArena (st : TopoSeLe4n) (blk : Block) : Option ArenaId :=
  (st.topo.spanById blk.span).map (·.arena)

/-- A block's information-flow label: its arena's label (§36.12). Depends only on the
block's span and the system arenas — never on the block's owner — so it is invariant
under any owner relabel. -/
def TopoSeLe4n.blockLabel (st : TopoSeLe4n) (blk : Block) : Option Label :=
  (st.blockArena blk).bind (fun a => (st.sys.arenaAuthOf a).map (·.label))

/-- **The abstraction relation `R` (§36.3.3/§36.7, W1-11a).** Every span's arena is a
known authority, and every backing frame both (a) has an `origin` that is a genuinely
*authorized* untyped capability and (b) sits at a §36.6-reachable lifecycle state. Clause
(a) is a real provenance constraint — not every state is admissible. -/
def R (st : TopoSeLe4n) : Prop :=
  (∀ d ∈ st.topo.spans, (st.sys.arenaAuthOf d.arena).isSome) ∧
    (∀ bk ∈ st.sys.backings, bk.origin ∈ st.sys.authorizedUntypeds ∧
      BackingState.Reaches BackingState.authorizedUntyped bk.state)

/-- The seLe4n invariant bundle (§36.4/§36.6): quota accounting is sound and arena
authorities are unique. -/
structure SysInvariants (sys : SystemState) : Prop where
  quota : ∀ au ∈ sys.arenas, au.quotaOk
  arenasNodup : (sys.arenas.map (·.arena)).Nodup

/-- **Label partition (§36.12).** Caches and free lists never mix labels: any two
blocks sharing one *non-live* owner (a cache, transfer cache, central list, or backend
pool) carry the same label. Live objects are individually owned and excluded — they
may carry any label. -/
def LabelPartition (st : TopoSeLe4n) : Prop :=
  ∀ blk1 ∈ st.topo.blocks, ∀ blk2 ∈ st.topo.blocks,
    blk1.owner = blk2.owner → blk1.owner ≠ Owner.live → st.blockLabel blk1 = st.blockLabel blk2

/-- **`TopoSeLe4nWellFormed` (§36.3.3, W1-11c).** The single composite predicate:
TopoMalloc well-formedness, the seLe4n invariant bundle, the abstraction relation,
and the label partition. Total (defined on every combined state); each clause is
cross-referenced to its SPEC bullet. -/
structure TopoSeLe4nWellFormed (st : TopoSeLe4n) : Prop where
  topoWf : WellFormed st.topo
  sysInv : SysInvariants st.sys
  rel : R st
  labels : LabelPartition st

/-- Lift a TopoMalloc transition to the combined state, leaving the seLe4n system
unchanged (the topo fast/slow paths do not touch capability state). -/
def TopoSeLe4n.withTopo (st : TopoSeLe4n) (s' : State) : TopoSeLe4n := { st with topo := s' }

@[simp] theorem withTopo_sys (st : TopoSeLe4n) (s' : State) : (st.withTopo s').sys = st.sys := rfl
@[simp] theorem withTopo_topo (st : TopoSeLe4n) (s' : State) : (st.withTopo s').topo = s' := rfl

end TopoMalloc.SeLe4n

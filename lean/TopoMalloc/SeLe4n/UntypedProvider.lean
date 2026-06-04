-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The backing-provider state machine (SPEC §36.6, plan 02 W1-11b).

The provider's lifecycle is the exact eleven-state chain of §36.6, from
`AuthorizedUntyped` to `RecyclableUntyped`. `backingNext` *is* that chain (W1-11b:
"transitions match §36.6 exactly"); `BackingStep` is its one-step relation and
`Reaches` its reflexive-transitive closure. The provenance theorems
(`every_state_reaches_recyclable`, `reaches_authorized`) make precise that every
mapped frame descends from authorized untyped memory and ends at recyclable untyped
— the spine `backing_descends_from_untyped` and `no_live_object_released` build on.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Types

namespace TopoMalloc.SeLe4n

/-- The §36.6 backing lifecycle states, in order. -/
inductive BackingState where
  | authorizedUntyped
  | reservedUntyped
  | frameCapMinted
  | mappedToServer
  | mappedToClient
  | allocatorCommitted
  | allocatorDirty
  | allocatorMuzzyOrScrubbed
  | unmapped
  | revoked
  | recyclableUntyped
  deriving Repr, DecidableEq

namespace BackingState

/-- The exact §36.6 successor of each state (the last state has none). -/
def next : BackingState → Option BackingState
  | authorizedUntyped => some reservedUntyped
  | reservedUntyped => some frameCapMinted
  | frameCapMinted => some mappedToServer
  | mappedToServer => some mappedToClient
  | mappedToClient => some allocatorCommitted
  | allocatorCommitted => some allocatorDirty
  | allocatorDirty => some allocatorMuzzyOrScrubbed
  | allocatorMuzzyOrScrubbed => some unmapped
  | unmapped => some revoked
  | revoked => some recyclableUntyped
  | recyclableUntyped => none

/-- One §36.6 provider transition. -/
def Step (s s' : BackingState) : Prop := next s = some s'

instance (s s' : BackingState) : Decidable (Step s s') := by unfold Step; infer_instance

/-- Reflexive-transitive reachability along the §36.6 chain. -/
inductive Reaches : BackingState → BackingState → Prop where
  | refl (s : BackingState) : Reaches s s
  | step {s s' s'' : BackingState} : Step s s' → Reaches s' s'' → Reaches s s''

theorem Reaches.trans {a b c : BackingState} (h1 : Reaches a b) (h2 : Reaches b c) :
    Reaches a c := by
  induction h1 with
  | refl => exact h2
  | step hs _ ih => exact Reaches.step hs (ih h2)

/-- A `live`/committed frame is mapped to the client and has passed through the
server: the states a slot can be backed by while holding a live object. -/
def isCommitted : BackingState → Bool
  | allocatorCommitted => true
  | allocatorDirty => true
  | _ => false

/-- **§36.6 provenance.** Every backing state is reachable from `AuthorizedUntyped`:
every frame descends from authorized untyped memory. -/
theorem reaches_authorized : ∀ s, Reaches authorizedUntyped s
  | authorizedUntyped => .refl _
  | reservedUntyped => .step rfl (.refl _)
  | frameCapMinted => .step rfl (.step rfl (.refl _))
  | mappedToServer => .step rfl (.step rfl (.step rfl (.refl _)))
  | mappedToClient => .step rfl (.step rfl (.step rfl (.step rfl (.refl _))))
  | allocatorCommitted => .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _)))))
  | allocatorDirty =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _))))))
  | allocatorMuzzyOrScrubbed =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _)))))))
  | unmapped =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.refl _))))))))
  | revoked =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.step rfl (.refl _)))))))))
  | recyclableUntyped =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.step rfl (.step rfl (.refl _))))))))))

/-- **§36.6 termination.** Every backing state can reach `RecyclableUntyped`: the
recycle path is always available, so released backing always returns to the pool. -/
theorem reaches_recyclable : ∀ s, Reaches s recyclableUntyped
  | recyclableUntyped => .refl _
  | revoked => .step rfl (.refl _)
  | unmapped => .step rfl (.step rfl (.refl _))
  | allocatorMuzzyOrScrubbed => .step rfl (.step rfl (.step rfl (.refl _)))
  | allocatorDirty => .step rfl (.step rfl (.step rfl (.step rfl (.refl _))))
  | allocatorCommitted => .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _)))))
  | mappedToClient =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _))))))
  | mappedToServer =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.refl _)))))))
  | frameCapMinted =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.refl _))))))))
  | reservedUntyped =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.step rfl (.refl _)))))))))
  | authorizedUntyped =>
      .step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl (.step rfl
        (.step rfl (.step rfl (.step rfl (.refl _))))))))))

end BackingState

end TopoMalloc.SeLe4n

-- SPDX-License-Identifier: GPL-3.0-or-later
/-
Capability-backed arenas (SPEC §36.4, plan 02 W1-11/W1-12a).

In the seLe4n profile an arena is also a capability-controlled resource domain:
an authority (a set of rights), an information-flow `label`, and a `quota` with a
running `used` count. This module models the authority lattice and the §36.4
attenuation invariants (authority/quota/label monotonicity under delegation) that
`arena_cap_authorizes_alloc`, `arena_quota_preserved`, and `label_partition_preserved`
rest on.

GPL-3.0-or-later (D5): this models the GPLv3 seLe4n system and is kept separate from
the MIT core. It imports only TopoMalloc's public types — never seLe4n internals — so
the core still builds without seLe4n.
-/
import TopoMalloc.Types

namespace TopoMalloc.SeLe4n

open TopoMalloc

/-- The rights an arena capability grants (§36.4): allocate, free, observe stats,
destroy. Capabilities are attenuable — a delegate may carry a subset. -/
structure CapRights where
  alloc : Bool
  free : Bool
  stats : Bool
  destroy : Bool
  deriving Repr, DecidableEq

/-- `a ≤ b`: every right `a` grants, `b` grants too (a is an attenuation of b). -/
def CapRights.le (a b : CapRights) : Prop :=
  (a.alloc = true → b.alloc = true) ∧ (a.free = true → b.free = true) ∧
    (a.stats = true → b.stats = true) ∧ (a.destroy = true → b.destroy = true)

instance (a b : CapRights) : Decidable (CapRights.le a b) := by unfold CapRights.le; infer_instance

theorem CapRights.le_refl (a : CapRights) : a.le a := ⟨id, id, id, id⟩

theorem CapRights.le_trans {a b c : CapRights} (h1 : a.le b) (h2 : b.le c) : a.le c :=
  ⟨fun h => h2.1 (h1.1 h), fun h => h2.2.1 (h1.2.1 h),
   fun h => h2.2.2.1 (h1.2.2.1 h), fun h => h2.2.2.2 (h1.2.2.2 h)⟩

/-- A capability-backed arena (§36.4): identity, label, quota accounting, and the
authority it confers, plus its delegating parent (if any). -/
structure ArenaAuth where
  arena : ArenaId
  label : Label
  quota : Nat
  used : Nat
  rights : CapRights
  parent : Option ArenaId
  deriving Repr, DecidableEq

/-- Quota accounting is sound for one arena: bytes in use never exceed quota. -/
def ArenaAuth.quotaOk (au : ArenaAuth) : Prop := au.used ≤ au.quota

instance (au : ArenaAuth) : Decidable (au.quotaOk) := by unfold ArenaAuth.quotaOk; infer_instance

/-- **§36.4 delegation invariants.** A child arena capability is a sound attenuation
of its parent: no new rights (authority monotonicity), no larger quota (quota
monotonicity), and the same label (label monotonicity — no silent downgrade). -/
structure DelegatesFrom (child parent : ArenaAuth) : Prop where
  authority : child.rights.le parent.rights
  quota : child.quota ≤ parent.quota
  label : child.label = parent.label

/-- Delegation composes: a grand-child is a sound attenuation of the grand-parent
(the authority/quota/label chain is transitive). -/
theorem DelegatesFrom.trans {a b c : ArenaAuth}
    (h1 : DelegatesFrom a b) (h2 : DelegatesFrom b c) : DelegatesFrom a c where
  authority := CapRights.le_trans h1.authority h2.authority
  quota := Nat.le_trans h1.quota h2.quota
  label := h1.label.trans h2.label

end TopoMalloc.SeLe4n

-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The resource-server authority/quota family (SPEC §36.17, plan 02 W1-12a).

* `arena_cap_authorizes_alloc`
* `arena_quota_preserved` (alloc and free sides)

The server admits an allocation only when the arena cap grants alloc authority *and*
the request fits the remaining quota (§36.4). Both obligations fall out of that guard:
a successful allocation implies the authority, and accounting it preserves
`used ≤ quota`; freeing only lowers `used`, so it too preserves the bound.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.SeLe4n.Bridge

namespace TopoMalloc.SeLe4n

open TopoMalloc

/-- An arena cap authorizes `size` bytes iff it grants alloc rights and the request
fits the remaining quota (§36.4). -/
def ArenaAuth.canAlloc (au : ArenaAuth) (size : Nat) : Bool :=
  au.rights.alloc && decide (au.used + size ≤ au.quota)

/-- Account an allocation against the arena's quota. -/
def ArenaAuth.alloc (au : ArenaAuth) (size : Nat) : ArenaAuth := { au with used := au.used + size }

/-- Release `size` bytes back to the arena's quota. -/
def ArenaAuth.free (au : ArenaAuth) (size : Nat) : ArenaAuth := { au with used := au.used - size }

/-- **`arena_cap_authorizes_alloc` (§36.17).** A successful (authorized) allocation
implies the caller's arena cap granted alloc authority. -/
theorem arena_cap_authorizes_alloc (au : ArenaAuth) (size : Nat) (h : au.canAlloc size = true) :
    au.rights.alloc = true := by
  unfold ArenaAuth.canAlloc at h
  exact (Bool.and_eq_true _ _ |>.mp h).1

/-- **`arena_quota_preserved` (§36.17), allocation side.** An authorized allocation
keeps quota accounting sound. -/
theorem arena_quota_preserved_alloc (au : ArenaAuth) (size : Nat) (h : au.canAlloc size = true) :
    (au.alloc size).quotaOk := by
  unfold ArenaAuth.canAlloc at h
  have h2 := (Bool.and_eq_true _ _ |>.mp h).2
  simp only [decide_eq_true_eq] at h2
  unfold ArenaAuth.alloc ArenaAuth.quotaOk
  exact h2

/-- **`arena_quota_preserved` (§36.17), free side.** Freeing keeps quota sound — `used`
only decreases. -/
theorem arena_quota_preserved_free (au : ArenaAuth) (size : Nat) (h : au.quotaOk) :
    (au.free size).quotaOk := by
  unfold ArenaAuth.quotaOk at h
  show au.used - size ≤ au.quota
  omega

end TopoMalloc.SeLe4n

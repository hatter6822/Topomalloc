-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The CSpace slot-allocation and derivation contract (SPEC §36.8.2, plan 02 W1-11).

CSpace slots (CSlots) are scarce allocator resources. The server reserves slots
before allocating frames (§36.5: "a frame without a place to store its capability
cannot be safely delegated"). This module models a CSlot pool with a reservation
discipline and proves the two obligations the resource server relies on:
distinct allocations occupy distinct slots, and reservation never exceeds capacity.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Types

namespace TopoMalloc.SeLe4n

/-- A CSpace pool: `capacity` slots, `used` of them reserved. -/
structure CSpacePool where
  capacity : Nat
  used : Nat
  deriving Repr, DecidableEq

/-- A pool is sound when no more slots are reserved than exist. -/
def CSpacePool.Ok (p : CSpacePool) : Prop := p.used ≤ p.capacity

instance (p : CSpacePool) : Decidable p.Ok := by unfold CSpacePool.Ok; infer_instance

/-- Reserve one CSlot, if any remains (§36.5: reserve slots before frames). -/
def CSpacePool.reserve (p : CSpacePool) : Option CSpacePool :=
  if p.used < p.capacity then some ⟨p.capacity, p.used + 1⟩ else none

/-- A successful reservation keeps the pool sound (never over-commits CSlots). -/
theorem CSpacePool.reserve_ok {p p' : CSpacePool} (h : p.reserve = some p') : p'.Ok := by
  unfold CSpacePool.reserve at h
  by_cases hlt : p.used < p.capacity
  · rw [if_pos hlt] at h; injection h with h; subst h
    show p.used + 1 ≤ p.capacity; omega
  · rw [if_neg hlt] at h; exact absurd h (by simp)

/-- Reservation strictly consumes a slot, so two successive reservations occupy
*distinct* slot indices (`used` is injective along the reservation chain). -/
theorem CSpacePool.reserve_strict {p p' : CSpacePool} (h : p.reserve = some p') :
    p.used < p'.used := by
  unfold CSpacePool.reserve at h
  by_cases hlt : p.used < p.capacity
  · rw [if_pos hlt] at h; injection h with h; subst h
    show p.used < p.used + 1; omega
  · rw [if_neg hlt] at h; exact absurd h (by simp)

/-- Exhaustion is detected, never silently overrun: a full pool refuses to reserve. -/
theorem CSpacePool.reserve_none_of_full {p : CSpacePool} (h : p.capacity ≤ p.used) :
    p.reserve = none := by
  unfold CSpacePool.reserve; rw [if_neg (by omega)]

end TopoMalloc.SeLe4n

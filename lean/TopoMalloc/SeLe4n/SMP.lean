-- SPDX-License-Identifier: GPL-3.0-or-later
/-
SMP / per-core bridge extensions (SPEC §36.17 SMP forms, plan 02 W1-14).

Per V-004 and §36.17, the SMP/per-core *extensions* of the bridge theorem families
MAY be staged after seLe4n's multicore resource invariants stabilize; staging applies
**only** to the SMP extensions, never to the bridge package or the single-core
checklist (both required and complete in the sibling modules).

This module makes the staging *explicit and machine-checked where it can be*: it models
the per-core cache vector and proves the one SMP property that decomposes cleanly to the
single-core results — **core isolation**: an event on one core never changes another
core's per-core view. The genuinely-multicore obligations (migration safety, cross-core
conservation under concurrent RSEQ) are recorded below as tracked V-004 refinement debt
to be discharged by M9, not asserted as axioms.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.SeLe4n.Bridge

namespace TopoMalloc.SeLe4n

open TopoMalloc

/-- A per-core cache vector: each CPU's per-core state, indexed by `CpuId`. -/
structure PerCore (α : Type) where
  view : CpuId → α

/-- Update only core `cpu`'s entry. -/
def PerCore.set {α} (pc : PerCore α) (cpu : CpuId) (a : α) : PerCore α :=
  ⟨fun c => if c = cpu then a else pc.view c⟩

/-- **Core isolation (SMP, single-core-decomposable).** An update to core `cpu` leaves
every *other* core's per-core view unchanged. This is the multicore frame condition that
the staged migration/conservation proofs build on. -/
theorem percore_set_other {α} (pc : PerCore α) (cpu c : CpuId) (a : α) (h : c ≠ cpu) :
    (pc.set cpu a).view c = pc.view c := by
  simp [PerCore.set, h]

/-- Updating core `cpu` sets exactly that core's view. -/
theorem percore_set_self {α} (pc : PerCore α) (cpu : CpuId) (a : α) :
    (pc.set cpu a).view cpu = a := by
  simp [PerCore.set]

/-!
### Tracked V-004 refinement debt (to be discharged by M9, §36.17)

The following SMP/per-core *extensions* of the single-core theorem families are staged.
They are recorded as engineering debt — **not** as axioms — and gate at M9:

* `smp_migration_no_lost_object` — a thread migrated mid-fast-path loses no object
  (the SMP extension of `per_core_cache_abort_no_change`).
* `smp_cache_conservation` — concurrent per-core refill/flush across cores conserves
  ownership globally (the SMP extension of `cache_*_preserves_ownership_conservation`).
* `smp_low_equivalence` — non-interference holds under concurrent multicore steps
  (the SMP extension of `topo_step_preserves_low_equivalence`).

The single-core forms of every family are proved in the sibling modules and are the
current required floor.
-/

end TopoMalloc.SeLe4n

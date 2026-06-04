-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The client-runtime family (SPEC §36.17, plan 02 W1-12d).

* `client_cache_refines_server_authority`
* `per_core_cache_abort_no_change`

The client's per-CPU cache is a *bounded refinement* of server-granted authority, not
an independent authority: its occupancy is bounded by the size-class capacity
(`WellFormed` clause 11), which the server grants. And an aborted per-CPU fast path
leaves the whole combined state unchanged — the seLe4n lift of the W1-7 abort
contract.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.SeLe4n.Bridge
import TopoMalloc.Rseq

namespace TopoMalloc.SeLe4n

open TopoMalloc State

/-- **`client_cache_refines_server_authority` (§36.17).** A client's per-CPU cache for
a class never holds more objects than the server granted: its occupancy is bounded by
the class capacity (clause 11), which is the server's grant. -/
theorem client_cache_refines_server_authority (st : TopoSeLe4n) (hwf : WellFormed st.topo)
    (cpu : CpuId) (sc : SizeClassId) (granted : Nat) (hgrant : maxLocalCapacity sc ≤ granted) :
    st.topo.countOwned (Owner.cpuCache cpu sc) ≤ granted :=
  Nat.le_trans (hwf.cacheCapacities cpu sc) hgrant

/-- **`per_core_cache_abort_no_change` (§36.17), single-core.** An aborted per-CPU fast
path leaves the combined seLe4n state unchanged (state and capability system both). -/
theorem per_core_cache_abort_no_change (st : TopoSeLe4n) (cpu : CpuId) (sc : SizeClassId)
    (h : rseqPop st.topo cpu sc = RseqPop.abort) :
    st.withTopo (rseqPopStep st.topo cpu sc) = st := by
  rw [rseqPopStep_abort_no_change st.topo cpu sc h]; rfl

end TopoMalloc.SeLe4n

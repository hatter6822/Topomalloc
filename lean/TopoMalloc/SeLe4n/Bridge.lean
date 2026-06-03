-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The seLe4n bridge package (SPEC §36.3.3, plan 02 W1-2).

Intentionally **empty** at M0 (W0-14c): it exists so the bridge co-develops from
day one and so the Lean toolchain is proven to handle it. The bridge models the
GPLv3 seLe4n system, so it is GPL-3.0-or-later (D5), kept separate from the MIT
core. It MUST build without seLe4n (no seLe4n imports here). The real relation
between `TopoState` and the seLe4n `SystemState`, the capability-backed arena
model, and the refinement theorems arrive with plans 02/09.
-/
namespace TopoMalloc.SeLe4n

-- (empty at M0)

end TopoMalloc.SeLe4n

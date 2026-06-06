-- SPDX-License-Identifier: MIT
/-
Root module of the TopoMalloc formal model (plan 02). Importing it brings in the
whole model:

* the core types (§33.2) and the size-class model + §9.4/§9.5 proofs (W1-4);
* the §20.1 extent physical-backing state machine (`ExtentState`, plan 04 W4-2d);
* the abstract `State`, the fourteen-clause `WellFormed` predicate (§33.3, W1-3),
  the total transitions (§33.4 subjects, W1-5), and the RSEQ contract (§33.5, W1-7);
* the §33.4 theorem families (W1-6/W1-8/W1-9) and the named coverage theorem (W1-4e);
* the executable model + trace replay (§33.7, W1-10);
* the generated, single-source-of-truth size-class table (DD-1);
* the seLe4n bridge package (§36.3.3, W1-11/W1-12/W1-13 single-core and W1-14 SMP),
  which builds without seLe4n; and
* the trust-boundary scaffolding (W1-14/OQ7): sorry-free Lean interfaces for the
  hardware/RSEQ, host/Rust, and progress refinements that are discharged elsewhere.
-/
import TopoMalloc.Types
import TopoMalloc.ExtentState
import TopoMalloc.SizeClass
import TopoMalloc.Generated.SizeClasses
import TopoMalloc.State
import TopoMalloc.WellFormed
import TopoMalloc.Transitions
import TopoMalloc.Rseq
import TopoMalloc.Theorems
import TopoMalloc.Exec
import TopoMalloc.Boundaries
import TopoMalloc.SeLe4n

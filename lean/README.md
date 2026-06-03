<!-- SPDX-License-Identifier: MIT -->
# `lean/` — the formal model (plan 02)

The Lean 4 abstract state machine and its proofs. Lean defines the allocator's
states, the well-formedness predicate, and the theorems (§33); it is **not** on
the production hot path. It is built with `lake` (pinned by `../lean-toolchain`)
and driven by `cargo xtask lean` / `cargo xtask ci`.

| Module | Charter |
|--------|---------|
| `TopoMalloc/Types.lean` | core types: `Range`, `Owner`, ids (§33.2) |
| `TopoMalloc/SizeClass.lean` | the `SizeClassRow` type + the §9.3/§9.5 well-formedness predicate |
| `TopoMalloc/Generated/SizeClasses.lean` | **generated** table (DO NOT EDIT) — the single source of truth, imported here (DD-1) |
| `TopoMalloc/SeLe4n/Bridge.lean` | the (empty at M0) seLe4n bridge — **GPL-3.0-or-later** (D5) |
| `Check.lean` | the `lake exe check` executable: replays the generated table through the predicate (G-table, Lean side) |

The State/WellFormed/Transitions/Theorems modules and the full bridge package
(§36.3.3) are built out in plan 02. The bridge is GPL-3.0-or-later and must build
without seLe4n.

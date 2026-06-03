-- SPDX-License-Identifier: MIT
/-
Core Lean types for the TopoMalloc abstract model (SPEC §33.2, plan 02 W1-1).
These mirror the runtime newtypes in `crates/topo-core/src/ids.rs`, so the proof
and the implementation name the same things.
-/
namespace TopoMalloc

abbrev Addr := Nat
abbrev Bytes := Nat
abbrev CpuId := Nat
abbrev ArenaId := Nat
abbrev SizeClassId := Nat
abbrev BlockId := Nat
abbrev SpanId := Nat
abbrev HugePageId := Nat

/-- A half-open byte range `[base, base + len)`. -/
structure Range where
  base : Addr
  len : Bytes
  deriving Repr, DecidableEq

/-- Every block has exactly one owner (SPEC §33.2). This enumeration includes
all SPEC owners, including `quarantine` and `released` (W1-1 acceptance). -/
inductive Owner where
  | live
  | cpuCache (cpu : CpuId) (sc : SizeClassId)
  | threadCache (tid : Nat) (sc : SizeClassId)
  | transferCache (domain : Nat) (sc : SizeClassId)
  | centralFree (arena : ArenaId) (sc : SizeClassId)
  | backendFree (arena : ArenaId)
  | quarantine (arena : ArenaId)
  | released (arena : ArenaId)
  deriving Repr, DecidableEq

end TopoMalloc

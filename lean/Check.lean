-- SPDX-License-Identifier: MIT
/-
`lake exe check` (CI job `lean`, W0-5c / W0-14d). Two machine-checked gates:

1. **G-table (Lean side, DD-1):** replays the *generated* size-class table (the single
   source of truth) through the Lean well-formedness predicate. If the generated table
   ever diverges from a sound table, this exits non-zero and the `lean` gate fails.
2. **Trace oracle (W1-10, §33.7):** replays a recorded trace through the executable
   model and confirms an injected violation is flagged — a runtime witness that the
   §33.7 replay oracle (`TopoMalloc.Exec`) is wired and rejects ill-formed traces.

The proofs themselves (§33.4 theorem set, §9.4/§9.5 size-class lemmas, the §36.17
single-core bridge families) are checked by `lake build` proof-checking every module;
this executable adds the two *evaluated* gates on top.
-/
import TopoMalloc

open TopoMalloc
open TopoMalloc.Generated

/-- The generated table passes the Lean §9.3/§9.5 predicate. -/
def tableGate : Bool := tableOk pageSize quantum smallMax sizeClasses

/-- The executable model (W1-10) replays a good trace cleanly and flags the injected
violation in the bad trace at the expected line. -/
def oracleGate : Bool :=
  match replay sampleGoodTrace, replay sampleBadTrace with
  | .ok ⟨[]⟩, .error (3, .freeOfUnknown 0x1000) => true
  | _, _ => false

def main : IO UInt32 := do
  let mut ok := true
  if tableGate then
    IO.println s!"lake check: size-class table OK ({sizeClasses.length} classes, small_max={smallMax})"
  else
    IO.eprintln "lake check: size-class table FAILED well-formedness (§9.3/§9.5)"
    ok := false
  if oracleGate then
    IO.println s!"lake check: trace oracle OK (good trace replays; injected violation flagged, §33.7)"
  else
    IO.eprintln "lake check: trace oracle FAILED (W1-10 replay)"
    ok := false
  return if ok then 0 else 1

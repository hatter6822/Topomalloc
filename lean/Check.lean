-- SPDX-License-Identifier: MIT
/-
`lake exe check` (CI job `lean`, W0-5c / W0-14d). Replays the *generated*
size-class table through the Lean well-formedness predicate. If the generated
table (the single source of truth, DD-1) ever diverges from a sound table, this
exits non-zero and the `lean` gate fails — the Lean side of G-table.
-/
import TopoMalloc

open TopoMalloc
open TopoMalloc.Generated

def main : IO UInt32 := do
  if tableOk pageSize quantum smallMax sizeClasses then
    IO.println s!"lake check: size-class table OK ({sizeClasses.length} classes, small_max={smallMax})"
    return 0
  else
    IO.eprintln "lake check: size-class table FAILED well-formedness (§9.3/§9.5)"
    return 1

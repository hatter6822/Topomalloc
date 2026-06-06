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
import TopoMalloc.SeLe4n.UntypedProvider

open TopoMalloc
open TopoMalloc.Generated
open TopoMalloc.SeLe4n

/-- The generated table passes the Lean §9.3/§9.5 predicate, its emitted lookup is sound
(`coversAllB`) and minimal (`minimalLookupB`) for every small request, it satisfies the §9.4
per-range spacing policy (`spacingOkB`), and the derived size-regime constants are consistent
with the rows (`maxAlignOkB` = widest alignment; `hugeThresholdOkB` = well-formed medium/large
boundary). These discharge — by evaluation on the generated tuned table — the hypotheses the
§33.4 `size_class_table_covers_all_small_requests` and `generated_table_spacing` theorems
consume (the kernel cannot `decide` the 2048-granule lookup / the spacing products). -/
def tableGate : Bool :=
  tableOk pageSize quantum smallMax sizeClasses && coversAllB && spacingOkB
    && minimalLookupB && lookupMatchesModelB && maxAlignOkB && hugeThresholdOkB

/-- The executable model (W1-10) replays a good trace cleanly and flags the injected
violation in the bad trace at the expected line — both for the structured event list and
for the **§33.7 text grammar** the Rust emitter produces (the differential-replay loop). -/
def oracleGate : Bool :=
  (match replay sampleGoodTrace, replay sampleBadTrace with
    | .ok ⟨[]⟩, .error (3, .freeOfUnknown 0x1000) => true
    | _, _ => false) &&
  (match replayText sampleText, replayText sampleTextBad with
    | .ok ⟨[]⟩, .error (3, .freeOfUnknown 0x1000) => true
    | _, _ => false)

/-- The pagemap differential (W3-3d): the recorded install/lookup trace replays
through the executable pagemap model with every `addr -> span` lookup as expected. The
Rust test `pagemap_matches_lean_replay_differential` checks the identical trace against
the runtime radix, so a divergence on either side fails CI. -/
def pagemapDiffGate : Bool := TopoMalloc.pagemapGate

/-- The full §36.6 backing-provider lifecycle, in order. The single source of truth
the Rust `ProviderState` chain is pinned to (plan 04 W4-1, A1). -/
def providerChain : List BackingState :=
  [.authorizedUntyped, .reservedUntyped, .frameCapMinted, .mappedToServer,
   .mappedToClient, .allocatorCommitted, .allocatorDirty, .allocatorMuzzyOrScrubbed,
   .unmapped, .revoked, .recyclableUntyped]

/-- The provider-state-machine differential (W4-1, A1): `BackingState.next` is exactly
the linear §36.6 chain `providerChain[i] -> providerChain[i+1]`, terminating at
`recyclableUntyped`. The Rust test `provider_next_matches_the_36_6_chain_exactly` pins
the runtime `ProviderState::can_transition` to the *same* chain, so the runtime checker
and this proof cannot drift (the analogue of the pagemap differential for §36.6). -/
def providerChainGate : Bool :=
  (providerChain.zip providerChain.tail).all (fun p => decide (BackingState.next p.1 = some p.2))
    && decide (BackingState.next BackingState.recyclableUntyped = none)

/-- The §20.1 extent-state-machine differential (W4-2d): `ExtentState.canTransition` is
exactly reflexivity ∪ the canonical `extentEdges`, over all 25 ordered pairs. The Rust
test `extent_state_transition_matches_lean` pins the runtime
`ExtentState::can_transition` to the *same* edge set, so the runtime physical-state
machine and this model cannot drift (the §20.1 analogue of `providerChainGate`). -/
def extentStateGate : Bool :=
  ExtentState.all.all (fun s => ExtentState.all.all (fun t =>
    ExtentState.canTransition s t == (s == t || ExtentState.hasEdge s t)))

def main : IO UInt32 := do
  let mut ok := true
  if tableGate then
    IO.println s!"lake check: size-class table OK ({sizeClasses.length} classes, small_max={smallMax}, huge_threshold={hugeThreshold}, max_align={maxAlign})"
  else
    IO.eprintln "lake check: size-class table FAILED well-formedness (§9.3/§9.5)"
    ok := false
  if oracleGate then
    IO.println s!"lake check: trace oracle OK (good trace replays; injected violation flagged, §33.7)"
  else
    IO.eprintln "lake check: trace oracle FAILED (W1-10 replay)"
    ok := false
  if pagemapDiffGate then
    IO.println "lake check: pagemap differential OK (install/lookup trace replays; matches Rust radix, W3-3d)"
  else
    IO.eprintln "lake check: pagemap differential FAILED (W3-3d replay)"
    ok := false
  if providerChainGate then
    IO.println "lake check: provider state machine OK (§36.6 chain matches BackingState.next; pins Rust ProviderState, W4-1)"
  else
    IO.eprintln "lake check: provider state machine FAILED (§36.6 chain drift, W4-1)"
    ok := false
  if extentStateGate then
    IO.println "lake check: extent state machine OK (§20.1 transitions match ExtentState.canTransition; pins Rust ExtentState, W4-2d)"
  else
    IO.eprintln "lake check: extent state machine FAILED (§20.1 transition drift, W4-2d)"
    ok := false
  return if ok then 0 else 1

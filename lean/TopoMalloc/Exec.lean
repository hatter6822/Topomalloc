-- SPDX-License-Identifier: MIT
/-
The executable model + trace replay (SPEC §33.7, plan 02 W1-10).

A proof-grade replay oracle: it consumes the §33.7 trace grammar as structured
`TraceEvent`s, maintains the live-pointer set, and checks the cardinal ownership
invariants at every boundary — *no two live objects at one address* (live
disjointness, §8.3) and *no free of a non-live pointer* (S-009). On a violation it
returns the 1-based line and the offending pointer, "flagging an injected violation"
(W1-10). The middle/back-end events (refill/flush/span-alloc/release) do not change
the live set, mirroring the host oracle in `topo_test_support::LiveModel`, with which
it is kept in lockstep via differential replay (plan 08).

The invariant the oracle maintains — the live set has no duplicates — is proved
preserved at every boundary (`apply_preserves_nodup`, `replay_nodup`), so a clean
replay is a machine-checked witness of live disjointness over the trace.
-/
import TopoMalloc.Types

namespace TopoMalloc

/-- One event of the §33.7 trace grammar (only the fields the live-set oracle reads
are retained; the rest are summarised). -/
inductive TraceEvent where
  /-- `ALLOC … -> ptr …` (`ptr = 0` means the allocation failed). -/
  | alloc (ptr : Nat)
  /-- `FREE ptr …` (`ptr = 0` is `free(NULL)`, a no-op). -/
  | free (ptr : Nat)
  /-- `REFILL cpu sc count source`. -/
  | refill (cpu sc count : Nat)
  /-- `FLUSH cpu sc count target`. -/
  | flush (cpu sc count : Nat)
  /-- `SPAN_ALLOC arena sc span pages hugepage`. -/
  | spanAlloc (arena sc span pages : Nat) (hugepage : Option Nat)
  /-- `RELEASE base:len state`. -/
  | release (base len : Nat)
  deriving Repr, DecidableEq

/-- A well-formedness violation detected during replay. -/
inductive ExecError where
  /-- Two live objects reported at the same address (live disjointness, §8.3). -/
  | doubleAlloc (ptr : Nat)
  /-- A free of a pointer that is not currently live (S-009). -/
  | freeOfUnknown (ptr : Nat)
  /-- A line that is not blank/comment yet does not parse in the §33.7 grammar
  (an unknown verb, or a known verb with malformed fields) — rejected, not skipped,
  to stay in differential lockstep with the host replayer. -/
  | malformedLine
  deriving Repr, DecidableEq

/-- The executable model: the set of currently-live pointers. -/
structure ExecModel where
  live : List Nat
  deriving Repr, DecidableEq

/-- The empty model (no live pointers). -/
def ExecModel.empty : ExecModel := ⟨[]⟩

/-- Apply one trace event, checking the cardinal invariants. -/
def ExecModel.apply (m : ExecModel) : TraceEvent → Except ExecError ExecModel
  | .alloc ptr =>
      if ptr = 0 then .ok m
      else if ptr ∈ m.live then .error (.doubleAlloc ptr)
      else .ok ⟨ptr :: m.live⟩
  | .free ptr =>
      if ptr = 0 then .ok m
      else if ptr ∈ m.live then .ok ⟨m.live.erase ptr⟩
      else .error (.freeOfUnknown ptr)
  | _ => .ok m

/-- The boundary invariant the oracle checks: live pointers are distinct. -/
def ExecModel.WellFormed (m : ExecModel) : Prop := m.live.Nodup

/-- A successful step preserves the boundary invariant (live disjointness). -/
theorem ExecModel.apply_preserves_nodup {m m' : ExecModel} {e : TraceEvent}
    (hwf : m.WellFormed) (h : m.apply e = .ok m') : m'.WellFormed := by
  unfold ExecModel.WellFormed at *
  cases e with
  | alloc ptr =>
    simp only [ExecModel.apply] at h
    by_cases hz : ptr = 0
    · rw [if_pos hz] at h; injection h with h; subst h; exact hwf
    · rw [if_neg hz] at h
      by_cases hm : ptr ∈ m.live
      · rw [if_pos hm] at h; exact absurd h (by simp)
      · rw [if_neg hm] at h; injection h with h; subst h
        exact List.nodup_cons.mpr ⟨hm, hwf⟩
  | free ptr =>
    simp only [ExecModel.apply] at h
    by_cases hz : ptr = 0
    · rw [if_pos hz] at h; injection h with h; subst h; exact hwf
    · rw [if_neg hz] at h
      by_cases hm : ptr ∈ m.live
      · rw [if_pos hm] at h; injection h with h; subst h; exact hwf.erase ptr
      · rw [if_neg hm] at h; exact absurd h (by simp)
  | refill _ _ _ => simp only [ExecModel.apply] at h; injection h with h; subst h; exact hwf
  | flush _ _ _ => simp only [ExecModel.apply] at h; injection h with h; subst h; exact hwf
  | spanAlloc _ _ _ _ _ => simp only [ExecModel.apply] at h; injection h with h; subst h; exact hwf
  | release _ _ => simp only [ExecModel.apply] at h; injection h with h; subst h; exact hwf

/- ----------------------------------------------------------------------- -/
/- Parsing the §33.7 *text* grammar — closing the differential loop with the  -/
/- Rust trace emitter (`topo_core::trace`) / host replayer.                   -/
/- ----------------------------------------------------------------------- -/

/-- A single hex digit's value, if valid. -/
def hexDigit? (c : Char) : Option Nat :=
  if '0' ≤ c ∧ c ≤ '9' then some (c.toNat - '0'.toNat)
  else if 'a' ≤ c ∧ c ≤ 'f' then some (c.toNat - 'a'.toNat + 10)
  else if 'A' ≤ c ∧ c ≤ 'F' then some (c.toNat - 'A'.toNat + 10)
  else none

/-- Parse an address field: `0x`-prefixed hex (as emitted) or plain decimal. -/
def parseAddr (s : String) : Option Nat :=
  if s.startsWith "0x" || s.startsWith "0X" then
    (s.toList.drop 2).foldl (fun acc c => acc.bind fun n => (hexDigit? c).map fun d => n * 16 + d)
      (some 0)
  else s.toNat?

/-- The result of parsing one trace line. -/
inductive LineParse where
  /-- A recognized event (drives the live-set oracle). -/
  | event (e : TraceEvent)
  /-- A blank line or a `#` comment — skipped, by design. -/
  | skip
  /-- A line that is neither blank/comment nor a valid §33.7 record (unknown verb or a
  known verb with malformed fields) — a parse failure to be reported, not skipped. -/
  | malformed
  deriving Repr, DecidableEq

/-- Parse one line of the §33.7 grammar, reading the live-set-relevant fields (the `ptr`
for `ALLOC`/`FREE`). Blank and `#`-comment lines are `skip`; a recognized verb with good
fields is an `event`; anything else (unknown verb, or a known verb with an unparseable
pointer) is `malformed` — so the oracle rejects exactly what the host parser rejects. -/
def parseTraceLine (line : String) : LineParse :=
  match (line.splitOn " ").filter (· ≠ "") with
  | [] => .skip
  | first :: rest =>
    if first.startsWith "#" then .skip
    else match first, rest with
      -- ALLOC request_id size align arena flags -> ptr usable_size sc span
      | "ALLOC", _ =>
        match rest.dropWhile (· ≠ "->") with
        | _ :: ptr :: _ => match parseAddr ptr with | some p => .event (.alloc p) | none => .malformed
        | _ => .malformed
      | "FREE", ptr :: _ => match parseAddr ptr with | some p => .event (.free p) | none => .malformed
      | "FREE", [] => .malformed
      | "REFILL", _ => .event (.refill 0 0 0)
      | "FLUSH", _ => .event (.flush 0 0 0)
      | "SPAN_ALLOC", _ => .event (.spanAlloc 0 0 0 0 none)
      | "RELEASE", _ => .event (.release 0 0)
      | _, _ => .malformed

/-- Replay a whole *text* trace (blank lines and `#` comments skipped), parsing each line
in the §33.7 grammar and checking the cardinal invariants — the proof-grade counterpart of
the Rust host replayer (`topo_test_support`/`tools/trace-replay`), so the two agree by
differential replay (plan 08). -/
def replayText (input : String) : Except (Nat × ExecError) ExecModel :=
  go (input.splitOn "\n") ExecModel.empty 1
where
  go : List String → ExecModel → Nat → Except (Nat × ExecError) ExecModel
  | [], m, _ => .ok m
  | line :: rest, m, n =>
    match parseTraceLine line with
    | .skip => go rest m (n + 1)           -- blank / `#` comment only
    | .malformed => .error (n, .malformedLine)  -- reject, don't skip (lockstep)
    | .event e => match m.apply e with
      | .ok m' => go rest m' (n + 1)
      | .error err => .error (n, err)

/-- Replay a whole trace, reporting the 1-based line of the first violation. -/
def replay (events : List TraceEvent) : Except (Nat × ExecError) ExecModel :=
  go events ExecModel.empty 1
where
  go : List TraceEvent → ExecModel → Nat → Except (Nat × ExecError) ExecModel
  | [], m, _ => .ok m
  | e :: rest, m, n =>
      match m.apply e with
      | .ok m' => go rest m' (n + 1)
      | .error err => .error (n, err)

/-- A successful replay witnesses live disjointness over the whole trace: the final
model — and, by `apply_preserves_nodup` at each step, every boundary — is well-formed. -/
theorem replay_nodup {events : List TraceEvent} {m : ExecModel}
    (h : replay events = .ok m) : m.WellFormed := by
  suffices H : ∀ evs m₀ n m', m₀.WellFormed → replay.go evs m₀ n = .ok m' → m'.WellFormed by
    exact H events ExecModel.empty 1 m (by unfold ExecModel.WellFormed ExecModel.empty; exact List.nodup_nil) h
  intro evs
  induction evs with
  | nil => intro m₀ n m' hwf h; simp only [replay.go] at h; cases h; exact hwf
  | cons e rest ih =>
    intro m₀ n m' hwf h
    simp only [replay.go] at h
    cases hstep : m₀.apply e with
    | error err => rw [hstep] at h; simp at h
    | ok m₁ =>
      rw [hstep] at h
      exact ih m₁ (n + 1) m' (ExecModel.apply_preserves_nodup hwf hstep) h

/- W1-10 acceptance: replay a recorded trace, and flag an injected violation. -/

/-- A well-formed recorded trace (two allocations, freed in turn, plus middle/back-end
events that do not disturb the live set). -/
def sampleGoodTrace : List TraceEvent :=
  [.alloc 0x1000, .alloc 0x2000, .refill 3 1 32, .free 0x1000, .release 0x3000 4096, .free 0x2000]

/-- The same trace with an injected double-free of `0x1000`. -/
def sampleBadTrace : List TraceEvent :=
  [.alloc 0x1000, .free 0x1000, .free 0x1000]

example : replay sampleGoodTrace = .ok ⟨[]⟩ := by rfl

/-- The injected double-free is flagged at its line with the offending pointer. -/
example : replay sampleBadTrace = .error (3, .freeOfUnknown 0x1000) := by rfl

/-- A recorded trace in the **exact §33.7 text grammar** the Rust emitter (`topo_core::trace`)
produces. `lake exe check` replays it through the Lean oracle (the differential loop with the
Rust host replayer). The text parsing is evaluated, not kernel-reduced. -/
def sampleText : String :=
  "ALLOC 0 24 16 0 0 -> 0x1000 32 1 5\n\
   ALLOC 1 24 16 0 0 -> 0x2000 32 1 5\n\
   REFILL 3 1 32 central\n\
   FREE 0x1000 32 -> 1 5\n\
   FREE 0x2000 32 -> 1 5\n"

/-- The same text grammar with an injected double-free, for the negative replay check. -/
def sampleTextBad : String :=
  "ALLOC 0 8 8 0 0 -> 0x1000 16 0 0\nFREE 0x1000 8 -> 0 0\nFREE 0x1000 8 -> 0 0\n"

end TopoMalloc

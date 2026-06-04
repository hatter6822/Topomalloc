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

end TopoMalloc

-- SPDX-License-Identifier: MIT
/-
Trust boundaries (plan 02 W1-14, open question 7) — the three refinements that cannot be
discharged *inside* this Lean/Rust artifact, formalized as precise, **sorry-free** Lean
interfaces so each obligation is explicit and auditable rather than implicit.

The pattern for each boundary is the same and adds **no new axioms**:

* an abstract model of the lower level (hardware config, host replayer, attempt stream);
* a `Prop`-level *refinement obligation* naming exactly what a future proof must establish;
* a **conditional safety theorem** — "*if* the obligation holds, *then* the desired
  property follows" — proved here, so the moment the obligation is discharged the guarantee
  is immediate. Nothing about a concrete asm / Rust binary is *assumed*; these are the
  sockets the deferred work (W1-14 per-arch verification, a host-model refinement) plugs
  into.
-/
import TopoMalloc.Exec
import TopoMalloc.Theorems.Cache

namespace TopoMalloc.Boundaries

open TopoMalloc

/- ===================================================================== -/
/- Boundary 1 — hardware ↔ the RSEQ contract (§33.5, W1-14 / OQ7).        -/
/- ===================================================================== -/

/-- An abstract per-CPU hardware configuration: the opaque machine context a restartable
sequence runs against (which CPU, and whether it was preempted/migrated mid-sequence). The
per-architecture register/memory detail is left abstract — that is the W1-14 work. -/
structure HwConfig where
  cpu : CpuId
  preempted : Bool

/-- The abstract hardware semantics of an RSEQ pop: what the real x86-64/AArch64 assembly
*computes* from a machine config and the allocator state. A `Type` of such functions, not a
fixed one — the refinement obligation quantifies over implementations. -/
def HwRseqSpec : Type := HwConfig → State → CpuId → SizeClassId → RseqPop

/-- **The hardware-refinement obligation (W1-14 / OQ7).** A concrete assembly `hw`
*refines* the model `rseqPop` at config `cfg` iff, whenever the sequence is not preempted,
the hardware and the model agree on every state. Discharging this for a real per-arch
implementation is future work; this pins exactly what that work must prove. -/
def HwRefinesRseq (hw : HwRseqSpec) (cfg : HwConfig) : Prop :=
  ∀ s cpu sc, cfg.preempted = false → hw cfg s cpu sc = rseqPop s cpu sc

/-- **Conditional safety (sorry-free).** If an assembly refines the model, then the model's
contract transfers to the hardware: a successful *hardware* pop on a non-preempted config
preserves well-formedness. No assembly is assumed to exist — this is the bridge the W1-14
proof slots into. -/
theorem hw_pop_preserves_wellformed (hw : HwRseqSpec) (cfg : HwConfig)
    (href : HwRefinesRseq hw cfg) (s : State) (cpu : CpuId) (sc : SizeClassId)
    (hwf : WellFormed s) (hnp : cfg.preempted = false)
    {p : BlockId} {s' : State} (h : hw cfg s cpu sc = RseqPop.success p s') : WellFormed s' := by
  rw [href s cpu sc hnp] at h
  exact malloc_fast_path_preserves_wellformed s cpu sc hwf h

/- ===================================================================== -/
/- Boundary 2 — the Lean model ↔ the host (Rust) implementation (§33.1).  -/
/- ===================================================================== -/

/-- The host (Rust) allocator's trace replayer, as an *abstract* function with the same
shape as the Lean oracle `replayText`. Its concrete behavior is the shipped `trace-replay`
binary; modelling that binary in Lean is future work. -/
def HostReplay : Type := String → Except (Nat × ExecError) ExecModel

/-- **The differential-lockstep obligation (§33.7).** The host replayer and the Lean oracle
reach the same accept/reject verdict on every trace. The `cargo xtask` differential test
checks finite instances of this equality; a full proof needs the host modelled in Lean. -/
def HostLockstep (host : HostReplay) : Prop :=
  ∀ trace : String, (host trace).isOk = (replayText trace).isOk

/-- **Conditional safety (sorry-free).** Under lockstep, whenever the host *accepts* a
trace the Lean oracle accepts it too — so every accepted-trace guarantee the oracle proves
(live-set disjointness / no double-free, §8.3, via `replay_disjoint`) transfers to the host. -/
theorem host_accept_imp_model_accept (host : HostReplay) (hl : HostLockstep host)
    (trace : String) (h : (host trace).isOk = true) : (replayText trace).isOk = true := by
  rw [← hl trace]; exact h

/- ===================================================================== -/
/- Boundary 3 — progress / liveness (§33.5): `abort` is a retry.          -/
/- ===================================================================== -/

/-- The bounded RSEQ retry loop (§33.5): try the hardware attempts `cfgs (k-1), …, cfgs 0`
in turn, returning the first *committed* (non-`abort`) outcome, or `abort` if every attempt
in the budget was preempted. Structural recursion on the fuel `k` — terminating and
computable, no fairness needed to *define* it. -/
def retry (hw : HwRseqSpec) (cfgs : Nat → HwConfig) (s : State) (cpu : CpuId) (sc : SizeClassId) :
    Nat → RseqPop
  | 0 => RseqPop.abort
  | k + 1 => match hw (cfgs k) s cpu sc with
             | RseqPop.abort => retry hw cfgs s cpu sc k
             | r => r

/-- **Fair progress (sorry-free).** `abort` is a genuine *retry*, not a stuck state: if
*some* attempt in range commits (the fairness the OS scheduler provides — eventually the
sequence runs without preemption), the bounded retry loop returns a committed outcome. This
is the liveness socket; discharging the fairness premise itself is W1-14. -/
theorem retry_commits (hw : HwRseqSpec) (cfgs : Nat → HwConfig) (s : State) (cpu : CpuId)
    (sc : SizeClassId) :
    ∀ k n, n < k → hw (cfgs n) s cpu sc ≠ RseqPop.abort → retry hw cfgs s cpu sc k ≠ RseqPop.abort := by
  intro k
  induction k with
  | zero => intro n hn _; exact absurd hn (Nat.not_lt_zero n)
  | succ k ih =>
    intro n hn hc
    rw [retry]
    cases hr : hw (cfgs k) s cpu sc with
    | abort =>
      rcases Nat.lt_succ_iff_lt_or_eq.mp hn with hlt | heq
      · exact ih n hlt hc
      · exact absurd hr (heq ▸ hc)
    | empty => exact fun h => RseqPop.noConfusion h
    | success p s' => exact fun h => RseqPop.noConfusion h

end TopoMalloc.Boundaries

# TopoMalloc Implementation Plan — Overview & Index

**Document status:** Master plan, revision 2.1 (split into domain plans + per-task deep dives)
**Implements:** [`../SPEC.md`](../SPEC.md) (TopoMalloc Specification, rev 0.3)
**Supersedes:** the former single-file `../WORKSTREAM_PLAN.md` (now a redirect)

This is the **front door** to the TopoMalloc implementation plan. The plan is split into one overview (this
file) plus ten focused **domain plans**, each owning a coherent set of workstreams. This file carries the
cross-cutting spine — principles, the central architectural seam, ratified decisions, the milestone ladder,
CI gates, the global risk register, the Definition of Done, and the requirement-traceability matrix. The
domain plans carry the workstreams and their decomposed work units.

---

## 0. How to use this plan set

Three nouns are used precisely throughout every document:

| Term | Meaning | ID form |
|---|---|---|
| **Workstream** | A long-lived capability track that owns an interface and a body of code/proof. | `W0`..`W23` |
| **Work Unit (WU)** | The smallest planned, independently reviewable, independently testable change (a few days at most). Complex WUs are split into **letter-suffixed sub-units**. | `W5-3`, `W5-3a` |
| **Milestone** | A temporal integration gate composing WUs from several workstreams into a releasable capability. Has CI gates and exit criteria. | `M0`..`M9` |

Each WU carries a **size** (`S`/`M`/`L`, relative) and a **parallelism** flag (`∥` = runnable concurrently
with siblings once deps are met). A **▸ Decomposition** note after a table gives the design space, pitfalls,
and sequencing for the hardest units. A bare cluster reference (e.g. "W5-3") denotes the whole sub-unit
family. Acceptance criteria are written so a reviewer can say *done* or *not done* without judgement. Each
domain plan ends with a **Deep dives** section that expands every complex task into a mini design doc
(*Problem · Design space · Structures · Work breakdown · Invariants · Verify · Failure modes · Sequencing*).

**Reading order for a newcomer:** §1 (principles) → §3 (the seam) → [01 Repository & Infrastructure](01-repository-and-infrastructure.md)
(the first work) → §5 (milestones) → the domain plan you own (§ index below).

### 0.1 Document index

| # | Domain plan | Workstreams | What it covers |
|---|---|---|---|
| — | **this file** | — | spine: principles, seam, decisions, milestones, gates, risks, DoD, traceability |
| 01 | [Repository & infrastructure](01-repository-and-infrastructure.md) | W0 | repo layout, toolchains, build, CI, governance, the M0 walking skeleton |
| 02 | [Formal model & seLe4n bridge](02-formal-model.md) | W1 | Lean model, size-class proofs, RSEQ contract, theorem sets, trace oracle, bridge |
| 03 | [Core allocator](03-core-allocator.md) | W2, W3, W5 | size classes/classify, metadata/pagemap/bootstrap, spans/slabs/central lists |
| 04 | [Backend, hugepages, release & topology](04-backend-hugepages-release.md) | W4, W11, W12, W13 | the `TopoBackingProvider` seam, POSIX backend, hugepage filler, release controller, NUMA |
| 05 | [Caches, concurrency & fast paths](05-caches-concurrency-fastpath.md) | W6, W7, W16 | thread/per-CPU caches, transfer caches, RSEQ asm, lock hierarchy, TLS, fork |
| 06 | [Public API, realloc & arenas](06-api-realloc-arenas.md) | W8, W15, W9, W10 | C/C++/Rust ABI, realloc/aligned/calloc, capability-backed arenas, extent hooks |
| 07 | [Observability, placement & control](07-observability-placement-control.md) | W17, W14, W20 | stats/JSON/profiling/explain, lifetime/hotness placement, config + control plane |
| 08 | [Security, debug & testing](08-security-debug-testing.md) | W18, W19, W21 | hardening/quarantine/guard pages, debug checks/sanitizers, property/differential/fuzz |
| 09 | [seLe4n integration](09-sele4n-integration.md) | W22 | `Sele4nSim`, resource server, client runtime, labels, real-kernel bring-up, conformance |
| 10 | [Deployment & ABI](10-deployment-and-abi.md) | W23 | interposition, mixed-allocator safety, ABI stability, packaging, perf validation |

### 0.2 Change log

| Rev | Summary |
|---|---|
| 1.0 | Initial single-file plan: 24 workstreams, 10 milestones, requirement traceability. |
| 1.1 | Integrated the real `hatter6822/seLe4n` Rust ABI; `Sele4nBackingProvider` compiles against it from M1. |
| 1.2 | Deep-decomposition pass: dependency audit + corrections; every L-unit split into letter-suffixed sub-units; engineering ▸ Decomposition notes. |
| 2.0 | Split into this overview + ten domain plans, each with domain-local interfaces, sequencing, risks, and a best-practices checklist. |
| **2.1** | **Per-task deep dives.** Every domain plan ends with a **Deep dives** section expanding each complex task into a mini design doc — *Problem · Design space (options → chosen + rationale) · Structures (interface/data-structure sketches) · Work breakdown (finer than the table) · Invariants · Verify (test + proof obligations) · Failure modes (+ guards) · Sequencing*. End-to-end optimize + refine + best-practices sweep across the whole set. |

---

## 1. Execution principles (binding on every WU)

1. **Safety before policy (SPEC §2.4).** A WU may ship a dumb-but-correct policy; it may never ship an unsafe
   fast path. Every PR keeps the unconditional safety invariants (S-001..S-010) green.
2. **Vertical slices, always runnable.** Every milestone produces a *working allocator*, not a pile of
   subsystems. M1 is a real (slow) malloc; later milestones make it faster/richer without regressing.
3. **Formal model in lockstep (SPEC §33, V-004).** A WU that adds/changes an abstract transition updates the
   Lean model in the same WU, or records a tracked `V-004` refinement debt.
4. **Single source of truth for generated artifacts (Appendix F).** Size-class tables, lookup tables, batch
   sizes, alignment masks, capacity limits are emitted by **one** generator, checked by Lean, consumed never
   hand-edited.
5. **One seam, two backends (D2).** All OS/kernel interaction goes through `TopoBackingProvider` (§3). POSIX
   and seLe4n are co-equal behind it from M1.
6. **Deterministic-replay testability (SPEC §30.4, §33.7).** Every layer can emit the SPEC trace grammar and
   replay against the Lean executable model (differential testing).
7. **Invariant checks are first-class code.** Debug/hardened builds run the Appendix B checklist as runtime
   assertions; transitions are tagged with their SPEC state-machine name (M-001).
8. **Profiles are features, not forks.** `performance`/`hardened`/`debug`/`deterministic_test`/`low_rss`/
   `hugepage_optimized` are feature-gated from M0.
9. **No new anti-patterns.** Appendix F is a CI-enforced review checklist.

---

## 2. Conformance map — what "done" means

| Conformance class (SPEC) | Primary workstreams | Plan(s) | First | Full |
|---|---|---|---|---|
| **Core** (API, ownership, metadata, safety) | W2, W3, W5, W8, W15, W16 | 03, 05, 06 | M1 | M4 |
| **Performance** (per-CPU, batching, hugepage, budgets, background return) | W6, W7, W11, W12, W13 | 04, 05 | M3 | M5 |
| **Formal** (Lean model, machine-checkable tables, contracts) | W1 | 02 | M0 | M7 (single-core), M9 (SMP) |
| **Operational** (stats, profiles, controls, diagnostics) | W17, W20, W12 | 07, 04 | M4 | M6 |
| **Microkernel** (seLe4n profile, **required**) | W22 + W1 bridge + W4 seam | 09, 02, 04 | M1 (sim + bridge skeleton) | M8 (real ABI), M9 (SMP) |

> **Microkernel conformance is normative (SPEC §36).** On a non-microkernel host the floor is: the Lean
> bridge **builds and is proved** (single-core) and the **fixed-arena profile** ships, with POSIX as the
> default runtime backend. D2 raises our ambition: we co-develop the capability backend against a
> deterministic simulator from M1 so the real-ABI swap at M8 is a backend change, not an architecture change.

---

## 3. The central architectural seam (the spine of the plan)

Everything hangs off one interface. Getting it right at M0/M1 is what lets POSIX and seLe4n be co-equal and
lets domains parallelize.

```text
            ┌──────────────────────────────────────────────────────────────┐
            │ Public API:  C ABI (malloc/free/...) + Rust GlobalAlloc        │  W8  → plan 06
            ├──────────────────────────────────────────────────────────────┤
            │ Request classifier: size class, align, arena, label, hints     │  W2  → plan 03
            ├──────────────────────────────────────────────────────────────┤
            │ Front-end: per-CPU (RSEQ / pinned-core) or thread cache         │  W6,W7 → plan 05
            ├──────────────────────────────────────────────────────────────┤
            │ Middle-end: transfer caches + central free lists (label-part.)  │  W5,W6 → plan 03,05
            ├──────────────────────────────────────────────────────────────┤
            │ Arena policy/authority domains: cap + label + quota + decay      │  W9  → plan 06
            ├──────────────────────────────────────────────────────────────┤
            │ Placement: hugepage filler / large-mapping policy               │  W11 → plan 04
            ├───────────────────────────  S E A M  ────────────────────────┤
            │ trait TopoBackingProvider   (POSIX = degenerate single-authority │  W4  → plan 04
            │   single-label case of the §36.6 capability provider)            │
            │   ┌───────────────────────┐   ┌──────────────────────────┐    │
            │   │ PosixBackingProvider  │   │ Sele4nBackingProvider     │    │  W22 → plan 09
            │   │ mmap/madvise/mprotect │   │  built on sele4n-abi/     │    │
            │   │ (ambient authority)   │   │  -types/-sys; host Sim →  │    │
            │   └───────────────────────┘   │  QEMU/real kernel (M8)    │    │
            │                               └──────────────────────────┘    │
            ├──────────────────────────────────────────────────────────────┤
            │ Formal: Lean allocator model  +  seLe4n Bridge (co-developed)   │  W1  → plan 02
            └──────────────────────────────────────────────────────────────┘
```

**Why this shape (D2).** The SPEC's POSIX backend (§18) and seLe4n backing-provider contract (§36.6) are the
*same* abstraction at different fidelity. By expressing POSIX as the *degenerate, single ambient-authority,
single-label* case of the capability provider, the allocator core is written **once**; capabilities, labels,
and quotas exist in the core types from M1 (trivial on POSIX, real on seLe4n).

**The seLe4n ABI is real and Rust-native.** [`hatter6822/seLe4n`](https://github.com/hatter6822/seLe4n/tree/main/rust/sele4n-abi)
ships a `no_std` workspace — `sele4n-abi` (`MessageInfo`, `encode_syscall`/`invoke_syscall` via AArch64
`svc #0`, `IpcBuffer`, `TypeTag`, `PagePerms`), `sele4n-types` (`CPtr`/`Slot`/`ObjId`, `KernelError`/
`KernelResult`, `AccessRights`, `SyscallId`), plus `sele4n-sys`/`sele4n-hal`. `Sele4nBackingProvider`
compiles against these from M1; a host `Sele4nSim` mirrors the same `invoke_syscall` surface for host
execution and differential testing; M8 swaps only the execution target.

### 3.1 Cross-plan interface contracts (frozen at M0/M1)

These are the seams between domain plans; changing one is a cross-cutting WU touching both sides.

| Seam | Contract (abstract) | Owned by (plan) | Consumed by (plans) |
|---|---|---|---|
| Classification | `classify(size,align,flags) -> Request{arena, sc/medium/large, label}` | 03 | 05, 06, 08 |
| Size-class tables | generated `SizeClass[]`, `size->class`, batch/capacity | 02→03 | 03, 05 |
| Pagemap | `classify_ptr(addr) -> Small/Large/Interior/Metadata/Released/External` | 03 | 05, 06, 08 |
| Central list | `central_remove_batch / insert_batch / span_is_empty` | 03 | 05 |
| Front-end | `fe_pop/fe_push -> {success/empty/full/abort}` | 05 | 06 |
| Backing provider | `TopoBackingProvider` (§3 box, §36.6) | 04 | 03, 06, 09 |
| Arena/authority | `Arena{id, authority_cap, label, quota, decay, hooks}` + lifecycle | 06 | all |
| Stats | `stats_snapshot(flags) -> epoch-consistent view` | 07 | 10, 08 |
| Trace | SPEC §33.7 grammar emit/replay | 02+08 | all differential tests |

---

## 4. Key decisions

### 4.1 Ratified

- **D1 — Stack: Rust core (`#![no_std]`-capable hot path) + per-arch asm (RSEQ) + Lean 4.** Safety-first
  mandate, formal-verification emphasis, seLe4n alignment, StarMalloc precedent (R9). C ABI via `#[no_mangle]`;
  `GlobalAlloc` adapter for Rust. No C++ core — operators/C ABI export from the Rust core.
- **D2 — seLe4n co-equal from the start, against the *real* ABI.** POSIX + capability backends co-developed
  behind `TopoBackingProvider`; `Sele4nSim` is a host double sharing the real `sele4n-abi`/`sele4n-types`
  surface; M8 runs on the real kernel. Capability arenas/labels/quotas are core types from M1.

### 4.2 Decisions ratified in W0 (W0-1)

D3–D8 are **ratified**; the full record (outcome + rationale) is in
[`docs/DECISIONS.md`](../../docs/DECISIONS.md). Summary:

| ID | Decision | Outcome | Status |
|---|---|---|---|
| D3 | Build orchestration | Cargo workspace + `cargo xtask` driving `lake` + codegen | Ratified, implemented (W0-4) |
| D4 | Allocator page / `small_max` | `16 KiB` page; `small_max` (→`32 KiB`) finalized with the tuned table at M1 | Ratified (page); `small_max` at M1 |
| D5 | Licensing (seLe4n GPLv3 vs core, §36.20) | core **MIT**; seLe4n integration **GPL-3.0-or-later** + `NOTICE` | Ratified, implemented (W0-12) |
| D6 | Arena routing in caches (§11.7) | bound-arena fast path → arena-qualified slots at M4 | Ratified, deferred to M2 |
| D7 | Property/fuzz stack | `proptest` + `cargo-fuzz`; custom differential harness | Ratified, scaffolded (W0-7) |
| D8 | Consuming seLe4n crates | git dep pinned to SHA `57c1105…` + vendored mirror + periodic-bump WU | Ratified; pin recorded |

---

## 5. Milestones (the temporal integration spine)

Each milestone is a demoable allocator. CI gates (§6) must be green to close one. "seLe4n acceptance"
reflects D2: the simulated capability backend passes the same vertical slice as POSIX.

| M | Theme (SPEC phase) | Composes (plans) | Exit (abbrev.) |
|---|---|---|---|
| **M0** | Bootstrap & walking skeleton | 01 (all), 02 (skeleton+bridge stub), 04 (provider trait+Posix/Sim), 09 (W22-0 ABI pin) | dual-arch build+lint+`lake check`; stub malloc over either provider; size-class generator emits a Lean-accepted table; trace line parses. |
| **M1** | Minimal correct sequential allocator (Phase A) | 02 (core), 03, 04 (core), 06 (C core + realloc basics), 05 (global lock) | ABI+property tests; B.1/B.3 debug checks; core §33.4 theorems; calloc overflow+rounding; runs identically over Posix+Sim. |
| **M2** | Concurrency & label/arena-aware caches (Phase B) | 05 (caches+lock hierarchy+TLS+fork v0), 04 (single-domain), 08 (concurrency) | TSan-clean stress; bounded cache footprint; refill/flush conservation replay; lock-order checker. |
| **M3** | Per-platform fast paths (Phase C + §36.10) | 05 (RSEQ x86-64/AArch64; pinned-core), 04 (topology) | no correctness delta vs locked (forced-migration); RSEQ contract proved; `per_core_cache_abort_no_change`. |
| **M4** | Arenas, authority, quotas & control (Phase D + §36.4/§36.13) | 06 (arenas+hooks), 07 (config/control), 04 (topology), 09 (CSlot/VSpace accounting) | isolation/reset/destroy/revocation; CSlot/VSpace exhaustion; quota/authority/label; arena theorems. |
| **M5** | Hugepage / large-mapping + release (Phase E + §36.9) | 04 (filler/cache/region + release controller) | H-001..H-005; release avoids refault loop; whole-mapping release preferred on Sim. |
| **M6** | Observability + information-flow (Phase F + §36.12) | 07 (stats/profiling/explain + placement), 09 (label-scoped stats) | RSS explainable; sampling production-safe; `stats_observation_noninterference`; redaction test. |
| **M7** | Hardening + formal refinement (Phase G) | 08 (hardened/debug + fuzz), 02 (full single-core theorem set + trace replay) | all §33.4 + single-core §36.17; fuzz+sanitizer clean; scrub/label tests. |
| **M8** | Run on the real seLe4n kernel (Phase H) | 09 (real provider; resource server in QEMU; adapters) | §36.16 suite in QEMU; fixed-arena no-retype; dynamic exhaustion clean. |
| **M9** | Deployment, ABI freeze, SMP bridge & GA | 10 (deploy/ABI/perf), 09+02 (SMP bridge) | ABI compatibility; SMP bridge families; perf vs baselines recorded. |

```text
M0 ─▶ M1 ─▶ M2 ─▶ M3 ─▶ M4 ─▶ M5 ─▶ M6 ─▶ M7 ─▶ M8 ─▶ M9
        └─ plan 02 (formal) + plan 09 W22 (Sim) co-developed continuously from M0 ─┘
Continuous: plan 02 (formal), plan 07 (stats/config), plan 08 (testing) grow every milestone.
```

---

## 6. CI gate ladder (additive per milestone)

| Gate | At | Checks |
|---|---|---|
| G-build | M0 | x86-64 + AArch64 build (debug+performance); `lake exe check`; lint; SPDX; markdownlint. |
| G-table | M1 | size-class generator output == golden, Lean accepts golden (single source of truth). |
| G-core | M1 | ABI + property + B.1/B.3 debug checks; calloc overflow+rounding; errno semantics. |
| G-model | M1→ | named §33.4 theorems for shipped transitions proved, or tracked `V-004` debt. |
| G-sim | M1→ | the milestone's vertical slice passes over `Sele4nSim`, not only POSIX. |
| G-conc | M2 | TSan-clean concurrency stress; lock-order checker; budget bounds. |
| G-fast | M3 | forced-migration RSEQ/pinned-core equivalence vs locked; RSEQ battery. |
| G-arena | M4 | isolation, reset/destroy, revocation, CSlot/VSpace exhaustion, quota/authority/label. |
| G-mem | M5 | H-001..H-005; release avoids refault loop. |
| G-ops | M6 | stats reconcile (§8.6); redaction; sampling overhead bound. |
| G-hard | M7 | full §33.4 + single-core §36.17; fuzz clean; sanitizer-clean; scrub/label. |
| G-sele4n | M8 | §36.16 suite on real seLe4n in QEMU; fixed-arena no-retype; dynamic exhaustion. |
| G-abi | M9 | ABI compatibility; SMP bridge families; perf recorded. |

**Formal cadence:** tables Lean-verified from M1; each shipped transition proved or carrying a tracked
`V-004` debt; debt reviewed every milestone and burned to zero (single-core) at M7. **Security-review
cadence:** run the repo's `security-review` at the close of M4, M7, M8, and on any WU touching `/sele4n`,
`/arch`, freelist encoding, or metadata protection.

---

## 7. Global risk register

| # | Risk | L | I | Mitigation | Plan |
|---|---|---|---|---|---|
| R1 | Empty-span detection across caches wrong → leak or premature reuse. | Med | High | Debug-exact conservation co-located with fast path; differential tests. | 03, 08 |
| R2 | D2 co-equal seLe4n slips to "POSIX-first." | Med | High | Provider compiles vs real ABI from M1; Sim shares the surface; G-sim every milestone. | 09, 04 |
| R3 | TLS re-entrancy → init deadlock/recursion. | Med | High | Initial-exec TLS + dlopen bootstrap proven at M2 before fast paths. | 05 |
| R4 | Hand-maintained tables drift. | Med | Med | Single generator + Lean check + CI golden-diff. | 02, 03 |
| R5 | RSEQ correctness under migration/signal. | Med | High | Locked baseline first; equivalence is a gate; per-arch abort handlers + battery. | 05 |
| R6 | Lock-order cycles → deadlock. | Med | High | Total order + debug lock-order checker (gate); hand-over-hand ≤1 middle-end lock. | 05, 03 |
| R7 | Capability/CSlot leaks on destroy/revoke. | Med | High | Generation counters; revocation tests; `destroy_revokes_descendants`. | 06, 09, 02 |
| R8 | IPC slow-path cost on seLe4n. | Med | Med | Batch backing; pre-grant/fixed-arena; latency classes. | 09, 04 |
| R9 | Formal proofs lag → "formal" theater. | Med | High | G-model gate + V-004 debt burned to zero at M7. | 02 |
| R10 | Info-flow leak via stats/dirty reuse. | Low | High | Label-partitioned structures from M1; scrub-before-downgrade; redaction. | 09, 08, 07 |
| R11 | License incompatibility (MIT vs GPLv3). | Low | Med | D5 split + NOTICE before any `/sele4n` code. | 01 |
| R12 | Scope/throughput; analysis paralysis. | High | Med | Vertical-slice milestones; M1 is a usable allocator. | all |
| R13 | Upstream seLe4n ABI drift. | Med | Med | Pin to SHA (D8); periodic bump WU; Sim mirrors pinned surface ⇒ drift is a compile error. | 09, 01 |

---

## 8. Definition of Done (every WU) & PR checklist

The PR template (W0-11) encodes this. A WU is **done** only when:

- [ ] Builds clean on x86-64 **and** AArch64 (debug + performance); no new lint warnings.
- [ ] Unit tests for the WU; **property tests** updated if API behavior changes (§34.3).
- [ ] Debug **invariant checks** (Appendix B) for the touched state pass; new state adds its checker.
- [ ] Transition added/changed ⇒ **Lean model + named §33.4/§36.17 obligation** updated and proved, or a
      `V-004` debt filed with a tracking ID + reason.
- [ ] State added ⇒ **stats (plan 07)** expose it and it **reconciles** (§8.6); **trace grammar** updated.
- [ ] Knob/behavior added ⇒ **control plane (plan 07)** + docs updated; default matches §32.3.
- [ ] The **seLe4n vertical slice** still passes over `Sele4nSim` (G-sim) for milestones ≥ M1.
- [ ] No new **Appendix-F anti-pattern** (reviewer-confirmed).
- [ ] SPDX header present; `/sele4n` files under the correct license (D5).
- [ ] Transitions tagged with their SPEC state-machine name (M-001).

---

## 9. Parallelization guide (tracks → documents)

Once M1's seams (§3.1) are frozen, these tracks proceed largely independently. Each track maps to one or two
domain plans; the frozen seams are the only contracts a track may not change unilaterally.

| Track | Plans | Owns |
|---|---|---|
| Formal | 02 | Lean model, tables, trace oracle, bridge. *Mostly independent.* |
| Core spine | 03 | classify → pagemap → spans → central list. |
| Backend | 04 | provider seam, POSIX, hugepage, release, topology. |
| Caches/concurrency | 05 | thread/per-CPU caches, RSEQ, lock hierarchy, TLS, fork. |
| API/arenas | 06 | public ABI, realloc, capability arenas, hooks. |
| Ops | 07 | stats, placement, config/control. |
| Quality | 08 | hardening, debug, property/differential/fuzz/bench. |
| seLe4n | 09 | Sim, resource server, client, labels, real-kernel. Co-develops with Backend across the seam. |
| Release | 10 | deployment, ABI, perf validation. |

---

## 10. Traceability matrix (SPEC requirement family → workstream → plan)

| SPEC family | IDs | Workstreams | Plan | Verified by |
|---|---|---|---|---|
| Functional | F-001..F-010 | W8, W9, W4, W6/W7 | 06, 04, 05 | ABI + property tests (plan 08) |
| Safety | S-001..S-010 | W1, W3, W5, W4, W16 | 02, 03, 04, 05 | Lean (02) + Appendix-B checks (08) |
| Performance | P-001..P-008 | W6, W7, W11, W12, W13 | 05, 04 | benchmarks (08), gates G-fast/G-mem |
| Operational | O-001..O-007 | W17, W20, W12 | 07, 04 | stats reconcile (07), control tests (07) |
| Formal | V-001..V-005 | W1, W2 | 02, 03 | `lake exe check` (G-table/G-model) |
| State machine | M-001..M-005 | W1, W5, W4 | 02, 03, 04 | Lean + transition tags + B checks |
| Pagemap | P-Map-001..006 | W3 | 03 | W3 tests + `pagemap_lookup_sound` |
| Central list | C-001..C-005 | W5 | 03 | W5 tests, B.3 |
| Hugepage | H-001..H-005 | W11 | 04 | B.4, G-mem |
| Cache | B.2 set | W6, W7 | 05 | budget tests, G-conc |
| Arena | §22 + B.5 | W9 | 06 | isolation/reset/destroy tests, G-arena |
| seLe4n | §36 conformance | W22 | 09 | §36.16 suite + §36.17 theorems, G-sele4n |

---

## 11. The very first actions (start here)

1. **W0-1** — record D3–D8 in §4.2 (D5 must close before any `/sele4n` file; D8 before `Sele4nBackingProvider`
   first compiles). → [plan 01](01-repository-and-infrastructure.md)
2. **W0-2/3/4** — repo layout, pinned toolchains, Cargo workspace + `xtask` + Lean `lake`. → plan 01
3. **W0-5/6** — CI (x86-64 + AArch64) with build/lint/`lake exe check` required on the branch. → plan 01
4. **W0-9** — Claude-on-web SessionStart hook (use the `session-start-hook` skill). → plan 01
5. **W22-0 (pin) + W1-1/W1-2 + W4-1 + W0-14** — pin the seLe4n crates; Lean skeleton + empty bridge; the
   `TopoBackingProvider` trait with Posix + `Sele4nSim` stubs; the walking skeleton — **closing M0**.
   → plans 09, 02, 04, 01
6. **W1-4a → W1-4e + W2-1** — the size-class model + generator (single source of truth), the M1 longest pole.
   → plans 02, 03

Everything else follows the milestone ladder (§5). Material changes to sequencing, seams, or conformance are
PRs against this overview; workstream detail changes are PRs against the relevant domain plan.

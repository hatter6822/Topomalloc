# TopoMalloc Workstream & Implementation Plan

**Document status:** Execution plan, revision 1.1
**Date:** 2026-06-03
**Implements:** `planning/SPEC.md` (TopoMalloc Specification, rev 0.3)
**Audience:** allocator implementers, formal-methods engineers, build/release engineers, and reviewers
**Companion to, not a replacement for:** the SPEC is normative; this document sequences *how* the SPEC gets built.

---

## 0. How to use this document

This is a **workstream plan**, not a schedule. It decomposes the SPEC into independently buildable
tracks and orders them so that the allocator is **correct at every step**, **testable at every step**,
and **formally modeled in lockstep**. It deliberately avoids calendar dates and headcount; it uses
*relative effort* and *dependencies* so the work can be executed by one engineer serially or several
engineers in parallel.

Three nouns are used precisely:

| Term | Meaning | ID form |
|---|---|---|
| **Workstream** | A long-lived capability track (e.g. "the back-end"). Owns an interface and a body of code/proof. | `W0`..`W23` |
| **Work Unit (WU)** | The smallest planned, independently reviewable, independently testable change. A few days at most. | `W5-3` |
| **Milestone** | A temporal integration gate that composes WUs from several workstreams into a releasable, demonstrable capability. Has CI gates and exit criteria. | `M0`..`M9` |

Each WU carries a **size** (`S`/`M`/`L`, relative) and a **parallelism** flag (`∥` = can run concurrently
with sibling WUs once dependencies are met). Acceptance criteria are written so a reviewer can say *done*
or *not done* without judgement calls.

**Reading order for a newcomer:** §1 (principles) → §3 (the central seam) → §4 (W0 repository setup,
the first work) → §5 (milestones) → the workstream(s) you own in §6.

### 0.1 Change log

| Rev | Date | Summary |
|---|---|---|
| 0.1 | 2026-06-03 | Initial decomposition draft. |
| 0.9 | 2026-06-03 | **Optimization pass.** Reordered to a single `TopoBackingProvider` seam; folded seLe4n into every milestone via a deterministic simulator; pulled the size-class single-source-of-truth, bootstrap metadata, and TLS-model constraints onto the critical path; collapsed redundant WUs; added the interface-contract table to unblock parallel tracks. |
| 1.0 | 2026-06-03 | **Refinement pass.** End-to-end consistency: every WU traces to a SPEC section and a requirement family; milestone exit criteria reconciled with SPEC conformance classes; risk register and Definition-of-Done finalized; traceability matrix completed; ratified decisions D1/D2 recorded. |
| 1.1 | 2026-06-03 | **seLe4n real-ABI integration.** Incorporated the existing `hatter6822/seLe4n` Rust ABI (`sele4n-abi`/`sele4n-types`/`sele4n-sys`/`sele4n-hal`; `no_std`; AArch64 `svc #0`). `Sele4nBackingProvider` now compiles against the real ABI from M1; `Sele4nSim` reframed as a host test-double sharing the same `invoke_syscall`/`SyscallId`/`TypeTag`/`KernelError` surface; M8 reframed from "build the real API" to "execute on the real kernel (QEMU/RPi5)"; added decision D8 (dependency pinning) and risk R13 (upstream ABI drift). |
| 1.2 | 2026-06-03 | **Deep-decomposition pass.** Audited dependencies/sequencing and corrected per-WU dependency scoping (W4 trait vs extent ops; W8's M1 path goes via the central list, not the caches; W15 classify references; W3-1 bootstrap independence). Broke every L-sized (and several pivotal M-sized) work units into smaller, independently-shippable sub-units (letter-suffixed, e.g. `W5-3a`..`W5-3e`), and added engineering **▸ Decomposition** notes — design space, ordered steps, pitfalls, proof/test obligations — for the highest-complexity units (size-class model, pagemap, empty-span conservation, extents, RSEQ assembly, refill/flush locking, arena revocation, hugepage filler, release controller, differential testing, `Sele4nSim`, resource server). End-to-end best-practices sweep. |

---

## 1. Execution principles

These are binding on every WU. They are how the plan stays *correct* while moving fast.

1. **Safety before policy (SPEC §2.4).** A WU may ship a dumb-but-correct policy; it may *never* ship an
   unsafe fast path. Every PR keeps the unconditional safety invariants (S-001..S-010) green. A policy
   mistake must never be reachable as a safety mistake.
2. **Vertical slices, always runnable.** Every milestone produces a *working allocator*, not a pile of
   subsystems. M1 is a real (slow) malloc; each later milestone makes it faster/richer without ever
   regressing correctness.
3. **Formal model in lockstep (SPEC §33, V-004).** When a WU adds or changes an abstract transition, the
   Lean model and its proof obligations are updated *in the same WU* — or the gap is recorded explicitly as
   a `V-004` refinement debt with a tracking ID. The model never silently lags the code.
4. **Single source of truth for generated artifacts (Appendix F).** Size-class tables, lookup tables, batch
   sizes, alignment masks, and capacity limits are emitted by **one** generator, checked by Lean, and
   consumed (never hand-edited) by the implementation. Hand-maintained tables are a rejected anti-pattern.
5. **One seam, two backends (decision D2).** All OS/kernel interaction goes through the
   `TopoBackingProvider` contract (§3). POSIX and seLe4n are co-equal implementations behind it from M1.
   No allocator-core code calls `mmap` or a kernel `retype` directly.
6. **Deterministic-replay testability (SPEC §30.4, §33.7).** Every layer can run in a deterministic mode
   that emits the SPEC trace grammar and replays against the Lean executable model (differential testing).
   "Hard to test deterministically" is a design bug to fix, not a fact to accept.
7. **Invariant checks are first-class code.** Debug/hardened builds run the Appendix B checklist as runtime
   assertions. Transitions are tagged in code with their SPEC state-machine name (M-001).
8. **Profiles are compile-and-run-time features, not forks.** `performance` / `hardened` / `debug` /
   `deterministic_test` / `low_rss` / `hugepage_optimized` are feature-gated from M0 so hardening is never a
   later "rewrite."
9. **No new anti-patterns.** Appendix F is a CI-enforced review checklist, not a suggestion.

---

## 2. Conformance map — what "done" means

The SPEC defines five conformance classes. The plan delivers them as follows; this is the backbone of the
milestone exit criteria (§5) and the traceability matrix (§11).

| Conformance class (SPEC) | Primary workstreams | First satisfied at | Fully satisfied at |
|---|---|---|---|
| **Core** (API, ownership, metadata, safety) | W2, W3, W5, W8, W15, W16 | M1 | M4 |
| **Performance** (per-CPU, batching, hugepage, adaptive budgets, background return) | W6, W7, W11, W12, W13 | M3 | M5 |
| **Formal** (Lean model, machine-checkable tables, contracts) | W1 | M0 (skeleton) | M7 (full theorem set), M9 (SMP bridge) |
| **Operational** (structured stats, profiles, controls, diagnostics) | W17, W20, W12 | M4 | M6 |
| **Microkernel-integration** (seLe4n profile, **required**) | W22 + W1 bridge + W4 seam | M1 (simulated + bridge skeleton) | M8 (real ABI), M9 (SMP) |

> **Microkernel conformance is normative (SPEC §36 preamble, normative-language §).** Per the SPEC, on a
> non-microkernel host the floor is: the Lean bridge model **builds and is proved** (single-core forms) and
> the **fixed-arena profile** is provided, with POSIX as the default runtime backend. Decision **D2** raises
> our ambition above that floor: we co-develop the capability backend against a deterministic simulator from
> M1 so the real-ABI swap at M8 is a backend change, not an architecture change.

---

## 3. The central architectural seam (the spine of the plan)

Everything in the plan hangs off one interface. Getting this seam right at M0/M1 is what lets POSIX and
seLe4n be co-equal and lets workstreams parallelize.

```text
            ┌──────────────────────────────────────────────────────────────┐
            │ Public API:  C ABI (malloc/free/...) + Rust GlobalAlloc        │  W8
            ├──────────────────────────────────────────────────────────────┤
            │ Request classifier: size class, align, arena, label, hints     │  W2
            ├──────────────────────────────────────────────────────────────┤
            │ Front-end: per-CPU (RSEQ / pinned-core) or thread cache         │  W6, W7
            │            keyed by (cpu/core, arena, sc) and label             │
            ├──────────────────────────────────────────────────────────────┤
            │ Middle-end: transfer caches + central free lists                │  W5, W6
            │             label-partitioned, arena-qualified                  │
            ├──────────────────────────────────────────────────────────────┤
            │ Arena policy/authority domains: cap + label + quota + decay      │  W9
            ├──────────────────────────────────────────────────────────────┤
            │ Placement: hugepage filler / large-mapping policy               │  W11
            ├───────────────────────────  S E A M  ────────────────────────┤
            │ trait TopoBackingProvider                                      │  W4 defines
            │   reserve_window / create_frame / map / unmap /                 │  (§36.6 contract,
            │   commit / decommit / purge / revoke / recycle                  │   generalized so
            │                                                                │   POSIX is a
            │   ┌───────────────────────┐   ┌──────────────────────────┐    │   degenerate
            │   │ PosixBackingProvider  │   │ Sele4nBackingProvider     │    │   single-authority
            │   │ mmap/madvise/mprotect │   │  ┌────────────────────┐   │    │   case)
            │   │ (ambient authority)   │   │  │ Sele4nSim (host)   │   │    │
            │   └───────────────────────┘   │  └─────────┬──────────┘   │    │
            │                               │  on sele4n-abi/-types/-sys│    │
            │                               └──────────────────────────┘    │
            ├──────────────────────────────────────────────────────────────┤
            │ Formal: Lean allocator model  +  seLe4n Bridge (co-developed)   │  W1
            └──────────────────────────────────────────────────────────────┘
```

**Why this shape (D2 rationale).** The SPEC's POSIX backend (§18) and seLe4n backing-provider contract
(§36.6) are the *same* abstraction at different fidelity: reserve address space, back it, change its
physical/authority state, return it safely. By expressing the POSIX backend as the *degenerate, single
ambient-authority, single-label* case of the capability provider, the allocator core is written **once**.
Capabilities, labels, and quotas exist in the core types from M1; on POSIX they are trivial constants, on
seLe4n they are real. This is what "seLe4n co-equal from the start" buys us, and it is the single most
important structural decision in this plan.

**The seLe4n ABI is real and Rust-native.** [`hatter6822/seLe4n`](https://github.com/hatter6822/seLe4n/tree/main/rust/sele4n-abi)
ships a `no_std` Rust workspace —
`sele4n-abi` (syscall/IPC encoding: `MessageInfo`, `encode_syscall`/`decode_response`, `invoke_syscall` via
AArch64 `svc #0`, `IpcBuffer`, `RegisterFile`, the `TypeTag` retype enum, `PagePerms`), `sele4n-types`
(identifier newtypes `CPtr`/`Slot`/`ObjId`/…, the 50-variant `KernelError` + `KernelResult`, `AccessRights`,
the `SyscallId` enum), plus `sele4n-sys` and `sele4n-hal`. Because it is the *same language* as our core
(D1) and `no_std`, `Sele4nBackingProvider` **compiles against these crates from M1** and is type-checked in
CI continuously; the host-side `Sele4nSim` implements the *same* `invoke_syscall`/`SyscallId`/`TypeTag`/
`KernelError` surface so the allocator runs and is differentially tested on the host without QEMU. M8 then
swaps only the *execution target* (host Sim → real kernel under QEMU/RPi5), not the API. This collapses the
biggest unknown the original plan carried and is why R2 is downgraded (§8).

### 3.1 Interface contracts to freeze early (enables parallelism)

These signatures are defined as part of M0/M1 so workstreams can develop against stable seams. They are the
"public edges" between tracks; changing one is a cross-cutting WU.

| Seam | Contract (abstract) | Defined by | Consumed by |
|---|---|---|---|
| Classification | `classify(size, align, flags) -> Request{arena, sc | medium | large, label}` | W2 | W8, W6, W18 |
| Size-class tables | generated `SizeClass[]`, `size->class` lookup, batch/capacity | W1+W2 generator | W2, W5, W6 |
| Pagemap | `classify_ptr(addr) -> SmallObject|Large|Interior|Metadata|Released|External` | W3 | W8, W5, W15, W18 |
| Central list | `central_remove_batch(node,arena,sc) / insert_batch(...) / span_is_empty(span)` | W5 | W6 |
| Front-end | `fe_pop(core,arena,sc) / fe_push(...)` with `{success|empty|full|abort}` | W6/W7 | W8 |
| Backing provider | `TopoBackingProvider` (§3 box, §36.6) | W4 | W5, W11, W9, W22 |
| Arena/authority | `Arena{id, authority_cap, label, quota, decay, hooks}` + lifecycle | W9 | everything |
| Stats | `stats_snapshot(flags) -> epoch-consistent view` | W17 | W20, tests |
| Trace | SPEC §33.7 grammar emit/replay | W1+W21 | all differential tests |

---

## 4. Key decisions (ratified + open)

### 4.1 Ratified

- **D1 — Implementation stack: Rust (core, `#![no_std]`-capable hot path) + per-architecture assembly for
  RSEQ/restartable sequences + Lean 4 for the model.** *Rationale:* the SPEC's safety-first mandate and
  formal-verification emphasis, alignment with the seLe4n target (Lean 4 kernel, Rust HAL crates), and the
  verified-allocator precedent (StarMalloc, R9). The allocator core avoids `std` and avoids re-entering its
  own global allocator (S-007). C ABI symbols are exported via `#[no_mangle]`; a `GlobalAlloc`/allocator-API
  adapter is provided for Rust consumers. *Implication for C++ consumers:* operator new/delete and the C ABI
  are exported from the Rust core; no C++ core is maintained.
- **D2 — seLe4n is co-equal from the start, against the *real* ABI.** A real seLe4n Rust ABI exists
  (`hatter6822/seLe4n`: `no_std` workspace crates `sele4n-abi`, `sele4n-types`, `sele4n-sys`, `sele4n-hal`;
  AArch64). `Sele4nBackingProvider` (W22) is written against those crates from M1 and **type-checks in CI
  continuously**. A host-side `Sele4nSim` implements the *same* `invoke_syscall`/`SyscallId`/`TypeTag`/
  `KernelError` surface so the allocator runs and is differentially tested on the host without QEMU; M8 swaps
  the *execution target* to the real kernel (QEMU/RPi5), not the API. Capability-backed arenas, labels, and
  quotas are core types from M1 (trivial/ambient on POSIX).

### 4.2 Open decisions to ratify during W0 (do not block M0; must close before the dependent milestone)

| ID | Decision | Default if unresolved | Needed by |
|---|---|---|---|
| D3 | Build orchestration: Cargo workspace + `cargo-xtask` vs. add Bazel for cross-language graph | Cargo workspace + `xtask` driving `lake` and codegen | M0 |
| D4 | Allocator-page default (`8 KiB` vs `16 KiB` server) and `small_max` (`32` vs `64 KiB`) — Appendix C | `16 KiB` page, `32 KiB` small_max; both are *generated constants*, cheap to revisit | M1 (table gen) |
| D5 | Licensing: SPEC §36.20 flags seLe4n GPLv3 vs TopoMalloc core. | Keep core **MIT** (current LICENSE); put the seLe4n integration layer (`/sele4n`) under a separate, seLe4n-compatible license with a NOTICE. | W0 (before any `/sele4n` code) |
| D6 | Arena routing in caches (§11.7): bound-arena fast path vs arena-qualified slots. | **Bound-arena fast path** for default arena through M3; add arena-qualified slots in M4 when explicit arenas land. | M2 |
| D7 | Property-test + fuzz stack | `proptest` + `cargo-fuzz`/libFuzzer; differential harness custom | M1 (property), M7 (fuzz) |
| D8 | How to consume the upstream seLe4n crates (`sele4n-abi`/`-types`/`-sys`) | **git dependency pinned to a commit SHA**, with a vendored mirror as fallback and a periodic bump WU; share the workspace `no_std`/edition discipline | M1 (when `Sele4nBackingProvider` first compiles) |

---

## 5. Milestones (temporal integration gates)

Each milestone is a *demoable allocator*. "seLe4n acceptance" rows reflect D2: the simulated capability
backend must pass the same vertical slice as POSIX. CI gates (§7.2) must be green to close a milestone.

### M0 — Bootstrap & walking skeleton
**Theme:** the dual-backend toolchain proves itself end to end.
**Composes:** W0 (all), W1-1/W1-2 (Lean skeleton + empty bridge), W4-1 (provider trait + Posix/Sim stubs),
W22-0 (pin the real `sele4n-abi`/`sele4n-types` so the seLe4n provider stub type-checks).
**Deliverable:** library builds and links on x86-64 **and** AArch64; exports a stub `malloc` that
bump-allocates through `TopoBackingProvider` (selectable Posix or Sim) and a `free` that is a no-op leak; a
trivial Lean package and an empty `Bridge.lean` compile under `lake`; CI runs build+lint+lean-check.
**Exit:** `xtask ci` green on both arches; the size-class generator emits a (possibly trivial) table that
Lean accepts; trace emit produces a parseable line. seLe4n acceptance: skeleton runs identically over Sim,
**and the workspace compiles against the real `sele4n-abi`/`sele4n-types` crates** (pinned per D8) so
`Sele4nSim` and `Sele4nBackingProvider` already share one type surface.

### M1 — Minimal correct sequential allocator (dual-backend) — *SPEC Phase A*
**Composes:** W1-core, W2, W3, W4-core (Posix+Sim), W5, W8-core (`malloc`/`free`/`calloc`/`realloc` + usable_size), W15-basic, W16-globallock.
**Deliverable:** correct single-threaded `malloc/free/calloc/realloc/aligned_alloc/posix_memalign/
malloc_usable_size`; pagemap-backed unsized free; a **bootstrap metadata allocator** (S-007); a single
default **capability-backed arena** with ambient authority + single label; one global lock for correctness.
Runs identically over Posix and Sim under deterministic replay. `Sele4nBackingProvider` already compiles
against the real ABI (D8); Sim provides host-side execution and differential testing.
**Exit (Core floor):** ABI tests + property tests pass; debug invariant checks (Appendix B.1/B.3) pass;
Lean proves `malloc/free_preserves_wellformed`, live-disjointness, `size_class_table_covers_all_small_requests`,
`pagemap_lookup_sound` for the sequential model; bridge `WellFormed` skeleton (single-label) compiles and the
`backing_descends_from_untyped` shape is stated. calloc overflow **and** rounding-overflow guarded (§26.1, §9.7).

### M2 — Concurrency & label/arena-aware caches — *SPEC Phase B*
**Composes:** W6 (thread cache → per-CPU locked), W16 (lock hierarchy + atomics ordering + fork v0 + TLS
initial-exec), W13-min (single-domain fallback), W21-concurrency.
**Deliverable:** batched refill/flush; cache budget controller v1; correct under many-thread stress; lock
order checker in debug builds (§27.2). Caches are label-partitioned and arena-qualified per D6.
**Exit (Performance, start):** many-thread tests stable; bounded cache footprint (B.2); model replay for
cache refill/flush conservation (`cache_refill/flush_preserves_ownership_conservation`). Sim passes the same
concurrency suite.

### M3 — Per-platform fast paths — *SPEC Phase C + §36.10*
**Composes:** W7 (Linux RSEQ x86-64 + AArch64; abort handling), W7-sele4n (pinned-thread per-core mode),
W13 (LLC/NUMA topology discovery).
**Deliverable:** lock-free common path on Linux via RSEQ; pinned-core fast path for the seLe4n profile; both
behind one front-end contract with a proven abort/no-change case.
**Exit:** no correctness delta vs locked mode (forced-migration tests); measurable hot-path win; Lean RSEQ
axioms (abort/empty/success + frame condition, §33.5) discharged for both front-ends;
`per_core_cache_abort_no_change` (single-core) proved.

### M4 — Arenas, authority, quotas & control plane — *SPEC Phase D + §36.4/§36.13*
**Composes:** W9 (full capability-backed arenas, delegation/attenuation, reset/destroy + revocation
protocol §36.13), W10 (extent hooks), W20 (config + control namespace), W13-full, W22-cslot/vspace
accounting (simulated), D6 arena-qualified slots.
**Deliverable:** explicit arenas with cap/label/quota; delegation honoring authority/quota/label
monotonicity (§36.4); arena reset/destroy with cache-drain + revocation; CSlot/VSpace accounting and clean
exhaustion errors against Sim; full control namespace (Appendix E).
**Exit (Core full):** arena isolation invariant (§22.7) tested; reset/destroy safety tests; hook
failure-injection; `arena_reset_invalidates_only_target_arena`, `arena_destroy_preserves_other_arenas`,
`arena_quota_preserved`, `destroy_revokes_descendants` (single-core) proved; CSlot/VSpace-exhaustion tests
(§36.16) pass on Sim.

### M5 — Hugepage / large-mapping backend + release controller — *SPEC Phase E + §36.9*
**Composes:** W11 (HugeAllocator/HugeCache/HugePageFiller/RegionCache; large-mapping policy on Sim), W12
(release controller, decay, background purge, pressure modes, demand reserve, emergency reserve).
**Deliverable:** hugepage-aware placement and coverage metrics on POSIX; the same placement model over
contiguous normal-frame runs on Sim (§36.9); release priority + emergency mode (§21).
**Exit (Performance full):** high hugepage coverage on target workloads; release avoids refault loops;
H-001..H-005 invariants checked; `no_live_object_released`/`release_to_os_preserves_live_objects` proved;
whole-large-mapping release preferred over partial subrelease on Sim.

### M6 — Observability + information-flow — *SPEC Phase F + §36.12*
**Composes:** W17 (stats/JSON/epoch, profiling, fragmentation, explain endpoint), W14 (lifetime/hotness
placement), W22-infoflow (label-scoped & redacted stats).
**Deliverable:** structured machine-readable stats that reconcile to managed memory (§8.6); sampled heap +
lifetime profiling; "explain memory" endpoint; label-scoped/redacted stats so low domains can't infer high-
domain allocation patterns.
**Exit (Operational full):** an engineer can explain RSS from stats; sampling is production-safe;
`stats_observation_noninterference` (single-core) proved; stats-redaction test (§36.16) passes.

### M7 — Hardening + formal refinement — *SPEC Phase G*
**Composes:** W18 (hardened/debug profiles, quarantine, guard pages, encoded freelist ptrs, scrub-before-
downgrade §36.12), W19 (sanitizer integration, deterministic mode), W1-full (entire §33.4 theorem set +
§36.17 single-core checklist; trace replay), W21-fuzz.
**Deliverable:** hardened + debug profiles catching common misuse; generated tables fully Lean-verified;
trace replay/differential testing wired into CI.
**Exit (Formal full, single-core):** all §33.4 theorems proved; §36.17 bridge families proved in single-core
form; fuzzers run clean; sanitizer builds green; `scrub_before_downgrade`, `label_partition_preserved` proved.

### M8 — Run on the real seLe4n kernel — *SPEC Phase H + §36.19 S2–S5/S7*
**Composes:** W22 (switch execution target Sim → real kernel; TopoResourceServer boot in QEMU; boot
inventory + largest-first retype §36.5; allocman/VKA/VSpace adapters §36.15; Rust GlobalAlloc + C++ ABI on
seLe4n).
**Deliverable:** the *execution target* switches from `Sele4nSim` to the real kernel — the
`Sele4nBackingProvider` API surface is unchanged because it has compiled against `sele4n-abi`/`sele4n-types`
since M1 (W22-0). `TopoResourceServer` boots on the real kernel in QEMU, classifies untyped, and serves
arenas; static fixed-arena and dynamic-service profiles both work. `Sele4nSim` is retained as the host CI
test double.
**Exit (Microkernel full, single-core):** boot-inventory/largest-first/quota/authority/label/revocation/
migration tests (§36.16) pass on real seLe4n in QEMU; fixed-arena profile needs no runtime retype; dynamic
profile handles CSpace/VSpace/quota exhaustion cleanly.

### M9 — Deployment, ABI freeze, SMP bridge & GA
**Composes:** W23 (deployment modes, mixed-allocator detection, ABI stability, init/shutdown phases,
packaging), W22-SMP (per-core/migration bridge theorems §36.17 SMP forms), W1 SMP extensions, perf
validation (§36.19 S9).
**Deliverable:** ABI-stable release series; SMP per-core caches with migration flush proved; benchmarks on
RPi5/QEMU vs static pools and an allocman-like baseline; deployment + interposition docs.
**Exit:** ABI compatibility tests; SMP bridge families proved; perf targets met or documented; release
engineering complete.

### 5.1 Milestone dependency graph

```text
M0 ─▶ M1 ─▶ M2 ─▶ M3 ─▶ M4 ─▶ M5 ─▶ M6 ─▶ M7 ─▶ M8 ─▶ M9
        │           ▲      │                    ▲      ▲
        └─ W1 bridge┘      └─ W22 (Sim) ────────┘      │
           (continuous, co-developed from M0) ─────────┘
W1 (formal), W17 (stats), W20 (config), W21 (testing) are CONTINUOUS: they grow every milestone.
```

---

## 6. Workstream catalog

Legend for WU tables: **Size** = S/M/L relative effort; **∥** = parallelizable with siblings once deps met.
Every WU's acceptance criteria implicitly include the global **Definition of Done** (§9). Complex units are
broken into **letter-suffixed sub-units** (e.g. `W5-3a`..`W5-3e`), each independently shippable and testable;
a **▸ Decomposition** note after a table gives the design space, pitfalls, and sequencing for the hardest
ones. A bare cluster reference (e.g. "W5-3") denotes the whole family.

---

### W0 — Repository, build & developer infrastructure  *(the first work; blocks everything)*
**Goal:** a greenfield repo that any contributor (or web/CI agent) can clone and get a green build, tests,
lints, and Lean check on x86-64 and AArch64, with the dual-backend layout already in place.
**SPEC:** Appendix F (anti-patterns to encode as CI checks), §34 (test categories to scaffold), §35
(deployment/ABI to anticipate in layout).
**Depends on:** nothing. **Enables:** all.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W0-1 | Ratify D3–D7 (build orchestration, page/`small_max`, license, arena routing, test stack). Record outcomes here. | S | | §4.2 table updated with decisions; no `TBD` left for D5 before any `/sele4n` file exists. |
| W0-2 | Repo layout (below) created with `README` stubs per top dir explaining its contract. | S | ∥ | `tree -L 2` matches §6 W0 layout; each dir has a one-paragraph charter. |
| W0-3 | Toolchain pinning: `rust-toolchain.toml`, `lean-toolchain`/`elan`, pinned `nightly` only if an unstable feature is justified and recorded. | S | ∥ | Fresh clone + `xtask setup` installs exact toolchains; versions committed. |
| W0-4 | Build orchestration per D3: Cargo workspace + `cargo xtask` driving Lean (`lake`), codegen, cross builds. | M | | `cargo xtask build` builds Rust+Lean; `--target aarch64` cross-builds. |
| W0-5 | CI pipeline (GitHub Actions): matrix {x86-64, aarch64-via-cross/QEMU} × {debug, performance}; jobs = build, unit, lint, `lake exe check`, doc build; cache toolchains. | M | | PR cannot merge unless all jobs green; status checks required on the branch. |
| W0-6 | Formatting + linting gates: `rustfmt`, `clippy -D warnings`, `markdownlint` for `/planning` + `/docs`, Lean style check. | S | ∥ | CI fails on any lint; `cargo xtask fmt --check` reproduces locally. |
| W0-7 | Test harness skeletons: unit (per crate), property (`proptest`, D7), differential (trace replay stub), fuzz target stub (`cargo-fuzz`). | M | ∥ | `cargo xtask test` runs all suites (even if mostly empty); one example test per kind passes. |
| W0-8 | Benchmark harness skeleton (`criterion` micro-bench + a placeholder workload-replay driver per §34.6). | S | ∥ | `cargo xtask bench` runs and reports; not gating. |
| W0-9 | **SessionStart hook for Claude-on-web** so web sessions auto-build/test the project (use the `session-start-hook` skill). | S | ∥ | `.claude/` hook present; a fresh web session can run `xtask ci` without manual setup. |
| W0-10 | Coding standards + invariant conventions doc: transition-tagging (`// M-003: sc immutable`), `assert!`/`debug_assert!` gating by profile, error-type taxonomy. | S | ∥ | `docs/CONVENTIONS.md` exists; a lint or review checklist references it. |
| W0-11 | Governance: `CONTRIBUTING.md`, `CODEOWNERS`, PR/issue templates embedding the Appendix-F checklist + DoD (§9), `SECURITY.md`. | S | ∥ | Opening a PR shows the checklist; CODEOWNERS routes `/lean`, `/sele4n`, `/arch`. |
| W0-12 | License resolution per D5: keep core MIT; add `/sele4n/LICENSE` (seLe4n-compatible) + top-level `NOTICE` explaining the split; SPDX headers policy. | S | ∥ | `NOTICE` present; CI checks SPDX headers on new files. |
| W0-13 | Versioning + ABI-series policy doc (semver; stats-JSON additive rule §35.3); `topomalloc_version` constant wired to stats. | S | ∥ | `docs/ABI.md` defines the series; version surfaced in stats JSON (Appendix D). |
| W0-14 | **Walking skeleton (M0 exit):** library crate exporting stub `malloc`/`free` over `TopoBackingProvider`; `lake` package with empty `Bridge.lean`; trace emit prints one line. | M | | M0 exit criteria met end to end on both arches. |

**Proposed repository layout** (charter per directory):

```text
/Cargo.toml                  workspace
/rust-toolchain.toml lean-toolchain
/xtask/                      build/codegen/CI driver (D3)
/crates/
  topo-core/                 allocator core: classifier, spans, central list, front/middle-end (no_std-capable)
  topo-abi/                  C ABI exports + Rust GlobalAlloc adapter (W8)
  topo-backend-posix/        PosixBackingProvider (W4)
  topo-backend-sele4n/       Sele4nBackingProvider + Sele4nSim (W22)
  topo-arch/                 per-arch asm: RSEQ x86-64/aarch64, restartable sections (W7)
  topo-stats/                stats/profiling/explain (W17)
  topo-control/              config + control namespace (W20)
  topo-test-support/         trace grammar, deterministic harness, property generators
/lean/
  TopoMalloc/                model: Types, State, WellFormed, SizeClass, Rseq, Theorems (W1)
  TopoMalloc/SeLe4n/         bridge package (§36.3.3): Bridge, CapBackedArena, *Provider, ResourceServer,...
  lakefile.lean lean-toolchain
/tools/
  size-class-gen/            THE size-class generator (single source of truth, W1+W2)
  trace-replay/              executable model replay / differential runner
/include/                    generated C headers (topomalloc.h, topomalloc_sele4n.h)
/sele4n/                     resource-server component + adapters (allocman/VKA/VSpace), separate LICENSE
/bench/                      workloads, drivers, results schema
/tests/                      cross-crate integration, ABI, concurrency, fork, fuzz corpora
/docs/                       CONVENTIONS, ABI, deployment, profiles, mdbook site
/profiles/                   profile definitions/feature wiring
/ci/                         CI helper scripts
/planning/                   SPEC.md, this plan
/.claude/                    SessionStart hook (W0-9)
```

---

### W1 — Formal core & Lean model (+ seLe4n bridge)  *(continuous; co-developed from M0)*
**Goal:** an executable, machine-checked abstract allocator and its seLe4n bridge, providing (a) the
generated/verified tables consumed by the implementation, (b) the trace-replay oracle for differential
testing, and (c) the proofs that make Core/Formal/Microkernel conformance real.
**SPEC:** §33 (whole), §9.5, §21.6, §22.5/§22.6, §33.5, §36.3.3, §36.12, §36.17; V-001..V-005.
**Depends on:** W0. **Enables:** W2 (tables), W5/W6 (conservation theorems), W21 (oracle), W22 (bridge).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W1-1 | Lake package + CI `lake exe check`; core types (§33.2): `Owner`, `Range`, `State`. | S | | `Owner` includes all SPEC owners incl. `released`/`quarantine`; compiles in CI. |
| W1-2 | **Empty bridge package** scaffolding (§36.3.3) so seLe4n is co-developed from day one. | S | ∥ | `TopoMalloc/SeLe4n/*.lean` files compile (stubs); importable without seLe4n internals. |
| W1-3 | `WellFormed` predicate (§33.3): single-owner, live-disjointness, cache/central residency, span-fit, bitmap agreement, pagemap agreement, hugepage occupancy, arena uniqueness, released-no-live, capacity. | M | | predicate total; each clause cross-referenced to a SPEC bullet. |
| W1-4a | Abstract `SizeClass` record + the *parameterized* table-construction function in Lean (size, alignment, slab_pages, objects_per_slab, batch_size, max_local_capacity) — a pure function of the build params (D4), no literals. | M | | table derives entirely from params; reproducible. |
| W1-4b | Prove the **spacing-dominated** bound `r(c)=size(c)/size(prev(c)) ≤ 1+W` for each §9.4 size range. | M | ∥ | per-range spacing lemma discharged. |
| W1-4c | Prove the **alignment-dominated** caveat (`req<q/W` ⇒ waste ≤ `(q-1)/req`) and classify each class into its regime, so no unattainable flat target is claimed (§9.4). | S | ∥ | regime split machine-checked. |
| W1-4d | Prove the layout lemmas (§9.5): `size(c)` is an integer multiple of `alignment(c)`; objects fit the span; object ranges pairwise-disjoint. | M | | layout lemmas proved. |
| W1-4e | Prove the lookup obligations (total, monotonic, in-bounds, `size≥req`, `align≥req.align`, `batch≤max_local_capacity`) and **emit** the concrete table as machine-readable data + define the golden-diff contract consumed by W2-1. | M | | `size_class_table_covers_all_small_requests` proved; emitted table is the single source of truth. |
| W1-5 | Abstract transitions: `malloc`, `free`, `central_batch_remove/insert`, `cache_refill/flush`. | M | | each transition is a total function on `State`. |
| W1-6a | `malloc_preserves_wellformed` + `malloc_success_returns_aligned_sufficient_disjoint_object`. | M | | both proved, named per §33.4. |
| W1-6b | `free_preserves_wellformed_for_valid_pointer` + `free_removes_liveness_and_adds_exactly_one_free_owner`. | M | ∥ | both proved. |
| W1-6c | `cache_refill_preserves_ownership_conservation` + `cache_flush_preserves_ownership_conservation` (consumes the W1-7 frame condition). | M | | both proved; depend on W1-7. |
| W1-7 | RSEQ abstraction (§33.5): `RseqPop`/`RseqPush` with **abort / empty(full) / success** + **frame condition**; axioms stated. | M | ∥ | distinct abort vs empty/full cases; frame condition present; refill/flush proofs depend on it. |
| W1-8a | `span_split_preserves_disjointness` + `span_merge_preserves_disjointness`. | M | | both proved; mirror W4-2b/c. |
| W1-8b | `pagemap_lookup_sound` (mirrors W3-3d). | M | ∥ | proved. |
| W1-8c | `release_to_os_preserves_live_objects` — the §21.6 release-safety theorem (every pointer live before stays live and committed). | M | | proved; mirrors W4-2d/W12-2. |
| W1-9 | Arena theorems: `arena_reset_invalidates_only_target_arena`, `arena_destroy_preserves_other_arenas`. | M | | proved. |
| W1-10 | Executable model + trace replay (§33.7): consume SPEC trace grammar, check `WellFormed` at boundaries. | M | | replays a recorded trace; flags an injected violation. |
| W1-11a | Bridge relation `TopoState ↔ SeLe4n SystemState` + abstraction function (§36.3.3, §36.7). | M | | relation + abstraction compile; importable without seLe4n internals. |
| W1-11b | Backing-provider state machine as a Lean transition system (§36.6 ordering: `AuthorizedUntyped → … → RecyclableUntyped`). | M | ∥ | transitions match §36.6 exactly. |
| W1-11c | Single-label `TopoSeLe4nWellFormed` predicate composing TopoMalloc `WellFormed` with the seLe4n invariant bundle. | M | | predicate total; clauses cross-referenced to §36.7/§36.12. |
| W1-12a | Authority/quota family: `arena_cap_authorizes_alloc`, `arena_quota_preserved`, `client_cache_refines_server_authority`. | M | | proved single-core. |
| W1-12b | Provenance/release family: `backing_descends_from_untyped`, `no_live_object_released`. | M | ∥ | proved single-core. |
| W1-12c | Destroy/label/scrub family: `destroy_revokes_descendants`, `label_partition_preserved`, `scrub_before_downgrade`. | M | | proved single-core; mirror W9-6/W18-6. |
| W1-12d | Per-core/stats/composite family: `per_core_cache_abort_no_change`, `stats_observation_noninterference`, `topo_step_preserves_sele4n_invariants`. | M | ∥ | proved single-core; gate M7/M8. |
| W1-13 | Non-interference (§36.12): `topo_step_preserves_low_equivalence` shape proved for the cache/stats steps. | L | ∥ | proved for the modeled steps. |
| W1-14 | SMP/per-core bridge extensions (§36.17 SMP forms) — staged per V-004. | L | | proved or recorded as explicit refinement debt by M9. |

> **▸ Decomposition — W1-4 (size-class model), the M1 longest pole.** The single-source-of-truth table
> (Appendix F: never hand-maintained) is split so each sub-proof ships independently. The reason it is split
> into a spacing regime (W1-4b) and an alignment regime (W1-4c) is that the SPEC (§9.4) proves a flat per-
> request waste target is *unattainable* below the ABI quantum `q`: a 17 B request under a 16 B quantum must
> round to 32 B. W1-4b targets the achievable `r(c) ≤ 1+W`; W1-4c bounds the small-request tail by `q`. The
> emitted table (W1-4e) is data, consumed by W2-1 and golden-diffed in CI (G-table). **Pitfall:** keeping
> alignment-multiple (W1-4d) and lookup (W1-4e) as separate proofs lets a generator change re-check cheaply
> rather than re-proving the whole table. **Sequencing:** W1-4a starts in M0 alongside W0; 4b/4c run in
> parallel; 4d/4e gate W2 and therefore M1.

---

### W2 — Size classes, alignment & request classification
**Goal:** the runtime classifier and generated tables, provably consistent with W1.
**SPEC:** §9 (whole), §25.5 (over-aligned routing), §A.1 (`classify`), §9.7 (overflow).
**Depends on:** W1-4. **Enables:** W5, W6, W8, W15, W18.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W2-1 | `tools/size-class-gen` emits Rust tables + `include/` constants from the **same** model as W1-4; build fails if generator output diverges from a checked-in golden that Lean verifies. | M | | regenerate-and-diff is a CI gate; no hand-edited tables anywhere. |
| W2-2 | `size_class(size, align)` lookup (fast, branch-light) + `usable_size(sc)`. | M | ∥ | matches Lean lookup on exhaustive small-range test. |
| W2-3 | `classify(size, align, flags)` (§A.1) producing small/medium/large + arena + label + hints. | M | | unit + property tests; over-aligned requests route per §25.5, never offset-adjusted in a shared slab (§9.3). |
| W2-4 | Overflow-safe rounding (§9.7): size-class, page, hugepage, alignment rounding all checked; integrates with calloc (§26.1). | S | | overflow tests return null/`bad_alloc`, never wrap (W15, W8). |

---

### W3 — Metadata, pagemap, bootstrap allocator & pointer classification
**Goal:** the metadata substrate every other layer reads: bootstrap-safe metadata, the pagemap, span
descriptors, and pointer classification.
**SPEC:** §16.2 (span descriptor), §17 (whole), §27.5 (generation/ABA), P-Map-001..006, S-007.
**Depends on:** W0 — **W3-1 (bootstrap allocator) needs only W0 and starts in M0**, off the W1-4 critical
path; W2 (`sc`) is needed only from W3-2 (span descriptor) onward. **Enables:** W4, W5, W8, W15, W18.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W3-1 | **Bootstrap metadata allocator** (§17.4, S-007): no dependency on public malloc, monotonic, idempotent init, lock-free before threading. | M | | unit test allocates metadata before any arena exists; never calls global `malloc`. |
| W3-2 | Span descriptor (§16.2) incl. `object_count/live_count/central_free_count`, `free_bitmap`, optional `cache_bitmap`, `generation`. | M | ∥ | fields derivable to the §16.4 conservation law; sizes asserted. |
| W3-3a | Pagemap data structure: multi-level radix keyed by allocator-page, O(1) lookup, full address-range coverage; level count chosen for the page size (D4). | L | | lookup is O(1); metadata overhead bounded + documented. |
| W3-3b | Entry encoding: small (arena/span/sc), large (descriptor), released-retained, external sentinel (P-Map-002/004/005). | M | ∥ | every allocator page maps to exactly one descriptor; non-owned → sentinel. |
| W3-3c | Concurrent install/update synchronized to span lifecycle (P-Map-006): release-store publish / acquire-load read; generation-guarded so a descriptor is never freed while a classifier may read it (with W3-5, §27.5). | L | | no unsynchronized update (Appendix F); ABA-safe. |
| W3-3d | Lookup-soundness tests (P-Map-001..006) + differential check vs Lean `pagemap_lookup_sound` (W1-8b). | M | | passes; divergence fails CI. |
| W3-4 | Pointer classification (§17.5): Null/Small/Large/Interior/Metadata/Released/Quarantined/External. | M | | interior & foreign pointers detected in debug/hardened; base-pointer-only frees enforced. |
| W3-5 | Generation counters + stale-descriptor protection (§27.5, §16.6). | S | ∥ | reused span bumps generation; debug catches stale ref. |
| W3-6 | Pagemap↔span synchronization protocol (P-Map-006) used by split/merge (W4) and span lifecycle (W5). | M | | no unsynchronized pagemap update (Appendix F); lock-order respected. |

> **▸ Decomposition — W3-3 (pagemap).** The radix shape is the key design call: a 2- or 3-level radix over
> allocator-page numbers gives O(1) lookup with bounded, lazily-populated overhead, where a flat array would
> waste virtual space on 64-bit. The subtle correctness pieces are (P-Map-005) released-but-retained pages
> keeping enough metadata to forbid reuse without recommit, and (P-Map-006 / §27.5) publishing entries with
> release/acquire ordering plus a generation so a concurrent `free`-classification can never follow a stale
> pointer to a recycled descriptor. **Pitfall:** updating the pagemap and the span state in *different*
> critical sections is the classic use-after-free in pointer classification — W3-6 owns that single protocol
> and W4-2b/W5-5 must both call through it.

---

### W4 — Back-end seam & POSIX provider
**Goal:** define the `TopoBackingProvider` seam (§3) and ship the POSIX implementation; this is the
degenerate single-authority case that the seLe4n provider (W22) generalizes.
**SPEC:** §18 (whole), §20 (dirty/muzzy/retained/released), §23 (hooks shape), §36.6 (the contract the seam
must also satisfy), M-004/M-005.
**Depends on:** W0 for **W4-1 (the trait + stubs is an M0 deliverable and must not depend on the pagemap)**;
W3 for **W4-2 onward** (extent ops install/update pagemap entries). **Enables:** W5, W9, W11, W22.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W4-1 | **Define `TopoBackingProvider`** (§3 box ≅ §36.6) with POSIX + `Sele4nSim` stub impls; provider-state machine type (§36.6 ordering). The seLe4n impl skeleton compiles against the real `sele4n-abi`/`sele4n-types` (D8). | M | | trait compiles; M0 skeleton runs over either provider; seLe4n side type-checks against the pinned upstream crates. |
| W4-2a | Extent descriptor (§18.2) + free-extent index (by size and by address) to support coalescing. | M | | index supports best/first-fit + neighbor lookup. |
| W4-2b | `alloc` + `split` (§18.4): both results page-aligned; metadata installed *before* publication; pagemap update atomic wrt readers (via W3-6). | M | | split rules enforced; no torn publish. |
| W4-2c | `merge`/coalesce (§18.4): adjacency, arena-compatibility, state-compatibility, hugepage-accounting update, no stale descriptors visible to classification. | M | ∥ | merge rules enforced; mirrors W1-8a. |
| W4-2d | `commit`/`decommit`/`purge_lazy`/`purge_forced`/`release` with pre/postconditions mirrored in Lean (W1-8c); enforces M-004 (no live in released) and M-005 (recommit before use). | M | | preconditions checked; failure leaves state well-formed (W4-5). |
| W4-3 | POSIX physical-state mapping (§20.4): `madvise`/`mprotect` for dirty/muzzy/released; retain-vs-unmap policy (§20.5). | M | ∥ | platform mapping documented; states reconcile in stats. |
| W4-4 | Large allocation path (§18.5) + region cache hook point (§18.6, filled by W11). | M | | large allocs bypass small caches; round-overflow safe. |
| W4-5 | Backend failure semantics: every op fallible, leaves state well-formed (mirrors §36.6 "failure leaves state well-formed"). | S | | failure-injection test keeps invariants green. |

---

### W5 — Spans, slabs, bitmaps & central free lists
**Goal:** the span/slab layer and the central free list — including the hardest accounting problem in the
allocator, empty-span detection across caches.
**SPEC:** §16 (whole), §14 (middle-end/central), §A.4, C-001..C-005, B.3.
**Depends on:** W3, W4. **Enables:** W6, W11.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W5-1 | Slab layout (§16.3): `base0 + i*sc.size`, header/bitmap non-overlap, alignment from `size`-multiple-of-`alignment`. | M | | object ranges fit & disjoint (proved-in-Lean shape mirrored by tests). |
| W5-2 | Free bitmap + `central_free_count = popcount` (§16.4); update count and bitmap in one critical section (§8.5). | M | | invariant test; no torn updates. |
| W5-3a | Encode the five-term partition `object_count = live + local_cached + transfer_cached + central_free + quarantined` (§16.4) as the span accounting model; mark which terms are exact vs reconstructed. | M | | partition documented; no term double-counts. |
| W5-3b | `central_free_count == popcount(free_bitmap)` maintained atomically with the bitmap (with W5-2) — the authoritative *central-residency* invariant. | M | | invariant holds under all central transitions. |
| W5-3c | Debug-exact reconstruction of `local_cached`/`transfer_cached` (via `cache_bitmap` or a cache scan) so the conservation law holds *exactly* in debug builds. | M | ∥ | reconstructed counts match observed cache contents. |
| W5-3d | `span_is_empty(span)`: all four non-central terms zero AND `central_free == object_count`; never reads a cached free object as live (§8.4/§16.5). | M | | predicate correct; B.3 empty-detection check passes. |
| W5-3e | Empty-detection trigger protocol: re-evaluate on central insert (flush) and on cache drain so a newly-emptied span is detected and returned, never stranded. | M | | a span emptied only by the *last* cache flush is detected + returned (test). |
| W5-4a | Central structure keyed `(node, arena, label, sc)` (§14.5, D2): partial-span list + empty-span cache + span occupancy counters. | M | | C-001/C-002 hold by construction. |
| W5-4b | `remove_batch` (§A.4 / A.2 loop): pull from partial spans, activate an empty/backend span on demand, carve a batch, update counts; return empty so the caller can request a new span and retry. | M | | batch single-arena/label, distinct, correct-size; OOM-retry path exercised. |
| W5-4c | `insert_batch`: return objects, update bitmap+count atomically (W5-3b), run empty-detection (W5-3e) on affected spans. | M | | C-003/C-004: empty detected, non-empty never returned. |
| W5-4d | Locking/sharding (§14.5) under the lock hierarchy (W16-1); per-`(node,sc)` shards to cut contention. | M | ∥ | no lock-order violation; contention measured. |
| W5-5 | Span activation/return-to-backend (§14.6 C-003/C-005) with lock-order discipline (§27.2). | M | | empty spans returned; non-empty never returned; no lock-order violation. |

> **▸ Decomposition — W5-3 (empty-span detection), the hardest accounting in the allocator (§16.5).** The
> difficulty: an object cached in a per-CPU/thread/transfer cache is *free* but invisible to its span — its
> bitmap bit is 0, exactly like a live object's. So liveness cannot be read off the bitmap, and a span is
> empty only once *every* cache has also released its objects. Chosen strategy: keep
> `free_bitmap`/`central_free_count` authoritative for *central* residency (cheap, exact, hot-path) and
> reconstruct `local_cached`/`transfer_cached` only in debug (W5-3c), where the full conservation law is
> checked exactly; performance builds therefore detect emptiness *eventually*, at the W5-3e trigger points
> (central insert + cache drain), instead of paying a per-span counter on every cache push/pop.
> **Two failure modes, both tested:** (1) a *leak* — a truly-empty span is never re-checked and never
> returned; (2) far worse, a span declared empty while a cache still holds one of its objects, which would let
> the backend recycle *live* memory. W5-3d/3e + the B.3 debug check + differential tests (W21-2) guard both.
> **Sequencing:** there are no caches to account for until M2, so land W5-3a/b/d at M1 (central-only) and
> complete W5-3c/3e in M2 when caches arrive.

---

### W6 — Front-end & middle-end caches (portable)
**Goal:** the cache machinery (thread cache, then per-CPU locked) and transfer caches, all label/arena-aware
— correct *before* RSEQ (W7) makes it fast.
**SPEC:** §11, §13, §14.2–§14.4, §11.7 (arena routing), §11.5/§11.6 (capacity/idle), B.2.
**Depends on:** W5. **Enables:** W7, W8 (fast path), W12 (drain).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W6-1 | Thread cache fallback (§13): per-`(arena,sc,label)` lists; GC on exit/pressure/budget (§13.3); arena-reset drain precondition (§13.4). | M | | thread-exit flush; budget bounded (B.2). |
| W6-2 | Transfer cache `(domain, arena, sc, label)` with `Batch` (§14.2); distinct/correct/free guarantee. | M | ∥ | batch invariants tested. |
| W6-3a | Refill (§14.3): try transfer-cache batch → central batch (W5-4b) → new span; push the batch to the cpu/thread cache. **Hand-over-hand:** release the transfer lock before taking the central lock. | M | | never holds two middle-end locks; conservation matches W1-6c. |
| W6-3b | Flush (§14.4): pop a batch from the cache → transfer cache if it has capacity, else central (W5-4c); same hand-over-hand discipline. | M | ∥ | lock-order checker clean; conservation matches W1-6c. |
| W6-3c | Wire empty-span detection (W5-3e) into flush-to-central; debug conservation check (B.2). | S | | a flush that empties a span triggers detection. |
| W6-4 | Per-CPU cache structure + **locked** per-CPU mode (§11.2–§11.5) as the RSEQ-free correct baseline. | M | | hard-capacity invariant (§11.5) holds; ready as RSEQ fallback. |
| W6-5 | Cache budget controller v1 (§11.5, P-005): adapt to miss/overflow counts; global budget + per-CPU soft/hard. | M | ∥ | budget honored; stats expose miss/overflow. |
| W6-6 | Arena routing per D6 (§11.7): bound-arena fast path now; arena-qualified `(cpu,arena,sc)` slots wired for M4. | M | | free always returns to owning arena's structures (safety, §11.7); alloc from A returns only A's objects. |
| W6-7 | Idle-CPU/affinity-change flush (§11.6) + control to release stranded caches. | S | ∥ | flushing an idle CPU moves objects to transfer/central; control plane hook present. |

---

### W7 — RSEQ / restartable fast paths & per-arch assembly
**Goal:** the lock-free common path: Linux RSEQ for the POSIX profile and a pinned-core/restartable contract
for seLe4n (§36.10), behind one front-end interface with a proven abort case.
**SPEC:** §11.3/§11.4, §12 (whole), §27.4, §34.5, §36.10; P-001..P-003.
**Depends on:** W6-4, W1-7. **Enables:** M3.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W7-1 | RSEQ registration + availability detection per thread (§12.3); initial-exec TLS interplay (§27.6, see W16-2). | M | | registered on Linux; clean fallback when absent (P-003). |
| W7-2a | RSEQ critical-section descriptors + abort-handler trampolines in a dedicated section; register the rseq area per thread. | M | | cs table present; abort vector wired. |
| W7-2b | x86-64 `rseq_pop` (load cpu → load head → single committing store); abort ⇒ logical no-op. | M | | pops one or reports empty; abort unchanged. |
| W7-2c | x86-64 `rseq_push` (capacity check → store → commit index); abort ⇒ no-op. | M | ∥ | pushes one or reports full; abort unchanged. |
| W7-2d | Clobber/barrier docs + compiler-fence discipline; audit/lint that no call or faulting ref occurs inside a CS (§12.3). | S | | documented; lint passes. |
| W7-2e | x86-64 equivalence vs locked mode under forced migration (feeds W7-6). | M | | no lost/duplicated object vs locked (G-fast). |
| W7-3a | AArch64 `rseq_pop`/`rseq_push` + abort handler (commit-store model); shares the arch with the seLe4n RPi5 target. | L | ∥ | pop/push correct; abort unchanged. |
| W7-3b | AArch64 clobber/barrier docs + no-call/no-fault-in-CS audit. | S | ∥ | documented; lint passes. |
| W7-3c | AArch64 equivalence vs locked under forced migration (QEMU). | M | ∥ | matches locked (G-fast). |
| W7-4 | Non-owner coordination (§27.4): flushing an idle CPU vs owner RSEQ (epoch/stop-the-world/per-CPU lock). | M | | concurrent flush-vs-fastpath stress clean. |
| W7-5 | seLe4n pinned-thread per-core mode (§36.10 option 1) behind the same front-end contract; abort/no-change case. | M | | migration flush/hand-off correct; `per_core_cache_abort_no_change` mirrored in tests. |
| W7-6 | RSEQ test battery (§34.5): migration, signal near sequence, preemption, registration failure, compare-vs-locked. | M | | all pass in CI (QEMU where needed). |

> **▸ Decomposition — W7-2/W7-3 (per-arch restartable sequences).** This is the only hand-written assembly in
> the allocator and its highest-risk performance code, so each sequence is its own reviewable unit with its
> own equivalence test. Non-negotiable rules (§12.3): the critical section contains no calls and no
> possibly-faulting memory reference; the abort handler restores a logical no-op; the only state-changing
> instruction is the single commit store at the end, so an abort before commit is invisible. Each sequence is
> validated *two* ways — the Lean RSEQ contract (W1-7) with its abort/empty/success + frame condition, and a
> forced-migration differential test against the locked baseline (W6-4) that must show identical object
> movement. **Why pop/push/abort are separate units:** they fail differently (pop underflows, push overflows,
> abort retries) and the SPEC requires these be *distinct* outcomes, never conflated (§33.5).

---

### W8 — Public API & ABI (C and C++ and Rust)
**Goal:** the standards-correct public surface, including errno/C23 semantics and C++ operators, exported
from the Rust core.
**SPEC:** §10 (whole), §5.1 (F-001..F-009), §9.6 (zero-size), §25 (realloc/aligned cross-ref), §35.2/§35.3.
**Depends on:** W2, W3, W5 — **the M1 public path allocates/frees through the central list under the global
lock; it does *not* depend on W6.** W6 is wired in as the per-CPU fast path at M2 (W16-4 handles the
global-lock→fast-path transition). **Enables:** every consumer + tests.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W8-1 | C core: `malloc/free/calloc/realloc` (§10.1) with `errno=ENOMEM` on failure, `free` preserves `errno`, realloc-failure preserves original (§10.1). | M | | ABI tests; errno semantics tested. |
| W8-2 | Aligned/POSIX: `posix_memalign`, `aligned_alloc`, `memalign`, `malloc_usable_size` (F-003/F-008). | M | ∥ | alignment + usable-size tests; never silently ignore alignment (§10.4). |
| W8-3 | C23: `free_sized`, `free_aligned_sized`, `reallocarray` (overflow-checked); sized-delete-style hint use + sample-check mismatch (§10.1). | S | ∥ | mismatched size sample-checked in hardened/debug. |
| W8-4 | Zero-size policy (§9.6/F-004): `compat.zero_unique`/`zero_null`, `free(NULL)` no-op, documented + configurable. | S | ∥ | behavior consistent and configurable; documented. |
| W8-5 | C++ operators (§10.2): new/new[]/aligned/sized delete overloads; map errors to `bad_alloc` per API. | M | | C++ link test; sized delete used when valid. |
| W8-6 | Extended C API (§10.3/§10.4): `topo_mallocx/rallocx/xallocx/dallocx/sdallocx/nallocx` + flags; validate flag combos. | M | ∥ | invalid combos fail deterministically; mandatory flags honored. |
| W8-7 | Rust `GlobalAlloc`/allocator-api adapter (D1). | S | ∥ | a Rust test program uses TopoMalloc as `#[global_allocator]`. |
| W8-8 | Generated `include/topomalloc.h` + a header/ABI compatibility test. | S | ∥ | header compiles under C and C++; ABI test pins struct/opaque-handle layout (§35.3). |

---

### W9 — Arena policy & authority domains (capability-backed)
**Goal:** arenas as *both* policy domains *and* capability-controlled resource domains (D2) — the SPEC's
§22 and §36.4 unified.
**SPEC:** §22 (whole), §36.4 (cap-backed arenas + monotonicity invariants), §36.13 (revocation), §15.5
(NUMA policy), F-005/F-006.
**Depends on:** W4 (provider), W5 (central per arena), W6 (cache routing). **Enables:** M4, W10, W22.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W9-1 | Arena descriptor (§22.2) extended with `authority_cap`, `label`, `quota` (§36.4); trivial ambient values on POSIX. | M | | POSIX default arena works; fields present for seLe4n. |
| W9-2 | Arena states + lifecycle (§22.3): Initializing/Active/Draining/Resetting/Destroyed; allocations only in Active. | M | | illegal-state ops rejected; tested. |
| W9-3 | Create/configure (§22.4, F-005/F-006): validate policy, metadata from safe arena, hooks installed before first extent, publish id only after init. | M | | creation order enforced; default-arena policy (F-006) covers no-extended-API programs. |
| W9-4a | State transitions Active→Resetting/Draining + precondition checks (no active allocators; explicit, not the default arena unless special mode) (§22.5). | M | | illegal reset rejected; tested. |
| W9-4b | Cache drain/invalidate of *every* per-CPU/thread/transfer cache holding the arena's objects (uses W6 routing) — the hard part. | M | | post-drain: no cache holds an arena object (B.5). |
| W9-4c | Return arena extents to backend/retain per policy; reset accounting; bump reset generation. | M | ∥ | §22.5 postconditions met. |
| W9-4d | Destroy = reset + metadata removal + id non-reuse-while-stale (§22.6); isolation preserved (§22.7); mirrors W1-9. | M | | `arena_destroy` tests; isolation invariant. |
| W9-5 | **Capability monotonicity** (§36.4): authority/quota/label monotonic on delegation; attenuation-only. | M | | delegation cannot widen rights/quota or downgrade label; tested (§36.16 quota/authority/label). |
| W9-6a | Enter DRAINING: reject new allocations + new delegations; notify participating clients (§36.13). | M | | no new alloc/delegation accepted while draining. |
| W9-6b | Drain local/transfer caches + central lists; quarantine or reject stale frees. | M | | post-drain inventory empty (shares W9-4b). |
| W9-6c | Unmap client VSpace windows; scrub dirty pages if cross-label reuse is possible (uses W18-6). | M | ∥ | unmapped before revoke; scrub recorded. |
| W9-6d | Revoke derived frame/mapping caps; delete CSlots; recycle untyped to free pools (provider `revoke_descendants`/`recycle_untyped`). | M | | no live derived cap/mapping remains. |
| W9-6e | Finalize DESTROYED + generation++; **partial failure ⇒ DRAINING/ERROR_QUARANTINED, never DESTROYED**; emergency allocs never depend on a destroying arena. | M | | revocation test (§36.16); mirrors `destroy_revokes_descendants` (W1-12c). |
| W9-7 | NUMA policy modes (§15.5) + binding-failure visibility in stats. | M | ∥ | local/interleave/bind/arena_policy/OS_default supported; failures surfaced. |

> **▸ Decomposition — W9-6 (arena revocation), the seLe4n-critical lifecycle.** Ordering is the whole game:
> the protocol must *unmap before revoke before recycle*, because recycling untyped backing while a client
> mapping or derived capability still exists would hand live authority to another security domain. Each step
> is its own unit so a partial failure stops cleanly — the arena lands in DRAINING/ERROR_QUARANTINED, never
> DESTROYED, and never with a half-revoked CSpace. The scrub step (W9-6c → W18-6) is what makes cross-label
> reuse safe (§36.12) and is skippable only when the reused-at label is ≥ the old label. **Pitfall:** draining
> *all* caches (W9-6b, shared with W9-4b) is the same hard search as empty-span detection — an object can hide
> in any per-CPU/thread/transfer cache; bound-arena routing (D6) plus arena-qualified slots (M4) make it
> tractable. Mirrors Lean `destroy_revokes_descendants` (W1-12c); on the M4 G-arena gate.

---

### W10 — Extent hooks & custom backing
**Goal:** application-supplied backing/policy with explicit contracts and failure safety.
**SPEC:** §23 (whole), §34.8 (hook fuzzing).
**Depends on:** W4, W9. **Enables:** M4.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W10-1 | Hook interface (§23.2) wired through the provider seam. | M | | alloc/dealloc/commit/decommit/purge/split/merge dispatch to user hooks. |
| W10-2 | Hook contracts (§23.3) enforced/validated (alignment, size, no-overlap, subrange-only, no undocumented reentrancy). | M | | contract violations detected in debug; documented assumptions in Lean (§23.4). |
| W10-3 | Failure-injection tests (§34.8): every hook can fail; allocator stays well-formed. | M | ∥ | fuzzed hook failures never corrupt state. |

---

### W11 — Hugepage / large-mapping backend
**Goal:** Temeraire-style packing on POSIX and the generalized large-mapping policy on seLe4n (§36.9),
behind the provider seam.
**SPEC:** §19 (whole), §18.6 (region cache), §36.9, H-001..H-005, B.4.
**Depends on:** W4, W5. **Enables:** M5.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W11-1 | HugeAllocator + HugeCache (§19.2). | M | | hugepage-aligned reservations; empty-backed cache reuse. |
| W11-2a | Bin set (§19.4: empty_backed/nearly_empty/sparse/medium/nearly_full/full/partial_subreleased/cold_sparse/hot_dense) as the filler structure; each hugepage in exactly one bin; bin transitions on occupancy change. | M | | H-003: bin membership consistent with occupancy. |
| W11-2b | Candidate selection/scoring (§19.3): approximate-bin scan; packing/locality/lifetime/hotness/release-preservation bonuses minus fragmentation/cross-numa/partial-subrelease penalties. | M | | no full scan of all hugepages; deterministic in test mode. |
| W11-2c | Bin↔occupancy consistency invariant + tests (H-002/H-003): occupancy bytes equal the sum of contained spans/large allocs. | M | ∥ | invariants checked in debug (B.4). |
| W11-3 | RegionCache for awkward sizes (§18.6). | M | ∥ | slightly-larger-than-hugepage allocs avoid full-hugepage rounding waste. |
| W11-4 | Packing policy (§19.5) + partial-subrelease guards (§19.6/H-005): never intersect a live object; pressure/coldness gated. | M | | partial subrelease only when allowed; recorded as metric. |
| W11-5 | Coverage metrics (§19.7) exported to stats (W17). | S | ∥ | all §19.7 fields present; `coverage_ratio` computed. |
| W11-6 | seLe4n large-mapping policy (§36.9): same placement over contiguous normal-frame runs; prefer whole-mapping release. | M | | correct when every backing range is normal pages; Sim test. |

> **▸ Decomposition — W11-2 (hugepage filler).** Splitting *bins* (W11-2a) from *scoring* (W11-2b) matters
> because the bins are the correctness object (H-003: exactly one bin, consistent with occupancy) while the
> score is pure policy (§2.4: a wrong score may hurt fragmentation but must never misplace a live object).
> Bins also let the filler avoid scanning every hugepage (§19.3 "approximate bins"). The seLe4n large-mapping
> policy (W11-6) reuses the same placement model over contiguous normal-frame runs, so keep scoring inputs
> backend-agnostic and never assume a hardware hugepage exists.

---

### W12 — Memory release controller & background purging
**Goal:** decide when memory returns to the backing provider, balancing RSS/faults/coverage/latency.
**SPEC:** §20.2/§20.3 (decay/background), §21 (whole), §36.11 (latency classes), O-007 (emergency).
**Depends on:** W4, W6, W11. **Enables:** M5.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W12-1 | Decay config per arena (§20.2) + background purge worker (§20.3): off hot paths, fair, yields under CPU pressure. | M | | no purging on the allocation fast path; backlog in stats. |
| W12-2a | Inputs collection (§21.2): the observation vector (live/rss/dirty/muzzy/coverage, alloc/free/refill rates, cgroup current/max, pressure notifications, NUMA pressure, hints). | M | | all §21.2 inputs sampled cheaply. |
| W12-2b | Release priority ladder (§21.3/§A.5): drain idle caches → release empty hugepages → purge dirty (not hot) → dirty→muzzy → subrelease cold-sparse → emergency shrink. | M | | ladder applied in order; each step gated by pressure mode (§21.5). |
| W12-2c | Demand reserve (§21.4) + anti-oscillation: reserve = f(recent rate, peak, refill latency, pressure); prevents release-then-refault. | M | ∥ | refault-loop oscillation test passes. |
| W12-3 | Pressure modes (§21.5) + **emergency mode** (O-007) + bounded emergency reserve (§36.5). | M | | emergency bypasses optional caches, releases aggressively; reserve never depends on normal heap. |
| W12-4 | Latency classes (§36.11) annotated on slow paths; arena `no_ipc_fast_only`/`bounded_slow_path`/`may_block`. | S | ∥ | each slow path tagged; real-time arenas can forbid blocking. |

---

### W13 — Topology awareness (CPU / LLC / NUMA)
**Goal:** model and use CPU→LLC→NUMA topology for placement and rebalancing, degrading to single-domain.
**SPEC:** §15 (whole).
**Depends on:** W6 (domains used by transfer caches). **Enables:** M3 (LLC), M4 (full).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W13-1 | Topology discovery (§15.2) from sysfs/CPUID/OS; conservative single-domain fallback. | M | | missing/inconsistent data ⇒ one domain, still correct. |
| W13-2 | Placement policy (§15.3): LLC-local alloc, NUMA-local backing, arena overrides. | M | ∥ | placement honors topology where present. |
| W13-3 | Cross-domain rebalancer (§15.4): preference order; no permanent stranding. | M | | stranded memory test: rebalancer moves batches/spans under pressure. |
| W13-4 | Hotplug/affinity/cgroup refresh (§15.2). | S | ∥ | snapshot refreshes on notification or periodic mismatch detection. |

---

### W14 — Lifetime, hotness & placement policy
**Goal:** use hints + sampled profiles to place objects, **never** affecting validity/size/alignment.
**SPEC:** §24 (whole), §24.5 (safety boundary).
**Depends on:** W11, W17 (sampling). **Enables:** M6.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W14-1 | Hint plumbing (§24.1): hot/cold + lifetime flags from §10.4 into placement. | M | | flags reach the filler; ignored-safely if absent. |
| W14-2 | Lifetime classes (§24.2) + allocation-site profile record (§24.4). | M | ∥ | sampled profiles recorded; missing/wrong profiles never break safety (§24.5). |
| W14-3 | Cold/short/long handling (§24.6–§24.8): grouping policy in the filler. | M | | grouping observable in stats; safety boundary test (placement never changes size/align). |

---

### W15 — Reallocation, aligned allocation & calloc zeroing
**Goal:** correct realloc/aligned/calloc semantics including in-place grow/shrink and zeroing sources.
**SPEC:** §25 (whole), §26 (whole), §9.7.
**Depends on:** W2 (size classification), W3 (pointer classification), W4 (extent grow/shrink), W8.
**Enables:** M1 (basic), M5 (in-place via extents).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W15-1 | realloc semantics (§25.1): `realloc(NULL,n)`, `realloc(p,0)` policy, content preservation, failure keeps original. | M | | property test: content preserved across realloc; failure safety. |
| W15-2 | Move realloc (§25.4): allocate-before-free, copy `min(old_usable,new)`, alignment/arena preserved. | M | | tested; old object preserved on OOM. |
| W15-3 | In-place grow/shrink (§25.2/§25.3): same-size-class fast path now; extent-merge growth at M5. | M | ∥ | same-class realloc is in-place; large in-place via W4 extents. |
| W15-4 | Aligned allocation validation (§25.5) + over-aligned routing (§9.3). | S | ∥ | power-of-two/min checks; over-aligned never offset-adjusts a shared slab. |
| W15-5 | calloc zeroing (§26.2/§26.3) + overflow+rounding guard (§26.1, with W2-4). | M | | zeroed result; zero-state metadata invalidated on reuse; overflow safe. |

---

### W16 — Concurrency, memory ordering, fork, signal, TLS
**Goal:** the cross-cutting correctness rules that keep the allocator safe under threads, fork, and signals.
**SPEC:** §27 (whole), §28 (whole), §35.4 (init phases), S-008/S-010.
**Depends on:** touches all; **Enables:** M2 (real concurrency).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W16-1a | Encode the lock ranks (front-end<transfer<central<span<backend<stats, §27.2) as a typed/ranked lock wrapper; acquisition records its rank. | M | | every lock has a compile-time rank; refill/flush hold ≤1 middle-end lock by construction. |
| W16-1b | Debug lock-order checker: a per-thread held-rank stack asserts monotonic acquisition; wired as the G-conc gate. | S | ∥ | any out-of-order acquire fails in debug CI. |
| W16-2 | **TLS initial-exec model** (§27.6): no `malloc` re-entry on first TLS access; `dlopen` allocation-free bootstrap path. | M | | TLS-recursion test (load via dlopen) does not re-enter allocator. |
| W16-3 | Atomics ordering map (§27.3): publication=release, consumption=acquire, transitions=acq-rel, stats=relaxed; documented per atomic. | M | ∥ | each atomic annotated; TSan clean. |
| W16-4 | Single global lock (M1) → fine-grained (M2) migration without correctness regression. | M | | M1 passes with global lock; M2 passes with hierarchy. |
| W16-5a | Pre-fork + parent-post-fork handlers (§28.1): acquire the fork lock + quiesce background threads pre-fork; release + resume in the parent. | M | | parent unaffected; no leaked held lock. |
| W16-5b | Child-post-fork handler: reset lock states, disable background threads, flush/conservative-mode inconsistent per-CPU state (§28.1). | M | | fork-in-multithread test: child allocates safely; no inherited held lock. |
| W16-6 | Signal/reentrancy/crash (§28.2–§28.4): document non-async-signal-safety; reentrancy guard; lock-free crash summary. | S | ∥ | reentrancy during init/hooks handled; crash summary needs no lock/alloc. |
| W16-7 | Initialization phases (§35.4) Phase0–6, each reentrancy-safe; shutdown policy (§35.5). | M | | phased init test; teardown available for tests, leak-by-default in prod. |

---

### W17 — Observability: stats, telemetry, profiling  *(continuous)*
**Goal:** answer "where is the memory?" with machine-readable, epoch-consistent stats and low-overhead
profiling — including label-scoped views for seLe4n.
**SPEC:** §31 (whole), §8.6 (stats consistency), §19.7, §36.12 (redaction), O-001..O-006.
**Depends on:** every owner of state. **Enables:** M6, operators, tests.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W17-1 | Stats core (§31.1, O-002): all byte classes; epoch/sequence + consistent-snapshot mode (§8.6). | M | | classes reconcile to managed VM modulo documented convention (§8.6). |
| W17-2 | Snapshot/JSON/print API (§31.2) + flags (SUMMARY/BY_ARENA/BY_SIZE_CLASS/BY_CPU/BY_NUMA/BY_HUGEPAGE). | M | ∥ | JSON matches Appendix D shape; additive-field rule (§35.3). |
| W17-3a | Sampling mechanism (§31.4): per-thread/per-CPU bytes-between-samples counter (Poisson), no hot-path lock. | M | | sampling decision lock-free; rate configurable. |
| W17-3b | Stack capture on a sampled alloc without recursive malloc (bounded, alloc-free unwind into a fixed buffer). | M | | unwinder never re-enters the allocator (§31.4). |
| W17-3c | Sampled-object bookkeeping: track sampled live objects, free them safely, right-censored lifetime accounting (§31.4). | M | ∥ | freeing a sampled object is correct + accounted. |
| W17-3d | Heap + lifetime profile aggregation + dump format (§31.3). | M | ∥ | profiles dumpable; low overhead. |
| W17-4 | Fragmentation metrics (§31.5) + hugepage coverage (§19.7) wired from W11. | M | ∥ | internal/external/cache/hugepage fragmentation reported. |
| W17-5 | `topo_explain_memory()` (§31.6). | S | ∥ | returns a human-readable RSS attribution string. |
| W17-6 | **Label-scoped & redacted stats** (§36.12): low domains cannot infer high-domain patterns. | M | | stats-redaction test (§36.16); `stats_observation_noninterference` mirrored. |

---

### W18 — Security & hardening
**Goal:** the hardened profile: protect metadata, detect misuse, quarantine, guard pages, and (for seLe4n)
scrub-before-downgrade.
**SPEC:** §29 (whole), §17.3 (metadata protection), §36.12 (scrub), threat model §3.3.
**Depends on:** W3, W5, W8. **Enables:** M7.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W18-1 | Metadata protection (§29.2/§17.3): out-of-line large headers, encoded freelist pointers, generation tags, optional guard pages. | M | | encoded pointers in hardened; large-header checksum/tag. |
| W18-2 | Double/invalid-free detection (§29.3, S-009): same-cache double free, flush-time detect, quarantine hit, sized-delete mismatch. | M | | detected in hardened/debug; never corrupts unrelated state. |
| W18-3 | Quarantine (§29.4): accounted separately; policy knobs; drain protocol. | M | ∥ | quarantined bytes in stats; reuse delayed per policy. |
| W18-4 | Guarded allocations (§29.5) sampled/opt-in. | M | ∥ | guard pages detect overrun/underrun on sampled allocs. |
| W18-5 | Junk filling (§29.6) debug-only; off in performance. | S | ∥ | alloc/free fill patterns; verify-on-reuse in debug. |
| W18-6 | **Scrub-before-downgrade** (§36.12): high→low reuse only after scrub + revoke. | M | | `scrub_before_downgrade` test on Sim; label test (§36.16). |

---

### W19 — Debugging & sanitization modes
**Goal:** the debug profile and tooling: exhaustive invariant checks, sanitizer compatibility, deterministic
replay.
**SPEC:** §30 (whole), §34.1 (sanitizer builds), Appendix B (runtime checklist).
**Depends on:** W5, W3, W11. **Enables:** M7.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W19-1a | Global invariants (B.1): one-owner, live-disjoint, free-structure reachability, page↔descriptor, released-no-live. | M | | each a callable checker; runs in debug CI. |
| W19-1b | Cache invariants (B.2): capacity/budget, batch distinctness, refill/flush count preservation. | M | ∥ | checkers pass on the M2 cache paths. |
| W19-1c | Span invariants (B.3): ranges fit/disjoint, sc match, free-count==bitmap, empty-detection across caches, generation. | M | ∥ | checkers pass; consumes W5-3. |
| W19-1d | Hugepage + arena invariants (B.4/B.5): bins match occupancy, live-bytes sum, reset/destroy drain, hook-install order. | M | ∥ | checkers pass on M4/M5 paths. |
| W19-2 | Sanitizer integration (§30.3): ASan/MSan/TSan builds; disable custom asm under sanitizers. | M | | sanitizer CI jobs green; no false positives from RSEQ asm. |
| W19-3 | Deterministic test mode (§30.4): seeded randomness, deterministic refill, reproducible sampling, force-slow-path, trace IDs. | M | ∥ | a trace replays identically; differential runner uses it. |

---

### W20 — Configuration & control plane  *(continuous)*
**Goal:** the knobs + runtime control namespace with documented precedence and validation.
**SPEC:** §32 (whole), §10.5 (control API), Appendix E, O-006.
**Depends on:** W17 (stats controls). **Enables:** M4, operators.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W20-1 | Config sources + precedence (§32.1): env/file/linker/init-API/runtime/build; security envs can disable env config. | M | | precedence documented + tested; env-disable works. |
| W20-2 | Knob set (§32.2) wired to behavior; safe server defaults (§32.3). | M | ∥ | each knob has effect + default; defaults match §32.3. |
| W20-3 | Control namespace (§10.5/Appendix E): stats/cache/release/arena/profile/emergency controls; blocking controls documented. | M | | every Appendix E entry resolves; blocking ones flagged. |
| W20-4 | Runtime-change validation (§32.4): immediate vs future vs quiescence-required. | S | ∥ | each change classified + enforced. |

---

### W21 — Testing, benchmarking & validation  *(continuous; cross-cutting)*
**Goal:** the verification apparatus that makes every other workstream's "Exit" provable, including
differential testing against the Lean model.
**SPEC:** §34 (whole), §33.7 (trace replay).
**Depends on:** W0 harness, W1 oracle. **Enables:** every milestone gate.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W21-1 | Property-test generators (§34.3) for the API op set; properties: no dup live ptr, content preserved, alignment, nonneg stats, ownership conservation. | M | | properties run in CI; shrink to minimal counterexamples. |
| W21-2a | Trace capture: instrument the allocator (deterministic mode, W19-3) to emit the §33.7 grammar for every op. | M | | every public op emits a trace line; replayable. |
| W21-2b | Replay driver: feed the trace to the Lean executable model (W1-10); check `WellFormed` + abstract-outcome agreement at each boundary. | M | | model and impl agree on a recorded corpus. |
| W21-2c | Divergence reporting: minimal failing op + state diff; wired into CI as a gate. | S | ∥ | a seeded divergence fails CI with the offending op. |
| W21-3a | Throughput stress (§34.4): many threads, same + mixed size classes, cross-thread free, producer/consumer ownership transfer. | M | ∥ | no data race under TSan; invariants hold. |
| W21-3b | Adversarial lifecycle (§34.4): affinity changes, thread-exit-with-full-cache, purge-during-alloc, rejected arena-reset races. | M | ∥ | each scenario green; rejected races return errors, not corruption. |
| W21-3c | Model-checking the lock-free sequences (loom/shuttle-style) where feasible. | M | ∥ | bounded interleavings of push/pop/flush explored. |
| W21-4 | Memory-pressure + failure-injection (§34.7): cgroup approach, mmap/madvise/retype/map/CSlot/VSpace failures deterministic. | M | ∥ | each failure is deterministic + documented. |
| W21-5 | Fuzzing (§34.8): API sequences, flags, arena lifecycle, control inputs, stats JSON, hook failures, corrupted metadata harness. | M | ∥ | fuzz targets in CI nightly; corpus committed. |
| W21-6 | ABI + benchmark suites (§34.6): hot-path throughput, producer/consumer, idle-cache footprint, churn, fragmentation-over-trace, RSS phase changes, tail latency. | M | ∥ | benchmarks reproducible; results schema in `/bench`. |

> **▸ Decomposition — W21-2 (differential testing), the bridge between code and proof.** This is what makes
> the Lean model load-bearing rather than decorative (R9): the implementation and the executable model
> (W1-10) must agree on every operation's abstract outcome and on `WellFormed` at each boundary. Splitting
> capture/replay/reporting lets capture ride on deterministic mode (W19-3) and lets the replay driver evolve
> with the model. **Why high-value:** one divergence localizes a bug to a single operation with a minimal
> trace — far cheaper than a crash three million allocations later. It runs against *both* backends, so a
> POSIX↔Sim behavioral difference is caught here too.

---

### W22 — seLe4n integration profile (capability backend, resource server, bridge)  *(co-developed against the real ABI; runs on the kernel at M8)*
**Goal:** the required microkernel-integration conformance: `Sele4nBackingProvider` (built on the real
`sele4n-abi`/`sele4n-types`/`sele4n-sys` crates), the `TopoResourceServer`, capability/label/quota
machinery, and the adapters. The provider compiles against the real ABI from M1; a host `Sele4nSim` mirrors
the same `invoke_syscall` surface for host-side execution and differential testing; M8 runs it on the real
kernel in QEMU.
**SPEC:** §36 (whole), R10–R15; upstream crates `sele4n-abi`/`sele4n-types`/`sele4n-sys`/`sele4n-hal`.
**Depends on:** W4 (seam), W9 (cap arenas), W1-11..14 (bridge). **Enables:** Microkernel conformance.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W22-0 | **Bind to the real ABI:** add `sele4n-abi`/`sele4n-types`/`sele4n-sys` as pinned deps (D8); map `TopoBackingProvider` ops → `SyscallId`/`TypeTag` retype + page map/unmap with `AccessRights`/`PagePerms`; map the §36.14 `TOPO_ERR_*` taxonomy onto `KernelError`/`KernelResult`. | M | | provider + Sim share the real types; the op→syscall and error-mapping tables are reviewed; workspace type-checks against the pinned upstream. |
| W22-1a | Model kernel objects on the host: untyped pool, CNode/CSlots, frames, VSpace — typed with `CPtr`/`Slot`/`ObjId`/`TypeTag`/`AccessRights` from `sele4n-types`. | M | | object model uses the real types; deterministic. |
| W22-1b | Implement `invoke_syscall` dispatch for the retype/map/unmap/delete/revoke `SyscallId`s deterministically; return `KernelResult`/`KernelError`. | L | | each modeled syscall matches the real ABI signature + error space. |
| W22-1c | Enforce the provider state machine (§36.6) + provenance: every frame descends from authorized untyped; device/DMA isolation. | M | | illegal transitions rejected as `KernelError`; provenance recorded. |
| W22-1d | Trace emit + replay hook so the Sim participates in differential testing against the Lean bridge (W21-2, W1-11). | M | ∥ | Sim traces replay against the bridge model. |
| W22-2 | Capability authority set (§36.4): `TopoHeapServiceCap/ArenaCap/BackingCap/ControlCap/StatsCap/EmergencyCap`; attenuation. | M | ∥ | authority checks enforced; monotonicity tests (W9-5). |
| W22-3 | CSlot/VSpace accounting (§36.8) + clean exhaustion errors (`TOPO_ERR_CSPACE_EXHAUSTED`/`VSPACE_EXHAUSTED`). | M | | exhaustion tests (§36.16) fail cleanly, never borrow authority. |
| W22-4 | Backing-state mapping (§36.7): Topo states ↔ seLe4n resource states; `Released`=revoked+recycled. | M | | state-mapping tests; no cross-label reuse without scrub+revoke. |
| W22-5a | Server state (§36.3.1): untyped inventory, CSlot/VSpace accounting (W22-3), per-arena quota ledger. | M | | state model + accounting consistent. |
| W22-5b | IPC request handlers (arena create/quota/purge/stats/backing) over `MessageInfo`/`IpcBuffer`; per-cap authorization (W22-2). | L | | each request authorized by capability; denials explicit. |
| W22-5c | Boot/init integration (with W22-7) + emergency reserve allocated before clients. | M | | server boots on Sim (S2); reserve independent of normal heap. |
| W22-5d | Maintenance scheduling: resumable revoke/scrub chunks per latency classes (§36.11, W12-4). | M | ∥ | maintenance never blocks a client critical path unboundedly. |
| W22-6a | Client-side caches + local metadata for already-mapped objects; the no-IPC fast path (§36.3.2/§36.11). | M | ∥ | common-path malloc/free issue no IPC. |
| W22-6b | Slow-path batching to the server; correct on denial (quota/CSlot/VSpace/policy/pressure). | M | | every denial class handled; client never assumes success. |
| W22-6c | Flush/quarantine before arena destroy, capability revocation, thread migration, or domain transfer. | M | ∥ | caches drained before any such event (ties W9-6b). |
| W22-7 | Boot inventory + largest-first retype (§36.5): classify device/DMA/normal; emergency reserve before clients; provenance recorded. | M | | boot-inventory + largest-first tests (§36.16); device memory never in normal pool. |
| W22-8a | Label type + arena label assignment; partition local/transfer/central/backend pools by label (§36.12). | M | | pools never mix labels by construction. |
| W22-8b | Cross-label reuse gate: high→low reuse only after scrub+revoke (W18-6/W9-6c); enforced at the central/backend boundary. | M | | cross-label reuse blocked without scrub (§36.16). |
| W22-8c | Label-scoped stats/profiling (with W17-6) + non-interference test (mirrors `stats_observation_noninterference`, W1-12d). | M | ∥ | low domain cannot infer high-domain patterns. |
| W22-9 | seLe4n API surface (§36.14): bootstrap/create/delegate/mallocx_arena/destroy/snapshot + full error taxonomy. | M | ∥ | every error class reachable + tested; C++ maps selected errors to `bad_alloc`. |
| W22-10 | Adapters (§36.15): VKA-like, VSpace, allocman-like, Rust `GlobalAlloc`, C++ ABI on seLe4n. | M | ∥ | each adapter passes a compatibility test (S7). |
| W22-11 | Deployment profiles (§36.18): static fixed-arena, dynamic-service, kernel-adjacent bootstrap (bump-only). | M | | fixed-arena needs no runtime retype; bootstrap profile has no general free. |
| W22-12a | Feature-flag the backend to select real `invoke_syscall` vs `Sele4nSim`; build `topo-backend-sele4n` against the real kernel ABI. | M | | both targets build; selection is one flag. |
| W22-12b | Boot `TopoResourceServer` in QEMU; serve a minimal client; vertical-slice malloc/free works on the real kernel. | L | | server boots in QEMU; client allocates/frees. |
| W22-12c | Run the §36.16 suite in QEMU; fixed-arena profile needs no runtime retype; dynamic exhaustion clean. | L | | §36.16 green on the real kernel (G-sele4n). |
| W22-13a | §36.16 test list realized as concrete tests on Sim (then reused in QEMU at W22-12c). | M | | every §36.16 bullet has a test. |
| W22-13b | Wire the single-core §36.17 theorem families (from W1-12) as CI gates for M8. | S | ∥ | G-sele4n requires the families proved. |

> **▸ Decomposition — W22-1 (`Sele4nSim`) and why it is split from W22-0.** W22-0 *binds the types* (compile
> against `sele4n-abi`/`sele4n-types`, map ops→`SyscallId`/`TypeTag` and errors→`KernelError`); W22-1
> *implements the behavior* of those syscalls on the host. Keeping them separate means the type/error mapping
> is reviewed once and frozen, and the Sim's behavior (W22-1b, the largest piece) can evolve without
> re-touching the seam. The Sim must enforce the same *failures* the real kernel does — provenance
> (every frame descends from untyped, W22-1c), authority, and the §36.6 transition order — or it gives false
> confidence; that is why W22-1c is its own unit and why the Sim participates in differential replay (W22-1d).
> At M8 only the execution target changes (W22-12a, a feature flag); the suite (W22-13a) is reused verbatim.
>
> **Risk-front-loading (D2):** W22-0 (bind to the real ABI) + W22-1 (`Sele4nSim`) are the linchpins that make
> "seLe4n co-equal from the start" real without hardware: the provider type-checks against the actual
> `sele4n-abi`/`sele4n-types` from M1, and the host Sim shares that exact surface for execution. Both land in
> M1 and are maintained as a first-class, CI-gated test backend at every milestone (G-sim), or the co-equal
> promise silently degrades into "POSIX-first, seLe4n-later."

---

### W23 — Deployment, ABI compatibility & release engineering
**Goal:** ship it: interposition, mixed-allocator safety, ABI stability, init/shutdown, packaging, docs.
**SPEC:** §35 (whole), Appendix F (deployment anti-patterns).
**Depends on:** W8, W16, W17. **Enables:** M9/GA.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W23-1 | Deployment modes (§35.1): static, dynamic-as-process-allocator, LD_PRELOAD (documented caveats), runtime integration. | M | | each mode documented + smoke-tested. |
| W23-2 | Mixed-allocator detection (§35.2): TopoMalloc-frees-only-TopoMalloc; foreign-pointer detection in hardened/debug. | M | ∥ | foreign free fails safely in hardened. |
| W23-3 | ABI stability (§35.3): stable C names/struct ABI, opaque handles, additive stats fields; ABI test in CI. | M | | ABI test pins the surface across a release series. |
| W23-4 | Packaging + docs site (mdbook): deployment, profiles, tuning, ABI, seLe4n integration guide. | M | ∥ | `cargo xtask docs` builds the site; published artifact. |
| W23-5a | POSIX perf vs jemalloc/TCMalloc: micro-benchmarks + workload replays (§34.6); record in `/bench`. | M | ∥ | reproducible; targets met or documented as open. |
| W23-5b | seLe4n perf (§36.19 S9): RPi5/QEMU vs static pools + an allocman-like baseline. | L | ∥ | reproducible; recorded in `/bench`. |

---

## 7. Cross-cutting tracks & CI gates

### 7.1 Continuous workstreams
`W1` (formal), `W17` (stats), `W20` (config), `W21` (testing) are **not** one-shot. Every feature WU in any
workstream carries a *tax*: if it adds state, it updates W17 stats and W21 properties; if it adds a knob, it
updates W20; if it adds/changes a transition, it updates W1. This tax is enforced by the PR checklist (§9).

### 7.2 CI gate ladder (per milestone)
A milestone closes only when its gate set is green. Gates are additive — later milestones include earlier
gates.

| Gate | Introduced at | Checks |
|---|---|---|
| G-build | M0 | x86-64 + AArch64 build (debug+performance); `lake exe check`; lint; SPDX; markdownlint. |
| G-table | M1 | size-class generator output == golden, and Lean accepts golden (single source of truth). |
| G-core | M1 | ABI + property + Appendix-B.1/B.3 debug checks; calloc overflow+rounding; errno semantics. |
| G-model | M1→ | named §33.4 theorems for shipped transitions proved or `V-004` debt logged with an ID. |
| G-sim | M1→ | the milestone's vertical slice passes over `Sele4nSim`, not only POSIX. |
| G-conc | M2 | TSan-clean concurrency stress; lock-order checker clean; budget bounds. |
| G-fast | M3 | forced-migration RSEQ/pinned-core equivalence vs locked; RSEQ battery. |
| G-arena | M4 | isolation, reset/destroy, revocation, CSlot/VSpace exhaustion, quota/authority/label. |
| G-mem | M5 | hugepage invariants H-001..H-005; release avoids refault loop (oscillation test). |
| G-ops | M6 | stats reconcile (§8.6); redaction test; sampling overhead bound. |
| G-hard | M7 | full §33.4 + single-core §36.17; fuzz clean; sanitizer-clean; scrub/label tests. |
| G-sele4n | M8 | §36.16 suite on real seLe4n in QEMU; fixed-arena no-retype; dynamic exhaustion clean. |
| G-abi | M9 | ABI compatibility; SMP bridge families; perf results recorded. |

### 7.3 Formal-methods cadence
- Tables are Lean-verified from M1 (G-table).
- Each shipped transition is named per §33.4 and proved or carries a tracked `V-004` debt (G-model). The debt
  list is reviewed at every milestone; M7 burns it to zero for single-core.
- The bridge (W1-11..13) is co-developed; its single-core theorem families gate M7/M8; SMP forms (W1-14)
  may stage to M9 per §36.17/V-004.

### 7.4 Security review cadence
Run the repo's `security-review` over the diff at the close of M4 (authority/arenas), M7 (hardening), and M8
(real seLe4n), plus any WU touching `/sele4n`, `/arch`, freelist encoding, or metadata protection.

---

## 8. Risk register

| # | Risk | Likelihood | Impact | Mitigation | Owning WS |
|---|---|---|---|---|---|
| R1 | **Empty-span detection across caches** (§16.5) is subtly wrong → leaks or, worse, premature reuse. | Med | High | Debug-exact conservation law co-located with the fast path (W5-3); differential tests; never let performance build diverge from debug accounting. | W5, W21 |
| R2 | **D2 co-equal seLe4n slips to "POSIX-first."** | Med | High | The real ABI exists, so `Sele4nBackingProvider` *compiles against `sele4n-abi`/`sele4n-types` from M1* and is CI-gated (G-sim); host `Sele4nSim` shares that exact type surface; seLe4n acceptance row in every milestone; W22-0/W22-1 land in M1. | W22, W4 |
| R3 | **TLS re-entrancy** (§27.6) causes init deadlock/recursion. | Med | High | Initial-exec TLS + dlopen bootstrap path proven in M2 (W16-2) before per-CPU fast paths. | W16 |
| R4 | **Hand-maintained tables drift** (Appendix F). | Med | Med | Single generator + Lean check + CI golden-diff (G-table). | W1, W2 |
| R5 | **RSEQ correctness** under migration/signal. | Med | High | Locked per-CPU baseline first (W6-4); RSEQ equivalence vs locked is a gate (G-fast); per-arch abort handlers + battery (W7-6). | W7 |
| R6 | **Lock-order cycles** → deadlock. | Med | High | Total order (§27.2) + debug lock-order checker as a gate (G-conc); hand-over-hand refill/flush hold ≤1 middle-end lock. | W16, W6 |
| R7 | **Capability/CSlot leaks** on destroy/revoke. | Med | High | Generation counters; mandatory revocation tests; `destroy_revokes_descendants` theorem. | W9, W22, W1 |
| R8 | **IPC slow-path cost** on seLe4n. | Med | Med | Batch backing requests; pre-grant/fixed-arena profile; latency classes (W12-4). | W22, W12 |
| R9 | **Formal proofs lag implementation** → "formal" becomes theater. | Med | High | G-model gate + tracked V-004 debt burned to zero at M7; formal-in-lockstep principle. | W1 |
| R10 | **Information-flow leak via stats/dirty reuse** (§36.12). | Low | High | Label-partitioned structures from M1 (even trivial on POSIX); scrub-before-downgrade (W18-6); redaction tests. | W22, W18, W17 |
| R11 | **License incompatibility** (MIT core vs GPLv3 seLe4n, §36.20). | Low | Med | D5 split: MIT core + separate-licensed `/sele4n` layer + NOTICE, before any `/sele4n` code. | W0 |
| R12 | **Scope/throughput**: the SPEC is vast; risk of analysis paralysis. | High | Med | Vertical-slice milestones; M1 is a *usable* allocator; never block a milestone on a later-milestone feature. | all |
| R13 | **Upstream seLe4n ABI drift** breaks `Sele4nBackingProvider`. | Med | Med | Pin deps to a commit SHA (D8) with a vendored mirror; a periodic bump WU re-validates against the §36.16 suite; `Sele4nSim` mirrors the pinned surface so drift surfaces as a *compile error in CI*, not a runtime surprise on the kernel. | W22, W0 |

---

## 9. Definition of Done (every WU) & PR checklist

A WU is **done** only when all of the following hold (the PR template, W0-11, encodes this):

- [ ] Builds clean on x86-64 **and** AArch64 in debug + performance profiles; no new `clippy`/lint warnings.
- [ ] Unit tests for the WU; **property tests** updated if the WU changes API behavior (§34.3).
- [ ] Debug **invariant checks** (Appendix B) relevant to the touched state pass; new state adds its checker.
- [ ] If a transition was added/changed: the **Lean model + named §33.4/§36.17 obligation** is updated and
      proved, **or** a `V-004` refinement debt is filed with a tracking ID and a reason.
- [ ] If state was added: **stats (W17)** expose it and it **reconciles** (§8.6); **trace grammar** updated if
      observable.
- [ ] If a knob/behavior was added: **control plane (W20)** + **docs** updated; default matches §32.3 intent.
- [ ] The **seLe4n vertical slice** still passes over `Sele4nSim` (G-sim) for milestones ≥ M1.
- [ ] No new **Appendix-F anti-pattern** introduced (reviewer-confirmed).
- [ ] SPDX header present; `/sele4n` files under the correct license (D5).
- [ ] Transitions tagged in code with their SPEC state-machine name (M-001).

---

## 10. Parallelization guide (how N engineers/agents split this)

Once M1's seams (§3.1) are frozen, the following tracks proceed largely independently:

- **Track Formal** (W1, ongoing): owns Lean + tables + trace oracle. Unblocks everyone via stable table/oracle
  outputs. *Mostly independent.*
- **Track Backend** (W4 → W11 → W12): owns the provider seam, POSIX, hugepage, release. Talks to Track Sim via
  the seam.
- **Track Core** (W3 → W5 → W6 → W7): pagemap → central → caches → RSEQ. The allocator spine.
- **Track API** (W8, W15, W23): public surface + realloc/aligned + deployment. Consumes Core/Backend seams.
- **Track Arena/seLe4n** (W9, W10, W22): capability arenas, hooks, resource server, Sim. Co-develops with
  Backend across the seam.
- **Track Ops** (W17, W19, W20): stats, debug, config. Cross-cuts; lands the "tax" from every other track.
- **Track Topology/Policy** (W13, W14): placement. Plugs into Core/Backend after M3.
- **Track Verify** (W21, ongoing): property/differential/concurrency/fuzz/bench. Pairs with every track.

The frozen seams in §3.1 are the contracts that make this safe: a track may change only its own side of a
seam without a cross-cutting WU.

---

## 11. Traceability matrix (SPEC requirement family → workstream)

| SPEC family | Requirement IDs | Primary WS | Verified by |
|---|---|---|---|
| Functional | F-001..F-010 | W8, W9, W4, W6/W7 | ABI + property tests (W21) |
| Safety | S-001..S-010 | W1, W3, W5, W4, W16 | Lean (W1) + Appendix-B checks (W19) |
| Performance | P-001..P-008 | W6, W7, W11, W12, W13 | benchmarks (W21-6), gates G-fast/G-mem |
| Operational | O-001..O-007 | W17, W20, W12 | stats reconcile (W17), control tests (W20) |
| Formal | V-001..V-005 | W1, W2 (tables) | `lake exe check` (G-table/G-model) |
| State machine | M-001..M-005 | W1, W5, W4 | Lean + transition tags + B checks |
| Pagemap | P-Map-001..006 | W3 | W3 unit tests + `pagemap_lookup_sound` |
| Central list | C-001..C-005 | W5 | W5 tests, B.3 |
| Hugepage | H-001..H-005 | W11 | B.4, G-mem |
| Cache | B.2 set | W6, W7 | budget tests, G-conc |
| Arena | §22 + B.5 | W9 | isolation/reset/destroy tests, G-arena |
| seLe4n profile | §36 conformance | W22 + W1 bridge + W4 seam | §36.16 suite + §36.17 theorems, G-sele4n |

---

## 12. The very first actions (start here)

In strict order, the first concrete moves that begin executing this plan:

1. **W0-1** — record D3–D8 outcomes in §4.2 (D5 license split is the only one that must close before any
   `/sele4n` file is written; D8 must close before `Sele4nBackingProvider` first compiles).
2. **W0-2/W0-3/W0-4** — create the repo layout, pin toolchains, stand up the Cargo workspace + `xtask` + Lean
   `lake` package.
3. **W0-5/W0-6** — CI (x86-64 + AArch64) with build/lint/`lake exe check` required on the branch.
4. **W0-9** — add the Claude-on-web SessionStart hook so subsequent web sessions self-provision (use the
   `session-start-hook` skill).
5. **W22-0 (pin) + W1-1/W1-2 + W4-1 + W0-14** — pin and vendor `sele4n-abi`/`sele4n-types`/`sele4n-sys`
   (D8) so the seLe4n provider stub type-checks against the real ABI; Lean skeleton + **empty bridge
   package**; the `TopoBackingProvider` trait with POSIX + `Sele4nSim` stubs; and the bump-allocating
   **walking skeleton** — closing **M0**.
6. **W1-4a (start) → W1-4e + W2-1** — the size-class model + generator (the single source of truth), because
   it gates M1 and is the longest pole in the Core floor. Begin W1-4a in M0; W1-4d/4e gate W2 and M1.

Everything else follows the milestone ladder in §5.

---

*End of plan. This document is itself maintained under the change-log discipline of §0.1; material changes to
sequencing, seams, or conformance mapping are PRs against `planning/WORKSTREAM_PLAN.md`.*

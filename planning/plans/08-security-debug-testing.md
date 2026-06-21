# Plan 08 — Security, Debug & Testing

**Workstreams:** W18 (security/hardening), W19 (debug/sanitizers/deterministic), W21 (testing/bench/differential)
· **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §29, §30, §34, §17.3, §36.12, §33.7, Appendix B; S-009.
**Upstream deps:** [03](03-core-allocator.md), [02](02-formal-model.md) (trace oracle). **Downstream:** every
plan (this is the verification apparatus that makes their "Exit" provable). **Milestones:** testing is
**continuous**; hardening/debug land at **M7**.

> This is the assurance domain: make misuse detectable (W18), make every invariant runtime-checkable (W19),
> and make every layer provable by property/differential/concurrency/fuzz testing (W21). W21-2 (differential
> testing) is what keeps the Lean model **load-bearing** rather than decorative (R9).

---

## W18 — Security & hardening

**Depends on:** plan 03 (W3,W5), plan 06 W8. **Enables:** M7. **Threat model:** §3.3.

> **Status — landed (ahead of its M7 slot).** All six units are implemented behind
> granular, profile-composed Cargo features (`crate::harden`; `performance` pays
> nothing): **W18-1a/b** out-of-line large metadata + generation/integrity tags
> (always-on; the freelist is an out-of-line bitmap, §16.4, so no critical metadata
> is in user-writable memory — the encoded-freelist requirement is met *structurally*;
> optional metadata guard pages remain a deferred refinement). **W18-2** double/invalid-
> free detection (always-on: bitmap double-free, interior/foreign/metadata, sized-delete
> mismatch, quarantine-hit; flush-time detection arrives with the M2 caches). **W18-3**
> quarantine (`quarantine` feature; accounted as `quarantine.bytes`, budgets + random-
> evict + sampling + drain, `topomalloc_quarantine_*` control, off by default).
> **W18-4** guarded allocations (`guard-pages` feature; the new `TopoBackingProvider::protect`
> seam + `LargeAllocator::allocate_guarded` + `GuardSampler`; **tight right-aligned** guards
> so a one-past-`usable` overrun faults, **randomized** GWP-ASan-style sampling, a real
> `mprotect` SIGSEGV trap, proven by a POSIX death test). **W18-5** junk filling (`junk-fill`
> feature; fill-on-alloc/free + verify-on-reuse for **small *and* large** objects — the large
> path carries a sound per-extent canary provenance, proven by a forked UAF death test).
> **W18-6** scrub-before-downgrade (`secure-scrub` feature; the non-PUBLIC scrub is
> unconditional, the runtime image of the Lean `scrub_before_downgrade` theorem; a
> POSIX/Sim co-equality test is the §36.16 label test).
>
> **Optimal-completion pass.** A self-audit hardened every "present-but-inert" piece:
> **W18-5** verify-on-reuse now covers large allocations soundly — a per-`Slot` `canary`
> provenance bit (set only on a canary-filled free-to-`Dirty`, cleared on every commit/
> decommit/muzzy/grow transition, split-inherited, merge-AND-joined) means a stale bit on a
> decommitted extent can never arise, so a correct program is never false-aborted
> (`extent::tests::{retained_canary,…}` + the `large_use_after_free_aborts_on_reuse` death
> test). **W18-3** the quarantine gained an Appendix-B invariant checker (asserted on the
> offer/drain hot paths), a randomized property test + a `quarantine` fuzz target (invariant
> + budgets + exact byte conservation), background convergence (`drain_excess` /
> `topomalloc_quarantine_converge`, so lowering the budget on a quiescent heap converges
> promptly), the §8.6 reconciliation proven under concurrent load with the quarantine on, and
> budget readers (`topomalloc_quarantine_max_bytes`/`_max_objects`). **W18** `corruption_abort`
> is the single allocation-free abort path (Appendix F). A latent bug was fixed: `topo-abi`'s
> `hardened`/`debug` composed only `topo-core/hardened`, leaving topo-abi's own feature gates
> off — so `topomalloc_quarantine_set_limits` silently compiled to a no-op in the hardened
> artifact; the profiles now compose the granular units. CI tests each hardening feature
> **alone**, not only the composed profile (principle 8).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W18-1a | Out-of-line metadata for large allocations + generation tags (§29.2/§17.3). | M | | large headers separated from user data. |
| W18-1b | Encoded freelist pointers for free small objects; large-header checksum/tag; optional metadata guard pages (§29.2). | M | ∥ | encoded pointers in hardened; tamper detected. |
| W18-2 | Double/invalid-free detection (§29.3, S-009): same-cache double free, flush-time detect, quarantine hit, sized-delete mismatch, free-of-not-live (debug bitmap). | M | | detected in hardened/debug; never corrupts unrelated state. |
| W18-3 | Quarantine (§29.4): accounted separately; policy knobs (max_bytes/objects, random_evict, per-arena, sampled_only); drain protocol. | M | ∥ | quarantined bytes in stats; reuse delayed per policy. |
| W18-4 | Guarded allocations (§29.5), sampled/opt-in: inaccessible pages around a user object. | M | ∥ | guard pages detect overrun/underrun on sampled allocs. |
| W18-5 | Junk filling (§29.6), debug-only: fill-on-alloc, fill-on-free, verify-on-reuse. | S | ∥ | patterns applied; off in performance. |
| W18-6 | **Scrub-before-downgrade** (§36.12): high→low reuse only after scrub + revoke; used by plan 06 W9-6c. | M | | `scrub_before_downgrade` test on Sim; label test (§36.16). |

> **▸ Decomposition — W18 (hardening) and the safety/performance split.** Each protection is its own opt-in,
> profile-gated unit so the performance profile pays for none of them and the hardened profile composes them.
> The two with cross-plan reach are W18-2 (invalid-free detection, which the free path in plan 06 W8 and the
> pointer classifier in plan 03 W3-4b both call) and W18-6 (scrub-before-downgrade, which plan 06's revocation
> W9-6c invokes and plan 02's `scrub_before_downgrade` theorem certifies). Encoded freelist pointers (W18-1b)
> matter because the SPEC forbids storing critical metadata only in user-writable memory (Appendix F).

> **▸ Scoped deferrals (with rationale).** A few refinements are deliberately deferred; each is a
> *narrowing*, not a safety gap (§2.4 holds throughout), and is recorded here so the scope is explicit:
>
> * **Slab red-zones (per-small-object guard bytes).** The generated size-class table fixes each class's
>   geometry (DD-1, never hand-edited); inserting inter-object red-zone bytes would change that geometry and
>   the proven §9.4/§9.5 packing. Small-object overrun detection is instead served by the **verify-on-reuse
>   canary** (a write-after-free into a free object is caught at reuse) and, for objects that need true
>   bracketing, the **sampled guard-page path** (`TOPO_GUARDED` promotes any size to a guarded large).
> * **Metadata guard pages (W18-1b "optional").** Metadata lives in a densely bump-packed `MetaArena` accessed
>   on every hot-path op; bracketing individual pools/pagemap nodes with `PROT_NONE` pages would fragment that
>   arena and add a syscall per structure, while metadata integrity is *already* covered structurally
>   (out-of-line storage — no critical metadata in user-writable memory) plus generation/integrity tags
>   (W18-1a). Deferred as a low-value/high-cost refinement, not a correctness gap.
> * **Over-scrub conservatism.** `secure-scrub` scrubs on release for *every* non-PUBLIC arena (and, under the
>   feature, even PUBLIC) rather than computing the minimal must-scrub set; this is conservative-by-design
>   (it can only scrub *more*, never less, than §36.12 requires) and upholds the Lean `scrub_before_downgrade`
>   obligation.
> * **Flush-time double-free detection (W18-2).** The same-cache/flush-time check lands with the M2 front-end
>   caches (no per-CPU cache exists yet to flush); the always-on bitmap double-free + quarantine-hit checks
>   cover the M1 paths.
> * **MTE / pointer authentication.** Hardware tagging (AArch64 MTE, PAC) is a platform *MAY*; the
>   `protect`/canary/quarantine mechanisms are the portable baseline and the seam is ready for a tagging
>   provider later.
> * **Quarantine double-free detection is best-effort in the eviction window.** A double free of a *held*
>   object is caught precisely (`AlreadyQuarantined`). One narrow concurrency-only gap remains: because the
>   quarantine lock is the §27.2 leaf, an evicted object is physically freed by the caller *after* the lock
>   is released, so in the brief [removed-from-ring, inserted-to-central] interval a concurrent double free
>   of that same object (with the opt-in quarantine enabled — itself UB in the program) is missed by both the
>   ring scan and `is_central_free`. Closing it fully needs an authoritative per-object `quarantined` bitmap
>   state (the §16.4/§17.5 scaffolding), set under the span lock at hold time and transitioned atomically on
>   drain — a span-metadata change deferred as disproportionate. A lock-free address-keyed "draining set" is
>   **not** a sound fix (it races address reuse, falsely rejecting a re-vended object's valid free — strictly
>   worse), and was rejected after evaluation. Documented on `harden::Quarantine`; consistent with §29.4's
>   best-effort framing (a correct program is never affected).

---

## W19 — Debugging & sanitization modes

**Depends on:** plan 03 (W5,W3), plan 04 W11. **Enables:** M7.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W19-1a | Global invariant checks (B.1): one-owner, live-disjoint, free-structure reachability, page↔descriptor, released-no-live. | M | | each a callable checker; runs in debug CI. |
| W19-1b | Cache invariant checks (B.2): capacity/budget, batch distinctness, refill/flush count preservation. | M | ∥ | checkers pass on the M2 cache paths. |
| W19-1c | Span invariant checks (B.3): ranges fit/disjoint, sc match, free-count==bitmap, **empty-detection across caches**, generation. | M | ∥ | checkers pass; consumes plan 03 W5-3. |
| W19-1d | Hugepage + arena invariant checks (B.4/B.5): bins match occupancy, live-bytes sum, reset/destroy drain, hook-install order. | M | ∥ | checkers pass on M4/M5 paths. |
| W19-2 | Sanitizer integration (§30.3): ASan/MSan/TSan builds; disable custom asm under sanitizers to avoid false positives. | M | | sanitizer CI jobs green; no RSEQ-asm false positives. |
| W19-3 | Deterministic test mode (§30.4): seeded randomness, deterministic refill, reproducible sampling, force-slow-path, trace IDs. | M | ∥ | a trace replays identically; differential runner uses it. |

> **▸ Decomposition — W19-1 (debug invariant checks = Appendix B).** Split by invariant group (B.1 global /
> B.2 cache / B.3 span / B.4 hugepage / B.5 arena) so each lands with the workstream that creates the state it
> checks (B.3 with plan 03 W5, B.4 with plan 04 W11, B.5 with plan 06 W9). These are *first-class code*
> (principle 7), not test-only helpers: debug/hardened builds run them as runtime assertions, and they are the
> backbone of the G-core/G-mem/G-arena gates. W19-1c (span/empty-detection) is the runtime counterpart to the
> conservation law (plan 03 W5-3) and the differential test (W21-2).

---

## W21 — Testing, benchmarking & validation *(continuous; cross-cutting)*

**Depends on:** plan 01 harness, plan 02 oracle. **Enables:** every milestone gate.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W21-1 | Property-test generators (§34.3) for the API op set; properties: no dup live ptr, content preserved, alignment, non-negative stats, ownership conservation. | M | | properties run in CI; shrink to minimal counterexamples. |
| W21-2a | Trace capture: instrument the allocator (deterministic mode, W19-3) to emit the §33.7 grammar for every op. | M | | every public op emits a trace line; replayable. |
| W21-2b | Replay driver: feed traces to the Lean executable model (plan 02 W1-10); check `WellFormed` + abstract-outcome agreement at each boundary. | M | | model and impl agree on a recorded corpus. |
| W21-2c | Divergence reporting: minimal failing op + state diff; wired into CI as a gate. | S | ∥ | a seeded divergence fails CI with the offending op. |
| W21-3a | Throughput stress (§34.4): many threads, same + mixed size classes, cross-thread free, producer/consumer ownership transfer. | M | ∥ | no data race under TSan; invariants hold. |
| W21-3b | Adversarial lifecycle (§34.4): affinity changes, thread-exit-with-full-cache, purge-during-alloc, rejected arena-reset races. | M | ∥ | each scenario green; rejected races return errors, not corruption. |
| W21-3c | Model-checking the lock-free sequences (loom/shuttle-style) where feasible. | M | ∥ | bounded interleavings of push/pop/flush explored. |
| W21-4 | Memory-pressure + failure-injection (§34.7): cgroup approach, mmap/madvise/retype/map/CSlot/VSpace failures deterministic. | M | ∥ | each failure deterministic + documented. |
| W21-5 | Fuzzing (§34.8): API sequences, flags, arena lifecycle, control inputs, stats JSON, hook failures, corrupted-metadata harness. | M | ∥ | fuzz targets in CI nightly; corpus committed. |
| W21-6 | ABI + benchmark suites (§34.6): hot-path throughput, producer/consumer, idle-cache footprint, churn, fragmentation-over-trace, RSS phase changes, tail latency. | M | ∥ | benchmarks reproducible; results schema in `/bench`. |

> **▸ Decomposition — W21-2 (differential testing), the bridge between code and proof.** This is what makes
> the Lean model load-bearing rather than decorative (R9): the implementation and the executable model (plan
> 02 W1-10) must agree on every operation's abstract outcome and on `WellFormed` at each boundary. Splitting
> *capture* (W21-2a, rides on deterministic mode W19-3), *replay* (W21-2b), and *reporting* (W21-2c) lets the
> replay driver evolve with the model. **Why high-value:** one divergence localizes a bug to a single
> operation with a minimal trace — far cheaper than a crash three million allocations later. It runs against
> *both* backends, so a POSIX↔Sim behavioral difference is caught here too.

> **▸ Decomposition — W21-3 (concurrency stress).** Split *throughput* patterns (W21-3a) from *adversarial
> lifecycle* patterns (W21-3b) from *exhaustive model-checking* (W21-3c). 3a/3b run under TSan as the G-conc
> gate; 3c (loom/shuttle) explores bounded interleavings of the lock-free push/pop/flush sequences that TSan's
> single-schedule runs can miss — the strongest guarantee available short of the Lean RSEQ proof (plan 02
> W1-7), and complementary to it.

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · Differential testing (W21-2) — the bridge between code and proof

**Problem.** A Lean model that nobody ties to the implementation is theater (R9). The implementation and the
executable model must be shown to agree on *every* operation's abstract outcome and on `WellFormed` at each
boundary — on **both** backends.

**Design space.** **Capture a trace from the running allocator (deterministic mode), replay it through the
Lean executable model, and assert agreement at each step** — chosen (§33.7). Splitting capture/replay/report
lets capture ride on deterministic mode (W19-3) and the driver evolve with the model.

**The trace grammar (§33.7).**
```text
ALLOC req size align arena flags -> ptr usable sc span      FREE ptr hint -> sc span
REFILL cpu sc count source        FLUSH cpu sc count target
SPAN_ALLOC arena sc span pages hp  RELEASE range state
```

**Work breakdown (refines W21-2a..c).** 1. instrument every public op to emit the grammar under deterministic
mode (W21-2a). 2. the replay driver: feed the trace to the Lean executable model (plan 02 W1-10); after each
op, check `WellFormed` and that the model's abstract outcome (owner transitions) matches the implementation's
(W21-2b). 3. divergence reporting: shrink to the minimal failing op + emit a state diff; wire as a CI gate
(W21-2c).

**Invariants.** model and implementation agree on abstract outcome + `WellFormed` after every op, for both
POSIX and `Sele4nSim`.

**Verify.** a recorded corpus replays clean in CI; a seeded fault (e.g. a deliberately wrong empty-detection)
is caught with the offending op named.

**Failure modes.** *F1* nondeterminism makes traces unreplayable → deterministic mode (W19-3) seeds RNG,
fixes refill order, reproduces sampling. *F2* the model lags the code → the formal-in-lockstep DoD + this gate
force them together. *F3* a POSIX↔Sim behavioral difference → caught here because both backends replay the
same corpus.

**Sequencing.** capture **M1**; replay/report **M2**; corpus grows every milestone.

### DD-2 · Debug invariant checks (W19-1) — Appendix B as runtime code

**Problem.** The Appendix B invariants must be *executable* so debug/hardened builds (and CI gates) can assert
them after transitions — not just live in prose.

**Design space.** **One callable checker per invariant group, run from debug assertions and from CI gates** —
chosen (principle 7). Each lands with the workstream that creates the state it checks.

**Work breakdown (refines W19-1a..d).** 1. B.1 global (one-owner, live-disjoint, free-structure reachability,
page↔descriptor, released-no-live). 2. B.2 cache (capacity/budget, batch distinctness, refill/flush count).
3. B.3 span (ranges fit/disjoint, sc match, free-count==bitmap, **empty-detection across caches**,
generation). 4. B.4/B.5 hugepage + arena (bins match occupancy, live-bytes sum, reset/destroy drain,
hook-install order).

**Invariants.** each checker is total and side-effect-free; running all of them is the strongest single
debug-time correctness statement.

**Verify.** the checkers *are* the verification — they back G-core (B.1/B.3), G-conc (B.2), G-mem (B.4),
G-arena (B.5); W19-1c is the runtime counterpart to plan 03's conservation law (W5-3) and DD-1.

**Failure modes.** *F1* a checker is too slow to run often → keep them O(state) and run on a sampled cadence
in long tests, every op in unit tests. *F2* a new state has no checker → the DoD requires one.

**Sequencing.** B.1/B.3 **M1**; B.2 **M2**; B.5 **M4**; B.4 **M5**.

### DD-3 · Hardening: double-free detection & quarantine (W18-2/W18-3)

**Problem.** The hardened profile must detect common heap misuse (double/invalid free, sized-delete mismatch)
and delay reuse of suspicious frees — without the performance profile paying for any of it.

**Design space.** **Profile-gated, layered detection + an accounted quarantine** — chosen (§29.3/§29.4):
fast mode detects nothing extra; hardened detects same-cache double free, flush-time double free, quarantine
hits, free-of-not-live (debug bitmap), and sized-delete mismatch; quarantine holds freed objects out of
circulation under a byte/object budget.

**Work breakdown.** detection (W18-2): the five checks above, each cheap and local. quarantine (W18-3):
separate accounting, policy knobs (max_bytes/objects, random_evict, per-arena, sampled_only), a drain
protocol. Plus encoded freelist pointers (W18-1b) so free-object metadata is not plain in user-writable
memory (Appendix F).

**Invariants.** a detected misuse never corrupts *unrelated* state; quarantined bytes are accounted
separately and are not available for allocation (plan 07 stats); reuse is delayed per policy.

**Verify.** a misuse suite (double free into same cache, free of interior pointer, sized-delete mismatch)
asserts detection + safe failure; quarantine accounting reconciles in stats.

**Failure modes.** *F1* detection corrupts state while reporting → checks are read-only until the abort
decision. *F2* quarantine unbounded → byte/object budget + eviction.

**Sequencing.** **M7**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M1 | W21-1 (property generators), W19-1a/c (B.1/B.3 checks), W19-3 (deterministic mode), W21-2a (capture). |
| M2 | W21-2b/c (differential replay), W21-3a/b (concurrency under TSan), W19-1b (B.2), W19-2 (sanitizers). |
| M4 | W19-1d (B.5 arena checks), W21-4 (failure-injection incl. CSlot/VSpace). |
| M5 | W19-1d (B.4 hugepage checks), W21-6 (fragmentation/RSS benchmarks). |
| M7 | **W18 (all)** hardened/debug profiles, W21-3c (model-checking), W21-5 (fuzzing), full B-checks. |

## Domain risks

- **R1** (empty-span) — the B.3 check (W19-1c) + differential test (W21-2) are the runtime guards.
- **R9** (formal theater) — W21-2 is the mitigation: the model is only as trustworthy as the differential
  corpus that ties it to the implementation.
- **R10** (info-flow) — the redaction/label tests live here (with plan 07 W17-6 and plan 09 W22-8).

## Definition of Done (addendum)

A WU that adds state adds its **B-check** (W19-1) and a **property** (W21-1); a WU that adds a transition adds
a **differential** assertion (W21-2). Sanitizer builds stay green; the deterministic mode reproduces any
reported failure.

## Best-practices checklist

- [ ] Hardening is profile-gated; the performance profile pays for none of it.
- [ ] Critical metadata is never stored only in user-writable memory (encoded freelist pointers).
- [ ] Debug invariant checks are first-class runtime code, grouped by invariant, gating CI.
- [ ] Differential testing ties the Lean model to the implementation on *both* backends.
- [ ] Model-checking complements (does not replace) the Lean RSEQ proof for the lock-free sequences.
- [ ] Every failure mode (incl. CSlot/VSpace/retype) is deterministic and documented.

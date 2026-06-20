# Plan 07 — Observability, Placement & Control

**Workstreams:** W17 (stats/telemetry/profiling), W14 (lifetime/hotness/placement), W20 (config/control plane)
· **Status:** rev 2.3 — **W14 landed (all units); W17 landed (all units — stats core, epoch/consistent
snapshot, JSON/print/snapshot API + flags, fragmentation, `explain_memory`, label-scoped redaction —
ahead of its M6 slot, the W17-3 sampling slice having landed first to feed W14).** · **Overview:**
[README.md](README.md)
**SPEC anchors:** §31, §8.6, §19.7, §36.12, §24, §32, §10.5, Appendices D/E; O-001..O-007.
**Upstream deps:** every state-owner (stats read all of them), [04](04-backend-hugepages-release.md) (coverage),
[08](08-security-debug-testing.md)/[02](02-formal-model.md) (sampling unwind safety). **Downstream:**
operators, tests, [09](09-sele4n-integration.md) (label-scoped views). **Milestones:** **continuous**; full
operational conformance at **M6**.

> Stats must answer "**where is the memory?**" (§31.1) in machine-readable, epoch-consistent form, with
> low-overhead profiling and — for seLe4n — label-scoped/redacted views so a low domain cannot infer a high
> domain's allocation patterns. This plan is *continuous*: every WU in every other plan that adds state pays a
> "tax" here (the DoD requires it).

## Interfaces owned here

```text
stats:    stats_snapshot(flags) -> epoch-consistent view; stats_json(); stats_print()  (§31.2)
control:  control_get/set(path) over the topo.* namespace                              (§10.5, App. E)
placement (consumed by plan 04 filler): hot/cold + lifetime hints; allocation-site profile records (§24)
```

---

## W17 — Observability: stats, telemetry, profiling *(continuous)*

**Depends on:** every state-owner. **Enables:** M6, operators, tests.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W17-1a | Stats core (§31.1, O-002): all byte classes (app/cache/central/backend/metadata/quarantine/hugepage/arena). | M | | classes present; non-negative. ✅ every §31.1 class is in the snapshot/JSON, all `u64` (non-negative by construction): the §20.1 `backend.active_bytes` / `retained_bytes` distinction (Retained = the extent manager's `reserved`), `quarantine.bytes` (present, `0` until plan 08 wires byte accounting), `arena.destroyed` (a real cumulative `ArenaTable::destroyed_count`, bumped at each `*→Destroyed`), alongside the already-present app/cache/central/hugepage/release/topology/placement blocks. |
| W17-1b | Epoch/sequence + consistent-snapshot mode (§8.6): a snapshot reconciles to managed VM modulo the documented convention. | M | ∥ | reconciliation test passes. ✅ a process-global monotonic `STATS_EPOCH` stamps every composed snapshot (`topomalloc_stats_*`); `CONSISTENT_SNAPSHOT` is the §31.2 read-mode flag. The §8.6 convention is documented + tested: `virtual == active + pageheap_free` and `pageheap_free == retained + dirty + muzzy + released` hold *algebraically* (even under a torn concurrent read), and `live + central_free <= active`, `live == allocated − freed` hold at any quiescent point — proven over a live allocator, sequential **and** under concurrent load that then quiesces (`tests/tests/stats.rs`). |
| W17-2 | Snapshot/JSON/print API (§31.2) + flags (SUMMARY/BY_ARENA/BY_SIZE_CLASS/BY_CPU/BY_NUMA/BY_HUGEPAGE); additive-field rule (§35.3). | M | | JSON matches Appendix D shape. ✅ the C `topomalloc_stats_json` / `_print(FILE*)` / `_snapshot(topomalloc_stats_t*)` trio (plus `_json_for_label`) over a **live composed** snapshot — allocator byte classes + the heap sampler + the live NUMA router's §15/§19.7 coverage. `StatsFlags` (the eight §31.2 bits) gate `BY_ARENA` (per-arena lines, reconciling with `arenas.count`) and `BY_SIZE_CLASS` (per-class central-free, summing to `central.free_bytes`); the renderer is additive (§35.3, new fields only append). |
| W17-3a | Sampling mechanism (§31.4): per-thread/per-CPU bytes-between-samples counter (Poisson), **no hot-path lock**. | M | | sampling decision lock-free; rate configurable. ✅ (landed early to feed W14) `topo_core::sampling::Sampler` — a per-thread Poisson "bytes-until-next-sample" counter (fixed-point exponential interval, so the core stays FP-free); the decision touches only thread-local state (no lock/syscall/alloc). Wired into `AnyAllocator::{allocate,free,realloc}`; rate set by `$TOPOMALLOC_SAMPLE_RATE` / `topomalloc_profile_set_rate`. Off by default. |
| W17-3b | Stack capture on a sampled alloc **without recursive malloc** (bounded, alloc-free unwind into a fixed buffer). | M | | unwinder never re-enters the allocator (§31.4). ✅ (early) `StackBuf` — a fixed `[usize; MAX_STACK_FRAMES]` the platform unwinder (`libc::backtrace`, warmed up once at enable) fills *in place*; folds to an opaque `StackId`. A thread-local re-entrancy guard makes the sampled slow path non-re-entrant; `sampling_lifecycle_*` pins that the sampler never re-enters the allocator. |
| W17-3c | Sampled-object bookkeeping: track sampled live objects, free them safely, right-censored lifetime accounting. | M | ∥ | freeing a sampled object is correct + accounted. ✅ (early) `SampledObjects` — a fixed-capacity, alloc-free live set (open addressing + backward-shift deletion); a sampled free resolves the object's lifetime; `fold_censored` right-censors still-live objects at dump. The **free hot path stays lock-free** via the atomic `SampleBloom` (no false negatives), so only a maybe-positive consults the locked set (DD-1 F2). |
| W17-3d | Heap + lifetime profile aggregation + dump format (§31.3). | M | ∥ | profiles dumpable; low overhead. ✅ (early) aggregation **is** `SiteProfileTable` (W14-2); `topomalloc_profile_dump_json` renders the §31.3 dump. (The broader W17 stats core / epoch snapshot / flags / redaction / `explain` remain for M6.) |
| W17-4 | Fragmentation metrics (§31.5) + hugepage coverage (§19.7) wired from plan 04 W11-5. | M | ∥ | internal/external/cache/hugepage fragmentation reported. ✅ a `fragmentation` JSON block: `internal_sampled_bytes` (the §31.5 `Σ(usable − requested)` over the **live sampled set** — the sampler now records both sizes; computed on demand, exact for the live set, `0` when sampling is off), `external_bytes` (dirty + muzzy — free backed bytes not immediately useful), `cache_bytes` (per-CPU + thread + transfer), `hugepage_bytes` (from §19.7 `HugeStats`, wired live through `NodeRouter::coverage`), `metadata_overhead_bytes`. |
| W17-5 | `topo_explain_memory()` (§31.6): a human-readable RSS attribution string. | S | ∥ | returns e.g. "RSS high because: 2.1 GiB live, 700 MiB per-CPU cache, …". ✅ `topomalloc_explain_memory()` + `Stats::explain()` render "RSS is attributed to: 2.5 GiB live, 700.0 MiB per-CPU cache, 1.4 GiB dirty retained, …" — the byte classes named largest-first in binary units (integer-only), with decommitted (`released`) bytes noted apart as managed-VM-not-RSS, and an explicit idle case. |
| W17-6 | **Label-scoped & redacted stats** (§36.12): low domains cannot infer high-domain patterns. | M | | stats-redaction test (§36.16); mirrors `stats_observation_noninterference` (plan 02 W1-12d). ✅ a pure `redact_arenas(lines, observer_label)` keeps only the arenas an observer dominates (`label <= observer`), dropping every higher-domain line; the C `topomalloc_stats_json_for_label` applies it to the `BY_ARENA` detail. Pinned by `redaction_is_label_noninterference` (changing/adding any *higher*-labelled arena leaves the low view bit-for-bit identical) — the Rust analogue of the proved Lean `stats_observation_noninterference`. On POSIX every arena is `PUBLIC`, so a `PUBLIC` observer sees everything (identity); the redaction has teeth under the seLe4n multi-label profile (plan 09). |

> **▸ Decomposition — W17-3 (sampled profiling), the "don't re-enter yourself" problem.** Split the *sampling
> decision* (W17-3a, must be lock-free on the hot path), the *stack capture* (W17-3b, must unwind without
> calling `malloc` — the unwinder writing into a heap buffer would recurse into the allocator from inside an
> allocation), the *sampled-object lifecycle* (W17-3c, including right-censored lifetimes for objects still
> live at dump time), and *aggregation* (W17-3d). W17-3b is the trap the SPEC calls out (§31.4, Appendix F:
> "making profiling callbacks allocate through the same allocator recursively"); it uses a fixed,
> pre-allocated buffer and an alloc-free unwinder.

> **▸ Decomposition — W17-1 (stats consistency).** Split *the counters* (W17-1a) from *epoch-consistent
> snapshotting* (W17-1b). The hard part is §8.6: a snapshot taken while threads allocate must still reconcile
> (sum of parts == managed VM, modulo a documented convention). An epoch/sequence number lets readers get a
> coherent view without locking the fast path.

> **▸ Implementation status.** W17 is **landed** (all units, ahead of its M6 slot). The pure renderer is
> `topo-stats` (`Stats` summary + `StatsFlags` + `StatsDetail` + `ArenaLine`/`SizeClassLine` + `redact_arenas`
> + `explain`); the live composer + C surface is `crates/topo-abi/src/stats_api.rs`
> (`topomalloc_stats_json` / `_json_for_label` / `_print` / `_snapshot` + `topomalloc_explain_memory`, with a
> monotonic `STATS_EPOCH`). Composition reads the running engine (`AllocatorStats`, with the new
> `arenas_destroyed` from `ArenaTable::destroyed_count` and the per-class central-free decomposition), the
> heap sampler (`PlacementStats` + the new §31.5 `sampled_internal_fragmentation_bytes`, fed by `usable` now
> carried on each `SampledRecord`), and — under `hugepage-optimized` — the live router (`NodeRouterStats` +
> the new `NodeRouter::coverage` / `RouterControl::coverage`). Stats are **derived observability**, not an
> abstract state-machine transition, so there is **no §33.4 obligation**: the reconciliation is pinned by the
> fixed-wall `tests/tests/stats.rs` battery, and the W17-6 redaction is the Rust analogue of the proved Lean
> `stats_observation_noninterference` theorem (pinned by `redaction_is_label_noninterference`). The control
> namespace (W20) and the C header gain the matching keys/symbols; the ABI struct/flags are frozen by the
> two-sided ABI smoke tests. The broader **stats epoch *snapshot-isolation*** (a true seqlock over the fast
> path) and the **seLe4n resource-server-enforced** per-label aggregate scoping remain the deferred pieces
> (the §8.6 bounded-skew convention + the per-arena redaction cover the POSIX profile today).

---

## W14 — Lifetime, hotness & placement policy

**Depends on:** plan 04 W11 (filler), W17 (sampling). **Enables:** M6. **Safety boundary (§24.5):** placement
affects locality/fragmentation only — **never** validity, size, alignment, or free correctness.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W14-1 | Hint plumbing (§24.1): hot/cold + lifetime flags from §10.4 into the filler. | M | | flags reach the filler; ignored-safely if absent. ✅ the public `TOPO_HOT`/`TOPO_COLD`/`TOPO_LIFETIME_*` flags decode to `RequestFlags` → `Hints` → `req.flags.hints_with_numa()` → `large.allocate_in_hinted()` → the filler's `PlaceHints { hotness, lifetime }`; a hint-less path (incl. the §18.6 seam) places with `PlaceHints::default()` (neutral/unspecified), so absent hints are ignored safely. |
| W14-2 | Lifetime classes (§24.2) + allocation-site profile record (§24.4): stack_id, size-class dist, lifetime histogram, hotness, rates, confidence. | M | ∥ | sampled profiles recorded; missing/wrong profiles never break safety (§24.5). ✅ `crates/topo-core/src/placement.rs`: the six §24.2 `LifetimeClass`es; the §24.4 `AllocationSiteProfile` (all eight fields — `stack_id`, a bounded Space-Saving `SizeClassDist` with overcount bounds, the `LifetimeHistogram` incl. right-censored, a recency-weighted (EWMA) hotness with a MAD *stability* gate, EWMA alloc/free rates, `sampled_live_bytes`, per-dimension + combined `confidence_bp`); and `SiteProfileTable`, a pure/`no_std`/host-driven **16-way set-associative** learning policy (record/lookup/`place_hints`/`write_learned_hints`, least-confidence replacement). Fed **live** by the W17-3 sampler. The output is only advisory `PlaceHints`, so a missing/wrong profile can never change size/alignment/validity — the `learned_profile_*` tests + the `placement` fuzz target pin it. |
| W14-2-loop | **Learn → place loop (live).** Confident, *consistent* per-bucket consensus is published into the engine's lock-free `LearnedHints` table; the allocation path reads it (one atomic acquire load) and a *placement-unhinted* request adopts it, an explicit hint always winning. | — | | learned profiles steer live placement; lock-free hot path; placement unchanged when nothing is learned. ✅ `SiteProfileTable::write_learned_hints` → `Allocator::{publish_learned_hints,learned_hints}`; the sampler republishes on a bounded cadence; disabling clears it. Proven end-to-end by `learned_hot_profile_steers_unhinted_large_allocations_hot` (a published hot profile sends *unhinted* large allocations to the hot-dense bin) with the `no_learned_hint_*` contrast. |
| W14-3 | Cold/short/long handling (§24.6–§24.8): grouping policy in the filler (cold spans, short-lived grouped, long-lived hot densely packed). | M | | grouping observable in stats; **safety-boundary test** (placement never changes size/align/validity). ✅ **Two layers.** *Hugepages:* the W11 filler groups by the §24.6–§24.8 axes (cold → `ColdSparse`, same-lifetime via open-fresh-on-mismatch, long+hot → `HotDense`), observable as the §19.4 `bin_counts`. *Spans (§24.6/§24.7):* small objects are segregated into cold / hot / short-lived span pools — a `PlaceClass`-tagged span + class-preferring `CentralCache::remove_batch` (with an `ANY_PLACE_CLASS` availability fallback so grouping never causes a spurious OOM, §2.4); an all-default program keeps one pool per size class (no RSS regression). The **fixed-wall safety-boundary test** `engine_size_align_validity_free_are_invariant_under_hints` (+ pure-filler, proptest, fuzz, and `learned_profile_hints_uphold_the_wall` companions, and the §36.9 G-sim `safety_wall_holds_identically_over_sele4n_sim`): for every `(size, align)`, under *every* hint (incl. adversarial / learned), the usable size, alignment, writability, and free path are identical. |

> **▸ Decomposition — W14 (placement) and its safety boundary.** The whole workstream is *policy*; the single
> non-negotiable is W14-3's safety-boundary test, which asserts that no placement decision changes an object's
> size, alignment, validity, or free path (§24.5). Splitting hints (W14-1), profiles (W14-2), and grouping
> (W14-3) lets the learned-profile machinery evolve while the safety boundary stays a fixed, tested wall.

> **▸ Implementation status.** W14 is **landed** (ahead of its M6 slot). W14-1 rides the existing W11 hint
> plumbing (flags → `Hints` → filler `PlaceHints`). W14-2 is `crates/topo-core/src/placement.rs`: the §24.2
> `LifetimeClass` taxonomy, the §24.4 `AllocationSiteProfile` record, and the `SiteProfileTable` learning
> policy — a **pure, `no_std`, host-driven** object (the same pattern as the W12 `ReleaseController`), with a
> bounded Space-Saving size-class summary, a right-censored lifetime histogram, per-dimension confidence, and
> a total `place_hints` query. W14-3 is the W11 filler grouping (already live) plus the **fixed-wall**
> safety-boundary test. To feed the policy from *real* traffic, the **minimal W17-3 sampling slice**
> (`crates/topo-core/src/sampling.rs` + the `topo-abi` glue) landed alongside it: a lock-free per-thread
> Poisson decision, an alloc-free `libc::backtrace` capture into a fixed buffer, a lock-free
> `SampleBloom`-gated sampled-object lifecycle, and a re-entrancy guard — all **off by default**, enabled by
> `$TOPOMALLOC_SAMPLE_RATE` / `topomalloc_profile_set_rate`. Because placement is **policy, not a modeled
> transition** (§2.4 — exactly as for the W13 NUMA router), there is **no Lean obligation and no trace-grammar
> change**; the profiler's running counters reconcile into `topo-stats` JSON (`placement` block) and the
> `topo.placement.*` control namespace (these are *profiling* estimates, not a managed-VM byte class, so they
> sit outside the §8.6 reconciliation). The safety boundary holds **by construction** — the policy's only
> output is advisory `PlaceHints`, the score-only input the certified filler already tolerates — and is
> **pinned, not merely asserted**, by the fixed-wall `engine_size_align_validity_free_are_invariant_under_hints`
> (every size × align × hint leaves usable size / alignment / validity / free path identical), the
> reference the W13 router's `placement_never_breaks_the_allocation_contract` wall mirrors.
>
> **Optimal-completion pass.** The loop is **closed end-to-end**: confident per-bucket consensus is published
> into a lock-free `LearnedHints` table the allocation path reads (one atomic acquire load; explicit hints
> override; the placement is unchanged when nothing is learned), so learned profiles steer *live*
> placement — for medium/large through the filler, and for **small objects** through new §24.6/§24.7
> `PlaceClass`-tagged span pools (class-preferring `CentralCache::remove_batch` with an `ANY_PLACE_CLASS`
> availability fallback, so grouping never causes a spurious OOM). The profile quality was hardened
> (event-driven EWMA rates, a MAD-stability-gated recency-weighted hotness, a 16-way set-associative table).
> The W17-3 verification matches the SPEC's prescribed methods: the **`sampler_no_alloc`** test installs a
> counting `#[global_allocator]` and proves the sampled path makes **zero** heap allocations across 50k
> sampled allocations (§31.4 / Appendix F); a **sampling-overhead** criterion bench (off vs on) bounds the
> hot-path cost; the membership filter auto-refreshes to cap its false-positive rate; and a **§36.9 G-sim**
> test re-proves the §24.5 safety wall + the learn → place loop identically over `Sele4nSim`.

---

## W20 — Configuration & control plane *(continuous)*

**Depends on:** W17 (stats controls). **Enables:** M4, operators.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W20-1 | Config sources + precedence (§32.1): env/file/linker/init-API/runtime/build; security envs can disable env config. | M | | precedence documented + tested; env-disable works. |
| W20-2 | Knob set (§32.2) wired to behavior; safe server defaults (§32.3). | M | ∥ | each knob has effect + default; defaults match §32.3. |
| W20-3 | Control namespace (§10.5/Appendix E): stats/cache/release/arena/profile/emergency controls; blocking controls documented. | M | | every Appendix E entry resolves; blocking ones flagged. |
| W20-4 | Runtime-change validation (§32.4): immediate vs future vs quiescence-required. | S | ∥ | each change classified + enforced. |

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · Sampled profiling without recursive malloc (W17-3)

**Problem.** Heap/lifetime profiling must capture a stack trace on a *sampled* allocation, but the obvious
implementation — an unwinder that allocates a buffer — re-enters the allocator *from inside an allocation*,
the exact anti-pattern the SPEC forbids (Appendix F, §31.4). It must also be lock-free on the hot path and
correctly account objects still live at dump time (right-censored lifetimes).

**Design space.** **A lock-free per-thread Poisson sampling counter + an allocation-free unwinder writing into
a fixed, pre-allocated per-thread buffer** — chosen. Sampling decisions touch only thread-local state; the
unwind never calls back into `malloc`.

**Structures.**
```text
thread-local: bytes_until_sample (Poisson), sample_buffer[FIXED]   // pre-allocated, never grows
sampled set:  map<ptr, {stack_id, size, alloc_epoch}>              // for live + lifetime accounting
```

**Work breakdown (refines W17-3a..d).** 1. lock-free sampling decision (W17-3a). 2. alloc-free stack capture
into the fixed buffer (W17-3b). 3. sampled-object lifecycle: record on sampled alloc, resolve on free, count
right-censored (still-live) objects at dump (W17-3c). 4. heap + lifetime aggregation + dump format (W17-3d).

**Invariants.** the unwinder never re-enters the allocator; sampling never takes a hot-path lock; freeing a
sampled object is correct whether or not it was sampled.

**Verify.** a re-entrancy guard counts allocator depth and asserts the sampler never increments it; an
overhead micro-benchmark bounds the sampled-path cost (a G-ops check); a lifetime test injects known-lifetime
objects and checks the histogram including right-censoring.

**Failure modes.** *F1* unwinder allocates → recursion → fixed buffer + alloc-free unwind. *F2* a sampled
object freed on another thread → the sampled set is concurrency-safe (sharded/lock-free). *F3* sampling cost
creeps → the overhead budget gate.

**Sequencing.** **M6**.

### DD-2 · Epoch-consistent stats (W17-1)

**Problem.** A stats snapshot taken while threads allocate must still *reconcile* — the sum of the parts
equals managed VM, modulo a documented convention (§8.6) — without locking the allocation fast path to take
the snapshot.

**Design space.** **Per-shard relaxed counters + an epoch/sequence number for consistent snapshots** —
chosen: fast paths bump relaxed per-CPU/per-arena counters; a snapshot advances the epoch and reads a coherent
set, accepting bounded skew that the reconciliation convention accounts for.

**Work breakdown (refines W17-1a/b).** 1. the byte-class counters (app/cache/central/backend/metadata/
quarantine/hugepage/arena) as sharded relaxed atomics (W17-1a). 2. epoch/sequence + a consistent-snapshot
mode that reconciles (W17-1b).

**Invariants.** every byte the allocator manages is in exactly one class; a `CONSISTENT_SNAPSHOT` reconciles
to managed VM modulo the convention; counters are non-negative.

**Verify.** a reconciliation test under concurrent load asserts the snapshot balances; the JSON matches
Appendix D; fields are additive across the release series (§35.3).

**Failure modes.** *F1* double-counting a byte in two classes → the five-term partition (plan 03 W5-3) is the
source of truth for cached/central/live; stats derive from it. *F2* a snapshot that never balances →
the documented convention bounds the skew and the test enforces it.

**Sequencing.** W17-1a **M1** (so M1 reconciles); W17-1b **M6**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M1 | W17-1a (basic stats so M1 reconciles); `topomalloc_version` (plan 01 W0-13). |
| M2 | stats grow with caches (cache bytes, miss/overflow); W20-1/2 basic knobs. |
| M4 | W20-3 control namespace (arena/cache/release controls land with arenas). |
| M5 | W17-4 (fragmentation + hugepage coverage from plan 04). |
| M6 | W17-1b (epoch snapshot), W17-2 (JSON/flags), **W17-3 (sampling)**, W17-5 (explain), W17-6 (label-scoped), W14 (placement). |

## Domain risks

- **R10** (info-flow leak via stats) — owned with plans 08/09: W17-6 redaction + the non-interference theorem.
- *Local:* sampling overhead creep. *Mitigation:* W17-3a is lock-free and rate-bounded; an overhead budget is
  a G-ops check.

## Definition of Done (addendum)

This plan **collects the tax**: any WU in any plan that adds state must, in the same PR, add its bytes to
W17-1 and confirm reconciliation (§8.6); any WU that adds a knob updates W20. Sampling never allocates through
the allocator it samples.

## Best-practices checklist

- [ ] Stats reconcile (sum of parts == managed VM, modulo a documented convention).
- [ ] The sampler is lock-free on the hot path and never re-enters `malloc` while unwinding.
- [ ] Stats JSON fields are additive across a release series (§35.3).
- [ ] Placement never affects validity/size/alignment — the safety-boundary test is a fixed wall.
- [ ] Low-domain stats cannot reveal high-domain patterns (redaction + non-interference).
- [ ] `topo_explain_memory()` exists — RSS must be explainable, not just measurable.

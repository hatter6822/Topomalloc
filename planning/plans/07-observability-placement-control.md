# Plan 07 — Observability, Placement & Control

**Workstreams:** W17 (stats/telemetry/profiling), W14 (lifetime/hotness/placement), W20 (config/control plane)
· **Status:** rev 2.0 · **Overview:** [README.md](README.md)
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
| W17-1a | Stats core (§31.1, O-002): all byte classes (app/cache/central/backend/metadata/quarantine/hugepage/arena). | M | | classes present; non-negative. |
| W17-1b | Epoch/sequence + consistent-snapshot mode (§8.6): a snapshot reconciles to managed VM modulo the documented convention. | M | ∥ | reconciliation test passes. |
| W17-2 | Snapshot/JSON/print API (§31.2) + flags (SUMMARY/BY_ARENA/BY_SIZE_CLASS/BY_CPU/BY_NUMA/BY_HUGEPAGE); additive-field rule (§35.3). | M | | JSON matches Appendix D shape. |
| W17-3a | Sampling mechanism (§31.4): per-thread/per-CPU bytes-between-samples counter (Poisson), **no hot-path lock**. | M | | sampling decision lock-free; rate configurable. |
| W17-3b | Stack capture on a sampled alloc **without recursive malloc** (bounded, alloc-free unwind into a fixed buffer). | M | | unwinder never re-enters the allocator (§31.4). |
| W17-3c | Sampled-object bookkeeping: track sampled live objects, free them safely, right-censored lifetime accounting. | M | ∥ | freeing a sampled object is correct + accounted. |
| W17-3d | Heap + lifetime profile aggregation + dump format (§31.3). | M | ∥ | profiles dumpable; low overhead. |
| W17-4 | Fragmentation metrics (§31.5) + hugepage coverage (§19.7) wired from plan 04 W11-5. | M | ∥ | internal/external/cache/hugepage fragmentation reported. |
| W17-5 | `topo_explain_memory()` (§31.6): a human-readable RSS attribution string. | S | ∥ | returns e.g. "RSS high because: 2.1 GiB live, 700 MiB per-CPU cache, …". |
| W17-6 | **Label-scoped & redacted stats** (§36.12): low domains cannot infer high-domain patterns. | M | | stats-redaction test (§36.16); mirrors `stats_observation_noninterference` (plan 02 W1-12d). |

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

---

## W14 — Lifetime, hotness & placement policy

**Depends on:** plan 04 W11 (filler), W17 (sampling). **Enables:** M6. **Safety boundary (§24.5):** placement
affects locality/fragmentation only — **never** validity, size, alignment, or free correctness.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W14-1 | Hint plumbing (§24.1): hot/cold + lifetime flags from §10.4 into the filler. | M | | flags reach the filler; ignored-safely if absent. |
| W14-2 | Lifetime classes (§24.2) + allocation-site profile record (§24.4): stack_id, size-class dist, lifetime histogram, hotness, rates, confidence. | M | ∥ | sampled profiles recorded; missing/wrong profiles never break safety (§24.5). |
| W14-3 | Cold/short/long handling (§24.6–§24.8): grouping policy in the filler (cold spans, short-lived grouped, long-lived hot densely packed). | M | | grouping observable in stats; **safety-boundary test** (placement never changes size/align/validity). |

> **▸ Decomposition — W14 (placement) and its safety boundary.** The whole workstream is *policy*; the single
> non-negotiable is W14-3's safety-boundary test, which asserts that no placement decision changes an object's
> size, alignment, validity, or free path (§24.5). Splitting hints (W14-1), profiles (W14-2), and grouping
> (W14-3) lets the learned-profile machinery evolve while the safety boundary stays a fixed, tested wall.

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

# Plan 04 — Backend, Hugepages, Release & Topology

**Workstreams:** W4 (backend seam + POSIX), W11 (hugepage/large-mapping), W12 (release controller), W13
(topology) · **Status:** rev 2.6 — **plan 04 complete: W4 + W11 + W12 + W13 all landed.** (rev 2.6 completes
W13 *optimally*: genuine first-touch for `OsDefault` — an unbound default backend — real `sched_getcpu`
`Local`, nearest-node spillover, and a host-driven `topomalloc_numa_*` C control surface for
rebalance/refresh/idle-release. rev 2.5 took W13 live via the `NodeRouter`; rev 2.4 hardened the §15.4
rebalancer to move only a donor's *surplus*.) W4 (the seam + POSIX backend + extents + large path);
**W11 landed (all units, ahead of M5): the hugepage filler / huge cache / region cache as a real,
backend-agnostic placement subsystem over the provider seam, wired into the live large path through the
§18.6 `RegionCacheHook`**; **W12 landed (all units, ahead of M5): the memory release controller &
background-purge pump — the §21.3 ladder / §21.4 demand reserve / §21.5 pressure modes as a pure,
host-driven policy, wired live into the W11 demand-reserve hook via `HugePageBackend::release_tick`**;
**W13 landed and live (all units): the §15 CPU/LLC/NUMA topology model + sysfs discovery + best-effort
`mbind`, and the `NodeRouter` (per-node hugepage backends) that makes placement + the cross-domain
rebalancer affect real allocations under the `hugepage-optimized` ABI** ·
**Overview:** [README.md](README.md)
**SPEC anchors:** §18, §20, §21, §19, §15, §36.6, §36.9, §36.11; M-004/M-005, H-001..H-005, O-007.
**Upstream deps:** [03](03-core-allocator.md) (pagemap/spans). **Downstream:** [03](03-core-allocator.md)
(spans come from extents), [09](09-sele4n-integration.md) (the capability provider implements the same seam).
**Milestones:** the seam at **M0/M1**; topology at **M3**; hugepage + release at **M5**.

> This plan owns **the central seam** (§3 of the overview). Everything OS/kernel goes through
> `TopoBackingProvider`; the allocator core never calls `mmap` or `retype`. POSIX is the *degenerate single
> ambient-authority, single-label* case; [plan 09](09-sele4n-integration.md) supplies the capability case
> behind the identical interface.

## The seam (owned here, consumed by 03/06/09)

```rust
/// Generalized from SPEC §36.6 so POSIX is the single-authority degenerate case.
trait TopoBackingProvider {
    fn reserve_window(&self, arena: ArenaId, size: usize, align: usize, rights: Rights) -> Result<VWindow, Err>;
    fn create_frame(&self, arena: ArenaId, size_bits: u32, label: Label)            -> Result<Frame, Err>;
    fn map_frame(&self, arena: ArenaId, f: Frame, w: VWindow, rights: Rights, cache: CachePolicy) -> Result<MappedRange, Err>;
    fn unmap_frame(&self, arena: ArenaId, m: MappedRange)                            -> Result<Frame, Err>;
    fn commit(&self, r: &MappedRange, off: usize, len: usize)   -> Result<(), Err>;
    fn decommit(&self, r: &MappedRange, off: usize, len: usize) -> Result<(), Err>;
    fn purge_lazy(&self, r: &MappedRange, off: usize, len: usize)   -> Result<(), Err>;
    fn purge_forced(&self, r: &MappedRange, off: usize, len: usize) -> Result<(), Err>;
    fn revoke_descendants(&self, arena: ArenaId, cap: Cap) -> Result<(), Err>;   // no-op on POSIX
    fn recycle(&self, arena: ArenaId, m: MappedRange)      -> Result<(), Err>;
}
```

On POSIX, `Frame`/`VWindow`/`Cap` collapse to address ranges, `rights` to `mprotect` bits, and
`revoke_descendants` is a no-op; on seLe4n they are real capabilities. The provider **state machine** (§36.6,
`AuthorizedUntyped → … → RecyclableUntyped`) is modeled in Lean (plan 02 W1-11b) and asserted at runtime.

---

## W4 — Back-end seam & POSIX provider

**Depends on:** plan 01 for **W4-1 (trait + stubs is an M0 deliverable, depends only on plan 01)**; plan 03 W3
for **W4-2 onward** (extent ops touch the pagemap). **Enables:** W5, plan 06 (arenas), W11, plan 09.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W4-1 | Define `TopoBackingProvider` (above) with POSIX + `Sele4nSim` stub impls; provider-state-machine type (§36.6). seLe4n side compiles against real `sele4n-abi`/`sele4n-types` (D8). | M | | trait compiles; M0 skeleton runs over either; seLe4n side type-checks vs pinned upstream. |
| W4-2a | Extent descriptor (§18.2) + free-extent index (by size **and** address) for coalescing. | M | | index supports best/first-fit + neighbour lookup. |
| W4-2b | `alloc` + `split` (§18.4): both results page-aligned; metadata installed *before* publication; pagemap update atomic wrt readers (via W3-6). | M | | split rules enforced; no torn publish. |
| W4-2c | `merge`/coalesce (§18.4): adjacency, arena-compat, state-compat, hugepage-accounting update, no stale descriptors visible to classification. | M | ∥ | merge rules enforced; mirrors plan 02 W1-8a. |
| W4-2d | `commit/decommit/purge_lazy/purge_forced/release` with pre/postconditions mirrored in Lean (W1-8c); enforces M-004 (no live in released) + M-005 (recommit before use). | M | | preconditions checked; failure leaves state well-formed (W4-5). |
| W4-3a | POSIX physical-state mapping (§20.4): `madvise(DONTNEED/FREE)` and `mprotect` mapped to dirty/muzzy/released; documented per platform. | M | ∥ | mapping documented; states reconcile in stats (plan 07). |
| W4-3b | Retain-vs-unmap policy (§20.5): retain by default on 64-bit; unmap more aggressively in debug/low_rss. | S | ∥ | policy honored per profile. |
| W4-4 | Large allocation path (§18.5) + region-cache hook point (§18.6, filled by W11-3). | M | | large allocs bypass small caches; round-overflow safe (plan 06 W2-4). |
| W4-5 | Backend failure semantics: every op fallible, leaves state well-formed (mirrors §36.6). | S | | failure-injection test keeps invariants green. |

> **▸ Decomposition — W4-2 (extents).** Split into the *index* (W4-2a, the data structure that makes
> coalescing cheap), *alloc/split* (W4-2b, which must install metadata before publishing and update the
> pagemap through W3-6), *merge* (W4-2c, with the §18.4 compatibility gates), and *physical-state ops*
> (W4-2d, mirrored to the Lean release-safety theorem). Keeping split and merge separate matters because they
> have different preconditions and different stale-descriptor hazards; coalescing (merge) is where most
> backend use-after-free bugs hide.

---

## W11 — Hugepage / large-mapping backend

**Depends on:** W4, plan 03 W5. **Enables:** M5. Reuses the same placement model on seLe4n over contiguous
normal-frame runs (§36.9).

| WU | Description | Size | ∥ | Acceptance | Status |
|---|---|---|---|---|---|
| W11-1a | HugeAllocator: reserve hugepage-aligned virtual ranges (§19.2). | M | | reservations hugepage-aligned. | ✅ `HugePageBackend::new` reserves a `HUGEPAGE_SIZE`-aligned region; `reserve_hugepages` carves contiguous whole-hugepage runs. |
| W11-1b | HugeCache: cache empty *backed* hugepages for quick reuse; demand-reserve hook (W12). | M | ∥ | empty-backed reuse avoids immediate faults. | ✅ a freed hugepage stays `EmptyBacked` (committed); reuse hits the same backed pages with no `commit_run` (no fault). `release_empty_excess(reserve)` is the W12 demand-reserve hook: it returns the backing of empty hugepages beyond `reserve` to the OS. |
| W11-2a | HugePageFiller bin set (§19.4: empty_backed/nearly_empty/sparse/medium/nearly_full/full/partial_subreleased/cold_sparse/hot_dense) as the structure; each hugepage in **exactly one** bin; bin transitions on occupancy change. | M | | H-003: bin membership consistent with occupancy. | ✅ nine `HugeBin`s; `classify_bin` is total; `refile` re-files on every occupancy/state change (H-003 by construction). |
| W11-2b | Candidate selection/scoring (§19.3): approximate-bin scan; packing/locality/lifetime/hotness/release-preservation bonuses minus fragmentation/cross-numa/partial-subrelease penalties. | M | | no full scan of all hugepages; deterministic in test mode. | ✅ `PACKING_ORDER` scan capped at `SCAN_CAP`; `score` is a deterministic `i64` over packing/hotness-match/**lifetime-match**/release-preservation/fragmentation/partial-subrelease (locality/cross-NUMA score 0 until W13 topology), ties break on lowest base. The `PlaceHints` (hotness+lifetime) flow from the request flags through the seam to the filler. |
| W11-2c | Bin↔occupancy consistency invariant + tests (H-002/H-003): occupancy bytes == sum of contained spans/large allocs. | M | ∥ | invariants checked in debug (B.4). | ✅ `check_invariants` verifies `live ⊆ committed`, `committed ∩ released = ∅`, the filed bin == `target_bin`, and the bin-list counts; `debug_assert`ed after every mutation + fuzzed. |
| W11-3 | RegionCache for awkward sizes (§18.6): allocations slightly larger than a hugepage avoid rounding to multiple full hugepages. | M | ∥ | waste on awkward sizes bounded; tested. | ✅ a validating `RegionCache` re-reserves freed empty-backed runs in O(1) (stale entries pruned, never double-vended); awkward runs reused, not re-rounded. |
| W11-4a | Packing policy (§19.5): pack same-lifetime/hot-dense; prefer partially-used hugepages; keep some empty-backed in HugeCache. | M | | policy observable in stats; never misplaces a live object. | ✅ packing-ordered scan fills denser hugepages first; a run is carved from the free bitmap, so the score can never misplace a live object. The policy's **effect is observable in stats**: `HugeStats::bins` (rendered as `hugepage.bin_counts` in the JSON) reports the live set's distribution across the nine §19.4 bins, and it reconciles with the touched-hugepage count (H-003). Same-lifetime packing is enforced: when the best partial hugepage's score loses to a fresh hugepage's (a strong lifetime/hotness mismatch), placement opens a fresh hugepage instead of mixing lifetimes (bounded by region capacity). |
| W11-4b | Partial-subrelease guards (§19.6/H-005): subrange has no live object, aligned to release granularity, gated on coldness/pressure, recorded as a metric. | M | ∥ | partial subrelease only when all guards pass; metric emitted. | ✅ `subrelease` refuses any run intersecting a live page (H-005), is page-aligned, gated on cold/sparse-or-pressure, **and a real §19.6 cost/benefit test** (predicted RSS benefit ≥ a fragmentation cost that scales with the hugepage's live coverage); on success the backend does §36.6 revoke-before-decommit; the §19.6 metric is emitted; the W12 `mark_cold` hook is provided. |
| W11-5 | Coverage metrics (§19.7) exported to stats (plan 07): coverage_bytes, intact/partial live bytes, empty_backed/released, partial_subreleased, fragmentation, coverage_ratio. | S | ∥ | all §19.7 fields present; ratio computed. | ✅ `HugeStats` carries all §19.7 fields; `Stats::record_huge` renders them (+ `coverage_ratio_bp` and the §19.4 `bin_counts` distribution) in the stats JSON. |
| W11-6 | seLe4n large-mapping policy (§36.9): same placement over contiguous normal-frame runs; prefer whole-mapping release. | M | | correct when every backing range is normal pages; Sim test (plan 09). | ✅ a §36.9 **G-sim slice** (`filler_outcome_is_identical_over_posix_and_the_sele4n_simulator`) runs the identical workload over POSIX and `Sele4nSim` and asserts an **identical** abstract outcome; `subrelease` does §36.6 revoke-before-decommit so the seLe4n release path is capability-correct. |

> **▸ Implementation status.** W11 is **landed** (ahead of its M5 slot), in `crates/topo-core/src/huge.rs`,
> and **wired live** through the engine. The pure `HugePageFiller` (per-hugepage live/committed/released
> page bitmaps + the nine §19.4 bin lists) is the correctness object — H-002/H-003 by construction, H-005
> enforced by the `subrelease` guard — and the provider-driven `HugePageBackend` wraps it behind the §27.2
> backend lock, drives `revoke`/`commit`/`decommit`, and implements the §18.6 `RegionCacheHook` (now
> hint-carrying, §19.3/§19.5). `Allocator::new_with_huge` (the `hugepage_optimized` configuration) routes
> every medium/large allocation through the filler — carrying the request's hotness/lifetime hints — with
> the small/free paths byte-for-byte unchanged; `Allocator::new` keeps the M1 extent path. The §19.4
> `classify_bin` is pinned to the Lean `TopoMalloc.Huge.HugeBin.classifyBin` by
> `huge_bin_classification_matches_lean` + the `lake exe check` `hugeBinGate`; H-002/H-003 and the per-page
> place/free/subrelease state machine (H-001/H-004/H-005 preservation) are modeled in
> `lean/TopoMalloc/HugePageFiller.lean`. The integration test `tests/tests/hugepage.rs` drives the live
> engine + G-sim slices; the `hugepage_filler_stays_well_formed_and_reconciles` **gating** proptest
> (`tests/tests/property.rs`) and the nightly `fuzz/fuzz_targets/huge_filler.rs` both drive arbitrary
> place/free/subrelease/unsubrelease/reserve-run/free-run/mark-cold streams against the §19.8 invariants +
> the §19.7 coverage reconciliation (with shrinking in the proptest, deeper campaigns in the fuzzer);
> `crates/topo-core/benches/huge.rs` is the non-gating criterion harness measuring place/free churn and
> placement into a fragmented region **swept across filler sizes** (a flat curve is the evidence the
> candidate scan is bounded, not O(hugepages)). The live C `malloc`/`free` entry points already run over
> the filler **under the `hugepage-optimized` feature**: `topo-abi`'s `build_posix_allocator` constructs a
> `HugePageBackend` over the POSIX provider and serves the named `"posix"` allocator through
> `Allocator::new_with_huge`, gated so the default MIT artifact stays byte-for-byte the M1 extent path.
> What W11 leaves to **M5** (not a W11 concern): making the hugepage path the **unconditional** process
> `malloc` backing (a plan-10 deployment wiring — the feature seam exists; flipping the default is M5) and the W12 release controller (pressure modes /
> demand-reserve *policy*) that drives `mark_cold` and `release_empty_excess` (the *mechanisms* W11 provides).

> **▸ Decomposition — W11-2 (hugepage filler).** Splitting *bins* (W11-2a) from *scoring* (W11-2b) matters
> because bins are the **correctness** object (H-003: exactly one bin, consistent with occupancy) while the
> score is pure **policy** (§2.4: a wrong score may hurt fragmentation but must never misplace a live object).
> Bins also let the filler avoid scanning every hugepage (§19.3 "approximate bins"). Partial subrelease
> (W11-4b) is split out because it is the single most dangerous hugepage op — it must *never* intersect a
> live object (H-005) — so its guards are isolated and individually testable. Keep all scoring inputs
> backend-agnostic so W11-6 (seLe4n) reuses the model over normal-frame runs without assuming a hardware
> hugepage exists.

---

## W12 — Memory release controller & background purging

**Depends on:** W4, plan 05 W6 (cache drain), W11. **Enables:** M5.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W12-1a | Per-arena decay config (§20.2): `dirty_decay_ms`, `muzzy_decay_ms`, `release_rate`, `background_purge_enabled`. | S | ∥ | knobs wired (plan 07). |
| W12-1b | Background purge worker (§20.3): off hot paths, fair across arenas, respects pressure, yields under CPU pressure, exposes backlog. | M | | no purge on the alloc fast path; backlog in stats. |
| W12-2a | Inputs vector (§21.2): live/rss/dirty/muzzy/coverage, alloc/free/refill rates, cgroup current/max, pressure notifications, NUMA pressure, hints. | M | | all §21.2 inputs sampled cheaply. |
| W12-2b | Release priority ladder (§21.3/§A.5): drain idle caches → release empty hugepages → purge dirty (not hot) → dirty→muzzy → subrelease cold-sparse → emergency shrink. | M | | ladder applied in order; each step gated by pressure mode. |
| W12-2c | Demand reserve (§21.4) + anti-oscillation: reserve = f(recent rate, peak, refill latency, pressure); prevents release-then-refault. | M | ∥ | refault-loop oscillation test passes. |
| W12-3a | Pressure modes (§21.5): Normal / Soft / Hard / Emergency triggers + behaviors. | M | | mode transitions tested against simulated pressure. |
| W12-3b | **Emergency mode** (O-007) + bounded emergency reserve (§36.5): bypass optional caches, release aggressively, disable HugeCache reserve; reserve never depends on the normal heap. | M | ∥ | emergency path tested; reserve independent. |
| W12-4 | Latency classes (§36.11) annotated on slow paths; arena flags `no_ipc_fast_only`/`bounded_slow_path`/`may_block`. | S | ∥ | each slow path tagged; real-time arenas can forbid blocking. ✅ `LatencyClass` (FastOnly/BoundedSlow/MayBlock) subsumes the three flags as `ArenaPolicy::latency`; `ReleaseController::for_arena` adopts it as `max_latency`, so a fast-only arena skips every blocking ladder rung. |

> **▸ Implementation status (W12).** **Landed** (ahead of its M5 slot), in
> `crates/topo-core/src/release.rs`: a **pure, `no_std`, host-driven** `ReleaseController` —
> `tick(now_ms, inputs) -> ReleasePlan` with an injected clock, so it runs identically over POSIX and
> seLe4n and is fully deterministic/unit-testable. **W12-1a** the §20.2 decay config is consolidated
> onto `arena::DecayConfig` (extended with `release_rate_bytes_per_sec`/`background_purge_enabled` +
> `low_rss`/`debug`/`server` presets) and wired into `ArenaPolicy`. **W12-2a** `ReleaseInputs` is the
> §21.2 vector (rates derived from cumulative-counter deltas). **W12-3a** `PressureMode` is the §21.5
> Normal/Soft/Hard/Emergency ladder with escalate-now / de-escalate-past-the-margin hysteresis
> (alloc-failure / cgroup-critical force Emergency, O-007). **W12-2c** `demand_reserve` is the §21.4
> anti-oscillation brake (grows with the alloc rate + refill cost, caps at the §21.4 **`recent_peak`** of
> releasable-free memory — a *leaky peak-hold* that relaxes the peak **anchor** toward current free over
> `PEAK_DECAY_MS`, decayed by the anchor's *age* so it is independent of tick cadence — so a transient free
> spike does not pin the cap high and over-retain RSS, attenuates with pressure; Emergency
> reserves nothing, §36.5). **W12-2b** the §21.3 six-rung ladder is gated by
> mode + the §36.11 latency ceiling and rate-capped (§20.2) with the unmet remainder accrued as backlog
> (§20.3, W12-1b). The backlog is the **max** of (carried backlog, this tick's desire), never their sum:
> the plan is recomputed from the absolute current supply each tick, so summing would double-count a
> rate-capped persistent supply and let the backlog diverge past the memory that exists. **W12-3b** a
> heap-independent emergency reserve is fixed at construction. It is wired
> **live** via `HugePageBackend::release_tick`, which drives the W11-1b `release_empty_excess`
> demand-reserve hook from the plan — the W11→W12 handoff — and the §36.9 G-sim slice
> (`release_outcome_is_identical_over_posix_and_the_sele4n_simulator`) proves it is backend-agnostic. The
> running counters reconcile into `topo-stats` JSON + the `topo.release.*` control namespace. **No new
> abstract transition:** the controller sequences mechanisms already certified by the §21.6
> `release_to_os_preserves_live_objects` (`lean/TopoMalloc/Theorems/Release.lean`), so there is no new
> Lean obligation. Tested by 28 `release` unit tests (incl. the §21.1 R2 oscillation property, the
> `recent_peak` decay and its tick-cadence independence, and the bounded-backlog guard), the
> `release_controller_plan_never_exceeds_supply` gating proptest, and the `tests/tests/release.rs` live
> integration + G-sim slices. **What W12 leaves to M5/M6:** driving the extent-path rungs (purge dirty →
> muzzy → release) and cold-sparse subrelease from a host pump over the live engine (the controller
> *plans* them today; the §20.4 mechanisms execute them), and the mutating decay control surface
> (`topo.dirty_decay_ms`, …) which is plan 07 W20.

> **▸ Decomposition — W12-2 (release controller).** Split *inputs* (W12-2a, a pure read of the observation
> vector), *the ladder* (W12-2b, the ordered policy), and *the demand reserve* (W12-2c, the anti-oscillation
> brake). The reserve is the subtle piece: aggressive release that immediately refaults is *worse* than
> keeping memory (SPEC §21.1/R2), so W12-2c is a first-class unit with its own oscillation test, not a knob
> bolted onto the ladder. The release-safety theorem `release_to_os_preserves_live_objects` (plan 02 W1-8c)
> certifies that every ladder step keeps live pointers committed.

---

## W13 — Topology awareness (CPU / LLC / NUMA)

**Depends on:** plan 05 W6 (transfer-cache domains). **Enables:** M3 (LLC), M4 (full).

| WU | Description | Size | ∥ | Acceptance | Status |
|---|---|---|---|---|---|
| W13-1 | Topology discovery (§15.2) from sysfs/CPUID/OS; **conservative single-domain fallback**. | M | | missing/inconsistent data ⇒ one domain, still correct. | ✅ `topology::Topology`/`TopologyBuilder` (single-domain fallback on any inconsistency); `topo-backend-posix::discover_topology` parses Linux sysfs (`node*/cpulist`, `physical_package_id`, `node*/distance`) with the same fallback. |
| W13-2 | Placement policy (§15.3): LLC-local alloc, NUMA-local backing, arena overrides. | M | ∥ | placement honors topology where present. | ✅ **Live.** The `NodeRouter` routes the live large path: explicit `Local` (real `sched_getcpu` CPU) / `Bind` / `Interleave` → `mbind`-bound per-node backends; **`OsDefault`/`ArenaPolicy` → an unbound default backend (genuine kernel first-touch, not pinned to node 0)**. A full node spills to the *nearest* other node (§15.4). The engine resolves the arena's `NumaPolicy` into `Hints::numa` (`ArenaTable::numa_of`, lock-free); single-node = one backend (unchanged). Wired into the `hugepage-optimized` ABI. (LLC-local alloc is the M2 transfer-cache layer's job; the LLC map is discovered.) |
| W13-3 | Cross-domain rebalancer (§15.4): preference order; no permanent stranding. | M | | stranded-memory test: rebalancer moves batches/spans under pressure. | ✅ **Live.** `Rebalancer::plan` — nearest-donor → most-pressured-node moves with the §15.4 tiers, moving only a donor's **movable surplus** (`free − own demand`) so a move **never strands the donor** and a no-surplus round plans nothing (no churn); ties prefer the larger surplus. `NodeRouter::rebalance_tick` executes it live (sample per-node free + approximate demand → plan → return the donor's idle empty hugepages to the OS via `release_empty_excess`). Driven to a fixpoint + executed in tests. |
| W13-4 | Hotplug/affinity/cgroup refresh (§15.2). | S | ∥ | snapshot refreshes on notification or periodic mismatch. | ✅ **Live.** `Topology::detect_mismatch` + `NodeRouter::refresh`/`refresh_if_mismatch` — the host-driven swap (no background thread, matching the release pump); a moved CPU rebuilds + swaps the snapshot, changing placement (tested over the real provider). |

> **▸ Implementation status (W13).** **Landed and live.** The §15.2 `Topology` snapshot
> (`crates/topo-core/src/topology.rs`, pure/`no_std`/bounded; CPU→LLC→NUMA maps + a node-distance matrix,
> all queries total) is built by a `TopologyBuilder` that collapses to `Topology::single_domain` on any
> inconsistency and **densely renumbers the OS node ids — and the LLC-domain ids — in use** (no phantom
> node or LLC domain on a sparse platform, for any builder caller; the raw OS node id is kept in
> `os_node_of` for `mbind`) — W13-1. `preferred_node`/`preferred_node_at` is the
> §15.3/§15.5 placement decision over `NumaPolicy` (W13-2); `Rebalancer::plan` is the §15.4 nearest-donor →
> most-pressured **surplus-only** move (never strands the donor, no churn) — W13-3; `detect_mismatch` is the
> §15.2 refresh probe (W13-4). `topo-backend-posix::discover_topology` is the real Linux sysfs read with the
> single-domain fallback; `PosixBackingProvider::bind_node` is the best-effort Linux `mbind(MPOL_PREFERRED)`
> (no-op elsewhere). The **`NodeRouter`** (`crates/topo-core/src/node_router.rs`) makes it all *live*: one
> `HugePageBackend` per node (a fixed `[…; MAX_NODES]` array — `no_std`), each bound to its node, routing the
> large path to the preferred node's backend (engine resolves the arena's policy into `Hints::numa`), with
> spillover on a full node, free-routes-home by address, live `rebalance_tick` execution (returns a donor's
> idle empty hugepages to the OS), and host-driven `refresh`. It is installed into the `hugepage-optimized`
> ABI via the existing `new_with_huge(&dyn RegionCacheHook)` seam — **the default extent path and a
> single-node host are byte-for-byte unchanged**. Placement / rebalancing are **policy, not modeled
> transitions** (§2.4), so there is no Lean obligation — a claim pinned (not asserted) by the fixed-wall
> `placement_never_breaks_the_allocation_contract` (size/alignment/validity/free-home invariant under
> every NUMA policy and a bind failure) and `placement_covers_every_numa_mode` (`preferred_node`
> totality), the W13 analogue of W14's `engine_size_align_validity_free_are_invariant_under_hints`.
> The router's §15.4/§15.5 counters (bind failures,
> rebalancer moves/bytes, spillovers) + the node/LLC counts reconcile into `topo-stats` JSON and the
> `topo.numa.*` control namespace. The optimal-completion pass added: **genuine first-touch** for
> `OsDefault` (an unbound default backend, so the common case is not pinned to node 0 on multi-node);
> real `sched_getcpu` `Local` (`OsCore`); nearest-node spillover; `Interleave` over the backend count;
> the W12 `release_idle` handoff; and the **host-driven `topomalloc_numa_*` C control surface**
> (`RouterControl`) so the rebalancer / refresh / idle-release are reachable on the deployed allocator —
> while the now-redundant per-candidate NUMA score term was removed. **What W13 still leaves to M2/M5:** a
> first-class per-node *demand* signal (the rebalancer uses an approximate alloc-failure-derived one
> today, M5) and LLC-domain placement (the transfer-cache layer, plan 05 W6 / M2 — the LLC map is
> discovered but not yet consumed for placement).

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · Extents: split, merge & coalesce (W4-2)

**Problem.** The backend owns virtual ranges and physical backing and must split a range to satisfy a request
and merge adjacent free ranges to fight fragmentation — while keeping the pagemap, hugepage accounting, and
pointer-classification all consistent and never exposing a stale descriptor.

**Design space.** **A boundary-tag + free-extent index (by size and by address)** — chosen: address-ordered
lookup makes neighbour-coalescing O(log n); size-ordered lookup makes best/first-fit O(log n). Split and merge
are kept as *separate* operations with separate preconditions because they have different stale-descriptor
hazards (merge is where most backend use-after-free hides).

**Structures.**
```rust
struct Extent { id: ExtentId, arena: ArenaId, base: usize, len: usize,
                committed_len: usize, state: ExtentState, huge: HugeRange, split_gen: u32 }
// indices: by_size: BTree<(len, base)>,  by_addr: BTree<base>  (neighbour lookup)
```

**Work breakdown (refines W4-2a..d).** 1. descriptor + dual index (W4-2a). 2. `alloc`+`split` (W4-2b):
results page-aligned; install both halves' metadata, then publish; pagemap via **plan 03 W3-6**. 3.
`merge`/coalesce (W4-2c): adjacency + arena/state compat + hugepage-accounting update; **retire the old
descriptors only after no classifier can reach them** (epoch/generation). 4. physical-state ops (W4-2d),
each mirrored to Lean (plan 02 W1-8a/c).

**Invariants.** §18.4 split/merge rules; M-004 (no live in a released range); M-005 (recommit before reuse);
disjointness preserved across split/merge (mirrors plan 02 W1-8a).

**Verify.** unit on split/merge edge cases (zero-length tail, alignment boundary); property: random
split/merge sequences keep ranges disjoint + the pagemap sound; failure-injection (W4-5) leaves state
well-formed.

**Failure modes.** *F1* publish-before-metadata on split → reader sees an uninitialized half → install then
publish. *F2* coalescing a range a classifier still points into → retire descriptors behind a generation/epoch
(`split_gen`). *F3* hugepage occupancy not updated on merge → H-002 violation → W11-2c checks it in debug.

**Sequencing.** **M1**.

### DD-2 · Hugepage filler: bins & scoring (W11-2) + partial subrelease (W11-4b)

**Problem.** Pack sub-hugepage spans densely so few hugepages hold the live set and empty hugepages release
easily (§19), without scanning every hugepage on each placement, and never releasing a subpage that
intersects a live object (H-005).

**Design space.** **Approximate bins keyed by free space + state, with a score computed only over the
candidate bin** — chosen (§19.3): exact "best hugepage" would scan all of them; bins give O(1)-ish placement.
Crucially, **bins are correctness, score is policy**: H-003 requires each hugepage in exactly one bin
consistent with occupancy; the score may be arbitrarily wrong without ever misplacing a live object (§2.4).

**Structures.** 9 bins (§19.4: empty_backed/nearly_empty/sparse/medium/nearly_full/full/partial_subreleased/
cold_sparse/hot_dense). `score = packing+locality+lifetime+hotness+release_preservation −
fragmentation−cross_numa−partial_subrelease`.

**Work breakdown.** 1. bin set + transition-on-occupancy-change (W11-2a). 2. candidate scan over the target
bin + score (W11-2b). 3. H-002/H-003 consistency checks (W11-2c). 4. **partial subrelease** (W11-4b) as its
own guarded unit: only if the subrange has *no live object*, is aligned to release granularity, the hugepage
is cold/sparse or pressure is high, and predicted RSS benefit > predicted fragmentation cost; record the
metric.

**Invariants.** H-001 a live range is in a committed, non-released subrange; H-002 occupancy bytes == Σ
contained spans/large; H-003 bin == occupancy/state; H-005 partial subrelease never intersects a live object.

**Verify.** unit: bin transitions on occupancy crossings; property: random span placement keeps Σ-occupancy ==
bin accounting; a dedicated H-005 test attempts a subrelease overlapping a live object and asserts refusal.

**Failure modes.** *F1* a hugepage in two bins → transition logic centralized + B.4 check. *F2* subrelease
races a new allocation into the subrange → take the placement lock + re-check liveness at commit. *F3* score
overfit to x86 hugepages → keep inputs backend-agnostic so W11-6 reuses them on normal-frame runs.

**Sequencing.** **M5**; W11-6 (seLe4n large-mapping over normal frames) lands with plan 09.

### DD-3 · Release controller & demand reserve (W12-2)

**Problem.** Decide *when* to return memory to the provider, balancing RSS, page faults, hugepage coverage,
latency, and cgroup pressure — and specifically **avoid the release→refault oscillation** where memory is
freed only to be faulted straight back (§21.1, R2 of the SPEC's tuning guidance).

**Design space.** **A priority ladder driven by a pressure mode, braked by a demand reserve** — chosen
(§21.3/§21.4). The reserve is the non-obvious, load-bearing piece: it is *not* a knob on the ladder but a
separate predictor that withholds release proportional to recent demand.

**Structures.**
```text
inputs = {live, rss, dirty, muzzy, coverage, alloc_rate, free_rate, refill_latency, cgroup_cur/max, pressure}
demand_reserve = f(recent_alloc_rate, recent_peak, refill_latency, pressure)
ladder (gated by mode): drain idle caches → release empty hugepages → purge dirty(not hot)
                         → dirty→muzzy → subrelease cold-sparse → emergency shrink
```

**Work breakdown.** 1. sample the input vector cheaply (W12-2a). 2. the ordered ladder, each step gated by the
pressure mode (W12-2b). 3. the demand reserve + the anti-oscillation brake (W12-2c). Plus W12-3b: **emergency
mode** bypasses optional caches and the HugeCache reserve, drawing only on a pre-reserved pool that never
depends on the normal heap (§36.5).

**Invariants.** every ladder step is certified by `release_to_os_preserves_live_objects` (plan 02 W1-8c): a
live pointer stays live + committed across release; the emergency reserve is independent of the normal heap.

**Verify.** the **oscillation test**: a workload that frees then immediately re-allocates must *not* thrash
the OS — the reserve holds enough back; pressure-mode transitions tested against simulated cgroup pressure;
emergency path tested with an injected allocation failure.

**Failure modes.** *F1* release-then-refault loop → the demand reserve (a first-class, tested unit). *F2*
emergency allocation depends on the heap it is trying to rescue → the reserve is pre-allocated at boot (plan
09 W22-5c on seLe4n). *F3* releasing a hot hugepage breaks TLP → the ladder purges dirty *not on hot
hugepages* first.

**Sequencing.** **M5**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M0 | W4-1 (trait + Posix/Sim stubs). |
| M1 | W4-2..W4-5 (extents, POSIX state mapping, large path). |
| M3 | W13-1..W13-4 (topology; LLC domains for transfer caches). |
| M5 | W11 (all), W12 (all) — hugepage filler, region cache, release ladder, pressure/emergency. |

## Domain risks

- **R2** (seLe4n co-equal) — the seam is owned here; W4-1 must keep POSIX as the degenerate case so plan 09's
  provider is a drop-in. **R8** (IPC cost) — latency classes (W12-4) make seLe4n slow paths visible.
- *Local:* `madvise` semantics differ across kernels; W4-3a documents the platform mapping and reconciles it
  in stats so RSS behavior is explainable (plan 07).

## Definition of Done (addendum)

Every backend op is fallible and leaves state well-formed (W4-5); every hugepage op preserves H-001..H-005
(B.4 in debug); every release step is certified by the Lean release-safety theorem or carries a `V-004` debt.

## Best-practices checklist

- [ ] The core never calls `mmap`/`retype` — only the provider seam.
- [ ] POSIX stays the degenerate single-authority case so seLe4n drops in (D2).
- [ ] Bins are correctness; scores are policy — a bad score never misplaces a live object.
- [ ] Partial subrelease never intersects a live object (H-005); its guards are isolated.
- [ ] The demand reserve prevents release→refault oscillation (a first-class unit, tested).
- [ ] Topology degrades to a single domain rather than failing.

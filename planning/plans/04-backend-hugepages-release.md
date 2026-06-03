# Plan 04 — Backend, Hugepages, Release & Topology

**Workstreams:** W4 (backend seam + POSIX), W11 (hugepage/large-mapping), W12 (release controller), W13
(topology) · **Status:** rev 2.0 · **Overview:** [README.md](README.md)
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

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W11-1a | HugeAllocator: reserve hugepage-aligned virtual ranges (§19.2). | M | | reservations hugepage-aligned. |
| W11-1b | HugeCache: cache empty *backed* hugepages for quick reuse; demand-reserve hook (W12). | M | ∥ | empty-backed reuse avoids immediate faults. |
| W11-2a | HugePageFiller bin set (§19.4: empty_backed/nearly_empty/sparse/medium/nearly_full/full/partial_subreleased/cold_sparse/hot_dense) as the structure; each hugepage in **exactly one** bin; bin transitions on occupancy change. | M | | H-003: bin membership consistent with occupancy. |
| W11-2b | Candidate selection/scoring (§19.3): approximate-bin scan; packing/locality/lifetime/hotness/release-preservation bonuses minus fragmentation/cross-numa/partial-subrelease penalties. | M | | no full scan of all hugepages; deterministic in test mode. |
| W11-2c | Bin↔occupancy consistency invariant + tests (H-002/H-003): occupancy bytes == sum of contained spans/large allocs. | M | ∥ | invariants checked in debug (B.4). |
| W11-3 | RegionCache for awkward sizes (§18.6): allocations slightly larger than a hugepage avoid rounding to multiple full hugepages. | M | ∥ | waste on awkward sizes bounded; tested. |
| W11-4a | Packing policy (§19.5): pack same-lifetime/hot-dense; prefer partially-used hugepages; keep some empty-backed in HugeCache. | M | | policy observable in stats; never misplaces a live object. |
| W11-4b | Partial-subrelease guards (§19.6/H-005): subrange has no live object, aligned to release granularity, gated on coldness/pressure, recorded as a metric. | M | ∥ | partial subrelease only when all guards pass; metric emitted. |
| W11-5 | Coverage metrics (§19.7) exported to stats (plan 07): coverage_bytes, intact/partial live bytes, empty_backed/released, partial_subreleased, fragmentation, coverage_ratio. | S | ∥ | all §19.7 fields present; ratio computed. |
| W11-6 | seLe4n large-mapping policy (§36.9): same placement over contiguous normal-frame runs; prefer whole-mapping release. | M | | correct when every backing range is normal pages; Sim test (plan 09). |

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
| W12-4 | Latency classes (§36.11) annotated on slow paths; arena flags `no_ipc_fast_only`/`bounded_slow_path`/`may_block`. | S | ∥ | each slow path tagged; real-time arenas can forbid blocking. |

> **▸ Decomposition — W12-2 (release controller).** Split *inputs* (W12-2a, a pure read of the observation
> vector), *the ladder* (W12-2b, the ordered policy), and *the demand reserve* (W12-2c, the anti-oscillation
> brake). The reserve is the subtle piece: aggressive release that immediately refaults is *worse* than
> keeping memory (SPEC §21.1/R2), so W12-2c is a first-class unit with its own oscillation test, not a knob
> bolted onto the ladder. The release-safety theorem `release_to_os_preserves_live_objects` (plan 02 W1-8c)
> certifies that every ladder step keeps live pointers committed.

---

## W13 — Topology awareness (CPU / LLC / NUMA)

**Depends on:** plan 05 W6 (transfer-cache domains). **Enables:** M3 (LLC), M4 (full).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W13-1 | Topology discovery (§15.2) from sysfs/CPUID/OS; **conservative single-domain fallback**. | M | | missing/inconsistent data ⇒ one domain, still correct. |
| W13-2 | Placement policy (§15.3): LLC-local alloc, NUMA-local backing, arena overrides. | M | ∥ | placement honors topology where present. |
| W13-3 | Cross-domain rebalancer (§15.4): preference order; no permanent stranding. | M | | stranded-memory test: rebalancer moves batches/spans under pressure. |
| W13-4 | Hotplug/affinity/cgroup refresh (§15.2). | S | ∥ | snapshot refreshes on notification or periodic mismatch. |

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

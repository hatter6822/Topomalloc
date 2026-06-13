<!-- SPDX-License-Identifier: MIT -->
# Architecture

The allocator is a stack of layers over a single backing seam. The full diagram
and rationale are in the plan overview (§3). Since plan 06 W8 the public API
runs over the **M1 central-path allocator** (`topo_core::Allocator`): classify
→ central free lists (small) / extent-backed large path, composed from the
plan-03/04 parts below; the M0 skeleton survives only as a test fixture.

```text
Public API:  C ABI (topomalloc_*/topo_*x) + C++ header      topo-abi, include/
             + Rust GlobalAlloc
M1 engine:   classify → central lists / large path           topo-core (allocator)
Request classifier: small/medium/large, size class,         topo-core (classify)
                    align, arena, label, hints
Metadata substrate: bootstrap bump alloc, pagemap,          topo-core (bootstrap,
  span/large descriptors, pointer classification              pagemap, span, ptr_class)
Back-end: extents (split/merge/coalesce), physical          topo-core (extent,
  state (dirty/muzzy/released), large path + classify         large), plan 04 W4
Front/middle ends (M2+)                                      topo-core, plan 05
        ─────────────────  S E A M  ─────────────────
trait TopoBackingProvider (+ §36.6 provider state machine)   topo-core (backend)
  ├─ PosixBackingProvider  (ambient authority)               topo-backend-posix
  └─ Sele4nSim / Sele4nBackingProvider (capabilities)        topo-backend-sele4n
Formal: Lean model + seLe4n bridge                           lean/
```

## Crates

| Crate | Role | License |
|-------|------|---------|
| `topo-core` | classifier, size classes, the seam, the M1 central-path allocator (`no_std`) | MIT |
| `topo-abi` | the full prefixed C API (core/aligned/C23/extended + flags), errno + zero-size policy, `GlobalAlloc`, runtime backend selector | MIT |
| `topo-backend-posix` | `PosixBackingProvider` (degenerate single-authority case) | MIT |
| `topo-backend-sele4n` | `Sele4nSim` + (M1) `Sele4nBackingProvider` | GPL-3.0-or-later |
| `topo-arch` | per-arch identity + the RSEQ restartable per-CPU sequences (plan 05 W7) | MIT |
| `topo-stats` | stats snapshot + additive JSON (`topomalloc_version`) | MIT |
| `topo-control` | configuration + control namespace | MIT |
| `topo-test-support` | trace parser, deterministic PRNG, executable model | MIT |

## Request classification (plan 03 W2)

`classify(size, align, flags)` (§A.1) is the first step on every allocation path.
It is total and overflow-safe (§9.7): it returns a `Request` or `None` (→ null /
`bad_alloc`), never panicking or wrapping. The decision is three-way (§9.2):

- **Small** — a size-class slab (`size_class`, a branch-light direct-mapped
  granule lookup). Over-aligned requests route to a sufficiently-aligned class or
  out to medium/large — never widening a shared slab's stride (§9.3 / §25.5).
- **Medium** — a page-rounded extent above the small path.
- **Large** — at/above the hugepage threshold (`HUGE_THRESHOLD`), hugepage-backed
  (plan 04). The medium/large split is decided on `max(size, align)`.

Advisory flags (§10.4) are decoded and **validated** by `RequestFlags` into a
structured `Hints` (zero, cache-bypass, guard, hugepage preference, lifetime,
hotness) plus arena routing; reserved bits and contradictory combinations fail
deterministically. The public C `TOPO_*` values map onto this internal layout at
the plan-06 API boundary. The Lean model mirrors the classifier and proves both
the size-coverage invariant (plan 02 W1-4) and the over-alignment-sufficiency
invariant (plan 03 W2-3b: the lookup only ever returns a class whose natural
alignment covers the request).

## Metadata, pagemap & pointer classification (plan 03 W3)

Below the classifier sits the metadata substrate every later layer reads (§16/§17):
turn a request into a span, find the descriptor that owns *any* address, and do it
while spans are concurrently created and recycled. Four modules in `topo-core`:

- **Bootstrap metadata allocator** (`bootstrap.rs`, W3-1, §17.4/S-007). A monotonic
  bump core (`BumpArena`) over a region, wrapped by a lifecycle (`Bootstrap`): no
  dependency on the public `malloc` (it only bumps an atomic cursor), lock-free
  before threading, safe-failure on exhaustion (`None`, never a wrap), idempotent
  init, and a **real hand-off** — `hand_off_to(successor)` routes new metadata to the
  normal allocator once it exists, while bytes already vended stay valid. The
  process-wide `Bootstrap::global()` binds a `BOOTSTRAP_REGION_BYTES` static reserve
  lazily (the DD-2 "static reservation"). A **re-entrancy guard** *refuses* any
  same-thread re-entry of the metadata path — `alloc` returns `None` rather than
  recursing, in **every** profile, and additionally debug-aborts under
  `debug-assertions` (`is_in_alloc()` exposes the state) — the concrete form of
  S-007's "must never re-enter the public allocator." Span descriptors, their
  out-of-line bitmaps, and the pagemap's radix nodes all come from this seam
  (`MetadataAlloc`), so the source can change at hand-off without callers caring.

- **Span & large descriptors** (`span.rs`, W3-2/W3-5, §16.2/§17.2/§8.5). The
  `SpanDescriptor` carries the §16.2 fields and *derives the §16.4 conservation law*
  `object_count = live + local_cached + transfer_cached + central_free + quarantined`
  with `central_free = popcount(free_bitmap)` as the authoritative central residency;
  the cached terms are logical, reconstructed in debug (W5-3c) and zero before caches
  exist (M1), so empty-span detection (§16.5) never reads a cached free object as
  live. The bitmap and its cached count update in **one critical section** (§8.5): a
  per-span lock (§27.2's span lock) gates them through a `SpanGuard`, so the
  `central_free == popcount` invariant can never be observed torn. The bitmap is
  **hybrid** — inline for small slabs, out-of-line and class-sized for the few
  high-count classes — keeping the descriptor compact (96 bytes) whatever the class.
  A `generation` (W3-5, §16.6/§27.5) is bumped on recycle so a captured reference is
  detectably stale (`GenGuard`), a **seqlock** version makes a classifier's geometry
  read consistent against a racing recycle, and an integrity tag (§17.3) lets
  debug/hardened detect a corrupted read-mostly header.

- **Pagemap** (`pagemap.rs`, W3-3/W3-6, §17.1/DD-1). A fixed-fan-out **multi-level
  radix over allocator-page numbers**, chosen over a flat array (wastes virtual space
  on 64-bit) and a hash map (worst-case/resize hazards): O(1) worst-case, lazily
  populated from the metadata seam, no resize, interior-pointer lookup by masking an
  address to its page. The depth is **derived** from the page size and `usize::BITS`,
  so the radix covers the **entire address space** with no VA-width assumption — an
  address from 5-level paging / a 57-bit VA maps as readily as a 48-bit one. Nodes
  are a uniform 8 KiB (1024 slots), small for lazy population and `low-rss`; the cost
  is a few extra dependent loads on the *slow* path (5 levels on a 64-bit/16 KiB-page
  target), never on the fast path. Leaf slots are **tagged pointers** — `Empty` (the
  zero word, non-owned, P-Map-002), `Small`/`Large` (a descriptor pointer), or
  `ReleasedRetained` (a span kept so a released page cannot be reused without
  recommit, P-Map-005); descriptors are ≥ 8-aligned so the low three bits carry the
  tag. The **publish/read protocol** (W3-3c/P-Map-006): a leaf slot is filled with a
  *release store* only after its descriptor is initialized, readers *acquire-load*,
  and new nodes are zeroed before being linked with a release CAS — so a concurrent
  classifier sees `Empty` or a fully-formed entry, never a half-built node. Because
  descriptors live in monotonic metadata and are recycled with a generation bump, a
  stale pointer is always dereferenceable and the generation flags the reuse (the
  §27.5 use-after-free the SPEC warns of). An install is **two-phase, so it is atomic
  on metadata exhaustion** — every radix node is created before any entry is published,
  so a failed install leaves no page mapped. **This module is the single mutator**
  (W3-6): split/merge (plan 04 W4-2b) and span lifecycle (W5-5) route every change
  through `install_span`/`release_span`/`retire_span`/`install_large`, never poking a
  leaf; `metadata_bytes()` reports the bounded node overhead.

- **Pointer classification** (`ptr_class.rs`, W3-4, §17.5). `classify_ptr` consults
  the pagemap and the metadata ranges and returns the §17.5 class — `Null`, `Small`
  (with the object index, by the §16.3 slab-layout inverse, read through the seqlock
  so a recycle race never yields a torn result), `Large`, `Interior`, `Metadata`,
  `Released`, `Quarantined`, or `External`. It is **total and corruption-resistant**:
  a recycle that re-bases a span *above* the queried address yields `External` rather
  than an `addr − base` underflow; the size-class lookup is bounds-checked, so a
  corrupted/out-of-range `sc` yields `External` rather than an out-of-bounds panic;
  and a hardened build (`debug-checks`) validates the §17.3 integrity tag on the
  classification path, so a wild write to a descriptor's read-mostly header makes the
  pointer classify foreign rather than be trusted. A tag mismatch is *disambiguated*
  by re-reading the seqlock version — it is genuine corruption only when the version
  is unchanged; a mismatch from a recycle that merely raced the check is retried, not
  misreported — so a benign concurrent recycle is never spuriously classified foreign.
  Classification never panics or wraps on any input, valid metadata or not. Metadata is recognized across **all**
  sources via `AnyMetadataRegion` (the bootstrap region plus the post-hand-off
  successor). `free` requires a **base pointer** (§17.5); `validate_free` enforces
  exactly that, mapping a base pointer or `Null` to a `FreeTarget` and an
  interior/foreign/released/metadata/quarantined pointer to an `InvalidFree` that
  debug/hardened builds *detect and report* — never act on (W3-4b, ties plan 08
  W18-2). A fuzz target (`fuzz/fuzz_targets/ptr_class.rs`) hardens the total guarantee
  over adversarial addresses and pagemap layouts; the `large` path is seqlock-read,
  symmetric with the span path.

The runtime pagemap is held to the **same soundness property the Lean model proves**.
Beyond the property test (`tests/tests/pagemap.rs`) that discharges
`pagemap_lookup_sound` over random layouts, an **executable Lean pagemap model**
(`lean/TopoMalloc/Theorems/PagemapExec.lean`) proves `install_lookup_sound`
(kernel-checked) and replays a recorded install/lookup trace that `lake exe check`
evaluates; the Rust `pagemap_matches_lean_replay_differential` test replays the
*identical* trace against the radix and asserts the same `addr → span` results — so a
divergence on either side fails CI (the W3-3d differential, the pagemap analogue of
the live-set oracle's trace-replay loop). The radix's index decomposition is tested
lossless (distinct pages never collide), and the subtle lock-free protocols — the
W3-4 seqlock (with its hardened integrity-vs-race disambiguation), the W3-3c
publish/read, the lazy-node CAS race, and the W4 large-free critical section (the
lookup-under-the-pool-lock that makes a concurrent double-free settle on exactly one
winner) — are **model-checked by `loom`**
(`tests/loom_protocols.rs`, `cargo xtask test --kind loom`, gated to `--cfg loom` so
its deps stay out of the normal build), an exhaustive-interleaving complement to the
std-thread stress tests. Criterion benchmarks (`benches/metadata.rs`,
`cargo xtask bench`) measure the lookup/classify/install latency and report the
node-byte overhead, so the W3-3a "bounded + documented" claim is measured, not
asserted.

## Back-end: extents & physical state (plan 04 W4)

Below the metadata substrate sits the **back-end** (§18): it owns virtual address
ranges and physical backing, hands spans/large ranges up to the allocator, and
returns memory to the OS — everything OS/kernel-facing goes through the
[`TopoBackingProvider`] seam, never a direct `mmap`/`retype` (overview §3). POSIX
is the **degenerate single ambient-authority case**; the seLe4n simulator and (at
M1) the capability provider drop in behind the *identical* seam (D2).

- **The seam & the provider state machine** (`backend.rs`, W4-1, §36.6). The trait
  is offered in **two layers**: the *collapsed* ops the allocator core drives
  (`reserve`/`commit`/`decommit`/`purge_lazy`/`purge_forced`/`release` and
  `revoke_descendants`, a no-op on POSIX), and the *full §36.6 typed surface*
  (`reserve_window`/`create_frame`/`map_frame`/`unmap_frame`/`recycle` over
  `VWindow`/`FrameCap`/`MappedRange`). On POSIX the capability types collapse to an
  address range and the granular methods **default-compose** the collapsed ops, so
  one `reserve` (an `mmap`) realises `reserve_window ∘ create_frame ∘ map_frame`;
  plan 09 overrides the granular methods with real capabilities. A `ProviderState`
  enum models the §36.6 lifecycle as the **exact linear chain** `AuthorizedUntyped →
  … → RecyclableUntyped` — mirroring the Lean `BackingState.next` (W1-11b)
  one-for-one (allocator *reuse* is the extent manager's `ExtentState`, not a
  back-edge of this lifecycle). `can_transition` enforces the §36.6 ordering
  (**unmap before revoke before recycle**), so recycled untyped never retains a live
  client mapping. The Rust↔Lean agreement is pinned both ways — a Rust test
  (`provider_next_matches_the_36_6_chain_exactly`) and a `lake exe check` gate
  (`providerChainGate`) — so the runtime checker and the proof cannot drift. The
  seLe4n simulator walks every reservation/release through the machine, asserting
  each step on the backend where capability semantics are real.

- **The extent manager** (`extent.rs`, W4-2, DD-1). A managed region is tiled by
  `Extent` descriptors in a fixed-capacity, metadata-backed slot pool (no global
  allocator — re-entrancy-free, like every allocator-internal structure). Two
  indices make the §18.4 operations cheap and chosen per DD-1's *boundary-tag +
  free-extent index*: an **address-ordered** intrusive list over *all* extents
  (free and allocated) gives O(1) neighbour-coalescing, and a **size-segregated**
  free-list (binned by `floor(log2(pages))`) gives best/first-fit as a bounded bin
  scan (the bins are size-segregated, so the smallest fit is in the lowest non-empty
  adequate bin — best-fit is exact). `split` and `merge` are deliberately separate:
  a split installs both halves' metadata *before* publishing them (failure mode
  F1), while a merge retires the absorbed descriptor *behind a generation* so no
  reader resolves a recycled slot to the wrong range (F2 — "coalescing is where most
  backend use-after-free hides"). An `ExtentRef` pairs a slot id with the generation
  it was minted at, so a reference captured before a merge resolves to `None` once
  the slot recycles. The slot pool is index-based (not raw intrusive pointers), so
  the whole bookkeeping core (`ExtentMap`) is safe, directly unit-tested Rust; the
  `ExtentManager` wraps it behind the §27.2 **backend lock** (the lowest
  data-structure lock) and drives the provider.

- **Physical states & the POSIX mapping** (W4-2d / W4-3a, §20). Each extent tracks
  a `committed_len ∈ {0, len}` and a state — `Reserved`/`Active`/`Dirty`/`Muzzy`/
  `Released` — coupled so a free extent's state always matches its backing (an
  `Active` extent may be transiently uncommitted while the manager commits it;
  M-005 guarantees backing before *use*). The physical-state ops enforce **M-004**:
  decommit/purge/release require a *free* extent (a free extent holds no live
  object — the runtime evidence M-004 demands), and an attempt on an `Active`
  extent is refused (`NotFree`). On **unix the POSIX provider issues the real
  syscalls** for the per-platform `madvise`/`mprotect` mapping — Linux
  `MADV_DONTNEED` ↔ decommit/forced-purge, `MADV_FREE` ↔ lazy-purge,
  `mmap`/`munmap` ↔ reserve/release, `mprotect` ↔ commit (guard mode) — so
  `release`/`decommit` genuinely return memory to the OS (a `cfg(not(unix))`
  host-allocator fallback keeps the same observable behaviour everywhere). The
  §20.1 byte breakdown (`state_bytes()` → `dirty`/`muzzy`/`released`) reconciles into
  the §21.2 stat fields via `topo_stats::Stats::record_backend`. The
  **retain-vs-unmap** policy (§20.5, W4-3b) is profile-keyed (retain on 64-bit perf,
  unmap under `debug`/`low-rss`/32-bit); a **guard-mode** provider
  (`with_guard_pages`) `mprotect`s released ranges `PROT_NONE` so a use-after-free
  **faults** (a fork-based test asserts the trap).

- **Large path & classification** (`large.rs`, W4-4, §18.5/§18.6). `LargeAllocator`
  composes the extent manager, a `PageMap`, and a **recycling `LargeDescriptor`
  pool** (no per-allocation metadata leak). `allocate` page-rounds overflow-safely,
  **bypasses the small-object path** (never touches size classes, §18.5), takes a
  best-fit extent, and installs a `LargeDescriptor` through the W3-6 mutator
  (`install_large`) — so the result is **classifiable**: `free`/`usable_size`
  recover it by pagemap lookup, retire the entry *before* the descriptor slot can
  recycle — under the pool lock, so there is no stale-address hazard and two threads
  racing to free one pointer settle on exactly one winner (never a double free) — and
  return the extent. A `RegionCacheHook` (the
  §18.6 awkward-size hook) gets first refusal; a cache-served region is freed back to
  the cache, defining the lifecycle W11-3 fills (M5).

Every back-end op is **fallible and leaves the state well-formed on failure**
(W4-5, §36.6): the fallible provider step is sequenced so a failure rolls the
bookkeeping back rather than stranding a half-committed extent.
`ExtentMap::check_invariants` is the executable form of the §18 tiling/index
well-formedness predicate — the address list tiles the region exactly, every free
extent is in its correct bin, no `Active` extent is binned, the slot accounting
balances — and is `debug_assert`ed after every mutation and exercised by the
property, **concurrency** (a multi-threaded stress over the §27.2-locked manager),
failure-injection, and **fuzz** (`fuzz/fuzz_targets/extent.rs`) tests. The **Lean**
theorems certify the geometric core: `span_split`/`span_merge_preserves_disjointness`
(`Theorems/Span.lean`, W1-8a — the Rust `split` is Lean's `splitLeft`/`splitRight`,
`merge` their union), `release_to_os_preserves_live_objects` (`Theorems/Release.lean`,
W1-8c — decommit/M-004), and the `recommit_*` theorems (`Theorems/Extent.lean`,
W4-2d — commit/M-005); so the implementation discharges the obligations they state.
The §20.1 physical-state machine *itself* is pinned to the Lean `ExtentState.canTransition`
model by a Rust differential test (`extent_state_transition_matches_lean`) and the
`lake exe check` `extentStateGate` — the §20.1 analogue of `providerChainGate` — and
`can_transition` is `debug_assert`ed at every physical-state write, so the extent-state
transitions the allocator actually runs cannot drift from the model.

- **Extent hooks & custom backing** (`hooks.rs`, W10, §23). Because everything
  OS/kernel-facing already flows through the seam, a **custom backing** is just a
  provider: `ExtentHooks` is the §23.2 interface (alloc/dealloc/commit/decommit/
  purge/split/merge) and `HookProvider<H>` adapts it to `TopoBackingProvider`, so an
  `ExtentManager`/`Allocator` built over it runs the whole central path on the user
  memory source **unchanged above the seam**. The six physical ops gate (a hook
  failure is a `BackendError` the manager recovers from, W4-5); `split`/`merge` are
  **advisory** seam notifications (the `ExtentMap` owns sub-extent geometry, so a
  failure is recorded — `HookProvider::split_hook_failures` — never corrupting the
  bookkeeping, §23.3/§23.4), dispatched from carve/coalesce through the `ExtentNotify`
  sink (default `NoNotify` ⇒ POSIX/seLe4n unchanged). `HookProvider` **enforces** the
  load-bearing §23.3 output contracts (alignment, size, sub-range) — rejecting *and*
  debug-aborting a violation (§2.4) rather than trusting it. The §23.4 "allocator
  correctness assumes hook correctness" assumption is modeled in
  `lean/TopoMalloc/ExtentHooks.lean`: given the §23.3 contracts, alloc/split/merge/
  subrange preserve range disjointness and the region tiling. A proptest and the
  `fuzz/fuzz_targets/extent_hooks.rs` target assert the back-end stays well-formed
  under arbitrary hook failures (W10-3).

## Front-end: per-CPU caches & the RSEQ fast path (plan 05 W6/W7)

The front-end holds free objects in per-`(cpu, size-class)` slots and serves the
common small `malloc`/`free` without touching the middle-end. The **locked**
per-CPU cache (`cpu_cache.rs`, W6-4) is the correct baseline: a per-CPU spinlock
serialises each slot, and refill/flush move batches up/down the hierarchy
hand-over-hand (`cache_ops.rs`, never two middle-end locks at once). Everything
obeys one **front-end contract** — `FeOutcome::{Success, Empty, Full, Abort}` —
where `Abort` (a preempted/migrated fast path → retry, state unchanged) is kept
distinct from `Empty`/`Full` (genuine under/overflow → slow path).

**RSEQ fast path (W7, `topo-arch/src/rseq/`).** On Linux the per-CPU `pop`/`push`
become lock-free via a restartable sequence the kernel **restarts** on
preemption/migration before its single committing store — the only hand-written
assembly in the project, per-arch for x86-64 (W7-2) and AArch64 (W7-3, co-primary
as the seLe4n target). The shape is non-negotiable (§12.3): load `cpu_id` *inside*
the critical section, address `&cpus[cpu]`'s slot, bounds-check, then commit with
**one store**; an abort before it is a logical no-op (the plan 02 W1-7 frame
condition). There are no calls and no possibly-faulting references in the section
(an `xtask` lint enforces the no-call rule, W7-2d). `CpuCache` is **mode-aware**:
`enable_rseq()` fronts `fe_pop`/`fe_push` with the sequence and falls through to
the locked path on anything it cannot handle, so the two are behaviourally
identical — the acceptance criterion (G-fast), proven by a pinned outcome-equality
comparison and a forced-migration token-conservation differential against the
locked baseline. RSEQ availability is detected at run time (glibc's registered
area where present, a `std` self-registration fallback otherwise); where it is
absent the allocator uses the locked baseline unchanged (P-003). A **non-owner**
draining an idle CPU coordinates with the owner's sequence by taking the per-CPU
lock (new sequences then divert) and issuing `membarrier(…_RSEQ)` to abort any
in-flight one (W7-4, §27.4).

**seLe4n pinned-core (W7-5, `pinned.rs`).** The non-Linux target has no `rseq`, so
its per-core fast path (§36.10 option 1) is a software restartable sequence behind
the *same* contract: it reads the current core from the runtime, **aborts with no
state change** if the thread is not on its pinned core, and commits a single store
only when the core is stable across the read — mirroring the Lean
`per_core_cache_abort_no_change` obligation (W1-12d).

## Single source of truth (DD-1)

`tools/size-class-gen` reads the committed golden
`tools/size-class-gen/size-classes.json` and emits the Rust table, the C header,
and the Lean table — including the size-regime constants `HUGE_THRESHOLD` (the
medium/large boundary, authored) and `MAX_ALIGN` (the widest class alignment,
*derived* so over-alignment routing cannot drift) — all checked byte-for-byte in
CI (G-table). No table value is ever hand-edited. On the Lean side, `lake exe
check` replays the *generated* table through the well-formedness predicate
(`tableOk`) and the decidable lookup-coverage and §9.4-spacing gates
(`coversAllB`, `spacingOkB`); the §9.5/§33.4 coverage and §9.4 spacing theorems
are proved generally and discharged on the emitted table through those
predicates, so the shipped artifact provably cannot drift from a sound table.

## Formal model (`lean/`, plan 02 W1)

Lean 4 carries the allocator's abstract state machine (§33) and the seLe4n bridge
(§36). It is not on the hot path; it makes conformance real. The single-core
theorem set is complete:

- **State & invariants** — an ownership-map `State`, the 12-clause `WellFormed`
  predicate (§33.3), and the transitions (malloc/free/cache/central/release/arena)
  as total functions whose frame conditions are definitional.
- **§33.4 theorems** — every required theorem, proved and named exactly per the
  SPEC: well-formedness preservation, ownership conservation (consuming the §33.5
  RSEQ frame contract), span split/merge disjointness, pagemap soundness, release
  safety, the arena lifecycle, and size-class coverage.
- **§9.4/§9.5 size classes** — the spacing-dominated ratio bound, the
  alignment-dominated waste caveat, the slab-layout lemmas, and lookup coverage.
- **§23.4 extent-hook assumption (W10)** — `ExtentHooks.lean` states the §23.3 hook
  contracts as hypotheses and proves the operations preserve the well-formedness
  core *under* them (a contract-honouring `alloc` keeps ranges disjoint;
  `split`/`merge` keep the region tiled; a sub-range op touches no other extent) —
  the precise "allocator correctness assumes hook correctness" of §23.4, with the
  cheap half of the premise enforced at runtime by `HookProvider`.
- **seLe4n bridge (§36, GPL-3.0-or-later)** — the abstraction relation and
  `TopoSeLe4nWellFormed`, the §36.6 backing-provider state machine, the §36.17
  families (authority/quota, provenance/release, destroy/label/scrub,
  per-core/stats/non-interference), and a *coupled* TopoMalloc↔seLe4n step whose
  whole invariant bundle is preserved together. The SMP/multicore forms are
  **proved** by interleaving semantics over all schedules (not staged).
- **Executable model (§33.7)** — a proof-grade trace-replay oracle that checks
  well-formedness at trace boundaries and flags injected violations; `lake exe
  check` runs it as a CI gate alongside the G-table check.

There are no `sorry`s; the only trusted axioms are the four §33.5 RSEQ primitives.
See [`lean/README.md`](../../lean/README.md) for the module map.

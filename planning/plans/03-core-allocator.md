# Plan 03 — Core Allocator

**Workstreams:** W2 (size classes/classify), W3 (metadata/pagemap/bootstrap), W5 (spans/slabs/central) ·
**Status:** rev 2.0 · **Overview:** [README.md](README.md)
**SPEC anchors:** §9, §16, §17, §14, §25.5, §A.1/§A.4, P-Map-001..006, C-001..C-005, S-007, §27.5.
**Upstream deps:** [02](02-formal-model.md) (size-class table, theorems), [04](04-backend-hugepages-release.md)
(the `TopoBackingProvider` seam + extents). **Downstream:** [05](05-caches-concurrency-fastpath.md) (refills
from the central list), [06](06-api-realloc-arenas.md) (public path), [09](09-sele4n-integration.md).
**Milestones:** the heart of **M1**; the conservation law completes at **M2**.

> This is the allocator spine: turn a request into a size class, find the descriptor that owns any pointer,
> and carve/return objects from spans via the central free list. It contains **the hardest accounting problem
> in the allocator** — empty-span detection across caches (§16.5) — which is decomposed in detail below.

## Interfaces this plan owns

| Seam | Signature (abstract) | Consumed by |
|---|---|---|
| Classification | `classify(size,align,flags) -> Request{arena, Small(sc)/Medium(pages)/Large, label}` | 05, 06, 08 |
| Size→class | `size_class(size,align) -> sc`, `usable_size(sc) -> bytes` | 05, 06 |
| Pagemap | `classify_ptr(addr) -> Small{span,sc,arena,idx}/Large{desc}/Interior/Metadata/Released/External` | 05, 06, 08 |
| Central list | `central_remove_batch(node,arena,label,sc) -> Batch`; `central_insert_batch(batch)`; `span_is_empty(span) -> bool` | 05 |

---

## W2 — Size classes, alignment & request classification

**Goal:** the runtime classifier + generated tables, provably consistent with [plan 02 W1-4](02-formal-model.md).
**Depends on:** W1-4. **Enables:** W5, plan 05, plan 06, plan 08.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W2-1 | `tools/size-class-gen` emits Rust tables + `include/` constants from the **same** model as W1-4; build fails if output diverges from the Lean-verified golden. | M | | regenerate-and-diff is a CI gate (G-table); no hand-edited tables. |
| W2-2a | `size_class(size,align)` fast lookup (branch-light; small sizes via a direct-mapped index table, larger via the class array). | M | | matches the Lean lookup on an exhaustive small-range test. |
| W2-2b | `usable_size(sc)` + the reverse `sc → (size, align, objects_per_slab, batch)` accessors. | S | ∥ | accessors agree with the generated table. |
| W2-3a | `classify(size,align,flags)` (§A.1): small/medium/large split + arena + label + hints. | M | | unit + property tests; deterministic. |
| W2-3b | Over-aligned routing (§25.5/§9.3): over-aligned small requests route to aligned size classes or medium/large — **never** offset-adjusted inside a shared slab. | M | ∥ | over-aligned request never shares a slab with a normally-aligned one. |
| W2-4 | Overflow-safe rounding (§9.7): size-class, page, hugepage, **and** alignment rounding all checked; integrates with calloc (§26.1). | S | | overflow tests return null/`bad_alloc`, never wrap (with plan 06 W8/W15). |

> **▸ Decomposition — request classification (W2-3).** The classifier is the first thing on every hot path,
> so it is split into the *decision* (W2-3a: which bucket, branch-light) and the *over-alignment escape*
> (W2-3b), which is a correctness rule, not a performance one: silently widening a shared slab's stride to
> satisfy one over-aligned request would break the disjointness/alignment invariants for its neighbours.
> Routing over-aligned requests out of shared slabs keeps W5-1's layout lemmas (plan 02 W1-4d) intact.

---

## W3 — Metadata, pagemap, bootstrap allocator & pointer classification

**Goal:** the metadata substrate every layer reads. **Depends on:** plan 01; W2 for `sc` (from W3-2 onward —
**W3-1 needs only plan 01 and starts in M0**, off the W1-4 critical path). **Enables:** plan 04, W5, plan 06.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W3-1a | **Bootstrap metadata allocator** core (§17.4, S-007): a monotonic bump arena over a static/early reservation; no dependency on public `malloc`; lock-free before threading. | M | | allocates metadata before any arena exists; never calls global `malloc`. |
| W3-1b | Idempotent init + safe-failure + the hand-off to the normal metadata allocator once available (§17.4). | S | ∥ | double init is a no-op; OOM in bootstrap fails safely. |
| W3-2 | Span descriptor (§16.2): `object_count/live_count/central_free_count`, `free_bitmap`, optional `cache_bitmap`, `generation`, `flags`. | M | ∥ | fields derive the §16.4 conservation law; struct size asserted. |
| W3-3a | Pagemap data structure: multi-level radix keyed by allocator-page; O(1) lookup; full address-range coverage; level count chosen for the page size (D4). | L | | lookup O(1); metadata overhead bounded + documented. |
| W3-3b | Entry encoding: Small (arena/span/sc), Large (descriptor), Released-retained, External sentinel (P-Map-002/004/005). | M | ∥ | every allocator page maps to exactly one descriptor; non-owned → sentinel. |
| W3-3c | Concurrent install/update synchronized to span lifecycle (P-Map-006): release-store publish / acquire-load read; generation-guarded (with W3-5). | L | | no unsynchronized update (Appendix F); ABA-safe. |
| W3-3d | Lookup-soundness tests (P-Map-001..006) + differential check vs Lean `pagemap_lookup_sound` (W1-8b). | M | | passes; divergence fails CI. |
| W3-4a | Pointer classification (§17.5) returning Null/Small/Large/Interior/Metadata/Released/Quarantined/External. | M | | matches the pagemap; base-pointer-only frees enforced. |
| W3-4b | Interior- and foreign-pointer detection in debug/hardened (the invalid-free path, ties plan 08 W18-2). | M | ∥ | interior/foreign frees detected, reported, not acted on. |
| W3-5 | Generation counters + stale-descriptor protection (§27.5, §16.6). | S | ∥ | a recycled span bumps its generation; debug catches stale refs. |
| W3-6 | **Pagemap↔span synchronization protocol** (P-Map-006), the single path W4-2b (split/merge) and W5-5 (span lifecycle) both call through. | M | | no unsynchronized pagemap update; lock-order respected. |

> **▸ Decomposition — W3-3 (pagemap).** The radix shape is the key design call: a 2- or 3-level radix over
> allocator-page numbers gives O(1) lookup with bounded, lazily-populated overhead, where a flat array would
> waste virtual space on 64-bit. The subtle correctness pieces, each its own sub-WU: (P-Map-005, W3-3b)
> released-but-retained pages keep enough metadata to forbid reuse without recommit; (P-Map-006/§27.5, W3-3c)
> entries publish with release/acquire ordering plus a generation so a concurrent `free`-classification can
> never follow a stale pointer to a recycled descriptor. **Pitfall:** updating the pagemap and span state in
> *different* critical sections is the classic use-after-free in classification — **W3-6 owns the single
> protocol** and W4-2b/W5-5 must both route through it (never poke the pagemap directly).

> **▸ Decomposition — W3-1 (bootstrap metadata, S-007).** Split into the bump core (W3-1a) and the lifecycle
> (W3-1b: idempotent init + hand-off). It must never re-enter the public allocator (the allocator does not
> exist yet) — this is the metadata analogue of plan 05's TLS bootstrap rule (W16-2). Lands in **M0** so plan
> 04's extents and W3-2's descriptors have somewhere to live.

---

## W5 — Spans, slabs, bitmaps & central free lists

**Goal:** the span/slab layer and the central free list, including empty-span detection across caches.
**Depends on:** W3, plan 04 (extents). **Enables:** plan 05, plan 04 (W11 span↔hugepage).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W5-1 | Slab layout (§16.3): `object_i = align_up(base+hdr, align) + i*size`; header/bitmap non-overlap; alignment from the `size`-multiple-of-`alignment` invariant. | M | | object ranges fit + disjoint (mirrors Lean W1-4d). |
| W5-2 | Free bitmap + `central_free_count = popcount(free_bitmap)` (§16.4); bitmap and count updated in **one** critical section (§8.5). | M | | invariant test; no torn updates. |
| W5-3a | Encode the five-term partition `object_count = live + local_cached + transfer_cached + central_free + quarantined` (§16.4) as the span accounting model; mark which terms are exact vs reconstructed. | M | | partition documented; no term double-counts. |
| W5-3b | `central_free_count == popcount(free_bitmap)` maintained atomically with the bitmap — the authoritative *central-residency* invariant. | M | | holds under all central transitions. |
| W5-3c | Debug-exact reconstruction of `local_cached`/`transfer_cached` (via `cache_bitmap` or a cache scan) so the conservation law holds exactly in debug. | M | ∥ | reconstructed counts match observed cache contents. |
| W5-3d | `span_is_empty(span)`: all four non-central terms zero AND `central_free == object_count`; never reads a cached free object as live (§8.4/§16.5). | M | | predicate correct; B.3 empty-detection check passes. |
| W5-3e | Empty-detection trigger protocol: re-evaluate on central insert (flush) and on cache drain so a newly-emptied span is detected and returned, never stranded. | M | | a span emptied only by the *last* cache flush is detected + returned. |
| W5-4a | Central structure keyed `(node, arena, label, sc)` (§14.5, D2): partial-span list + empty-span cache + occupancy counters. | M | | C-001/C-002 hold by construction. |
| W5-4b | `central_remove_batch` (§A.4 / A.2 loop): pull from partial spans → activate an empty/backend span on demand → carve a batch → update counts; return empty so the caller can request a new span and retry. | M | | batch single-arena/label, distinct, correct-size; OOM-retry path exercised. |
| W5-4c | `central_insert_batch`: return objects, update bitmap+count atomically (W5-3b), run empty-detection (W5-3e). | M | | C-003/C-004: empty detected, non-empty never returned. |
| W5-4d | Locking/sharding (§14.5) under the lock hierarchy (plan 05 W16-1); per-`(node,sc)` shards to cut contention. | M | ∥ | no lock-order violation; contention measured. |
| W5-5 | Span activation + return-to-backend (§14.6, C-003/C-005) routed through W3-6; never returns a non-empty span. | M | | empty spans returned; non-empty never; lock-order respected. |

> **▸ Decomposition — W5-3 (empty-span detection), the hardest accounting in the allocator (§16.5).** The
> difficulty: an object cached in a per-CPU/thread/transfer cache is *free* but invisible to its span — its
> bitmap bit is 0, exactly like a live object's. Liveness cannot be read off the bitmap, and a span is empty
> only once *every* cache has released its objects. **Chosen strategy:** keep `free_bitmap`/`central_free_count`
> authoritative for *central* residency (cheap, exact, hot-path); reconstruct `local_cached`/`transfer_cached`
> only in debug (W5-3c) where the full conservation law is checked exactly; performance builds detect
> emptiness *eventually* at the W5-3e trigger points (central insert + cache drain) rather than paying a
> per-span counter on every cache push/pop. **Two failure modes, both tested:** (1) a *leak* — a truly-empty
> span never re-checked, never returned; (2) far worse — a span declared empty while a cache still holds one
> of its objects, letting the backend recycle *live* memory. W5-3d/3e + the B.3 debug check (plan 08 W19-1c)
> + differential tests (plan 08 W21-2) guard both. **Sequencing:** there are no caches to account for until
> M2, so land W5-3a/b/d at **M1** (central-only) and complete W5-3c/3e at **M2** when caches arrive.

> **▸ Decomposition — W5-4 (central free list).** Splitting the *structure* (W5-4a), the *remove* path
> (W5-4b, with the §A.2 OOM-retry loop), the *insert* path (W5-4c, which triggers empty-detection), and the
> *locking* (W5-4d) lets the lock strategy evolve independently of the carve/return logic. The remove path
> deliberately returns `empty` rather than allocating a span itself, so the *caller* (plan 05 refill, or plan
> 06's M1 direct path) owns the "ask the backend for a new span and retry" decision (§A.2) — keeping span
> creation out of the locked central critical section.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M0 | W3-1a/b (bootstrap metadata). |
| M1 | W2 (all), W3-2..W3-6, W5-1, W5-2, **W5-3a/b/d**, W5-4a/b/c (central-only path), W5-5. |
| M2 | **W5-3c/3e** (cache-aware conservation + triggers), W5-4d (sharded locks under the hierarchy). |

## Domain risks

- **R1** (empty-span detection) — owned here; see the W5-3 decomposition. Co-locate debug-exact accounting
  with the fast path; never let the performance build's accounting diverge from the debug law.
- **R4** (table drift) — owned with plan 02 via W2-1's golden-diff gate.
- **R6** (lock-order) — W5-4d/W5-5 must obey plan 05's hierarchy; the lock-order checker is the gate.

## Definition of Done (addendum)

Every W5 WU touching span accounting runs the **B.3** span checks in debug and reconciles the five-term
partition; every pagemap mutation routes through **W3-6**; every new table value comes from the **W2-1**
generator, never a literal.

## Best-practices checklist

- [ ] Over-aligned requests never share a slab (W2-3b).
- [ ] One critical section updates bitmap **and** count together (W5-2); no torn accounting.
- [ ] Central-residency is authoritative + cheap; cache residency is reconstructed in debug, not tracked on
      the hot path.
- [ ] Empty-detection is *triggered* (W5-3e), so emptiness is found, not waited for.
- [ ] Pagemap and span state never move in separate critical sections (W3-6).
- [ ] Span creation stays out of the locked central critical section (W5-4b returns `empty`).

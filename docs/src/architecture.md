<!-- SPDX-License-Identifier: MIT -->
# Architecture

The allocator is a stack of layers over a single backing seam. The full diagram
and rationale are in the plan overview (§3); the M0 skeleton wires a thin
vertical slice through it.

```text
Public API:  C ABI (topomalloc_*) + Rust GlobalAlloc        topo-abi
Request classifier: small/medium/large, size class,         topo-core (classify)
                    align, arena, label, hints
Metadata substrate: bootstrap bump alloc, pagemap,          topo-core (bootstrap,
  span/large descriptors, pointer classification              pagemap, span, ptr_class)
Front/middle/back ends (M1+)                                 topo-core, plans 03/05
        ─────────────────  S E A M  ─────────────────
trait TopoBackingProvider                                    topo-core (backend)
  ├─ PosixBackingProvider  (ambient authority)               topo-backend-posix
  └─ Sele4nSim / Sele4nBackingProvider (capabilities)        topo-backend-sele4n
Formal: Lean model + seLe4n bridge                           lean/
```

## Crates

| Crate | Role | License |
|-------|------|---------|
| `topo-core` | classifier, size classes, the seam, the skeleton allocator (`no_std`) | MIT |
| `topo-abi` | C ABI + `GlobalAlloc` + runtime backend selector | MIT |
| `topo-backend-posix` | `PosixBackingProvider` (degenerate single-authority case) | MIT |
| `topo-backend-sele4n` | `Sele4nSim` + (M1) `Sele4nBackingProvider` | GPL-3.0-or-later |
| `topo-arch` | per-arch identity + fast-path mode (RSEQ lands in plan 05) | MIT |
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
  bump arena over a small static (or early-reserved) region — the floor everything
  else stands on. It has no dependency on the public `malloc` (it only bumps an
  atomic cursor), is lock-free before threading, fails safely on exhaustion
  (`None`, never a wrap), and has an idempotent init plus a hand-off flag for the
  transition to the normal metadata allocator once arenas exist. Span descriptors
  and the pagemap's radix nodes both come from this seam (`MetadataAlloc`), so the
  node source can change at hand-off without the pagemap caring. This is the
  metadata analogue of the §27.5 TLS bootstrap rule: the path that builds the
  allocator must never re-enter it.

- **Span & large descriptors** (`span.rs`, W3-2/W3-5, §16.2/§17.2). The `SpanDescriptor`
  carries the §16.2 fields and *derives the §16.4 conservation law*
  `object_count = live + local_cached + transfer_cached + central_free + quarantined`
  with `central_free = popcount(free_bitmap)` as the authoritative, cheap central
  residency. The cached terms are logical quantities reconstructed in debug (W5-3c)
  and trivially zero before caches exist (M1), so empty-span detection (§16.5) never
  reads a cached free object as live. Every concurrently-read field is atomic, and a
  `generation` counter (W3-5, §16.6/§27.5) is bumped on recycle so a stale reference
  captured before the recycle is detectably invalid (`GenGuard`).

- **Pagemap** (`pagemap.rs`, W3-3/W3-6, §17.1/DD-1). A fixed-fan-out **three-level
  radix over allocator-page numbers** (chosen over a flat array, which wastes virtual
  space on 64-bit, and a hash map, which has worst-case/resize hazards on the hot
  path): O(1) worst-case lookup, lazily populated from bootstrap metadata, no resize,
  interior-pointer lookup by masking an address to its page. For the 16 KiB page (D4)
  the levels are 11/11/12 bits over a 34-bit page number (48-bit VA); a leaf covers
  64 MiB. Leaf slots are **tagged pointers** — `Empty` (the zero word, a non-owned
  page, P-Map-002), `Small`/`Large` (a descriptor pointer), or `ReleasedRetained`
  (a span kept so a released page cannot be reused without recommit, P-Map-005);
  descriptors are ≥ 8-aligned so the low three bits carry the tag. The **publish/read
  protocol** (W3-3c/P-Map-006) is the subtle part: a leaf slot is filled with a
  *release store* only after its descriptor is fully initialized, readers
  *acquire-load*, and new radix nodes are zeroed before being linked with a release
  CAS — so a concurrent classifier sees either `Empty` or a fully-formed entry, never
  a half-built node. Because descriptors live in monotonic metadata and are recycled
  in place with a generation bump, a stale pointer is always dereferenceable and the
  generation flags the reuse (the §27.5 use-after-free the SPEC warns of). **This
  module is the single mutator** (W3-6): span split/merge (plan 04 W4-2b) and span
  lifecycle (W5-5) route every pagemap change through `install_span`/`release_span`/
  `retire_span`/`install_large`, never poking a leaf directly.

- **Pointer classification** (`ptr_class.rs`, W3-4, §17.5). `classify_ptr` consults
  the pagemap and the metadata ranges and returns the §17.5 class — `Null`, `Small`
  (with the object index, by the §16.3 slab-layout inverse), `Large`, `Interior`,
  `Metadata`, `Released`, `Quarantined`, or `External`. `free` requires a **base
  pointer** (§17.5); `validate_free` enforces exactly that, mapping a base pointer or
  `Null` to a `FreeTarget` and an interior/foreign/released/metadata/quarantined
  pointer to an `InvalidFree` that debug/hardened builds *detect and report* — never
  act on (W3-4b, ties plan 08 W18-2).

The runtime pagemap is held to the **same soundness property the Lean model proves**:
`pagemap_lookup_sound` (plan 02 W1-8b) — "if the pagemap maps an address's page to a
span, that span is real and its range contains the address" — is discharged against
the radix implementation by the W3-3d property test (`tests/tests/pagemap.rs`), so a
divergence between the proof and the implementation fails CI.

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

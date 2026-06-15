<!-- SPDX-License-Identifier: MIT -->
# Decision record — D3–D8 (W0-1)

This is the ratified record of the open decisions from
[`planning/plans/README.md`](../planning/plans/README.md) §4.2. Each is closed
here with its outcome and rationale. D1 and D2 are already ratified in the
overview (§4.1) and are not repeated.

| ID | Decision | Outcome | Status |
|----|----------|---------|--------|
| D3 | Build orchestration | Cargo workspace + `cargo xtask` driving `lake` + codegen | **Ratified, implemented** (W0-4) |
| D4 | Allocator page / `small_max` | `16 KiB` page; `small_max` finalized with the tuned table at M1 | **Ratified (page); small_max at M1** |
| D5 | Licensing | core **MIT**; seLe4n integration **GPL-3.0-or-later** + `NOTICE` | **Ratified, implemented** (W0-12) |
| D6 | Arena routing in caches | bound-arena fast path → arena-qualified slots at M4 | **Ratified, deferred to M2** |
| D7 | Property / fuzz stack | `proptest` + `cargo-fuzz` + a custom differential harness | **Ratified, scaffolded** (W0-7) |
| D8 | Consuming seLe4n crates | git dep pinned to a SHA + vendored mirror + periodic-bump WU | **Ratified; pin recorded** |

## D3 — Build orchestration

`cargo xtask` is the single entry point (`xtask setup/build/gen/test/fmt/lint/lean/bench/ci`).
It orchestrates **both** toolchains: `cargo` for Rust and `lake` for Lean, plus
the codegen pipeline and cross builds. CI invokes the same subcommands, so no
build logic lives only in YAML. See [`CONTRIBUTING.md`](../CONTRIBUTING.md).

### Lean toolchain pin (W0-3, related to D8)

`lean-toolchain` pins **`leanprover/lean4:v4.28.0`** — deliberately the same Lean
version upstream seLe4n uses (per its `scripts/setup_lean_env.sh`), so the Lean
bridge (which imports seLe4n's public model at M1, D2) never suffers toolchain
skew. It is installed by [`scripts/setup_lean.sh`](../scripts/setup_lean.sh),
which downloads the toolchain tarball directly from GitHub releases and verifies
its SHA-256 (the technique adapted from seLe4n), instead of letting `elan`
resolve it through `release.lean-lang.org` (unreachable from some sandboxed/
proxied environments). seLe4n's Rust MSRV is **1.82**; TopoMalloc pins a newer
stable (`rust-toolchain.toml`) which satisfies it, so the pinned seLe4n crates
(D8) build cleanly under our toolchain.

## D4 — Allocator page, `small_max`, and the medium/large boundary

The allocator page is **16 KiB** (server default, Appendix C). `small_max` — the
largest size served by the front-end size-class table — is **finalized at 32 KiB**
with the tuned, non-uniform table the Lean model emits and verifies (plan 02
W1-4e): 72 classes over `[1, 32768] B`, 16 B-spaced in the alignment-dominated
tiny region and eight geometric classes per power-of-two octave above it
(worst-case spacing ratio 1.125, within the §9.4 targets).

Above the small path the classifier (plan 03 W2-3) splits requests into
**Medium** — a page-rounded extent — up to the hugepage boundary
`huge_threshold = 2 MiB` (= 128 pages; Appendix C `hugepage_size`), and **Large**
at or above it (§9.2 / §A.1 / §18.5). The boundary is decided on
`max(size, alignment)`, so an over-aligned request whose alignment alone reaches
the hugepage threshold is routed to the Large/hugepage path rather than the
medium extent allocator (§9.3 / §25.5).

`huge_threshold` is authored in the golden; `MAX_ALIGN` — the widest class
alignment, used to reject an over-aligned request in O(1) before the lookup — is
**derived** by the generator. Like every size-class value these are generated
constants, never hand-edited (Appendix F). The earlier M0 placeholder table
(`small_max = 128 B`, uniform) existed only to exercise the single-source
pipeline (DD-1) and has been replaced by the tuned table.

## D5 — Licensing (closed before the first `/sele4n` byte, W0-12)

The standalone allocator core is **MIT**. The seLe4n-integration layer is
**GPL-3.0-or-later**, because upstream seLe4n
([`hatter6822/seLe4n`](https://github.com/hatter6822/seLe4n)) is GPL-3.0-or-later
and the integration links/models its ABI. The boundary is drawn **now**, before
any seLe4n-linked code exists, so there is never a relicensing event:

* MIT: `crates/topo-core`, `topo-abi`, `topo-backend-posix`, `topo-arch`,
  `topo-stats`, `topo-control`, `topo-test-support`, `xtask`, `tools/*`,
  `lean/TopoMalloc/**` (except the bridge), `include/**`.
* GPL-3.0-or-later: `crates/topo-backend-sele4n`, `lean/TopoMalloc/SeLe4n/**`,
  `sele4n/**`.

The default `libtopomalloc` artifact links no GPL code and is MIT. Building with
the `sele4n-sim` feature produces a GPL combined work. SPDX headers are enforced
by CI. See [`NOTICE`](../NOTICE).

## D6 — Arena routing in caches

The front-end caches use a **bound-arena fast path** (one arena per cache slot
class), upgrading to arena-qualified slots at M4 when multiple authority domains
become active. No cache code exists at M0; the decision is recorded so plan 05
(M2) implements the fast path directly.

**Update (W9 landed).** Multiple authority domains are now active — the live
multi-arena data path ships ahead of M4 (see the W9 notes below) — but it rides
on the **central** cache, which is already arena-keyed and filters by arena on
remove (W5-4a), so arena isolation (§22.7) holds without front-end caches. D6
remains the standing decision for the *front-end* caches when they arrive (M2):
the per-CPU/thread/transfer slots take the bound-arena fast path, and the W9
`drain_arena` teardown grows its cache-drain hook there (W9-4b/W9-6b).

## D7 — Property / fuzz / differential stack

* **Property testing:** [`proptest`](https://crates.io/crates/proptest) (see
  `tests/tests/property.rs`).
* **Fuzzing:** [`cargo-fuzz`](https://crates.io/crates/cargo-fuzz) +
  `libfuzzer-sys` (see `fuzz/`), nightly-only and opt-in.
* **Differential testing:** a custom harness — the host executable model
  (`topo-test-support::LiveModel`) and `tools/trace-replay` at M0, superseded by
  the Lean executable model (plan 02 W1-10) as the proof-grade oracle.

## D8 — Consuming the seLe4n crates (closed before plan 09 compiles)

The seLe4n Rust ABI is consumed as a **git dependency pinned to an exact SHA**,
with a vendored mirror and a periodic-bump work unit (plan 09). The pin as of
W0:

| Field | Value |
|-------|-------|
| Repository | `https://github.com/hatter6822/seLe4n` |
| Commit (pin) | `57c11054d31b819364d8089268ab38927881ab0b` |
| Crates | `sele4n-abi`, `sele4n-types` (`sele4n-sys`/`sele4n-hal` as needed) |
| Upstream workspace version | `0.31.50` |
| Upstream MSRV | `1.82` |
| Upstream license | GPL-3.0-or-later |

At M0 the dependency is **recorded but not yet linked**: `topo-backend-sele4n`
ships only the host `Sele4nSim` (no upstream dependency), so the M0 build stays
hermetic. M1 (plan 09 W22-0) enables the pinned git dependency behind the
`real-abi` feature and compiles `Sele4nBackingProvider` against it. The vendored
mirror is produced by [`scripts/vendor_sele4n.sh`](../scripts/vendor_sele4n.sh),
which clones the exact pinned SHA into `vendor/sele4n/` (GPL-3.0-or-later, on the
seLe4n side of the D5 boundary) for hermetic/offline builds. Because the
simulator mirrors the pinned ABI surface, any upstream drift becomes a compile
error (risk R13). See [`docs/ABI.md`](ABI.md).

**Vendor + `real-abi` committed (plan 04 W4-1).** The pinned seLe4n ABI is now
**vendored into the repository** and `topo-backend-sele4n --features real-abi`
links it. `vendor_sele4n.sh` fetched the exact SHA `57c1105…`; the minimal pristine
closed set — `sele4n-types`, `sele4n-abi`, and `sele4n-sys` (a dev-dependency of
`sele4n-abi`, so its manifest must resolve) — lives under `vendor/sele4n/`, GPL-3.0
-or-later, with the full `LICENSE` and the `PINNED_SHA`. A nested workspace
(`vendor/sele4n/Cargo.toml`, excluded from the MIT root) supplies the upstream
`*.workspace` fields. `real-abi` (off by default) pulls `sele4n-types`/`sele4n-abi`
as optional path deps and compiles the `KernelError -> BackendError` bridge
(`UntypedRegionExhausted -> OutOfMemory`; `InvalidCapability`/`UntypedTypeMismatch`/
`UntypedDeviceRestriction`/`UntypedAllocSizeTooSmall`/`AddressOutOfBounds`/
`MappingConflict`/`TargetSlotOccupied -> InvalidRequest`; else `Unsupported`) — the
W4-1 "seLe4n side type-checks vs pinned upstream" evidence, now exercised by a unit
test under that feature.

Supply-chain plumbing (the D5/D8 invariants, all kept):

* **Default build stays hermetic + MIT.** The seLe4n deps are *optional* (off by
  default), so a default `cargo build`/`metadata` resolves nothing from the mirror;
  the MIT default artifact links no GPL (the `xtask` license-boundary check confirms
  `topo-abi` pulls no seLe4n crate). `topo-backend-sele4n` is marked `publish = false`
  (it cannot be published — it links path-only GPL crates).
* **Licenses.** `cargo-deny` allows GPL-3.0-or-later only for `topo-backend-sele4n`
  and the `sele4n-*` exceptions (the standing D5 authorization). The vendored tree's
  own files keep their upstream SPDX + `LICENSE` and are exempt from the per-file
  SPDX gate (the `xtask` scan skips `vendor/`); NOTICE §2/§4 attributes them.
* **No build artifacts committed.** Only the vendored source + manifests are
  committed; `vendor/sele4n/target` and the standalone `vendor/sele4n/Cargo.lock`
  are gitignored (the root lock governs the real-abi build).

## W2 — classifier design notes (plan 03)

Four W2 implementation choices are ratified here; the module docs
(`crates/topo-core/src/{classify,size_class,flags,overflow}.rs`) carry the detail.

* **Medium/large bucketing on `max(size, align)`.** The classifier decides the
  Small/Medium/Large split (§A.1) on the *span* — the smallest region that
  satisfies both the size and the alignment — rather than §A.1's illustrative
  `size`-only test. An over-aligned request whose alignment alone reaches the
  hugepage threshold is thus routed to the Large/hugepage path instead of
  burdening the medium extent allocator with a hugepage-aligned hole (§9.3 /
  §25.5 / §18.5).
* **Full direct-mapped size→class lookup.** The plan sketches a hybrid (direct-map
  small sizes, compute larger ones from the class array). At `small_max = 32 KiB`
  the granule LUT is 2 KiB (`u8 × 2048`), L1-resident and constant-time, so the
  shipped lookup is a single direct map for the whole small range — no second code
  path, no arithmetic class derivation. Ratified over the hybrid. The over-aligned
  escape is factored into `align_walk` (table-parametric, so the integrated path is
  unit-tested against synthetic tables with over-aligned classes — the shipped
  table is uniformly 16-aligned) and **proved** in Lean: `alignWalk_sufficient`
  shows the walk only ever returns a class whose natural alignment covers the
  request (the W2-3b "never share a less-aligned slab" rule). The Lean `lake exe
  check` gate also runs a model-vs-emitted lookup differential (`lookupMatchesModelB`:
  the emitted `sizeToClass` equals the lookup the model recomputes from the row
  sizes), and lifts each evaluated gate to a theorem (`size_class_lookup_minimal`,
  `maxAlign_is_upper_bound`, `huge_threshold_wellformed`).
* **Internal flag/hints model, validated (§10.4).** `RequestFlags` is the
  allocator's internal, validated representation of the advisory flags (zero,
  cache-bypass, guard, hugepage preference, lifetime, hotness, arena routing). It
  decodes to a structured `Hints`; reserved bits and the contradictory
  `NO_HUGEPAGE | PREFER_HUGEPAGE` combination are rejected deterministically
  (`classify` → `None`), satisfying §10.4 without freezing the public C `TOPO_*`
  ABI — plan 06 maps those macros onto this layout. Alignment is a dedicated
  `classify` parameter, never a flag bit, so the "mandatory alignment MUST NOT be
  silently ignored" rule holds trivially. Per-call NUMA-node / explicit-tcache
  routing are deferred to the placement/cache subsystems (plans 15/05).
* **Hugepage rounding: a checked primitive now, page-rounded Large for now.**
  `overflow::hugepage_round` discharges the §9.7 "hugepage rounding overflow"
  obligation as a verified, overflow-safe primitive; the classifier still
  page-rounds Large until the hugepage backend (plan 04) sets the §18.6
  region-cache policy that governs whole-hugepage rounding.

## W3 — metadata, pagemap & classification design notes (plan 03)

The W3 metadata substrate (`crates/topo-core/src/{bootstrap,span,pagemap,ptr_class}.rs`)
makes six implementation choices; the module docs carry the detail.

* **Bootstrap = bump core + lifecycle, with a real hand-off, a static global, and a
  re-entrancy guard.** The monotonic bump arena (`BumpArena`, W3-1a) is split from the
  idempotent-init/hand-off state machine (`Bootstrap`, W3-1b), so the carving logic is
  unit-tested over a caller buffer with no global state. The hand-off is **real**:
  `hand_off_to(successor)` routes new metadata to the normal allocator (§17.4) while
  already-vended bytes stay valid; the `MetadataAlloc: Sync` supertrait makes the
  "safe to call concurrently" contract a type fact (the successor lives behind the
  `Sync` `Bootstrap` and is called from every thread). `Bootstrap::global()` binds a
  `BOOTSTRAP_REGION_BYTES` static reserve lazily (DD-2's "static reservation"),
  profile-aware (smaller under `low-rss`) and faulting in lazily from BSS; on a
  concurrent first-use race `global()` waits for the init winner to publish `Active`
  before returning, so a loser of the init CAS never hands back an
  apparently-uninitialized allocator (no spurious metadata `None`). A
  **re-entrancy guard** (`is_in_alloc()`, a thread-local under `std`/tests, a no-op
  leaf otherwise) *refuses* the S-007 re-entry of the metadata path: `alloc` returns
  `None` rather than recurse — in **every** profile, not only debug — and additionally
  debug-aborts under `debug-assertions`. The arena is **never** freed
  — metadata a classifier may reach must outlive it (§27.5) — so logical reuse is caught
  by a generation, not by reclaiming memory.

* **Pagemap radix: derived depth over the full `usize` range, uniform 8 KiB nodes.**
  The depth is `ceil((usize::BITS − PAGE_SHIFT) / RADIX_BITS)` with `RADIX_BITS = 10`
  (5 levels on a 64-bit / 16 KiB-page target, 2 on 32-bit), so the radix covers the
  **entire address space with no virtual-address-width assumption** — an address from
  5-level paging / a 57-bit VA maps as readily as a 48-bit one (the earlier 48-bit cap
  mis-mapped owned memory on such hosts). Uniform 1024-slot (8 KiB) nodes keep the
  resident footprint low (lazy population, the `low-rss` interest); the cost is a
  couple more dependent loads on the *slow* path only. Leaf entries are tagged pointers
  (low 3 bits; both `SpanDescriptor` and `LargeDescriptor` are ≥ 8-aligned, each pinned
  by a compile-time `const` assert so a field reorder can never collide the tag with a
  real address bit): `Empty` = 0 (a zeroed leaf is valid),
  `Small`/`Large`, `ReleasedRetained` (P-Map-005). Chosen over a flat array (wastes
  virtual space) and a hash map (worst-case/resize hazards), DD-1. `metadata_bytes()`
  reports the bounded overhead.

* **Publish/read with release/acquire + a single lock-free mutator (W3-6).** Leaf
  entries are release-stored *after* the descriptor is initialized; readers
  acquire-load; new nodes are zeroed before a release-CAS links them — so a classifier
  sees `Empty` or a fully-formed entry, never a torn node (F1). Installs are
  **two-phase, hence atomic on metadata exhaustion**: `publish_range` creates every
  radix node the range needs (the only fallible step) *before* publishing any entry, so
  an `OutOfMetadata` leaves every page `Empty` — a failed allocation never leaves a
  page mapped to its descriptor. The pagemap is the **only** mutator (F3): W4-2b and
  W5-5 route through `install_span`/`release_span`/`retire_span`/`install_large`. A
  page maps to exactly one descriptor (P-Map-001): a double-install over a *different*
  descriptor is a caller bug a `debug_assert` aborts on, and release builds **preserve
  the existing owner** (skip the store) rather than silently retarget a live page. Its
  operations are lock-free (atomic publish), so they acquire no lock and cannot violate
  the §27.2 hierarchy; they run inside the caller's span critical section (P-Map-006).

* **Generation for identity + a seqlock for read-consistency (W3-5/W3-4).** Span/large
  descriptors carry a `generation` bumped on recycle (§16.6/§27.5; it **wraps**,
  skipping 0, rather than saturating — saturating would pin the counter at its ceiling
  and let a `GenGuard` captured there match every later incarnation); `GenGuard` lets a
  stashed reference detect a recycle (ABA), and descriptors are never freed, so the
  pointer stays valid. Separately, a **seqlock** version brackets a recycle's geometry
  writes, and `classify_ptr` reads the geometry through it — so a classification racing
  a recycle (a use-after-free signal) is never composed from two incarnations; a
  persistently racing recycle resolves conservatively to `External`. Classification is
  also **total**: because a stale pagemap entry can point a low address at a span the
  seqlock reports re-based *above* it, `classify_in_span` first rejects `addr < base`
  as `External`, so every slab-layout subtraction is underflow-free and the function
  never panics on any address (the recycle holds the span lock across the whole
  geometry update, so a lock-holder never sees the bitmap and `object_count`
  disagree). Totality extends to corrupted metadata: the size-class lookup is
  bounds-checked (`size_class::checked_row`), so an out-of-range `sc` resolves to
  `External` instead of an out-of-bounds panic, and a `const` scan pins every class's
  object size `> 0` so the `delta / object_size` on that path can never divide by zero.
  The §17.3 integrity tag (FNV-1a over the read-mostly header, hashed with **acquire**
  loads) is **consumed**, not merely computed: in a hardened build (`debug-checks`) the
  classification path validates it, and a tag mismatch is *disambiguated* by re-reading
  the seqlock version — genuine corruption only when the version is unchanged, otherwise
  a recycle merely raced the check and the read is retried. So a wild write to a header
  field makes the pointer classify foreign, while a benign concurrent recycle is
  **never** misreported as corruption — closing the gap where the tag was a tested
  capability with no consumer. The `large` path is seqlock-read too (symmetric with the
  span). The two lock-free protocols (the seqlock and the W3-3c publish/read), plus
  the W4 large-free critical section (whose lookup-under-the-pool-lock makes a
  concurrent double-free of one pointer settle on exactly one winner), are
  `loom`-model-checked under `--cfg loom`.

* **§8.5 in one critical section via a per-span lock.** `central_free ==
  popcount(free_bitmap)` is the authoritative central count; the bitmap edit and the
  count update move together under a lightweight per-span spinlock (§27.2's span lock,
  which W5 adopts), reachable only through a `SpanGuard` — so the invariant is updated
  atomically and never observed torn. The cached `local`/`transfer`/`quarantined` terms
  are logical (reconstructed in debug W5-3c, zero before caches), so `is_empty` (§16.5)
  takes them as explicit inputs and never infers liveness from the bitmap (the DD-3
  catastrophe guard).

* **Hybrid bitmap: inline-small, out-of-line-large.** A slab of ≤ `INLINE_BITS` (128)
  objects keeps its bitmap inline; the few high-count classes allocate a class-sized
  bitmap from the metadata seam. The descriptor stays a fixed, compact 96 bytes for
  every class (the earlier all-inline design carried a 128-byte bitmap in *every*
  descriptor), and recycle grows the out-of-line block only when a larger class needs
  it. The descriptor footprint is asserted (W3-2). The pagemap's runtime soundness is
  tied to the proof by an **executable Lean pagemap model** (`PagemapExec.lean`):
  `install_lookup_sound` is kernel-checked (mirroring `pagemap_lookup_sound`), and a
  recorded install/lookup trace is replayed by `lake exe check` and by the Rust
  `pagemap_matches_lean_replay_differential` test against the radix — the W3-3d
  differential loop.

## W4 — back-end extents, physical state & the provider seam (plan 04)

The W4 back-end (`crates/topo-core/src/{extent,large,backend}.rs` plus the
`topo-backend-{posix,sele4n}` providers) makes seven implementation choices; the
module docs carry the detail.

* **A two-layer seam: collapsed POSIX ops + the full §36.6 typed surface.** The
  trait carries the **collapsed** ops the allocator core drives
  (`reserve`/`commit`/`decommit`/`purge_lazy`/`purge_forced`/`release`/
  `revoke_descendants` — the last a default no-op, POSIX having a single ambient
  authority) **and** the **full §36.6 typed surface**
  (`reserve_window`/`create_frame`/`map_frame`/`unmap_frame`/`recycle` over
  `VWindow`/`FrameCap`/`MappedRange`). On POSIX the capability types collapse to an
  address range and the granular methods **default-compose** the collapsed ops, so
  one `reserve` (an `mmap`) realises `reserve_window ∘ create_frame ∘ map_frame`;
  plan 09's capability provider overrides the granular methods with real capability
  operations, and the core above the seam is unchanged (D2). Offering both layers
  (rather than only the collapsed `reserve`/`release`) was the chosen path so the
  §36.6 surface exists *now* for plan 09 without forcing trivial wrapper overrides on
  POSIX.

* **The `ProviderState` machine is the exact §36.6 linear chain, pinned to Lean.**
  `ProviderState::next` mirrors the Lean `BackingState.next`
  (`SeLe4n/UntypedProvider.lean`, W1-11b) **one-for-one** — the single linear
  lifecycle `AuthorizedUntyped → … → RecyclableUntyped`. (An earlier draft added
  reuse-cycle back-edges; those were removed — allocator reuse is the extent
  manager's `ExtentState` concern, not a back-edge of the capability lifecycle.) Two
  checks pin the runtime checker and the proof together so they cannot drift: the
  Rust `provider_next_matches_the_36_6_chain_exactly` test and the
  `lake exe check` `providerChainGate` (the §36.6 analogue of the pagemap
  differential). The seLe4n simulator walks every reservation/release through the
  machine, so the cardinal §36.6 bug (recycling untyped that still has a live client
  mapping — the illegal jump that skips unmap→revoke) trips the checker.

* **The `ExtentState` machine is pinned to Lean too (W4-2d).** The §20.1
  physical-backing lifecycle (`Reserved`/`Active`/`Dirty`/`Muzzy`/`Released`) that
  enforces M-004/M-005 at runtime has its legal transition relation
  (`ExtentState::can_transition`) `debug_assert`ed at every physical-state write and
  pinned 1:1 to the Lean `ExtentState.canTransition` model by the
  `extent_state_transition_matches_lean` test and the `lake exe check`
  `extentStateGate` — the §20.1 analogue of `providerChainGate`. So the machine the
  allocator actually runs cannot drift from the model the `recommit_*`/`release_*`
  theorems reason about. (`split`/`merge` are *structural* geometry ops that derive a
  result state from combined backing, not lifecycle transitions, so they are
  deliberately outside the relation. A forbidden edge carries meaning: nothing
  returns to `Reserved`, and `Active` may not step straight to `Muzzy` — a live
  extent must be freed before it can be purged.)

* **Real `mmap`/`madvise`/`mprotect` on unix; host fallback elsewhere (W4-3).** The
  POSIX provider issues the **real syscalls** on `cfg(unix)`: `mmap` reserves,
  `madvise(MADV_DONTNEED)` decommits / forcibly purges, `madvise(MADV_FREE)` lazily
  purges, `mprotect` commits in guard mode, `munmap` releases — the documented
  §20.4 mapping (per-platform table in the module docs), so `release`/`decommit`
  genuinely return memory to the OS. Over-aligned reservations use the standard
  over-allocate-and-trim. A `cfg(not(unix))` host-allocator fallback keeps the crate
  building everywhere with the same observable behaviour. The dual-backend
  co-equality test (D2) still holds because it compares *abstract* outcomes
  (success + size), which are address- and backend-independent.

* **Guard mode traps use-after-free (§20.5, W4-3b).**
  `PosixBackingProvider::with_guard_pages` reserves `PROT_NONE` and `mprotect`s
  ranges `READ|WRITE` on commit / `PROT_NONE` on decommit, so a stray access to a
  released range **faults** — the §20.5 UAF trap the unmap-aggressive
  (`debug`/`low-rss`) profiles want; a fork-based test asserts the trap. The
  `RetainPolicy` (`Retain` on 64-bit perf, `Unmap` under `debug`/`low-rss`/32-bit)
  selects eager-decommit-on-free; pairing it with a guard provider gives the trap.

* **Extent index = a metadata-backed, index-based slot pool; segregated bins, not a
  BTree (DD-1, recorded deviation).** An allocator-internal structure must never
  re-enter the global allocator (the metadata analogue of S-007), so the free-extent
  index is **not** an `alloc::BTreeMap`; it is a fixed-capacity slot pool from the
  `MetadataAlloc` seam (zeroed → a valid all-unoccupied pool) with **slot-index**
  intrusive links, keeping the whole `ExtentMap` core *safe* Rust (the only `unsafe`
  is one bounds-checked `get`/`put` accessor pair). **Chosen deviation from DD-1's
  `BTree<(len,base)>` sketch:** the by-size index is **size-segregated free lists**
  (binned by `floor(log2(pages))`) and neighbour lookup is via an **address-ordered
  intrusive list** (boundary-tag, O(1) coalescing). Bins make best-fit *exact* (the
  smallest fit is in the lowest non-empty adequate bin) and first-fit O(1)-ish; the
  trade-off is that an exact best-fit scans within one bin (O(bin occupancy)) rather
  than the BTree's O(log n). This is the standard production-allocator design
  (jemalloc/tcmalloc), avoids a balanced tree in `no_std` without a heap, and is the
  structure W11's approximate-bin hugepage filler builds on (§19.3). Exhaustion
  returns `None`/`Exhausted`, never a wrap (§9.7).

* **Split/merge kept separate; M-004/M-005 enforced structurally.** `split` installs
  both halves' metadata before publishing (F1) and is atomic on slot exhaustion (the
  descriptor pop is the only fallible step, first); `merge` retires the absorbed
  descriptor **behind a generation bump** so a captured `ExtentRef` resolves to
  `None` after recycle (F2, "no stale descriptor"). `committed_len ∈ {0, len}` is
  coupled to state for *free* extents (an `Active` extent may be transiently
  uncommitted while the manager commits it — M-005 guarantees backing before *use*).
  **M-004** is structural: only a *free* extent (no live object by construction) may
  be decommitted/released/purged; an op on `Active` is refused (`NotFree`). **M-005**
  is enforced by `alloc` recommitting a `Released` source. The geometric core is
  certified by Lean: `span_split`/`span_merge_preserves_disjointness` (the Rust
  `split` is Lean's `splitLeft`/`splitRight`, `merge` their union),
  `release_to_os_preserves_live_objects` (decommit/M-004), and the new
  `Theorems/Extent.lean` `recommit_*` theorems (commit/M-005). `state_bytes()` gives
  the §20.1 dirty/muzzy/released breakdown that `topo_stats::Stats::record_backend`
  reconciles into the §21.2 stat fields (W4-3a "states reconcile in stats").

* **The large path is wired to classification via a `LargeAllocator` (W4-4 + A4).**
  `LargeAllocator` composes the `ExtentManager`, a `PageMap`, and a **recycling
  `LargeDescriptor` pool** (so a long-running large alloc/free workload does not leak
  a descriptor per allocation): `allocate` page-rounds overflow-safely, bypasses the
  small-object path (§18.5), takes a best-fit extent, and installs a `LargeDescriptor`
  via the W3-6 mutator (`install_large`) so the result is **classifiable** —
  `free`/`usable_size` recover it by pagemap lookup, retire the entry *before* the
  slot can recycle (no stale-address hazard), and return the extent. A
  `RegionCacheHook` (the §18.6 awkward-size hook) gets first refusal; a cache-served
  region (no backing `ExtentRef`) is freed back to the cache, defining the §18.6
  lifecycle that W11-3 fills. Every back-end op is fallible and leaves the state
  well-formed on failure (W4-5): `ExtentMap::check_invariants` (the executable §18
  tiling/index predicate) is `debug_assert`ed after every mutation and held green by
  the property, failure-injection, concurrency, and fuzz (`fuzz/.../extent.rs`) tests.

## W7 — RSEQ / restartable fast paths & per-arch assembly (plan 05)

The W7 fast path (`crates/topo-arch/src/rseq/**`, plus `topo-core`'s
`cpu_cache.rs` and `pinned.rs`) makes per-CPU `pop`/`push` lock-free. It is the
only hand-written assembly in the project and the highest-risk code; six
implementation choices are ratified here, the module docs carry the rest.

* **Correct before fast — one front-end contract.** The locked per-CPU baseline
  (W6-4) ships first and is *never* replaced, only fronted. The RSEQ path and the
  seLe4n pinned-core path implement the *same* `FeOutcome` contract and must be
  **behaviourally equivalent** to the locked path (P-003, §12.1). `CpuCache` is
  mode-aware: `enable_rseq()` flips `fe_pop`/`fe_push` to the restartable sequence
  when the platform supports it, and on *any* condition the sequence cannot handle
  (a non-owner holding the per-CPU lock, an uninitialised slot, the abort retry
  bound) it falls through to the locked path on the current CPU. Equivalence is the
  acceptance criterion (G-fast), proven empirically by a pinned outcome-for-outcome
  comparison and a forced-migration token-conservation differential against the
  locked baseline — *not* merely "it passes".

* **Registration: use glibc's area, self-register as the fallback — "support
  both".** On the supported `-gnu` targets glibc ≥ 2.35 auto-registers an `rseq`
  area per thread and exports `__rseq_offset`/`__rseq_size`; we read it at
  `thread_pointer + __rseq_offset` (a direct `%fs`/`tpidr_el0` register read — *no
  TLS of our own*, no allocation, the DD-4-correct path). Where glibc did **not**
  register (musl, glibc < 2.35, the rseq tunable off) we self-register our own area
  via the raw `rseq(2)` syscall, stored in a `thread_local!` (the `std` fallback).
  RSEQ mode is enabled only when the non-owner fence
  (`membarrier(…_PRIVATE_EXPEDITED_RSEQ)`) is also registerable, so W7-4 stays sound
  wherever the fast path runs at all. Everything else (no kernel rseq, other arch,
  non-Linux) reports unavailable and uses the locked baseline.

* **The self-registration area uses stable `thread_local!`, not nightly
  `#[thread_local]`.** This is a deliberate, measured choice tied to the DD-4 / R3
  TLS-re-entrancy rule. The *primary* (glibc) path owns no TLS, so DD-4 is satisfied
  by construction on every supported target and in CI; the TLS-model question only
  touches the narrow self-registration fallback. A `const`-initialised
  `thread_local!` compiles to **Local Exec** (a direct `%fs:…@TPOFF` load, no
  `__tls_get_addr`, no lazy-init guard) in an executable — verified by inspecting the
  generated asm — i.e. exactly DD-4's "a static `__thread` slot reached without a
  dynamic TLS allocation". It is General Dynamic only as a `cdylib`, and even then
  only allocates for a `dlopen`ed module's *first* access; we neutralise that by
  registering **explicitly at thread start** (§27.6), never lazily on the malloc
  path, and by letting the fast path receive the resolved area pointer from the
  caller's IE bootstrap (plan 06 / W16-2). Nightly `#[thread_local]+IE` was rejected:
  it would break the ratified stable-toolchain invariant (W0-3) for a benefit that is
  (a) only on a fallback the `-gnu` targets never run, (b) not free (also needs the
  nightly `-Z tls-model` flag), (c) no cure for the `dlopen` case (IE can fail to
  load there), and (d) without a consumer here (the `no_std` target, seLe4n, uses
  pinned-core, not rseq).

* **The restartable sequence shape (§12.3, the non-negotiable rules).** Each
  per-arch `pop`/`push` is a single `asm!` critical section: arm the descriptor
  (`rseq.rseq_cs = &cs`), then `start_ip` loads `cpu_id` **inside** the CS (so
  migration after the load aborts), computes `&cpus[cpu]`, checks the per-CPU lock
  byte and the buffer pointer (diverting to the locked path if set/null), bounds-
  checks, and ends with **one committing store** of the new length. Everything before
  the commit is a load or a register op, so an abort before it is a logical no-op
  (the plan 02 W1-7 frame condition); for `push` the object is staged into the
  logically-free `buf[len]` and published only by the `len` increment. The descriptor
  lives in a `__rseq_cs` section; the abort handler sits in `__rseq_failure` prefixed
  by the four `RSEQ_SIG` bytes the kernel verifies. **No calls and no possibly-
  faulting reference** in the CS: every reference is to already-resident per-CPU
  metadata, and an `xtask` lint (`check_rseq_cs`, W7-2d) fails the build on any
  `call`/`bl`/`blr`/`svc`/`syscall` in the sequence files. AArch64 is **co-primary**
  (it is the seLe4n target), not a port; its only deviation from x86-64 is a
  load-acquire (`ldarb`) on the lock byte for the weak memory model.

* **W7-4 non-owner coordination: per-CPU lock checked in the CS + an RSEQ fence.**
  A thread draining a CPU it is not running on (idle-CPU flush) takes that CPU's
  spinlock — so new sequences see the lock byte and divert — and then issues
  `membarrier(…_PRIVATE_EXPEDITED_RSEQ)`, which aborts any *in-flight* sequence on
  other CPUs before it can commit (§27.4). Owner-side batch ops (the common
  refill/flush on the current CPU) need no fence: only a thread running on a CPU can
  mutate its slot via RSEQ, the kernel aborts a preempted sequence, and that thread
  is the one holding the lock. `pop_batch`/`push_batch` fence exactly when `core` is
  not the caller's current CPU.

* **seLe4n pinned-core mode (§36.10 option 1) behind the same contract.** seLe4n is
  not Linux and has no `rseq`; the per-core fast path there is a *software*
  restartable sequence (`fe_pop_pinned`/`fe_push_pinned`) that reads the current core
  from an injectable `CoreProvider` (the runtime's per-core identity, the analogue of
  `cpu_id`), **aborts with no state change** if the thread is not on the expected
  core, and commits with a single store only when the core is stable across the read
  — mirroring the Lean `per_core_cache_abort_no_change` obligation (W1-12d). Under the
  pinned-thread contract the thread is the sole accessor, so the read-modify-write is
  race-free; a fully migration-atomic commit on a non-pinned thread is option 2 (a
  kernel restartable section) and slots in behind the same contract when seLe4n
  exposes that ABI. The migration flush/hand-off is the caller's responsibility
  (flush or make the cache unreachable before affinity changes).

* **Coverage honesty (what runs where).** On x86-64 CI, glibc registers rseq, so the
  sequences, the forced-migration conservation, and the signal-near-CS battery run
  against the **real kernel mechanism**. Under qemu-user (AArch64 CI) rseq is
  unavailable, so the cache uses the locked fallback there; to still exercise the
  AArch64 *instruction encoding/logic* in CI, a test drives the sequence with an
  explicit, unregistered area whose `cpu_id` it controls. The AArch64 kernel-*restart*
  property is validated on a **native arm64 CI runner** (`ubuntu-24.04-arm`, a real ARM
  kernel where glibc registers rseq) — the only place real preemption/migration
  exercises the AArch64 commit (the seLe4n RPi5 target is the same arch). The
  `#[repr(C)]` `CpuSlot`/`PerCpu` layout the asm addresses is pinned by `offset_of!`
  const guards; the asm also bounds-checks the kernel `cpu_id` against `MAX_CPUS`
  before forming a slot address (a migration to a CPU beyond the array on a
  >128-core host would otherwise be an out-of-bounds commit).

* **Performance is measured, not asserted.** `crates/topo-core/benches/cpu_cache.rs`
  compares the RSEQ fast path against the locked baseline; on a 4-core x86-64 host
  with RSEQ active the push/pop round-trip is **≈ 21 ns → ≈ 12.5 ns (≈ 40 % faster)**
  and a 16-op burst **≈ 265 ns → ≈ 190 ns (≈ 28 %)** — the per-op lock-CAS the
  restartable sequence removes (plus one saved thread-pointer read on the hot path).
  Non-gating (`cargo xtask bench`).

* **Concurrency runs under TSan (the DoD addendum).** `cargo xtask test --kind tsan`
  (opt-in nightly, a gating CI job) runs the equivalence/W7-4/battery and the W6
  cache-concurrency tests under ThreadSanitizer; all pass clean, validating the
  locked path, every atomic, and the W7-4 lock/fence coordination as race-free.
  **Blind spot:** TSan instruments compiler-generated accesses, not inline assembly,
  so the RSEQ sequence interior is invisible to it — the asm-vs-atomic interactions
  are covered by the conservation tests instead.

* **Three modes behind one `fe_pop`/`fe_push`; W7-4 fence validated.** `CpuCache` is a
  runtime three-way mode (`Locked`/`Rseq`/`PinnedCore`); `enable_rseq` (Linux) and
  `enable_pinned_core` (seLe4n, given a per-core oracle) select the fast paths, and
  the shared `fe_pop`/`fe_push` dispatch to them — so the seLe4n pinned path is behind
  the *same* entry point, not just the same return type. The idle-CPU flush goes
  through `CpuCache::drain_cpu`, which is strict **hand-over-hand**: it pops each chunk
  under the per-CPU lock + a non-owner RSEQ fence, then **releases the lock before**
  the sink takes a transfer/central lock — so the per-CPU lock is never held while a
  middle-end lock is taken, and it cannot form a cycle with the refill path (which
  takes the transfer lock and then, separately, the per-CPU lock). Lock-free
  `is_initialized`/`len != 0` pre-checks skip the (all-CPU-IPI) fence on empty and
  uninitialised slots, so the idle path fences only when it actually drains. The fence
  is validated with a test membarrier at `enable` time, so RSEQ mode is only selected
  if the fence actually works; the per-use fence return is `debug_assert`ed. Pinned
  mode's non-owner coordination is the §36.10 hand-off contract, not the membarrier.

* **One stable-Rust limitation, documented.** The glibc-area path takes a **hard** link
  reference to `__rseq_offset`/`__rseq_size` (glibc ≥ 2.35). On a `-gnu` target linked
  against older glibc the crate fails to *link* (not just fall back); the robust fix is
  weak linkage, which is nightly-only, so it is rejected to keep the stable invariant
  (W0-3). The supported `-gnu` targets are modern; musl is unaffected (it uses
  self-registration, which references no glibc symbol).

* **Review-driven correctness fixes (PR #10).** Three subtle bugs an automated review
  caught, all real, all fixed and the reasoning recorded: (1) the abort-handler
  **signature is architecture-specific** — `0x53053053` on x86-64 but `0xd428bc00`
  (`BRK #0x45E0`) on AArch64; using the x86 value on AArch64 made a real abort
  `SIGSEGV` instead of jumping to the handler (caught only on a native-arm64 runner,
  where RSEQ is truly active). (2) The AArch64 push commit is a **store-release**
  (`stlr`), so a reader that acquire-observes the incremented `len` also observes the
  staged `buf[len]` (the weak model would otherwise allow a stale/zero read). (3) The
  self-registration probe treats `EBUSY` as **unavailable**, not success: `EBUSY` for
  our fresh area means a *foreign* rseq area is already registered (musl / another
  runtime), so using ours would arm `rseq_cs` in an untracked area; the kernel-set
  `cpu_id` distinguishes "our area won" from "a foreign area owns it".

## W8 — public API & ABI over the M1 central path (plan 06)

The W8 surface (`crates/topo-abi`, `topo-core/src/allocator.rs`,
`include/topomalloc.h`, `include/topomalloc_new_delete.hpp`) makes nine
implementation choices; the module docs carry the detail.

* **The M1 engine is a composition, not a new mechanism.**
  `topo_core::Allocator` (W8-1a) composes the proven plan-03/04 parts —
  classify → `CentralCache` (small, one object per `remove_batch` at M1) and
  `LargeAllocator` (medium + large) — borrowing a caller-supplied `PageMap`
  and metadata arena (`MetaArena`). Span backing comes from a
  dedicated `ExtentManager` region; span descriptors live in a recycling,
  metadata-backed pool (the `LargePool` pattern: never freed, generation-
  bumped on reuse, §27.5; the base kept as a *pointer* so object addresses
  retain provenance). The span lifecycle's two cross-structure transitions
  are single-owner by construction: creation by the `NeedSpan` observer (a
  lost race only activates an extra span), retirement by exactly the thread
  whose insert emptied the span. Retirement ordering is load-bearing and
  documented: deactivate → clear pagemap entries → free the extent — the
  pagemap must be clean *before* the address range can be reused
  (P-Map-001). No front-end caches at M1: W16-4 (plan 05) wires them under
  this unchanged API at M2.
* **Two free-path hardening fixes in `central.rs` (found by W8 review).**
  (1) `insert_batch` rejects spans whose state is no longer `Active` under
  the bin lock — a stale/double free racing a deactivation would otherwise
  "insert" into the cleared bitmap and underflow `live_count` in release
  mode. (2) The empty-span transition is owned by the insert that *made*
  the span empty (`inserted > 0`): a zero-insert double free can no longer
  re-link an unlinked span or claim a second deactivation (which would
  corrupt `span_count` and re-release retired pagemap entries). Both have
  debug-loud / release-safe tests, and `xtask ci` gained a release-mode
  `topo-core` test pass so the `cfg(not(debug_assertions))` semantics
  actually run in CI.
* **Pointer-consuming entry points are `unsafe` for Rust callers.** With a
  real recycling `free`, a *stale* pointer aliasing a recycled live
  allocation is indistinguishable from a valid free — classification rejects
  everything else (foreign/interior/metadata/released/double-free) with no
  state change (§35.2), but that one class makes a safe `free` unsound.
  `Allocator::free/realloc`, their `AnyAllocator` mirrors, and the
  `free`/`realloc`-family `extern "C"` exports carry the explicit contract
  (`unsafe extern "C"` changes nothing for C callers). Allocation-only and
  read-only entries stay safe. The same promotion now covers
  `LargeAllocator::free`/`free_with` (the internal W4 building block had the
  identical window) — closed by the W8 self-audit, no residual debt.
* **errno is a protocol, not a side effect (W8-1b).** Two combinators in
  `topo-abi::errno_shim` implement §10.1: `alloc_protocol` (preserve the
  caller's errno across success — backend syscalls may clobber it — set
  `ENOMEM` on null) and `preserving_errno` (free family, pure queries).
  `EINVAL` is set explicitly at validation sites. Non-unix hosts compile a
  no-op shim; the return values carry the whole story there.
* **Zero-size policy: one lazily-initialized atomic (W8-4).** Default
  `zero_unique` (the dominant-platform/glibc expectation, §9.6);
  `TOPOMALLOC_ZERO_SIZE=null` or `set_zero_size_policy` flips to
  `zero_null`. The env read uses `libc::getenv` (allocation-free), so
  consulting the policy can never re-enter the allocator in
  `#[global_allocator]` deployments. `realloc(p≠NULL, 0)` is fixed
  free-and-NULL under both policies; `free(NULL)` is always a no-op.
* **realloc states its alignment; it does not inherit one (§25.4).** The
  engine's move path satisfies exactly the alignment the *call* states
  (fundamental for C `realloc`, `TOPO_ALIGN_LG` for `topo_rallocx`) — the
  glibc/jemalloc rule. The first draft preserved the original allocation's
  alignment across moves; the C ABI harness caught it: an alignment-
  inherited move changes the storage *family*, breaking sized-free hint
  coherence (`sdallocx(p, size)` no longer classifies to the allocation's
  actual storage) and pinning small reallocs to 16 KiB extents. In-place
  paths (same small class; medium/large whose page-rounded size still fits
  the extent) commit nothing and so cannot fail; the move path is
  allocate-before-free with `copy = min(old_usable, new_size)` and
  zero-then-copy under `TOPO_ZERO`. The composition is pinned in Lean:
  `reallocMove = free ∘ malloc` with `realloc_move_preserves_wellformed`
  (chaining the proved malloc/free preservation) and
  `realloc_move_window_keeps_old_live` (old and new are simultaneously live
  in the intermediate state, so §8.3 live-disjointness covers the copy).
* **Sized-free hints are cross-checked, never trusted (W8-3).** The C23
  `free_sized`/`free_aligned_sized`/`topo_sdallocx` size+alignment hints are
  verified against the classifier when `debug_assertions` or the core's
  `debug-checks` feature is on — a mismatch aborts (a heap-corruption
  signal); the actual free is always the classified path, so a wrong hint
  cannot corrupt state in *any* profile. Plan 08 W18-2 turns the full check
  into production sampling.
* **C++ operators are an opt-in header, not exported symbols (W8-5).**
  `include/topomalloc_new_delete.hpp` defines all replacement forms (scalar/
  array, nothrow, C++14 sized, C++17 over-aligned) over the C entry points,
  with the conforming `new_handler` loop and zero-size-means-one semantics;
  a program opts in by including it in exactly one TU (the mimalloc/jemalloc
  deployment model). Exporting mangled operator symbols from the default
  artifact would hijack linkees' allocators — that belongs to plan 10's
  override artifact, with interposition.
* **W8-8 "generated header" is realized as machine-verified equivalence.**
  The function-declaration surface of `topomalloc.h` stays hand-authored
  (the generated artifact remains `topomalloc_tables.h`), and `xtask
  abi-test` enforces what generation would have only approximated: the
  exported `topomalloc_*`/`topo_*` symbol set (via `nm`) must exactly equal
  the header's declarations, the header must compile under C11 *and* C++17
  (`-Wall -Wextra -Werror`), both harnesses call every entry point, and the
  flag layout is pinned numerically on both sides. Drift in either
  direction fails CI, which is strictly stronger than a generator whose
  output could drift from intent.
* **Extended-API flags: a public 64-bit word mapped at the boundary
  (W8-6).** The public layout (documented in `extended.rs` and the header)
  is independent of the internal `RequestFlags`, exactly as `flags.rs`
  planned; `decode_flags` validates totally (§10.4) and rejects
  deterministically with `EINVAL`. At M1 the acted-on flags are
  `TOPO_ALIGN_LG`, `TOPO_ZERO`, and `TOPO_ARENA(0)`; the placement hints
  validate and thread through `Request::flags` for plans 04/05/07/08.
  Requests naming a nonexistent arena fail as allocation failures until W9
  lands the arena API.

### W8 self-audit completions (second pass)

A deliberate post-landing audit of W8 found two defects and a set of
incompletely-discharged obligations; all are now closed. The fixes, in
severity order:

* **Sized-free cross-check: fit, not equality (defect, fixed + regression
  test).** The debug/hardened `free_sized` checker demanded
  `align_up(size, PAGE) == usable` for medium/large — but the in-place
  shrink keeps the original extent, so the *truthful* (last-requested) size
  legitimately rounds below the usable size, and a correct C23 program
  aborted in debug builds (reproduced before fixing). The comparison is now
  `<=` for medium/large (exact-class equality stays for small, whose
  in-place path is same-class only): in a corruption *heuristic*, a false
  accept merely weakens it, a false reject is a bug. Pinned by
  `free_sized_accepts_truthful_size_after_inplace_shrink`.
* **`MetaArena` replaces the unsound-as-safe `reserve_meta_arena` (defect,
  fixed).** The old helper returned a `BumpArena` aliasing provider memory
  while only *documenting* that the provider must outlive it — and both
  providers reclaim their reservations on `Drop`, so safe code could dangle
  the arena. `MetaArena<P>` **owns** its provider: the borrow checker now
  enforces the §27.5 lifetime story (allocator borrows arena ⇒ arena and
  backing outlive it), one leak pins the whole metadata backing in the ABI,
  and tests scope it naturally.
* **Liveness probes + the `recognizes` routing predicate.** `owns`,
  `usable_size`, and `realloc`'s source classification now consult the span
  free-bitmap (`SpanDescriptor::is_central_free`, a lock-free advisory
  read): a freed small object awaiting reuse is no longer reported live,
  `realloc`-of-freed fails (`EINVAL` at the ABI — making the header's
  documented behavior true) instead of silently aliasing the free list, and
  `malloc_usable_size`/ownership probes return 0/false. The mixed-allocator
  split is now two predicates: `owns` = live object; `recognizes` = any
  engine-managed address (live, freed, interior, retained, metadata). The
  `GlobalAlloc` adapter routes on `recognizes`, so a contract-violating
  freed engine pointer can never be misrouted to `System`.
* **Arena errors are one deterministic `EINVAL` (§10.4).** "No such arena"
  previously surfaced as `ENOMEM` for encodable ids and `EINVAL` for
  unencodable ones; `decode_flags` now rejects every non-default arena until
  W9 lands the arena API, so the failure is a single invalid-argument code
  at any id magnitude (C- and Rust-side tests pin it).
* **errno shim: graceful platform matrix.** The per-OS accessor list now
  covers Linux/Android/Emscripten (`__errno_location`), Apple + FreeBSD
  (`__error`), OpenBSD/NetBSD (`__errno`), and Solaris/illumos (`___errno`);
  any *other* host compiles the documented no-op shim instead of failing the
  build (previously an unlisted unix was a compile error).
* **Stats wired and reconciling (the deferred DoD item).** The engine keeps
  two relaxed counters (allocated/freed usable bytes; `live` is their
  difference by construction, so the §8.6 application identity cannot
  drift) and `Allocator::stats()` snapshots them with central-free bytes
  (Σ class `total_central_free × size`), both regions' §20.1 `StateBytes`,
  pagemap metadata bytes, and live span/large counts.
  `topo_stats::Stats::record_allocator` maps the snapshot into the
  Appendix-D JSON (regions summed through `record_backend`); reconciliation
  is asserted by an engine unit test over a full alloc/free/realloc cycle,
  by the fuzz target after every input, and over the simulator backend by
  the G-sim suite.
* **Zero-size policy state moved to the core; control key added.** The
  policy atomic lives in `topo_core::compat` (`no_std`), the ABI applies
  the `TOPOMALLOC_ZERO_SIZE` env default once (an explicit set always
  wins), and `topo-control` exposes `topo.compat.zero_size` — closing the
  "knob ⇒ control plane" DoD line. The policy *matrix* test moved into its
  own integration binary (`tests/tests/zero_size_policy.rs`) so flipping
  the process-global policy can never race sibling tests.
* **Checked metadata sizing.** `fixed_pool_metadata_bytes`' per-slot bound
  for the private extent/large slot types is now a named constant pinned by
  a compile-time assert against the real `size_of` values, and the ABI's
  `META_BYTES` carries a compile-time 4× headroom assert against
  `AllocatorConfig::DEFAULT` — a config growth that outruns the arena fails
  the build, not the first allocation.
* **Verification closures.** A loom model
  (`w8_span_retirement_is_claimed_exactly_once`) exhaustively checks the two
  free-path guards: across every interleaving of the two emptying frees and
  a racing double free, exactly one caller claims exactly one deactivation
  and no count underflows (writing the model also sharpened the invariant's
  statement: when the duplicate free arrives *first* it is the
  indistinguishable winner — exactly-once is the global property, not
  per-thread innocence). Miri (with `-Zmiri-ignore-leaks` for the tests'
  deliberate §27.5 metadata leaks) passes the engine and central suites —
  the provenance-careful `object_ptr` path is machine-validated. A fourth
  fuzz target (`malloc_api`) drives arbitrary allocate/free/realloc/probe
  streams against a fresh engine asserting content survival, probe
  agreement, §25.1 failure safety, §8.6 reconciliation, and backend
  well-formedness (30k-run smoke campaign clean). The
  `new_allocator_named("sele4n-sim")` arm — previously compiled but never
  executed — now runs in the G-sim suite, and the `tests` crate's
  `sele4n-sim` feature correctly lights up `topo-abi/sele4n-sim`. The C
  harness pins `topomalloc_size_class_t` with `_Static_assert`
  sizeof/offsetof (§35.3), and the C++ harness exercises the `new_handler`
  loop, the `bad_alloc` throw path, and concurrent new/delete.
* **Performance recorded (non-gating).** Criterion on the 4-core x86-64 dev
  host: `size_class` ≈ 1.6 ns, `classify` ≈ 4.6 ns (5.3 ns with a
  non-trivial flag word), and the full `malloc(64)+free` round-trip on the
  M1 central path ≈ **131 ns** — the per-op central+span lock cost the
  M2 caches and M3 RSEQ path (≈ 12.5 ns per cached op, W7) exist to remove.

## W9 — capability-backed arenas over a live multi-arena data path (plan 06)

W9 makes an arena two things at once (D2): a jemalloc-style **policy domain**
(§22) and a seLe4n-style **capability-controlled resource domain** (§36.4) — the
types live in the MIT core (`crates/topo-core/src/arena.rs`), trivial/ambient on
POSIX, real on seLe4n. The design notes, in the order they matter:

* **Shared-region, arena-tagged isolation — not per-arena regions.** §22.7
  permits "shared global backend structures … only if each extent retains its
  arena identity." The live multi-arena path takes that route: one engine, one
  central cache (already keyed `(node, arena, label, sc)`, W5-4a), one span +
  one large region, but **every span and every large descriptor carries its
  requesting arena**. The alternative — a full engine (with its own pools) per
  arena — was rejected because re-creating an arena would re-carve pools from
  the never-freed metadata arena (§27.5), an unbounded leak under arena churn.
  Tagging keeps metadata bounded (spans/larges recycle) while satisfying every
  §22.7 clause: each extent/span/cache-entry belongs to exactly one arena.
  Per-arena cache *sharding* (D6, W5-4d/W6-6) stays an M2/M4 performance concern;
  it is not needed for correctness here (no front-end caches exist at M1).
* **The default arena fast path is untouched.** `allocate` routes by the
  requested arena; the ambient default arena (id 0, all rights, unlimited quota,
  always `Active`) admits unconditionally, so the existing hot path keeps its
  characteristics. Explicit arenas pay one lock-free state/rights load plus a
  quota compare-exchange (`ArenaTable::try_charge`), and credit on free
  (`credit`, saturating so a stale/double free can never underflow). The result
  is the §8.6/§36.17 reconciliation `Σ arena.used == live_bytes`, asserted
  through an alloc/free cycle *including a reset that discards live objects*
  (the discarded bytes are accounted as freed so the global counter stays exact).
* **The lifecycle is a state machine with a non-`DESTROYED` failure state.**
  `ArenaState` is the §22.3 set plus §36.13 `ErrorQuarantined`; `can_transition`
  is the exact legal edge set (mirrored, and proof-checked, in
  `lean/TopoMalloc/ArenaLifecycle.lean` — the new abstract transition the
  governance rule requires). Reset is `Active → Resetting →` drain `→ Active`;
  destroy is `Active → Draining →` drain `→ Destroyed`; either drain failure
  `quarantine`s to `ErrorQuarantined`, which Lean proves terminal — so a partial
  failure can **never** be reported as a clean teardown (§36.13). The default
  arena rejects reset/destroy (§22.5).
* **Revocation is ordered and step-isolated; POSIX is the collapse.**
  `RevocationPhase` pins the §36.13 / DD-3 order (drain → unmap → **revoke** →
  recycle → finalize) with Lean lemmas `unmap_before_revoke` /
  `revoke_before_recycle`. On POSIX the unmap/revoke/recycle steps collapse to
  "free the extent" (a no-op revoke under single ambient authority); the real
  VSpace unmap, capability revocation, and untyped recycle drop in at plan 09
  **with no change above the seam**, because the structure is identical.
* **Delegation is attenuation-only, mirroring Lean `DelegatesFrom`.**
  `ArenaTable::delegate` enforces the three §36.4 monotonicity invariants —
  rights ⊆ parent (`CapRights::attenuates`), quota ≤ the parent's *remaining*
  budget, label preserved — the runtime image of the bridge's `DelegatesFrom`.
  A property test drives arbitrary parent/child rights and quotas and asserts
  delegation succeeds *iff* it is a sound attenuation and never widens authority.
* **Id assignment retires-then-recycles behind a generation bump.** The
  `ArenaTable` is a fixed-capacity registry (ids `0..MAX_ARENAS`, the
  flag-encodable range, pinned to `RequestFlags::MAX_ARENA_ID` by a compile-time
  assertion). Ids are assigned from a high-water mark and only recycled under
  capacity pressure; destroy bumps the generation, so a stale reference to a
  prior incarnation is detectable (§B.5 / §36.13 "generation checks").
* **`realloc` preserves the arena (§25.4).** A move allocates the new object in
  the *original's* arena (recovered from its span/large descriptor), not the
  arena the call's flags happen to name — keeping the storage family a function
  of the live object, not the request.
* **The C ABI is `topo_arena_create/create_ex/delegate/reset/destroy`** over the
  existing `TOPO_ARENA(id)` flag routing (`topo-abi/src/arena_api.rs`). A request
  naming a nonexistent or draining arena is one deterministic `EINVAL` (the
  entry-point existence check, distinct from the `ENOMEM` of a real allocation
  failure); reset/destroy carry the same quiescence/invalidation contract as
  `free`. The five symbols are declared in `include/topomalloc.h` and exercised
  by the C and C++ ABI harnesses; the header↔symbol cross-check now balances 28
  exported functions. The arena summary (`live_arenas`, `numa_bind_failures`)
  reconciles into `topo-stats` and the `topo.arena.*` control keys.

### W9 second pass (optimization completions)

A deliberate completeness pass closed every item the first pass deferred:

* **`configure` (F-005) + the full §22.2 descriptor.** `arena_configure` /
  `topo_arena_configure` reconfigure the non-authority policy (decay/hugepage/
  NUMA/cache-budget/name); a configure can never widen authority (rights/label/
  quota are create/delegate-time). The descriptor now carries `decay`/`huge`/
  `cache_budget` (consumed by W12/W11/W6 when they land); the `hooks` field is
  W10's.
* **Generation split + capability handles (§36.13/§36.14).** The incarnation
  generation (create/destroy) is split from the reset generation (§22.5), so a
  `topo_arena_handle` survives a reset of its own arena but goes stale on a
  destroy+recreate. `topo_mallocx_arena` routes generation-checked — the §36.13
  stale-detection a raw `TOPO_ARENA(id)` flag cannot give.
* **A real revoke-before-recycle seam, fault-injection-tested.**
  `ExtentManager::free_revoking` / `LargeAllocator::free_revoking` revoke a
  backing's descendants before recycling it (§36.6/§36.13); the drain drives
  them, and a revoke failure **quarantines** (`ErrorQuarantined`, never
  `DESTROYED`). A provider whose `revoke_descendants` fails now exercises that
  partial-failure path end to end — it was unreachable dead code before. On
  POSIX `revoke_descendants` is a no-op, so this is the seam plan 09 fills.
* **Error taxonomy → `errno`.** The §36.14 classes map to `EACCES`
  (authority), `EBUSY` (draining), `ENOMEM` (quota), `EINVAL` (else) across the
  lifecycle API and `topo_mallocx_arena` — and, for the allocation cases, the
  flag-routed `topo_mallocx`/`topo_nallocx` front door too (third-pass parity).
* **Trace grammar + verification.** `ARENA_CREATE`/`DELEGATE`/`RESET`/`DESTROY`
  join the §33.7 grammar (round-trip-tested); `Check.lean` gains an executable
  **G-arena** gate pinning `ArenaState`/`RevocationPhase` to the Lean machines;
  a **loom** model checks the quota CAS under contention; a **concurrent
  multi-arena** isolation test and an **`arena_api` fuzz target** were added; and
  **Miri** runs clean over the new arena unsafe.
* **Rights enforcement is the D2 collapse (audited, made explicit).** All four
  `CapRights` are carried (delegation attenuates them; seLe4n enforces them), but
  the POSIX *engine* gates only `ALLOC` — `try_charge` rejects allocation from an
  arena whose cap lacks it (§36.16's authority test, the property intrinsic to the
  arena). `free`/`stats`/`destroy` are ambient on POSIX: the single process holds
  full authority, and gating them on a *delegated child's* rights would wrongly
  bar the *parent*-authority holder from destroying what it delegated. They are
  enforced against the **caller's** cap at the seLe4n resource-server IPC boundary
  (plan 09) — the engine API takes an arena id, not a cap, so it cannot know the
  caller's authority. This is documented on `CapRights` so the asymmetry reads as
  deliberate, not a forgotten check.
* **Zero-size policy uniformity (audit fix).** `topo_mallocx_arena` now applies
  the §9.6 zero-size policy (`zero_unique`/`zero_null`) exactly as `topo_mallocx`;
  the handle path previously skipped it, so a size-0 request returned a unique
  pointer even under `zero_null`. (Also: a `manual_is_multiple_of` clippy warning
  in the `arena_api` fuzz target — the standalone fuzz workspace is outside the
  main `xtask ci` clippy — was fixed.)
* **Documented boundaries (not avoidance).** Cross-label scrub *recording* is
  plan 08 W18-6 (POSIX is single-label, so decommit suffices); per-arena stats
  *rendering* is W17 (M6); label *restriction* (vs. the sound equality) would
  re-open the proven Lean `DelegatesFrom`, so it is an M7 model refinement; the
  full combined phase×`State` refinement is M7 formal hardening. (Quota
  *budget-partition* was once deferred here too; it was **pulled forward** — see
  the third pass below.)

### W9 third pass (PR #13 review hardening)

A round of automated review surfaced five real issues on the live data path —
four defects and one deliberately-deferred refinement pulled forward; each is
fixed with a test (and, for the refinement, a Lean theorem) that pins the
corrected behavior:

* **Every vendable arena id is flag-routable (off-by-one).** `MAX_ARENAS` was
  `256`, one past the flag-encodable range, so `create`/`delegate` could hand
  back id `255` while `TOPO_ARENA(255)` (capped at `MAX_ARENA_ID = 254`) rejected
  it — an arena usable through the advertised allocation path in name only. The
  bound is now `255 = MAX_ARENA_ID + 1`, and a *second* compile-time assertion in
  `flags.rs` pins the converse (every vendable id ≤ `MAX_ARENA_ID`), so the two
  can never drift apart again. (Larger populations still need the §36.14 handle
  surface and a wider internal field, not merely a bigger bound.)
* **`topo_xallocx` validates its arena flag.** The in-place-resize query decoded
  the flag word but skipped the existence check its siblings
  (`mallocx`/`rallocx`/`nallocx`) perform, so `TOPO_ARENA(uncreated_id)` returned
  a usable size instead of the documented `0 + EINVAL`. It now rejects a
  nonexistent/inactive arena like the rest — existence only, since `xallocx`
  resizes in place and never allocates *from* the flag arena, so its `ALLOC`
  authority is not consulted (as with `rallocx`, which preserves the original's
  arena across a move).
* **Normal span retirement revokes before cross-arena recycle.** An empty span's
  backing leaves the arena-tagged empty-span cache (DD-4, capacity 1/bin) for the
  single shared `span_extents` pool, from which a later `create_span` may serve a
  *different* arena — so the owning arena's capability descendants must be revoked
  first, exactly as the reset/destroy drain does. Retirement previously recycled
  without revoking (a stale comment claimed a per-arena pool that does not exist);
  it now reuses the same `free_revoking` discipline on **every** retirement, so
  `reclaim_span_slot` revokes unconditionally. On POSIX the revoke is the ambient
  no-op; a counting test provider observes it firing on the normal path. (A
  per-arena pool that would make same-domain reuse revoke-free is a plan-04
  performance concern, M3+; "safety before policy" ships the correct revoke now.)
* **The flag-routed front door reports `EACCES` for a no-`ALLOC` arena.** An
  active arena whose cap lacks `ALLOC` passed `topo_mallocx`/`topo_nallocx`'s
  existence-only gate, and the engine's bare-null rejection then surfaced as
  `ENOMEM` (with `nallocx` predicting a nonzero size) — inconsistent with the
  handle path, which maps the authority denial to `EACCES`. The front door now
  consults the arena's rights: `EACCES` for the denial, `EINVAL` for "no such
  arena", matching `topo_mallocx_arena` (§36.4).
* **Delegation reserves the child's quota on the parent (P1; budget-partition
  pulled forward from M7).** The first two passes used the *ceiling* model: a
  child's quota was checked `≤ parent.remaining` at delegation but never reserved,
  so a finite parent that delegated a finite child let **both** allocate the full
  quota — Σ descendants could reach 2× the parent's authority. Delegation now
  **reserves** the child's quota on the parent: a per-arena `committed` counter
  (own bytes + reserved child quota) is the single atomic the allocation gate and
  the reservation both compare-exchange against, so they can never jointly exceed
  the quota even under concurrency; `reserved` is tracked separately so the
  §36.17 `used` a caller sees stays own-live and `Σ used == live_bytes` still
  reconciles (§8.6). Reservation bites only a *finite* parent (an unlimited parent
  partitions nothing, and a finite parent cannot delegate an unlimited child). A
  child's destroy returns its reservation to the parent — **generation-checked**,
  so a destroyed/recycled parent slot is never miscredited; reset keeps the child
  alive, so its reservation stays. The Lean bridge gains `ArenaTree` +
  `subtree_used_le_quota` (`SeLe4n/CapBackedArena.lean`): under the reservation
  discipline the **whole subtree's** own-used bytes are bounded by the root's
  quota — the tree-wide monotonicity the per-edge `DelegatesFrom.quota` alone
  cannot give. New unit + property tests (`delegation_reserves_parent_quota_…`,
  `delegated_subtree_never_exceeds_parent_quota`) and the loom quota model (the
  reservation commits through the same gate CAS) pin it.

## W10 — extent hooks & custom backing over the provider seam (plan 06)

Extent hooks (§23) let an application supply a **custom memory source** or
**custom OS policies**. The design decision is where they attach, and the answer
follows from the existing architecture rather than adding a parallel mechanism:

* **Hooks *are* a backing provider.** Every OS/kernel interaction already flows
  through the `TopoBackingProvider` seam (§3/§36.6) — that seam *is* the
  custom-backing abstraction (overview §3, D2). So the §23.2 interface is realized
  as `topo_core::hooks::ExtentHooks` (the eight ops in Rust idiom), adapted to the
  seam by `HookProvider<H>`. An `ExtentManager`/`Allocator` built over a
  `HookProvider` runs the whole proven central path on the user backing **with no
  change above the seam** — the exact reading of "wired through the provider seam
  (plan 04)". This reuses the seam's fallibility-and-well-formedness contract
  (§36.6 / W4-5) instead of re-deriving it. The six physical ops map 1:1
  (`alloc→reserve`, `dealloc→release`, `commit`/`decommit`/`purge_lazy`/
  `purge_forced`), and a hook failure is the `BackendError` the manager already
  recovers from.

* **`split`/`merge` are advisory seam notifications, not gates.** They are the two
  §23.2 ops with no §20.4 physical analogue. In TopoMalloc's architecture the
  `ExtentMap` is the **source of truth** for sub-extent geometry: the manager
  reserves one region and subdivides it internally, so the provider/backing sees
  only offset ranges, never extent objects. `split`/`merge` were therefore added to
  `TopoBackingProvider` as **default-Ok notification** methods (joining the
  `decommit`/`purge`/`revoke_descendants` family of default seam ops, so POSIX and
  seLe4n are unchanged), dispatched from the manager's carve/coalesce through a new
  `ExtentNotify` sink (`ProviderNotify` adapter; default `NoNotify` is a ZST, so the
  pre-W10 path is byte-for-byte identical). A hook failure here is **recorded**
  (`HookProvider::split_hook_failures`/`merge_hook_failures`) but never alters the
  bookkeeping — §23.3 "hook failures are reported without corrupting allocator
  state", and §23.4 "allocator correctness assumes hook correctness". This is the
  only sound choice given an authoritative `ExtentMap`: a notification cannot be a
  veto without making the allocator's own metadata subordinate to an unverified
  hook. Threading was done **additively** (`carve`/`split`/`merge`/`coalesce`/`free`
  keep their signatures; new `*_in` variants take the notifier), so every existing
  caller, test, and fuzz target is untouched and the proven `ExtentMap` logic is
  unchanged save for the post-success notify call.

* **§23.3 contracts: enforce the load-bearing half, assume the rest (§2.4).**
  `HookProvider` validates the cheap, allocation-free output contracts on every
  call — an `alloc` result must be non-null, aligned to the request, and at least
  the requested size; a commit/decommit/purge target must be a sub-range of the
  reservation — and **rejects** a violation with `InvalidRequest` (so even a buggy
  hook cannot make the allocator hand out memory that fails the request) *and*
  debug-aborts to surface the hook bug. The stateful contracts (no-overlap with
  live ranges, dealloc pairs a live reservation) are self-checked by the test/fuzz
  backings. Reentrancy (§23.3 "no recursive TopoMalloc calls") is the inherited
  seam contract: hooks run under the held back-end extent lock (as the existing
  `commit`/`decommit` provider calls do), so a re-entrant allocator call deadlocks
  on that non-re-entrant lock — the documented, enforced boundary; explicit
  recursion detection is a hardened-profile concern (plan 08).

* **§23.4 modeled in Lean.** `lean/TopoMalloc/ExtentHooks.lean` states the §23.3
  contracts as hypotheses and proves the operations preserve the well-formedness
  core *under* them: a contract-honouring `alloc` keeps the allocator's ranges
  pairwise disjoint; `split`/`merge` keep the region tiled and preserve
  disjointness from every other extent; a sub-range op touches no other extent. Each
  theorem consumes the contract and concludes well-formedness — literally "allocator
  correctness assumes hook correctness" — and rests only on the standard axioms
  (`propext`/`Quot.sound`/`Classical.choice`), with a non-vacuity witness so nothing
  is proved vacuously. The Rust `HookProvider` discharges the cheap half of the
  premise at runtime, so only the genuinely unverifiable backing behaviour is
  assumed.

* **W10-3 failure injection.** Every fallible runtime hook can fail and the back-end
  stays well-formed: a `commit` failure rolls the carve back (W4-5), a `decommit`
  failure (unmap policy) retains the extent, `split`/`merge` failures are advisory.
  A proptest (`tests/tests/extent_hooks.rs`) and the `extent_hooks` fuzz target
  assert `check_invariants` after every step under randomized per-hook failures.

### W10 optimal pass (per-arena hooked regions + C ABI + hardening)

A deliberate completeness pass closed every gap the first pass deferred:

* **§23.3 stateful enforcement moved into `HookProvider` (not just test
  backings).** A fixed-capacity, lock-guarded reservation set (`ReservationSet`,
  allocation-free, cap 16 — a provider backs one manager ⇒ ≤ 1 live reservation)
  detects an `alloc` result that overlaps a live reservation, and a `dealloc` of a
  region never handed out — rejected *and* debug-aborted. A per-provider
  reentrancy guard (`in_hook` flag) catches a hook that re-enters an op on the same
  provider (refuse with `Unsupported` + debug-abort) instead of deadlocking on the
  held back-end lock — clean detection, the documented enforcement of §23.3's "no
  recursive TopoMalloc calls" for the common same-provider case (cross-provider
  re-entry still hits the lock).
* **Per-arena hooked regions — the full §22.2/§22.4 `hooks` descriptor field.**
  This is the re-architecture the first pass deferred (and the user later
  requested). An arena created with `Allocator::arena_create_hooked` serves its
  span **and** large allocations from its own `HookProvider`-backed region,
  **isolated** from every other arena's region by construction (§22.7 — proven in
  Lean by `perArena_disjoint_regions_isolate`: disjoint regions ⇒ disjoint
  allocations). The design keeps the proven shared path byte-identical:
  - The span/large paths route the *extent source* through new type-erased seams
    `ExtentBacking` / `LargeBacking` (impl'd by `ExtentManager<P>` /
    `LargeAllocator<P>`), so the call sites are not generic over the provider; a
    fixed-capacity `HookRegistry` (`MAX_HOOK_BACKENDS`) holds the per-arena
    backends, and a **lock-free `count == 0` fast path** means a program with no
    hooked arenas pays nothing. Hooks are **borrowed** (`&'a dyn ExtentHooks`), not
    boxed, so the core stays allocation-free (no re-entrant `Box::new` through the
    global allocator); a hooked arena's hooks outlive the allocator like the
    metadata/pagemap do.
  - Routing: span create/retire route by `span.arena()`; large alloc by arena;
    large **free / `usable_size` / `realloc` find the owner** (the one backend whose
    descriptor pool resolves the pointer — the shared pagemap is global, so the
    descriptor is found by trying the shared backend then each hooked one; a hooked
    arena's descriptor is *not* in the shared pool, so every pool-querying op must
    route, not just `free`); drain routes by arena. `stats()` and `check_invariants`
    **aggregate** every backend, so the live-large count and the §20.1 physical-state
    breakdown cover the hooked regions too.
  - Lifecycle (§22.4 order): the hooked backing is reserved + **registered before**
    the arena id is published `Active` (the id is private to the create call until
    then, so the window is race-free). `arena.rs` gains
    `create_pending`/`publish`/`abandon_pending` for that split. Destroy tears the
    region down (returns it to the hooks via `dealloc`, *outside* the registry lock
    so the hook never runs under it) **before** the terminal step, so a backing that
    refuses the return quarantines rather than reporting a clean destroy (the
    strict-teardown hardening below); reset keeps it.
  - Concurrency (soundness): the registry is accessed **per element via raw
    pointers** — never a whole-array `&[Option; N]` / `&mut [Option; N]` — the same
    slot-pool discipline `ExtentMap`/`SpanPool`/`ArenaTable` use. A reference into
    one slot is then disjoint from a concurrent destroy clearing *another* slot
    (which only forms a narrow `&mut` to its own element), so a worker holding a
    backend reference for arena X is never invalidated by a destroy of arena Y — no
    whole-array `&mut` to over-assert. Slots are cleared in place (never moved), so
    under the §22.5/§36.13 quiescence contract (an arena's create/destroy does not
    race its own alloc/free) a backend reference is stable for the op. A concurrent
    stress test (workers hammer one hooked arena while another thread create/destroys
    others) exercises exactly this.
* **The C `topo_extent_hooks_t` ABI (§23.2's C-struct surface).** `topo-abi`
  exposes the vtable + `topo_arena_create_hooked(hooks, ctx, span_bytes,
  large_bytes)` + `topo_max_hook_backends()`; a `CHooks` adapter maps the C
  function pointers (jemalloc bool convention: `true` ⇒ failure; NULL op ⇒ the
  no-op default) to the Rust trait. The vtable is copied (the caller's struct need
  not persist); the adapter is heap-owned with a `'static` borrow handed to the
  allocator, tracked in `CHOOKS_REGISTRY` and **reclaimed on destroy** (freed on a
  failed create) so a create/destroy loop stays bounded — see the PR-review
  hardening below (it is *not* leaked, despite an earlier pass that left it so).
  `include/topomalloc.h` declares it; the C and C++ ABI harnesses drive a real
  custom backing end to end; the `nm` header↔symbol cross-check balances at 30.
  (A hook backing must honour the requested `PAGE_SIZE` alignment — the §23.3 guard
  correctly rejects a plain-`malloc` backing, so the harnesses use `aligned_alloc`.)
* **Deeper Lean + a 7th gate.** `ExtentHooks.lean` now connects the §23.4 contract
  to the **real** `WfRangesDisjoint` clause of the abstract `State`
  (`allocContract_preserves_rangesDisjoint`), not just `Range` geometry; decidable
  mirrors of the Rust `HookProvider` checks (`alignedOk`/`atLeastOk`/`subrangeOk`)
  are tied to the contract props and gated by a new `lake exe check`
  `hookContractGate`, so the model and the runtime enforcement cannot drift.
* **Stronger tests.** The **full** central-path allocator (malloc/free/realloc, not
  just the extent manager) is fuzzed under injected hook failures asserting the
  §8.6 identity + non-aliasing; a hook-vs-POSIX behavioural-equivalence test; and
  per-arena routing/isolation/lifecycle/registry-full integration tests.

* **Audit pass (two real fixes).** A deliberate deep audit of the per-arena work
  found and fixed: **(a)** a soundness hazard — the registry originally formed
  whole-array `&`/`&mut` references and returned an element reference held without
  the lock, which a concurrent destroy of a *different* arena could invalidate
  under Stacked/Tree Borrows (latent UB despite disjoint writes); rewritten to
  per-element raw access. **(b)** a correctness bug — `realloc`/`usable_size` of a
  hooked-arena *large* object queried the shared large pool (which cannot resolve a
  hooked descriptor) and so spuriously returned NULL/None; routed through the owner
  like `free`. Both now have regression tests (a concurrent registry stress test
  and a hooked-arena large realloc test). The audit also closed the stats gap above
  (aggregate `live_large` + backend breakdown over the hooked regions).

* **PR-review hardening (PR #14, four fixes).** A code review of the per-arena +
  C-ABI work surfaced four issues, each fixed with a regression test:
  - **Adapter lifetime (P2).** The C `CHooks` adapter was leaked on every create.
    Now each adapter's box address is tracked in `CHOOKS_REGISTRY` keyed by arena
    id and **reclaimed** on `topo_arena_destroy` (and freed on a failed create), so
    a create/destroy loop no longer grows the heap. The reclaim runs only on a clean
    `Ok` destroy — by then the arena's backend (and its `HookProvider<&CHooks>`
    borrow) is gone, so the free is sound; a quarantined destroy keeps the adapter
    (a bounded, terminal-failure retention, never a use-after-free).
  - **Cross-region no-overlap (P2).** A hooked arena reserves its span and large
    regions through **two** `HookProvider` instances, so neither tracker sees the
    other's range. A buggy hook returning overlapping span/large regions would let
    small spans and large allocations alias. `arena_create_hooked` now checks the
    two reserved regions are disjoint at construction — debug-abort, release-reject
    (`ArenaError::Exhausted`), with both built managers handing their regions back.
  - **Reject-path hand-back (P2).** When a hook's `alloc` result fails the §23.3
    geometry check, `HookProvider::reserve` now returns the range to the hook
    (`dealloc`) before failing, so a rejected reservation never leaks the backing.
    (The rejected range was never recorded as live, so the hand-back dispatches the
    hook's `dealloc` directly, not `release` — which would trip the pairing check.)
  - **Strict fallible teardown (P2, §36.13).** Previously a backing `dealloc`
    failure during destroy was swallowed by the infallible `ExtentManager::drop`.
    Now `ExtentManager`/`LargeAllocator` expose an explicit fallible `teardown()`
    (release exactly once across it and `Drop`, via a `released` flag), and
    `arena_destroy` returns the hooked region to the hooks **before** `finish_destroy`
    — a refusal routes through the existing `Draining → ErrorQuarantined` edge
    (returns `Err`, arena quarantined, never a clean `Destroyed`), exactly the
    capability-revoke partial-failure shape. The failure is also counted for
    observability (`HookProvider::release_hook_failures`, mirroring the
    `split`/`merge` counters). No new abstract transition: the trigger rides the
    already-proven quarantine edge, pinned by the named Lean obligation
    `ArenaLifecycle.destroy_backing_release_failure_quarantines` (the Rust↔Lean
    `state_machine_is_exactly_the_spec_graph` differential stays green).

### W10 optimal-completion pass (routing, scale, observability)

A final pass closed the remaining big-O / observability gaps a self-audit surfaced.

* **O(1) per-arena routing (was an O(MAX_HOOK_BACKENDS) scan).** Each arena now
  records its hooked-backing **registry slot** in a lock-free `AtomicU8` on its
  `ArenaTable` entry (`hook_slot`, `0` = none, `k` = registry slot `k − 1`). Routing
  reads it directly instead of scanning the registry:
  - `hook_backend(arena)` keeps the zero-overhead `count == 0` fast path (no hooked
    arena ⇒ one atomic, no table/registry touch), then on a hit reads `hook_slot` and
    indexes the one slot under the registry lock (a defensive `b.arena == arena`
    identity check fails closed).
  - large `free`/`realloc`/`usable_size` route by the **descriptor's** arena —
    `FreeTarget::Large { desc }` / `PointerClass::Large { desc }` carry
    `*const LargeDescriptor`, whose `arena()` names the owner in O(1); the per-arena
    pool *is* where its descriptors live, so no descriptor-pool search remains.
  Memory ordering: `set_hook_slot` is program-ordered **before** the registry `count`
  release at registration, so a reader that observes `count ≥ 1` (Acquire) also
  observes the slot. Slot stability is the same §22.5/§36.13 quiescence argument as
  before (an arena's own create/destroy never races its own op; a *different* arena's
  destroy clears only its own slot in place). The change deleted the
  `LargeBacking::arena_of` scan callers.
* **Scale: `MAX_HOOK_BACKENDS` 8 → 32, footprint shrunk.** The registry is a fixed
  inline array built **on the stack** at construction (the allocator is created by
  value before being boxed/leaked), so the cap is **stack-bounded** — raising it to 64
  overflowed the dual-backend (`AnyAllocator`/G-sim) init. To raise it safely,
  `RESERVATION_CAP` dropped 16 → 4 (a provider holds ≤ 1 live reservation, so 16 was
  4× over-provisioned), shrinking each backend ~1 KiB → ~0.6 KiB; a `const _`
  `assert!` then caps the inline array at a **48 KiB stack-safe budget** so neither a
  raised cap nor a future field on `ArenaHookBackend` can overflow init or silently
  bloat every allocator. The `u8` slot index bounds it at 255; the budget bites first.
  (A genuinely large population would need the registry moved out of line into the
  metadata arena — a future refactor, not the inline array.)
* **Adapter reclaim made precise (reference-based, not success-code-based).** The C
  `topo_arena_destroy` now reclaims the `CHooks` adapter iff
  `!arena_has_hook_backend(id)` — i.e. exactly when the allocator no longer holds the
  backend (and thus the adapter borrow). That covers a clean destroy **and** a
  *teardown-failure* quarantine (the backend was dropped during the failed teardown),
  closing the small adapter leak the earlier "Ok-only" rule left there; a
  *drain-failure* quarantine keeps the backend (borrow alive) so the adapter is
  correctly retained. `Allocator::arena_has_hook_backend` is the `hook_slot != 0`
  read. (No use-after-free: teardown drops the backend synchronously before
  `arena_destroy` returns, so "no backend" ⇒ "no borrow".)
* **Observability: hook failures are now reachable.** The self-audit's headline gap
  was that the `HookProvider` failure counters were unreadable for a *hooked arena*
  (the provider is internal). Fixed at both granularities, mirroring
  `numa_bind_failures`:
  - `HookProvider` gained a `commit` (commit/decommit/purge) counter alongside the
    existing release/split/merge. Only **swallowed** failures are counted — those the
    allocator handles internally (and so are otherwise invisible). A `reserve` failure
    is deliberately *not* counted: it is **returned** to the caller (the alloc /
    arena-create fails visibly) and drops the backing, so a counter would be redundant
    *and* structurally always 0 in the aggregated view (a live/retired hooked arena
    reserved successfully). The reject-path hand-back `dealloc` failure (a backing
    refusing its *own* bad result back) counts as a **`release`** failure.
  - **Per-arena:** `ArenaStats::hooks: Option<HookFailureStats>` — aggregated over the
    arena's **two** providers (span + large) by `Allocator::arena_stats`; `None` for a
    non-hooked arena. Read **under the registry lock** (not via `hook_backend`, which
    releases it): `arena_stats` is introspection and *can* race `arena_destroy`, so it
    must block the teardown from dropping the backing mid-read — the alloc/free routing
    is instead protected by the §22.5 quiescence contract.
  - **Global:** `AllocatorStats::hook_failures` and the stats-JSON
    `arenas.hook_failures` object (additive, §35.3) — the operator-facing surface that
    reaches C. It is a **cumulative** total: a persistent `AtomicHookFailures` on the
    allocator folds a backend's counts in **before it drops** at teardown (so a
    `release` failure, which fires *during* teardown, survives the destroy), plus
    every live backend's current counts.

#### Deliberate constraints (documented, not "fixed")

These were weighed and left as the right design for a safety-first `no_std` core.

* **Cross-provider reentrancy is bounded by the lock, not detected.** The `in_hook`
  flag cleanly catches a hook re-entering an op on the **same** provider. A hook that
  re-enters via a *different* provider is bounded by the non-re-entrant back-end lock
  (it deadlocks rather than corrupting state — still safe). Full recursion detection
  needs a per-thread guard; `topo-core` is `no_std` and has **no thread-local**, and a
  global flag would false-positive across threads. Clean full detection is therefore a
  hardened/`std`-profile concern, already scheduled for plan 08 W18 — adding a `std`
  feature to the core here would duplicate it and breach the `no_std` discipline.
* **The teardown→quarantine link is modeled at the phase level, by design.** The Lean
  obligation `destroy_backing_release_failure_quarantines` states the
  `Draining → ErrorQuarantined` edge; the runtime fact that
  `teardown_hook_backend` returning `Err` *drives* it sits **below** the
  backing-provider abstraction (trust boundary #2, §36.6). Refining it into the
  abstract `arenaDestroy` transition would mean modeling the provider in Lean, which
  the trust boundary deliberately abstracts. The Rust behavior is pinned by a test.
* **A drain-failure quarantine retains its registry slot — necessarily.** A failed
  capability *revoke* (seLe4n; POSIX revoke is the ambient no-op) means the region's
  descendants are still live, so the region **must not** be returned to the backing
  (§36.6/§36.13 revoke-before-recycle). The backend therefore stays registered
  (slot held). This is correct, not a leak to "reclaim"; quarantine is terminal by
  spec, and a recovery/reaping mechanism for quarantined arenas is a separate feature.
* **The `alloc` `zero`/`commit` out-flags are not consumed.** `HookProvider::reserve`
  commits before use (M-005) and zeroes via the §26.3 span zeroed-flag, so a backing
  that pre-commits/pre-zeroes optimises through its own cheap `commit` hook, not the
  reservation flags — a deliberate layering choice that keeps the reservation path
  uniform across POSIX/seLe4n/hooks.
* **`RESERVATION_CAP = 4` degrades to best-effort past 4 live ranges** — which cannot
  happen (a provider backs one manager ⇒ ≤ 1 live reservation); past it the
  no-overlap/pairing checks never *false*-alarm, they only stop tracking.

Verified by per-arena/global stats tests (`per_arena_hook_failures_surface_in_stats`,
the topo-stats JSON test), the existing concurrent registry stress test (now over the
O(1) path), the strict-teardown + reclaim tests, and the §34.8 extent-stream fuzz
(which covers the state-corruption-risk hooks — commit/decommit/split/merge). A
dedicated arena-lifecycle fuzz target is the one deferred item (the per-arena path is
covered deterministically and concurrently; a nightly fuzz target could not be
compiled-verified in the work environment).

## W13 — topology awareness & live NUMA placement (plan 04)

These decisions close the live-integration gaps of the §15 topology workstream.

* **NUMA node ids are dense + internal, with a preserved OS-id map.** Discovery
  densely renumbers the OS node ids actually in use to `0..node_count()` (as it already
  does for LLC), so a sparsely-numbered platform (OS nodes 0 and 2 present, 1 absent)
  yields **two** nodes, not a three-node model with a phantom node 1 — keeping
  `node_count()`/stats exact and every dense id `< node_count()` a real node (the
  rebalancer, interleave, and the per-node router can iterate `0..node_count()` with no
  holes). Because `mbind`/`set_mempolicy` need the *kernel's* node number, the raw OS id
  is kept in `node_os_id` and recovered by `Topology::os_node_of`. On a dense platform
  the renumbering is the identity, so the common case is unchanged. The alternative
  (raw OS ids + a "present" mask) was rejected: it pushes present-awareness into every
  consumer, whereas dense ids give the clean invariant "all ids `< node_count()` are
  real".
* **The rebalancer donates only a node's *surplus*, never its raw free.** A move is
  sized and gated by `movable_surplus = free − own demand` (not raw free), so it can
  never strand the donor it draws from, and a round with no surplus anywhere plans
  nothing rather than churning memory between equally-starved nodes. Proven to converge
  in `≤ nodes` moves (the need/surplus node-potential strictly decreases each move).
* **Live placement is a per-node-backend *router*, not a NUMA-aware filler.** The
  formally-modeled, fuzzed `HugePageFiller` is left **untouched**; instead a `NodeRouter`
  (`RegionCacheHook`) holds one `HugePageBackend` per node in a fixed `[…; MAX_NODES]`
  array (no `Vec` — the core is `no_std` without `alloc`) and routes the large path to
  the preferred node's backend. Installed via the existing
  `new_with_huge(&dyn RegionCacheHook)` seam, so the engine change is one resolved
  `Hints.numa` field and the **default path stays byte-for-byte unchanged**. A
  single-node machine builds exactly one backend — identical to today — so the
  integration degrades cleanly where there is nothing to place.
* **The router computes the node from `Hints.numa`; the topology lives in the router.**
  The engine resolves the arena's `NumaPolicy` into a `Hints.numa` field and the router
  (holding the swappable `Topology`, a `CoreProvider`, and an atomic interleave counter)
  computes `preferred_node_at` and routes. This keeps the placement state in one
  cohesive, host-refreshable component (`refresh` swaps the snapshot under the router's
  lock — host-driven, matching the release pump's model, no background thread) rather
  than threading current-CPU + interleave + topology through the engine.
* **`mbind` is best-effort; a bind failure is recorded, never fatal (§15.5).** Each
  backend's region is bound to its node's OS id at construction via a new defaulted
  `TopoBackingProvider::bind_node` (no-op on seLe4n / non-Linux, real `set_mempolicy`/
  `mbind` on Linux). A failure increments `numa_bind_failures` (now a *driven*, non-vacuous
  counter) and placement proceeds — "safety before policy": a missed bind hurts locality,
  never correctness.
* **Per-node demand is approximated from per-node allocation failures.** The allocator
  has no first-class per-node *demand* signal, so the rebalancer's live driver derives it
  from each node-backend's recent alloc-failure count (free comes from each backend's
  empty-backed coverage). This is an explicit approximation — documented as such — that
  is honest enough to drive the §15.4 ladder without inventing per-node demand
  accounting (a real demand model is M5). An `Avoid` (`NO_HUGEPAGE`) decline is **not**
  counted as demand — it is short-circuited at the router before any node is chosen, so a
  policy decline never fabricates a rebalancer signal.
* **The arena's NUMA policy is read lock-free on the alloc path.** Resolving the arena's
  policy into the request's hints sits on the large/medium allocation path, and
  `try_charge` is deliberately lock-free (an atomic CAS, no table lock). So the policy
  lives in a per-arena `AtomicU64` (encoded, like `hook_slot`), read with a single
  `Relaxed` load — *not* under the table lock — keeping the alloc path lock-free. It is
  purely a policy value (a torn/garbage read decodes to the safe `OsDefault`, never a
  capability or quota), so a relaxed atomic with no lock is sound; it survives a reset
  (placement config is sticky) and is the single source of truth (`stats` decodes the same
  atomic, so the two can never disagree).

Verified by the topology unit tests (dense renumbering, the local-for-every-slot distance
diagonal, `os_node_of` round-trip, `preferred_node_at` equivalence, the move helper), the
router unit tests (routing-by-policy, the avoid-decline-does-not-fabricate-demand
regression, deterministic bind-failure counting, refresh, interleave, live rebalance), the
arena unit tests (the lock-free `numa` encode/decode round-trip + survives-reset), the
cross-crate integration tests (discovery → stats → control, the multi-node router placement
+ mbind-failure path, the host-driven refresh cycle, the live rebalancer drive-to-fixpoint),
the `rebalancer_never_strands_a_donor_and_converges` gating proptest, and the new
`fuzz/fuzz_targets/topology.rs` target (builder totality + rebalancer convergence over
arbitrary inputs).

### W13 optimal-completion pass (first-touch, real CPU, control surface)

A follow-up pass closed the live-behavior gaps a self-audit surfaced.

* **`OsDefault` is genuine first-touch (an unbound default backend), not node 0.** The
  earlier router resolved `OsDefault` to node 0 and served it from a bound backend,
  pinning the *common* case to node 0 + `mbind` on a multi-node box and defeating the
  kernel's first-touch policy (place the page on the node of the touching thread — usually
  optimal). The router now holds an **unbound** default backend (no `mbind`) serving
  `OsDefault`/`ArenaPolicy`, so the OS places those pages first-touch; the per-node *bound*
  backends serve only explicit `Local`/`Bind`/`Interleave`. On a single node there is no
  separate default (binding the only node is a no-op, so the lone backend *is* first-touch),
  so the single-node path is byte-for-byte unchanged. The `make` factory takes
  `Option<NodeId>` (`Some` = bound per-node, `None` = unbound default); the ABI sizes the
  default at full capacity (it serves the common case) and the per-node set at `capacity/n`.
  Rejected alternatives: routing `OsDefault` to the current node (`≈ Local` — simpler but
  binds to the *allocator's* node, wrong for producer/consumer); per-allocation `mbind`
  (would drop the per-node backends the rebalancer samples).
* **`Local` uses the real running CPU (`sched_getcpu`), not a fixed core.** A new `OsCore`
  `CoreProvider` (`topo-backend-posix`, Linux `sched_getcpu`, core 0 elsewhere) replaces
  `FixedCore(0)` in the live ABI, so `Local` tracks the actual thread. The seam stays
  injectable, so the RSEQ per-CPU identity (plan 05 W7) is still a drop-in.
* **The filler's NUMA score term was removed as dead code.** With placement done by
  *routing* to per-node backends (each one node), the per-candidate `locality_bonus`/
  `cross_numa_penalty` never discriminated anything on the live path — so `PlaceHints`/
  `HugeConfig`/`HugePageFiller` lost their `home_node` and the score lost its NUMA terms.
* **Spillover prefers the nearest node; `Interleave` rotates over the backend count.** A
  full preferred node spills to the nearest other node by `Topology::distance` (then the
  default backend), not index order (§15.4); and `Interleave` round-robins over the actual
  backend count, so a node-count-changing refresh can never bias it.
* **The rebalancer/refresh/idle-release are reachable on the deployed C ABI.** A type-erased
  `RouterControl` trait + a global handle let the C `topomalloc_numa_rebalance_tick` /
  `_refresh` / `_release` / `_nodes` / `_bind_failures` / `_rebalance_moves` / `_spillovers`
  entry points drive the live router (host-driven, no background thread — matching the
  chosen model). Each is a graceful no-op when no router is wired. `release_idle` is the
  W12 idle-memory handoff driven router-wide.

Verified by the expanded router unit tests (first-touch default, single-node default,
nearest-node spillover, `MAX_NODES` scale), the `numa_api` control-surface test (both feature
configs), the multi-node integration tests over the real provider (first-touch + W12 release,
and a concurrent alloc/free/rebalance/refresh stress test), and the W8-8 header↔symbol
cross-check over the seven new C symbols.

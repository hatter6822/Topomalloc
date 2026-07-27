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
  unit-tested against synthetic tables with awkward over-aligned distributions the
  shipped table does not contain — W15-4 later gave each shipped class its natural
  alignment, see below) and **proved** in Lean: `alignWalk_sufficient`
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

* **NUMA node *and* LLC ids are dense + internal, with a preserved OS-id map for nodes.**
  `TopologyBuilder::build` densely renumbers the OS node ids — and, by the identical
  construction, the LLC-domain ids — actually in use to `0..node_count()` / `0..llc_count()`,
  so a sparsely-numbered platform (OS nodes 0 and 2 present, 1 absent) yields **two** nodes,
  not a three-node model with a phantom node 1 — keeping `node_count()`/`llc_count()`/stats
  exact and every dense id a real domain (the rebalancer, interleave, and the per-node router
  can iterate `0..node_count()` with no holes). Because `mbind`/`set_mempolicy` need the
  *kernel's* node number, the raw OS *node* id is kept in `node_os_id` and recovered by
  `Topology::os_node_of`; LLC ids are internal only, so no raw-id map is kept for them.
  Doing **both** renumberings inside `build` (not only in the sysfs reader) hardens every
  direct builder caller, not just the discovery path — the documented "no phantom domain"
  invariant then holds uniformly (defense in depth). On a dense platform the renumbering is
  the identity, so the common case is unchanged. The alternative (raw OS ids + a "present"
  mask) was rejected: it pushes present-awareness into every consumer, whereas dense ids give
  the clean invariant "all ids `< node_count()` / `< llc_count()` are real".
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

Verified by the topology unit tests (dense **node and LLC** renumbering with the no-phantom
guarantee, the local-for-every-slot distance diagonal, `os_node_of` round-trip,
`preferred_node_at` equivalence, the move helper), the
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

## W14 — lifetime/hotness placement policy & heap sampling (plan 07)

These decisions close the §24 placement workstream, with the minimal §31.4 sampling slice that
feeds it from real traffic.

* **The learning policy is a pure, `no_std`, host-driven object — the `ReleaseController` (W12)
  pattern — not engine-owned state.** `SiteProfileTable` (`crates/topo-core/src/placement.rs`)
  holds the §24.4 `AllocationSiteProfile`s and answers `place_hints(stack_id)`; it reads no clock
  and makes no provider calls (timestamps are inputs). This keeps the core formally tractable and
  testable in isolation, and lets the *learned-profile machinery evolve behind a fixed safety
  wall* exactly as the plan's deep-dive prescribes. The sampler (W17-3) is the input feed; the
  policy itself is independently complete and correct.
* **The §24.5 safety boundary holds by construction, not by audit.** The policy's *only* output
  type is the advisory `PlaceHints { hotness, lifetime }` — the same score-only input the W11
  filler already documents as "a wrong hint can hurt fragmentation but never misplace a live
  object" (a run is carved from the free bitmap regardless of score). There is therefore **no
  code path** from a profile to a size/alignment/validity/free decision. The
  `engine_size_align_validity_free_are_invariant_under_hints` fixed-wall test (plus the
  pure-filler, proptest, and `placement` fuzz companions) sweeps every hint combination —
  including deliberately *wrong* learned ones — and asserts identical usable size, alignment,
  writability, and free path. Placement is policy, not a modeled transition (§2.4, as for W13),
  so there is **no Lean obligation and no trace-grammar change**.
* **§24.2 lifetime classes are a distinct, richer taxonomy from the §10.4 user hint.** The user
  flag (`Lifetime`: Unspecified/Short/Medium/Long) is a coarse *request* hint; the *inferred*
  `LifetimeClass` (Unknown/Ephemeral/Short/Medium/Long/Persistent) is learned from measured ages
  and projected back onto the coarse hint for the filler. Keeping them separate avoids
  conflating "what the caller said" with "what we observed", and the projection
  (`to_hint`) is the single, total bridge.
* **Bounded, allocation-free state everywhere.** The size-class distribution is a small
  Space-Saving summary (top-`K` with overcount bounds — the principled bounded frequent-items
  sketch, since a site allocates few distinct sizes); the profile table is fixed-capacity open
  addressing with bounded-probe least-confidence eviction; the live sampled-object set is
  fixed-capacity open addressing with backward-shift deletion (no tombstones) and drops past a
  7/8 load rather than evicting a live record. Nothing grows or allocates, so the whole
  subsystem is `no_std`-clean and the sampled path cannot recurse into the allocator via a
  growth.
* **Confidence is two-tier but reported as one number.** Hotness confidence scales with alloc
  samples (a mixed-hotness site self-corrects: its mean drifts to neutral ⇒ no grouping
  pressure); lifetime confidence is free-sample maturity × histogram concentration (so we never
  act on a lifetime we have not *seen die*). `place_hints` gates each dimension on its own
  confidence; the §24.4 `confidence` field is the conservative `min` of the two. A site with
  only allocations (no observed frees) is never "confident" about lifetime — the right
  conservative default (§24.6 "when the allocator has confidence").
* **Sampling is off by default and lock-free on the hot path (§31.4).** The decision is a
  per-thread Poisson `Sampler` (a fixed-point exponential inter-sample interval, so the core
  stays floating-point-free, §6) touching only thread-local state. The **free** path's
  "is this sampled?" test is a lock-free atomic `SampleBloom` with **no false negatives**, so the
  common non-sampled free never takes the sampled-set lock; only a (rare) maybe-positive does
  (DD-1 F2). When disabled, every hook is a single relaxed atomic load — the default artifact's
  path is unchanged.
* **Stack capture is `libc::backtrace` into a fixed buffer, warmed up at enable.** The §31.4
  / Appendix-F trap is an unwinder that allocates and re-enters the allocator from inside an
  allocation. We capture return addresses straight into a fixed `StackBuf` (no growth), and run
  the one-time glibc unwinder `dlopen` at *enable* time (outside any sampled allocation) so the
  capture itself never allocates. A thread-local re-entrancy guard makes the whole sampled slow
  path non-re-entrant; the `sampling_lifecycle_*` test drives 20k sampled allocations and proves
  no deadlock/recursion. On non-glibc targets capture degrades to "un-attributed" rather than
  failing to build.
* **The global sampled state is a lazily-`Box`ed `Mutex`, initialized at enable under the
  bootstrap guard.** An all-zero `static` would force the `min_confidence_bp != 0` table into
  `.data` (binary bloat); a stack-built `static` const is large. Lazy `Box` init at enable
  (off the hot path, and under the existing `BOOTSTRAPPING` guard when set from
  `$TOPOMALLOC_SAMPLE_RATE` at startup) costs the default build nothing and keeps the slow path
  allocation-free thereafter (`STATE.get()` is a plain load once armed).
* **`realloc` is sampled as retire-old + create-new.** The hook frees the old object's record
  before the resize and samples the result as a fresh allocation; an in-place resize is simply
  re-sampled. A failed resize leaves the old object un-tracked — an accepted, bounded profiling
  inaccuracy, never a correctness issue.
* **Observability rides the host-composes-stats seam.** `placement_stats()` is the accessor the
  host folds into `topo_stats::Stats::record_placement` (exactly like `record_release` /
  `record_node_router`); the `placement` JSON block and the `topo.placement.*` control keys read
  from it. These are *profiling* estimates (sampled live bytes ≠ managed VM), so they sit
  **outside** the §8.6 byte reconciliation by design.

Verified by 21 new `topo-core` unit tests (placement + sampling), the `placement` integration
suite (the fixed-wall safety boundary over a real hugepage-backed engine, grouping observability,
the learned-profile→hint loop, and the live off→on→concurrent sampling lifecycle), a
`learned_profile_never_breaks_placement_geometry` proptest, the `placement` fuzz target, the
stats/control reconciliation tests, and the W8-8 header↔symbol cross-check over the six new
`topomalloc_profile_*` C symbols.

### W14 optimal-completion pass (closing the loop + span grouping + verification)

These decisions close the gaps the first pass left (learned profiles were observational; grouping
was hugepage-only; the policy heuristics and W17-3 verification were thin).

* **The learn → place loop is closed with a coarse, lock-free, per-bucket applied-hint table —
  not online per-call-site stack capture.** A `LearnedHints` table (one packed `PlaceHints` per
  placement bucket + an `any` short-circuit) is published from the confident, *consistent*
  per-bucket consensus (`SiteProfileTable::write_learned_hints`; disagreement neutralizes a bucket)
  and read by the allocation path with a single relaxed load. The key is the bucket (size class /
  medium / large) — the §24.1-sanctioned input the engine has for free — because capturing a stack
  on *every* allocation to key by site would violate the §31.4 hot-path budget (this mirrors how
  TCMalloc applies hot/cold via cheap explicit hints, with the online sampler steering aggregate
  policy). An explicit per-call hint always wins; when nothing is learned the lookup short-circuits
  to neutral, so the default (profiling-off) path is **byte-for-byte unchanged**. Per-arena
  learned scoping was deliberately *not* added (it would churn `record_alloc`'s signature across
  fuzz/property/integration tests for marginal benefit, since arena placement is already explicit
  via `NumaPolicy` and the span carries its arena tag) — documented, not a gap.
* **The "hotness 0 = cold vs. unhinted" ambiguity is resolved by an all-or-nothing merge.** The
  flag encoding has no "unspecified hotness" sentinel (`TOPO_COLD ≡ TOPO_HOT(0)`), so a learned
  hint is applied only to a request that is *fully* placement-unhinted (hotness 0 **and** lifetime
  Unspecified); any explicit placement hint disables the learned override entirely. A bare
  `TOPO_COLD` is therefore indistinguishable from unhinted and may adopt a learned hint — an
  accepted, documented edge (the hint is advisory regardless). This avoided adding a flag bit (and
  the ABI churn it implies) for a corner case.
* **Small-object span grouping is a span-selection *preference* (one bin per size class), not a
  per-(class) bin shard.** The `PlaceClass` (Default / Cold / Hot / Short — hotness dominant,
  lifetime breaking the neutral tie) is packed into spare `SpanFlags` bits (no descriptor growth —
  the 104-byte footprint and its pin test are unchanged) and set at create / empty-reuse re-tag.
  `CentralCache::remove_batch(place_class)` prefers a class-matching partial, else re-tags an empty,
  else `NeedSpan`; the caller creates a class-tagged span and, only on backend exhaustion, falls
  back to `ANY_PLACE_CLASS` (reuse any partial) — so grouping is **availability-first** (§2.4) and
  never a spurious OOM. Because every unhinted request maps to one class, an all-default program
  keeps a single pool per size class (no RSS regression); only differing hints segregate spans.
  This rides the existing single-bin-per-sc structure, orthogonal to the M2 per-node/per-label
  sharding (W5-4d), and changes no abstract transition (which span an object is carved from is
  policy) — so no Lean obligation.
* **The profile heuristics are recency-aware and bounded.** Rates are an event-driven EWMA of the
  inter-event interval (recency-weighted, no fragile fixed-point `exp`); hotness is an event-driven
  EWMA gated by its mean-absolute-deviation, so a noisy/bimodal site is *unstable* and never drives
  a confident hot/cold grouping (the recency estimate alone could flip-flop) while a phase change is
  tracked. The table is a 16-way **set-associative** cache with least-confidence replacement — the
  textbook fixed-capacity design, using all capacity with clean local eviction (no overlapping
  probe windows).
* **W17-3 verification matches the SPEC's prescribed methods.** The `sampler_no_alloc` integration
  test installs a counting `#[global_allocator]` and asserts the sampled path makes **zero** heap
  allocations across 50k sampled allocations — the §31.4 / Appendix-F "the unwinder never re-enters
  the allocator" invariant, the depth-counter method DD-1 calls for. A `sampling_overhead` criterion
  bench (off vs on) bounds the hot-path cost (DD-1 *F3*); the `SampleBloom` auto-refreshes on a
  bounded cadence so its false-positive rate stays bounded over a long run; and a §36.9 G-sim test
  re-proves the §24.5 safety wall and the learn → place loop identically over `Sele4nSim`.

Verified additionally by the new `learned_hot_profile_steers_unhinted_large_allocations_hot` (+
`no_learned_hint_*` contrast), `remove_batch_prefers_a_class_matching_span`,
`place_class_round_trips_and_is_orthogonal_to_quarantine`, `learned_hints_publish_consensus_and_*`,
`ewma_rate_is_recency_weighted`, the `sampler_no_alloc` proof, and the `gsim` module — all green
under `cargo xtask ci` (dual-arch build + test + the eight Lean gates).

## W15 — reallocation, aligned allocation & calloc zeroing (plan 06)

W15 owns the user-facing resize/zero surface. Its semantics (W15-1), move path (W15-2), same-class
in-place grow (W15-3a), aligned validation (W15-4), and calloc overflow/zeroing (W15-5) shipped with
the W8 realloc core; the completion pass closed **W15-3b (in-place shrink with tail-page return)**, the
one piece the W8 core stubbed as "keep the whole extent". Decisions:

* **In-place shrink returns the tail to the backend, but only when it can do so for free.** A
  medium/large `realloc` whose page-rounded new size is *strictly* smaller than the current usable size
  now splits the backing extent at the new boundary and frees the tail (§25.3, DD-1 "shrink, large tail
  → split + return tail to backend, return p"). The base pointer is unchanged, so the result is still
  an in-place shrink — the win is RSS, not a copy. It is **best-effort under always-correct semantics
  (§2.4):** a **cache-served** allocation (the W11 hugepage filler owns its own page geometry), a
  **slot-pool-exhausted** split, or a **sub-page** shrink (nothing to return) all keep the allocation
  whole. None of those fail the call — keeping the whole extent is a correct §25.3 result (SHOULD, not
  MUST). This is the same "dumb-but-correct fallback under an optimization" discipline as the move path
  layering.
* **Ordering is the whole game: split → retire pagemap → shrink descriptor → free the tail.** The tail
  is split off as a still-`Active` extent first (the sole fallible step — on slot exhaustion *nothing*
  is mutated, W4-5, so the allocation is untouched and stays whole). Its pagemap entries are retired to
  `Empty` **before** the tail extent is freed (`PageMap::retire_large_range`), so a later reuse of the
  freed extent can never collide with the now-stale `Large` entries (P-Map-001 one-page-one-descriptor)
  — exactly the retire-before-free discipline `Allocator::retire_span` already follows. Only then is
  the owning `LargeDescriptor` shrunk in place (`shrink_usable`, under its seqlock so a concurrent
  classifier never mixes the old/new usable size, W3-4) and the tail freed (applying the §20.5
  retain/unmap policy). The descriptor keeps its id/base/generation — the allocation keeps its
  identity, so outstanding base pointers stay valid; only its size moves.
* **No lock nesting, no aliasing surprise.** `LargeAllocator::shrink` reads the descriptor + backing
  under the pool lock, captures the **pagemap-rooted** descriptor pointer (not a slot pointer borrowed
  from the pool `&mut`) plus plain values, then releases the lock before the extent-manager calls — the
  pool lock and the extent lock are never held together (the alloc path's discipline). The §25 realloc
  contract excludes a concurrent free/realloc of the same pointer, so the slot is neither recycled nor
  mutated after the lock drops.
* **Accounting stays exact across the shrink (§8.6/§36.17).** The freed tail is credited to the global
  `freed_bytes` **and** the allocation's *original* arena's quota (§25.4 arena preservation), so
  `live_bytes = allocated − freed` drops to the new usable size and `Σ arena.used == live_bytes` still
  reconciles. The subsequent `free` credits the *current* (shrunk) usable size, so the two together
  return exactly what was charged. The `malloc_api` fuzz target reconciles `live_bytes` against its own
  model across every shrink, catching any drift.
* **No new abstract transition — and the *precise* reason (not a hand-wave).** A large allocation is
  **not** a core `Block`: clause 12 (`WfSlabLayout`) keys every block to a size class, so the §33.3
  block state machine (malloc/free/realloc-move over `s.blocks`) models the small-object slab path only.
  A large allocation is modeled in the **extent backend**, where the in-place shrink is exactly
  `extent_split` (§18.3, the certified `span_split_preserves_disjointness` geometry) followed by `extent
  free` (§20.1 `ExtentState`, pinned by the `extent state machine` `lake exe check` gate). The shrink
  **sequences** these two certified transitions — the *same shape as the W12 release controller*
  sequencing the certified `release_to_os_*` transition. It is **not** the W13/W14 case (placement
  policy that is invisible to the abstract state — a different, stronger argument I originally
  over-applied here): the shrink *does* mutate geometry, but only via already-certified extent
  transitions. It is also explicitly **not** `reallocMove`: the move needs the old and new blocks
  *simultaneously live and disjoint* (the copy window, `realloc_move_window_keeps_old_live`), which two
  same-base ranges can never satisfy — the shrink instead frees the tail, so nothing overlapping is ever
  live at once. The composition's one geometric premise — the kept prefix stays disjoint from its
  neighbours, and the kept prefix and returned tail never overlap (so a reused tail never aliases the
  live prefix) — is named and machine-checked by `realloc_shrink_inplace_tail_tiles_disjointly`
  (`lean/TopoMalloc/Theorems/Realloc.lean`), discharged against `span_split_preserves_disjointness`.
  So there is no new §33.4 obligation and no trace-grammar change — confirmed by the eight `lake exe
  check` gates staying green and the new theorem proof-checking under `lake build`.

Verified by `realloc_large_shrink_returns_the_tail_pages` and `realloc_large_shrink_credits_the_owning_arena`
(engine: same base, usable drops, backend `active` drops by exactly the freed tail, §36.17 reconciliation,
sub-page shrink keeps the extent, clean free), the profile-robust `free_sized_accepts_truthful_size_after_inplace_shrink`
(C ABI cross-check across both the extent-backed and hugepage profiles), `realloc_medium_shrink_returns_the_tail_through_the_abi`
(C ABI, extent-backed), the `realloc_preserves_content_and_survives_failure` property test, and the
`malloc_api` fuzz target — all green under `cargo xtask ci` (dual-arch build + the full test matrix incl.
`hugepage-optimized`/`low-rss`/`sele4n-sim` + the eight Lean gates).

## W12/W13/W14 formal-obligation review (the W15-3b lesson, generalized)

The W15-3b review exposed an over-broad habit: labelling a change "policy, not safety / no Lean
obligation / sequences certified mechanisms" without a cited, auditable backing. A deliberate pass
re-examined every such claim. The conclusion was *not* "they all cut corners" — each is sound — but
the *backing* was tightened so the claim is verifiable, not trusted, and a guard was added so the gap
cannot recur.

* **The two patterns are distinct and must not be conflated.** *Sequences certified transitions*
  (W12 release controller, W15-3b shrink): the change mutates abstract state, but only by composing
  transitions the model already certifies — so it must cite the **named theorem(s)** it rests on.
  *Pure policy, invisible to abstract state* (W13 NUMA router, W14 placement): the decision provably
  cannot change size/alignment/validity/free (§2.4/§24.5), so the abstract state never moves — and it
  must cite the **fixed-wall safety test** that pins that boundary. "Pure policy" is the stronger
  claim; W15-3b was originally mislabelled with it when it is really the (weaker, geometry-mutating)
  "sequences certified transitions" case.
* **W12 — release controller: sound, now pinned end-to-end.** The mechanism is certified
  (`release_to_os_preserves_live_objects`, `subrelease_preserves_live_backing`) and the controller
  only sequences it; the mechanism *structurally* refuses live memory (`ExtentManager::decommit`
  rejects `Active` with `NotFree`; the filler releases only empty hugepages / H-005-guarded cold-sparse
  pages). The prior live-wiring tests released only empty supply, so a new end-to-end test
  `controller_driven_release_preserves_live_objects` mixes live objects with releasable supply, drives
  the controller to Emergency, and asserts every live object survives untouched — the W14-grade wall.
* **W13 — NUMA router: genuinely pure policy, now pinned by a fixed wall.** The router only chooses
  *which node's* (certified) `HugePageBackend` serves a request; the node never appears in the
  abstract model. That was asserted in prose but not pinned. The new
  `placement_never_breaks_the_allocation_contract` is the §2.4 fixed wall: for every NUMA policy
  (Local/Bind(valid/stale)/Interleave/OsDefault/ArenaPolicy) **and** a bind failure, a routed
  allocation has the requested size + alignment, a fully writable+readable range, and frees home —
  the W13 analogue of W14's hint-invariance wall. `Topology::preferred_node` totality (every policy
  yields an in-range node or `DEFAULT`) is pinned by `placement_covers_every_numa_mode`.
* **W14 — placement policy: already the gold standard.** `engine_size_align_validity_free_are_invariant_under_hints`
  (every size × align × hint) + a pure-filler test + proptest + fuzz + a §36.9 G-sim re-proof already
  pin the §24.5 wall. No change needed; it is the reference the others were brought up to.
* **The guard: an `obligation citations (V-004)` lint.** `cargo xtask lint` now scans
  `crates/**/src/**/*.rs`: a comment block asserting "no Lean obligation" / "adds no abstract
  transition" / "not a modeled transition" without a citation keyword (a theorem reference, or
  `pin`/`certified`/`proven`/`discharged`/`fixed wall`) within a few lines fails the gate. It joins
  wrapped doc-comment lines (so a phrase split across lines is still seen) and matches citation stems
  on **word boundaries** (so an accidental substring like "map**pin**g" cannot launder a bare claim —
  the exact false-negative found while building the lint). The pure matcher is unit-tested
  (`obligation_citation_lint_requires_a_backing_citation`) over bare/cited/wrapped/far/substring cases.

Net: every "no formal obligation" claim in the tree now cites a theorem or a fixed-wall test, and the
lint keeps it that way. Verified green under `cargo xtask ci` (dual-arch build + full test matrix +
the eight Lean gates + the new lint).

## W15 optimal completion (the six deferred/sub-optimal items, closed)

A completeness pass closed every item the W15 self-audit flagged as deferred or sub-optimal, so the
reallocation / aligned-allocation / calloc surface is now optimal with no deferrals. Each rides an
already-certified mechanism (no new abstract transition); the two geometry-mutating ones add a named,
machine-checked Lean obligation.

* **Extent-merge in-place grow (W15-3a).** The symmetric twin of the shrink: a medium/large `realloc`
  that outgrows its extent now **absorbs the address-adjacent free extent** in place (no copy) instead
  of always moving — `ExtentManager::grow_in_place` trims the free neighbour to the exact deficit,
  commits it, and `absorb_next_in` folds it into the live extent (the dual of `split_tail`), then the
  pagemap is extended and the descriptor grown; on no-adjacent-free / slot-or-commit exhaustion /
  metadata it rolls back and `realloc` moves. The arena is charged the growth first (quota honored),
  refunded on a declined grow. Pinned by `realloc_grow_inplace_absorbs_disjointly` (discharged against
  the certified `span_merge_preserves_disjointness` + `merge_subset_left`) — `extent_merge`, not a new
  transition, and **not** `reallocMove` (no copy: the prefix stays put).
* **`xallocx` in-place resize (the inconsistency, fixed).** `realloc` and `xallocx` now share
  `Allocator::resize_in_place` (extracted, so the shrink/grow accounting has one home): `xallocx`
  genuinely shrinks (returns the tail) and grows (absorbs the neighbour) in place, reporting the achieved
  usable size — never moving. A small object's fixed slab slot still reports its size unchanged.
* **calloc `memset` elision (W15-5 / §26.3).** A `committed_memory_is_zeroed` provider contract (POSIX
  `true` — the region is `mmap(MAP_ANON)` and `decommit` is `MADV_DONTNEED`, so an extent committed from
  unbacked backing reads zero) lets a large/medium calloc over a **freshly OS-zeroed** extent skip the
  redundant `memset`; a recycled (retained-dirty) extent is re-zeroed. The skip is debug-verified
  (spot-checking the bytes really are zero), and the §26.2 guarantee (calloc is always zero) is the real
  net, proven by `calloc_large_is_fully_zeroed_fresh_and_after_recycle` across the fresh/recycled paths.
  A user backing (`HookProvider`) does not opt in, so its calloc keeps zeroing — the trust boundary is
  explicit. Threaded as `alloc_large`'s `zeroed` flag; the shared large path self-zeroes (the engine
  zeroes only small + hooked-arena large), so the hooked path is never silently left non-zero.
* **Cache-served (hugepage) shrink (W15-3b D).** The filler gains `trim(base, old_pages, new_pages)` —
  the same exact-extent validation as `free` (so it is not a forgeable partial free, S-007) but freeing
  only the tail and keeping the head, so the kept prefix stays a valid allocation; the tail returns to
  the filler (reusable, committed — W12 subreleases cold pages later). Reached via
  `RegionCacheHook::try_trim` (default declines; HugePageBackend trims sub-hugepage allocations; the
  NodeRouter routes by address) and `LargeAllocator::shrink`'s cache-served branch, which — because the
  filler frees the tail atomically — retires the pagemap and shrinks the descriptor **first**, then
  trims, rolling both back on a decline (multi-hugepage runs keep whole). So the tail return now works on
  *both* the extent and hugepage profiles.
* **Aligned size classes (W15-4).** Over-aligned **small** requests (e.g. 64-byte/cache-line-aligned) are
  now served from a **slab slot, not a 16 KiB page**. The mechanism already existed and was proven — the
  classifier's over-alignment **walk** (W2-3b) and the `maxAlign` bound — but the shipped table was
  uniformly 16-aligned so the walk never advanced. The generated table now records **each class's natural
  alignment** (the largest power of two dividing its size, capped at the page-aligned slab base), which a
  power-of-two-divisor-sized class *already* provides with no layout change (objects at a page-aligned
  `base + i·size` are size-aligned). `MAX_ALIGN` is derived as the widest class alignment (now the page
  size); `> MAX_ALIGN` still routes to medium/large. **No Lean proof change** — `coversAllB` (size
  coverage) and `maxAlignOkB` (the bound) are evaluated over the new table and the over-alignment walk is
  generic, all re-verified by the G-table + `lake exe check` gates. A pure golden-config edit + regenerate.
* **`valloc`/`pvalloc`.** Rounded out the §10.1 optional-compatibility surface (page-aligned; `pvalloc`
  rounds up to a page, `pvalloc(0)` is one page), exported, declared in the C header, and exercised by the
  C ABI harness (which the symbol↔header cross-check validates).
* **realloc profiling (W15-2), assessed adequate.** A realloc is sampled as `on_free(old)` +
  `on_alloc(new)`, attributing the new allocation to its realloc **call site** (distinct from malloc
  sites in the backtrace) — which is what §25.4 "profile as realloc" means. A distinct realloc *event
  type* is a plan-07 profiling feature, not a W15 gap.

Verified by the new engine tests (`realloc_large_grow_absorbs_adjacent_free_in_place`,
`realloc_large_grow_falls_to_move_when_blocked`, `resize_in_place_shrinks_grows_and_reports_small_unchanged`),
the filler `trim_frees_the_tail_and_keeps_the_prefix_a_valid_allocation`, the ABI tests
(`xallocx_resizes_a_large_allocation_in_place`, `aligned_small_allocations_use_a_slab_not_a_page`,
`calloc_large_is_fully_zeroed_fresh_and_after_recycle`, `valloc_and_pvalloc_are_page_aligned`,
`hugepage_realloc_shrink_trims_the_cache_served_tail`), and the updated classify/size-class coverage —
all green under `cargo xtask ci` (dual-arch build, full test matrix incl. `hugepage-optimized`/`low-rss`/
`sele4n-sim`, the eight Lean gates + the two new realloc theorems, the C/C++ ABI harness, and the
obligation-citation lint).

**PR #19 review hardening (three findings, all on the W15 surface above).** An automated review flagged
three issues in the freshly-landed code; each was confirmed against the code and fixed:

* **`TOPO_ZERO` is honored on the in-place grow (§26.2, was a leak).** A `topo_rallocx`/`topo_xallocx`
  with `TOPO_ZERO` that grew a large allocation **in place** returned the original pointer without
  zeroing the newly exposed suffix — so a grow that absorbed an adjacent **dirty** free extent handed
  back its recycled bytes, violating the documented zero guarantee (the move path was always correct).
  The shared `apply_inplace_large_resize` now zeroes `[usable, usable+grown)` on a `TOPO_ZERO` grow,
  exactly as the move path does (the old prefix untouched). Pinned by
  `realloc_inplace_grow_zeroes_the_exposed_tail_under_topo_zero` (grows over a 0xff dirty neighbour,
  asserts the tail reads zero **and** that the in-place path was taken).
* **`committed_memory_is_zeroed` is platform-gated to Linux (was an unconditional `true`).** The opt-in
  is a *blanket* promise `alloc_z` applies to any `committed_len == 0` extent — Reserved **or** Released —
  so it may answer `true` only where both read zero. That holds on Linux (`MAP_ANONYMOUS` reserve +
  `MADV_DONTNEED` zero-fault) but **not** on Apple (`decommit` is `MADV_FREE_REUSABLE`, which may retain
  contents — a recommitted Released extent is not zero) nor the non-unix fallback (reserve via
  uninitialized `alloc` — a fresh Reserved extent is not zero). It is now `#[cfg(target_os = "linux")]`
  `true`, conservative `false` elsewhere (always `memset`), pinned by
  `committed_memory_is_zeroed_matches_the_platform_guarantee`.
* **In-place grow has no fallible step after the irreversible absorb (was a best-effort rollback that
  could itself fail).** The grow used to absorb the neighbour, then `install_large_range` (fallible on
  metadata), then on failure split the tail back off — a rollback that could fail under slot exhaustion,
  leaving the extent enlarged but unadvertised until free. The order is now **reserve the pagemap leaves
  → absorb → publish**: a new `PageMap::reserve_large_range` does phase-1 node creation (the lone
  metadata-fallible step) *before* the absorb, so the post-absorb publish allocates nothing and cannot
  fail. No rollback exists to fail. Pinned by `install_large_range_after_reserve_allocates_nothing_and_publishes`
  (the publish over a reserved multi-leaf range consumes zero further metadata). These are mechanism
  reorderings, not new transitions, so no Lean obligation changes.

## W16 — concurrency, memory ordering, fork, signal & TLS (plan 05)

The M2 concurrency foundation. Each decision below is "correct before fast": the verified-correct mechanism
ships first; a noted optimization is a later perf pass, never a correctness gap.

* **One ranked-lock primitive; every `topo-core` lock is a `RankedLock` (W16-1a, §27.2).** The four
  hand-rolled test-and-set spinlocks (`CentralLock`, `SpanLock`, `BackendLock`, the transfer-bin lock) and
  the per-CPU front-end lock are replaced by a single `RankedLock<const RANK: u8>` carrying a **compile-time
  rank**. The §27.2 hierarchy is *refined* into a total order over the concrete locks (the SPEC permits
  refinement): a `FRONT_END` rank for the per-CPU lock (the outermost data-path lock), a `SPAN_POOL` rank
  between `CENTRAL` and the per-span `SPAN` lock (because `create_span` recycles a descriptor — which takes
  the per-span lock — while holding the descriptor pool), and a single shared `BACKEND` rank for the extent
  manager / large pool / huge backend (proven never held simultaneously: the large path consults the
  region-cache hook *before* taking the extent lock, and a span's backing extent is allocated *and released*
  before the descriptor pool lock). The per-CPU lock keeps its `#[repr(C)]` offset-0 layout (a `RankedLock`
  is a single `AtomicBool`), so the RSEQ assembly that peeks the lock byte is byte-for-byte unchanged.

* **The lock-order checker is a per-thread held-rank set, allocation-free and debug-gated (W16-1b, the
  G-conc gate).** Every acquire records its rank in a fixed-size, `const`-initialised thread-local array
  (never a `Vec` — Local-Exec TLS, so recording a rank never re-enters `malloc` even when the crate backs
  the process `#[global_allocator]`) and asserts the new rank exceeds every rank held; release removes it.
  Active under `debug_assertions` + the `debug-checks` profile (the normal `cargo test` build and CI); a
  no-op in `performance` and pure `no_std`. Any out-of-order acquire trips a `debug_assert!` — the deadlock
  a lock-order cycle would cause, caught deterministically. A static `cargo xtask lint` gate
  (`lock hierarchy (G-conc)`) forbids the `compare_exchange(false, true, …)` spinlock idiom anywhere outside
  `lock.rs`, so a new hand-rolled lock that would escape the checker fails CI (DD-3 F1). The whole existing
  suite (incl. the multithreaded central/arena/hugepage tests) runs green under the checker — empirical
  proof the existing acquisition order already respects §27.2.

* **`fork()` safety is a drain gate, not "acquire every lock" (W16-5, §28.1, DD-5).** jemalloc-style prefork
  acquires every internal mutex in rank order; that is **infeasible here** because the per-span locks are
  *dynamic* (created on demand) and are taken *outside* the central lock (`activate_span`, `recycle`), so
  acquiring the fixed structure locks would not stop a thread from holding a per-span lock at `fork()`.
  Instead every public operation runs inside `fork::operation_guard()`, and the pre-fork handler **drains**
  the in-flight count to zero: no in-flight operation ⟺ no internal spinlock held ⟺ every structure is
  consistent. The parent resumes; the child **resets** (not unlocks) the gate, clears the lock-order
  checker's inherited bookkeeping, and disables background maintenance. This is the §28.1 "global allocator
  fork lock + quiesce". The per-op gate is one atomic on the hot path — the M2 fork-safety cost; a
  per-thread-sharded gate is the future perf optimization.

* **The gate is a single-word CAS read-write lock — chosen over a `SeqCst` Dekker gate so `loom` can verify
  it.** The natural "increment a count, then check a fork flag; the forker sets the flag, then waits for the
  count" is a store-then-load (Dekker) shape that needs `SeqCst` to be correct — but `loom` models `SeqCst`
  only as `AcqRel`, so it cannot machine-check such a gate (and indeed reports a spurious violation). The
  gate instead packs the fork-pending bit and the in-flight count into **one** `AtomicU64`: an operation
  enters by a compare-exchange that increments the count *iff* the fork bit is clear, so a reader that races
  a forker either wins before the bit is set (and is drained) or fails its CAS after (and parks) — the
  standard CAS read-write-lock shape, with **no `SeqCst`**. `gate_admits_no_op_across_a_fork`
  (`tests/loom_protocols.rs`) proves over every interleaving that no operation is admitted-and-in-flight at
  the instant the drain forks. The trade-off accepted: the CAS contends among concurrent entries (vs. a
  `fetch_add`), which the future sharded gate also removes.

* **TLS is initial-exec / `const`-init (W16-2, §27.6, DD-4) and tested via `dlopen`.** The allocator's own
  thread-locals — the lock-order checker, the bootstrap/sampling re-entrancy guards — are `const`-initialised
  `thread_local!`s, which compile to Local-Exec (a direct `%fs:…@TPOFF` load, no `__tls_get_addr`, no lazy
  guard, no allocation) in an executable, so a thread's first allocation never re-enters the allocator while
  establishing its TLS. The danger case is `dlopen` (where general-dynamic TLS may allocate on first access);
  `tls_dlopen.rs` loads the freshly-built `libtopo_abi` via `dlopen` (`RTLD_LOCAL`, its own statics/TLS) and
  drives its `malloc`/`free` from fresh threads under a watchdog — a re-entrancy regression would deadlock and
  the watchdog aborts loudly. The `global_allocator` example additionally exercises first-allocation on many
  fresh threads while TopoMalloc *is* the process `#[global_allocator]`.

* **The crash summary is lock-free and reads only cumulative-byte atomics (W16-6, §28.4).** A signal/crash
  handler must not take a contended lock (which the faulting thread may hold) or allocate (`malloc` is not
  async-signal-safe). `Allocator::crash_summary()` reads only the relaxed `allocated_bytes`/`freed_bytes`
  atomics + the process init phase / background flag and formats them into a caller buffer with an
  allocation-free bounded cursor (`init::CrashSummary::write`); the C `topomalloc_crash_summary` never forces
  the allocator's lazy init (it uses a non-initializing `GLOBAL.get()`). Object/state breakdowns, which take
  locks, are deliberately omitted — "full stats may be unavailable in crash context" (§28.4).

* **Init is observable as monotone §35.4 phases; shutdown leaks by default (W16-7).** `init::PhaseTracker`
  (one `AtomicU8`, lock-free, monotone — never winds backwards) advances through Phase 0–6 as the global
  initializer brings the allocator up (bootstrap metadata → atfork registration → OS discovery → engine →
  profiling → operational), so a reentrant caller can ask "open for business yet?" without a lock (§35.4
  "each phase MUST be reentrancy-safe"). Shutdown keeps the §35.5 policy: allocator metadata is process-lived
  (the `MetaArena` is `Box::leak`-ed), with explicit `teardown()`/`Drop` available for tests.

* **W16 is concurrency/operational, not an abstract §33.4 transition — no Lean obligation.** The lock
  hierarchy, fork gate, TLS model, and init phases change *when* and *how safely* the allocator runs, never
  the abstract ownership state the Lean model tracks. Per the V-004 citation rule, the two load-bearing
  properties are pinned by concrete artifacts rather than a bare claim: deadlock-freedom by the fixed-wall
  `lock::tests::out_of_order_acquire_trips_the_checker` (the checker rejects an out-of-order acquire), and
  fork-quiesce by the `gate_admits_no_op_across_a_fork` `loom` model (no operation crosses a fork).

### W16 optimal-completion pass (sharded gate, hook re-entry, load-bearing phases, ordering gate)

A self-audit of the first W16 pass surfaced gaps that this pass closes — each was either a deferred
optimization, a wired-ahead-of-consumer mechanism, or a guarantee that only held in `topo-core`'s own
tests. All are now genuine, tested, and active in the **real** artifact.

* **The fork gate is per-CPU sharded *and* loom-verifiable — no `membarrier` needed.** The first pass shipped
  a single-word CAS gate (verifiable but a contended cacheline) and deferred sharding behind `membarrier`
  (which `loom` cannot model). The key insight that dissolves the trade-off: put the **fork-pending bit in
  every shard**, so an op's `fetch_add` on its CPU's shard returns the bit in its *previous value* — the
  fork check rides on the RMW result, with **no separate load** (no Dekker hazard) and **no `membarrier`**.
  Each shard is thus an independent, loom-checkable single-word gate; `prefork` sets the bit in all shards
  then drains all counts. `operation_guard` is now one `fetch_add` on the running CPU's own (cache-line-
  padded) shard — no CAS retry-storm, no shared cacheline when sharded. Sharding auto-enables when a cheap
  per-CPU id is available (`topo_arch::rseq::current_cpu()`, the glibc/rseq area), falling back to "all ops
  on shard 0" (exactly the single-word gate) otherwise. Verified by `gate_admits_no_op_across_a_fork` **and**
  a new `multishard_gate_admits_no_op_across_a_fork` `loom` model, plus a `benches/fork_gate.rs` criterion
  bench that *measures* the ~13 ns/op uncontended cost. (So the chosen answer to the verifiability-vs-
  scalability question was "both", achieved without the membarrier complexity the question assumed.)

* **`prefork` quiesces background maintenance through the *same* drain.** "Quiesce background threads"
  (W16-5a) was a bare flag. Internal maintenance (the release pump, rebalancer) now runs inside a
  `fork::maintenance_guard`, which counts in the same gate as public ops — so `prefork`'s single drain
  quiesces both, and the child's `background_enabled = false` keeps maintenance off until the host re-arms
  it. A real handshake (tested), wired ahead of its pump consumer. The fork battery additionally gained a
  **concurrent-forker** test (two threads forking at once, serialized by `FORK_LOCK`) and a
  **parent-consistency** test (a long-lived allocation survives 20 forks intact).

* **The thread-local safety machinery is active in the *real* allocator, not just `topo-core`'s tests.**
  Nothing enabled `topo-core/std`, so in `topo-abi` (and every downstream build) the lock-order checker, the
  S-007 bootstrap guard, and the W16-6 hook guard were silent no-ops — the G-conc checker only ever ran in
  `topo-core`'s own lib tests. `topo-abi` and `topo-tests` now enable `topo-core/std` (they are hosted, std
  artifacts anyway), so the checker runs across the **integration + ABI** suites (it found zero new
  violations) and the hook guard works in production. The `no_std` hot-path capability is preserved for the
  kernel/seLe4n target, which builds `topo-core` *without* the feature (verified: `cargo build -p topo-core`
  and the seLe4n `real-abi` build stay `no_std`).

* **A re-entrant extent hook now fails safe instead of deadlocking (W16-6).** The per-provider `enter_hook`
  flag could not stop the *same-arena* re-entry that deadlocks on the back-end lock (the lock is taken
  before the flag is reached). A per-thread `hooks::hook_reentry` domain (the `reentry_flag!` macro's
  `scope()`+`active()` shape), set for every user-hook call, lets the **allocator entry** decline a
  re-entrant allocation with a null **before** any lock — a recoverable, defined-behaviour decline (not a
  panic: we are inside the hook's locked context, where an unwind could strand a lock). Pinned by
  `a_hook_that_reenters_the_allocator_fails_safe_not_deadlock` (a hook whose `commit` re-enters the same
  arena; watchdog-guarded).

* **Init phases are precise and load-bearing (W16-7).** The first pass skipped phases 3/5 and only *reported*
  the phase. The global initializer now advances through **every** §35.4 phase at its real boundary
  (bootstrap → atfork → OS discovery → arena registry → per-CPU/rseq → background/profiling → operational),
  and the phase **gates behaviour**: `maintenance_guard` declines before the background/profiling phase.
  Tested end-to-end (`global_init_reaches_operational_and_crash_summary_reflects_it`).

* **`SeqCst` is off the §27.3 map, now gate-enforced (W16-3).** The ordering map's policy is
  Release/Acquire/AcqRel/Relaxed; the redesigned gate uses no `SeqCst` at all. A new `cargo xtask lint` gate
  (`atomics ordering (W16-3)`) forbids an unjustified `SeqCst` in non-test `topo-core` (an inline
  `SeqCst: <reason>` permits a deliberate one, mirroring the V-004 citation discipline). The W16-1b lock
  lint was likewise strengthened: it is test-aware (scans only the non-`#[cfg(test)]` portion) and also
  forbids a raw `std::sync::Mutex`/`RwLock`/`Condvar`/`parking_lot` lock, not just the spinlock idiom.

* **TLS non-re-entrancy is *proven by depth*, and over-gating removed.** The `global_allocator` example now
  wraps the allocator in a depth-counting `#[global_allocator]` and asserts the steady-state path —
  including each fresh thread's first TLS-establishing allocation — runs at depth exactly 1 (makes **no**
  nested allocation), the precise statement of "first TLS access never re-enters" (the `dlopen` general-
  dynamic case stays covered by `tls_dlopen.rs`). And the lock-free `owns`/`recognizes` are no longer
  fork-gated (they hold no lock; the gate was pure overhead), while `usable_size`/`stats` (which take a
  lock) keep it. The crash summary gained a lock-free `in_flight_ops` field (allocator activity at crash
  time); per-object counts stay omitted by design (they need locks, §28.4) rather than taxing the hot path.

### W16 deep-audit pass (re-entrancy deadlock, numa fork hole, hook free/realloc)

A code-first audit ("do not trust the docs") of the W16 changes found three real defects — two of
them latent deadlocks — and fixed each with a regression test.

* **The fork gate had a latent re-entrancy deadlock; the gate is now genuinely re-entrancy-aware.** The
  module doc claimed "a re-entrant operation simply nests the count", but the code did not: a *nested*
  `operation_guard` re-checked the fork bit and, during a fork window, **parked** — while the *outer*
  guard's count was still held. `prefork`'s drain waits for that count, so it would hang forever. The
  current gated methods happen not to re-enter (the sampled path is allocation-free, verified), so it was
  latent — but gating the `numa` control ops (below), which *do* allocate, would have triggered it. Fixed
  by tracking a **per-thread nesting depth** (a `const`-init, allocation-free thread-local, Local-Exec,
  ~1 cycle): only the *first-level* guard takes a shard slot and does the fork check; a nested guard nests
  the depth and runs to completion (the fork is, by definition, draining the outer op). Pinned by
  `nested_guard_during_fork_window_does_not_deadlock` (a thread takes a nested guard *inside* an open fork
  window; a regression deadlocks and a watchdog aborts).

* **The NUMA control surface was not fork-gated (a `hugepage-optimized` deadlock).** Every
  `topomalloc_numa_*` entry point takes the router lock (rank `BACKEND`) and/or per-node backend locks,
  but none ran inside the fork gate — so a `fork()` during a `rebalance_tick` / `release` / `refresh` /
  stats read could leave a backend lock held in the child, deadlocking the child's large path. All seven
  are now wrapped in `operation_guard` (safe now that the gate is re-entrancy-aware: `refresh`'s
  `discover_topology` allocation nests rather than parks). Pinned by
  `child_is_safe_with_concurrent_numa_control_calls` (workers hammer the control surface while the main
  thread forks; the child both allocates and calls the surface — watchdog-guarded, `hugepage-optimized`).

* **The hook re-entrancy fail-safe covered only `malloc`; it now covers `free`/`realloc` too.** A
  re-entrant hook calling `free` would take a central/span lock while holding the hook's back-end lock — a
  lock-order inversion that, with the now-artifact-active checker, **panics inside the locked hook
  context** (stranding the lock); a same-arena large `free` would deadlock outright. The
  `hook_reentry_declines()` guard now gates `free` (declines as a `Null` no-op — the contract-violating
  object leaks, the §2.4 safe degradation) and `realloc`/`resize_in_place` (decline to null/`None`, the
  original preserved). The re-entrancy test now drives all three (malloc, free, realloc) and asserts the
  scratch survives the declined free/realloc.

* **A regression guard confirms the checker is genuinely live in the artifact.**
  `lock_order_checker_is_active_in_this_artifact` (in `topo-abi`) trips an out-of-order acquire and
  requires the panic — so if `topo-core/std` is ever dropped (silently disabling the checker + the S-007 /
  hook guards everywhere but `topo-core`'s own tests), CI fails. (An incidental fix from the audit: the
  helper insertion had orphaned `allocate_in`'s doc + `SPEC-transition` tag — an M-001 violation — now
  restored.)

### PR #20 review pass (Codex automated review of the W16 fork/concurrency work)

An automated review (Codex) of the W16 changes raised five findings; each was checked **against the
code** (not taken at face value) and four were confirmed and fixed, with the fifth (a narrow
lazy-init/fork race) deferred to a decision because its complete fix is architecturally significant.

* **P1 — the hook re-entry decline missed a *direct* `HookProvider` backend.** `hook_reentry_declines()`
  pre-gated on `self.hooks.count` (the per-arena hook-registry count), but `HookGuard` sets the per-thread
  `hook_reentry` domain for **every** `HookProvider` hook — including an allocator built directly over a
  `HookProvider` (`Allocator::new`, a documented custom-backing path), where `count` is 0 yet a hook is
  live. A re-entrant `malloc`/`free` from such a hook would slip past the decline and reach (deadlock on /
  invert the lock order of) the back-end lock. Fixed by keying the decline on the `hook_reentry` domain
  **alone** (one Local-Exec TLS read — it replaces, not adds to, the former atomic load, so the no-hook hot
  path is unchanged). Pinned by `direct_hook_backend_declines_reentrant_malloc_and_free` (a direct-backend
  allocator whose `commit` re-enters; the re-entrant malloc/free must be declined, not deadlock/trip the
  checker).

* **P2 — the lock-order checker was compiled out of a hardened *release* artifact (doc-vs-code).** Plan 05
  states the held-rank checker runs in "debug + the `debug-checks` profile", and the §17.3/Appendix-B
  checks follow the "profiles are features, not forks" principle (feature-gated, so they survive into a
  `--release --features hardened` build). But the checker was gated on `debug_assertions` alone, so a
  hardened release silently omitted it — and its violation check was a `debug_assert!`, elided in release
  even where the module compiled. Fixed both: the `checker` module (and its no-op stub) now gate on
  `any(debug_assertions, feature = "debug-checks")`, and the trip uses `assert!` (the module exists only
  when the checker is active, so the check must fire whenever it exists). A new CI step
  (`test hardened-release lock checker (G-conc)`, `--release --features debug-checks`) proves the checker
  is active **and** trips there; a plain release still compiles the zero-cost stub.

* **P2 — the C `topomalloc_numa_*` analogue for arenas: `arena_handle`/`arena_resolve_handle` were not
  fork-gated.** Both resolve through `ArenaRegistry::stats`, which takes the arena-registry lock, and they
  back the public C `topo_arena_handle`/`topo_arena_id`/`topo_mallocx_arena` entry points — so a `fork()`
  during one could strand that lock in the child, exactly the gap the W16 audit closed for the `numa`
  surface. Both now take `operation_guard` (the lock-free `arena_is_active`/`arena_has_hook_backend` stay
  ungated, like `owns`/`recognizes`).

* **P2 — the C smoke test read a non-NUL-terminated buffer.** `topomalloc_crash_summary` returns a byte
  count and does not terminate, but `abi_smoke.c` passed an *uninitialized* `char[256]` straight to
  `strstr`, which could read past the written region. Now caps the write at `sizeof-1` and terminates at
  the returned length before the C string call.

* **P2 — fork racing the very first lazy allocation (fixed; chosen approach: full fork-safe lazy init).**
  The `pthread_atfork` handlers were registered *inside* the first `GLOBAL.get_or_init`, and that init was
  not counted by the fork gate, so a `fork()` from another thread during the first-ever allocation could
  leave the child blocked on a half-initialized `OnceLock` (the initializer thread does not exist in the
  child). Real but extremely narrow (almost every process allocates — via the runtime — before it
  threads/forks). Closed by **both** halves the issue requires: (1) the handlers are now installed
  **eagerly at library load** by an ELF `.init_array` constructor (`fork_api::REGISTER_ATFORK_CTOR`),
  before any allocation, so a concurrent fork is intercepted — with the first-`global()` call kept as a
  fallback for a build where the ctor is elided (an rlib in a test binary); and (2) `global()`'s slow path
  now takes `fork::operation_guard()` around the lazy init, so the now-registered `prefork` **drains-and-
  waits** for an in-progress construction instead of forking mid-flight. Registration was switched from a
  blocking `Once` to a **CAS guard** (claims the flag *before* `pthread_atfork`) so it is re-entrancy-safe:
  if `pthread_atfork` itself allocates through this allocator during the ctor, the nested registration
  returns at once rather than dead-locking. Lazy init is preserved (the ctor does only the lightweight
  registration; the allocator is still built on first use), and the steady-state path is unchanged — the
  `global()` fast path returns the cached allocator with no gate cost, so the bootstrap depth-1 proof still
  holds. Pinned by `register_atfork_is_idempotent_and_reentrancy_safe` (concurrent + repeated registration:
  no panic/deadlock/double-register) and the existing fork battery (no regression).

* **W18 — security & hardening: granular features composed by profiles (plan 08, §29).**
  Each §29 protection is its **own opt-in Cargo feature** in `topo-core` (`junk-fill`,
  `quarantine`, `guard-pages`, `secure-scrub`) rather than a single `hardened` umbrella, and the
  `hardened`/`debug` profiles *compose* the ones they want. The decision follows "profiles are
  features, not forks" (principle 8) to its conclusion: a deployment can opt into one protection
  without the rest, and — crucially — a feature that is **off compiles every entry point to a
  true no-op**, so the `performance` build is byte-for-byte the un-hardened one (the protections'
  state, e.g. the quarantine ring and the guard sampler, lives behind `#[cfg]`-gated engine
  fields that do not exist without their feature). The runtime-expensive protections (quarantine,
  guard sampling) are **off by default even when compiled in** — the RSS/latency cost is not
  imposed unasked — and are armed via the `topomalloc_quarantine_*` / `topomalloc_guard_*` control
  surface or `$TOPOMALLOC_*` env, mirroring the W17 sampler. The single seam this adds to the
  provider trait is `protect` (the W18-4 guard-page `mprotect`), defaulted to a no-op so a backend
  without page protection (or the Sim) treats guards as advisory — the allocator never relies on a
  guard for correctness (§2.4). W18 adds **no abstract §33.4 transition** (the quarantine still
  "frees" from the application's view, guards/scrub change only placement/contents, never
  size/alignment/validity); the one model tie is W18-6, whose runtime scrub is the image of the
  pre-existing Lean `scrub_before_downgrade` theorem.

* **Audit — authority checks belong to the observer, not to the observed (0.3.0).**
  Two defects in this pass shared one shape: a security decision keyed on state the
  *untrusted side* controls. Label redaction returned the raw, cross-domain stats summary
  whenever the observer happened to dominate every **currently live** arena — so a high
  domain could flip the low view by creating and destroying a labelled arena (a covert
  channel), and every cross-domain aggregate was disclosed whenever no high arena existed.
  The POSIX provider's `madvise`/`mprotect`/`mbind` ops validated only that a sub-range fit
  inside the caller-supplied `Region`, never that the region was one of *its own*
  reservations — so safe code could hand it a foreign `Region` and have live memory zeroed
  or made inaccessible. Both are now decided from properties the caller cannot forge: the
  observer's own label against a fixed lattice top, and membership in the provider's owned
  set. The rule this ratifies: **a check that reads the state being protected is not a
  check.**

* **Audit — a "randomized" defence is only as good as its seed (0.3.0).**
  The W18-4 guard-page sampler and the W18-3 quarantine evictor documented
  unpredictability as the property that makes them useful, and drew from xorshift streams
  seeded by compile-time constants — identical in every process of a given binary, so the
  guarded allocation ordinals were offline-computable. "Randomized, not a fixed stride" was
  true and beside the point. `topo-core` is `no_std` and cannot read the OS, so entropy is
  now **pushed in** through `harden::set_process_entropy` — the hosted shell reads
  `AT_RANDOM`/`getrandom(2)` once at start-up (allocation-free, raw `libc`) and the engine
  re-seeds both samplers from it, with deterministic mode (§30.4) still overriding for
  replay. The companion rule: environment-derived configuration is skipped under
  `AT_SECURE`/setuid, as glibc does for `MALLOC_*`, because in a privileged process the
  environment is attacker input.

* **Audit — a lazily-initialized global must never re-enter its own initializer (0.3.0).**
  The startup hooks that honour `$TOPOMALLOC_*` ran inside `GLOBAL.get_or_init` and reached
  the engine through `global()`, which re-entered the still-running `OnceLock`; merely
  exporting `TOPOMALLOC_QUARANTINE=1` hung the process at its first `malloc`. Nothing set
  those variables in the tree, so no test noticed. Hooks now take the just-built engine by
  reference, and `crates/topo-abi/tests/env_startup.rs` re-execs the test binary once per
  documented variable with a bounded wait, so a reintroduction fails instead of hanging CI.
  The same shape appeared in the fork gate, whose quiesce window the *forking thread itself*
  could park on — an allocation from any sibling `pthread_atfork` handler was enough — fixed
  by exempting that thread for the window.

* **W6/W7 — a front-end cache needs a per-object residency marker, or exact double-free
  detection dies with it (0.4.0).**
  Wiring the per-CPU and transfer caches onto the live small path moves a freed object
  into a slot instead of the central free list, so `central_insert`'s test-and-set — the
  mechanism that made `free(p); free(p)` return `DoubleFree` — never sees it. The second
  free would push the same address into the slot again and the cache would hand one object
  to two callers. Answering "is this address cache-resident?" by scanning is
  `O(MAX_CPUS × capacity)`, so the object is **marked** instead: a per-span, lock-free
  `CachedBits` bitmap whose atomic test-and-set *is* the oracle, exactly as the free
  bitmap's is for the central path. The free path already resolved `(span, index)` through
  `validate_free`, so the mark costs nothing there; the alloc path pays one pagemap walk to
  clear it, which is cheaper than the two spinlocks the cache removes. Alternatives were
  rejected on principle, not effort: leaving cached objects untracked (§8.5 permits it —
  but it is a real safety regression against a pinned test), storing the marker in the
  freed object's own bytes (destroys the W18-1b property that free memory holds no
  allocator metadata), and a central-free-bit "hint cache" (keeps the span lock on every
  operation, which is the cost the layer exists to remove). The rule: **a fast path may
  move where state lives, never whether it exists.**

* **W6 — the front end caches one placement tuple, so everything it holds is
  substitutable (0.4.0).**
  A per-CPU slot is keyed by `(core, size class)` alone, so whatever it vends must be
  interchangeable for *any* request of that class. Only the default arena's
  placement-unhinted spans participate; an explicit arena, a labelled arena, or a
  cold/hot/short-tagged span keeps the pre-W6 central path byte for byte. That is what
  keeps §22.7 arena isolation, §36.4 quota exactness, §36.12 label isolation and the W14
  placement grouping *exact* rather than approximate — none of them becomes a property the
  cache has to remember, because memory subject to them never enters it. Cacheability
  cannot change under a cached object either: a span is re-tagged only while empty, and an
  empty span has no cached objects. The corollary bug this ratifies: the predicate was
  first written against `PlaceClass::Default`, but an unhinted request computes
  `Hotness::from_hint(0) == Cold`, so it matched *nothing* and the cache stayed silently
  dead. The class is now derived by running neutral hints through the same functions the
  allocation path uses, so the two agree by construction rather than by a named constant
  someone must remember to keep in step.

* **W6 — front-end residency is its own byte class, not a discount on the others
  (0.4.0).**
  A cached object has been freed by the application, so it left `live_bytes`; it never
  reached the central bitmap, so it is not in `central_free_bytes`. Reporting it in neither
  would have made the §8.6 covering identity `live + central_free <= active` *weaker* the
  moment the cache went live — it would still hold, by losing track of memory. `per_cpu_bytes`
  and `transfer_bytes` are therefore first-class in `AllocatorStats`, the covering identity
  counts them, and the §21.3 release ladder's "drain caches" rung
  (`topomalloc_cache_flush_all`) converts them back. Draining had to sweep **both** layers:
  flushing a core's slots pushes their contents one level *down* into the transfer cache, so
  a slot-only drain reports success while the spans stay non-empty and their backing stays
  unreclaimable.

* **W6 — a 360 KiB per-CPU array does not belong inside a value type (0.4.0).**
  `CpuCache` is `MAX_CPUS` × a slot per size class. Embedding it in `Allocator` by value
  made every construction materialise that much stack — enough to overflow a test thread —
  and charged it to every embedding that never touches the front end (a `no_std` profile,
  an arena-only host). The array is now carved from monotonic metadata on first use, which
  is the discipline the span/large descriptor pools and the free bitmaps already follow.
  Zeroing is the initialiser: every field of `PerCpu` is an atomic whose all-zero pattern
  is its `new()` value, so nothing constructs a temporary — a property that is a fact about
  the field set rather than about Rust, and is therefore pinned by a test. A failed carve is
  not an error: the accessors read a null array as "no slots", the front end declines, and
  the central path serves (§2.4 — a policy layer may degrade, never fail an allocation).

* **W6 — the §13 thread cache is an *alternative* front end, not a layer; deleting the
  prototype (0.4.0).**
  `thread_cache.rs` shipped ~850 lines of unit-tested, publicly re-exported code that no
  allocation ever reached, and it could not be wired as written: its slots were
  `Vec<usize>`, so `push` would `realloc` through the global allocator on the allocation
  fast path and the thread-exit flush would `dealloc` through it during TLS teardown —
  the Appendix-F recursion anti-pattern at both ends. Worse, `pub unsafe fn
  set_flush_hook(&mut self, hook, ctx: *const ())` carried a contract ("`ctx` must remain
  valid until the thread exits") that **no caller can discharge**, because the thread that
  must outlive `ctx` is not the one calling; `Drop` then invoked that raw pointer. Dead
  code is a cost; dead *public* code with an undischargeable safety contract is a trap.

  Rewriting the storage is not the job. Per-thread state loses four properties per-CPU
  state has for free: **bounded metadata** (monotonic metadata is never returned and
  thread count is unbounded, so a per-thread carve needs a registry and a reuse
  free-list — with its own lock), a **teardown path** that must never touch
  destructor-registered TLS (`THREAD_CACHE` is `RefCell<ThreadCache>`; re-entering it
  during its own destruction panics), a **fork story** (a child inherits the caches of
  threads that no longer exist, so their objects are unreachable forever — per-CPU slots
  have no analogue, since every CPU still exists in the child), and a third `CachedBits`
  transition plus a third residency term in every stats/invariant/drain path.

  The spec settles the value question. §13 is titled "**Optional** thread cache fallback"
  and §13.1 says thread caches "exist for portability and for policy domains that cannot
  or should not use per-CPU caches" and "are **not** the preferred default on systems with
  RSEQ" — i.e. §13 *replaces* §11 on such platforms rather than stacking on it. P-003's
  fallback is already satisfied ("per-thread cache mode **or lock-sharded arena mode**" —
  the locked per-CPU baseline is the latter, and the `rseq_equivalence` battery proves the
  two paths observationally identical). §160 cites [R1] that per-CPU caches reduce cache
  blowup versus per-thread on high-thread-count systems — the spec prefers per-CPU on the
  very configuration where a thread cache might win latency. And O-002 asks that the byte
  class be *reported*, which `Stats::thread_cache_bytes` does, as `0`.

  Measurement agrees. The thing a thread cache exists to remove is the per-CPU spinlock;
  running the same `malloc(64)+free` loop per mode gave RSEQ (lock-free) 69.9–71.6 ns/op,
  `MODE_LOCKED` (per-CPU spinlock) 66.0–86.2 ns/op, and the central path 94.5 ns/op. The
  locked baseline is not measurably slower than lock-free — setting up a restartable
  critical section costs about what an uncontended test-and-set does — so there is no lock
  cost left for a third layer to remove.

  If a platform ever appears with neither RSEQ nor a per-core oracle *and* threads ≫ cores
  with heavy migration, the right response is a per-thread front end **replacing** the
  per-CPU one (§13.1's framing), built on the metadata-carved slot discipline `CpuSlot`
  already exemplifies, plus the registry/teardown/fork protocol above. The rule this
  ratifies: **an unwired implementation is not a head start; it is a claim the tree cannot
  honour.**

* **W6 — wiring a layer makes every "not yet" comment about it a claim to re-check
  (0.4.0).**
  Deleting the thread cache surfaced two things the *absence* of a front end had been
  quietly excusing. `TOPO_TCACHE_NONE` ("bypass local caches", §10.3) decoded into
  `RequestFlags::CACHE_BYPASS` and had **zero consumers** — harmless while there was no
  cache to bypass, a broken ABI promise the moment there was one. It is now honoured on
  `topo_mallocx` (served from central, not a slot) *and* on `topo_dallocx`/`topo_sdallocx`
  (returned straight to central), through a shared free body so the flagged and unflagged
  paths cannot drift in their `errno` handling. And the §31.2 `BY_CPU` stats flag rendered
  a hard-coded `[]` with the comment "front-end per-CPU caches land at M2" — true until
  they landed; it now renders the real per-core residency and is empty exactly when the
  front end is.

  Neither was reachable by any test, because both were *correct no-ops* under the old
  state of the tree. The rule: when a subsystem goes live, every comment that deferred to
  it is a claim that has just changed truth value, and grepping for those deferrals is
  part of landing the subsystem — not cleanup for later.

* **CI — a standalone workspace that nothing builds will break and stay broken (0.4.0).**
  The `fuzz/` workspace is excluded from the main one (so `cargo test` does not drag in
  `libfuzzer-sys`), which also meant no `xtask ci` step ever compiled it. A signature
  change in 0.3.0 (`topo_stats::redact_summary` gaining an `observer_label` parameter)
  left two fuzz targets uncompilable through a release with every other gate green. `ci`
  now runs `cargo check --manifest-path fuzz/Cargo.toml --all-targets` — cheap, no nightly
  needed, and it does not *run* the fuzzers (a campaign, not a gate). The general form:
  **excluding something from the build graph excludes it from the guarantees**, so
  anything deliberately excluded needs its own explicit gate.

* **ABI — a handle type with no API is a frozen guess, not a reserved name (0.4.0).**
  `typedef uint32_t topo_tcache_t` sat in the public header with no function accepting
  it, above a comment saying "the encoding is deferred to its subsystem rather than
  frozen as a guess" — while `uint32_t` *is* a frozen guess at the width, and
  `tests/c/abi_smoke.c` pinned it with a `_Static_assert`, so CI enforced a guess the
  header disclaimed. The same sentence cited `TOPO_TCACHE(id)`/`TOPO_NUMA(node)` as the
  precedent for deferring, and those are deferred by being *absent*: identical deferrals
  treated two different ways, one paragraph apart.

  §10.3 does not require it. Its block is a **SHOULD** whose "naming is illustrative" and
  whose conformance requirement is "equivalent **functionality**" — and the tree already
  reads it that way, since `topo_arena_purge` and `topo_arena_set_decay` from the same
  block are not exposed either. Nor would the type be forward-compatible: a handle names
  an *explicit* cache to route through, and a front end keyed by CPU has nothing for a
  caller-held handle to name, so whatever such a subsystem eventually needed (a registered
  id, an opaque pointer) is as likely to differ from `uint32_t` as to match it. Removed,
  with the header stating why the absence is deliberate. `TOPO_TCACHE_NONE` — *declining*
  the cache, which needs no handle — is supported and honoured.

  The general rule, the same one that retired the thread-cache prototype: **reserving a
  name you cannot yet design is not forward compatibility, it is a claim with a version
  number attached.** Adding a type when its API lands costs nothing; carrying one that
  documents an API that does not exist costs a reader's trust in the rest of the header.

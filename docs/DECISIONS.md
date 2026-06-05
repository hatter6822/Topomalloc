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
  already-vended bytes stay valid; `Bootstrap::global()` binds a `BOOTSTRAP_REGION_BYTES`
  static reserve lazily (DD-2's "static reservation"). A debug **re-entrancy guard**
  (`is_in_alloc()`, a thread-local under `std`/tests, a no-op leaf otherwise) traps the
  S-007 bug of the metadata path re-entering the allocator. The arena is **never** freed
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
  (low 3 bits; descriptors are ≥ 8-aligned): `Empty` = 0 (a zeroed leaf is valid),
  `Small`/`Large`, `ReleasedRetained` (P-Map-005). Chosen over a flat array (wastes
  virtual space) and a hash map (worst-case/resize hazards), DD-1. `metadata_bytes()`
  reports the bounded overhead.

* **Publish/read with release/acquire + a single lock-free mutator (W3-6).** Leaf
  entries are release-stored *after* the descriptor is initialized; readers
  acquire-load; new nodes are zeroed before a release-CAS links them — so a classifier
  sees `Empty` or a fully-formed entry, never a torn node (F1). The pagemap is the
  **only** mutator (F3): W4-2b and W5-5 route through
  `install_span`/`release_span`/`retire_span`/`install_large` (guarded in debug against
  a double-install over a different descriptor and a non-page-aligned span base). Its
  operations are lock-free (atomic publish), so they acquire no lock and cannot violate
  the §27.2 hierarchy; they run inside the caller's span critical section (P-Map-006).

* **Generation for identity + a seqlock for read-consistency (W3-5/W3-4).** Span/large
  descriptors carry a `generation` bumped on recycle (§16.6/§27.5); `GenGuard` lets a
  stashed reference detect a recycle (ABA), and descriptors are never freed, so the
  pointer stays valid. Separately, a **seqlock** version brackets a recycle's geometry
  writes, and `classify_ptr` reads the geometry through it — so a classification racing
  a recycle (a use-after-free signal) is never composed from two incarnations; a
  persistently racing recycle resolves conservatively to `External`. An integrity tag
  (§17.3, FNV-1a over the read-mostly header) lets debug/hardened detect a corrupted
  descriptor.

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

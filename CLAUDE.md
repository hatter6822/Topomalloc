# CLAUDE.md — TopoMalloc Project Guidance

This document serves as the engineering manual for TopoMalloc, a safety-first, formally-grounded, capability-aware general-purpose memory allocator. It establishes conventions, build procedures, and development workflows rather than design specifications (which belong in `planning/SPEC.md` and `planning/plans/README.md`).

## Core Project Summary

TopoMalloc is a general-purpose memory allocator combining per-CPU caching, topology-aware transfer layers, jemalloc-style policy arenas, Temeraire-style hugepage-aware backing, rigorous observability, a Lean 4 formal model, and a required seLe4n/seL4-style microkernel integration profile. The Rust core is `no_std`-capable on the hot path, with POSIX and the [seLe4n](https://github.com/hatter6822/seLe4n) capability microkernel co-equal behind one backing-provider seam.

**Current Status:** M0 closed; M1 (central-path allocator) under way. The public API runs over the real central-path allocator: classify → central free lists / extent-backed large path, with genuine `free`/`realloc`/`malloc_usable_size`, errno semantics, C23 sized frees, the extended `topo_*x` API, opt-in C++ operators, and the Rust `GlobalAlloc` adapter — identical over POSIX and the seLe4n simulator (G-sim). The full §25 realloc state machine (W15) is complete: move with failure-preserves-the-original (allocate-before-free, arena preserved), same-class/within-extent in-place grow, and in-place **shrink** that splits off and returns a medium/large allocation's tail pages to the backend across a page boundary (best-effort under always-correct semantics; cache-served/exhausted-split keep the allocation whole), plus aligned-allocation validation and calloc multiply+rounding overflow guards with full-usable zeroing. Capability-backed arenas (W9) ride on top: a live multi-arena data path with the full §22/§36.4/§36.13 lifecycle (create/delegate/reset/destroy/revocation), per-arena isolation, quota/authority/label enforcement, NUMA policy modes, and a C arena API (`topo_arena_create/delegate/reset/destroy`). Extent hooks & custom backing (W10) ride on the same seam: the §23.2 `ExtentHooks` interface + `HookProvider` adapter run the whole central path over a user-supplied backing, with §23.3 contracts enforced and the §23.4 conditional-correctness assumption modeled in Lean. The hugepage-aware backend (W11) rides on the §18.6 region-cache seam: the four §19.2 components (HugeAllocator / HugeCache / HugePageFiller / RegionCache) as a real, backend-agnostic placement subsystem over the provider seam — nine §19.4 occupancy bins (each hugepage in exactly one, H-003), packing-scored placement (§19.3), partial subrelease guarded by H-005, §19.7 coverage metrics in stats, and the §19.8 H-001..H-005 invariants checked in debug — wired into the live large path through the existing `RegionCacheHook`, identical over POSIX and the seLe4n simulator. The release controller (W12), live NUMA topology router (W13), and the lifetime/hotness/site-profile **placement policy** (W14) — fed live by a **minimal, off-by-default heap-sampling slice** (W17-3: lock-free per-thread Poisson decision, alloc-free `libc::backtrace` capture, right-censored sampled-object lifecycle, all behind a re-entrancy guard) — ride on top, the placement policy upholding the §24.5 safety boundary (it changes locality, never size/alignment/validity) by construction. Front-end caches (M2) and the remaining M1 pieces land per the plan.

## Essential Build Commands

```bash
cargo xtask setup                     # install the pinned Rust + Lean toolchains and cross targets (idempotent)
cargo xtask ci                        # the exact sequence CI runs: fmt, lint, gen-check, build, test, lean
cargo xtask build [--target T] [--profile debug|performance]
cargo xtask gen [--check]             # regenerate / verify the size-class tables (G-table)
cargo xtask test [--kind unit|prop|diff|fuzz|loom|tsan|rseq]
cargo xtask fmt --check               # rustfmt gate
cargo xtask lint                      # clippy -D warnings + SPDX + Lean style + obligation citations (V-004) + license boundary + markdownlint + shellcheck + deny
cargo xtask lean [--check]            # build the Lean package and run `lake exe check`
cargo xtask bench                     # criterion micro-benchmarks (non-gating)
cargo xtask abi-test                  # compile + link + run the C/C++ ABI harness (§34.1)
cargo xtask doc                       # build docs with -D warnings (broken-link check)
cargo xtask deny                      # cargo-deny: licenses + advisories + bans
```

`cargo xtask` is the **single entry point** developers and CI both use, so a build is never "Rust only". A fresh clone is green with just `cargo xtask setup && cargo xtask ci`. If the Lean toolchain is not installed, the Lean steps are skipped locally with a clear notice; CI always runs them.

The kernel has **zero external Lean-package dependencies** beyond Lean core. Rust third-party deps are limited to `libc` (POSIX syscalls), `criterion` (benches, dev-only), `loom` (model-checking, dev-only, cfg-gated), and `proptest` (property tests, dev-only). `cargo-deny` enforces the license allow-list.

## Absolute Governance Rules

**Safety before policy (ABSOLUTE, SPEC §2.4):** A change may ship a dumb-but-correct policy; it may never ship an unsafe fast path. Every PR keeps the unconditional safety invariants (S-001..S-010) green.

**Never hand-edit generated tables (ABSOLUTE, DD-1):** The size-class table is generated by `tools/size-class-gen` from the committed golden `tools/size-class-gen/size-classes.json`. **Never hand-edit** `crates/topo-core/src/generated/tables.rs`, `include/topomalloc_tables.h`, or `lean/TopoMalloc/Generated/SizeClasses.lean`. Edit the golden and run `cargo xtask gen`; CI fails (G-table) on any drift.

**No `sorry` in proofs (ABSOLUTE):** The Lean model contains no `sorry`, no `admit`, no `native_decide`. The only postulated axioms are the four §33.5 RSEQ primitives/contracts (the trusted hardware boundary). Every §33.4/§36.17 theorem rests only on Lean's standard axioms (`propext`/`Quot.sound`/`Classical.choice`).

**Formal model in lockstep (ABSOLUTE, SPEC §33, V-004):** A change that adds or alters an abstract state-machine transition updates the Lean model in the same change, or records a tracked `V-004` refinement debt. A claim that a change carries **no** formal obligation ("policy, not safety", "no Lean obligation", "sequences certified mechanisms") MUST cite a concrete backing artifact **in the same comment block** — a named Lean theorem (the "sequences certified transitions" pattern, e.g. `realloc_shrink_inplace_tail_tiles_disjointly`) or a fixed-wall safety test (the "pure policy" pattern, e.g. `placement_never_breaks_the_allocation_contract`) — never a bare assertion. This is gated by `cargo xtask lint` (`obligation citations (V-004)`) and detailed in `docs/CONVENTIONS.md` §8.

**SPDX headers (ABSOLUTE, D5):** Every source file starts with an `SPDX-License-Identifier` header. Core files use `MIT`; seLe4n-integration files (`crates/topo-backend-sele4n`, `lean/TopoMalloc/SeLe4n/`, `sele4n/`) use `GPL-3.0-or-later`. CI enforces this (`cargo xtask lint`). Vendored code under `vendor/` is exempt (carries upstream headers).

**`unsafe` discipline (ABSOLUTE, W0-10):** Every `unsafe` block and `unsafe impl` carries a `// SAFETY:` comment stating the invariant that makes it sound (enforced by `clippy::undocumented_unsafe_blocks`). `topo-core` sets `#![forbid(unsafe_op_in_unsafe_fn)]`; `unsafe fn`s spell out their `unsafe` blocks explicitly. Inline assembly is confined to the RSEQ sequences (`topo-arch/src/rseq/seq_*.rs`).

**Transition tagging (ABSOLUTE, M-001):** Every function that implements an abstract state-machine transition is tagged with a doc comment of the exact form `SPEC-transition: <name> (§ref)`, mapping code to model.

**No new anti-patterns (ABSOLUTE, Appendix F):** Error logging and profiling callbacks must not allocate through TopoMalloc (no recursion); assertion-failure paths must be allocation-free. The PR review checklist enforces this.

## Trust Assumptions

The Lean model abstracts four boundaries through postulated axioms (preserving `#print axioms` purity):

1. **RSEQ hardware contract (§33.5)** — the four RSEQ primitives/contracts model the trusted hardware boundary. The always-`abort` interpretation models them conservatively. Success pins the successor to the exact owner relabel; abort leaves allocator-visible state unchanged.
2. **Backing-provider seam (§36.6)** — POSIX (`mmap`/`madvise`/`mprotect`) and seLe4n (capability-typed frames) are co-equal behind one trait; the model abstracts the OS/kernel interface.

## Toolchains (pinned)

* **Rust** — `rust-toolchain.toml` pins `1.94.1` stable, with `rustfmt`, `clippy`, `rust-src` components and `x86_64-unknown-linux-gnu` + `aarch64-unknown-linux-gnu` cross targets. We do **not** use nightly for the allocator core; the nightly-only *tools* — `cargo-fuzz` and ThreadSanitizer — are opt-in (`cargo xtask test --kind fuzz|tsan`).
* **Lean** — `lean-toolchain` pins `leanprover/lean4:v4.28.0` — deliberately the same Lean version upstream seLe4n uses, so the Lean bridge never suffers toolchain skew. `cargo xtask setup` installs it via `scripts/setup_lean.sh`, which downloads the toolchain with SHA-256 verification.

## File Editing Protocol

**For existing files:** Use the Edit tool with precise `old_string`/`new_string` pairs, regardless of file size. The Write tool replaces entire files and is error-prone for files over ~100 lines.

**For large changes:**
- Read the target region first so `old_string` matches exactly
- One logical change per Edit call
- Break large additions into multiple sequential Edit calls, anchoring each to existing context
- Spot-check the modified region and file tail after changes

**For large search output:** Cap with `head_limit`; use `output_mode: "files_with_matches"` first, then drill in.

## Background-Agent File Protection

Background agents run concurrently and may finish after foreground modifications:

1. Never delegate file writes to background agents for files you may edit
2. Partition files strictly across parallel agents
3. Use background agents only for read-only or independent-file tasks
4. Check background results before acting on shared state

## Naming and Style Conventions

- **Rust types/traits:** `CamelCase` (e.g., `TopoBackingProvider`, `SpanDescriptor`, `ExtentRef`)
- **Rust functions/variables:** `snake_case` (e.g., `classify_request`, `align_up`)
- **Lean theorems/lemmas:** `snake_case` (e.g., `malloc_preserves_wf`, `size_class_table_covers_all_small_requests`)
- **Lean structures/types:** `CamelCase` (e.g., `State`, `WellFormed`, `SizeClassRow`)
- **Lean state variables:** `s`, `s'`; hypotheses: `h`-prefixed (`hpre`, `hwf`)
- **Lean namespaces:** `TopoMalloc`, `TopoMalloc.Theorems`, `TopoMalloc.SeLe4n`
- **Lean proof style:** Prefer tactic mode (`by …`) for non-trivial proofs; use `calc` for equational chains
- **Crate names:** `topo-<domain>` (e.g., `topo-core`, `topo-abi`, `topo-backend-posix`)
- **IDs:** typed newtype wrappers: `ArenaId`, `SizeClassId`, `SpanId`, `LargeId`, `Label`, `Generation`
- **`assert!` vs `debug_assert!`:** `assert!` for cheap, load-bearing safety checks; `debug_assert!` for expensive Appendix-B invariant checks
- **Error handling:** `Option`/`Result` on the hot path, never panics on ordinary failure (OOM, overflow, bad request). Panics reserved for impossible internal invariants.
- **Generated code:** Carries an `@generated` banner; never hand-edited (DD-1)
- **Formatting:** `rustfmt.toml` — edition 2021, max_width 100, Unix newlines

## Documentation Requirements

Public items are documented (`missing_docs` is a warning, denied in CI). `rustfmt` and `clippy -D warnings` are CI gates.

When changing behavior or transitions, update in the same PR:
1. `planning/SPEC.md` (if affecting the specification)
2. `lean/` (if adding/changing a state-machine transition — or file a `V-004` debt)
3. `README.md` (if affecting project status, build commands, or quickstart)
4. `CLAUDE.md` and `CONTRIBUTING.md` (if affecting conventions, build commands, or status)
5. `docs/CONVENTIONS.md` (if affecting coding standards)
6. `docs/DECISIONS.md` (if ratifying a new decision)

**Don't extend audit narratives in this file.** Completion details belong in commit messages and PR descriptions.

## Version Bumping (DEFAULT)

Each pull request bumps the patch component (semver) unless explicitly stated otherwise:

```toml
# Cargo.toml [workspace.package]
version = "0.1.0"
```

Mechanics: use patch (default) for bug fixes / refactors / tests; minor for new backwards-compatible functionality; major for breaking changes.

## Pull Request Authoring Policy (ABSOLUTE)

**Forbidden in PR descriptions/bodies:** Session URLs of the form `https://claude.ai/code/session_*` or equivalent agent-harness session permalinks.

**Why:** Privacy/opacity (readers cannot open it), link rot (sessions expire), provenance leakage, citation discipline.

**Allowed alternatives:** SPEC section numbers, theorem names with file paths, workstream/plan references.

**Enforcement:** Before creating a pull request, scan the prepared body for `https?://(?:www\.)?claude\.ai/code/session_[A-Za-z0-9]+` and strip every match.

## Definition of Done (every change)

A change is **done** only when (see `planning/plans/README.md` §8):

- [ ] Builds clean on x86-64 **and** AArch64 (debug + performance); no new lint warnings.
- [ ] Unit tests for the change; **property tests** updated if API behavior changes.
- [ ] Debug **invariant checks** (Appendix B) for touched state pass; new state adds its checker.
- [ ] A transition added/changed ⇒ the **Lean model + named §33.4/§36.17 obligation** is updated and proved, or a tracked `V-004` debt is filed.
- [ ] New state ⇒ **stats (plan 07)** expose it and it reconciles (§8.6); the **trace grammar** is updated.
- [ ] A new knob/behavior ⇒ **control plane (plan 07)** + docs updated; default matches §32.3.
- [ ] The **seLe4n vertical slice** still passes over `Sele4nSim` (G-sim) for milestones ≥ M1.
- [ ] No new **Appendix-F anti-pattern** (reviewer-confirmed; see the PR template).
- [ ] SPDX header present; seLe4n-integration files under `GPL-3.0-or-later` (D5).
- [ ] Transitions tagged with their SPEC state-machine name (M-001).

## Current Development Status

**Milestone:** M0 closed; M1 (central-path allocator) under way. M2 (front-end caches) is next — its
**concurrency foundation (W16: lock hierarchy, fork, TLS, init phases) is landed** (see below).
Reallocation, aligned allocation & calloc zeroing (W15) is **complete and optimal** (all units, no
deferrals): the §25 realloc state machine — `realloc(NULL,n)`/`realloc(p,0)` policy, content
preservation, failure-preserves-the-original via the always-correct move path (§25.4, arena preserved,
sampled as a realloc), in-place **grow** (§25.2, W15-3a) — same-class/within-extent *and* **extent-merge
grow** that absorbs the address-adjacent free extent (no copy; `ExtentManager::grow_in_place`), and
in-place **shrink** (§25.3, W15-3b) that returns a medium/large allocation's tail pages to the backend:
the extent path splits off the tail (`split_tail` → `retire_large_range` → `shrink_usable` → free), the
**cache-served hugepage path trims the tail in the filler** (`HugePageFiller::trim` via the
`RegionCacheHook::try_trim` seam), both with exact `live_bytes`/arena-quota accounting (§8.6/§36.17). The
dedicated in-place-resize API `xallocx` shares this via `Allocator::resize_in_place` (shrink + grow, never
move). calloc zeroing (§26) elides the redundant `memset` when the backing is **freshly OS-zeroed**
(`TopoBackingProvider::committed_memory_is_zeroed`, POSIX `true`), guarded by a debug check; it re-zeroes a
recycled extent. **Aligned classes (W15-4):** the generated size-class table records each class's
**natural alignment** (largest power of two dividing its size, capped at the page-aligned slab base), so a
cache-line-aligned (or any power-of-two ≤ page) small request is served from a **slab slot, not a page**
(via the already-proven over-alignment walk; `MAX_ALIGN` is now the page size). `valloc`/`pvalloc` round
out the §10.1 surface. A large allocation is modeled in the **extent backend**, not as a core `Block`
(clause 12 keys every block to a size class), so the in-place grow/shrink **sequence the certified extent
transitions** `extent_split`/`extent_merge` (§18.3) + `extent free` (§20.1) — the W12 "sequence certified
backend mechanisms" pattern, **not** `reallocMove` (whose copy window needs two simultaneously-live
disjoint ranges that two same-base ranges can never be). They add no new §33.4 obligation; the geometric
premises are pinned by `realloc_shrink_inplace_tail_tiles_disjointly` (via `span_split_preserves_disjointness`)
and `realloc_grow_inplace_absorbs_disjointly` (via `span_merge_preserves_disjointness`), and aligned
classes need no proof change (the over-alignment walk + `maxAlign` bound were already proven, re-verified by
the G-table/`maxAlignOkB` gates).
Capability-backed arenas (W9) are implemented ahead of their M4 slot: the full §22/§36.4/§36.13
lifecycle (create / delegate / reset / destroy / revocation), a live multi-arena data path with
per-arena isolation (§22.7), quota / authority / label enforcement, and NUMA policy modes (§15.5).
Extent hooks & custom backing (W10) are implemented ahead of their M4 slot too: the §23.2
`ExtentHooks` interface + the `HookProvider` backing-seam adapter (six physical ops + the `split`/
`merge` advisory notifications), §23.3 contract enforcement (alignment/size/subrange + no-overlap/
dealloc-pairing + reentrancy), §34.8 hook failure injection, the §23.4 conditional-correctness Lean
model, **the full §22.2/§22.4 per-arena `hooks` descriptor field** (`arena_create_hooked` — an arena
served from its own `HookProvider`-backed region, §22.7-isolated, with **O(1)** routing), the **C
`topo_extent_hooks_t` ABI**, and **per-arena + global hook-failure observability** (`ArenaStats::hooks`
/ `AllocatorStats::hook_failures` / stats JSON).

The hugepage-aware backend (W11) is implemented ahead of its M5 slot, in `crates/topo-core/src/huge.rs`:
the pure, single-threaded `HugePageFiller` (per-hugepage live/committed/released page bitmaps + the
nine §19.4 bin lists) and the provider-driven `HugePageBackend` that wraps it behind the §27.2 backend
lock, drives `revoke`/`commit`/`decommit`, and implements the §18.6 `RegionCacheHook` the large path
consults. It covers W11-1a (HugeAllocator: hugepage-aligned reservations), W11-1b (HugeCache:
empty-backed reuse + a `release_empty_excess` demand-reserve hook for W12), W11-2a/b (nine bins +
packing-ordered scored placement carrying hotness **and** lifetime hints, §19.3/§19.5, no full scan),
W11-2c (B.4 bin↔occupancy oracle), W11-3 (validating RegionCache for awkward sizes — bounded waste, no
double-vend), W11-4a (packing — same-lifetime segregation by opening a fresh hugepage on a strong
mismatch, with the §19.4 per-bin distribution surfaced in stats), W11-4b (H-005-guarded partial
subrelease with a cold/sparse/pressure gate, a real §19.6 cost/benefit gate, §36.6
revoke-before-decommit, and the W12 `mark_cold` hook),
W11-5 (§19.7 coverage metrics + the §19.4 `bin_counts` distribution → `topo-stats` JSON), and W11-6 (backend-agnostic: a §36.9 G-sim slice
proves the *identical* outcome over POSIX and `Sele4nSim`). It is wired **live** through the engine:
`Allocator::new_with_huge` (the `hugepage_optimized` configuration) routes every medium/large
allocation through the filler — carrying the request's hotness/lifetime hints — with the small/free
paths unchanged; the default `Allocator::new` keeps the M1 extent path. Under the `hugepage-optimized`
feature, `topo-abi`'s `build_posix_allocator` serves the live C `malloc`/`free` over a
`HugePageBackend`-backed engine, gated so the default MIT artifact is byte-for-byte the extent path. The `huge::classify_bin`
(§19.4) is pinned to the Lean model by `huge_bin_classification_matches_lean` and the `lake exe check`
`hugeBinGate`, and the filler's place/free/subrelease are modeled as a per-page state machine with
H-001/H-004/H-005 preservation proved in `lean/TopoMalloc/HugePageFiller.lean`.

The memory release controller & background-purge pump (W12) is implemented ahead of its M5
slot, in `crates/topo-core/src/release.rs`: a **pure, `no_std`, host-driven** policy object —
`ReleaseController::tick(now_ms, inputs) -> ReleasePlan` — that decides when and how much unused
memory returns to the OS (§20–§21). It covers the §20.2 decay config (W12-1a, consolidated onto
`arena::DecayConfig`), the §21.2 observation vector (W12-2a), the §21.5 Normal/Soft/Hard/Emergency
pressure modes with hysteresis (W12-3a, alloc-failure/cgroup-critical force Emergency, O-007), the
§21.4 demand-reserve anti-oscillation brake (W12-2c — capped at the §21.4 `recent_peak` of
releasable-free memory, a *leaky peak-hold* relaxing the peak anchor toward current free over
`PEAK_DECAY_MS`, decayed by the anchor's age so it is tick-cadence independent, so a transient free spike
never pins the cap high and over-retains RSS), the §21.3 priority ladder gated by
mode and the §36.11 latency ceiling (W12-2b) — drain caches → release empty hugepages → purge
aged dirty → convert aged dirty→muzzy → subrelease cold-sparse → release aged muzzy
(`muzzy_decay_ms`) → emergency shrink, with dirty/muzzy each retained until their decay interval —
the background-purge pump with decay-timer gating /
CPU-pressure yield / fair multi-arena round-robin (W12-1b — rate-capped per §20.2, the unmet remainder
held as a backlog that is the *max* of carried-vs-current desire, never their sum, so a rate-capped
persistent supply cannot make the backlog diverge), a heap-independent emergency reserve
(W12-3b), and the `LatencyClass` arena flag (W12-4, `ArenaPolicy::latency`). It is wired **live**
through `HugePageBackend::release_tick`, which drives the W11 `release_empty_excess` demand-reserve
hook from the plan — the exact W11→W12 handoff — identical over POSIX and the seLe4n simulator
(§36.9 G-sim slice). The controller **adds no abstract transition**: it sequences mechanisms already
certified by the §21.6 release-safety theorem (`release_to_os_preserves_live_objects`,
`lean/TopoMalloc/Theorems/Release.lean`), so there is no new Lean obligation. Its running counters
(pressure mode, backlog, demand reserve, planned bytes) reconcile into `topo-stats` JSON and the
`topo.release.*` control namespace.

Topology awareness (W13) completes plan 04 and is **live**. The §15.2 `Topology` snapshot
(`crates/topo-core/src/topology.rs`, pure/`no_std`; CPU→LLC→NUMA maps + a node-distance matrix, all
queries total) is built by a `TopologyBuilder` that **falls back to a conservative single domain** on
any inconsistency and **densely renumbers the OS node ids — and the LLC-domain ids — in use** (so a
sparse platform has no phantom node *or* LLC domain; the raw OS node id is kept in `os_node_of` for
`mbind`) — W13-1. `preferred_node`/`preferred_node_at`
is the §15.3/§15.5 placement decision over `NumaPolicy` (Local / Bind / Interleave / OsDefault /
ArenaPolicy, W13-2); the §15.4 `Rebalancer` plans a nearest-donor → most-pressured move that strands
no one — moving only a donor's **movable surplus** (`free − own demand`, via
`NodePressure::movable_surplus`/`unmet_need`), so a move can never strand the donor or churn when no
node has spare memory (W13-3); `detect_mismatch` is the §15.2 refresh probe (W13-4).
`topo-backend-posix::discover_topology` parses Linux sysfs with the single-domain fallback;
`PosixBackingProvider::bind_node` is the best-effort Linux `mbind(MPOL_PREFERRED)` (no-op elsewhere); and
`OsCore` is the real `sched_getcpu` current-CPU oracle. The **`NodeRouter`**
(`crates/topo-core/src/node_router.rs`) makes it all *live*: one `mbind`-bound `HugePageBackend` per node
(a fixed `[…; MAX_NODES]` array — `no_std`) serving explicit Local/Bind/Interleave, **plus an unbound
default backend** serving `OsDefault`/`ArenaPolicy` so the kernel places those pages **first-touch** (on
the using thread's node — never pinning the common case to node 0). `Local` tracks the real running CPU
(`OsCore`), `Interleave` round-robins over the backend count, a full node spills to the **nearest** other
node, frees route home by address, and `rebalance_tick`/`release_idle`/`refresh` are host-driven. It is
installed into the `hugepage-optimized` ABI via the existing `new_with_huge(&dyn RegionCacheHook)` seam —
**the default extent path and a single-node host are byte-for-byte unchanged**. The host drives the §15.4
rebalancer, §15.2 refresh, and W12 idle-release through the C `topomalloc_numa_*` control surface (a
type-erased `RouterControl` handle). Placement/rebalancing are policy, not modeled transitions (§2.4), so
there is no Lean obligation. The router's §15.4/§15.5 counters (driven bind failures, rebalancer
moves/bytes, spillovers) + the node/LLC counts reconcile into `topo-stats` JSON and the `topo.numa.*`
control namespace. A first-class per-node *demand* signal (the rebalancer uses an alloc-failure
approximation) and LLC-domain placement (transfer caches, M2) remain the deferred pieces.

Lifetime, hotness & placement policy (W14) completes plan 07's placement track ahead of its M6 slot, in
`crates/topo-core/src/placement.rs`: the six §24.2 `LifetimeClass`es, the §24.4 `AllocationSiteProfile`
record (stack id, a bounded Space-Saving `SizeClassDist`, a right-censored `LifetimeHistogram`, mean
hotness, alloc/free rates, `sampled_live_bytes`, per-dimension + combined confidence), and the
`SiteProfileTable` learning policy — a **pure, `no_std`, host-driven** object (the W12 `ReleaseController`
pattern) whose `place_hints` distils a *confident* profile into the advisory `PlaceHints` (hotness +
lifetime) the placement layers group by (§24.6–§24.8). The **learn → place loop is closed live**: confident,
consistent per-bucket consensus is published into a lock-free `LearnedHints` table the allocation path reads
(one atomic load; `Allocator::{publish_learned_hints,learned_hints}`), so a *placement-unhinted* request
adopts its site's learned profile — an explicit hint always winning, and the placement
unchanged when nothing is learned. Grouping acts at **two layers**: the W11 hugepage filler (medium/large:
cold → cold/sparse, same-lifetime via open-fresh-on-mismatch, long+hot → hot-dense), and §24.6/§24.7
`PlaceClass`-tagged **span pools** for small objects (a class-preferring `CentralCache::remove_batch` with an
`ANY_PLACE_CLASS` availability fallback, so grouping never causes a spurious OOM, §2.4; an all-default
program keeps one pool per size class). The profile is recency-aware (event-driven EWMA rates, a MAD-stability
-gated hotness) over a 16-way set-associative table. W14-1 reuses the existing W11 hint plumbing. The single
non-negotiable, the §24.5 safety boundary, holds **by construction** — the policy's only output is
score-only `PlaceHints`, so a missing/wrong/adversarial profile can change *where* an object lands but
never its size/alignment/validity/free path — and is pinned by the fixed-wall test
(`engine_size_align_validity_free_are_invariant_under_hints` + pure-filler + proptest + fuzz companions,
re-proved over `Sele4nSim` by a §36.9 G-sim test).
The **minimal W17-3 sampling slice** landed alongside to feed the policy live (`crates/topo-core/src/sampling.rs`
+ the `topo-abi` glue): a lock-free per-thread Poisson `Sampler` (W17-3a, fixed-point exponential, FP-free
core), an allocation-free `libc::backtrace` capture into a fixed `StackBuf` (W17-3b, warmed up at enable),
a `SampleBloom`-gated `SampledObjects` lifecycle with right-censored lifetimes (W17-3c, the free hot path
stays lock-free), and `SiteProfileTable` as the aggregator (W17-3d) with a `topomalloc_profile_dump_json`
dump. Sampling is wired into `AnyAllocator::{allocate,free,realloc}`, **off by default** (one atomic
load on the hot path), enabled by `$TOPOMALLOC_SAMPLE_RATE` / `topomalloc_profile_set_rate`, and a
thread-local re-entrancy guard keeps the sampler from re-entering the allocator (§31.4) — proved by the
`sampler_no_alloc` test (a counting `#[global_allocator]` shows the sampled path makes **zero** heap
allocations across 50k samples), with a sampling-overhead criterion bench bounding the hot-path cost and the
membership filter auto-refreshing to cap its false-positive rate. Placement is
policy, not a modeled transition (§2.4, as for W13), so there is **no Lean obligation and no trace-grammar
change**; the profiler's counters reconcile into `topo-stats` JSON (`placement` block) and the
`topo.placement.*` control namespace (profiling estimates, outside the §8.6 byte reconciliation). The full
W17 stats core / epoch snapshot / flags / redaction / `explain` remain for M6.

**Formal-obligation hardening (W12/W13/W14 review).** A review of the "policy, not safety / no
Lean obligation" claims confirmed each is sound but tightened how it is *backed*. W15-3b's in-place
shrink now cites the `realloc_shrink_inplace_tail_tiles_disjointly` theorem (the W12 "sequence
certified backend mechanisms" pattern — not the W13/W14 "policy invisible to abstract state" one).
W13's §2.4 boundary is now pinned by the fixed-wall `placement_never_breaks_the_allocation_contract`
(size/alignment/validity/free-home invariant under every NUMA policy + a bind failure — the analogue
of W14's `engine_size_align_validity_free_are_invariant_under_hints`), and W12 by the end-to-end
`controller_driven_release_preserves_live_objects`. A new `cargo xtask lint` gate (`obligation
citations (V-004)`, `docs/CONVENTIONS.md` §8) makes a bare "no Lean obligation" claim — one without a
cited theorem or fixed-wall test in the same comment block — fail CI, so the gap cannot recur.

Concurrency, memory ordering, fork, signal & TLS (W16) completes plan 05's concurrency track ahead of
its M2 slot. The §27.2 lock hierarchy is a **ranked-lock total order** (`crates/topo-core/src/lock.rs`):
`RankedLock<const RANK: u8>` is the single lock primitive, and **every** `topo-core` lock — the per-CPU
front-end lock (rank `FRONT_END`, its byte still at offset 0 for the RSEQ asm), transfer/central/per-span
locks, the span/large descriptor pools, the extent/huge backends, and the arena registries — is one, so a
**per-thread held-rank checker** (allocation-free `const`-init TLS) asserts every acquisition is strictly
rank-increasing and fails any out-of-order acquire (**G-conc**). The checker + the S-007/hook re-entrancy
guards are **active in the real artifact** — `topo-abi`/`topo-tests` enable `topo-core/std`, so G-conc runs
across the lib + integration + ABI suites (the `no_std` kernel/seLe4n build keeps the feature off). Two
static `cargo xtask lint` gates back it: `lock hierarchy (G-conc)` forbids a hand-rolled spinlock or
unranked `Mutex`/`RwLock` outside `lock.rs`, and `atomics ordering (W16-3)` forbids off-map `SeqCst`
(§27.3 map: publication=release, consumption=acquire, transitions=acq-rel, stats=relaxed). `fork()` safety
(`crates/topo-core/src/fork.rs`) is a **per-CPU sharded read-write quiesce gate**: each shard packs a
fork-pending bit + in-flight count in one cache-line-padded word, every public operation enters by a
single `fetch_add` on its CPU's shard (the fork check rides on the returned value — no Dekker hazard, **no
`membarrier`**, loom-verifiable), the §28.1 pre-fork handler sets the bit in all shards then **drains**
them to zero (no internal lock held at `fork()` — the per-span locks are dynamic, so draining beats
"acquire every lock"), the parent resumes, and the child **resets** every shard + the lock-order checker
and disables background maintenance (the same gate quiesces it via `maintenance_guard`). The gate is
**genuinely re-entrancy-aware**: a nested entry (an arena or C `topomalloc_numa_*` control op that itself
allocates) takes **no** shard slot and skips the fork check, nesting instead on a per-thread depth (a
`const`-init Local-Exec TLS, ~1 cycle) — only the *first-level* guard the drain waits for is fork-checked,
so a nested op can never park-and-deadlock the drain (the fork is by definition draining the outer op).
The whole `topomalloc_numa_*` control surface (router + per-node backend locks, rank `BACKEND`) runs
inside the gate, so a `fork()` quiesces it too. The
`pthread_atfork` registration (installed **eagerly at load** by an ELF `.init_array` ctor — re-entrancy-safe
via a CAS guard, not a blocking `Once` — so a `fork()` racing the very first allocation is intercepted; the
lazy `global()` init is itself fork-gated so `prefork` drains-and-waits for it rather than forking a child
onto a half-built `OnceLock`) + the lock-free C `topomalloc_crash_summary` (§28.4, with an `in_flight_ops`
field) live in `crates/topo-abi/src/fork_api.rs`. The §35.4 init phases (Phase 0–6, advanced through
**each** boundary and load-bearing — maintenance declines before its phase), the `reentry_flag!` domain
(wired into the extent-hook path so a re-entrant hook's `malloc`/`free`/`realloc`/in-place-resize is
declined before any lock — not deadlocked, and never tripping the now-artifact-active checker), and the
crash summary
live in `crates/topo-core/src/init.rs`, with `INIT_PHASE` advanced through the global initializer. It is
**concurrency/operational, not an abstract §33.4 transition**, so there is **no Lean obligation**:
deadlock-freedom is pinned by the fixed-wall checker test
(`lock::tests::out_of_order_acquire_trips_the_checker`, plus `lock_order_checker_is_active_in_this_artifact`
in `topo-abi` proving the checker is live — not a silent no-op — in the real artifact), re-entrancy
deadlock-freedom by `nested_guard_during_fork_window_does_not_deadlock` (a nested guard inside an open fork
window must nest, not park; a regression hangs and a watchdog aborts), and fork-quiesce by the `loom` models
(`gate_admits_no_op_across_a_fork` + `multishard_…`, no `SeqCst`), with the fork-in-multithread battery
(`fork_safety.rs`: concurrent forkers, parent consistency, the gated `numa` control surface under
`hugepage-optimized`), the TLS **depth proof** (the `global_allocator`
example asserts the steady-state path runs at depth 1) + TLS-via-`dlopen` (`tls_dlopen.rs`), the
hook-re-entry fail-safe test, and a TSan pass over the whole `topo-core` lib. The per-op gate (~13 ns,
`benches/fork_gate.rs`) is the M2 fork-safety cost; per-CPU sharding removes the contended cacheline.

Observability: stats, telemetry & profiling (W17) completes plan 07's observability track ahead of its M6
slot. The pure renderer is `topo-stats` and the live C surface is `crates/topo-abi/src/stats_api.rs`. **W17-1a
(stats core):** the §31.1 "where is the memory?" snapshot now carries *every* byte class as a non-negative
`u64` — the §20.1 `backend.active_bytes` / `retained_bytes` distinction (Retained = the extent manager's
`reserved`, distinct from advised-away `released`), `quarantine.bytes` (present, `0` until plan 08 wires byte
accounting), and `arena.destroyed` (a real cumulative `ArenaTable::destroyed_count`, bumped at each
`*→Destroyed` and never derivable from the recycled slot states) — beside the existing app/cache/central/
hugepage/release/topology/placement blocks. **W17-1b (epoch + consistent snapshot):** a process-global
monotonic `STATS_EPOCH` stamps every composed snapshot (§8.6 "MUST include an epoch or sequence number"),
with `CONSISTENT_SNAPSHOT` the §31.2 read-mode flag; the §8.6 reconciliation convention is documented and
**pinned by a fixed-wall test** (`tests/tests/stats.rs`) — `virtual == active + pageheap_free` and
`pageheap_free == retained + dirty + muzzy + released` hold *algebraically* (even under a torn concurrent
read), and `live + central_free <= active`, `live == allocated − freed` hold at any quiescent point, proven
over a live allocator sequentially **and** under concurrent load that then quiesces. **W17-2 (snapshot/JSON/
print API + flags):** the C `topomalloc_stats_json` / `_print(FILE*)` / `_snapshot(topomalloc_stats_t*)` trio
(+ `_json_for_label`) renders a **live-composed** snapshot — allocator byte classes + the heap sampler + (under
`hugepage-optimized`) the live NUMA router's §15/§19.7 coverage via the new `NodeRouter::coverage` /
`RouterControl::coverage` — selected by `StatsFlags` (the eight §31.2 bits), with `BY_ARENA` per-arena lines
(reconciling with `arenas.count`) and `BY_SIZE_CLASS` per-class central-free (summing to `central.free_bytes`);
the JSON is additive (§35.3) and the `#[repr(C)] topomalloc_stats_t` struct + flag bits are frozen by the
two-sided ABI smoke tests. **W17-4 (fragmentation):** a `fragmentation` block — `internal_sampled_bytes` (the
§31.5 `Σ(usable − requested)` over the **live sampled set**, the sampler now recording `usable` on each
`SampledRecord` and the sum computed on demand so it is exact for the live set and `0` when sampling is off),
`external_bytes` (dirty + muzzy), `cache_bytes`, `hugepage_bytes` (§19.7), `metadata_overhead_bytes`. **W17-5
(`explain_memory`):** `topomalloc_explain_memory()` + `Stats::explain()` render "RSS is attributed to: 2.5 GiB
live, 700.0 MiB per-CPU cache, 1.4 GiB dirty retained, …" — byte classes named largest-first in integer-only
binary units, decommitted bytes noted apart as managed-VM-not-RSS, idle made explicit. **W17-6 (label-scoped
redaction, §36.12):** a pure `redact_arenas(lines, observer_label)` keeps only the arenas an observer
dominates (`label <= observer`) and the C `topomalloc_stats_json_for_label` applies it to the `BY_ARENA`
detail; pinned by `redaction_is_label_noninterference` (adding/changing any *higher*-labelled arena leaves the
low view bit-for-bit identical) — the Rust analogue of the proved Lean `stats_observation_noninterference`.
Stats are **derived observability, not an abstract §33.4 transition**, so there is **no Lean obligation**
(the reconciliation is the fixed-wall test above, the redaction the cited non-interference theorem). The new
keys reconcile into the `topo.stats.*` / `topo.backend.*` / `topo.quarantine.*` / `topo.fragmentation.*`
control namespace (W20).

**Optimal-completion pass (W17).** A self-audit hardened every "present-but-inert" piece: **W17-6** redaction
now scopes the *whole* summary a low observer receives (`redact_summary` zeroes cross-domain aggregates and
recomputes `live_bytes` from the visible arenas via `Σ arena.used == live_bytes`), so the JSON is genuinely
non-interfering — fuzzed (`stats_render`) and pinned by `summary_redaction_is_noninterference_for_the_whole_view`.
**W17-1b** `CONSISTENT_SNAPSHOT` is a real read-twice-until-stable loop; **W17-2** `RESET_PEAKS` clears a true
`peak_live_bytes` high-water (maintained at the allocation charge point), `BY_NUMA`/`BY_HUGEPAGE` render genuine
per-node (`NodeRouter::node_coverage`) / per-bin detail, an unknown flag bit is strictly rejected (§10.4), and
`topomalloc_stats_t` is the full 27-field snapshot. **W17-4** internal fragmentation is now *exact* for
medium/large (the large descriptor records each request — refreshed on every in-place resize, including a
*same-page-count* shrink/grow that moves no backing, so the figure never goes stale; a free-path-agnostic
walk sums the live waste, robust to single / arena-bulk / realloc frees) alongside the sampled small
estimate. **W17-5** `explain_memory`
reads the **real RSS** (`/proc/self/statm`), leads with it, and attributes the non-heap remainder. The only
deferrals left are narrow: true stats epoch *snapshot-isolation* (a seqlock; the read-twice loop + §8.6
bounded-skew convention cover operational debugging today) and the seLe4n resource-server-enforced per-label
backend/cache *partitioning* (the per-arena + whole-summary redaction is the complete Rust-side mechanism for
the POSIX profile).

**Test counts:**
- Rust: ~805 tests across 12 crates (`cargo test --workspace`)
- Lean: 85 build jobs including proof-checking every module (`lake build`) + 8 executable gates (`lake exe check`)
- C/C++ ABI: smoke harness (`cargo xtask abi-test`)
- Fuzzing: 10 targets (`fuzz/fuzz_targets/`, incl. `arena_api`, `extent_hooks`, `huge_filler`, `topology`, `placement`, and `stats_render`)

**Lean gates (`lake exe check`):**
- G-table: size-class table OK (72 classes, small_max=32768, huge_threshold=2097152, max_align=16384 — each class records its natural alignment so over-aligned small requests are slab-served, W15-4)
- Trace oracle OK (§33.7 replay)
- Pagemap differential OK (W3-3d)
- Provider state machine OK (§36.6, W4-1)
- Extent state machine OK (§20.1, W4-2d)
- Arena lifecycle OK (§22.3/§36.13 transitions + revocation chain; pins Rust `ArenaState`/`RevocationPhase`, W9)
- Extent-hook contracts OK (§23.3 alignment/size/subrange checks match `HookProvider`; §23.4 model, W10)
- Hugepage filler OK (§19.4 bins match `classifyBin`; H-002/H-003/H-005 model, W11)

The arena lifecycle state machine (§22.3/§36.13) is modeled in `lean/TopoMalloc/ArenaLifecycle.lean`
(proof-checked by `lake build`, mirroring the runtime `ArenaState`/`RevocationPhase`); the
capability-monotonicity, quota, and revocation theorems live in the seLe4n bridge.

## Workspace & Crate Structure

| Crate | Role | License | `no_std` |
|-------|------|---------|----------|
| `topo-core` | classifier, size classes, the backing-provider seam, metadata/pagemap, extent manager, the M1 central-path allocator, the capability-backed arena registry (W9), the extent-hook backing adapter (W10), the hugepage filler / region cache (W11), the release controller / background-purge pump (W12), the topology model / placement / rebalancer + the live NUMA `NodeRouter` (W13), the lifetime/hotness/site-profile placement policy + the heap-sampling machinery (W14 + W17-3) | MIT | Yes |
| `topo-abi` | C API (§10.1–§10.4), C23 sized free, `topo_*x` extended API, arena + `topo_extent_hooks_t` (§23.2) ABI, the `topomalloc_stats_*` / `topomalloc_explain_memory` observability surface (§31, W17) + live snapshot composer, the `topomalloc_profile_*` sampling control surface (W17-3), live sampler glue, errno, Rust `GlobalAlloc` | MIT | No |
| `topo-backend-posix` | `PosixBackingProvider` — mmap/madvise/mprotect (single-authority) + best-effort `bind_node` (Linux `mbind`, §15.5); `discover_topology` — §15.2 sysfs CPU/LLC/NUMA discovery; `OsCore` — `sched_getcpu` current-CPU oracle (W13) | MIT | No |
| `topo-backend-sele4n` | `Sele4nSim` + (M1) `Sele4nBackingProvider` over the real seLe4n ABI | GPL-3.0-or-later | No |
| `topo-arch` | per-arch RSEQ restartable sequences + fast-path mode selector | MIT | Yes |
| `topo-stats` | statistics snapshot + §31.2 flags, additive Appendix-D JSON, §31.6 `explain`, §36.12 label-scoped redaction, version wiring | MIT | Yes |
| `topo-control` | configuration sources, control namespace (Appendix E) | MIT | Yes |
| `topo-test-support` | trace grammar parser, `LiveModel` oracle, deterministic PRNG | MIT | No |

The central architectural seam: all OS/kernel interaction goes through `TopoBackingProvider` (defined in `topo-core`). POSIX and seLe4n are co-equal behind it from M1.

## The Lean Formal Model

The Lean 4 abstract state machine and proofs (SPEC §33, §36) are built with `lake` and driven by `cargo xtask lean`. Lean defines the allocator's states, well-formedness predicate, transitions, and theorems; it is **not** on the production hot path (§33.1).

### Soundness

No `sorry`, no `admit`, no `native_decide`. The only postulated axioms are the four §33.5 RSEQ primitives/contracts. Every §33.4/§36.17 theorem rests only on Lean's standard axioms. Verify with `#print axioms <thm>`.

### Core model (MIT)

| Module | Charter |
|--------|---------|
| `TopoMalloc/Types.lean` | `Range` geometry, `Owner` (all SPEC owners), IDs |
| `TopoMalloc/ArenaLifecycle.lean` | §22.3/§36.13 arena lifecycle + revocation phase machines (pins Rust `ArenaState`/`RevocationPhase`, W9) |
| `TopoMalloc/SizeClass.lean` | `SizeClassRow` predicate; `Params`/`buildTable`; §9.4/§9.5 proofs |
| `TopoMalloc/Generated/SizeClasses.lean` | **generated** tuned table (72 classes to 32 KiB) — single source (DD-1) |
| `TopoMalloc/State.lean` | abstract `State`, ownership map, `setOwner` frame primitive |
| `TopoMalloc/WellFormed.lean` | the **14** named `WellFormed` clauses + preservation |
| `TopoMalloc/Transitions.lean` | malloc/free/cache/central/release/arena as **total** functions |
| `TopoMalloc/ExtentState.lean` | §20.1 extent physical-backing state machine (pinned 1:1 to Rust) |
| `TopoMalloc/ExtentHooks.lean` | §23.4 hook assumption: §23.3 contracts ⇒ alloc/split/merge/subrange preserve disjointness (tied to the real `WfRangesDisjoint`); §22.7 per-arena-region isolation; the `hookContractGate` decidable checks (W10) |
| `TopoMalloc/HugePageFiller.lean` | §19.4 `classifyBin` (the nine bins, H-003 by construction); H-002 occupancy-is-sum; the filler as a per-page state machine with H-001/H-004/H-005 preservation (`subrelease_preserves_live`); H-005 over the `Range` geometry; the `hugeBinGate` decidable checks (W11) |
| `TopoMalloc/Rseq.lean` | RSEQ contract — trusted primitive + frame condition (§33.5) |
| `TopoMalloc/Boundaries.lean` | trust-boundary scaffolding for the RSEQ hardware boundary |
| `TopoMalloc/Theorems/*.lean` | one file per §33.4 family (SizeClass, Malloc, Free, Realloc, Cache, Central, Span, Pagemap, PagemapExec, Release, Extent, Arena, Allocate, Demo) |
| `TopoMalloc/Exec.lean` | executable model + §33.7 text-grammar trace replay |
| `Check.lean` | `lake exe check`: G-table gate, trace-oracle gate, pagemap/provider/extent/arena differentials, the §23.3 hook-contract gate, the §19.4 hugepage-bin gate |

### seLe4n bridge (GPL-3.0-or-later)

| Module | Charter |
|--------|---------|
| `SeLe4n/CapBackedArena.lean` | capability-backed arenas, rights/quota/label attenuation (§36.4); the `ArenaTree` reservation model + tree-wide quota bound |
| `SeLe4n/UntypedProvider.lean` | §36.6 backing-provider state machine + provenance |
| `SeLe4n/VSpaceProvider.lean` / `CSpaceProvider.lean` | VSpace/CSlot provider contracts |
| `SeLe4n/Bridge.lean` | abstraction relation `R` + `TopoSeLe4nWellFormed` (§36.3.3) |
| `SeLe4n/ResourceServer.lean` | `arena_cap_authorizes_alloc`, `arena_quota_preserved` |
| `SeLe4n/ClientRuntime.lean` | `client_cache_refines_server_authority`, `per_core_cache_abort_no_change` |
| `SeLe4n/InformationFlow.lean` | non-interference: `stats_observation_noninterference`, low-equivalence |
| `SeLe4n/Refinement.lean` | **coupled** alloc/free steps, destroy revokes, label partition, provenance/release/scrub |
| `SeLe4n/SMP.lean` | **multicore** model: conservation/isolation/abort/non-interference over every interleaving |

### Headline theorems

| Property | Theorem | Module |
|----------|---------|--------|
| Size-class coverage | `size_class_table_covers_all_small_requests` | `Theorems/SizeClass.lean` |
| 14-clause `WellFormed` preservation | per-transition preservers | `Theorems/*.lean` |
| Coupled alloc preserves invariants | `allocStep_preserves_invariants` | `SeLe4n/Refinement.lean` |
| Coupled free preserves invariants | `freeStep_preserves_invariants` | `SeLe4n/Refinement.lean` |
| Exact byte accounting | `ArenaQuotaExact` | `SeLe4n/Refinement.lean` |
| Delegated subtree ≤ root quota | `subtree_used_le_quota` | `SeLe4n/CapBackedArena.lean` |
| Hooks preserve disjointness (given §23.3) | `alloc_preserves_disjoint` | `ExtentHooks.lean` |
| Per-arena hooked regions isolate (§22.7) | `perArena_disjoint_regions_isolate` | `ExtentHooks.lean` |
| Partial subrelease preserves live backing (H-005) | `subrelease_preserves_live_backing` | `HugePageFiller.lean` |
| Hugepage bin matches occupancy (H-003) | `partialSubreleased_iff_subreleased` | `HugePageFiller.lean` |
| Bundle inhabitation (non-vacuity) | `topoSeLe4nWellFormed_empty` | `SeLe4n/Refinement.lean` |
| SMP correctness | `schedule_invariant` (every interleaving) | `SeLe4n/SMP.lean` |
| RSEQ abort safety | `per_core_cache_abort_no_change` | `SeLe4n/ClientRuntime.lean` |
| Stats non-interference | `stats_observation_noninterference` | `SeLe4n/InformationFlow.lean` |

## Milestone & Conformance Map

| Conformance class | Primary workstreams | First | Full |
|--------------------|---------------------|-------|------|
| **Core** (API, ownership, metadata, safety) | W2, W3, W5, W8, W15, W16 | M1 | M4 |
| **Performance** (per-CPU, batching, hugepage, budgets, placement) | W6, W7, W11, W12, W13, W14 | M3 | M5 |
| **Formal** (Lean model, machine-checkable tables, contracts) | W1 | M0 | M7 (single-core), M9 (SMP) |
| **Operational** (stats, profiles, controls, diagnostics) | W17, W20, W12, W14 | M4 | M6 |
| **Microkernel** (seLe4n profile, **required**) | W22 + W1 bridge + W4 seam | M1 (sim) | M8 (real ABI), M9 (SMP) |

## Source Organization

```text
topomalloc/
├── crates/
│   ├── topo-core/             (no_std allocator core: classifier, seam, metadata, spans, extents, hugepage filler, placement policy + sampling)
│   ├── topo-abi/              (C/C++/Rust ABI surface: malloc, free, GlobalAlloc, stats/explain + profile/sampling control)
│   ├── topo-backend-posix/    (mmap/madvise/mprotect — the POSIX backend)
│   ├── topo-backend-sele4n/   (Sele4nSim + real seLe4n ABI — GPL-3.0-or-later)
│   ├── topo-arch/             (per-arch RSEQ assembly: x86-64, AArch64)
│   ├── topo-stats/            (statistics snapshot + flags, Appendix-D JSON, explain, label redaction, version)
│   ├── topo-control/          (config sources, control namespace)
│   └── topo-test-support/     (trace grammar, LiveModel oracle, PRNG)
├── tools/
│   ├── size-class-gen/        (the single source of truth for size-class tables)
│   └── trace-replay/          (§33.7 trace grammar replay)
├── lean/
│   ├── TopoMalloc/            (core model: Types, State, WellFormed, Transitions, Theorems/)
│   ├── TopoMalloc/SeLe4n/     (bridge model — GPL-3.0-or-later)
│   ├── TopoMalloc/Generated/  (generated size-class table for Lean)
│   └── Check.lean             (lake exe check: G-table + oracle + differentials)
├── tests/                     (cross-crate integration tests + C/C++ ABI harness)
├── fuzz/                      (cargo-fuzz targets — nightly, standalone workspace)
├── xtask/                     (the build/codegen/CI driver — dependency-free)
├── include/                   (public + generated C/C++ headers)
├── vendor/sele4n/             (pinned seLe4n ABI mirror — GPL-3.0-or-later)
├── sele4n/                    (seLe4n resource-server component — GPL-3.0-or-later)
├── bench/                     (benchmark config + results schema)
├── profiles/                  (profile definitions: features, not forks)
├── docs/                      (CONVENTIONS.md, DECISIONS.md, ABI.md, mdbook)
├── planning/
│   ├── SPEC.md                (the specification: ~100 sections)
│   └── plans/                 (overview + 10 domain plans, 24 workstreams, M0–M9)
├── scripts/                   (setup_lean.sh, vendor_sele4n.sh)
├── .claude/                   (Claude Code session hook)
└── .github/                   (CI workflows, PR template, CODEOWNERS)
```

## Licensing (D5)

Split-licensed. The standalone allocator **core is MIT** (see `LICENSE`); the **seLe4n-integration layer is GPL-3.0-or-later** (see `sele4n/LICENSE`), because it links/models the GPLv3 seLe4n ABI. The default `libtopomalloc` artifact links no GPL code and is MIT; building with the `sele4n-sim` feature produces a GPL combined work. See `NOTICE` for the full split and SPDX policy.

## Vulnerability Reporting

TopoMalloc is research-stage software. For security-sensitive bugs, see `SECURITY.md`. Run `security-review` at the close of M4, M7, M8, and on any change touching `/sele4n`, `/arch`, freelist encoding, or metadata protection.

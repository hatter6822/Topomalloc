<!-- SPDX-License-Identifier: MIT -->
# Coding standards & invariant conventions (W0-10)

These conventions are binding; the PR review checklist
([`.github/pull_request_template.md`](../.github/pull_request_template.md))
references them. They exist to keep TopoMalloc's safety-before-policy discipline
(SPEC §2.4) legible in the code.

## 1. Transition tagging (M-001)

Every function that implements an abstract state-machine transition from the
SPEC (object/span/hugepage/arena transitions, §7) is tagged with the SPEC
transition name, so a reader (and a reviewer) can map code to model:

```rust
/// Pop an object from the per-CPU cache.
/// SPEC-transition: object `FreeInLocalCache -> Live` (§7.2)
fn fe_pop(...) { ... }
```

The tag is a doc comment of the exact form `SPEC-transition: <name> (§ref)`. A
change that adds or alters a transition updates the Lean model in the same
change, or files a tracked `V-004` refinement debt (DoD, §8).

## 2. `assert!` vs `debug_assert!` — profile gating

Profiles are features, not forks (overview principle 8). Assertions follow a
strict rule keyed to the cost and the consequence of the check:

| Macro | When it runs | Use for |
|-------|--------------|---------|
| `assert!` | **all** profiles | Cheap, load-bearing safety checks whose failure means corruption is imminent (e.g. an overflow guard that must never wrap, §9.7). |
| `debug_assert!` | `debug` profile (and tests) | The Appendix-B invariant checklist and any check too expensive for the hot path (e.g. full free-list scan, bitmap/span reconciliation). |
| `debug-checks` feature | when enabled (implied by `debug`/`hardened` as configured) | Opt-in invariant checkers callable from tests and on demand from the control plane (`topomalloc_debug_check_now` / `topo.debug.check_now`). |

Never gate a *memory-safety* invariant behind `debug_assert!` if violating it in
`performance` would corrupt memory silently — promote it to `assert!` or to a
branch that fails the allocation safely. Conversely, never put an O(n) invariant
sweep on the `performance` hot path.

The Appendix-B checklist (W19-1, DD-2) is **first-class runtime code**: one *total*,
side-effect-free `check_invariants` method per type, landing with the state it checks
(`SpanDescriptor`/`CentralCache` for B.3/B.1, `PageMap` for the B.1 pagemap↔descriptor
agreement, the `CpuCache`/`TransferCache` pair for B.2, `HugePageFiller` for
B.4, `ArenaTable` for B.5), gathered and documented by invariant group in the `crate::debug`
module (the B.1–B.5 → code map, the `check_b2_cache` group callable, the `Group` enum). The
**cheap per-span B.3 check runs as a `debug_assert!` at every central transition** (the
extent/huge pattern); the **O(state) sweeps** (B.1 central reachability + the pagemap walk,
full B.2 distinctness, B.5, and the §30.2 redzone free-pattern sweep
`verify_free_patterns` under `junk-fill`) run on demand — from `Allocator::check_invariants`
(the engine aggregate, which the C `topomalloc_debug_check_now` exposes) and from tests —
kept off the per-transition hot path (DD-2 failure-mode F1). A sweep that **reads object
backing** (the redzone free-pattern check) is wired only into the engine aggregate, never
into a per-span/central checker that also runs on synthetic fake-base spans in unit tests.
The engine aggregate runs in CI under `debug-checks` over both the `topo-core` unit tests
*and* the cross-crate integration suite (`topo-tests --features debug-checks`), so the
on-demand sweeps are exercised over real end-to-end sequences. A WU that adds state adds its
checker (DoD addendum), with a **negative** test proving the checker catches a real
violation.

Corollary (Appendix F): error logging and profiling callbacks **must not**
allocate through TopoMalloc (no recursion); assertion-failure paths must be
allocation-free.

## 3. Error taxonomy

Errors are explicit and typed; the hot path returns `Option`/`Result`, never
panics on ordinary failure (OOM, overflow, bad request). The layers:

* **`topo_core::BackendError`** — failures at the backing-provider seam
  (`OutOfMemory`, `InvalidRequest`, `Unsupported`). The seLe4n backend maps
  richer `KernelError`s onto this (plan 09). Every seam op is fallible and
  leaves state well-formed on failure (§36.6).
* **C ABI** — failure is signalled the C way: a null return and `errno =
  ENOMEM` where the platform ABI specifies it (§10.1). `free` never modifies
  `errno`. Overflowing `calloc`/`reallocarray` returns null, never wraps (§26.1).
* **Overflow** — all size/alignment/page rounding goes through
  `topo_core::overflow` (checked; returns `None` on overflow). Code must not
  hand-roll rounding arithmetic.

A panic is reserved for *impossible* states (a violated internal invariant that
indicates a bug), not for user-input failures.

## 4. `unsafe` discipline

* Every `unsafe` block and `unsafe impl` carries a `// SAFETY:` comment stating
  the invariant that makes it sound (enforced by
  `clippy::undocumented_unsafe_blocks`).
* `topo-core` sets `#![forbid(unsafe_op_in_unsafe_fn)]`; `unsafe fn`s spell out
  their `unsafe` blocks explicitly.
* Raw-pointer types (`Region`) are not `PartialEq`/`Ord` by default — comparisons
  must be intentional.
* **Inline assembly is confined to the RSEQ sequences** (`topo-arch/src/rseq/seq_*.rs`,
  the only hand-written assembly in the project). Inside a restartable critical
  section there are **no calls and no possibly-faulting memory references** (§12.3);
  the `xtask` lint `check_rseq_cs` (in `lint`/`ci`) fails the build on any
  `call`/`bl`/`blr`/`svc`/`syscall` there. Every scratch register is declared as an
  `asm!` output (clobber), and the per-module "Clobbers / barriers" docs state the
  ordering each sequence relies on. Any struct an RSEQ sequence addresses is
  `#[repr(C)]` with its field offsets pinned by `offset_of!` `const` guards.

## 5. `no_std` discipline

`topo-core` and the hot-path crates are `#![no_std]`-capable
(`#![cfg_attr(not(any(test, feature = "std")), no_std)]`). They use `core`/`alloc`
only; `std` appears only behind a feature or in tests. Backends and the ABI
crate may use `std`.

## 6. Generated code

Generated files (`crates/topo-core/src/generated/`, `include/topomalloc_tables.h`,
`lean/TopoMalloc/Generated/`) are never hand-edited and carry an
`@generated` banner. Edit the golden and run `cargo xtask gen` (DD-1).

## 7. Documentation & formatting

* Public items are documented (`missing_docs` is a warning, denied in CI).
* `rustfmt` and `clippy -D warnings` are gates (`cargo xtask fmt --check`,
  `cargo xtask lint`). Markdown under `planning/` and `docs/` is markdownlinted.

## 8. Formal-obligation citations (V-004, gated)

A change that claims it carries *no* formal-model obligation — "policy, not
safety", "no Lean obligation", "adds no abstract transition", "composes /
sequences certified mechanisms" — MUST back that claim with a concrete,
auditable artifact **in the same comment block** (within a few lines of the
claim). A bare assertion is not acceptable: it is exactly how real proof work
slips by under a plausible label (the W15-3b review caught precisely this).

Two legitimate patterns, each requiring its own kind of citation:

* **Sequences certified transitions** (W12 release controller, W15-3b in-place
  shrink). The operation mutates abstract state, but only by composing
  transitions the model already certifies. Cite the **named Lean theorem(s)** it
  rests on — e.g. `release_to_os_preserves_live_objects`,
  `realloc_shrink_inplace_tail_tiles_disjointly`, `span_split_preserves_disjointness`.
* **Pure policy, invisible to abstract state** (W13 NUMA router, W14 placement).
  The decision steers locality/timing/placement and provably cannot change an
  allocation's size/alignment/validity/free path (§2.4/§24.5). Cite the
  **fixed-wall safety test** that pins the boundary — e.g.
  `placement_never_breaks_the_allocation_contract` (W13),
  `engine_size_align_validity_free_are_invariant_under_hints` (W14),
  `controller_driven_release_preserves_live_objects` (W12 end-to-end).

Do **not** conflate the two: "pure policy" is a *stronger* claim (the abstract
state never moves) than "sequences certified transitions" (it moves, but only
through proved steps). Citing the right one keeps the argument honest.

**Enforcement.** `cargo xtask lint` runs the `obligation citations (V-004)`
check: it scans `crates/**/src/**/*.rs`, and any comment block making an
obligation claim without a citation keyword (a theorem reference, or `pin`/
`certified`/`proven`/`discharged`/`fixed wall`) within `LINE_WINDOW` lines fails
the gate. The matcher joins wrapped doc-comment lines and matches citation stems
on word boundaries (so "map**pin**g" is not mistaken for a `pin` citation).

## 9. Locks, lock order & atomics (W16, §27)

* **Every lock in `topo-core` is a `lock::RankedLock<const RANK: u8>`** — the single
  lock primitive — so the debug lock-order checker (W16-1b) sees every acquisition.
  A hand-rolled spinlock (the `compare_exchange(false, true, …)` test-and-set idiom)
  anywhere outside `lock.rs` fails the `cargo xtask lint` **G-conc** gate
  (`lock hierarchy`). Pick the rank from the §27.2 total order in the `lock` module
  docs; acquisitions must be **strictly rank-increasing** (the checker `debug_assert!`s
  it, active in debug + `debug-checks`). Refill/flush stay hand-over-hand — at most one
  middle-end lock held at a time.
* **Every atomic carries a documented memory order** (§27.3): publication = `Release`,
  consumption = `Acquire`, a transition visible to concurrent free/classify = `AcqRel`
  (or a `Release`/`Acquire` pair), approximate counters/stats = `Relaxed`. The map is
  in the `lock` module docs; the `loom` models in `tests/loom_protocols.rs` machine-check
  the ordering-protocol invariants, and `cargo xtask test --kind tsan` must stay clean.
* **A thread-local on (or reachable from) an allocation path must be allocation-free**
  (W16-2 / S-007): use a `const`-initialised `thread_local!` (Local-Exec TLS, no lazy
  alloc) — never one whose initializer allocates — so a thread's first allocation cannot
  re-enter the allocator. The `reentry_flag!` macro and the lock-order checker follow
  this.
* **Public process-allocator entry points run inside `fork::operation_guard()`** so a
  `fork()` quiesces them (W16-5); a new entry point that takes internal locks must be
  gated too — including control surfaces like `topomalloc_numa_*` that take backend
  locks. The gate is **re-entrancy-aware** (a nested entry nests on a per-thread depth
  instead of parking on the fork bit), so gating an entry point that itself re-enters
  `malloc` is safe and never deadlocks the pre-fork drain. `pthread_atfork` registration
  is `topo-abi`'s job (it has `libc`); the core exposes the mechanism
  (`fork::{prefork,postfork_parent,postfork_child}`).

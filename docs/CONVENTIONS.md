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
| `debug-checks` feature | when enabled (implied by `debug`/`hardened` as configured) | Opt-in invariant checkers callable from tests and from the control plane (`topo.debug.check_now`). |

Never gate a *memory-safety* invariant behind `debug_assert!` if violating it in
`performance` would corrupt memory silently — promote it to `assert!` or to a
branch that fails the allocation safely. Conversely, never put an O(n) invariant
sweep on the `performance` hot path.

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

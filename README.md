<!-- SPDX-License-Identifier: MIT -->
# TopoMalloc

A safety-first, formally-grounded, capability-aware general-purpose memory
allocator. Rust core (`no_std`-capable hot path) + per-arch assembly + a Lean 4
model, with POSIX and the [seLe4n](https://github.com/hatter6822/seLe4n)
capability microkernel co-equal behind one backing-provider seam.

> **Status: milestone M0 — the walking skeleton.** Every tool in the pipeline is
> wired and runs end to end: the Cargo workspace, the size-class single source of
> truth, the Lean model, both backends, and the trace/replay differential spine.
> The real allocator is built milestone by milestone on top (see the plan).

## Quick start

```sh
cargo xtask setup     # install the pinned Rust + Lean toolchains and cross targets
cargo xtask ci        # build (x86-64 + AArch64), lint, gen-check, test, Lean
```

`cargo xtask` is the single entry point developers and CI both use, so a build is
never "Rust only". See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full command
list and the Definition of Done.

## Layout

```text
crates/      the Rust workspace (core, ABI, backends, arch, stats, control, test-support)
xtask/       the build/codegen/CI driver
tools/       size-class-gen (the single source of truth) + trace-replay
lean/        the Lean 4 formal model + the seLe4n bridge
include/      public + generated C headers
sele4n/      the seLe4n resource-server component (GPL-3.0-or-later)
bench/  tests/  docs/  profiles/  ci/  planning/  .claude/
```

Each top-level directory carries a one-paragraph charter README.

## Licensing (D5)

Split-licensed. The standalone allocator **core is MIT** (see [`LICENSE`](LICENSE));
the **seLe4n-integration layer is GPL-3.0-or-later** (see [`sele4n/LICENSE`](sele4n/LICENSE)),
because it links/models the GPLv3 seLe4n ABI. The default `libtopomalloc`
artifact links no GPL code and is MIT; building with the `sele4n-sim` feature
produces a GPL combined work. The full split and SPDX policy are in
[`NOTICE`](NOTICE).

## Planning & design

- **Specification:** [`planning/SPEC.md`](planning/SPEC.md)
- **Implementation plan:** [`planning/plans/README.md`](planning/plans/README.md) — an
  overview plus ten focused domain plans (24 workstreams, milestones M0–M9).
- **Decisions, conventions, ABI:** [`docs/`](docs/)

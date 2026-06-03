<!-- SPDX-License-Identifier: MIT -->
# Building & CI

Everything goes through one entry point, `cargo xtask`, so developers and CI run
identical commands.

```sh
cargo xtask setup     # install the pinned Rust + Lean toolchains and cross targets
cargo xtask ci        # the exact sequence CI runs
```

## Commands

| Command | Does |
|---------|------|
| `xtask setup [--verify]` | install/verify toolchains + cross targets (idempotent) |
| `xtask build [--target T] [--profile debug\|performance]` | build all crates (+ Lean via `lake`) |
| `xtask gen [--check]` | regenerate / verify the generated tables (G-table) |
| `xtask test [--kind unit\|prop\|diff\|fuzz]` | run the test suites |
| `xtask fmt [--check]` | rustfmt |
| `xtask lint` | clippy `-D warnings` + SPDX + markdownlint + Lean style |
| `xtask lean [--check]` | `lake build` + `lake exe check` |
| `xtask bench` | criterion micro-benchmarks (non-gating) |
| `xtask ci` | fmt + gen-check + lint + build matrix + test + Lean |

## Toolchains

`rust-toolchain.toml` and `lean-toolchain` pin exact versions. A fresh clone is
green with `cargo xtask setup && cargo xtask ci`. If `lake` (Lean) is not
installed, the Lean steps are skipped locally with a notice; CI always runs them.

## Dual-architecture

The CI matrix builds and tests on **x86-64** and **AArch64** (the seLe4n /
Raspberry Pi 5 target), the latter via a cross toolchain + `qemu-user`. Locally,
`xtask` compile-checks AArch64 with `cargo check` when no cross-linker is
present. AArch64 is co-primary from M0, never deferred.

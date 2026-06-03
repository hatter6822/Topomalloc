<!-- SPDX-License-Identifier: MIT -->
# `ci/` — continuous integration

The CI **workflow** lives in [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml).
Its guiding rule (W0-5): **CI calls `cargo xtask` verbatim — no build logic lives
only in YAML.** Every job is reproducible locally with the same `xtask`
subcommand, and `cargo xtask ci` runs the whole sequence end to end.

## Job graph (W0-5, additive per milestone)

| Job | Runs | Gate |
|-----|------|------|
| `build` | `{x86-64, aarch64-via-QEMU} × {debug, performance}` → `xtask build` | G-build |
| `lint` | `xtask lint` (clippy `-D warnings` + SPDX + markdownlint) | G-build |
| `lean` | `xtask lean` (`lake build` + `lake exe check`) | G-build → G-model |
| `test` | `xtask test` on both arches | G-core |
| `gen-golden-diff` | `xtask gen --check` | G-table |
| `docs` | `mdbook build docs` | non-gating until plan 10 |

Toolchains are cached; AArch64 uses a cross toolchain + `qemu-user`. Required
status checks are configured on the working branch so a red job blocks merge.
This directory is reserved for any CI helper scripts that must be shared between
jobs; today everything routes through `xtask`.

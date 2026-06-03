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

Toolchains are cached; AArch64 uses a cross toolchain + `qemu-user`. Making a red
job *block merge* is a one-time repository setting — see "Required status checks"
below. This directory is reserved for any CI helper scripts that must be shared
between jobs; today everything routes through `xtask`.

## Required status checks (branch protection)

CI runs on every push/PR but is not *enforced* as a merge gate by a file — that
is a one-time repository setting (W0-5). To enforce it:

**Settings → Branches → Add branch ruleset** (or *Branch protection rule*) for
`main` → enable **Require status checks to pass before merging**, then add these
checks (the job names from `ci.yml`):

- `lint`
- `gen-golden-diff`
- `build (x86_64-unknown-linux-gnu, debug)`
- `build (x86_64-unknown-linux-gnu, performance)`
- `build (aarch64-unknown-linux-gnu, debug)`
- `build (aarch64-unknown-linux-gnu, performance)`
- `test (x86_64-unknown-linux-gnu)`
- `test (aarch64-unknown-linux-gnu)`
- `lean`
- `abi-test`
- `doc`

Do **not** require `docs (non-gating)` — the mdbook job is informational until
plan 10. A check name appears in the picker only after it has run once on the
repo (it already has). Also tick *Require branches to be up to date before
merging* so checks always reflect the merged result.

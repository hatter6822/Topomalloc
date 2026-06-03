<!-- SPDX-License-Identifier: MIT -->
# `xtask/` — the build/codegen/CI driver (D3, W0-4)

`cargo xtask` is the single entry point that developers and CI both use, so
"build" never means "Rust only" and no build logic lives only in CI YAML. It
orchestrates `cargo` (Rust), `lake` (Lean), the codegen pipeline, cross builds,
and the lint/format/SPDX gates. It is intentionally dependency-free so it builds
from a bare toolchain before anything else is fetched.

See [`../CONTRIBUTING.md`](../CONTRIBUTING.md) for the command list, or run
`cargo xtask help`. The contract between dev and CI is `cargo xtask ci`.

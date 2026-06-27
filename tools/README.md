<!-- SPDX-License-Identifier: MIT -->
# `tools/` — build-time and validation tools

These host tools are driven by `cargo xtask` and are not allocator runtime code.

| Tool | Purpose |
|------|---------|
| `size-class-gen` | Single source of truth for size classes. It validates the golden JSON and emits Rust, C, and Lean tables; CI fails on generated-output drift. |
| `trace-replay` | Parses SPEC traces and replays them against the executable model/differential harness, checking ownership and well-formedness boundaries. |

<!-- SPDX-License-Identifier: MIT -->
# `tools/` — build-time and validation tools

Host tools driven by `cargo xtask`. They are not part of the allocator runtime.

| Tool | Charter |
|------|---------|
| `size-class-gen` | **THE** size-class generator (DD-1) — the single source of truth. Reads the committed golden `size-classes.json`, validates every §9.3/§9.5 invariant, and emits the Rust table, the C header, and the Lean table. CI golden-diffs the output (G-table); nothing is ever hand-edited. |
| `trace-replay` | Executable-model replay / differential runner (§33.7). Parses a trace in the SPEC grammar and replays it against the host executable model, checking well-formedness at each boundary. The Lean executable model becomes the proof-grade oracle later (plan 02). |

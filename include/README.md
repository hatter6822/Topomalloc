<!-- SPDX-License-Identifier: MIT -->
# `include/` — generated and public C headers

C clients of TopoMalloc include these headers (§10, plan 06).

| Header | Charter |
|--------|---------|
| `topomalloc.h` | The hand-written public C API surface (the prefixed `topomalloc_*` entry points). The full standard/extended C API and the seLe4n-specific `topomalloc_sele4n.h` arrive with plan 06/09. |
| `topomalloc_tables.h` | **Generated** size-class constants and table (DO NOT EDIT). Emitted by `tools/size-class-gen` from the golden; byte-for-byte consistent with the Rust and Lean tables (DD-1). |

<!-- SPDX-License-Identifier: MIT -->
# `include/` — generated and public C headers

C clients of TopoMalloc include these headers (§10, plan 06).

| Header | Charter |
|--------|---------|
| `topomalloc.h` | The public C API surface (plan 06 W8): the prefixed standard core (`topomalloc_malloc/free/calloc/realloc/…`), aligned/POSIX entries, C23 sized frees, and the extended `topo_*x` API with the `TOPO_*` flag macros. Machine-verified against the exported symbols and compiled as C11 **and** C++17 by `cargo xtask abi-test` (W8-8). The seLe4n-specific `topomalloc_sele4n.h` arrives with plan 09. |
| `topomalloc_new_delete.hpp` | **Opt-in** C++ global `operator new`/`delete` replacements over the C API (W8-5): include it in exactly one translation unit. Scalar/array, nothrow, sized (C++14), and over-aligned (C++17) forms, with the conforming `new_handler` loop. Never linked implicitly — the library exports no operator symbols (the plan 10 override artifact owns interposition). |
| `topomalloc_tables.h` | **Generated** size-class constants and table (DO NOT EDIT). Emitted by `tools/size-class-gen` from the golden; byte-for-byte consistent with the Rust and Lean tables (DD-1). |

<!-- SPDX-License-Identifier: MIT -->
# `include/` — public C/C++ headers

C and C++ clients include headers from this directory. The exported symbol set is
checked by `cargo xtask abi-test` against these declarations.

| Header | Purpose |
|--------|---------|
| `topomalloc.h` | Main C API: prefixed allocation functions, aligned/POSIX forms, C23 sized frees, `topo_*x` extensions, arena/extent-hook controls, stats, profiling, hardening, deterministic/debug, and NUMA controls. |
| `topomalloc_new_delete.hpp` | Opt-in C++ global `operator new`/`delete` replacements. Include it in exactly one translation unit; the library does not interpose C++ operators implicitly. |
| `topomalloc_tables.h` | Generated size-class constants and table. Do not edit directly; regenerate with `cargo xtask gen`. |

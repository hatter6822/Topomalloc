<!-- SPDX-License-Identifier: MIT -->
# Versioning & ABI-series policy (W0-13)

## Semantic version

TopoMalloc follows [SemVer](https://semver.org/). The crate version is the
single source of truth: it lives in `Cargo.toml` (`workspace.package.version`)
and is surfaced at runtime as `topo_core::VERSION`, in the stats JSON field
`topomalloc_version` (Appendix D), and via the control key `topo.version` and
the C entry point `topomalloc_version()`. They can never drift because they all
derive from one constant.

## ABI series

| Series | Stability |
|--------|-----------|
| `0.x` (current) | **Unstable.** No ABI or API stability guarantees. Symbol names, struct layouts, the size-class table, and stats fields may all change between `0.x` releases. |
| `>= 1.0` | A **stable release series** per SPEC §35.3 (see below). |

We are in `0.x`: the project is at milestone M0 (walking skeleton). Do not
depend on ABI stability yet.

## Stability rules for a stable series (`>= 1.0`, SPEC §35.3)

Within a stable major series:

* **Public C API names and struct ABI MUST remain stable.** Opaque handles are
  preferred over exposed structs.
* **Stats JSON fields are additive** — fields may be *added*, never removed or
  renamed; consumers must ignore unknown fields. The renderer in `topo-stats`
  is written to make additions the only change.
* **Size-class table changes are allowed but MUST be documented**, because they
  affect memory footprint and performance. The generated table carries the
  golden it came from; a change is a documented, reviewed edit to the golden.

## C ABI surface (M0)

The exported symbols are **prefixed** (`topomalloc_*`) so linking the library
never hijacks the process `malloc`. Interposition/override deployment (§35.1)
that replaces the system allocator is a separate, deliberate step (plan 10).

| Symbol | Semantics |
|--------|-----------|
| `topomalloc_malloc(size)` | allocate; null on OOM/overflow |
| `topomalloc_free(ptr)` | free; `free(NULL)` is a no-op |
| `topomalloc_calloc(n, size)` | zeroed; overflow-checked (§26.1) |
| `topomalloc_aligned_alloc(align, size)` | aligned; null if `align` not a power of two |
| `topomalloc_version()` | NUL-terminated version string |
| `topomalloc_backend()` | active backend name (`"posix"` / `"sele4n-sim"`) |

The full standard/extended C API (`realloc`, `posix_memalign`,
`malloc_usable_size`, C23 sized free, …) and the C++ operators arrive with
plan 06.

## Upstream seLe4n ABI pin (D8)

The seLe4n integration consumes upstream crates pinned to an exact SHA; see
[`DECISIONS.md`](DECISIONS.md) §D8. The host `Sele4nSim` mirrors the pinned ABI
surface, so upstream drift surfaces as a compile error (risk R13). The pin is
bumped only via an explicit work unit (plan 09).

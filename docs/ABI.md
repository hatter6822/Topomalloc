<!-- SPDX-License-Identifier: MIT -->
# Versioning & ABI-series policy (W0-13, plan 06 W8)

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
| `0.x` (current) | **Unstable.** No ABI or API stability guarantees. Symbol names, struct layouts, the flag layout, the size-class table, and stats fields may all change between `0.x` releases. |
| `>= 1.0` | A **stable release series** per SPEC §35.3 (see below). |

We are in `0.x`: milestone M0 is closed and the W8 public API (plan 06) now
runs over the real M1 central-path allocator. Do not depend on ABI stability
yet.

## Stability rules for a stable series (`>= 1.0`, SPEC §35.3)

Within a stable major series:

* **Public C API names and struct ABI MUST remain stable.** Opaque handles are
  preferred over exposed structs (`topo_arena_t`/`topo_tcache_t` are opaque
  integer handles; the only exposed struct is the generated, pinned
  `topomalloc_size_class_t`).
* **Stats JSON fields are additive** — fields may be *added*, never removed or
  renamed; consumers must ignore unknown fields. The renderer in `topo-stats`
  is written to make additions the only change.
* **Size-class table changes are allowed but MUST be documented**, because they
  affect memory footprint and performance. The generated table carries the
  golden it came from; a change is a documented, reviewed edit to the golden.

## C ABI surface (W8)

The exported symbols are **prefixed** (`topomalloc_*` / `topo_*`) so linking
the library never hijacks the process `malloc`. Interposition/override
deployment (§35.1) that replaces the system allocator is a separate,
deliberate step (plan 10). The same rule covers C++: the global operator
new/delete replacements are an **opt-in header**
(`include/topomalloc_new_delete.hpp`, included in exactly one translation
unit), never exported symbols.

### Standard C core (§10.1, W8-1)

| Symbol | Semantics |
|--------|-----------|
| `topomalloc_malloc(size)` | allocate, ≥16-aligned; null + `ENOMEM` on OOM/overflow |
| `topomalloc_free(ptr)` | free; `free(NULL)` no-op; invalid pointers detected and ignored (§35.2); never modifies `errno` |
| `topomalloc_calloc(n, size)` | zeroed through the whole usable size; overflow-checked in product *and* rounding (§26.1) |
| `topomalloc_realloc(ptr, size)` | §25.1 contract — failure (null + `ENOMEM`) leaves the original valid; `realloc(p, 0)` frees and returns null; invalid `ptr` is null + `EINVAL`, untouched |
| `topomalloc_reallocarray(ptr, n, size)` | overflow-checked `realloc(ptr, n*size)` |

### Aligned / POSIX (W8-2)

| Symbol | Semantics |
|--------|-----------|
| `topomalloc_aligned_alloc(align, size)` | power-of-two `align`, `size` a multiple of it (§25.5); else null + `EINVAL` |
| `topomalloc_posix_memalign(&p, align, size)` | returns `0`/`EINVAL`/`ENOMEM`; never touches `errno`; writes `*memptr` only on success |
| `topomalloc_memalign(align, size)` | obsolete compatibility: power-of-two `align`, any size |
| `topomalloc_malloc_usable_size(ptr)` | usable bytes (≥ requested); `0` for null/foreign pointers |

### C23 sized free (W8-3)

| Symbol | Semantics |
|--------|-----------|
| `topomalloc_free_sized(ptr, size)` | `size` must equal the last requested size; mismatches abort in debug/hardened builds and are ignored (the free is hint-independent) in performance builds |
| `topomalloc_free_aligned_sized(ptr, align, size)` | the aligned counterpart; `align` joins the checked hint |

### Extended API (§10.3/§10.4, W8-6)

| Symbol | Semantics |
|--------|-----------|
| `topo_mallocx(size, flags)` | allocate with flags; invalid flag word → null + `EINVAL`, deterministically |
| `topo_rallocx(ptr, size, flags)` | realloc with flags; result alignment from `TOPO_ALIGN_LG`; `TOPO_ZERO` zeroes bytes beyond the preserved prefix |
| `topo_xallocx(ptr, size, extra, flags)` | in-place-only resize; returns the real usable size (success ⇔ `result >= size`); cannot grow at M1 (extent-merge growth is M5) |
| `topo_dallocx(ptr, flags)` | free with (validated, advisory) flags |
| `topo_sdallocx(ptr, size, flags)` | sized free with flags (same cross-checks as the C23 family) |
| `topo_nallocx(size, flags)` | the usable size `topo_mallocx` would return; pure; `0` on error |

The `topo_flags_t` (u64) layout — pinned by tests on both the Rust and C
sides, frozen at 1.0:

```text
bits 0–5   lg(alignment), 0 = natural        TOPO_ALIGN_LG(la)
bit  6     zero returned memory              TOPO_ZERO
bit  7     bypass local caches               TOPO_TCACHE_NONE
bit  8     guard allocation                  TOPO_GUARDED
bit  9     avoid hugepages                   TOPO_NO_HUGEPAGE
bit 10     prefer hugepages                  TOPO_PREFER_HUGEPAGE
bits 11–12 lifetime hint                     TOPO_LIFETIME_{SHORT,MEDIUM,LONG}
bits 13–20 hotness 0..=255                   TOPO_HOT(h) / TOPO_COLD
bits 21–52 arena id + 1, 0 = default         TOPO_ARENA(id)
bits 53–63 reserved — must be zero
```

Validation is total (§10.4): reserved bits, the contradictory hugepage pair,
an unrepresentable alignment, or a nonexistent arena fail deterministically —
never a silently degraded allocation. Alignment is extracted into a dedicated
classifier argument, so it can never be dropped as an advisory bit.

### Identification

| Symbol | Semantics |
|--------|-----------|
| `topomalloc_version()` | NUL-terminated version string |
| `topomalloc_backend()` | active backend name (`"posix"` / `"sele4n-sim"`) |

### Behavior contracts

* **errno (W8-1b):** allocation failure ⇒ `ENOMEM`; validation failure ⇒
  `EINVAL`; success restores the caller's `errno`; the free family never
  modifies it; `posix_memalign` reports only through its return value.
* **Zero-size policy (W8-4, §9.6):** `malloc(0)`-style requests return a
  unique freeable pointer by default (`compat.zero_unique`, the glibc
  expectation); `TOPOMALLOC_ZERO_SIZE=null` in the environment — or
  `topo_abi::set_zero_size_policy` — switches to `NULL`-with-untouched-errno
  (`compat.zero_null`). Fixed regardless of policy: `free(NULL)` is a no-op
  and `realloc(p != NULL, 0)` frees and returns `NULL`.
* **realloc alignment (§25.4):** the result satisfies the alignment *stated
  by the call* — fundamental (16) for `realloc`, `TOPO_ALIGN_LG` for
  `topo_rallocx` — exactly like glibc/jemalloc. An over-aligned allocation's
  alignment is not silently inherited across a move; reallocate it with
  `topo_rallocx` to keep it.
* **Invalid frees (§35.2):** foreign, interior, metadata, released, and
  already-free pointers are detected by pagemap classification and ignored
  with no state change. The hardened profile (plan 08 W18) escalates these
  to aborts/quarantine.
* **Rust callers:** the pointer-consuming entry points
  (`free`/`realloc`/sized frees/`posix_memalign` and the `topo_*x`
  equivalents) are `unsafe fn` in the Rust API — a *stale* pointer that
  aliases a recycled live allocation is the one class validation cannot
  detect, so the C-style ownership contract is explicit. The allocation-only
  and read-only entry points are safe.

### ABI pinning (W8-8)

Three mechanisms keep `include/topomalloc.h` and the binary in lockstep:

1. `cargo xtask abi-test` compiles, links, and runs a C (C11) **and** a C++
   (C++17) harness against the staticlib — a wrong signature fails to
   compile or link, and the harnesses call **every** exported function.
2. The same step cross-checks the exported `topomalloc_*`/`topo_*` symbol
   set (via `nm`) against the header's declarations — an exported-but-
   undeclared or declared-but-unexported function fails CI.
3. The flag-layout constants are pinned numerically on both sides
   (`extended::tests::public_flag_layout_is_pinned` in Rust, the asserts in
   `tests/c/abi_smoke.c` in C).

## Upstream seLe4n ABI pin (D8)

The seLe4n integration consumes upstream crates pinned to an exact SHA; see
[`DECISIONS.md`](DECISIONS.md) §D8. The host `Sele4nSim` mirrors the pinned ABI
surface, so upstream drift surfaces as a compile error (risk R13). The pin is
bumped only via an explicit work unit (plan 09).

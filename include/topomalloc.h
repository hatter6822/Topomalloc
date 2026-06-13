/* SPDX-License-Identifier: MIT */
/*
 * topomalloc.h — the public C API surface (SPEC §10, plan 06 W8).
 *
 * Every symbol is *prefixed* (`topomalloc_*` / `topo_*`), so linking
 * libtopomalloc never hijacks the process `malloc`; the interposition/
 * override deployment that replaces the system allocator is a separate,
 * opt-in step (plan 10, §35.1). The C++ operator replacements follow the
 * same rule: include <topomalloc_new_delete.hpp> in exactly one translation
 * unit to opt in (W8-5).
 *
 * ABI series: 0.x — unstable (see docs/ABI.md). The struct/flag layouts
 * below are pinned by the two-sided ABI tests (tests/c/abi_smoke.c,
 * tests/cpp/abi_smoke.cpp, and the Rust unit tests) and freeze at 1.0
 * (§35.3).
 *
 * errno discipline (§10.1): allocation failure sets ENOMEM; argument-
 * validation failure sets EINVAL; the free family never modifies errno;
 * posix_memalign reports through its return value only.
 *
 * Zero-size policy (§9.6): malloc(0)-style requests return a unique
 * freeable pointer by default; set TOPOMALLOC_ZERO_SIZE=null in the
 * environment to get NULL instead (errno untouched — not a failure).
 * free(NULL) is always a no-op; realloc(p, 0) always frees p and returns
 * NULL.
 */
#ifndef TOPOMALLOC_H
#define TOPOMALLOC_H

#include <stddef.h>
#include <stdint.h>

#include "topomalloc_tables.h"

#ifdef __cplusplus
extern "C" {
#endif

/* ------------------------------------------------------------------------
 * Standard C core (§10.1)
 * --------------------------------------------------------------------- */

/* Allocate `size` bytes (16-byte aligned). NULL + ENOMEM on OOM/overflow. */
void *topomalloc_malloc(size_t size);

/* Free a pointer returned by any topomalloc_ or topo_ allocation entry.
 * free(NULL) is a no-op; invalid (foreign/interior/already-free) pointers
 * are detected and ignored without state change (§35.2); errno is never
 * modified. */
void topomalloc_free(void *ptr);

/* Allocate `n * size` zeroed bytes. NULL + ENOMEM on overflow of the product
 * or of any subsequent rounding (§26.1) — never wraps. The whole usable size
 * is zeroed (§26.2). */
void *topomalloc_calloc(size_t n, size_t size);

/* Standard realloc semantics (§25.1): realloc(NULL, n) == malloc(n);
 * realloc(p, 0) frees p and returns NULL; on success the result carries
 * min(old_usable, size) bytes of old content; ON FAILURE (NULL + ENOMEM)
 * THE ORIGINAL ALLOCATION REMAINS VALID. Invalid p: NULL + EINVAL, no state
 * change. */
void *topomalloc_realloc(void *ptr, size_t size);

/* realloc(ptr, n * size) with the multiplication overflow-checked (BSD/
 * glibc): NULL + ENOMEM on overflow, original untouched. */
void *topomalloc_reallocarray(void *ptr, size_t n, size_t size);

/* ------------------------------------------------------------------------
 * Aligned / POSIX (§10.1, §25.5 — W8-2)
 * --------------------------------------------------------------------- */

/* Allocate `size` bytes aligned to `alignment`. `alignment` must be a power
 * of two and `size` an integer multiple of it (SPEC §25.5); otherwise NULL +
 * EINVAL. Alignment is never silently ignored (§10.4). */
void *topomalloc_aligned_alloc(size_t alignment, size_t size);

/* POSIX: *memptr receives `size` bytes aligned to `alignment` (a power of
 * two multiple of sizeof(void*)). Returns 0, EINVAL, or ENOMEM; never
 * modifies errno; writes *memptr only on success. */
int topomalloc_posix_memalign(void **memptr, size_t alignment, size_t size);

/* Obsolete compatibility allocator: power-of-two `alignment`, any size. */
void *topomalloc_memalign(size_t alignment, size_t size);

/* The number of usable bytes in the allocation at `ptr` (>= the requested
 * size). 0 for NULL or a pointer this allocator does not own. */
size_t topomalloc_malloc_usable_size(void *ptr);

/* ------------------------------------------------------------------------
 * C23 sized deallocation (§10.1 — W8-3)
 * --------------------------------------------------------------------- */

/* free(ptr) where `size` must equal the size the allocation was last made
 * with (malloc/realloc). A mismatch is undefined behavior that debug/
 * hardened builds abort on; the hint is never trusted for the free itself,
 * so it cannot corrupt state in any build. free_sized(NULL, n) is a no-op. */
void topomalloc_free_sized(void *ptr, size_t size);

/* The aligned-allocation counterpart: `alignment` and `size` must match the
 * original aligned request. */
void topomalloc_free_aligned_sized(void *ptr, size_t alignment, size_t size);

/* ------------------------------------------------------------------------
 * Extended API (§10.3/§10.4 — W8-6)
 * --------------------------------------------------------------------- */

/* Handle/flag types (§10.3). An arena id names a policy + authority domain
 * (§22/§36.4): the default arena (id 0) is always present, and explicit
 * arenas are created with the arena API below (plan 06 W9). TOPO_ARENA(id)
 * routes an allocation to arena `id`; naming an arena that does not exist (or
 * is being reset/destroyed) is a deterministic EINVAL. topo_tcache_t is
 * declared for the §10.3 surface but has no consumer until explicit-tcache
 * routing lands (plan 05, M2) — as with TOPO_TCACHE(id)/TOPO_NUMA(node), the
 * encoding is deferred to its subsystem rather than frozen as a guess
 * (reserved flag bits hold the space). */
typedef uint32_t topo_arena_t;
typedef uint32_t topo_tcache_t;
typedef uint64_t topo_flags_t;

/* The topo_flags_t layout (validated; reserved bits MUST be zero — §10.4):
 *   bits 0–5   lg(alignment), 0 = natural        TOPO_ALIGN_LG(la)
 *   bit  6     zero returned memory              TOPO_ZERO
 *   bit  7     bypass local caches               TOPO_TCACHE_NONE
 *   bit  8     guard allocation                  TOPO_GUARDED
 *   bit  9     avoid hugepages                   TOPO_NO_HUGEPAGE
 *   bit 10     prefer hugepages                  TOPO_PREFER_HUGEPAGE
 *   bits 11–12 lifetime hint                     TOPO_LIFETIME_*
 *   bits 13–20 hotness 0..=255                   TOPO_HOT(h) / TOPO_COLD
 *   bits 21–52 arena id + 1, 0 = default         TOPO_ARENA(id)
 *   bits 53–63 reserved (must be zero)
 * Invalid words fail deterministically (NULL/0 + EINVAL); advisory hints are
 * validated and threaded to the placement subsystems as they land.
 *
 * TOPO_ALIGN_LG(la): an out-of-range `la` (>= 64, including negative values
 * via the unsigned conversion) encodes a reserved-bit word, so the request
 * fails with EINVAL instead of silently using a different alignment (§10.4).
 * Note `la` is evaluated twice; do not pass an expression with side
 * effects. */
#define TOPO_ALIGN_LG(la)                                                    \
    (((topo_flags_t) (la)) < 64u ? ((topo_flags_t) (la) & 0x3fu)             \
                                 : ((topo_flags_t) 1 << 63))
#define TOPO_ZERO ((topo_flags_t) 1 << 6)
#define TOPO_TCACHE_NONE ((topo_flags_t) 1 << 7)
#define TOPO_GUARDED ((topo_flags_t) 1 << 8)
#define TOPO_NO_HUGEPAGE ((topo_flags_t) 1 << 9)
#define TOPO_PREFER_HUGEPAGE ((topo_flags_t) 1 << 10)
#define TOPO_LIFETIME_SHORT ((topo_flags_t) 1 << 11)
#define TOPO_LIFETIME_MEDIUM ((topo_flags_t) 2 << 11)
#define TOPO_LIFETIME_LONG ((topo_flags_t) 3 << 11)
#define TOPO_HOT(h) ((topo_flags_t) ((h) & 0xffu) << 13)
#define TOPO_COLD TOPO_HOT(0)
#define TOPO_ARENA(id) (((topo_flags_t) (id) + 1) << 21)

/* Allocate with extended flags. NULL + EINVAL on an invalid flag word or a
 * TOPO_ARENA naming no such (active) arena; NULL + EACCES if the named arena's
 * capability lacks the alloc right (§36.4, as topo_mallocx_arena); NULL + ENOMEM
 * on allocation failure (quota/storage). */
void *topo_mallocx(size_t size, topo_flags_t flags);

/* Reallocate with flags under the §25 contract (failure leaves the original
 * valid). TOPO_ZERO zeroes bytes beyond the preserved prefix; size == 0
 * follows the public realloc(p, 0) policy (free + NULL, errno untouched);
 * an invalid flag word is NULL + EINVAL and frees nothing. */
void *topo_rallocx(void *ptr, size_t size, topo_flags_t flags);

/* Resize in place only, to at least `size` (best effort toward size+extra);
 * returns the allocation's real usable size — success iff result >= size.
 * Never moves or frees. At M1 in-place growth beyond the current usable
 * size is not possible (extent-merge growth lands at M5). */
size_t topo_xallocx(void *ptr, size_t size, size_t extra, topo_flags_t flags);

/* Free with flags (advisory on this path; validated, never trusted). */
void topo_dallocx(void *ptr, topo_flags_t flags);

/* Sized free with flags: the extended counterpart of free_sized /
 * free_aligned_sized, with the same debug/hardened cross-checks. */
void topo_sdallocx(void *ptr, size_t size, topo_flags_t flags);

/* The usable size topo_mallocx(size, flags) would return, without
 * allocating — exact for over-aligned requests too (the prediction shares
 * the allocation path's formula); 0 on an invalid flag word or
 * unsatisfiable size. Pure. */
size_t topo_nallocx(size_t size, topo_flags_t flags);

/* ------------------------------------------------------------------------
 * Arena policy & authority domains (§22/§36.4/§36.13 — W9)
 * --------------------------------------------------------------------- */

/* Arena-capability rights (§36.4), an attenuable authority set. A delegated
 * arena may carry any subset of its parent's rights, never a superset. */
#define TOPO_RIGHT_ALLOC ((uint64_t) 1)   /* allocate from the arena */
#define TOPO_RIGHT_FREE ((uint64_t) 2)    /* free the arena's allocations */
#define TOPO_RIGHT_STATS ((uint64_t) 4)   /* observe the arena's statistics */
#define TOPO_RIGHT_DESTROY ((uint64_t) 8) /* reset/destroy the arena */
#define TOPO_RIGHTS_ALL ((uint64_t) 0xf)  /* every right (the ambient default) */

/* Create an explicit arena with full authority and an unlimited quota.
 * Returns its id (>= 1, routable via TOPO_ARENA(id)), or 0 on failure. */
topo_arena_t topo_arena_create(void);

/* Create an explicit arena conferring `rights` with a quota of `quota_bytes`
 * (0 = unlimited). Returns the new arena id, or 0 on failure (EINVAL on a bad
 * rights word, or the arena table being full). */
topo_arena_t topo_arena_create_ex(size_t quota_bytes, uint64_t rights);

/* Delegate an attenuated child arena from `parent` (§36.4): `rights` MUST be a
 * subset of the parent's, `quota_bytes` MUST not exceed the parent's remaining
 * quota, and the label is preserved. Returns the child id, or 0 + EINVAL on
 * any attenuation violation. */
topo_arena_t topo_arena_delegate(topo_arena_t parent, size_t quota_bytes, uint64_t rights);

/* Reset an arena (§22.5): discard all its allocations, return their backing,
 * and bump its reset generation; the arena stays usable. Returns 0, or -1 +
 * EINVAL (e.g. the default arena, or an illegal state). The caller MUST ensure
 * the arena is quiesced and accepts that its outstanding pointers become
 * invalid — exactly like the contract around free(). */
int topo_arena_reset(topo_arena_t arena);

/* Destroy an arena (§22.6/§36.13): reset it, then retire its id behind a
 * generation bump. Returns 0, or -1 + EINVAL. Same quiescence/invalidation
 * contract as topo_arena_reset.
 *
 * Failures carry a mapped errno (the POSIX projection of §36.14's error
 * classes): EACCES (authority denied), EBUSY (arena draining), ENOMEM (quota
 * exceeded), EINVAL (no such arena / illegal request). */
int topo_arena_destroy(topo_arena_t arena);

/* Reconfigure arena `id`'s decay timing (§22.4 configure, F-005) — the headline
 * per-arena tunable. The authority and quota are immutable here (a configure can
 * never widen authority). Returns 0, or -1 + a mapped errno. */
int topo_arena_configure(uint32_t id, uint64_t dirty_decay_ms, uint64_t muzzy_decay_ms);

/* A generation-checked arena handle (§36.13/§36.14): packs (incarnation
 * generation << 32) | id. Unlike a raw id it detects a destroyed-then-recreated
 * arena as stale; 0 is never a valid handle. */
typedef uint64_t topo_arena_handle_t;

/* Mint a handle for arena `id`'s current incarnation, or 0 if unregistered. */
topo_arena_handle_t topo_arena_handle(uint32_t id);

/* The arena id a handle names (its low 32 bits), for use with TOPO_ARENA(id).
 * Does not check the handle's generation — use topo_mallocx_arena for that. */
topo_arena_t topo_arena_id(topo_arena_handle_t handle);

/* Allocate from the arena a handle names, with generation checking (§36.14): a
 * stale handle (its arena was destroyed, possibly recreated at the same id) is
 * NULL + EINVAL — the §36.13 guarantee a raw TOPO_ARENA(id) flag cannot give.
 * The flag word's arena field is ignored (the handle wins); its other hints
 * apply, and size == 0 follows the zero-size policy (§9.6) exactly as
 * topo_mallocx. Allocation failure maps through the arena taxonomy
 * (EACCES/EBUSY/ENOMEM). */
void *topo_mallocx_arena(topo_arena_handle_t handle, size_t size, topo_flags_t flags);

/* ------------------------------------------------------------------------
 * Identification
 * --------------------------------------------------------------------- */

/* The NUL-terminated TopoMalloc version string (matches stats
 * topomalloc_version). */
const char *topomalloc_version(void);

/* The active backing-provider name ("posix" or "sele4n-sim"). */
const char *topomalloc_backend(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TOPOMALLOC_H */

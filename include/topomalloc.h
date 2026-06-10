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

/* Handle/flag types (§10.3). Arena ids come from topo_arena_create /
 * topo_arena_delegate (W9): id 0 is the always-present default arena; ids
 * are allocated monotonically and never reused after destroy, so a stale id
 * is one deterministic EINVAL forever. topo_tcache_t is declared for the
 * §10.3 surface but has no consumer until explicit-tcache routing lands
 * (plan 05, M2) — as with TOPO_TCACHE(id)/TOPO_NUMA(node), the encoding is
 * deferred to its subsystem rather than frozen as a guess (reserved flag
 * bits hold the space). */
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

/* Allocate with extended flags. NULL + EINVAL on an invalid flag word;
 * NULL + ENOMEM on allocation failure. */
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
 * Arena API (§22, §36.4, §36.14 — plan 06 W9)
 *
 * An arena is both a policy domain and a capability-controlled resource
 * domain (D2): it carries an authority (rights word), an information-flow
 * label, and a byte quota — trivial ambient values on POSIX, real on seLe4n.
 * The lifecycle functions return 0 on success or a positive errno value
 * (also set as errno):
 *   EINVAL  no such arena / invalid configuration
 *   EPERM   missing capability right, label violation, or the protected
 *           default arena
 *   EDQUOT  delegation exceeds the parent's remaining quota
 *   EBUSY   the arena's lifecycle state forbids the operation (draining /
 *           resetting / quarantined)
 *   EAGAIN  could not quiesce in-flight operations; retry
 *   ENOMEM  registry/backing exhaustion
 * --------------------------------------------------------------------- */

/* Capability rights (§36.4). 0 in topo_arena_config_t.rights means "all
 * rights" for create and "the parent's rights" for delegate. Rights are
 * fixed at creation: authority not granted at mint cannot be conjured later
 * (an arena without TOPO_ARENA_RIGHT_DESTROY can never be reset/destroyed
 * and holds its quota until process exit). */
#define TOPO_ARENA_RIGHT_ALLOC ((uint32_t) 1 << 0)
#define TOPO_ARENA_RIGHT_FREE ((uint32_t) 1 << 1)
#define TOPO_ARENA_RIGHT_STATS ((uint32_t) 1 << 2)
#define TOPO_ARENA_RIGHT_PURGE ((uint32_t) 1 << 3)
#define TOPO_ARENA_RIGHT_DESTROY ((uint32_t) 1 << 4)
#define TOPO_ARENA_RIGHT_DELEGATE ((uint32_t) 1 << 5)
#define TOPO_ARENA_RIGHTS_ALL ((uint32_t) 0x3f)

/* NUMA placement modes (§15.5). */
#define TOPO_NUMA_OS_DEFAULT ((uint32_t) 0)
#define TOPO_NUMA_LOCAL ((uint32_t) 1)
#define TOPO_NUMA_INTERLEAVE ((uint32_t) 2)
#define TOPO_NUMA_BIND ((uint32_t) 3)
#define TOPO_NUMA_ARENA_POLICY ((uint32_t) 4)

/* Arena configuration for create/delegate. Zero-initialization ({0}) is the
 * ambient default: unnamed, unlimited quota (0 = unlimited; delegation
 * requires an explicit bound), all/parent rights, PUBLIC label (0),
 * OS-default NUMA placement. */
typedef struct topo_arena_config {
    const char *name;   /* optional NUL-terminated name, <= 32 bytes */
    uint64_t quota_bytes;
    uint32_t rights;    /* TOPO_ARENA_RIGHT_* bits */
    uint32_t numa_mode; /* TOPO_NUMA_* */
    uint32_t numa_node; /* target node for TOPO_NUMA_BIND */
    uint32_t label;     /* information-flow label (§36.12); 0 = PUBLIC */
} topo_arena_config_t;

/* Create an explicit arena (§22.4, F-005). cfg may be NULL for the default
 * configuration; *out_id receives the new id only on success. */
int topo_arena_create(const topo_arena_config_t *cfg, topo_arena_t *out_id);

/* Delegate an attenuated child arena (§36.4): the child's rights must be a
 * subset of the parent's, its quota (required nonzero) is reserved from the
 * parent's remaining quota, and its label must equal the parent's. Requires
 * the parent's DELEGATE right and Active state. */
int topo_arena_delegate(topo_arena_t parent, const topo_arena_config_t *cfg,
                        topo_arena_t *out_id);

/* Update the arena's NUMA placement mode (§15.5) — the mutable policy
 * subset; identity/authority fields are immutable after creation. Requires
 * the PURGE right. */
int topo_arena_configure(topo_arena_t id, uint32_t numa_mode,
                         uint32_t numa_node);

/* Reset the arena (§22.5): every outstanding allocation from it is
 * discarded and every outstanding pointer into it becomes invalid (later
 * frees of them are rejected, not honored). The arena remains active and
 * reusable. The default arena is protected (EPERM). Requires the DESTROY
 * right. */
int topo_arena_reset(topo_arena_t id);

/* Destroy the arena (§22.6/§36.13): reset plus the ordered revocation
 * protocol (drain -> unmap -> scrub if required -> revoke -> recycle), then
 * the id is retired forever. Live delegated children are destroyed first.
 * On a partial failure the arena is quarantined — never reported destroyed
 * — and a later topo_arena_destroy retries the protocol. The default arena
 * is protected (EPERM). Requires the DESTROY right. */
int topo_arena_destroy(topo_arena_t id);

/* Allocate from an explicit arena (§36.14) — the entry point for arena ids
 * beyond the TOPO_ARENA flag field. The flag word may not name a different
 * arena. NULL + EINVAL for an unknown id or invalid flags, EBUSY for an
 * inactive arena, EPERM without the ALLOC right, ENOMEM for quota/OOM. */
void *topo_mallocx_arena(topo_arena_t arena, size_t size, topo_flags_t flags);

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

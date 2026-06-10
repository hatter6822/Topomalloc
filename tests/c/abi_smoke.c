/* SPDX-License-Identifier: MIT */
/*
 * C-side ABI smoke test (§34.1 "ABI compatibility tests", plan 06 W8-8).
 * Compiled and linked against the TopoMalloc staticlib by `cargo xtask
 * abi-test`. This is what proves the `include/topomalloc.h` header actually
 * matches the exported Rust ABI: a wrong signature fails to compile or link,
 * a wrong flag-layout constant fails the runtime asserts, and the generated
 * table header is exercised too. Every exported entry point is called.
 * Run from the repo root so the relative includes resolve.
 */
#include "topomalloc.h"

#include <assert.h>
#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* §35.3 ABI pinning: the one exposed struct's layout is frozen by
 * compile-time asserts, not just by reading field values at runtime — a
 * field reorder or width change fails this translation unit. */
_Static_assert(sizeof(topomalloc_size_class_t) == 24,
               "topomalloc_size_class_t layout changed");
_Static_assert(offsetof(topomalloc_size_class_t, size) == 0,
               "size moved");
_Static_assert(offsetof(topomalloc_size_class_t, align) == 4,
               "align moved");
_Static_assert(offsetof(topomalloc_size_class_t, slab_pages) == 8,
               "slab_pages moved");
_Static_assert(offsetof(topomalloc_size_class_t, objects_per_slab) == 12,
               "objects_per_slab moved");
_Static_assert(offsetof(topomalloc_size_class_t, batch) == 16,
               "batch moved");
_Static_assert(offsetof(topomalloc_size_class_t, max_local_capacity) == 20,
               "max_local_capacity moved");
_Static_assert(sizeof(topo_flags_t) == 8, "topo_flags_t must be 64-bit");
_Static_assert(sizeof(topo_arena_t) == 4, "topo_arena_t must be 32-bit");
_Static_assert(sizeof(topo_tcache_t) == 4, "topo_tcache_t must be 32-bit");

/* W9 (§35.3): the arena-config layout is pinned — the Rust #[repr(C)]
 * TopoArenaConfig must match field-for-field. */
_Static_assert(sizeof(topo_arena_config_t) == sizeof(const char *) + 8 + 16,
               "topo_arena_config_t layout drifted");
_Static_assert(offsetof(topo_arena_config_t, name) == 0, "name must be first");
_Static_assert(offsetof(topo_arena_config_t, quota_bytes) ==
                   sizeof(const char *),
               "quota_bytes follows name");
_Static_assert(offsetof(topo_arena_config_t, rights) ==
                   sizeof(const char *) + 8,
               "rights follows quota_bytes");
_Static_assert(offsetof(topo_arena_config_t, numa_mode) ==
                   sizeof(const char *) + 12,
               "numa_mode follows rights");
_Static_assert(offsetof(topo_arena_config_t, numa_node) ==
                   sizeof(const char *) + 16,
               "numa_node follows numa_mode");
_Static_assert(offsetof(topo_arena_config_t, label) ==
                   sizeof(const char *) + 20,
               "label is last");

int main(void) {
    /* version + backend identification */
    const char *v = topomalloc_version();
    assert(v != NULL && v[0] != '\0');
    const char *backend = topomalloc_backend();
    assert(backend != NULL && strcmp(backend, "posix") == 0);

    /* malloc: non-null, 16-aligned, writable, distinct live pointers */
    void *p = topomalloc_malloc(64);
    assert(p != NULL);
    assert(((size_t) p % 16u) == 0u);
    memset(p, 0xab, 64);
    void *q = topomalloc_malloc(64);
    assert(q != NULL && q != p);

    /* malloc_usable_size covers the request; the whole span is writable */
    size_t usable = topomalloc_malloc_usable_size(p);
    assert(usable >= 64u);
    memset(p, 0xcd, usable);

    /* free recycles: the same class slot comes back (no more M0 leak) */
    topomalloc_free(p);
    void *p2 = topomalloc_malloc(64);
    assert(p2 == p);
    topomalloc_free(p2);
    topomalloc_free(q);
    topomalloc_free(NULL); /* must be a no-op */

    /* errno discipline (W8-1b): failed malloc sets ENOMEM; free preserves */
    errno = 0;
    assert(topomalloc_malloc((size_t) -64) == NULL);
    assert(errno == ENOMEM);
    errno = 1234;
    topomalloc_free(NULL);
    assert(errno == 1234);

    /* calloc: fully zeroed over recycled memory; overflow returns NULL */
    unsigned char *z = (unsigned char *) topomalloc_calloc(4, 16);
    assert(z != NULL);
    for (int i = 0; i < 64; i++) {
        assert(z[i] == 0);
    }
    topomalloc_free(z);
    assert(topomalloc_calloc((size_t) -1, 2) == NULL);

    /* realloc: NULL -> malloc; growth preserves; failure keeps original;
       realloc(p, 0) frees and returns NULL (§25.1) */
    char *r = (char *) topomalloc_realloc(NULL, 32);
    assert(r != NULL);
    for (int i = 0; i < 32; i++) {
        r[i] = (char) i;
    }
    r = (char *) topomalloc_realloc(r, 50000);
    assert(r != NULL);
    for (int i = 0; i < 32; i++) {
        assert(r[i] == (char) i);
    }
    errno = 0;
    assert(topomalloc_realloc(r, (size_t) -32) == NULL);
    assert(errno == ENOMEM);
    assert(r[31] == 31); /* original survives the failure */
    assert(topomalloc_realloc(r, 0) == NULL);

    /* reallocarray: overflow-checked */
    void *ra = topomalloc_malloc(8);
    assert(topomalloc_reallocarray(ra, (size_t) -1 / 2, 8) == NULL);
    ra = topomalloc_reallocarray(ra, 4, 8);
    assert(ra != NULL);
    topomalloc_free(ra);

    /* aligned_alloc: honors alignment; rejects non-power-of-two alignment and
       sizes that are not an integer multiple of the alignment (SPEC §25.5). */
    void *a = topomalloc_aligned_alloc(256, 512);
    assert(a != NULL && ((size_t) a % 256u) == 0u);
    topomalloc_free(a);
    errno = 0;
    assert(topomalloc_aligned_alloc(3, 64) == NULL); /* not a power of two */
    assert(errno == EINVAL);
    assert(topomalloc_aligned_alloc(256, 100) == NULL); /* not a multiple */

    /* posix_memalign: result codes, never errno */
    void *pm = NULL;
    errno = 4321;
    assert(topomalloc_posix_memalign(&pm, 2, 64) == EINVAL);
    assert(topomalloc_posix_memalign(&pm, 1024, 100) == 0);
    assert(pm != NULL && ((size_t) pm % 1024u) == 0u);
    assert(errno == 4321);
    topomalloc_free(pm);

    /* memalign: power-of-two alignment, no size-multiple constraint */
    void *ma = topomalloc_memalign(512, 100);
    assert(ma != NULL && ((size_t) ma % 512u) == 0u);
    topomalloc_free(ma);

    /* C23 sized frees: matching hints round-trip */
    void *fs = topomalloc_malloc(100);
    topomalloc_free_sized(fs, 100);
    void *fas = topomalloc_aligned_alloc(512, 1024);
    topomalloc_free_aligned_sized(fas, 512, 1024);
    topomalloc_free_sized(NULL, 7); /* no-op */

    /* extended API: flag layout + mallocx family (W8-6) */
    assert(TOPO_ZERO == ((topo_flags_t) 1 << 6));
    assert(TOPO_ALIGN_LG(10) == 10u);
    assert(TOPO_LIFETIME_LONG == ((topo_flags_t) 3 << 11));
    assert(TOPO_HOT(255) == ((topo_flags_t) 255 << 13));
    assert(TOPO_ARENA(0) == ((topo_flags_t) 1 << 21));
    assert(TOPO_COLD == 0u);

    size_t predicted = topo_nallocx(777, TOPO_ALIGN_LG(10) | TOPO_ZERO);
    assert(predicted >= 777u);
    void *x = topo_mallocx(777, TOPO_ALIGN_LG(10) | TOPO_ZERO);
    assert(x != NULL && ((size_t) x % 1024u) == 0u);
    assert(topomalloc_malloc_usable_size(x) == predicted);
    for (size_t i = 0; i < predicted; i++) {
        assert(((unsigned char *) x)[i] == 0);
    }
    assert(topo_xallocx(x, 700, 0, 0) == predicted); /* fits in place */
    assert(topo_xallocx(x, predicted + 1, 0, 0) < predicted + 1); /* cannot grow */
    void *x2 = topo_rallocx(x, 4096, TOPO_ZERO);
    assert(x2 != NULL);
    topo_sdallocx(x2, 4096, 0);

    /* invalid flag words fail deterministically (§10.4) */
    errno = 0;
    assert(topo_mallocx(64, ~(topo_flags_t) 0) == NULL);
    assert(errno == EINVAL);
    assert(topo_nallocx(64, (topo_flags_t) 1 << 60) == 0u);
    /* a nonexistent arena is one deterministic EINVAL, at any id magnitude */
    errno = 0;
    assert(topo_mallocx(64, TOPO_ARENA(3)) == NULL);
    assert(errno == EINVAL);
    assert(topo_mallocx(64, TOPO_ARENA(300)) == NULL);
    assert(topo_nallocx(64, TOPO_ARENA(3)) == 0u);

    void *d = topo_mallocx(48, 0);
    assert(d != NULL);
    topo_dallocx(d, 0);

    /* PR #11 review fixes, pinned from the C side:
     * 1. nallocx is exact for over-aligned requests (align > page size). */
    size_t oa_predicted = topo_nallocx(100, TOPO_ALIGN_LG(18));
    void *oa = topo_mallocx(100, TOPO_ALIGN_LG(18));
    assert(oa != NULL && ((size_t) oa % (1u << 18)) == 0u);
    assert(topomalloc_malloc_usable_size(oa) == oa_predicted);
    topomalloc_free(oa);
    /* 2. rallocx(p, 0) follows the public realloc(p, 0) policy: free+NULL,
     *    errno untouched. */
    void *rz = topo_mallocx(64, 0);
    errno = 7;
    assert(topo_rallocx(rz, 0, 0) == NULL);
    assert(errno == 7);
    /* 3. out-of-range TOPO_ALIGN_LG is EINVAL, never a different alignment. */
    errno = 0;
    assert(topo_mallocx(64, TOPO_ALIGN_LG(64)) == NULL);
    assert(errno == EINVAL);
    assert(topo_nallocx(64, TOPO_ALIGN_LG(99)) == 0u);

    /* ---- W9: the arena API (§22, §36.4, §36.14) ---- */
    /* Create with the all-default ambient config, allocate via both routing
     * paths, reset, destroy, and verify the id is retired forever. */
    topo_arena_t arena = 0;
    assert(topo_arena_create(NULL, &arena) == 0);
    assert(arena != 0u);
    void *ap = topo_mallocx_arena(arena, 256, 0);
    assert(ap != NULL);
    void *af = topo_mallocx(64, TOPO_ARENA(arena));
    assert(af != NULL);
    topo_dallocx(af, 0);
    /* Conflicting routing (handle vs flag word) is a deterministic EINVAL. */
    errno = 0;
    assert(topo_mallocx_arena(arena, 64, TOPO_ARENA(0)) == NULL);
    assert(errno == EINVAL);
    /* Reset: `ap` is deliberately abandoned (its pointer becomes invalid and
     * later frees of it are rejected, §22.5); the arena stays usable. */
    assert(topo_arena_reset(arena) == 0);
    void *ar = topo_mallocx_arena(arena, 64, 0);
    assert(ar != NULL);
    topo_dallocx(ar, 0);
    /* Configure the NUMA mode (§15.5); an unknown mode is EINVAL. */
    assert(topo_arena_configure(arena, TOPO_NUMA_LOCAL, 0) == 0);
    assert(topo_arena_configure(arena, 99u, 0) == EINVAL);
    /* Delegate an attenuated child (§36.4): alloc/free only, bounded quota. */
    topo_arena_config_t child_cfg = {0};
    child_cfg.quota_bytes = 256u * 1024u;
    child_cfg.rights = TOPO_ARENA_RIGHT_ALLOC | TOPO_ARENA_RIGHT_FREE;
    topo_arena_t child = 0;
    assert(topo_arena_delegate(arena, &child_cfg, &child) == 0);
    void *cp = topo_mallocx_arena(child, 128, 0);
    assert(cp != NULL);
    topo_dallocx(cp, 0);
    /* The child cannot destroy itself (no DESTROY right): EPERM. */
    assert(topo_arena_destroy(child) == EPERM);
    /* Destroying the parent revokes the child too (§36.13); both ids are
     * retired forever (B.5). */
    assert(topo_arena_destroy(arena) == 0);
    errno = 0;
    assert(topo_mallocx_arena(arena, 64, 0) == NULL);
    assert(errno == EINVAL);
    assert(topo_mallocx_arena(child, 64, 0) == NULL);
    assert(topo_arena_destroy(arena) == EINVAL);
    /* The default arena is protected (§22.5, F-006). */
    assert(topo_arena_reset(0) == EPERM);
    assert(topo_arena_destroy(0) == EPERM);

    /* generated table header is consistent and usable from C */
    assert(TOPOMALLOC_QUANTUM == 16u);
    assert(TOPOMALLOC_NUM_SIZE_CLASSES >= 1u);
    assert(topomalloc_size_classes[0].size == TOPOMALLOC_TINY_MIN);
    /* medium/large boundary and derived max-alignment (plan 03 W2) */
    assert(TOPOMALLOC_HUGE_THRESHOLD > TOPOMALLOC_SMALL_MAX);
    assert((TOPOMALLOC_HUGE_THRESHOLD % TOPOMALLOC_PAGE_SIZE) == 0u);
    assert(TOPOMALLOC_MAX_ALIGN == 16u);

    printf("C ABI smoke: OK (version=%s, backend=%s, %u size classes)\n",
           v, backend, (unsigned) TOPOMALLOC_NUM_SIZE_CLASSES);
    return 0;
}

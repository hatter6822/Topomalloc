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

    void *d = topo_mallocx(48, 0);
    assert(d != NULL);
    topo_dallocx(d, 0);

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

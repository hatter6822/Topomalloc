/* SPDX-License-Identifier: MIT */
/*
 * C-side ABI smoke test (§34.1 "ABI compatibility tests"). Compiled and linked
 * against the TopoMalloc staticlib by `cargo xtask abi-test`. This is what proves
 * the hand-written `include/topomalloc.h` actually matches the exported Rust ABI:
 * a wrong signature fails to compile or link, and the generated table header is
 * exercised too. Run from the repo root so the relative includes resolve.
 */
#include "topomalloc.h"

#include <assert.h>
#include <stddef.h>
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
    topomalloc_free(p);
    topomalloc_free(q);
    topomalloc_free(NULL); /* must be a no-op */

    /* calloc: fully zeroed, and overflow returns NULL (never wraps) */
    unsigned char *z = (unsigned char *) topomalloc_calloc(4, 16);
    assert(z != NULL);
    for (int i = 0; i < 64; i++) {
        assert(z[i] == 0);
    }
    topomalloc_free(z);
    assert(topomalloc_calloc((size_t) -1, 2) == NULL);

    /* aligned_alloc: honors alignment; rejects non-powers-of-two */
    void *a = topomalloc_aligned_alloc(256, 100);
    assert(a != NULL && ((size_t) a % 256u) == 0u);
    topomalloc_free(a);
    assert(topomalloc_aligned_alloc(3, 100) == NULL);

    /* generated table header is consistent and usable from C */
    assert(TOPOMALLOC_QUANTUM == 16u);
    assert(TOPOMALLOC_NUM_SIZE_CLASSES >= 1u);
    assert(topomalloc_size_classes[0].size == TOPOMALLOC_TINY_MIN);

    printf("C ABI smoke: OK (version=%s, backend=%s, %u size classes)\n",
           v, backend, (unsigned) TOPOMALLOC_NUM_SIZE_CLASSES);
    return 0;
}

/* SPDX-License-Identifier: MIT */
/*
 * topomalloc.h — the public C API surface (§10, plan 06).
 *
 * M0 exports the prefixed `topomalloc_*` entry points. The symbols are
 * deliberately prefixed so linking libtopomalloc never hijacks the process
 * `malloc`; interposition/override deployment that replaces the system
 * allocator is a separate, opt-in step (plan 10, §35.1). The full standard and
 * extended C API (realloc, posix_memalign, malloc_usable_size, C23 sized free,
 * ...) and the seLe4n header (topomalloc_sele4n.h) arrive with plans 06/09.
 */
#ifndef TOPOMALLOC_H
#define TOPOMALLOC_H

#include <stddef.h>

#include "topomalloc_tables.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Allocate `size` bytes (16-byte aligned). Returns NULL on OOM or overflow. */
void *topomalloc_malloc(size_t size);

/* Free a pointer returned by a topomalloc_* allocator. free(NULL) is a no-op. */
void topomalloc_free(void *ptr);

/* Allocate `n * size` zeroed bytes. Returns NULL on overflow (never wraps). */
void *topomalloc_calloc(size_t n, size_t size);

/* Allocate `size` bytes aligned to `alignment`. `alignment` must be a power of
 * two and `size` an integer multiple of it (SPEC 25.5); otherwise NULL. */
void *topomalloc_aligned_alloc(size_t alignment, size_t size);

/* The NUL-terminated TopoMalloc version string (matches stats topomalloc_version). */
const char *topomalloc_version(void);

/* The active backing-provider name ("posix" or "sele4n-sim"). */
const char *topomalloc_backend(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TOPOMALLOC_H */

/* SPDX-License-Identifier: MIT */
/*
 * topomalloc_new_delete.hpp — C++ global operator new/delete replacements
 * over the TopoMalloc C API (SPEC §10.2, plan 06 W8-5).
 *
 * OPT-IN, by design: include this header in **exactly one** translation unit
 * of a program to route every `new`/`delete` in that program through
 * TopoMalloc. The library itself never exports the operator symbols, for the
 * same reason its C symbols are prefixed — linking libtopomalloc must never
 * hijack a process's allocator by accident (§35.1; full interposition is the
 * plan 10 override artifact). This is the deployment model mimalloc and
 * jemalloc document for their new/delete headers.
 *
 * Conformance notes ([new.delete] / §10.2):
 *  - allocation failure runs the std::new_handler loop, then throws
 *    std::bad_alloc (or returns nullptr for the nothrow forms);
 *  - zero-size `new` requests one byte, so every result is a distinct,
 *    freeable, non-null pointer regardless of the C-level zero-size policy;
 *  - over-aligned forms (C++17 `align_val_t`) route through
 *    `topomalloc_memalign`, so the alignment is honored, never ignored
 *    (§10.4);
 *  - sized `delete` forwards the size hint to the C23 sized-free entry
 *    points, where debug/hardened builds cross-check it (W8-3) — a wrong
 *    hint can never corrupt state because the hint is not trusted for the
 *    free itself.
 */
#ifndef TOPOMALLOC_NEW_DELETE_HPP
#define TOPOMALLOC_NEW_DELETE_HPP

#include <new>

#include "topomalloc.h"

#if defined(__cpp_exceptions) || defined(__EXCEPTIONS) || defined(_CPPUNWIND)
#define TOPOMALLOC_THROW_BAD_ALLOC() throw std::bad_alloc()
#else
#include <exception>
#define TOPOMALLOC_THROW_BAD_ALLOC() std::terminate()
#endif

namespace topomalloc_detail {

/* The [new.delete.single] required behavior: attempt, then loop running the
 * installed new_handler until the allocation succeeds or no handler remains. */
inline void *alloc_loop(std::size_t size, std::size_t align) {
    if (size == 0) {
        size = 1; /* every successful new yields a distinct non-null pointer */
    }
    for (;;) {
        void *p = (align == 0) ? topomalloc_malloc(size)
                               : topomalloc_memalign(align, size);
        if (p != nullptr) {
            return p;
        }
        std::new_handler handler = std::get_new_handler();
        if (handler == nullptr) {
            return nullptr; /* caller throws or returns per its form */
        }
        handler(); /* may free memory, install another handler, or throw */
    }
}

inline void *alloc_or_throw(std::size_t size, std::size_t align) {
    void *p = alloc_loop(size, align);
    if (p == nullptr) {
        TOPOMALLOC_THROW_BAD_ALLOC();
    }
    return p;
}

} // namespace topomalloc_detail

/* ---- replaceable single-object and array forms (§10.2) ----------------- */

void *operator new(std::size_t size) {
    return topomalloc_detail::alloc_or_throw(size, 0);
}

void *operator new[](std::size_t size) {
    return topomalloc_detail::alloc_or_throw(size, 0);
}

void *operator new(std::size_t size, const std::nothrow_t &) noexcept {
    return topomalloc_detail::alloc_loop(size, 0);
}

void *operator new[](std::size_t size, const std::nothrow_t &) noexcept {
    return topomalloc_detail::alloc_loop(size, 0);
}

void operator delete(void *ptr) noexcept { topomalloc_free(ptr); }
void operator delete[](void *ptr) noexcept { topomalloc_free(ptr); }

void operator delete(void *ptr, const std::nothrow_t &) noexcept {
    topomalloc_free(ptr);
}
void operator delete[](void *ptr, const std::nothrow_t &) noexcept {
    topomalloc_free(ptr);
}

/* ---- sized delete (C++14): the size hint reaches the allocator ---------- */
#if defined(__cpp_sized_deallocation) || (defined(__cplusplus) && __cplusplus >= 201402L)

void operator delete(void *ptr, std::size_t size) noexcept {
    /* `new` rounds 0 up to 1; mirror it so the checked hint matches. */
    topomalloc_free_sized(ptr, size == 0 ? 1 : size);
}

void operator delete[](void *ptr, std::size_t size) noexcept {
    topomalloc_free_sized(ptr, size == 0 ? 1 : size);
}

#endif /* sized deallocation */

/* ---- over-aligned forms (C++17 align_val_t) ----------------------------- */
#if defined(__cpp_aligned_new)

void *operator new(std::size_t size, std::align_val_t align) {
    return topomalloc_detail::alloc_or_throw(size,
                                             static_cast<std::size_t>(align));
}

void *operator new[](std::size_t size, std::align_val_t align) {
    return topomalloc_detail::alloc_or_throw(size,
                                             static_cast<std::size_t>(align));
}

void *operator new(std::size_t size, std::align_val_t align,
                   const std::nothrow_t &) noexcept {
    return topomalloc_detail::alloc_loop(size, static_cast<std::size_t>(align));
}

void *operator new[](std::size_t size, std::align_val_t align,
                     const std::nothrow_t &) noexcept {
    return topomalloc_detail::alloc_loop(size, static_cast<std::size_t>(align));
}

void operator delete(void *ptr, std::align_val_t) noexcept {
    topomalloc_free(ptr);
}
void operator delete[](void *ptr, std::align_val_t) noexcept {
    topomalloc_free(ptr);
}

void operator delete(void *ptr, std::align_val_t align,
                     const std::nothrow_t &) noexcept {
    (void) align;
    topomalloc_free(ptr);
}
void operator delete[](void *ptr, std::align_val_t align,
                       const std::nothrow_t &) noexcept {
    (void) align;
    topomalloc_free(ptr);
}

void operator delete(void *ptr, std::size_t size,
                     std::align_val_t align) noexcept {
    topomalloc_free_aligned_sized(ptr, static_cast<std::size_t>(align),
                                  size == 0 ? 1 : size);
}

void operator delete[](void *ptr, std::size_t size,
                       std::align_val_t align) noexcept {
    topomalloc_free_aligned_sized(ptr, static_cast<std::size_t>(align),
                                  size == 0 ? 1 : size);
}

#endif /* __cpp_aligned_new */

#endif /* TOPOMALLOC_NEW_DELETE_HPP */

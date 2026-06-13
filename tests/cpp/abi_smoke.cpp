// SPDX-License-Identifier: MIT
/*
 * C++-side ABI smoke test (§34.1, plan 06 W8-5/W8-8). Two obligations:
 *
 *  1. `include/topomalloc.h` compiles cleanly as C++ and links (the extern
 *     "C" surface is usable from C++ unchanged) — the W8-8 "header compiles
 *     under C and C++" acceptance.
 *  2. The opt-in operator new/delete replacement header
 *     (`topomalloc_new_delete.hpp`, W8-5) routes scalar, array, nothrow,
 *     sized, and over-aligned forms through TopoMalloc — proven by checking
 *     that operator-new results are TopoMalloc-owned
 *     (`topomalloc_malloc_usable_size != 0`) and that sized/aligned deletes
 *     round-trip.
 *
 * Compiled and run by `cargo xtask abi-test` with a C++17 compiler.
 */
#include "topomalloc_new_delete.hpp" // defines the replacement operators (one TU)

#include <cassert>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <new>
#include <thread>
#include <vector>

namespace {

struct Pod {
    std::uint64_t a;
    std::uint64_t b;
};

struct alignas(128) OverAligned {
    std::uint64_t a;
};

/// new_handler instrumentation: counts invocations and uninstalls itself, so
/// the [new.delete.single] retry loop terminates deterministically without
/// needing real memory exhaustion.
int g_handler_calls = 0;
void counting_handler() {
    ++g_handler_calls;
    std::set_new_handler(nullptr);
}

} // namespace

int main() {
    // The plain C surface is callable from C++.
    void *c = topomalloc_malloc(48);
    assert(c != nullptr);
    assert(topomalloc_malloc_usable_size(c) >= 48u);
    topomalloc_free(c);

    // Scalar new/delete routes through TopoMalloc (usable size is nonzero
    // exactly for TopoMalloc-owned pointers).
    Pod *p = new Pod{1, 2};
    assert(p != nullptr);
    assert(topomalloc_malloc_usable_size(p) >= sizeof(Pod));
    assert(p->a == 1 && p->b == 2);
    delete p; // sized delete where the compiler emits it (C++14)

    // Array new/delete.
    auto *arr = new std::uint32_t[100];
    assert(topomalloc_malloc_usable_size(arr) >= 100 * sizeof(std::uint32_t));
    for (int i = 0; i < 100; i++) {
        arr[i] = static_cast<std::uint32_t>(i);
    }
    assert(arr[99] == 99u);
    delete[] arr;

    // nothrow forms return null instead of throwing on an impossible size
    // (read through a volatile so the compiler cannot flag the constant).
    volatile std::size_t impossible = static_cast<std::size_t>(-64);
    void *huge = operator new(impossible, std::nothrow);
    assert(huge == nullptr);
    void *ok = operator new(64, std::nothrow);
    assert(ok != nullptr && topomalloc_malloc_usable_size(ok) >= 64u);
    operator delete(ok, std::nothrow);

#if defined(__cpp_aligned_new)
    // Over-aligned new honors alignas beyond the default new alignment and
    // pairs with the aligned (sized) deletes (§10.2 / §10.4).
    auto *oa = new OverAligned{7};
    assert((reinterpret_cast<std::uintptr_t>(oa) % 128u) == 0u);
    assert(topomalloc_malloc_usable_size(oa) >= sizeof(OverAligned));
    assert(oa->a == 7u);
    delete oa;

    // Explicit aligned operator forms.
    void *av = operator new(300, std::align_val_t(256));
    assert((reinterpret_cast<std::uintptr_t>(av) % 256u) == 0u);
    operator delete(av, 300, std::align_val_t(256));
#endif

    // Explicit sized-delete operator call with the truthful size.
    void *sv = operator new(200);
    operator delete(sv, 200);

    // Zero-size new yields distinct, freeable, non-null pointers regardless
    // of the C-level zero-size policy.
    void *z1 = operator new(0);
    void *z2 = operator new(0);
    assert(z1 != nullptr && z2 != nullptr && z1 != z2);
    operator delete(z1);
    operator delete(z2, static_cast<std::size_t>(0));

    // The [new.delete.single] new_handler loop: on failure the installed
    // handler runs; when it uninstalls itself the loop ends — with bad_alloc
    // for the throwing form, nullptr for nothrow. (A handler that frees
    // memory and retries is the same loop with a success exit; exhaustion
    // cannot be staged deterministically, so the termination arm is what
    // this pins.)
    volatile std::size_t impossible2 = static_cast<std::size_t>(-128);
    g_handler_calls = 0;
    std::set_new_handler(counting_handler);
    void *nt = operator new(impossible2, std::nothrow);
    assert(nt == nullptr);
    assert(g_handler_calls == 1 && "nothrow new must still run the handler");
    assert(std::get_new_handler() == nullptr);

#if defined(__cpp_exceptions) || defined(__EXCEPTIONS) || defined(_CPPUNWIND)
    g_handler_calls = 0;
    std::set_new_handler(counting_handler);
    bool caught = false;
    try {
        (void) operator new(impossible2);
    } catch (const std::bad_alloc &) {
        caught = true;
    }
    assert(caught && "throwing new must raise bad_alloc after the handler");
    assert(g_handler_calls == 1);
    assert(std::get_new_handler() == nullptr);
#endif

    // Concurrent new/delete through the replaced operators: the engine under
    // them is thread-safe, contents stay per-thread intact.
    {
        std::vector<std::thread> threads;
        for (int t = 0; t < 4; t++) {
            threads.emplace_back([t] {
                for (int i = 0; i < 200; i++) {
                    const std::size_t n = 1 + ((static_cast<std::size_t>(i) * 37u +
                                                static_cast<std::size_t>(t) * 11u) %
                                               700u);
                    auto *buf = new unsigned char[n];
                    std::memset(buf, t + 1, n);
                    assert(buf[0] == static_cast<unsigned char>(t + 1));
                    assert(buf[n - 1] == static_cast<unsigned char>(t + 1));
                    delete[] buf;
                }
            });
        }
        for (auto &th : threads) {
            th.join();
        }
    }

    // Arena API (§22/§36.4 — W9): create an explicit arena, allocate from it
    // through the flag-routed path, then destroy it. The header's C linkage is
    // exercised from C++ too.
    {
        const topo_arena_t arena = topo_arena_create();
        assert(arena >= 1u);
        void *ap = topo_mallocx(96, TOPO_ARENA(arena));
        assert(ap != nullptr);
        std::memset(ap, 0x33, 96);
        topomalloc_free(ap);
        const topo_arena_t child =
            topo_arena_delegate(arena, 256, TOPO_RIGHT_ALLOC | TOPO_RIGHT_FREE);
        assert(child >= 1u);
        assert(topo_arena_destroy(child) == 0);
        assert(topo_arena_destroy(arena) == 0);
        // The default arena is protected.
        assert(topo_arena_destroy(0) == -1);
    }

    std::printf("C++ ABI smoke: OK (version=%s, %u size classes)\n",
                topomalloc_version(),
                static_cast<unsigned>(TOPOMALLOC_NUM_SIZE_CLASSES));
    return 0;
}

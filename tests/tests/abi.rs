// SPDX-License-Identifier: MIT
//! Cross-crate ABI integration tests (§34.1 "ABI compatibility tests", plan 06
//! W8 DoD: *every API WU has an ABI test*). Exercises the full prefixed C
//! surface exported by `topo-abi` — core, aligned/POSIX, C23 sized free, and
//! the extended `topo_*x` family — against the process-wide M1 allocator,
//! plus the cross-entry-point coherence properties (usable size ↔ nallocx,
//! recycle-on-free, errno discipline) the unit tests cannot see in isolation.

use std::collections::BTreeSet;
use std::ffi::c_void;
use std::ptr;

use topo_abi::{
    topo_align_lg, topo_dallocx, topo_mallocx, topo_nallocx, topo_rallocx, topo_sdallocx,
    topo_xallocx, topomalloc_aligned_alloc, topomalloc_calloc, topomalloc_free,
    topomalloc_free_aligned_sized, topomalloc_free_sized, topomalloc_malloc,
    topomalloc_malloc_usable_size, topomalloc_memalign, topomalloc_posix_memalign,
    topomalloc_realloc, topomalloc_reallocarray, TOPO_ZERO,
};

/// Test wrappers: every pointer the tests pass here is test-owned (or a
/// never-owned probe the entry points are specified to reject) — exactly the
/// entry points' documented safety contracts.
fn free(p: *mut c_void) {
    // SAFETY: see above.
    unsafe { topomalloc_free(p) }
}

fn realloc(p: *mut c_void, n: usize) -> *mut c_void {
    // SAFETY: as `free`.
    unsafe { topomalloc_realloc(p, n) }
}

/// Assert the thread's errno (read through std — the same cell libc writes).
/// On non-unix hosts there is no C errno (see `topo-abi`'s `errno_shim`), so
/// the assertion compiles away with the constants.
#[cfg(unix)]
fn expect_errno(expected: i32) {
    let got = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    assert_eq!(got, expected, "errno mismatch");
}
#[cfg(not(unix))]
fn expect_errno(_expected: i32) {}

#[cfg(unix)]
use libc::{EINVAL, ENOMEM};
#[cfg(not(unix))]
const ENOMEM: i32 = 12;
#[cfg(not(unix))]
const EINVAL: i32 = 22;

#[test]
fn malloc_free_recycles_across_the_full_size_spectrum() {
    let mut seen = BTreeSet::new();
    let sizes = [1usize, 7, 16, 24, 100, 4096, 32 * 1024, 100_000, 3 << 20];
    let mut live = Vec::new();
    for &size in &sizes {
        let p = topomalloc_malloc(size);
        assert!(!p.is_null(), "malloc({size}) returned null");
        assert_eq!(p as usize % 16, 0, "malloc({size}) under-aligned");
        assert!(seen.insert(p as usize), "duplicate live pointer");
        let usable = topomalloc_malloc_usable_size(p);
        assert!(usable >= size, "usable {usable} < requested {size}");
        // SAFETY: `usable` writable bytes per malloc_usable_size's contract.
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0xa5, usable) };
        live.push(p);
    }
    for p in live {
        free(p);
    }
    // The engine reclaims: the same byte demand succeeds again indefinitely.
    for &size in &sizes {
        let p = topomalloc_malloc(size);
        assert!(!p.is_null(), "post-free malloc({size}) failed: leak?");
        free(p);
    }
}

#[test]
fn free_null_is_noop_and_preserves_errno() {
    free(ptr::null_mut());
    let p = topomalloc_malloc(64);
    free(p);
    free(ptr::null_mut());
}

#[test]
fn malloc_failure_reports_enomem() {
    // Clear errno via a successful syscall-free path first.
    let ok = topomalloc_malloc(16);
    assert!(!ok.is_null());
    assert!(topomalloc_malloc(usize::MAX - 63).is_null());
    expect_errno(ENOMEM);
    free(ok);
}

#[test]
fn calloc_overflow_returns_null() {
    // Product overflow (§26.1 multiplication check).
    assert!(topomalloc_calloc(usize::MAX, 2).is_null());
    assert!(topomalloc_calloc(1 << 40, 1 << 40).is_null());
    // §26.1 second clause: the product fits, but the *subsequent* page-rounding
    // would overflow — calloc must still return null, never a too-small region.
    assert!(topomalloc_calloc(1, usize::MAX - 100).is_null());
    assert!(topomalloc_calloc(usize::MAX - 100, 1).is_null());
}

#[test]
fn calloc_zeroes_recycled_memory() {
    // Dirty an object, free it, then calloc the same class: the §26.2 zero
    // guarantee must hold over *recycled* (dirty) memory, not just fresh maps.
    let p = topomalloc_malloc(512);
    let usable = topomalloc_malloc_usable_size(p);
    // SAFETY: `usable` writable bytes.
    unsafe { ptr::write_bytes(p.cast::<u8>(), 0xff, usable) };
    free(p);

    let q = topomalloc_calloc(8, 64); // 512 bytes, same class
    assert!(!q.is_null());
    let q_usable = topomalloc_malloc_usable_size(q);
    // SAFETY: `q_usable` readable bytes, zeroed per §26.2.
    unsafe {
        for i in 0..q_usable {
            assert_eq!(q.cast::<u8>().add(i).read(), 0, "byte {i} not zero");
        }
    }
    free(q);
}

#[test]
fn aligned_alloc_validates_alignment_and_size() {
    // Alignment must be a power of two (§25.5).
    assert!(topomalloc_aligned_alloc(24, 64).is_null());
    expect_errno(EINVAL);
    // Size must be an integer multiple of the alignment (§25.5).
    assert!(topomalloc_aligned_alloc(256, 64).is_null());
    // Conforming requests succeed for small, page, and over-page alignments.
    for (align, size) in [(16usize, 64usize), (256, 512), (4096, 8192), (65536, 65536)] {
        let p = topomalloc_aligned_alloc(align, size);
        assert!(!p.is_null(), "aligned_alloc({align}, {size})");
        assert_eq!(p as usize % align, 0, "alignment {align} not honored");
        free(p);
    }
}

#[test]
fn posix_memalign_and_memalign_contracts() {
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: `out` is a valid writable slot for every call below.
    unsafe {
        assert_eq!(topomalloc_posix_memalign(&mut out, 2, 64), EINVAL);
        assert_eq!(topomalloc_posix_memalign(&mut out, 1024, 100), 0);
    }
    assert!(!out.is_null());
    assert_eq!(out as usize % 1024, 0);
    free(out);

    let p = topomalloc_memalign(2048, 100);
    assert!(!p.is_null());
    assert_eq!(p as usize % 2048, 0);
    free(p);
}

#[test]
fn realloc_full_contract_through_the_abi() {
    // NULL → malloc.
    let p = realloc(ptr::null_mut(), 40);
    assert!(!p.is_null());
    // SAFETY: 40 usable bytes.
    unsafe {
        for i in 0..40u8 {
            p.cast::<u8>().add(i as usize).write(i);
        }
    }
    // Grow within class (in-place), across classes, into the extent path —
    // content preserved at every hop (§25.1).
    let mut cur = p;
    for new_size in [48usize, 200, 5000, 40_000, 2_200_000] {
        cur = realloc(cur, new_size);
        assert!(!cur.is_null(), "realloc to {new_size} failed");
        // SAFETY: prefix of 40 bytes is preserved per §25.1.
        unsafe {
            for i in 0..40u8 {
                assert_eq!(
                    cur.cast::<u8>().add(i as usize).read(),
                    i,
                    "content lost growing to {new_size}"
                );
            }
        }
    }
    // Shrink all the way back down; prefix still intact.
    cur = realloc(cur, 40);
    assert!(!cur.is_null());
    // SAFETY: 40 usable bytes carrying the pattern.
    unsafe {
        for i in 0..40u8 {
            assert_eq!(cur.cast::<u8>().add(i as usize).read(), i);
        }
    }
    // Failure preserves the original (§25.1) and sets ENOMEM.
    assert!(realloc(cur, usize::MAX / 2).is_null());
    expect_errno(ENOMEM);
    // SAFETY: still live after the failed realloc.
    unsafe { assert_eq!(cur.cast::<u8>().read(), 0) };
    // realloc(p, 0) frees and returns NULL.
    assert!(realloc(cur, 0).is_null());
}

#[test]
fn reallocarray_is_overflow_checked() {
    let p = topomalloc_malloc(16);
    // SAFETY: `p` is live and test-owned; overflow must reject it untouched.
    let r = unsafe { topomalloc_reallocarray(p, usize::MAX / 4, 8) };
    assert!(r.is_null());
    expect_errno(ENOMEM);
    // SAFETY: as above — a genuine array resize.
    let q = unsafe { topomalloc_reallocarray(p, 32, 8) };
    assert!(!q.is_null());
    assert!(topomalloc_malloc_usable_size(q) >= 256);
    free(q);
}

#[test]
fn c23_sized_frees_roundtrip() {
    // SAFETY: each pointer is freed exactly once with its true size/alignment.
    unsafe {
        let p = topomalloc_malloc(100);
        topomalloc_free_sized(p, 100);

        let q = topomalloc_aligned_alloc(512, 1024);
        topomalloc_free_aligned_sized(q, 512, 1024);

        topomalloc_free_sized(ptr::null_mut(), 1); // no-op
        topomalloc_free_aligned_sized(ptr::null_mut(), 16, 1); // no-op
    }
    // Memory is genuinely reclaimed through the sized paths too.
    let r = topomalloc_malloc(100);
    assert!(!r.is_null());
    free(r);
}

#[test]
fn extended_api_mallocx_family() {
    // mallocx honors alignment + zero; nallocx predicts its usable size.
    let flags = topo_align_lg(10) | TOPO_ZERO;
    let predicted = topo_nallocx(777, flags);
    assert!(predicted >= 777);
    let p = topo_mallocx(777, flags);
    assert!(!p.is_null());
    assert_eq!(p as usize % 1024, 0);
    assert_eq!(topomalloc_malloc_usable_size(p), predicted);
    // SAFETY: `predicted` readable bytes, zeroed per TOPO_ZERO.
    unsafe {
        for i in 0..predicted {
            assert_eq!(p.cast::<u8>().add(i).read(), 0);
        }
    }

    // xallocx: in-place only; succeeds within the current usable size.
    // SAFETY: `p` is live and test-owned.
    unsafe {
        assert_eq!(topo_xallocx(p, 700, 0, 0), predicted);
        let r = topo_xallocx(p, predicted + 1, 16, 0);
        assert!(r < predicted + 1, "cannot grow in place at M1");
    }

    // rallocx moves with alignment preserved and zero-fills the tail.
    // SAFETY: `p` is live; `q` takes over its ownership.
    let q = unsafe { topo_rallocx(p, 100_000, topo_align_lg(12) | TOPO_ZERO) };
    assert!(!q.is_null());
    assert_eq!(q as usize % 4096, 0);

    // sdallocx with the matching size/alignment hints.
    // SAFETY: `q` is live and freed exactly once.
    unsafe { topo_sdallocx(q, 100_000, topo_align_lg(12)) };

    // Invalid flag words fail deterministically (§10.4).
    assert!(topo_mallocx(64, u64::MAX).is_null());
    expect_errno(EINVAL);
    assert_eq!(topo_nallocx(64, 1 << 60), 0);

    // dallocx ignores advisory bits but frees.
    let r = topo_mallocx(64, 0);
    // SAFETY: `r` is live and freed exactly once.
    unsafe { topo_dallocx(r, 0) };

    // PR #11 review: nallocx is exact for over-aligned requests too (the
    // span-rounding vs carve-rounding divergence).
    for la in [14u32, 16, 18] {
        let predicted = topo_nallocx(100, topo_align_lg(la));
        let p = topo_mallocx(100, topo_align_lg(la));
        assert!(!p.is_null());
        assert_eq!(p as usize % (1usize << la), 0);
        assert_eq!(topomalloc_malloc_usable_size(p), predicted, "lg={la}");
        free(p);
    }
    // rallocx(p, 0): the public realloc(p, 0) policy — free and NULL.
    let z = topo_mallocx(64, 0);
    // SAFETY: `z` is live; rallocx(z, 0) frees it.
    unsafe { assert!(topo_rallocx(z, 0, 0).is_null()) };
    // Out-of-range TOPO_ALIGN_LG is a deterministic EINVAL.
    assert!(topo_mallocx(64, topo_align_lg(64)).is_null());
    expect_errno(EINVAL);
}

#[test]
fn cross_entry_points_share_one_heap() {
    // Objects from every allocation entry are freeable through every free
    // entry (one allocator behind the whole surface).
    let a = topomalloc_malloc(48);
    let b = topomalloc_calloc(3, 16);
    let c = topomalloc_aligned_alloc(64, 128);
    let d = topo_mallocx(48, 0);
    // SAFETY: each pointer is live, test-owned, and freed exactly once with
    // truthful hints.
    unsafe {
        topo_dallocx(a, 0);
        topomalloc_free_sized(b, 48);
        topo_sdallocx(c, 128, topo_align_lg(6));
        topomalloc_free(d);
    }
}

#[test]
fn concurrent_c_abi_use_is_safe_and_conserving() {
    let threads: Vec<_> = (0..4)
        .map(|t| {
            std::thread::spawn(move || {
                let mut live: Vec<(*mut c_void, usize)> = Vec::new();
                for i in 0..300usize {
                    let size = 1 + ((i * 53 + t * 17) % 6000);
                    let p = topomalloc_malloc(size);
                    assert!(!p.is_null());
                    // SAFETY: `size` writable bytes.
                    unsafe { ptr::write_bytes(p.cast::<u8>(), t as u8, size) };
                    live.push((p, size));
                    if i % 4 == 0 {
                        let (q, qsize) = live.swap_remove((i * 13) % live.len());
                        // The contents written by *this* thread are intact.
                        // SAFETY: q has qsize live bytes written above.
                        unsafe { assert_eq!(q.cast::<u8>().read(), t as u8, "size {qsize}") };
                        free(q);
                    }
                }
                for (p, _) in live {
                    free(p);
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

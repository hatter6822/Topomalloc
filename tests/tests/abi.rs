// SPDX-License-Identifier: MIT
//! Cross-crate ABI integration tests (§34.1 "ABI compatibility tests").
//! Exercises the prefixed C entry points exported by `topo-abi`.

use topo_abi::{topomalloc_aligned_alloc, topomalloc_calloc, topomalloc_free, topomalloc_malloc};

#[test]
fn malloc_free_many_distinct_aligned() {
    let mut seen = std::collections::BTreeSet::new();
    for size in [1usize, 7, 16, 24, 100, 4096, 100_000] {
        let p = topomalloc_malloc(size);
        assert!(!p.is_null(), "malloc({size}) returned null");
        assert_eq!(p as usize % 16, 0, "malloc({size}) under-aligned");
        // The skeleton bumps, so every live pointer is distinct.
        assert!(seen.insert(p as usize), "duplicate live pointer");
        // SAFETY: at least `size` writable bytes are available.
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0xa5, size) };
        topomalloc_free(p);
    }
}

#[test]
fn free_null_is_noop() {
    topomalloc_free(std::ptr::null_mut());
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
fn aligned_alloc_validates_alignment_and_size() {
    // Alignment must be a power of two (§25.5).
    assert!(topomalloc_aligned_alloc(24, 64).is_null());
    // Size must be an integer multiple of the alignment (§25.5).
    assert!(topomalloc_aligned_alloc(256, 64).is_null());
    // A conforming request: size is a multiple of the power-of-two alignment.
    let p = topomalloc_aligned_alloc(256, 512);
    assert!(!p.is_null());
    assert_eq!(p as usize % 256, 0);
    topomalloc_free(p);
}

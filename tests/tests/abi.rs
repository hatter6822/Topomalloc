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
    assert!(topomalloc_calloc(usize::MAX, 2).is_null());
    assert!(topomalloc_calloc(1 << 40, 1 << 40).is_null());
}

#[test]
fn aligned_alloc_rejects_non_power_of_two() {
    assert!(topomalloc_aligned_alloc(24, 64).is_null());
    let p = topomalloc_aligned_alloc(256, 64);
    assert!(!p.is_null());
    assert_eq!(p as usize % 256, 0);
    topomalloc_free(p);
}

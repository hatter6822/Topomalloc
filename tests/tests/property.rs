// SPDX-License-Identifier: MIT
//! Property tests (§34.3, plan 08 W21-1) over the M0 skeleton, using `proptest`
//! (D7). The properties checked here are the milestone-independent ones the SPEC
//! lists: no duplicate live pointer, alignment satisfied, stats nonnegative, and
//! monotonic (conserved) usage. Richer ownership-conservation properties arrive
//! with the real caches (plan 05) and the Lean differential oracle (plan 08).

use proptest::prelude::*;

use topo_abi::{topomalloc_calloc, topomalloc_free};
use topo_backend_posix::PosixBackingProvider;
use topo_core::SkeletonAllocator;

proptest! {
    /// Across an arbitrary request stream, every successful allocation is
    /// non-null, correctly aligned, and at a distinct address from every other
    /// live allocation; usage is monotonic and never exceeds capacity.
    #[test]
    fn skeleton_allocations_are_aligned_distinct_and_bounded(
        ops in prop::collection::vec((1usize..=2048, 0u32..=8), 0..120)
    ) {
        let alloc = SkeletonAllocator::new(PosixBackingProvider::new(), 8 * 1024 * 1024)
            .expect("heap");
        let mut live = std::collections::BTreeSet::new();
        let mut prev_used = 0usize;

        for (size, align_exp) in ops {
            let align = 1usize << align_exp; // 1..=256, always a power of two
            let p = alloc.malloc(size, align);
            if !p.is_null() {
                prop_assert_eq!(p as usize % align, 0, "alignment not satisfied");
                prop_assert!(live.insert(p as usize), "duplicate live pointer");
            }
            let used = alloc.used_bytes();
            prop_assert!(used >= prev_used, "usage went backwards");
            prop_assert!(used <= alloc.capacity(), "usage exceeded capacity");
            prev_used = used;
        }
    }
}

proptest! {
    /// `calloc` never under-allocates: an overflowing `n * size` returns null,
    /// never a too-small region (§9.7 / §26.1).
    #[test]
    fn calloc_overflow_is_always_null(
        n in (usize::MAX / 2)..=usize::MAX,
        size in 2usize..=usize::MAX,
    ) {
        prop_assume!(n.checked_mul(size).is_none());
        prop_assert!(topomalloc_calloc(n, size).is_null());
    }
}

proptest! {
    /// Small `calloc` results are fully zeroed (§26.2).
    #[test]
    fn calloc_small_results_are_zeroed(n in 0usize..=64, size in 0usize..=64) {
        let p = topomalloc_calloc(n, size);
        let total = n * size;
        if total == 0 {
            // Zero-size: the skeleton returns a unique non-null pointer
            // (zero_unique, §9.6). Either way it is freeable.
            if !p.is_null() {
                topomalloc_free(p);
            }
        } else {
            prop_assert!(!p.is_null());
            // SAFETY: `p` points to at least `total` zeroed bytes.
            unsafe {
                for i in 0..total {
                    prop_assert_eq!(p.cast::<u8>().add(i).read(), 0);
                }
            }
            topomalloc_free(p);
        }
    }
}

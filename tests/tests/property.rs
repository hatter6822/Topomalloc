// SPDX-License-Identifier: MIT
//! Property tests (§34.3, plan 08 W21-1) over the M0 skeleton, using `proptest`
//! (D7). The properties checked here are the milestone-independent ones the SPEC
//! lists: no duplicate live pointer, alignment satisfied, stats nonnegative, and
//! monotonic (conserved) usage. Richer ownership-conservation properties arrive
//! with the real caches (plan 05) and the Lean differential oracle (plan 08).

use proptest::prelude::*;

use topo_abi::{topomalloc_calloc, topomalloc_free};
use topo_backend_posix::PosixBackingProvider;
use topo_core::classify::RequestKind;
use topo_core::generated::tables::{HUGE_THRESHOLD, MAX_ALIGN, PAGE_SIZE};
use topo_core::size_class::row;
use topo_core::{classify, trace, usable_size, ArenaId, SkeletonAllocator};
use topo_test_support::{parse_trace_line, LiveModel, TraceRecord};

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

proptest! {
    /// The trace spine round-trips: anything `topo_core::trace` emits parses back
    /// to an equal record (the emit and parse sides share one grammar, §33.7).
    #[test]
    fn alloc_trace_roundtrips(
        rid in any::<u32>(),
        size in any::<u32>(),
        align in any::<u32>(),
        arena in any::<u16>(),
        flags in any::<u16>(),
        ptr in any::<u64>(),
        usable in any::<u32>(),
        sc in proptest::option::of(any::<u32>()),
        span in proptest::option::of(any::<u32>()),
    ) {
        let mut s = String::new();
        trace::emit_alloc(
            &mut s, rid as u64, size as usize, align as usize, arena as u32, flags as u32,
            ptr as usize, usable as usize, sc.map(u64::from), span.map(u64::from),
        ).unwrap();
        let parsed = parse_trace_line(s.trim_end()).expect("emitted ALLOC must parse");
        prop_assert_eq!(parsed, TraceRecord::Alloc {
            request_id: rid as u64, size: size as u64, align: align as u64,
            arena: arena as u64, flags: flags as u64, ptr,
            usable_size: usable as u64, sc: sc.map(u64::from), span: span.map(u64::from),
        });
    }
}

proptest! {
    /// Ownership conservation (§34.3): under any well-formed ALLOC/FREE stream the
    /// executable model's live set tracks a reference set exactly, and never
    /// reports a spurious violation. Each slot `k` is the range `[16k, 16k+16)`, so
    /// distinct slots are disjoint — a well-formed stream under the model's
    /// range-disjointness check (§8.3), not merely under address-distinctness.
    #[test]
    fn live_model_matches_reference(ops in prop::collection::vec((any::<bool>(), 1u64..=64), 0..200)) {
        let mut model = LiveModel::new();
        let mut reference = std::collections::BTreeSet::new();
        for (is_alloc, slot) in ops {
            let base = slot * 16; // disjoint 16-byte ranges, one per slot
            if is_alloc {
                if reference.contains(&slot) {
                    continue; // skip re-allocating a still-live slot (its range would overlap itself)
                }
                let rec = TraceRecord::Alloc {
                    request_id: 0, size: 8, align: 8, arena: 0, flags: 0,
                    ptr: base, usable_size: 16, sc: Some(0), span: Some(0),
                };
                model.apply(&rec).expect("well-formed alloc accepted");
                reference.insert(slot);
            } else {
                if !reference.contains(&slot) {
                    continue; // skip would-be free-of-unknown
                }
                let rec = TraceRecord::Free { ptr: base, size_hint: 0, sc: None, span: None };
                model.apply(&rec).expect("well-formed free accepted");
                reference.remove(&slot);
            }
            prop_assert_eq!(model.live_count(), reference.len());
        }
    }
}

proptest! {
    /// Request classification (§A.1, plan 03 W2-3/W2-4) is total, deterministic,
    /// and sound over an arbitrary `(size, align)`: it never panics and never
    /// wraps; whatever it returns satisfies the request with the alignment
    /// honored *naturally* (never by offset-adjusting a shared slab, §9.3/§25.5);
    /// and every medium/large extent is a whole number of allocator pages.
    #[test]
    fn classify_is_total_deterministic_and_sound(
        size in 0usize..=(1usize << 48),
        align_exp in 0u32..=48,
        flags in any::<u32>(),
    ) {
        let align = 1usize << align_exp;
        // Determinism: a pure function of its inputs (W2-3a "deterministic").
        prop_assert_eq!(classify(size, align, flags), classify(size, align, flags));

        let Some(req) = classify(size, align, flags) else { return Ok(()); };
        prop_assert_eq!(req.align, align);
        prop_assert_eq!(req.flags, flags);
        prop_assert_eq!(req.arena, ArenaId::DEFAULT);

        let span = size.max(1).max(align); // effective size folded with alignment
        match req.kind {
            RequestKind::Small { sc, usable } => {
                prop_assert_eq!(usable, usable_size(sc));
                prop_assert!(usable >= size.max(1));
                // The class's natural alignment covers the request and its size is
                // an integer multiple of it — the slab needs no offset adjustment.
                let r = row(sc);
                prop_assert!(r.align as usize >= align);
                prop_assert_eq!(r.size % r.align, 0);
                prop_assert!(align <= MAX_ALIGN);
            }
            RequestKind::Medium { bytes } => {
                prop_assert!(bytes >= span);
                prop_assert_eq!(bytes % PAGE_SIZE, 0);
                prop_assert!(span < HUGE_THRESHOLD);
            }
            RequestKind::Large { bytes } => {
                prop_assert!(bytes >= span);
                prop_assert_eq!(bytes % PAGE_SIZE, 0);
                prop_assert!(span >= HUGE_THRESHOLD);
            }
        }
    }
}

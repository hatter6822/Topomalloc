// SPDX-License-Identifier: MIT
//! Property tests (§34.3, plan 08 W21-1), using `proptest` (D7): the
//! milestone-independent properties the SPEC lists (no duplicate live pointer,
//! alignment satisfied, monotonic skeleton usage), plus the W8 (plan 06) DoD
//! properties over the M1 central-path allocator — realloc content
//! preservation with failure safety (§25.1), calloc zeroing over recycled
//! memory (§26.2), and an allocate/free stream replayed against the
//! `LiveModel` ownership oracle (§33.7).

use proptest::prelude::*;

use topo_abi::{
    topomalloc_calloc, topomalloc_free, topomalloc_malloc, topomalloc_malloc_usable_size,
    topomalloc_realloc,
};
use topo_backend_posix::PosixBackingProvider;
use topo_core::classify::RequestKind;
use topo_core::generated::tables::{HUGE_THRESHOLD, MAX_ALIGN, PAGE_SIZE};
use topo_core::size_class::row;
use topo_core::{classify, trace, usable_size, RequestFlags, SkeletonAllocator};
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
            // Zero-size: a unique non-null pointer under the default
            // zero_unique policy (§9.6). Either way it is freeable.
            if !p.is_null() {
                // SAFETY: `p` is a live zero-size allocation we own.
                unsafe { topomalloc_free(p) };
            }
        } else {
            prop_assert!(!p.is_null());
            // SAFETY: `p` points to at least `total` zeroed bytes.
            unsafe {
                for i in 0..total {
                    prop_assert_eq!(p.cast::<u8>().add(i).read(), 0);
                }
            }
            // SAFETY: `p` is live and owned by this test.
            unsafe { topomalloc_free(p) };
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
        // Include sizes near usize::MAX so the overflow → None path is exercised,
        // not just the comfortable range.
        size in prop_oneof![0usize..=(1usize << 48), (usize::MAX - 65_536)..=usize::MAX],
        align_exp in 0u32..=48,
        // Mostly-valid flag words (no reserved bits) plus some fully-random ones,
        // so both the accept and the §10.4 reject paths get coverage.
        flags in prop_oneof![3 => 0u32..(1u32 << 23), 1 => any::<u32>()],
    ) {
        let align = 1usize << align_exp;
        // Determinism: a pure function of its inputs (W2-3a "deterministic").
        prop_assert_eq!(classify(size, align, flags), classify(size, align, flags));

        // §10.4: an invalid flag word is rejected deterministically. `align` is a
        // valid power of two here, so the only other failure source is overflow.
        if RequestFlags::from_raw(flags).is_none() {
            prop_assert!(classify(size, align, flags).is_none());
            return Ok(());
        }

        let Some(req) = classify(size, align, flags) else { return Ok(()); }; // remaining None ⇒ overflow
        prop_assert_eq!(req.align, align);
        prop_assert_eq!(req.flags.raw(), flags);
        // The arena is decoded from the flag word (§A.1 choose_arena).
        prop_assert_eq!(req.arena, RequestFlags::from_raw(flags).unwrap().arena());

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

proptest! {
    /// The §25.1 realloc contract as a property (plan 06 DD-1 "Verify"): under
    /// an arbitrary chain of grow/shrink steps across the small, medium, and
    /// large families, a written pattern survives in the preserved prefix
    /// `min(old_usable, new_size)` at every step, and an unsatisfiable step
    /// (allocation-failure injection via an impossible size) leaves the
    /// original allocation valid with its contents intact.
    #[test]
    fn realloc_preserves_content_and_survives_failure(
        first in 1usize..=4096,
        steps in prop::collection::vec(
            prop_oneof![
                1usize..=512,            // small targets
                33_000usize..=200_000,   // medium targets
            ],
            1..8,
        ),
        fail_at in proptest::option::of(0usize..8),
    ) {
        /// One byte of recognizable pattern per offset.
        fn pat(i: usize) -> u8 { (i as u8) ^ 0x5A }

        let p = topomalloc_malloc(first);
        prop_assert!(!p.is_null());
        let mut usable = topomalloc_malloc_usable_size(p);
        // SAFETY: `usable` writable bytes.
        unsafe {
            for i in 0..usable {
                p.cast::<u8>().add(i).write(pat(i));
            }
        }
        let mut cur = p;
        for (step, &target) in steps.iter().enumerate() {
            if fail_at == Some(step) {
                // Failure injection: a size whose rounding overflows can never
                // be served (§9.7) — the original must survive untouched.
                // SAFETY: `cur` is live and owned here.
                let failed = unsafe { topomalloc_realloc(cur, usize::MAX - 11) };
                prop_assert!(failed.is_null());
                // SAFETY: `cur` is still live with its full content (§25.1).
                unsafe {
                    for i in 0..usable.min(64) {
                        prop_assert_eq!(cur.cast::<u8>().add(i).read(), pat(i));
                    }
                }
            }
            // SAFETY: `cur` is live and owned; ownership moves to the result.
            let next = unsafe { topomalloc_realloc(cur, target) };
            prop_assert!(!next.is_null());
            let new_usable = topomalloc_malloc_usable_size(next);
            prop_assert!(new_usable >= target);
            let preserved = usable.min(target);
            // SAFETY: `preserved` readable bytes hold the old prefix (§25.4).
            unsafe {
                for i in 0..preserved {
                    prop_assert_eq!(
                        next.cast::<u8>().add(i).read(), pat(i),
                        "lost content at {} growing to {}", i, target
                    );
                }
                // Refresh the pattern across the (possibly larger) usable size
                // so the next step checks against this step's full extent.
                for i in 0..new_usable {
                    next.cast::<u8>().add(i).write(pat(i));
                }
            }
            cur = next;
            usable = new_usable;
        }
        // SAFETY: `cur` is live and owned by this test.
        unsafe { topomalloc_free(cur) };
    }
}

proptest! {
    /// The M1 allocator against the ownership oracle (§33.7 / §8.3): an
    /// arbitrary allocate/free stream through the C ABI, re-emitted in the
    /// trace grammar and replayed through `LiveModel`, never produces an
    /// overlap between live objects (over their *usable* ranges) or a
    /// free-of-unknown — i.e. the engine's address handouts really are
    /// disjoint live ranges with exact ownership hand-back.
    #[test]
    fn engine_stream_replays_clean_against_live_model(
        ops in prop::collection::vec((any::<bool>(), 1usize..=40_000), 1..120)
    ) {
        let mut model = LiveModel::new();
        let mut live: Vec<(*mut std::ffi::c_void, usize)> = Vec::new();
        for (i, (do_alloc, size)) in ops.into_iter().enumerate() {
            if do_alloc || live.is_empty() {
                let p = topomalloc_malloc(size);
                prop_assert!(!p.is_null());
                let usable = topomalloc_malloc_usable_size(p);
                let rec = TraceRecord::Alloc {
                    request_id: i as u64, size: size as u64, align: 16,
                    arena: 0, flags: 0, ptr: p as u64,
                    usable_size: usable as u64, sc: None, span: None,
                };
                prop_assert!(model.apply(&rec).is_ok(), "live ranges overlapped");
                live.push((p, usable));
            } else {
                let (p, _) = live.swap_remove(i % live.len());
                let rec = TraceRecord::Free { ptr: p as u64, size_hint: 0, sc: None, span: None };
                prop_assert!(model.apply(&rec).is_ok(), "free of unknown pointer");
                // SAFETY: `p` is live and owned by this test.
                unsafe { topomalloc_free(p) };
            }
        }
        for (p, _) in live {
            // SAFETY: `p` is live and owned by this test.
            unsafe { topomalloc_free(p) };
        }
    }
}

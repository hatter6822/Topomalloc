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
use topo_core::bootstrap::BumpArena;
use topo_core::classify::RequestKind;
use topo_core::generated::tables::{HUGE_THRESHOLD, MAX_ALIGN, PAGE_SIZE};
use topo_core::size_class::row;
use topo_core::{
    classify, trace, usable_size, ArenaPolicy, ArenaTable, CapRights, Delegation, RequestFlags,
    SkeletonAllocator,
};
use topo_core::{
    DecayConfig, Hotness, HugePageFiller, Lifetime, PlaceHints, PressureMode, ReleaseController,
    ReleaseInputs, HUGEPAGE_SIZE,
};
use topo_core::{NodePressure, Rebalancer, TopologyBuilder};
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
    /// The arena trace events round-trip (§33.7, W9): anything
    /// `topo_core::trace`'s arena emitters write parses back to an equal record,
    /// so the emit and parse sides of the grammar cannot drift.
    #[test]
    fn arena_trace_roundtrips(
        arena in any::<u16>(),
        rights in 0u8..16,
        quota in proptest::option::of(any::<u32>()),
        parent in any::<u16>(),
        reset_gen in any::<u16>(),
        generation in any::<u16>(),
    ) {
        // CREATE
        let mut s = String::new();
        trace::emit_arena_create(&mut s, arena as u64, rights, quota.map(u64::from)).unwrap();
        prop_assert_eq!(
            parse_trace_line(s.trim_end()).unwrap(),
            TraceRecord::ArenaCreate { arena: arena as u64, rights: rights as u64, quota: quota.map(u64::from) }
        );
        // DELEGATE
        let mut s = String::new();
        trace::emit_arena_delegate(&mut s, parent as u64, arena as u64, rights, quota.map(u64::from)).unwrap();
        prop_assert_eq!(
            parse_trace_line(s.trim_end()).unwrap(),
            TraceRecord::ArenaDelegate {
                parent: parent as u64, child: arena as u64, rights: rights as u64, quota: quota.map(u64::from)
            }
        );
        // RESET / DESTROY
        let mut s = String::new();
        trace::emit_arena_reset(&mut s, arena as u64, reset_gen as u64).unwrap();
        prop_assert_eq!(
            parse_trace_line(s.trim_end()).unwrap(),
            TraceRecord::ArenaReset { arena: arena as u64, reset_gen: reset_gen as u64 }
        );
        let mut s = String::new();
        trace::emit_arena_destroy(&mut s, arena as u64, generation as u64).unwrap();
        prop_assert_eq!(
            parse_trace_line(s.trim_end()).unwrap(),
            TraceRecord::ArenaDestroy { arena: arena as u64, generation: generation as u64 }
        );
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

proptest! {
    /// **Capability monotonicity (§36.4/§36.16, plan 06 W9-5).** Over arbitrary
    /// parent/child rights and quotas, a delegation succeeds *iff* it is a sound
    /// attenuation — the child's rights are a subset of the parent's and its
    /// quota is within the parent's remaining budget — and whenever it succeeds
    /// the child can neither widen authority, exceed the parent's remaining
    /// quota, nor downgrade the label. This is the runtime mirror of the Lean
    /// `DelegatesFrom` invariants.
    #[test]
    fn delegation_is_attenuation_only(
        parent_rights in 0u8..16,
        parent_quota in 1u64..1_000_000,
        child_rights in 0u8..16,
        child_quota in 0u64..2_000_000,
    ) {
        let t = ArenaTable::new();
        let prights = CapRights::from_bits(parent_rights).unwrap();
        let parent = t
            .create(&ArenaPolicy::explicit().with_rights(prights).with_quota(parent_quota))
            .unwrap();
        let pstats = t.stats(parent).unwrap();
        let crights = CapRights::from_bits(child_rights).unwrap();
        let del = Delegation::inheriting(&pstats, child_quota, "child").with_rights(crights);

        let result = t.delegate(parent, &del);
        // `inheriting` copies the parent's label, so label monotonicity always
        // holds here; the decisive conditions are a positive quota (a zero quota
        // is an invalid policy, §22.4), attenuating authority, and a quota within
        // the parent's remaining budget.
        let should_succeed = child_quota >= 1
            && crights.attenuates(prights)
            && child_quota <= pstats.remaining_quota();
        prop_assert_eq!(result.is_ok(), should_succeed);

        if let Ok(child) = result {
            let cs = t.stats(child).unwrap();
            prop_assert!(cs.rights.attenuates(pstats.rights), "delegation widened rights");
            prop_assert!(cs.quota_limit <= pstats.remaining_quota(), "delegation widened quota");
            prop_assert_eq!(cs.label, pstats.label, "delegation downgraded the label");
            prop_assert_eq!(cs.parent, Some(parent));
        }
    }
}

proptest! {
    /// **Tree-wide quota monotonicity (§36.4, PR #13).** Delegation reserves each
    /// child's quota on its parent, so the *whole subtree's* own-live bytes never
    /// exceed the parent's quota — however the budget is split across children and
    /// whatever each child (or the parent) then allocates. This is the runtime
    /// mirror of the Lean bridge's `subtree_used_le_quota` theorem; without the
    /// reservation a parent and child could each allocate the full quota (the
    /// PR #13 P1).
    #[test]
    fn delegated_subtree_never_exceeds_parent_quota(
        parent_quota in 1u64..10_000,
        parent_charge in 0u64..10_000,
        child_quotas in prop::collection::vec(0u64..4_000, 0..8),
        child_charges in prop::collection::vec(0u64..4_000, 0..8),
    ) {
        let t = ArenaTable::new();
        let parent = t.create(&ArenaPolicy::explicit().with_quota(parent_quota)).unwrap();
        let mut children = Vec::new();
        for &q in &child_quotas {
            let pstats = t.stats(parent).unwrap();
            let before = pstats.remaining_quota();
            match t.delegate(parent, &Delegation::inheriting(&pstats, q, "c")) {
                Ok(child) => {
                    // A successful delegation reserves exactly `q`: the parent's
                    // remaining budget drops by the child's quota, no more.
                    prop_assert_eq!(t.stats(parent).unwrap().remaining_quota(), before - q);
                    children.push(child);
                }
                // A delegation fails only for a zero quota (an arena that can
                // never allocate is an invalid policy, §22.4) or a quota exceeding
                // the parent's (reservation-aware) remaining budget — never within
                // budget for a valid quota.
                Err(_) => prop_assert!(
                    q == 0 || q > before,
                    "a valid delegation within budget must succeed"
                ),
            }
        }
        // Charge the parent and each child arbitrarily; over-budget charges simply
        // fail and change nothing.
        let _ = t.try_charge(parent, parent_charge);
        for (child, &c) in children.iter().zip(child_charges.iter()) {
            let _ = t.try_charge(*child, c);
        }
        // The guarantee: Σ over the subtree of own-live bytes ≤ the parent's quota.
        let subtree_used: u64 = core::iter::once(parent)
            .chain(children.iter().copied())
            .map(|id| t.stats(id).unwrap().used)
            .sum();
        prop_assert!(
            subtree_used <= parent_quota,
            "subtree used {} exceeded parent quota {}",
            subtree_used,
            parent_quota
        );
        prop_assert!(t.check_invariants());
    }
}

proptest! {
    /// **Quota is never exceeded (§36.4, plan 06 W9).** Under an arbitrary stream
    /// of charges against a fixed ceiling, the arena's used bytes never exceed
    /// the quota and never wrap — a charge either fits and is recorded or is
    /// refused, deterministically.
    #[test]
    fn arena_quota_is_never_exceeded(
        quota in 1u64..100_000,
        charges in prop::collection::vec(1u64..20_000, 0..40),
    ) {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit().with_quota(quota)).unwrap();
        for c in charges {
            let before = t.stats(id).unwrap().used;
            let r = t.try_charge(id, c);
            let after = t.stats(id).unwrap().used;
            prop_assert!(after <= quota, "used {} exceeded quota {}", after, quota);
            match r {
                Ok(()) => prop_assert_eq!(after, before + c, "a successful charge records exactly"),
                Err(_) => prop_assert_eq!(after, before, "a refused charge changes nothing"),
            }
        }
        prop_assert!(t.check_invariants());
    }
}

proptest! {
    /// **Reset accounting (§22.5/B.5, plan 06 W9-4c).** After a reset, the
    /// arena's used bytes are zero and its generation has advanced (so stale
    /// references are detectable) — regardless of how much was charged first.
    #[test]
    fn arena_reset_zeroes_used_and_bumps_generation(
        charges in prop::collection::vec(1u64..50_000, 0..30),
    ) {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit()).unwrap(); // unlimited quota
        let rg0 = t.stats(id).unwrap().reset_generation;
        let inc0 = t.stats(id).unwrap().generation;
        for c in charges {
            let _ = t.try_charge(id, c);
        }
        t.begin_reset(id).unwrap();
        let rg1 = t.finish_reset(id).unwrap();
        prop_assert_eq!(t.stats(id).unwrap().used, 0, "B.5: no live bytes after reset");
        prop_assert_ne!(rg1, rg0, "reset must bump the reset generation (§22.5)");
        prop_assert_eq!(t.stats(id).unwrap().generation, inc0, "reset keeps the incarnation");
        prop_assert!(t.is_active(id), "reset returns the arena to Active (§22.5)");
    }
}

proptest! {
    /// **Release-controller safety invariants under arbitrary op streams (§20–§21,
    /// plan 04 W12).** Across any sequence of ticks (varying time, supply, churn, and
    /// pressure), the [`ReleaseController`] never plans to release more than exists,
    /// honors the Emergency trigger, and keeps its accounting monotonic — the policy
    /// can be arbitrarily *tuned* but must never over-promise a rung's supply (a
    /// release-safety analogue of §2.4: a bad policy never releases memory it lacks).
    #[test]
    fn release_controller_plan_never_exceeds_supply(
        ops in prop::collection::vec(
            (
                0u32..5_000,            // dt_ms between ticks
                0u64..16,               // empty-backed hugepages (units)
                0u64..(8 << 20),        // dirty bytes
                0u64..(8 << 20),        // idle cache bytes
                0u64..(8 << 20),        // cold-sparse bytes
                0u64..(8 << 20),        // muzzy bytes
                0u64..(128 << 20),      // allocation delta since the last tick
                0u32..=100,             // cgroup utilization %
                any::<bool>(),          // alloc_failed
            ),
            0..120,
        ),
    ) {
        let mut c = ReleaseController::new(DecayConfig::low_rss());
        let mut now: u64 = 0;
        let mut allocated_total: u64 = 0;
        let mut prev_planned: u64 = 0;

        for (dt, empty_hp, dirty, idle, cold_sparse, muzzy, alloc_delta, cg, failed) in ops {
            now += dt as u64;
            allocated_total = allocated_total.saturating_add(alloc_delta);
            let empty_backed = empty_hp * HUGEPAGE_SIZE as u64;
            let inputs = ReleaseInputs {
                dirty_bytes: dirty,
                idle_cache_bytes: idle,
                cold_sparse_bytes: cold_sparse,
                muzzy_bytes: muzzy,
                empty_backed_hugepage_bytes: empty_backed,
                allocated_bytes_total: allocated_total,
                cgroup_current: cg as u64,
                cgroup_max: 100,
                alloc_failed: failed,
                ..ReleaseInputs::default()
            };
            let plan = c.tick(now, inputs);

            // No rung over-promises its supply (the §21.3 mechanisms could not honor it).
            prop_assert!(plan.drain_caches_bytes <= idle, "drain ≤ idle cache");
            prop_assert!(
                plan.release_empty_hugepages_bytes <= empty_backed,
                "release ≤ empty-backed supply"
            );
            prop_assert!(plan.purge_dirty_bytes <= dirty, "purge ≤ dirty");
            prop_assert!(plan.dirty_to_muzzy_bytes <= dirty, "dirty→muzzy ≤ dirty");
            prop_assert!(
                plan.subrelease_cold_sparse_bytes <= cold_sparse,
                "subrelease ≤ cold-sparse supply"
            );
            prop_assert!(plan.emergency_shrink_bytes <= muzzy, "emergency shrink ≤ muzzy");
            // Purge and the dirty→muzzy lazy conversion partition the dirty supply: the
            // two together never exceed it (no double-counting the same dirty bytes).
            prop_assert!(
                plan.purge_dirty_bytes + plan.dirty_to_muzzy_bytes <= dirty,
                "purge + dirty→muzzy ≤ dirty (no double count)"
            );
            // The Emergency trigger (O-007) is honored, and only then is the reserve
            // dropped to zero by mode (idle may also yield a zero reserve).
            if failed {
                prop_assert_eq!(plan.mode, PressureMode::Emergency, "alloc failure ⇒ Emergency");
                prop_assert!(plan.disable_hugecache_reserve, "Emergency disables the reserve");
                prop_assert_eq!(plan.demand_reserve_bytes, 0, "Emergency reserves nothing");
            }
            // The plan mode matches the controller's tracked mode.
            prop_assert_eq!(plan.mode, c.mode());
            // Cumulative planned bytes never decrease (a monotonic activity counter).
            let s = c.stats();
            prop_assert!(s.planned_bytes_total >= prev_planned, "planned total monotonic");
            prev_planned = s.planned_bytes_total;
        }
    }
}

proptest! {
    /// **HugePageFiller invariants under arbitrary op streams (§19.8 / B.4, plan 04
    /// W11).** A *gating* companion to the nightly `huge_filler` fuzz target: under any
    /// sequence of place (at varied power-of-two alignments) / free / subrelease /
    /// unsubrelease / whole-hugepage reserve / free-run / mark-cold operations, the
    /// filler stays well-formed (H-001/H-003/H-005 via `check_invariants`) and its
    /// §19.7 coverage reconciles after *every* op — live == intact + partial, and the
    /// nine §19.4 bins sum to the touched-hugepage count (H-003: each touched hugepage
    /// is in exactly one bin). Draining every live allocation returns it to empty.
    #[test]
    fn hugepage_filler_stays_well_formed_and_reconciles(
        ops in prop::collection::vec((0u8..6, any::<u8>()), 0..200),
    ) {
        const CAP: usize = 6;
        let base = HUGEPAGE_SIZE * 16; // hugepage-aligned synthetic region base
        // One metadata buffer per case (dropped at the end of the case — no leak).
        // `buf` outlives `arena`, which outlives `f` (reverse-declaration drop order),
        // and a 1 MiB arena dwarfs the CAP-slot descriptor pool + region cache.
        let mut buf = vec![0u8; 1 << 20];
        // SAFETY: `buf` outlives `arena`/`f`, and is never moved or aliased below.
        let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
        let mut f = HugePageFiller::new(&arena, base, CAP).expect("filler construction");
        prop_assert!(f.check_invariants());

        let hints = |b: u8| PlaceHints {
            hotness: match b % 3 {
                0 => Hotness::Cold,
                1 => Hotness::Hot,
                _ => Hotness::Neutral,
            },
            lifetime: match (b / 3) % 4 {
                0 => Lifetime::Short,
                1 => Lifetime::Medium,
                2 => Lifetime::Long,
                _ => Lifetime::Unspecified,
            },
            home_node: None,
        };
        // Live sub-hugepage runs (freed via `free`) and whole-hugepage runs (freed via
        // `free_hugepages`), tracked separately so frees target real, live allocations.
        let mut live: Vec<(usize, usize)> = Vec::new();
        let mut runs: Vec<(usize, usize)> = Vec::new();

        for (op, b) in ops {
            match op {
                // place a 1..=16 page run at a varied power-of-two alignment
                0 | 1 => {
                    let pages = (b as usize % 16) + 1;
                    let align = PAGE_SIZE << ((b >> 5) & 0b11); // PAGE..=8*PAGE
                    if let Some(p) = f.place(pages, align, hints(b)) {
                        f.mark_committed(&p);
                        live.push((p.base, p.pages));
                    }
                }
                // free a live run; sometimes subrelease it, sometimes roll that back
                2 => {
                    if !live.is_empty() {
                        let i = b as usize % live.len();
                        let (bb, pp) = live.swap_remove(i);
                        prop_assert!(f.free(bb, pp).valid);
                        if b & 0x40 != 0 {
                            if let Some(sr) = f.subrelease(bb, pp, b & 0x80 != 0) {
                                if b & 0x20 != 0 {
                                    f.unsubrelease(&sr);
                                }
                            }
                        }
                    }
                }
                // reserve a 1..=3 hugepage whole-run (the §19.2 large/awkward path)
                3 => {
                    let n = (b as usize % 3) + 1;
                    if let Some(run) = f.reserve_hugepages(n) {
                        f.mark_run_committed(&run);
                        runs.push((run.base, run.hugepages));
                    }
                }
                // free a whole-hugepage run
                4 => {
                    if !runs.is_empty() {
                        let i = b as usize % runs.len();
                        let (bb, hh) = runs.swap_remove(i);
                        prop_assert!(f.free_hugepages(bb, hh).valid);
                    }
                }
                // mark a hugepage cold (the W12 idle-decay hook)
                _ => {
                    let _ = f.mark_cold(base + (b as usize % CAP) * HUGEPAGE_SIZE);
                }
            }
            // The §19.8 / B.4 invariant and the §19.7 reconciliation hold after each op.
            prop_assert!(f.check_invariants(), "filler invariant violated");
            let cov = f.coverage();
            prop_assert_eq!(
                cov.live_total_bytes,
                cov.live_bytes_on_intact + cov.live_bytes_on_partial,
                "live must split into intact + partial"
            );
            let binned: u32 = cov.bins.iter().sum();
            prop_assert_eq!(
                binned as u64,
                cov.coverage_bytes / HUGEPAGE_SIZE as u64,
                "the nine bins must sum to the touched-hugepage count (H-003)"
            );
        }

        // Draining both allocation kinds returns the filler to empty, well-formed.
        for (bb, pp) in live {
            prop_assert!(f.free(bb, pp).valid);
        }
        for (bb, hh) in runs {
            prop_assert!(f.free_hugepages(bb, hh).valid);
        }
        prop_assert!(f.check_invariants());
        prop_assert_eq!(f.coverage().live_total_bytes, 0, "drained to empty");
    }
}

proptest! {
    /// **Rebalancer surplus-safety + convergence under arbitrary topologies (§15.4,
    /// plan 04 W13).** For any node count, distance matrix, and per-node (free, demand),
    /// driving [`Rebalancer::plan`] to a fixpoint: every move is bounded by the donor's
    /// *surplus* (free beyond its own demand) so applying it can neither underflow nor
    /// strand the donor, the total unmet need strictly decreases by exactly the move each
    /// step (monotone progress), and the loop converges in `≤ nodes` moves — the §15.4
    /// "never permanently stranded" guarantee with no churn, the randomized companion to
    /// the unit fixpoint test.
    #[test]
    fn rebalancer_never_strands_a_donor_and_converges(
        n_nodes in 2u32..=8,
        // (free, demand) per node (indices 0..n_nodes used), and a flat distance matrix.
        pressures in prop::collection::vec((0u64..(1u64 << 16), 0u64..(1u64 << 16)), 8),
        dists in prop::collection::vec(10u8..=40, 8 * 8),
    ) {
        // Build an n-node topology (one CPU per node) over the random distance matrix.
        let mut b = TopologyBuilder::new(n_nodes);
        for c in 0..n_nodes {
            b.set_cpu(c, c, c);
        }
        for a in 0..n_nodes {
            for d in 0..n_nodes {
                b.set_distance(a, d, dists[(a * n_nodes + d) as usize]);
            }
        }
        let topo = b.build();
        // Only a genuine multi-node snapshot can rebalance; the build keeps the shape we
        // constructed (every CPU set, nodes 0..n_nodes), so this holds — assert to be sure.
        prop_assert_eq!(topo.node_count(), n_nodes);

        let mut nodes: Vec<NodePressure> = (0..n_nodes as usize)
            .map(|i| NodePressure {
                free_bytes: pressures[i].0,
                demand_bytes: pressures[i].1,
            })
            .collect();
        let total_need = |ns: &[NodePressure]| -> u64 { ns.iter().map(|p| p.unmet_need()).sum() };

        let mut prev_total = total_need(&nodes);
        let mut moves = 0u32;
        while let Some(m) = Rebalancer::plan(&nodes, &topo) {
            let (s, d) = (m.src.0 as usize, m.dst.0 as usize);
            prop_assert_ne!(s, d, "donor and recipient are distinct nodes");
            prop_assert!((s as u32) < n_nodes && (d as u32) < n_nodes, "indices in range");
            prop_assert!(m.bytes > 0, "a planned move transfers ≥ 1 byte");
            prop_assert!(
                m.bytes <= nodes[s].movable_surplus(),
                "the move is bounded by the donor's surplus"
            );
            prop_assert!(
                m.bytes <= nodes[d].unmet_need(),
                "the move is bounded by the recipient's need"
            );

            // Apply it: the donor parts with surplus, relieving the recipient's demand.
            nodes[s].free_bytes -= m.bytes;
            nodes[d].demand_bytes -= m.bytes;

            prop_assert_eq!(nodes[s].unmet_need(), 0, "a move never strands the donor");
            let now_total = total_need(&nodes);
            prop_assert_eq!(
                now_total,
                prev_total - m.bytes,
                "total unmet need drops by exactly the moved bytes"
            );
            prop_assert!(now_total < prev_total, "strict progress each move (no churn)");
            prev_total = now_total;

            moves += 1;
            prop_assert!(moves <= n_nodes, "converges in ≤ nodes moves");
        }

        // Fixpoint: nothing relievable is left — no node has need, or none has surplus.
        let any_need = nodes.iter().any(|p| p.unmet_need() > 0);
        let any_surplus = nodes.iter().any(|p| p.movable_surplus() > 0);
        prop_assert!(!any_need || !any_surplus, "fixpoint has no relievable stranding");
    }
}

// SPDX-License-Identifier: MIT
//! Hugepage-backend integration tests (§19, plan 04 W11). The in-crate
//! `topo_core::huge` tests cover the filler and the provider-driven backend in
//! isolation; this file adds the **cross-crate live data path**: the M1 large
//! allocator (`topo_core::large::LargeAllocator`) served by the hugepage backend
//! through the §18.6 [`RegionCacheHook`] seam — proving W11 is wired end to end,
//! exactly as the W10 extent-hook tests prove the custom-backing seam.
//!
//! What is exercised:
//!
//! * **W11-3 seam:** a medium/large request flows `large_allocate → RegionCacheHook
//!   → HugePageFiller`, is **classifiable** (pagemap → `usable_size`), **writable**
//!   (the backend committed it), and on free is **returned to the filler** — never to
//!   the extent manager;
//! * **W11-2a/W11-4a packing:** several sub-hugepage allocations pack into one
//!   hugepage; coverage reflects it (H-002/H-003);
//! * **W11-1a/W11-3 large/awkward path:** an allocation larger than a hugepage is a
//!   contiguous run, and freeing then re-allocating the same awkward size is a
//!   region-cache hit (bounded waste);
//! * **W11-5 coverage** reconciles into the `topo_stats` snapshot/JSON;
//! * **W11-6 backend-agnostic:** the identical backend runs over POSIX here; the
//!   filler model assumes only contiguous page runs (the seLe4n slice reuses it).

use topo_backend_posix::PosixBackingProvider;
use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::ids::ArenaId;
use topo_core::{
    HugeConfig, HugePageBackend, LargeAllocator, LargeConfig, PageMap, RegionCacheHook,
    HUGEPAGE_SIZE, PAGES_PER_HUGEPAGE,
};

const PAGE: usize = PAGE_SIZE;

/// A leaked heap metadata arena, valid for the process (the shared test pattern) so
/// vended slot pools outlive every allocator built over them.
fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

fn pagemap() -> &'static PageMap {
    Box::leak(Box::new(PageMap::new()))
}

/// A large allocator (the M1 large path) over a real POSIX backing.
fn large_allocator(pm: &'static PageMap) -> LargeAllocator<'static, PosixBackingProvider> {
    LargeAllocator::new(
        PosixBackingProvider::new(),
        meta(1 << 20),
        pm,
        ArenaId::DEFAULT,
        LargeConfig {
            region_bytes: 64 * 1024 * 1024,
            region_align: PAGE,
            extent_slots: 4096,
            large_slots: 256,
        },
    )
    .expect("large allocator")
}

/// A hugepage backend (the filler) over its own real POSIX backing, usable as a
/// [`RegionCacheHook`] for the large path.
fn huge_backend(capacity: usize) -> HugePageBackend<PosixBackingProvider> {
    HugePageBackend::new(
        PosixBackingProvider::new(),
        meta(1 << 20),
        ArenaId::DEFAULT,
        HugeConfig::with_capacity(capacity),
    )
    .expect("hugepage backend")
}

/// Allocate through the large path served by the hugepage backend, asserting the
/// allocation is backed by the hugepage region (not the large allocator's extents).
fn alloc_huge(
    la: &LargeAllocator<'_, PosixBackingProvider>,
    hp: &HugePageBackend<PosixBackingProvider>,
    bytes: usize,
    align: usize,
) -> *mut u8 {
    let (lo, hi) = hp.reserved_region().addr_range();
    let p = la.allocate_with(bytes, align, hp);
    if !p.is_null() {
        let a = p as usize;
        assert!(
            a >= lo && a < hi,
            "served from the hugepage region, not the extents"
        );
    }
    p
}

/// Free a large allocation served by the hugepage backend (returning it to the filler).
///
/// # Safety
/// `p` is a base pointer this large allocator handed out from `hp` (or null).
unsafe fn free_huge(
    la: &LargeAllocator<'_, PosixBackingProvider>,
    hp: &HugePageBackend<PosixBackingProvider>,
    p: *mut u8,
) -> bool {
    // SAFETY: forwarded caller contract.
    unsafe { la.free_with(p, hp) }
}

#[test]
fn large_path_served_by_filler_is_classifiable_writable_and_freeable() {
    // W11-3 seam end to end: a medium request flows through the RegionCacheHook into
    // the filler, classifies via the pagemap, is writable, and frees back to the
    // filler — never touching the large allocator's extent manager.
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(4);

    let bytes = 256 * 1024; // 16 pages — sub-hugepage, packed by the filler
    let p = alloc_huge(&la, &hp, bytes, PAGE);
    assert!(!p.is_null());
    assert_eq!(p as usize % PAGE, 0);
    // Classifiable through the shared pagemap.
    assert_eq!(la.usable_size(p), Some(bytes));
    assert_eq!(la.live_count(), 1);
    // The filler accounts the live bytes (H-002).
    assert_eq!(hp.coverage().live_total_bytes, bytes as u64);
    assert_eq!(hp.touched_hugepages(), 1);
    // Writable for its whole length (the backend committed it).
    // SAFETY: `p` is committed backing of `bytes` bytes.
    unsafe {
        std::ptr::write_bytes(p, 0x5a, bytes);
        assert_eq!(*p, 0x5a);
        assert_eq!(*p.add(bytes - 1), 0x5a);
    }
    // Free returns it to the filler (coverage drops to zero).
    // SAFETY: `p` is the live base pointer just handed out.
    let freed = unsafe { free_huge(&la, &hp, p) };
    assert!(freed, "free of a live region succeeds");
    assert_eq!(la.live_count(), 0);
    assert_eq!(la.usable_size(p), None);
    assert_eq!(hp.coverage().live_total_bytes, 0);
    assert!(hp.check_invariants());
    assert!(la.check_invariants());
}

#[test]
fn sub_hugepage_allocations_pack_into_one_hugepage() {
    // W11-2a/W11-4a: several sub-hugepage allocations pack into the same hugepage
    // (filling before opening a new one), and coverage reconciles.
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(4);

    let each = 128 * 1024; // 8 pages each
    let ptrs: Vec<*mut u8> = (0..4).map(|_| alloc_huge(&la, &hp, each, PAGE)).collect();
    for &p in &ptrs {
        assert!(!p.is_null());
    }
    assert_eq!(
        hp.touched_hugepages(),
        1,
        "four 8-page allocs pack into one hugepage"
    );
    assert_eq!(hp.coverage().live_total_bytes, 4 * each as u64);
    // Distinct, non-overlapping.
    let mut bases: Vec<usize> = ptrs.iter().map(|&p| p as usize).collect();
    bases.sort_unstable();
    for w in bases.windows(2) {
        assert!(w[1] - w[0] >= each, "allocations do not overlap");
    }
    for &p in &ptrs {
        // SAFETY: `p` is a live base pointer just handed out.
        assert!(unsafe { free_huge(&la, &hp, p) });
    }
    assert_eq!(hp.coverage().live_total_bytes, 0);
    assert!(hp.check_invariants());
}

#[test]
fn larger_than_hugepage_is_a_run_then_region_cache_reuses_it() {
    // W11-1a/W11-3: an allocation larger than a hugepage is a contiguous run; freeing
    // then re-allocating the same awkward size is a region-cache hit (no extra
    // hugepages reserved — bounded rounding waste).
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(8);

    let bytes = HUGEPAGE_SIZE + 64 * 1024; // just over one hugepage ⇒ rounds to 2
    let p = alloc_huge(&la, &hp, bytes, PAGE);
    assert!(!p.is_null());
    assert_eq!(p as usize % HUGEPAGE_SIZE, 0, "hugepage-aligned run base");
    assert_eq!(la.usable_size(p), Some(bytes));
    let touched = hp.touched_hugepages();
    assert_eq!(touched, 2, "two hugepages for the awkward run");
    // SAFETY: `p` is a live base pointer just handed out.
    assert!(unsafe { free_huge(&la, &hp, p) });
    // Re-allocate the same awkward size: a region-cache hit reuses the run.
    let q = alloc_huge(&la, &hp, bytes, PAGE);
    assert_eq!(q, p, "region-cache hit reuses the freed run");
    assert_eq!(
        hp.touched_hugepages(),
        touched,
        "no additional reservation (waste bounded)"
    );
    // SAFETY: `q` is a live base pointer just handed out.
    assert!(unsafe { free_huge(&la, &hp, q) });
    assert!(hp.check_invariants());
}

#[test]
fn coverage_metrics_reconcile_into_stats_json() {
    // W11-5: the §19.7 coverage of a live hugepage backend flows into the topo_stats
    // snapshot/JSON with a computed ratio.
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(4);

    let p = alloc_huge(&la, &hp, 512 * 1024, PAGE);
    assert!(!p.is_null());
    let cov = hp.coverage();
    assert_eq!(cov.live_bytes_on_intact, 512 * 1024);
    assert_eq!(cov.coverage_bytes, HUGEPAGE_SIZE as u64);
    // All live is intact (no subrelease) ⇒ ratio 100% (10000 bp).
    assert_eq!(cov.coverage_ratio_bp(), 10_000);
    let mut s = topo_stats::Stats::default();
    s.record_huge(cov);
    let json = s.to_json();
    // The §19.7 fields flow into the JSON snapshot with the computed ratio.
    assert!(
        json.contains("\"live_bytes_on_intact_hugepages\": 524288"),
        "{json}"
    );
    assert!(json.contains("\"coverage_ratio_bp\": 10000"), "{json}");
    assert!(json.contains(&format!("\"coverage_bytes\": {}", HUGEPAGE_SIZE)));
    // SAFETY: `p` is a live base pointer just handed out.
    assert!(unsafe { free_huge(&la, &hp, p) });
    assert!(hp.check_invariants());
}

#[test]
fn churn_through_the_seam_keeps_both_layers_well_formed() {
    // A mixed churn workload through the live seam keeps the filler and the large
    // allocator well-formed and never leaks (the integration stress).
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(8);

    let mut live: Vec<*mut u8> = Vec::new();
    let mut rng = 0x9e37_79b9u64;
    for _ in 0..600 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let do_alloc = live.is_empty() || (rng & 1 == 0);
        if do_alloc {
            // A mix of sub-hugepage and just-over-hugepage sizes. When the hugepage
            // region fills, the large path *correctly* falls through to its extents,
            // so we don't assert region membership here — the free path routes each
            // allocation back to whichever backend served it (via `has_extent`).
            let bytes = if rng & 2 == 0 {
                (1 + (rng as usize % (PAGES_PER_HUGEPAGE / 2))) * PAGE
            } else {
                HUGEPAGE_SIZE + ((rng as usize % 8) + 1) * PAGE
            };
            let p = la.allocate_with(bytes, PAGE, &hp);
            if !p.is_null() {
                // SAFETY: committed backing of at least one page.
                unsafe { std::ptr::write_bytes(p, 0x11, PAGE) };
                live.push(p);
            }
        } else {
            let idx = (rng as usize) % live.len();
            let p = live.swap_remove(idx);
            // SAFETY: `p` is a live base pointer this allocator handed out.
            assert!(unsafe { free_huge(&la, &hp, p) });
        }
        assert!(hp.check_invariants());
        assert!(la.check_invariants());
    }
    for p in live {
        // SAFETY: `p` is a live base pointer this allocator handed out.
        assert!(unsafe { free_huge(&la, &hp, p) });
    }
    assert_eq!(la.live_count(), 0);
    assert_eq!(hp.coverage().live_total_bytes, 0);
    assert!(hp.check_invariants());
}

#[test]
fn declined_request_falls_through_to_the_extent_manager() {
    // A request the hugepage backend declines (alignment larger than a hugepage) must
    // fall through to the large allocator's own extents — the seam degrades cleanly.
    let pm = pagemap();
    let la = large_allocator(pm);
    let hp = huge_backend(4);
    let (lo, hi) = hp.reserved_region().addr_range();

    // The hugepage backend cannot satisfy align > HUGEPAGE_SIZE within a hugepage.
    assert!(hp.try_alloc(64 * 1024, HUGEPAGE_SIZE * 2).is_none());
    // Through the large path, the same request is served by the extent manager.
    let p = la.allocate_with(64 * 1024, HUGEPAGE_SIZE * 2, &hp);
    assert!(!p.is_null());
    let a = p as usize;
    assert!(
        a < lo || a >= hi,
        "served by the extents, not the hugepage region"
    );
    assert_eq!(
        a % (HUGEPAGE_SIZE * 2),
        0,
        "alignment honored by the extents"
    );
    // SAFETY: `p` is a live base pointer just handed out.
    assert!(unsafe { la.free_with(p, &hp) });
    assert!(hp.check_invariants());
    assert!(la.check_invariants());
}

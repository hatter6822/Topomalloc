// SPDX-License-Identifier: MIT
//! Back-end extent integration + property tests (§18, plan 04 W4-2/W4-5, DD-1
//! "Verify"). The pure bookkeeping and provider-integration unit tests live in
//! `topo-core::extent`; this file adds the *cross-crate* properties: the extent
//! manager over the real POSIX provider, random split/merge/alloc/free sequences
//! that must keep the region tiled and live allocations disjoint, failure
//! injection that must leave the back-end well-formed (W4-5), the extent→pagemap
//! soundness tie (W4-2b / W3-6), and — under `--features sele4n-sim` — dual-backend
//! equivalence of the back-end over POSIX and the seLe4n simulator (D2 / G-sim).

use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use topo_backend_posix::PosixBackingProvider;
use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{
    ArenaId, BackendError, ExtentManager, ExtentMap, ExtentRef, ExtentState, Fit, LargeDescriptor,
    LargeId, NoRegionCache, PageMap, Region, TopoBackingProvider,
};

const PAGE: usize = PAGE_SIZE;

/// A leaked heap metadata arena, valid for the process (the `pagemap`/`span` test
/// pattern) so vended slot pools and descriptors outlive every map built over them.
fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

// ===========================================================================
// A POSIX provider wrapper with deterministic failure injection (W4-5).
// ===========================================================================

/// Wraps the POSIX provider and fails `commit`/`decommit` every `period`-th call,
/// so a property test can confirm the back-end stays well-formed under provider
/// failure (§36.6 / W4-5). `period == 0` disables injection.
struct FaultyProvider {
    inner: PosixBackingProvider,
    period: u64,
    calls: AtomicU64,
}

impl FaultyProvider {
    fn new(period: u64) -> Self {
        Self {
            inner: PosixBackingProvider::new(),
            period,
            calls: AtomicU64::new(0),
        }
    }
    fn should_fail(&self) -> bool {
        if self.period == 0 {
            return false;
        }
        let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        n.is_multiple_of(self.period)
    }
}

impl TopoBackingProvider for FaultyProvider {
    fn reserve(&self, arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        self.inner.reserve(arena, size, align)
    }
    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        if self.should_fail() {
            return Err(BackendError::OutOfMemory);
        }
        self.inner.commit(region, offset, len)
    }
    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        if self.should_fail() {
            return Err(BackendError::OutOfMemory);
        }
        self.inner.decommit(region, offset, len)
    }
    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        self.inner.purge_lazy(region, offset, len)
    }
    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        self.inner.purge_forced(region, offset, len)
    }
    fn release(&self, arena: ArenaId, region: Region) -> Result<(), BackendError> {
        self.inner.release(arena, region)
    }
    fn name(&self) -> &'static str {
        "posix-faulty"
    }
}

// ===========================================================================
// Deterministic integration tests.
// ===========================================================================

fn manager(pages: usize, slots: usize) -> ExtentManager<PosixBackingProvider> {
    ExtentManager::new(
        PosixBackingProvider::new(),
        meta(1 << 20),
        ArenaId::DEFAULT,
        pages * PAGE,
        PAGE,
        slots,
    )
    .expect("manager")
}

#[test]
fn extent_alloc_installs_a_classifiable_large_region_and_retires_it() {
    // W4-2b / W3-6: an allocated extent's range can carry a `LargeDescriptor` in the
    // classification pagemap; a pointer inside it classifies `Large`, and after the
    // allocation is freed and the entry retired it classifies `Empty` — the
    // extent↔pagemap soundness tie. (The pagemap is the single mutator, W3-6.)
    let mgr = manager(64, 4096);
    let r = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc");
    let region = mgr.region_of(r).expect("region");

    let pm = PageMap::new();
    let m = meta(1 << 16);
    // The descriptor lives in leaked metadata so the pagemap pointer stays valid.
    let large: &'static LargeDescriptor = Box::leak(Box::new(LargeDescriptor::new(
        LargeId(1),
        ArenaId::DEFAULT,
        region.base as usize,
        region.len,
        PAGE,
    )));
    pm.install_large(m, large).expect("install");

    // A pointer inside the extent resolves to the large descriptor.
    let probe = region.base as usize + PAGE; // second page, interior
    assert_eq!(
        pm.lookup(probe).large_ptr(),
        Some(large as *const LargeDescriptor)
    );
    // Outside the extent is non-owned.
    let after = region.base as usize + region.len;
    assert!(pm.lookup(after).is_empty());

    // Retire (the W3-6 mutator) and free; classification falls back to Empty.
    pm.retire_large(large);
    assert!(pm.lookup(probe).is_empty());
    mgr.free(r).expect("free");
    assert!(mgr.check_invariants());
}

#[test]
fn large_path_rounds_overflow_safely_and_bypasses_small_path() {
    let mgr = manager(64, 4096);
    // §18.5 / §9.7: an awkward size rounds up to whole pages, never wrapping.
    let (region, r, zeroed) = mgr
        .alloc_large(
            5 * PAGE + 7,
            PAGE,
            topo_core::Hints::default(),
            &NoRegionCache,
        )
        .expect("large");
    assert_eq!(region.len, 6 * PAGE, "rounded up to whole pages");
    assert!(r.is_some());
    // Carved fresh from the unbacked region over a POSIX provider (mmap zero pages,
    // §26.2), so it is reported OS-zeroed — letting calloc skip the memset (W15-5).
    assert!(zeroed, "a fresh POSIX-backed extent is reported zeroed");
    // A size that would overflow the page rounding fails safely (null, not a wrap).
    assert_eq!(
        mgr.alloc_large(
            usize::MAX,
            PAGE,
            topo_core::Hints::default(),
            &NoRegionCache
        )
        .err(),
        Some(topo_core::ExtentError::Overflow)
    );
    assert!(mgr.check_invariants());
}

#[test]
fn whole_region_alloc_free_cycle_reuses_and_stays_writable() {
    let mgr = manager(16, 4096);
    for _ in 0..8 {
        let r = mgr.alloc(16 * PAGE, PAGE, Fit::Best).expect("alloc");
        let region = mgr.region_of(r).unwrap();
        // SAFETY: the region is committed for its whole length.
        unsafe {
            region.base.write(0xa5);
            region.base.add(region.len - 1).write(0x5a);
            assert_eq!(region.base.read(), 0xa5);
        }
        mgr.free(r).expect("free");
        assert!(mgr.check_invariants());
    }
}

// ===========================================================================
// Property tests (DD-1 "Verify").
// ===========================================================================

/// A pure-`ExtentMap` op for the random split/merge property.
#[derive(Clone, Copy, Debug)]
enum MapOp {
    Carve(u8),         // carve N pages
    FreeReleased(u16), // free the live extent at index % len, as Released
}

proptest! {
    /// DD-1: random carve/free sequences over the pure bookkeeping keep the region
    /// tiled, the indices consistent, and live (carved) extents pairwise disjoint —
    /// the `check_invariants` predicate (the §18 tiling well-formedness) holds after
    /// every step, and no two live extents' ranges overlap.
    #[test]
    fn extent_map_random_carve_free_keeps_tiling_and_disjoint(
        ops in prop::collection::vec(
            prop_oneof![
                (1u8..=6).prop_map(MapOp::Carve),
                any::<u16>().prop_map(MapOp::FreeReleased),
            ],
            0..160,
        )
    ) {
        const REGION_PAGES: usize = 96;
        let mut m = ExtentMap::new(meta(1 << 18), 0x1_0000_0000, REGION_PAGES * PAGE, 1024)
            .expect("map");
        let mut live: Vec<u32> = Vec::new();

        for op in ops {
            match op {
                MapOp::Carve(pages) => {
                    if let Some(id) = m.carve(pages as usize * PAGE, PAGE, Fit::Best) {
                        live.push(id.0);
                    }
                }
                MapOp::FreeReleased(idx) => {
                    if !live.is_empty() {
                        let k = (idx as usize) % live.len();
                        let id = topo_core::ExtentId(live.swap_remove(k));
                        // Unbacked carves free to the unbacked free state.
                        m.free(id, ExtentState::Released);
                    }
                }
            }
            prop_assert!(m.check_invariants(), "tiling/index invariant must hold");

            // Live (Active) extents are pairwise disjoint (the DD-1 disjointness
            // property). Collect their ranges and check no overlap.
            let mut ranges: Vec<(usize, usize)> = live
                .iter()
                .filter_map(|&i| m.view(topo_core::ExtentId(i)).map(|e| (e.base, e.end())))
                .collect();
            ranges.sort();
            for w in ranges.windows(2) {
                prop_assert!(w[0].1 <= w[1].0, "live extents overlap: {:?} {:?}", w[0], w[1]);
            }
        }
    }
}

/// A manager op for the integration property.
#[derive(Clone, Copy, Debug)]
enum MgrOp {
    Alloc { pages: u8, align_exp: u8 },
    Free(u16),
}

proptest! {
    /// The extent manager over the real POSIX provider: random alloc/free keeps the
    /// back-end well-formed and live allocations disjoint and writable (the bytes a
    /// live allocation owns are exclusively its own).
    #[test]
    fn extent_manager_random_alloc_free_is_disjoint_and_writable(
        ops in prop::collection::vec(
            prop_oneof![
                (1u8..=5, 0u8..=2).prop_map(|(pages, align_exp)| MgrOp::Alloc { pages, align_exp }),
                any::<u16>().prop_map(MgrOp::Free),
            ],
            0..120,
        )
    ) {
        let mgr = manager(128, 4096);
        // Track (ref, base, len, tag) for each live allocation; the tag is written
        // into the first byte so we can detect any aliasing write.
        let mut live: Vec<(ExtentRef, usize, usize, u8)> = Vec::new();
        let mut tag: u8 = 1;

        for op in ops {
            match op {
                MgrOp::Alloc { pages, align_exp } => {
                    let align = PAGE << (align_exp as usize); // page-multiple alignment
                    if let Ok(r) = mgr.alloc(pages as usize * PAGE, align, Fit::Best) {
                        let region = mgr.region_of(r).expect("region");
                        prop_assert_eq!(region.base as usize % align, 0, "alignment honored");
                        // SAFETY: committed for its whole length; stamp every page.
                        unsafe {
                            for p in (0..region.len).step_by(PAGE) {
                                region.base.add(p).write(tag);
                            }
                        }
                        live.push((r, region.base as usize, region.len, tag));
                        tag = tag.wrapping_add(1).max(1);
                    }
                }
                MgrOp::Free(idx) => {
                    if !live.is_empty() {
                        let k = (idx as usize) % live.len();
                        let (r, _, _, _) = live.swap_remove(k);
                        mgr.free(r).expect("free a live extent");
                    }
                }
            }
            prop_assert!(mgr.check_invariants(), "back-end must stay well-formed");

            // No live allocation overlaps another, and each still carries its own
            // tag (nothing aliased its first page).
            let mut ranges: Vec<(usize, usize)> =
                live.iter().map(|&(_, b, l, _)| (b, b + l)).collect();
            ranges.sort();
            for w in ranges.windows(2) {
                prop_assert!(w[0].1 <= w[1].0, "live allocations overlap");
            }
            for &(_, base, _, t) in &live {
                // SAFETY: `base` is a committed, still-live allocation.
                let got = unsafe { (base as *const u8).read() };
                prop_assert_eq!(got, t, "a live allocation's byte was clobbered (aliasing)");
            }
        }
    }
}

proptest! {
    /// W4-5: under random provider commit/decommit failures, every back-end
    /// operation either succeeds or fails cleanly, and the back-end stays
    /// well-formed after each step — a failed op never half-mutates the index.
    #[test]
    fn failure_injection_keeps_invariants_green(
        period in 1u64..=4,
        ops in prop::collection::vec(
            prop_oneof![(1u8..=4).prop_map(Some), Just(None)], // Some(pages)=alloc, None=free
            0..120,
        )
    ) {
        let mgr = ExtentManager::new(
            FaultyProvider::new(period),
            meta(1 << 20),
            ArenaId::DEFAULT,
            128 * PAGE,
            PAGE,
            4096,
        )
        .expect("manager (reserve never injected)");
        let mut live: Vec<ExtentRef> = Vec::new();

        for op in ops {
            match op {
                Some(pages) => {
                    // Alloc may fail (injected commit failure) — that must roll back.
                    if let Ok(r) = mgr.alloc(pages as usize * PAGE, PAGE, Fit::Best) {
                        live.push(r);
                    }
                }
                None => {
                    if !live.is_empty() {
                        let r = live.swap_remove(live.len() - 1);
                        // Free may decommit (if the unmap policy is active); a failed
                        // decommit falls back to retain — still a clean free.
                        let _ = mgr.free(r);
                    }
                }
            }
            prop_assert!(
                mgr.check_invariants(),
                "an injected failure must leave the back-end well-formed (W4-5)"
            );
        }
    }
}

// ===========================================================================
// Dual-backend equivalence (D2 / G-sim) — only with the GPL seLe4n simulator.
// ===========================================================================

#[cfg(feature = "sele4n-sim")]
mod dual {
    use super::*;
    use topo_backend_sele4n::Sele4nSim;

    /// Run a fixed alloc/free op stream over an extent manager and record the
    /// abstract outcome (success + region length) of each alloc, independent of the
    /// concrete address.
    fn run<P: TopoBackingProvider>(
        mgr: &ExtentManager<P>,
        ops: &[(bool, u8)],
    ) -> Vec<(bool, usize)> {
        let mut out = Vec::new();
        let mut live: Vec<ExtentRef> = Vec::new();
        for &(is_alloc, pages) in ops {
            if is_alloc {
                match mgr.alloc(pages.max(1) as usize * PAGE, PAGE, Fit::Best) {
                    Ok(r) => {
                        let len = mgr.region_of(r).unwrap().len;
                        live.push(r);
                        out.push((true, len));
                    }
                    Err(_) => out.push((false, 0)),
                }
            } else if let Some(r) = live.pop() {
                mgr.free(r).expect("free");
                out.push((true, 0));
            } else {
                out.push((true, 0));
            }
        }
        out
    }

    #[test]
    fn extent_backend_is_coequal_over_posix_and_sim() {
        // The same op stream produces the same abstract outcomes on the POSIX
        // back-end and the seLe4n-simulator back-end (G-sim for §18), proving the
        // back-end is co-equal behind the seam (D2).
        const PAGES: usize = 256;
        let ops: Vec<(bool, u8)> = (0..400usize)
            .map(|i| (!i.is_multiple_of(3), ((i * 7) % 5 + 1) as u8))
            .collect();

        let posix = manager(PAGES, 8192);
        let sim = ExtentManager::new(
            Sele4nSim::new(PAGES * PAGE),
            meta(1 << 20),
            ArenaId::DEFAULT,
            PAGES * PAGE,
            PAGE,
            8192,
        )
        .expect("sim manager");

        assert_eq!(posix.backend_name(), "posix");
        assert_eq!(sim.backend_name(), "sele4n-sim");

        let posix_out = run(&posix, &ops);
        let sim_out = run(&sim, &ops);
        assert_eq!(
            posix_out, sim_out,
            "the extent back-end diverged across backends"
        );
        assert!(posix.check_invariants() && sim.check_invariants());
    }
}

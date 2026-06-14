// SPDX-License-Identifier: MIT
//! Fuzz target for the hugepage filler (W11-2/W11-4, §19, §34.8). The property:
//! under *any* sequence of place/free/subrelease/unsubrelease/reserve-run/free-run/
//! mark-cold operations, the [`HugePageFiller`] stays well-formed — every live page
//! is committed and not released (H-001), each hugepage's filed bin matches its
//! occupancy/state (H-003), no live page is ever subreleased (H-005), and the bin
//! lists and accounting balance ([`HugePageFiller::check_invariants`], the §19.8 /
//! B.4 oracle). A single counterexample (a misfiled bin, a live page released, an
//! accounting drift) trips `check_invariants` and the fuzzer reports it.
//!
//! The op stream exercises **both** in-hugepage carving paths — sub-hugepage
//! [`place`](HugePageFiller::place) at **varied alignments** and the whole-hugepage
//! [`reserve_hugepages`](HugePageFiller::reserve_hugepages) run path — interleaved,
//! plus the [`subrelease`](HugePageFiller::subrelease) /
//! [`unsubrelease`](HugePageFiller::unsubrelease) rollback round-trip, so the §19.6
//! release path and the §19.2 large/awkward path are fuzzed alongside the common
//! pack/unpack churn.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{Hotness, HugePageFiller, Lifetime, PlaceHints, HUGEPAGE_SIZE};

const PAGE: usize = PAGE_SIZE;
const CAPACITY: usize = 8;
const REGION_BASE: usize = HUGEPAGE_SIZE * 8; // hugepage-aligned synthetic base

fuzz_target!(|data: &[u8]| {
    // A fresh metadata arena per input (heap, freed when this input returns — so a
    // long fuzz campaign does not leak). `buf` must outlive `filler`, which holds raw
    // pointers into it; both drop at the end of this closure (filler first).
    let mut buf = vec![0u8; 1 << 18];
    // SAFETY: `buf` lives for this closure (longer than `arena`/`filler`), is not
    // moved or reallocated below, and is not aliased through any other path.
    let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
    let mut f = match HugePageFiller::new(&arena, REGION_BASE, CAPACITY) {
        Some(f) => f,
        None => return,
    };
    assert!(f.check_invariants());

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

    // A power-of-two byte alignment in `[PAGE, 8*PAGE]`, so the §19.3 aligned-fit
    // search (`find_run_aligned` / `align_stride`) is fuzzed, not just page alignment.
    let align = |b: u8| PAGE << ((b >> 5) & 0b11);

    // Currently-live sub-hugepage runs (freed via `free`) and whole-hugepage runs
    // (freed via `free_hugepages`), tracked separately so frees target real,
    // still-live allocations of the right kind (no double free in the harness).
    let mut live: Vec<(usize, usize)> = Vec::new();
    let mut runs: Vec<(usize, usize)> = Vec::new();

    for &b in data {
        match b % 6 {
            // place a 1..=16 page run at a varied alignment (then confirm its commit,
            // as the backend does)
            0 | 1 => {
                let pages = (b / 4) as usize % 16 + 1;
                if let Some(p) = f.place(pages, align(b), hints(b)) {
                    f.mark_committed(&p);
                    live.push((p.base, p.pages));
                }
            }
            // free a live run, then sometimes subrelease the now free-backed run — and
            // sometimes immediately roll the subrelease back (unsubrelease)
            2 => {
                if !live.is_empty() {
                    let i = (b as usize) % live.len();
                    let (base, pages) = live.swap_remove(i);
                    assert!(f.free(base, pages).valid, "freeing a live run must succeed");
                    if b & 0x40 != 0 {
                        // The freed run is not in `live`, so a subrelease (which may be
                        // refused by the §19.6 guards) cannot strand a live object.
                        if let Some(sr) = f.subrelease(base, pages, b & 0x80 != 0) {
                            // Roll the subrelease back in place (the decommit-failure
                            // path): recommitting must keep the filler well-formed.
                            if b & 0x20 != 0 {
                                f.unsubrelease(&sr);
                            }
                        }
                    }
                }
            }
            // reserve a 1..=3 hugepage whole-run (the §19.2 large/awkward path), confirm
            // its commit, and track it for a later whole-run free
            3 => {
                let n = (b / 4) as usize % 3 + 1;
                if let Some(run) = f.reserve_hugepages(n) {
                    f.mark_run_committed(&run);
                    runs.push((run.base, run.hugepages));
                }
            }
            // free a whole-hugepage run (the inverse of reserve_hugepages)
            4 => {
                if !runs.is_empty() {
                    let i = (b as usize) % runs.len();
                    let (base, hugepages) = runs.swap_remove(i);
                    assert!(
                        f.free_hugepages(base, hugepages).valid,
                        "freeing a reserved run must succeed"
                    );
                }
            }
            // mark a hugepage cold (the W12 idle-decay hook)
            _ => {
                let hp = (b as usize) % CAPACITY;
                let _ = f.mark_cold(REGION_BASE + hp * HUGEPAGE_SIZE);
            }
        }
        // The §19.8 / B.4 invariant must hold after *every* operation.
        assert!(f.check_invariants(), "hugepage filler invariant violated");
    }

    // Draining every remaining live allocation (both kinds) must also keep it
    // well-formed and leak nothing.
    for (base, pages) in live {
        assert!(f.free(base, pages).valid);
        assert!(f.check_invariants());
    }
    for (base, hugepages) in runs {
        assert!(f.free_hugepages(base, hugepages).valid);
        assert!(f.check_invariants());
    }
});

// SPDX-License-Identifier: MIT
//! Fuzz target for the hugepage filler (W11-2/W11-4, §19, §34.8). The property:
//! under *any* sequence of place/free/subrelease/mark-cold operations, the
//! [`HugePageFiller`] stays well-formed — every live page is committed and not
//! released (H-001), each hugepage's filed bin matches its occupancy/state (H-003),
//! no live page is ever subreleased (H-005), and the bin lists and accounting
//! balance ([`HugePageFiller::check_invariants`], the §19.8 / B.4 oracle). A single
//! counterexample (a misfiled bin, a live page released, an accounting drift) trips
//! `check_invariants` and the fuzzer reports it.
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
    };

    // Currently-live (allocated) runs, so frees target real allocations.
    let mut live: Vec<(usize, usize)> = Vec::new();

    for &b in data {
        match b % 4 {
            // place a 1..=16 page run (then confirm its commit, as the backend does)
            0 | 1 => {
                let pages = (b / 4) as usize % 16 + 1;
                if let Some(p) = f.place(pages, PAGE, hints(b)) {
                    f.mark_committed(&p);
                    live.push((p.base, p.pages));
                }
            }
            // free a live run, then sometimes subrelease the now free-backed run
            2 => {
                if !live.is_empty() {
                    let i = (b as usize) % live.len();
                    let (base, pages) = live.swap_remove(i);
                    assert!(f.free(base, pages).valid, "freeing a live run must succeed");
                    if b & 0x40 != 0 {
                        // The freed run is not in `live`, so a subrelease (which may be
                        // refused by the §19.6 guards) cannot strand a live object.
                        let _ = f.subrelease(base, pages, b & 0x80 != 0);
                    }
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

    // Draining the remaining live runs must also keep it well-formed.
    for (base, pages) in live {
        assert!(f.free(base, pages).valid);
        assert!(f.check_invariants());
    }
});

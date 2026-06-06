// SPDX-License-Identifier: MIT
//! Fuzz target for the back-end extent manager (W4-2/W4-5, §18, §34.8). The
//! property: under *any* sequence of carve/split/merge/free operations, the
//! [`ExtentMap`] stays well-formed — the address list tiles the managed region
//! exactly, every free extent is in its correct size bin, no `Active` extent is
//! binned, and the byte accounting balances ([`ExtentMap::check_invariants`], the
//! W4-5 invariant oracle). A single counterexample (a torn split, a bad coalesce,
//! an accounting drift) trips `check_invariants` and the fuzzer reports it.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{ExtentId, ExtentMap, ExtentState, Fit};

const PAGE: usize = PAGE_SIZE;
const REGION_PAGES: usize = 256;
const SLOTS: usize = 512;

fuzz_target!(|data: &[u8]| {
    // A fresh metadata arena per input (heap, freed when this input returns — so a
    // long fuzz campaign does not leak). `buf` must outlive `map`, which holds raw
    // pointers into it; both are dropped at the end of this closure (map first).
    let mut buf = vec![0u8; 1 << 18]; // 256 KiB of metadata for the slot pool
    // SAFETY: `buf` lives for this closure (longer than `arena`/`map`), is not moved
    // or reallocated below, and is not aliased through any other path.
    let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
    let mut map = match ExtentMap::new(&arena, 0x1_0000_0000, REGION_PAGES * PAGE, SLOTS) {
        Some(m) => m,
        None => return,
    };
    assert!(map.check_invariants());

    let mut live: Vec<ExtentId> = Vec::new();
    for &b in data {
        match b % 3 {
            // carve a 1..=8 page extent (alternating fit policy)
            0 | 1 => {
                let pages = (b / 3) as usize % 8 + 1;
                let fit = if b & 1 == 0 { Fit::First } else { Fit::Best };
                if let Some(id) = map.carve(pages * PAGE, PAGE, fit) {
                    live.push(id);
                }
            }
            // free a live extent (coalescing with free neighbours)
            _ => {
                if !live.is_empty() {
                    let i = (b as usize) % live.len();
                    let id = live.swap_remove(i);
                    // Unbacked carves free to the unbacked free state, `Released`.
                    map.free(id, ExtentState::Released);
                }
            }
        }
        // The W4-5 invariant must hold after *every* operation.
        assert!(map.check_invariants(), "extent invariant violated");
    }

    // Draining the rest must also keep it well-formed and reclaim all bytes.
    while let Some(id) = live.pop() {
        map.free(id, ExtentState::Released);
        assert!(map.check_invariants());
    }
    assert_eq!(map.free_bytes(), REGION_PAGES * PAGE);
});

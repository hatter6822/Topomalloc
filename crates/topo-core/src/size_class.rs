// SPDX-License-Identifier: MIT
//! Size classes and the size→class lookup (§9, plan 03 W2-2).
//!
//! The table data is *generated* (`crate::generated::tables`, the single source
//! of truth); this module only defines the row type and the lookup logic. The
//! invariants the table satisfies are validated by `size-class-gen` and proved
//! in Lean (plan 02 W1-4); here we re-assert the runtime-relevant ones in tests.

use crate::generated::tables::{
    MAX_ALIGN, PAGE_SIZE, QUANTUM, SIZE_CLASSES, SIZE_TO_CLASS, SMALL_MAX,
};
use crate::ids::SizeClassId;

/// One size class. Field semantics follow §9.3. The `generated::tables` module
/// supplies the constant array of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeClassRow {
    /// Object size in bytes (`>= request`, an integer multiple of `align`).
    pub size: u32,
    /// Natural alignment of every object in the class.
    pub align: u32,
    /// Pages backing one slab of this class.
    pub slab_pages: u32,
    /// Objects carved from one slab (`floor(slab_pages * page / size)`).
    pub objects_per_slab: u32,
    /// Objects moved per transfer-cache batch (`<= max_local_capacity`).
    pub batch: u32,
    /// Maximum objects a front-end cache may hold for this class.
    pub max_local_capacity: u32,
}

/// Map a request `(size, align)` to the smallest size class that satisfies both
/// the size and the alignment, or `None` if it exceeds `SMALL_MAX` (the
/// medium/large path) or cannot be served by a small class at this alignment.
///
/// The size→class step is a single direct-mapped lookup (`SIZE_TO_CLASS`,
/// branch-light, W2-2a) keyed by the granule `(size - 1) / QUANTUM`; the chosen
/// class is the smallest whose size covers the request (proved exhaustively by
/// the generator and in Lean, plan 02 W1-4e).
///
/// **Over-alignment escape (§9.3 / §25.5, W2-3b).** If the size-mapped class is
/// not *naturally* aligned enough, the lookup advances to the smallest larger
/// class whose natural alignment covers the request — it never offset-adjusts an
/// object inside a shared slab. A request needing more alignment than any class
/// provides (`align > MAX_ALIGN`) is rejected in O(1) and routed to medium/large
/// by the caller. Because the chosen class's size is an integer multiple of its
/// alignment (the §9.3 invariant), every object in its slab is aligned, so an
/// over-aligned request only ever lands in a class at least as aligned as it
/// requires — never sharing a *less*-aligned class's slab.
#[inline]
pub fn size_class(size: usize, align: usize) -> Option<SizeClassId> {
    // `align > MAX_ALIGN` short-circuits the common over-aligned reject (no class
    // can ever satisfy it); the walk below still returns `None` correctly for an
    // `align <= MAX_ALIGN` that no *size-sufficient* class happens to provide.
    if size == 0 || size > SMALL_MAX || align > MAX_ALIGN {
        return None;
    }
    let granule = (size - 1) / QUANTUM;
    // Indexing soundness: `1 <= size <= SMALL_MAX` and `SIZE_TO_CLASS` has
    // `SMALL_MAX / QUANTUM` entries, so `granule` is in bounds (the generator
    // proves this). We still use a checked `get` to keep the runtime total.
    let mut idx = *SIZE_TO_CLASS.get(granule)? as usize;
    while idx < SIZE_CLASSES.len() && (SIZE_CLASSES[idx].align as usize) < align {
        idx += 1;
    }
    if idx >= SIZE_CLASSES.len() {
        return None;
    }
    Some(SizeClassId::new(idx))
}

/// The usable size (allocated bytes) for a size class.
#[inline]
pub fn usable_size(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].size as usize
}

/// The full row for a size class.
#[inline]
pub fn row(sc: SizeClassId) -> SizeClassRow {
    SIZE_CLASSES[sc.index()]
}

/// Number of size classes in the table.
#[inline]
pub fn count() -> usize {
    SIZE_CLASSES.len()
}

// Reverse accessors (W2-2b): `sc -> field`. Each reads a single field from the
// generated row so callers need not destructure `row(sc)`; they agree with the
// table by construction (asserted in `accessors_agree_with_table`). All are
// `#[inline]` for the hot paths in plans 05/06.

/// The natural alignment (bytes) of every object in `sc` — always `>= the
/// requested alignment` for any request `size_class` maps here (§9.3).
#[inline]
pub fn align(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].align as usize
}

/// Pages backing one slab of `sc`.
#[inline]
pub fn slab_pages(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].slab_pages as usize
}

/// Objects carved from one slab of `sc` (`floor(slab_bytes / size)`, §16.3).
#[inline]
pub fn objects_per_slab(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].objects_per_slab as usize
}

/// Objects moved per transfer-cache batch for `sc` (`<= max_local_capacity`).
#[inline]
pub fn batch(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].batch as usize
}

/// Maximum objects a front-end cache may hold for `sc` (§11.5).
#[inline]
pub fn max_local_capacity(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].max_local_capacity as usize
}

/// Bytes in one slab of `sc` (`slab_pages * PAGE_SIZE`). Never overflows: both
/// factors are validated `u32` table values whose product fits `usize` on every
/// supported target (the generator checks `objects_per_slab * size <= slab_bytes`).
#[inline]
pub fn slab_bytes(sc: SizeClassId) -> usize {
    SIZE_CLASSES[sc.index()].slab_pages as usize * PAGE_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tables;

    #[test]
    fn covers_every_small_request_minimally() {
        // The runtime counterpart of `size_class_table_covers_all_small_requests`
        // (§33.4): every request size up to SMALL_MAX maps to a class whose size
        // is at least the request and is the smallest such class.
        for s in 1..=SMALL_MAX {
            let sc = size_class(s, 1).expect("small request must map to a class");
            let r = row(sc);
            assert!(r.size as usize >= s, "class {} too small for {}", r.size, s);
            if sc.index() > 0 {
                let prev = SIZE_CLASSES[sc.index() - 1];
                assert!((prev.size as usize) < s, "non-minimal mapping for {s}");
            }
            assert!(usable_size(sc) >= s);
        }
    }

    #[test]
    fn alignment_is_always_sufficient() {
        // §9.3: c.size is an integer multiple of c.align for every class, so a
        // slab places every object at a c.align-aligned address.
        for r in SIZE_CLASSES {
            assert_eq!(
                r.size % r.align,
                0,
                "size {} not multiple of align {}",
                r.size,
                r.align
            );
            assert!(r.align.is_power_of_two());
        }
    }

    #[test]
    fn oversize_and_overalign_route_out() {
        assert_eq!(size_class(SMALL_MAX + 1, 1), None);
        assert_eq!(size_class(0, 1), None);
        // Every class is 16-aligned in the shipped table, so a >16-byte alignment
        // request cannot be served by a small class and routes to medium/large.
        assert_eq!(size_class(16, 32), None);
        assert_eq!(size_class(1024, 64), None);
    }

    #[test]
    fn lookup_matches_linear_scan() {
        // Differential check of the direct-mapped lookup against a linear scan.
        for s in 1..=SMALL_MAX {
            let want = SIZE_CLASSES
                .iter()
                .position(|c| c.size as usize >= s)
                .unwrap();
            let got = size_class(s, 1).unwrap().index();
            assert_eq!(got, want, "lookup mismatch at size {s}");
        }
        assert_eq!(tables::SIZE_TO_CLASS.len(), SMALL_MAX / QUANTUM);
    }

    #[test]
    fn lookup_matches_generated_granule_map() {
        // The natural-alignment lookup is *exactly* the generated granule map —
        // the same array the Lean `size_class_table_covers_all_small_requests`
        // proof consumes (plan 02 W1-4e). This pins the Rust runtime to the
        // Lean-verified table, not merely to a local linear scan.
        for s in 1..=SMALL_MAX {
            let granule = (s - 1) / QUANTUM;
            let want = tables::SIZE_TO_CLASS[granule] as usize;
            assert_eq!(size_class(s, 1).unwrap().index(), want, "size {s}");
        }
    }

    #[test]
    fn max_align_equals_widest_class() {
        let widest = SIZE_CLASSES.iter().map(|c| c.align as usize).max().unwrap();
        assert_eq!(tables::MAX_ALIGN, widest);
    }

    #[test]
    fn accessors_agree_with_table() {
        // W2-2b: every reverse accessor returns the generated row's field.
        assert_eq!(count(), SIZE_CLASSES.len());
        for (i, &r) in SIZE_CLASSES.iter().enumerate() {
            let sc = SizeClassId::new(i);
            assert_eq!(usable_size(sc), r.size as usize);
            assert_eq!(align(sc), r.align as usize);
            assert_eq!(slab_pages(sc), r.slab_pages as usize);
            assert_eq!(objects_per_slab(sc), r.objects_per_slab as usize);
            assert_eq!(batch(sc), r.batch as usize);
            assert_eq!(max_local_capacity(sc), r.max_local_capacity as usize);
            assert_eq!(slab_bytes(sc), r.slab_pages as usize * tables::PAGE_SIZE);
            assert_eq!(row(sc), r);
        }
    }

    #[test]
    fn over_alignment_never_widens_a_shared_slab() {
        // W2-3b, the load-bearing correctness rule, stated table-agnostically:
        // whenever a request maps to a small class, that class's *natural*
        // alignment already covers the request and its size is an integer
        // multiple of that alignment (§9.3) — so every object in the slab is
        // aligned and no per-object offset adjustment is ever needed. A request
        // needing more alignment than any class provides is rejected outright.
        let aligns = [1usize, 2, 4, 8, 16, 32, 64, 256, 4096];
        for s in (1..=SMALL_MAX).step_by(7) {
            for &a in &aligns {
                match size_class(s, a) {
                    Some(sc) => {
                        let r = row(sc);
                        assert!(r.align as usize >= a, "class under-aligned for {s}/{a}");
                        assert_eq!(r.size % r.align, 0, "size not a multiple of align");
                        assert!(r.size as usize >= s, "class too small for {s}");
                    }
                    None => {
                        // Only legitimate when the request exceeds the small path
                        // or no class is aligned enough.
                        assert!(s > SMALL_MAX || a > tables::MAX_ALIGN);
                    }
                }
            }
        }
        // Anything beyond MAX_ALIGN is rejected in O(1) regardless of size.
        for s in [1usize, 16, 1024, SMALL_MAX] {
            assert_eq!(size_class(s, tables::MAX_ALIGN * 2), None);
        }
    }
}

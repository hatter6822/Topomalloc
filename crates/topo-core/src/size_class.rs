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
    size_class_in(
        size,
        align,
        QUANTUM,
        SMALL_MAX,
        MAX_ALIGN,
        SIZE_TO_CLASS,
        SIZE_CLASSES,
    )
    .map(SizeClassId::new)
}

/// Table-parametric core of [`size_class`] (W2-2a/W2-3b). Extracting it lets the
/// full granule-lookup → over-alignment-walk composition be unit-tested against
/// synthetic tables that actually contain over-aligned classes — the shipped
/// table is uniformly 16-aligned, so the public `size_class` never advances the
/// walk at runtime. The parameters mirror the generated constants exactly.
///
/// `align > max_align` short-circuits the common over-aligned reject in O(1) (no
/// class can satisfy it); the walk still returns `None` correctly for an
/// `align <= max_align` that no *size-sufficient* class happens to provide.
#[inline]
fn size_class_in(
    size: usize,
    align: usize,
    quantum: usize,
    small_max: usize,
    max_align: usize,
    size_to_class: &[u8],
    classes: &[SizeClassRow],
) -> Option<usize> {
    if size == 0 || size > small_max || align > max_align {
        return None;
    }
    // Indexing soundness: `1 <= size <= small_max` and `size_to_class` has
    // `small_max / quantum` entries, so `granule` is in bounds (the generator
    // proves this). We still use a checked `get` to keep the runtime total.
    let granule = (size - 1) / quantum;
    let start = *size_to_class.get(granule)? as usize;
    align_walk(start, align, classes)
}

/// The over-alignment escape (§9.3 / §25.5, W2-3b), factored out so it can be
/// exercised against tables that actually contain over-aligned classes (the
/// shipped table is uniformly 16-aligned, so `size_class` short-circuits before
/// reaching the walk).
///
/// From `start` — the smallest class whose *size* already covers the request —
/// advance to the smallest class at or after it whose *natural* alignment is
/// `>= align`, returning its index, or `None` if no such class exists (the
/// caller then routes to medium/large). Advancing to a more-aligned class,
/// rather than offset-adjusting an object inside a shared slab, is what preserves
/// the `base0 + i*size` layout for every neighbour.
#[inline]
fn align_walk(start: usize, align: usize, classes: &[SizeClassRow]) -> Option<usize> {
    let mut idx = start;
    while idx < classes.len() && (classes[idx].align as usize) < align {
        idx += 1;
    }
    if idx < classes.len() {
        Some(idx)
    } else {
        None
    }
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

/// The full row for a size class, or `None` if `sc` is out of the table's range.
///
/// The bounds-checked counterpart of [`row`]: callers that may hold an *untrusted*
/// or possibly-corrupted `SizeClassId` (e.g. pointer classification reading a size
/// class out of a span descriptor that a hardened build must survive being
/// corrupted, §17.3/W3-4) use this to stay total — a bad index resolves to `None`
/// instead of an out-of-bounds panic.
#[inline]
pub fn checked_row(sc: SizeClassId) -> Option<SizeClassRow> {
    SIZE_CLASSES.get(sc.index()).copied()
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
    fn align_walk_routes_over_aligned_to_a_more_aligned_class() {
        // A synthetic table WITH an over-aligned class (the shipped table is
        // uniformly 16-aligned, so this is the only way to exercise the
        // walk-forward branch, W2-3b). Class 2 is the lone 32-aligned class; it
        // is deliberately in the *middle* so "the only aligned class is behind
        // the size-start" is reachable. Sizes ascend; each size is a multiple of
        // its align (§9.3), so the rows are well-formed.
        fn r(size: u32, align: u32) -> SizeClassRow {
            SizeClassRow {
                size,
                align,
                slab_pages: 1,
                objects_per_slab: 1,
                batch: 1,
                max_local_capacity: 1,
            }
        }
        let table = [r(16, 16), r(32, 16), r(96, 32), r(160, 16), r(256, 16)];

        // align <= the start class's alignment: no advance.
        assert_eq!(align_walk(0, 16, &table), Some(0));
        assert_eq!(align_walk(1, 1, &table), Some(1));
        // Over-aligned: skip both 16-aligned classes 0,1 to reach the 32-aligned 2.
        assert_eq!(align_walk(0, 32, &table), Some(2));
        // Already at/inside a sufficiently-aligned class: stay.
        assert_eq!(align_walk(2, 32, &table), Some(2));
        // No class is 64-aligned (table max is 32): route out (None).
        assert_eq!(align_walk(0, 64, &table), None);
        // The 32-aligned class (2) exists but is *behind* the size-start (3): the
        // request can't be served small → None, so the caller routes to med/large.
        assert_eq!(align_walk(3, 32, &table), None);
        // start past the end → None (defensive).
        assert_eq!(align_walk(table.len(), 1, &table), None);
    }

    #[test]
    fn size_class_in_routes_over_aligned_to_a_distinct_aligned_class() {
        // The integrated granule-lookup → align-walk path on a synthetic table
        // with an over-aligned class (class 2 is 32-aligned; the rest 16). This is
        // the end-to-end W2-3b routing the shipped table can't exercise.
        fn r(size: u32, align: u32) -> SizeClassRow {
            SizeClassRow {
                size,
                align,
                slab_pages: 1,
                objects_per_slab: 1,
                batch: 1,
                max_local_capacity: 1,
            }
        }
        // Sizes 16,32,96,160,256 (each a multiple of its align, §9.3).
        let classes = [r(16, 16), r(32, 16), r(96, 32), r(160, 16), r(256, 16)];
        // Granule g (request g*16+1..(g+1)*16) → smallest class whose size fits.
        let size_to_class: [u8; 16] = [0, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4];
        let q = 16usize;
        let small_max = 256usize;
        let max_align = 32usize;
        let lookup = |size, align| {
            size_class_in(
                size,
                align,
                q,
                small_max,
                max_align,
                &size_to_class,
                &classes,
            )
        };

        // The crux of W2-3b: size 20 naturally maps to the less-aligned class 1
        // (16-aligned), but the over-aligned (32) request is routed FORWARD to the
        // distinct 32-aligned class 2 — never served from class 1's shared slab.
        assert_eq!(lookup(20, 16), Some(1));
        assert_eq!(lookup(20, 32), Some(2));
        // The chosen class genuinely provides the alignment (no offset hack).
        assert!(classes[lookup(20, 32).unwrap()].align as usize >= 32);
        // A request already mapping to the aligned class stays put.
        assert_eq!(lookup(40, 32), Some(2));
        // Alignment beyond the table maximum routes out (caller → medium/large).
        assert_eq!(lookup(20, 64), None);
        // Oversize and zero still reject.
        assert_eq!(lookup(small_max + 1, 16), None);
        assert_eq!(lookup(0, 16), None);
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

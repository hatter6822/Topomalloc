// SPDX-License-Identifier: MIT
//! Size classes and the size→class lookup (§9, plan 03 W2-2).
//!
//! The table data is *generated* (`crate::generated::tables`, the single source
//! of truth); this module only defines the row type and the lookup logic. The
//! invariants the table satisfies are validated by `size-class-gen` and proved
//! in Lean (plan 02 W1-4); here we re-assert the runtime-relevant ones in tests.

use crate::generated::tables::{QUANTUM, SIZE_CLASSES, SIZE_TO_CLASS, SMALL_MAX};
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
/// Over-aligned requests that no small class can satisfy return `None` so the
/// caller routes them to the medium/large path — never widening a shared slab's
/// stride (§9.3 / §25.5).
#[inline]
pub fn size_class(size: usize, align: usize) -> Option<SizeClassId> {
    if size == 0 || size > SMALL_MAX {
        return None;
    }
    let granule = (size - 1) / QUANTUM;
    // SAFETY of indexing: `size <= SMALL_MAX` and `SIZE_TO_CLASS` has
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
        // Every class is 16-aligned in the trivial table, so a 32-byte alignment
        // request cannot be served by a small class and routes to large.
        assert_eq!(size_class(16, 32), None);
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
}

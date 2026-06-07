// SPDX-License-Identifier: MIT
//! Slab layout computation (§16.3, plan 03 W5-1).
//!
//! A slab is a contiguous page run carved into equal-size objects:
//!
//! ```text
//! object_i = align_up(base + hdr, align) + i * size
//! ```
//!
//! The header (`hdr`) is 0 when metadata is out-of-line (§17.3, the common case).
//! Because `size` is an integer multiple of `align` (the §9.3 invariant), every
//! object address is naturally aligned, and the object ranges are disjoint by
//! construction — no padding between objects, no offset adjustment needed.
//!
//! This module provides the geometry computation and the verification that the
//! layout fits within the backing span (W5-1 acceptance: "object ranges fit +
//! disjoint, mirrors Lean W1-4d").
//!
//! **Overflow safety:** every arithmetic is checked (§9.7). All functions return
//! `None` on overflow rather than wrapping.

use crate::generated::tables::{PAGE_SIZE, SIZE_CLASSES};
use crate::ids::SizeClassId;
use crate::overflow::align_up;
use crate::size_class;

/// Geometry of a single slab (§16.3, W5-1). Produced by [`SlabLayout::compute`]
/// from a size class and a base address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlabLayout {
    /// Address of the first object (`align_up(base + slab_header, align)`).
    pub object0: usize,
    /// Bytes per object (the class's usable size, an integer multiple of `align`).
    pub object_size: usize,
    /// Natural alignment of every object (a power of two).
    pub align: usize,
    /// Objects carved from this slab (from the generated table, verified to fit).
    pub object_count: usize,
    /// Total slab bytes (`slab_pages * PAGE_SIZE`).
    pub slab_bytes: usize,
}

impl SlabLayout {
    /// Compute the slab layout for size class `sc` at `base` with `slab_header`
    /// bytes reserved before object 0. Returns `None` if the size class is out
    /// of range, the geometry overflows, or no objects fit. All arithmetic is
    /// checked (§9.7).
    ///
    /// The table's `objects_per_slab` is an upper bound (computed with `hdr = 0`);
    /// when a non-zero `slab_header` is present, the usable region shrinks and
    /// the actual count may be smaller.
    pub fn compute(sc: SizeClassId, base: usize, slab_header: usize) -> Option<Self> {
        let row = size_class::checked_row(sc)?;
        let object_size = row.size as usize;
        let alignment = row.align as usize;
        let slab_bytes = (row.slab_pages as usize).checked_mul(PAGE_SIZE)?;
        let object0 = align_up(base.checked_add(slab_header)?, alignment)?;
        let slab_end = base.checked_add(slab_bytes)?;

        if object_size == 0 || object0 >= slab_end {
            return None;
        }

        // The table's count assumes hdr = 0. With a header, fewer objects fit
        // because object0 shifts forward, shrinking the usable region.
        let usable = slab_end - object0;
        let fit = usable / object_size;
        let object_count = fit.min(row.objects_per_slab as usize);

        if object_count == 0 {
            return None;
        }

        // Defence in depth: verify the last object fits (should hold by
        // construction from the floor division, but an adversarial base could
        // make the checked arithmetic fail).
        let last_obj_end = object0
            .checked_add(object_count.checked_sub(1)?.checked_mul(object_size)?)?
            .checked_add(object_size)?;
        if last_obj_end > slab_end {
            return None;
        }

        Some(SlabLayout {
            object0,
            object_size,
            align: alignment,
            object_count,
            slab_bytes,
        })
    }

    /// Address of object `i` (§16.3): `object0 + i * object_size`. `None` if
    /// `i >= object_count` or the addition would overflow.
    #[inline]
    pub fn object_addr(&self, i: usize) -> Option<usize> {
        if i >= self.object_count {
            return None;
        }
        self.object0.checked_add(i.checked_mul(self.object_size)?)
    }

    /// Exclusive end of the last object (`object_addr(count-1) + object_size`).
    #[inline]
    pub fn objects_end(&self) -> Option<usize> {
        if self.object_count == 0 {
            return None;
        }
        self.object_addr(self.object_count - 1)?
            .checked_add(self.object_size)
    }

    /// Whether every object fits within `[base, base + slab_bytes)` and all
    /// objects are disjoint (W5-1 acceptance). True by construction when
    /// `object_size` is a positive integer multiple of `align` (§9.3), verified
    /// here for defence in depth.
    pub fn is_valid(&self, base: usize) -> bool {
        if self.object_count == 0 {
            return true;
        }
        // §9.3: size is a positive integer multiple of alignment.
        if self.object_size == 0 || !self.object_size.is_multiple_of(self.align) {
            return false;
        }
        // object0 is aligned and within the slab.
        if !self.object0.is_multiple_of(self.align) || self.object0 < base {
            return false;
        }
        // Last object ends within the slab.
        let slab_end = match base.checked_add(self.slab_bytes) {
            Some(e) => e,
            None => return false,
        };
        match self.objects_end() {
            Some(end) if end <= slab_end => {}
            _ => return false,
        }
        // Disjointness: because object_size > 0 and objects are laid out as
        //   object0, object0 + size, object0 + 2*size, ...
        // each pair differs by exactly `size` bytes and each occupies exactly
        // `size` bytes, so they are contiguous and non-overlapping.
        true
    }

    /// Convert a pointer `addr` within this slab to its object index, or `None`
    /// if the address is not exactly at an object boundary, is outside the
    /// object region, or the slab has zero-size objects.
    #[inline]
    pub fn addr_to_index(&self, addr: usize) -> Option<usize> {
        if self.object_size == 0 || addr < self.object0 {
            return None;
        }
        let delta = addr - self.object0;
        let idx = delta / self.object_size;
        if idx >= self.object_count || !delta.is_multiple_of(self.object_size) {
            return None;
        }
        Some(idx)
    }
}

/// Verify that the generated table's `objects_per_slab` never exceeds the
/// slab's capacity. This is a compile-time check (the generator should
/// guarantee it, but pinning it here catches a table regression).
const _: () = {
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        let r = &SIZE_CLASSES[i];
        let slab_bytes = r.slab_pages as u64 * PAGE_SIZE as u64;
        let used = r.objects_per_slab as u64 * r.size as u64;
        assert!(
            used <= slab_bytes,
            "objects_per_slab * size exceeds slab_bytes for a size class"
        );
        assert!(r.size > 0, "a size class has object size 0");
        assert!(
            r.objects_per_slab > 0,
            "a size class has 0 objects per slab"
        );
        assert!(
            r.align > 0 && (r.align & (r.align - 1)) == 0,
            "a size class has non-power-of-two alignment"
        );
        assert!(
            r.size.is_multiple_of(r.align),
            "a size class has size not a multiple of align (section 9.3)"
        );
        assert!(r.slab_pages > 0, "a size class has 0 slab pages");
        i += 1;
    }
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tables::SIZE_CLASSES;

    #[test]
    fn layout_for_every_size_class_at_page_aligned_base() {
        for (i, _) in SIZE_CLASSES.iter().enumerate() {
            let sc = SizeClassId::new(i);
            let base = 0x4000_0000usize;
            let layout = SlabLayout::compute(sc, base, 0)
                .unwrap_or_else(|| panic!("layout failed for sc {i}"));
            assert!(
                layout.is_valid(base),
                "layout invalid for sc {i}: {layout:?}"
            );
            assert!(layout.object_count > 0);
            assert!(layout.object_size > 0);
        }
    }

    #[test]
    fn object_addresses_are_aligned_and_disjoint() {
        let base = 0x4000_0000usize;
        for (i, _) in SIZE_CLASSES.iter().enumerate() {
            let sc = SizeClassId::new(i);
            let layout = SlabLayout::compute(sc, base, 0).unwrap();
            let mut prev_end = 0usize;
            for j in 0..layout.object_count {
                let addr = layout
                    .object_addr(j)
                    .unwrap_or_else(|| panic!("addr failed for sc {i} obj {j}"));
                assert_eq!(addr % layout.align, 0, "object {j} of sc {i} not aligned");
                // Objects are contiguous: addr(j) == addr(j-1) + size.
                if j > 0 {
                    assert_eq!(addr, prev_end, "gap/overlap at sc {i} obj {j}");
                }
                prev_end = addr + layout.object_size;
            }
            assert!(prev_end <= base + layout.slab_bytes);
        }
    }

    #[test]
    fn objects_end_is_within_slab() {
        let base = 0x4000_0000usize;
        for (i, _) in SIZE_CLASSES.iter().enumerate() {
            let sc = SizeClassId::new(i);
            let layout = SlabLayout::compute(sc, base, 0).unwrap();
            let end = layout.objects_end().unwrap();
            assert!(
                end <= base + layout.slab_bytes,
                "sc {i}: end overflows slab"
            );
        }
    }

    #[test]
    fn addr_to_index_round_trips_every_object() {
        let base = 0x4000_0000usize;
        for (i, _) in SIZE_CLASSES.iter().enumerate() {
            let sc = SizeClassId::new(i);
            let layout = SlabLayout::compute(sc, base, 0).unwrap();
            for j in 0..layout.object_count {
                let addr = layout.object_addr(j).unwrap();
                assert_eq!(
                    layout.addr_to_index(addr),
                    Some(j),
                    "sc {i}: addr {addr:#x} did not round-trip to index {j}"
                );
            }
        }
    }

    #[test]
    fn addr_to_index_rejects_interior_and_foreign() {
        let base = 0x4000_0000usize;
        // Test across several size classes with different object sizes.
        for idx in [0, 1, 10, 30, SIZE_CLASSES.len() - 1] {
            let sc = SizeClassId::new(idx);
            let layout = SlabLayout::compute(sc, base, 0).unwrap();
            // Interior of object 0 (not at boundary).
            if layout.object_size > 1 {
                assert_eq!(
                    layout.addr_to_index(layout.object0 + 1),
                    None,
                    "sc {idx}: interior byte accepted"
                );
            }
            // Mid-object address for last object.
            if layout.object_count > 1 && layout.object_size > 1 {
                let mid = layout.object_addr(layout.object_count - 1).unwrap() + 1;
                assert_eq!(
                    layout.addr_to_index(mid),
                    None,
                    "sc {idx}: mid-last-object accepted"
                );
            }
            // Before the slab.
            assert_eq!(
                layout.addr_to_index(base - 1),
                None,
                "sc {idx}: before-slab accepted"
            );
            // Past the last object.
            let past = layout.objects_end().unwrap();
            assert_eq!(
                layout.addr_to_index(past),
                None,
                "sc {idx}: past-end accepted"
            );
        }
    }

    #[test]
    fn slab_header_shifts_object0() {
        let base = 0x4000_0000usize;
        let sc = SizeClassId::new(0); // 16-byte, 16-align
        let no_hdr = SlabLayout::compute(sc, base, 0).unwrap();
        let with_hdr = SlabLayout::compute(sc, base, 64).unwrap();
        // The header pushes object0 forward.
        assert!(with_hdr.object0 >= base + 64);
        assert!(with_hdr.object0 >= no_hdr.object0);
        // Fewer objects fit (the header consumed slab space).
        assert!(with_hdr.object_count <= no_hdr.object_count);
        assert!(with_hdr.is_valid(base));
    }

    #[test]
    fn out_of_range_size_class_returns_none() {
        let sc = SizeClassId::new(50_000);
        assert!(SlabLayout::compute(sc, 0x4000_0000, 0).is_none());
    }

    #[test]
    fn overflowing_base_returns_none() {
        let sc = SizeClassId::new(0);
        assert!(SlabLayout::compute(sc, usize::MAX - 10, 0).is_none());
    }

    #[test]
    fn object_addr_rejects_out_of_range_index() {
        let base = 0x4000_0000usize;
        let sc = SizeClassId::new(0);
        let layout = SlabLayout::compute(sc, base, 0).unwrap();
        assert!(layout.object_addr(layout.object_count).is_none());
        assert!(layout.object_addr(usize::MAX).is_none());
    }

    #[test]
    fn layout_matches_span_descriptor_geometry() {
        use crate::bootstrap::BumpArena;
        use crate::ids::{ArenaId, SpanId};
        use crate::span::SpanDescriptor;

        let buf = vec![0u8; 64 * 1024].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        let m = unsafe { BumpArena::new(ptr, len) };

        let base = 0x4000_0000usize;
        let sc = SizeClassId::new(0);
        let row = size_class::row(sc);
        let sd = SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            sc,
            base,
            row.slab_pages,
            row.objects_per_slab,
            0,
            &m,
        )
        .unwrap();

        let layout = SlabLayout::compute(sc, base, 0).unwrap();
        assert_eq!(sd.object0_base(), Some(layout.object0));
        assert_eq!(sd.object_count() as usize, layout.object_count);
    }
}

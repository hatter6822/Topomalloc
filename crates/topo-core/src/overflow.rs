// SPDX-License-Identifier: MIT
//! Overflow-safe arithmetic for size/alignment/page rounding (§9.7, W2-4).
//!
//! Every function returns `None` on overflow rather than wrapping. The SPEC is
//! explicit (§9.7): a request that would overflow a size calculation MUST fail
//! safely and MUST NOT wrap around and allocate too little memory. These helpers
//! are the single place that arithmetic is allowed to round, so the property is
//! provable by inspection and is covered by exhaustive-ish unit tests below.
//!
//! **§9.7 overflow-check map.** Every rounding the SPEC requires be checked is
//! discharged by a helper here:
//!
//! | §9.7 rounding to check | Checked helper | Used at |
//! |---|---|---|
//! | `n * size` (calloc) | [`array_bytes`] | `topomalloc_calloc` / `topomalloc_reallocarray` (topo-abi) |
//! | alignment rounding | [`align_up`] | `classify`, the guarded/aligned engine paths, `pages_for` |
//! | size-class rounding | — (table-bounded) | `size_class` rejects `> SMALL_MAX`, no arithmetic |
//! | span / page count | [`pages_for`] | `ExtentManager::alloc_large`, the hugepage filler |
//! | hugepage rounding | [`align_up`] via [`pages_for`] | `HugePageBackend` (page-granular, W11) |
//! | metadata indexing | — | pagemap (plan 03 W3) |
//!
//! The hugepage backend accounts in **pages**, not whole hugepages, so it rounds with
//! [`pages_for`]; there is no separate hugepage-rounding helper (an earlier one existed
//! for a design that never landed and was removed rather than left as an unused, and
//! therefore untested-in-context, entry point).

/// Round `value` up to the next multiple of `align`, checked.
///
/// Returns `None` if the rounded value would not fit in `usize`. `align` must be
/// a power of two and nonzero (`debug_assert!`ed).
///
/// Correctness: for a power-of-two `align`, `align - 1` is the low-bit mask. The
/// result is `(value + (align - 1)) & !(align - 1)`. The only way to exceed
/// `usize::MAX` is in the `value + (align - 1)` step, which `checked_add`
/// guards; the subsequent mask only clears bits, so it cannot overflow.
#[inline]
pub fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    let mask = align - 1;
    Some(value.checked_add(mask)? & !mask)
}

/// Compute `n * elem_size` (the byte count for `calloc`/`reallocarray`), checked.
///
/// Returns `None` on overflow (§26.1: calloc overflow MUST be checked).
#[inline]
pub fn array_bytes(n: usize, elem_size: usize) -> Option<usize> {
    n.checked_mul(elem_size)
}

/// Round `bytes` up to a whole number of `page_size` pages, checked, returning
/// the page count. `page_size` must be a power of two and nonzero.
#[inline]
pub fn pages_for(bytes: usize, page_size: usize) -> Option<usize> {
    debug_assert!(
        page_size.is_power_of_two(),
        "page size must be a power of two"
    );
    Some(align_up(bytes, page_size)? / page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_exact_multiples_are_unchanged() {
        for align in [1usize, 2, 4, 8, 16, 4096, 16384] {
            for k in 0..8 {
                let v = align * k;
                assert_eq!(align_up(v, align), Some(v));
            }
        }
    }

    #[test]
    fn align_up_rounds_up_by_one() {
        assert_eq!(align_up(1, 16), Some(16));
        assert_eq!(align_up(15, 16), Some(16));
        assert_eq!(align_up(17, 16), Some(32));
        assert_eq!(align_up(0, 16), Some(0));
    }

    #[test]
    fn align_up_overflow_returns_none_not_wrap() {
        // Near the top of the address space, rounding up must fail, never wrap
        // to a tiny value (§9.7).
        assert_eq!(align_up(usize::MAX, 16), None);
        assert_eq!(align_up(usize::MAX - 14, 16), None);
        // The largest value that still rounds within range.
        let top_aligned = usize::MAX & !15usize;
        assert_eq!(align_up(top_aligned, 16), Some(top_aligned));
        assert_eq!(align_up(top_aligned + 1, 16), None);
    }

    #[test]
    fn array_bytes_detects_overflow() {
        assert_eq!(array_bytes(4, 8), Some(32));
        assert_eq!(array_bytes(0, 9999), Some(0));
        assert_eq!(array_bytes(usize::MAX, 2), None);
        assert_eq!(array_bytes(1 << 33, 1 << 33), None);
    }

    #[test]
    fn pages_for_rounds_and_overflows() {
        assert_eq!(pages_for(1, 4096), Some(1));
        assert_eq!(pages_for(4096, 4096), Some(1));
        assert_eq!(pages_for(4097, 4096), Some(2));
        assert_eq!(pages_for(0, 4096), Some(0));
        assert_eq!(pages_for(usize::MAX, 4096), None);
    }
}

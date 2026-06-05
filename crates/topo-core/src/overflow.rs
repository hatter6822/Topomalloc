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
//! discharged by a helper here, used at exactly one site:
//!
//! | §9.7 rounding to check | Checked helper | Used at |
//! |---|---|---|
//! | `n * size` (calloc) | [`array_bytes`] | `topomalloc_calloc` (topo-abi) |
//! | alignment rounding | [`align_up`] | `classify` span, skeleton address align |
//! | size-class rounding | — (table-bounded) | `size_class` rejects `> SMALL_MAX`, no arithmetic |
//! | span / page count | [`pages_for`] | backend page accounting (plan 04) |
//! | hugepage rounding | [`hugepage_round`] | hugepage backend (plan 04) |
//! | metadata indexing | — | pagemap (plan 03 W3) |
//!
//! The hugepage backend is plan 04, so [`hugepage_round`] is not yet on a live
//! path; it is the checked primitive that backend will use, verified
//! overflow-safe here now (so the §9.7 "hugepage rounding overflow" obligation is
//! discharged at the primitive even before the backend lands).

/// Returns `true` iff `value` is a multiple of `align`.
///
/// `align` must be a power of two (checked with `debug_assert!`); callers pass
/// alignments that come from the size-class table or a validated request.
#[inline]
pub fn is_aligned(value: usize, align: usize) -> bool {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    value & (align - 1) == 0
}

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

/// Round `bytes` up to a whole number of hugepages (`hugepage_size` bytes),
/// checked (§9.7 "hugepage rounding overflow MUST be checked"). `hugepage_size`
/// must be a power of two and nonzero.
///
/// The hugepage backend lands with plan 04; this is the checked rounding
/// primitive it will use. Sharing [`align_up`] keeps every rounding kind on the
/// one overflow-safe code path (§9.7 map in the module docs), so "round up to a
/// hugepage" can never wrap to a smaller region.
#[inline]
pub fn hugepage_round(bytes: usize, hugepage_size: usize) -> Option<usize> {
    debug_assert!(
        hugepage_size.is_power_of_two(),
        "hugepage size must be a power of two"
    );
    align_up(bytes, hugepage_size)
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
                assert!(is_aligned(v, align));
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

    #[test]
    fn hugepage_round_rounds_and_overflows() {
        // §9.7: rounding up to a hugepage is checked and never wraps.
        const HUGE: usize = 2 * 1024 * 1024; // 2 MiB
        assert_eq!(hugepage_round(1, HUGE), Some(HUGE));
        assert_eq!(hugepage_round(HUGE, HUGE), Some(HUGE));
        assert_eq!(hugepage_round(HUGE + 1, HUGE), Some(2 * HUGE));
        assert_eq!(hugepage_round(0, HUGE), Some(0));
        // Near the top of the address space it must fail, not wrap to a tiny size.
        assert_eq!(hugepage_round(usize::MAX, HUGE), None);
        assert_eq!(hugepage_round(usize::MAX - HUGE + 2, HUGE), None);
    }
}

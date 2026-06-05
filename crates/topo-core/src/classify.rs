// SPDX-License-Identifier: MIT
//! Request classification (§A.1, plan 03 W2-3). Turns a raw `(size, align,
//! flags)` request into a [`Request`] describing how it will be served. This is
//! the first thing on every allocation path, so it is branch-light and total
//! (never panics, never wraps — §9.7).
//!
//! The classifier owns the *decision* (which bucket), not the backing. It splits
//! a request three ways (§9.2 / §A.1):
//!
//! * **Small** — served from a size-class slab (`size_class`, W2-2a).
//! * **Medium** — a page-rounded extent, for requests above the small path but
//!   below the hugepage threshold (`HUGE_THRESHOLD`).
//! * **Large** — `HUGE_THRESHOLD` and above, served by the hugepage/region
//!   backend (plan 04). The skeleton page-rounds it; the hugepage rounding (also
//!   overflow-checked) arrives with plan 04.
//!
//! Over-aligned requests that no small class can satisfy fall out of `size_class`
//! and are routed to the medium/large path — **never** by widening a shared
//! slab's stride (§9.3 / §25.5, W2-3b).

use crate::flags::RequestFlags;
use crate::generated::tables::{HUGE_THRESHOLD, PAGE_SIZE};
use crate::ids::{ArenaId, Label, SizeClassId};
use crate::overflow::align_up;
use crate::size_class::{size_class, usable_size};

/// How a classified request will be served.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// A small object served from a size-class slab.
    Small {
        /// The chosen size class.
        sc: SizeClassId,
        /// Bytes the allocation will actually occupy (`>= requested size`).
        usable: usize,
    },
    /// A medium object served by a page-rounded extent: above the small path but
    /// below `HUGE_THRESHOLD` (§9.2). The optional limited cache for this family
    /// arrives with the central/arena extent allocator (plans 04/05).
    Medium {
        /// Page-rounded byte size of the backing extent (`>= max(size, align)`).
        bytes: usize,
    },
    /// A large object (`>= HUGE_THRESHOLD`) served by the hugepage/region backend
    /// (§18.5, plan 04). The skeleton page-rounds it; the hugepage/region
    /// rounding arrives with plan 04.
    Large {
        /// Page-rounded byte size of the backing region (`>= max(size, align)`).
        bytes: usize,
    },
}

/// A fully classified request (§A.1). Carries the routing decision plus the
/// arena, information-flow label, validated alignment, and the validated advisory
/// flags so the front/middle/back ends can act on placement hints (§10.4)
/// without re-parsing the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// Arena the request is routed to (the default arena on POSIX, §A.1
    /// `choose_arena`).
    pub arena: ArenaId,
    /// Information-flow label (`PUBLIC` on the single-authority POSIX backend;
    /// labels partition caches and statistics on seLe4n, §36.12).
    pub label: Label,
    /// The required alignment (a validated power of two, `>= 1`).
    pub align: usize,
    /// The validated advisory flags (§10.4). Decode placement hints with
    /// [`RequestFlags::hints`]; the arena routed from them is already in
    /// [`arena`](Self::arena). Acting on the placement hints (zeroing, cache
    /// bypass, hugepage preference, hotness/lifetime) lands with plans 04/05/06.
    pub flags: RequestFlags,
    /// How the request is served.
    pub kind: RequestKind,
}

/// Select the arena for a request (§A.1 `choose_arena`): decode the explicit
/// arena from the flag word (§10.4 `TOPO_ARENA`), defaulting to the always-present
/// default arena. Arena ids beyond [`RequestFlags::MAX_ARENA_ID`] cannot be
/// flag-encoded and reach the allocator through the explicit-arena handle of the
/// public API (plan 06); multi-authority cache routing follows at M2 (D6).
#[inline]
fn choose_arena(flags: RequestFlags) -> ArenaId {
    flags.arena()
}

/// Select the information-flow label for a request (§36.12). POSIX is a single
/// authority domain, so every request is `PUBLIC`; seLe4n derives the label from
/// the arena's authority at integration (plan 09).
#[inline]
fn choose_label(_arena: ArenaId, _flags: RequestFlags) -> Label {
    Label::PUBLIC
}

/// Classify a request (§A.1). Returns `None` if the alignment is not a power of
/// two, the flag word is invalid (§10.4), or the size/alignment rounding would
/// overflow (§9.7); the caller turns `None` into a null return / `bad_alloc`.
/// Never panics, never wraps.
///
/// Zero-size requests follow the `zero_unique` policy (§9.6): they are treated
/// as a 1-byte request, so `malloc(0)` yields a unique freeable pointer from the
/// smallest class.
///
/// The medium/large boundary is decided on `span = max(effective_size, align)` —
/// the smallest region that satisfies *both* the size and the alignment. This
/// refines §A.1's `size`-only test so that an over-aligned request whose
/// alignment demands hugepage-level placement is routed to the Large path rather
/// than burdening the medium extent allocator with a hugepage-aligned hole
/// (§9.3 / §25.5 / §18.5).
pub fn classify(size: usize, align: usize, flags: u32) -> Option<Request> {
    // align_invalid (§A.1 / §25.5): reject non-power-of-two alignments. `1` is a
    // power of two, so the default (natural) alignment is always accepted.
    if !align.is_power_of_two() {
        return None;
    }
    // flags_invalid (§10.4): reject reserved bits / contradictory combinations
    // deterministically, so an invalid flag word never silently degrades.
    let flags = RequestFlags::from_raw(flags)?;
    // §9.6 zero_unique: a zero-size request behaves like a 1-byte request.
    let effective = if size == 0 { 1 } else { size };

    let arena = choose_arena(flags);
    let label = choose_label(arena, flags);

    // Small path: a size class that covers both the size and the alignment. An
    // over-aligned request that no small class can satisfy returns `None` here
    // and falls through to the medium/large path (§9.3 / §25.5, W2-3b).
    if let Some(sc) = size_class(effective, align) {
        return Some(Request {
            arena,
            label,
            align,
            flags,
            kind: RequestKind::Small {
                sc,
                usable: usable_size(sc),
            },
        });
    }

    // Medium / Large path: the backing region must cover both the size and the
    // alignment, page-rounded. `span` cannot overflow (a `max` of two `usize`s);
    // the page rounding is checked so an overflowing request fails safely rather
    // than wrapping (§9.7, W2-4).
    let span = effective.max(align);
    let bytes = align_up(span, PAGE_SIZE)?;
    let kind = if span >= HUGE_THRESHOLD {
        RequestKind::Large { bytes }
    } else {
        RequestKind::Medium { bytes }
    };
    Some(Request {
        arena,
        label,
        align,
        flags,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tables::{HUGE_THRESHOLD, MAX_ALIGN, PAGE_SIZE, SMALL_MAX};

    #[test]
    fn small_requests_classify_small() {
        let r = classify(10, 1, 0).unwrap();
        assert_eq!(r.arena, ArenaId::DEFAULT);
        assert_eq!(r.label, Label::PUBLIC);
        match r.kind {
            RequestKind::Small { usable, .. } => assert!(usable >= 10),
            other => panic!("expected Small, got {other:?}"),
        }
    }

    #[test]
    fn zero_size_is_unique_smallest_class() {
        let r = classify(0, 1, 0).unwrap();
        match r.kind {
            RequestKind::Small { usable, .. } => assert!(usable >= 1),
            other => panic!("expected Small for zero size, got {other:?}"),
        }
    }

    #[test]
    fn medium_requests_classify_medium_and_page_round() {
        // Just above the small path, well below the hugepage threshold.
        let r = classify(SMALL_MAX + 1, 1, 0).unwrap();
        match r.kind {
            RequestKind::Medium { bytes } => {
                assert_eq!(bytes % PAGE_SIZE, 0);
                assert!(bytes > SMALL_MAX);
                assert!(bytes < HUGE_THRESHOLD);
            }
            other => panic!("expected Medium, got {other:?}"),
        }
    }

    #[test]
    fn large_requests_classify_large_and_page_round() {
        // At or above the hugepage threshold (§9.2 / §18.5).
        let r = classify(HUGE_THRESHOLD, 1, 0).unwrap();
        match r.kind {
            RequestKind::Large { bytes } => {
                assert_eq!(bytes % PAGE_SIZE, 0);
                assert!(bytes >= HUGE_THRESHOLD);
            }
            other => panic!("expected Large, got {other:?}"),
        }
    }

    #[test]
    fn medium_large_boundary_is_exact() {
        // `span < HUGE_THRESHOLD` is Medium; `span >= HUGE_THRESHOLD` is Large.
        let below = classify(HUGE_THRESHOLD - 1, 1, 0).unwrap();
        assert!(matches!(below.kind, RequestKind::Medium { .. }));
        let at = classify(HUGE_THRESHOLD, 1, 0).unwrap();
        assert!(matches!(at.kind, RequestKind::Large { .. }));
    }

    #[test]
    fn over_aligned_small_request_routes_out_not_into_a_slab() {
        // A small *size* with an alignment beyond every class's natural alignment
        // must NOT be served by a small class (which would force a slab stride
        // change, §9.3 / §25.5). It routes to a page-rounded extent instead.
        let a = MAX_ALIGN * 2;
        let r = classify(64, a, 0).unwrap();
        match r.kind {
            RequestKind::Medium { bytes } | RequestKind::Large { bytes } => {
                assert_eq!(bytes % PAGE_SIZE, 0);
                assert!(bytes >= a, "region must cover the alignment");
            }
            RequestKind::Small { .. } => panic!("over-aligned request stayed in a small slab"),
        }
        assert_eq!(r.align, a);
    }

    #[test]
    fn alignment_forcing_huge_placement_routes_large() {
        // A tiny request whose alignment alone reaches the hugepage threshold is
        // a Large (hugepage-backed) request — span = max(size, align) decides.
        let r = classify(64, HUGE_THRESHOLD, 0).unwrap();
        assert!(matches!(r.kind, RequestKind::Large { .. }));
    }

    #[test]
    fn non_power_of_two_alignment_is_rejected() {
        assert_eq!(classify(16, 24, 0), None);
        assert_eq!(classify(16, 0, 0), None); // 0 is not a power of two
    }

    #[test]
    fn valid_flags_are_decoded_and_threaded() {
        use crate::flags::HugepagePolicy;
        // zero + prefer-hugepage + explicit arena 3.
        let raw = RequestFlags::NONE
            .with_zero()
            .with_hugepage(HugepagePolicy::Prefer)
            .with_arena(3)
            .unwrap()
            .raw();
        let r = classify(32, 16, raw).unwrap();
        assert_eq!(r.flags.raw(), raw);
        assert_eq!(r.arena, ArenaId(3)); // §A.1 choose_arena decoded the flag
        let h = r.flags.hints();
        assert!(h.zero);
        assert_eq!(h.hugepage, HugepagePolicy::Prefer);
    }

    #[test]
    fn invalid_flags_are_rejected_deterministically() {
        // §10.4: a reserved bit fails deterministically (not silently dropped).
        assert_eq!(classify(32, 16, 1 << 23), None);
        // Contradictory hugepage preference is rejected.
        assert_eq!(
            classify(
                32,
                16,
                RequestFlags::NO_HUGEPAGE | RequestFlags::PREFER_HUGEPAGE
            ),
            None
        );
    }

    #[test]
    fn overflowing_request_fails_safely() {
        // A size near usize::MAX must not wrap to a tiny region (§9.7).
        assert_eq!(classify(usize::MAX, 1, 0), None);
        assert_eq!(classify(usize::MAX - 5, 1, 0), None);
        // The largest power-of-two alignment cannot, on its own, overflow the
        // page rounding (`1<<63` + a page still fits `usize`), so it classifies
        // Large rather than failing — the overflow guard is about size, not align.
        let huge_align = 1usize << (usize::BITS - 1);
        assert!(matches!(
            classify(1, huge_align, 0).unwrap().kind,
            RequestKind::Large { .. }
        ));
    }
}

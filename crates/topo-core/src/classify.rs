// SPDX-License-Identifier: MIT
//! Request classification (§A.1, plan 03 W2-3). Turns a raw `(size, align,
//! flags)` request into a `Request` describing how it will be served. This is
//! the first thing on every allocation path.

use crate::generated::tables::PAGE_SIZE;
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
    /// A large object served (in the skeleton) by a page-rounded region. The
    /// medium/hugepage split (§9.2) arrives with plan 04.
    Large {
        /// Page-rounded byte size of the backing region.
        bytes: usize,
    },
}

/// A fully classified request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// Arena the request is routed to (default arena in the skeleton).
    pub arena: ArenaId,
    /// Information-flow label (PUBLIC on POSIX).
    pub label: Label,
    /// The required alignment (a power of two, `>= 1`).
    pub align: usize,
    /// How the request is served.
    pub kind: RequestKind,
}

/// Classify a request. Returns `None` if the size/alignment arithmetic would
/// overflow (§9.7) or the alignment is not a power of two; the caller turns
/// `None` into a null return / `bad_alloc`.
///
/// Zero-size requests follow the `zero_unique` policy (§9.6): they are treated
/// as a 1-byte request and served by the smallest class, yielding a unique
/// freeable pointer.
pub fn classify(size: usize, align: usize, _flags: u32) -> Option<Request> {
    if !align.is_power_of_two() {
        return None;
    }
    let effective = if size == 0 { 1 } else { size };

    if let Some(sc) = size_class(effective, align) {
        return Some(Request {
            arena: ArenaId::DEFAULT,
            label: Label::PUBLIC,
            align,
            kind: RequestKind::Small {
                sc,
                usable: usable_size(sc),
            },
        });
    }

    // Large path: the region must cover both the requested size and alignment,
    // page-rounded. All arithmetic is checked so an overflowing request fails
    // safely rather than wrapping (§9.7).
    let span = effective.max(align);
    let bytes = align_up(span, PAGE_SIZE)?;
    Some(Request {
        arena: ArenaId::DEFAULT,
        label: Label::PUBLIC,
        align,
        kind: RequestKind::Large { bytes },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tables::{PAGE_SIZE, SMALL_MAX};

    #[test]
    fn small_requests_classify_small() {
        let r = classify(10, 1, 0).unwrap();
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
    fn large_requests_classify_large_and_page_round() {
        let r = classify(SMALL_MAX + 1, 1, 0).unwrap();
        match r.kind {
            RequestKind::Large { bytes } => {
                assert_eq!(bytes % PAGE_SIZE, 0);
                assert!(bytes > SMALL_MAX);
            }
            other => panic!("expected Large, got {other:?}"),
        }
    }

    #[test]
    fn non_power_of_two_alignment_is_rejected() {
        assert_eq!(classify(16, 24, 0), None);
    }

    #[test]
    fn overflowing_request_fails_safely() {
        // A size near usize::MAX must not wrap to a tiny region (§9.7).
        assert_eq!(classify(usize::MAX, 1, 0), None);
    }
}

// SPDX-License-Identifier: MIT
//! Fuzz target for request classification (W0-7, §34.8). The property: `classify`
//! must never panic and never wrap (§9.7) on *any* `(size, align, flags)` input —
//! it returns `None` for overflowing, unserviceable, or invalidly-flagged
//! requests (§10.4). When it returns a small class, the usable size must cover the
//! request and be correctly aligned; medium/large extents are whole pages.
#![no_main]

use libfuzzer_sys::fuzz_target;
use topo_core::classify::RequestKind;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{classify, usable_size, RequestFlags};

fuzz_target!(|data: &[u8]| {
    if data.len() < 13 {
        return;
    }
    let size = usize::from_le_bytes(data[0..8].try_into().unwrap());
    // Use a small alignment exponent so we exercise valid power-of-two
    // alignments (1..=2^31) without trivially overflowing every time.
    let align = 1usize << (data[8] % 32);
    let flags = u32::from_le_bytes(data[9..13].try_into().unwrap());

    // §10.4: an invalid flag word must be rejected deterministically (`align` is
    // a valid power of two here, so flags are the only validity variable).
    if RequestFlags::from_raw(flags).is_none() {
        assert!(classify(size, align, flags).is_none());
        return;
    }

    if let Some(req) = classify(size, align, flags) {
        // The validated alignment and flags are echoed back verbatim.
        assert_eq!(req.align, align);
        assert_eq!(req.flags.raw(), flags);
        // The arena is exactly the one decoded from the flag word.
        assert_eq!(req.arena, RequestFlags::from_raw(flags).unwrap().arena());
        // Whatever it returns must satisfy the request without wrapping.
        match req.kind {
            RequestKind::Small { sc, usable } => {
                assert_eq!(usable, usable_size(sc));
                assert!(usable >= size.max(1));
                // A small class was chosen, so its natural alignment covers the
                // request (otherwise classify would have routed to medium/large).
                assert!(usable >= align);
            }
            RequestKind::Medium { bytes } | RequestKind::Large { bytes } => {
                assert!(bytes >= size.max(1));
                assert!(bytes >= align);
                // Medium/Large extents are always whole allocator pages (§9.7).
                assert_eq!(bytes % PAGE_SIZE, 0);
            }
        }
    }
});

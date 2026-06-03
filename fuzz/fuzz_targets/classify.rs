// SPDX-License-Identifier: MIT
//! Fuzz target for request classification (W0-7, §34.8). The property: `classify`
//! must never panic and never wrap (§9.7) on *any* `(size, align)` input — it
//! returns `None` for overflowing or unserviceable requests. When it returns a
//! small class, the usable size must cover the request and be correctly aligned.
#![no_main]

use libfuzzer_sys::fuzz_target;
use topo_core::classify::RequestKind;
use topo_core::{classify, usable_size};

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }
    let size = usize::from_le_bytes(data[0..8].try_into().unwrap());
    // Use a small alignment exponent so we exercise valid power-of-two
    // alignments (1..=2^31) without trivially overflowing every time.
    let align = 1usize << (data[8] % 32);

    if let Some(req) = classify(size, align, 0) {
        // Whatever it returns must satisfy the request without wrapping.
        match req.kind {
            RequestKind::Small { sc, usable } => {
                assert_eq!(usable, usable_size(sc));
                assert!(usable >= size.max(1));
                // A small class was chosen, so its natural alignment covers the
                // request (otherwise classify would have routed to Large).
                assert!(usable >= align);
            }
            RequestKind::Large { bytes } => {
                assert!(bytes >= size.max(1));
                assert!(bytes >= align);
            }
        }
    }
});

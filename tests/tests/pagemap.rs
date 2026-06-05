// SPDX-License-Identifier: MIT
//! Pagemap lookup-soundness tests (§17.2 P-Map-001..006, plan 03 W3-3d).
//!
//! These are the executable mirror of the Lean theorem `pagemap_lookup_sound`
//! (plan 02 W1-8b, `lean/TopoMalloc/Theorems/Pagemap.lean`):
//!
//! > If the pagemap maps `addr`'s page to span `sp`, then `sp` is a real span
//! > whose range contains `addr`.
//!
//! The Lean proof establishes this over the abstract model; this property test
//! discharges the *same* statement against the runtime radix pagemap over random
//! span layouts — so a divergence between the implementation and the proved
//! property fails CI (the W3-3d differential gate). It also checks the structural
//! invariants P-Map-001 (every owned page → exactly one descriptor), P-Map-002
//! (non-owned → non-owned sentinel), and P-Map-003 (small pages name arena/span/sc).

use proptest::prelude::*;

use topo_core::generated::tables::PAGE_SIZE;
use topo_core::ids::{ArenaId, SizeClassId, SpanId};
use topo_core::pagemap::{PageMap, PAGENO_BITS, PAGE_SHIFT};
use topo_core::span::SpanDescriptor;
use topo_core::BumpArena;

/// A metadata arena leaked for the test's lifetime (matching the `'static` reserve
/// real bootstrap regions have), so descriptor and node pointers stay valid.
fn leak_arena(bytes: usize) -> BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the whole process; `len` bytes valid.
    unsafe { BumpArena::new(ptr, len) }
}

/// Build descriptors at sequential, non-overlapping page ranges from `(pages, gap)`
/// specs (a `gap` of unowned pages precedes each span). Disjoint by construction,
/// so each page maps to at most one span (P-Map-001).
fn lay_out_spans(specs: &[(u32, u32)]) -> Vec<SpanDescriptor> {
    let mut base = 0x4000_0000usize;
    let mut spans = Vec::with_capacity(specs.len());
    for (i, &(pages, gap)) in specs.iter().enumerate() {
        base += gap as usize * PAGE_SIZE;
        spans.push(SpanDescriptor::new(
            SpanId(i as u32),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            base,
            pages,
            64,
            0,
        ));
        base += pages as usize * PAGE_SIZE;
    }
    spans
}

proptest! {
    /// `pagemap_lookup_sound` (the Lean mirror): over an arbitrary disjoint span
    /// layout, every owned lookup names a real span whose range contains the queried
    /// address — and that span is exactly the one we installed for the page
    /// (P-Map-001/003).
    #[test]
    fn pagemap_lookup_is_sound(specs in prop::collection::vec((1u32..=4, 0u32..=3), 0..40)) {
        let meta = leak_arena(8 * 1024 * 1024);
        let pm = PageMap::new();
        let spans = lay_out_spans(&specs);
        // Descriptors are fixed in `spans` (no realloc after this), so the raw
        // pointers the pagemap stores remain valid for the lookups below.
        for s in &spans {
            pm.install_span(&meta, s).unwrap();
        }

        for s in &spans {
            let (lo, hi) = s.range();
            // Probe the first/last byte, both page boundaries, and the midpoint.
            let probes = [lo, lo + 1, lo + PAGE_SIZE.min(hi - lo - 1), (lo + hi) / 2, hi - 1];
            for addr in probes {
                let entry = pm.lookup(addr);
                let ptr = entry.span_ptr().expect("an owned page must resolve to a span");
                // P-Map-001/003: it is exactly the span we installed for that page.
                prop_assert_eq!(ptr, s as *const SpanDescriptor);
                // pagemap_lookup_sound: the named span's range contains the address.
                // SAFETY: `ptr` is one of our live, fixed descriptors.
                let named = unsafe { &*ptr };
                let (slo, shi) = named.range();
                prop_assert!(slo <= addr && addr < shi, "span range must contain addr");
                prop_assert_eq!(named.size_class(), SizeClassId::new(0));
                prop_assert_eq!(named.arena(), ArenaId::DEFAULT);
            }
        }
    }
}

proptest! {
    /// P-Map-002: an address beyond every installed span (and beyond the supported
    /// VA) is non-owned. Complements the soundness direction above.
    #[test]
    fn unowned_addresses_are_non_owned(addr in any::<u64>()) {
        let meta = leak_arena(1024 * 1024);
        let pm = PageMap::new();
        // One span at a fixed, low window; anything far above it is non-owned.
        let s = SpanDescriptor::new(SpanId(0), ArenaId::DEFAULT, SizeClassId::new(0), 0x4000_0000, 1, 64, 0);
        pm.install_span(&meta, &s).unwrap();

        let addr = addr as usize;
        let (lo, hi) = s.range();
        let owned = addr >= lo && addr < hi;
        let entry = pm.lookup(addr);
        if owned {
            prop_assert!(!entry.is_empty());
        } else {
            prop_assert!(entry.is_empty(), "addr {addr:#x} outside the span must be non-owned");
        }
        // An address at/above the supported VA is always non-owned (and never
        // panics the radix index).
        if addr >> (PAGENO_BITS + PAGE_SHIFT) != 0 {
            prop_assert!(pm.lookup(addr).is_empty());
        }
    }
}

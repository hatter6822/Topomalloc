// SPDX-License-Identifier: MIT
//! End-to-end W3 acceptance: the metadata substrate composes (plan 03 W3).
//!
//! This wires the whole W3 stack together exactly as the M1 allocator will, but
//! before any arena exists (DD-2 verify): the **bootstrap** metadata allocator
//! (W3-1) is the source for both a **span descriptor** (W3-2) *and* the **pagemap**
//! radix nodes (W3-3); the descriptor is **installed** through the single mutator
//! path (W3-6) and then **classified** (W3-4) — base, interior, the descriptor's own
//! metadata address, and a foreign address. It proves the pieces interlock with no
//! dependency on the public allocator (S-007).

use core::mem::{align_of, size_of};

use topo_core::bootstrap::Bootstrap;
use topo_core::ids::{ArenaId, SizeClassId, SpanId};
use topo_core::pagemap::PageMap;
use topo_core::ptr_class::{classify_ptr, validate_free, FreeTarget, InvalidFree, PointerClass};
use topo_core::size_class;
use topo_core::span::SpanDescriptor;

/// A leaked, page-aligned region to back the bootstrap allocator for the test's
/// lifetime (matching the `'static` reserve real bootstrap regions have).
fn leak_region(bytes: usize) -> (*mut u8, usize) {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    (Box::into_raw(buf).cast::<u8>(), len)
}

#[test]
fn bootstrap_metadata_drives_pagemap_and_classification_before_any_arena() {
    // --- W3-1: a bootstrap allocator over a static-like reserve, no arena yet ---
    let (base, len) = leak_region(2 * 1024 * 1024);
    let boot = Bootstrap::new();
    // SAFETY: `leak_region` leaks a live 2 MiB buffer for the process.
    assert!(unsafe { boot.init(base, len) });
    assert!(boot.is_initialized());

    // --- W3-2: place a span descriptor *in bootstrap metadata* (not on the heap) ---
    let span_addr = 0x4000_0000usize;
    let mem = boot
        .alloc(size_of::<SpanDescriptor>(), align_of::<SpanDescriptor>())
        .expect("bootstrap serves a descriptor before any arena exists");
    let desc_ptr = mem.as_ptr().cast::<SpanDescriptor>();
    // The bitmap for a 64-object slab is inline, so the descriptor needs no further
    // metadata; the seam still receives `boot` (it would serve an out-of-line bitmap
    // for a high-count class — also from bootstrap).
    let descriptor = SpanDescriptor::new(
        SpanId(1),
        ArenaId::DEFAULT,
        SizeClassId::new(0), // 16-byte class
        span_addr,
        1,  // one 16 KiB page
        64, // 64 carved objects
        0,
        &boot,
    )
    .expect("bootstrap serves the descriptor's bitmap before any arena exists");
    // SAFETY: `mem` is a fresh, exclusively-owned, suitably-aligned allocation of
    // exactly `size_of::<SpanDescriptor>()` bytes; we initialize it in place.
    unsafe {
        desc_ptr.write(descriptor);
    }
    // SAFETY: just initialized; the descriptor lives in the leaked region (`'static`).
    let span = unsafe { &*desc_ptr };

    // --- W3-3 + W3-6: install through the single mutator path, nodes from boot ---
    let pm = PageMap::new();
    pm.install_span(&boot, span)
        .expect("pagemap nodes come from bootstrap");

    let size = size_class::usable_size(span.size_class());
    let base0 = span.object0_base().unwrap();

    // --- W3-4: classify across the taxonomy ---
    // A base pointer to object 7 is a valid Small free.
    let obj7 = base0 + 7 * size;
    match classify_ptr(&pm, &boot, obj7) {
        PointerClass::Small {
            object_index, sc, ..
        } => {
            assert_eq!(object_index, 7);
            assert_eq!(sc, SizeClassId::new(0));
        }
        other => panic!("expected Small base pointer, got {other:?}"),
    }
    assert!(matches!(
        validate_free(&pm, &boot, obj7),
        Ok(FreeTarget::Small {
            object_index: 7,
            ..
        })
    ));

    // An interior pointer (mid-object) is detected and rejected, not acted on.
    assert!(matches!(
        validate_free(&pm, &boot, obj7 + 5),
        Err(InvalidFree::Interior { offset: 5, .. })
    ));

    // The descriptor's *own* address is allocator metadata, distinct from foreign.
    assert!(matches!(
        classify_ptr(&pm, &boot, desc_ptr as usize),
        PointerClass::Metadata
    ));

    // A wholly unrelated address is foreign (P-Map-002).
    assert!(matches!(
        classify_ptr(&pm, &boot, 0xffff_dead_0000),
        PointerClass::External
    ));

    // free(NULL) is a no-op (§9.6), not an error.
    assert!(matches!(validate_free(&pm, &boot, 0), Ok(FreeTarget::Noop)));

    // --- W3-1b: hand off; already-vended metadata stays valid, lookups still work ---
    assert!(boot.hand_off());
    assert!(boot.is_handed_off());
    assert_eq!(
        pm.lookup(obj7).span_ptr().unwrap(),
        desc_ptr as *const SpanDescriptor
    );
}

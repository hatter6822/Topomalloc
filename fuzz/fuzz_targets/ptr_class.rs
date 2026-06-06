// SPDX-License-Identifier: MIT
//! Fuzz target for pointer classification (W3-4, §17.5). The property: over an
//! arbitrary pagemap layout and an arbitrary query address, `classify_ptr` is
//! **total** (never panics, never wraps, no OOB radix index) and **self-consistent**
//! — a `Small`/`Large` result is a genuine base pointer that recomputes to the
//! queried address, and `validate_free` accepts exactly the base-pointer/`Null`
//! cases and rejects the rest (the §17.5 base-pointer-only rule).
#![no_main]

use libfuzzer_sys::fuzz_target;
use topo_core::bootstrap::BumpArena;
use topo_core::ids::{ArenaId, SizeClassId, SpanId};
use topo_core::pagemap::PageMap;
use topo_core::ptr_class::{classify_ptr, validate_free, FreeTarget, InvalidFree, PointerClass};
use topo_core::size_class;
use topo_core::span::SpanDescriptor;

fuzz_target!(|data: &[u8]| {
    if data.len() < 9 {
        return;
    }
    // Metadata + pagemap over a *local* buffer, so nothing leaks across the many
    // iterations of one fuzz run; every pointer stays within this call.
    let mut buf = vec![0u8; 512 * 1024];
    // SAFETY: `buf` is declared first, so it outlives `meta`, the pagemap, and the
    // descriptors below (locals drop in reverse declaration order); it is not
    // reallocated (no `push`).
    let meta = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
    let pm = PageMap::new();

    // Lay out up to four disjoint spans driven by the input.
    let nspans = (data[0] % 5) as usize;
    let mut descs: Vec<SpanDescriptor> = Vec::with_capacity(nspans);
    let mut base = 0x1_0000_0000usize;
    for i in 0..nspans {
        let sc = (data.get(1 + i).copied().unwrap_or(0) % 16) as usize;
        let objects = 1 + (u32::from(data.get(5 + i).copied().unwrap_or(1)) % 200);
        if let Some(d) = SpanDescriptor::new(
            SpanId(i as u32),
            ArenaId::DEFAULT,
            SizeClassId::new(sc),
            base,
            1,
            objects,
            0,
            &meta,
        ) {
            descs.push(d);
        }
        // Separate spans by 64 MiB so they never share a page.
        base = base.wrapping_add(64 * 1024 * 1024);
    }
    for d in &descs {
        let _ = pm.install_span(&meta, d);
    }

    // The query address: the last eight bytes of the input.
    let addr = usize::from_le_bytes(data[data.len() - 8..].try_into().unwrap());

    // Totality + self-consistency of the classification.
    let class = classify_ptr(&pm, &meta, addr);
    match class {
        PointerClass::Small {
            span,
            sc,
            object_index,
            ..
        } => {
            // SAFETY: `span` is one of our live, fixed descriptors.
            let d = unsafe { &*span };
            let obj0 = d.object0_base().expect("real span geometry");
            let size = size_class::usable_size(sc);
            // A `Small` base pointer recomputes to exactly the queried address and
            // names a carved object.
            assert_eq!(obj0 + object_index * size, addr);
            assert!(object_index < d.object_count() as usize);
        }
        PointerClass::Large { desc } => {
            // SAFETY: `desc` is a live descriptor (none are installed here, so this
            // arm is unreachable, but the check is sound regardless).
            assert_eq!(unsafe { &*desc }.base(), addr);
        }
        _ => {}
    }

    // `validate_free` accepts exactly base pointers / `Null` and rejects the rest.
    let vf = validate_free(&pm, &meta, addr);
    match (&vf, &class) {
        (Ok(FreeTarget::Noop), PointerClass::Null)
        | (Ok(FreeTarget::Small { .. }), PointerClass::Small { .. })
        | (Ok(FreeTarget::Large { .. }), PointerClass::Large { .. })
        | (Err(InvalidFree::Interior { .. }), PointerClass::Interior { .. })
        | (Err(InvalidFree::Metadata), PointerClass::Metadata)
        | (Err(InvalidFree::Released), PointerClass::Released)
        | (Err(InvalidFree::DoubleFreeQuarantined), PointerClass::Quarantined)
        | (Err(InvalidFree::Foreign), PointerClass::External) => {}
        (a, b) => panic!("validate_free/classify_ptr disagree: {a:?} vs {b:?}"),
    }
});

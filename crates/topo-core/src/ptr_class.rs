// SPDX-License-Identifier: MIT
//! Pointer classification (§17.5, plan 03 W3-4).
//!
//! Given an arbitrary address, [`classify_ptr`] consults the pagemap (W3-3) and
//! returns the §17.5 class: `Null`, `Small`, `Large`, `Interior`, `Metadata`,
//! `Released`, `Quarantined`, or `External`. This is what an unsized `free`,
//! `realloc`, or `usable_size` runs first, and what the invalid-free path
//! (W3-4b, debug/hardened) uses to detect a free of an interior or foreign
//! pointer (§8.4, ties plan 08 W18-2).
//!
//! `free` requires a **base pointer** returned by an allocation API (§17.5); an
//! interior pointer is an invalid free. [`validate_free`] enforces exactly that:
//! it maps the class to a [`FreeTarget`] for a valid base pointer (or a `Null`
//! no-op, §9.6) and to an [`InvalidFree`] otherwise. Performance builds may skip
//! the check (a non-base free is a contract violation, §17.5); debug/hardened
//! builds run it to *detect and report* — never act on — a bad free.
//!
//! Reading a descriptor through the pagemap's raw pointer is sound because
//! descriptors live in monotonic metadata and are never freed (§27.5, W3-5); a
//! recycle is caught by the generation, not by invalidating the pointer.

use crate::ids::{ArenaId, SizeClassId};
use crate::pagemap::{PageEntry, PageMap};
use crate::size_class;
use crate::span::{LargeDescriptor, LargeState, SpanDescriptor, SpanState};

/// A set of allocator **metadata** address ranges, consulted when a pointer is not
/// owned by any span/large allocation, to distinguish a metadata pointer (§17.5
/// `MetadataPointer`) from a genuinely foreign one. Implemented by the bootstrap
/// arena and, after hand-off, by the normal metadata allocator (§17.4).
pub trait MetadataRegion {
    /// Whether `addr` lies in an allocator-metadata range.
    fn contains(&self, addr: usize) -> bool;
}

/// The empty metadata set — every address is non-metadata. For callers that have
/// no metadata ranges to consult (e.g. a pure pagemap test); such a context
/// reports a non-owned pointer as `External` rather than `Metadata`.
pub struct NoMetadata;

impl MetadataRegion for NoMetadata {
    #[inline]
    fn contains(&self, _addr: usize) -> bool {
        false
    }
}

impl MetadataRegion for crate::bootstrap::Bootstrap {
    #[inline]
    fn contains(&self, addr: usize) -> bool {
        crate::bootstrap::Bootstrap::contains(self, addr)
    }
}

impl MetadataRegion for crate::bootstrap::BumpArena {
    #[inline]
    fn contains(&self, addr: usize) -> bool {
        crate::bootstrap::BumpArena::contains(self, addr)
    }
}

/// The classification of a pointer (§17.5). The `Small`/`Large` descriptor pointers
/// are into monotonic metadata and stay valid for the allocator's life (§27.5).
#[derive(Clone, Copy, Debug)]
pub enum PointerClass {
    /// The null pointer (`free(NULL)` is a no-op, §9.6).
    Null,
    /// A valid **base** pointer to object `object_index` of a small slab span.
    Small {
        /// The owning span descriptor.
        span: *const SpanDescriptor,
        /// The object's size class.
        sc: SizeClassId,
        /// The owning arena.
        arena: ArenaId,
        /// The object's index within the slab (`0 <= index < object_count`).
        object_index: usize,
    },
    /// A valid **base** pointer to a large allocation.
    Large {
        /// The owning large-allocation descriptor.
        desc: *const LargeDescriptor,
    },
    /// A non-base pointer into an allocation TopoMalloc owns (mid-object, slab
    /// header, or trailing padding) — an **invalid free** (§17.5). Carries the base
    /// of the containing allocation and the byte offset into it, for diagnostics.
    Interior {
        /// Base address of the containing object/allocation.
        base: usize,
        /// Byte offset of the pointer past `base`.
        offset: usize,
    },
    /// A pointer into allocator metadata (§17.5 `MetadataPointer`) — never a valid
    /// user free.
    Metadata,
    /// A pointer into a released-but-retained page (§20.1, P-Map-005) — the backing
    /// is gone; freeing it is a bug.
    Released,
    /// A pointer to an object currently held in quarantine (§17.5) — typically a
    /// double-free signal.
    Quarantined,
    /// A pointer TopoMalloc does not own (P-Map-002) — foreign.
    External,
}

/// Classify `addr` against the pagemap and metadata ranges (§17.5).
///
/// O(1): one pagemap lookup plus, for an owned page, the §16.3 slab-layout inverse
/// to decide base-vs-interior. Total — never panics. An unowned address is
/// `Metadata` if `meta` claims it, else `External`.
pub fn classify_ptr(pm: &PageMap, meta: &dyn MetadataRegion, addr: usize) -> PointerClass {
    if addr == 0 {
        return PointerClass::Null;
    }
    match pm.lookup(addr) {
        PageEntry::Empty => {
            if meta.contains(addr) {
                PointerClass::Metadata
            } else {
                PointerClass::External
            }
        }
        PageEntry::Small(span_ptr) => classify_in_span(span_ptr, addr),
        PageEntry::Large(desc_ptr) => classify_in_large(desc_ptr, addr),
        PageEntry::Released(_) => PointerClass::Released,
    }
}

/// Classify `addr`, known to lie in a `Small` span's page, by the §16.3 slab
/// layout. A quarantined span short-circuits to `Quarantined` (§17.5).
fn classify_in_span(span_ptr: *const SpanDescriptor, addr: usize) -> PointerClass {
    // SAFETY: `span_ptr` came from a pagemap `Small` entry, which the single
    // mutator path (W3-6) only publishes for a live descriptor in monotonic
    // metadata; descriptors are never freed (§27.5), so the read is valid.
    let span = unsafe { &*span_ptr };

    // Consult the descriptor's own state so classification stays consistent during
    // the W3-6 release window — when the span is marked `Released` but its pagemap
    // entries have not yet been overwritten, a stale `Small` entry must still
    // classify as `Released` (the eventual `ReleasedRetained` entry does too).
    if span.state() == SpanState::Released {
        return PointerClass::Released;
    }
    if span.is_quarantined() {
        return PointerClass::Quarantined;
    }

    let object0 = match span.object0_base() {
        Some(b) => b,
        // Geometry overflow is unreachable for a real span; treat defensively as
        // interior to the span base rather than fabricating an object index.
        None => {
            return PointerClass::Interior {
                base: span.base(),
                offset: addr.wrapping_sub(span.base()),
            }
        }
    };
    let size = size_class::usable_size(span.size_class());
    let object_count = span.object_count() as usize;

    // Below object 0 ⇒ in the slab header region (interior to the span).
    if addr < object0 {
        return PointerClass::Interior {
            base: span.base(),
            offset: addr - span.base(),
        };
    }
    let delta = addr - object0;
    let index = delta / size;
    let remainder = delta % size;

    // Past the last carved object ⇒ trailing padding (interior to the span).
    if index >= object_count {
        return PointerClass::Interior {
            base: span.base(),
            offset: addr - span.base(),
        };
    }
    let object_base = object0 + index * size;
    if remainder == 0 {
        // A clean base pointer to object `index`.
        PointerClass::Small {
            span: span_ptr,
            sc: span.size_class(),
            arena: span.arena(),
            object_index: index,
        }
    } else {
        // Mid-object ⇒ interior to object `index`.
        PointerClass::Interior {
            base: object_base,
            offset: remainder,
        }
    }
}

/// Classify `addr`, known to lie in a `Large` allocation's page (§17.5).
fn classify_in_large(desc_ptr: *const LargeDescriptor, addr: usize) -> PointerClass {
    // SAFETY: as in `classify_in_span` — a pagemap `Large` entry names a live,
    // never-freed descriptor in monotonic metadata.
    let desc = unsafe { &*desc_ptr };
    // As in `classify_in_span`: a `Released` descriptor classifies as `Released`
    // even if its pagemap entry has not yet been overwritten (W3-6 release window).
    if desc.state() == LargeState::Released {
        return PointerClass::Released;
    }
    if addr == desc.base() {
        PointerClass::Large { desc: desc_ptr }
    } else {
        // Any non-base address in the large allocation's pages is interior.
        PointerClass::Interior {
            base: desc.base(),
            offset: addr.wrapping_sub(desc.base()),
        }
    }
}

/// What a valid `free` resolves to.
#[derive(Clone, Copy, Debug)]
pub enum FreeTarget {
    /// `free(NULL)` — a no-op (§9.6).
    Noop,
    /// Free object `object_index` of a small slab span.
    Small {
        /// The owning span.
        span: *const SpanDescriptor,
        /// The object index within the slab.
        object_index: usize,
    },
    /// Free a large allocation.
    Large {
        /// The owning descriptor.
        desc: *const LargeDescriptor,
    },
}

/// Why a `free` is invalid (the §17.5 invalid-free taxonomy, W3-4b). Detected and
/// reported in debug/hardened; in performance the contract simply requires a base
/// pointer (§17.5) and the check is skipped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidFree {
    /// A non-base (interior) pointer into an owned allocation, with the containing
    /// base and the offset into it.
    Interior {
        /// Base of the containing object/allocation.
        base: usize,
        /// Byte offset of the freed pointer past `base`.
        offset: usize,
    },
    /// A pointer TopoMalloc does not own (foreign).
    Foreign,
    /// A pointer into allocator metadata.
    Metadata,
    /// A pointer into released-but-retained memory.
    Released,
    /// A pointer already in quarantine (a double-free signal).
    DoubleFreeQuarantined,
}

/// Validate a `free` of `addr` (W3-4a/b): enforce the **base-pointer-only** rule
/// (§17.5). Returns the [`FreeTarget`] for a `Null` no-op or a valid base pointer,
/// or an [`InvalidFree`] describing the violation. The caller decides what to do
/// with an error — debug/hardened reports it; performance need not call this at
/// all (the contract presumes a base pointer).
pub fn validate_free(
    pm: &PageMap,
    meta: &dyn MetadataRegion,
    addr: usize,
) -> Result<FreeTarget, InvalidFree> {
    match classify_ptr(pm, meta, addr) {
        PointerClass::Null => Ok(FreeTarget::Noop),
        PointerClass::Small {
            span, object_index, ..
        } => Ok(FreeTarget::Small { span, object_index }),
        PointerClass::Large { desc } => Ok(FreeTarget::Large { desc }),
        PointerClass::Interior { base, offset } => Err(InvalidFree::Interior { base, offset }),
        PointerClass::Metadata => Err(InvalidFree::Metadata),
        PointerClass::Released => Err(InvalidFree::Released),
        PointerClass::Quarantined => Err(InvalidFree::DoubleFreeQuarantined),
        PointerClass::External => Err(InvalidFree::Foreign),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::generated::tables::PAGE_SIZE;
    use crate::ids::{ArenaId, LargeId, SizeClassId, SpanId};
    use crate::size_class;
    use crate::span::{LargeDescriptor, SpanDescriptor, SpanState};

    fn meta_arena(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn span(base: usize, sc: usize, objects: u32) -> SpanDescriptor {
        SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            SizeClassId::new(sc),
            base,
            1,
            objects,
            0,
        )
    }

    #[test]
    fn null_classifies_null() {
        let pm = PageMap::new();
        assert!(matches!(
            classify_ptr(&pm, &NoMetadata, 0),
            PointerClass::Null
        ));
    }

    #[test]
    fn foreign_pointer_is_external() {
        let pm = PageMap::new();
        assert!(matches!(
            classify_ptr(&pm, &NoMetadata, 0xdead_0000),
            PointerClass::External
        ));
    }

    #[test]
    fn small_base_pointers_classify_small_with_object_index() {
        // A 16-byte class slab: object i is at base + i*16. Every base pointer
        // classifies Small with the right index; mid-object pointers are Interior.
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        let size = size_class::usable_size(s.size_class());
        let base0 = s.object0_base().unwrap();

        for i in [0usize, 1, 31, 63] {
            let addr = base0 + i * size;
            match classify_ptr(&pm, &m, addr) {
                PointerClass::Small {
                    object_index, sc, ..
                } => {
                    assert_eq!(object_index, i);
                    assert_eq!(sc, SizeClassId::new(0));
                }
                other => panic!("expected Small at object {i}, got {other:?}"),
            }
        }
    }

    #[test]
    fn interior_pointers_are_detected() {
        // W3-4b: a pointer one byte into an object is Interior (invalid free), with
        // the object base and offset reported.
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        let size = size_class::usable_size(s.size_class());
        let base0 = s.object0_base().unwrap();

        let interior = base0 + size + 4; // 4 bytes into object 1
        match classify_ptr(&pm, &m, interior) {
            PointerClass::Interior { base, offset } => {
                assert_eq!(base, base0 + size);
                assert_eq!(offset, 4);
            }
            other => panic!("expected Interior, got {other:?}"),
        }
        // validate_free rejects it, reporting the interior offset (not acting).
        assert!(matches!(
            validate_free(&pm, &m, interior),
            Err(InvalidFree::Interior { base, offset }) if base == base0 + size && offset == 4
        ));
    }

    #[test]
    fn trailing_padding_is_interior_not_a_phantom_object() {
        // Past the last carved object, within the owned page, is interior — never a
        // fabricated object index beyond object_count.
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        // Only 4 objects carved in a page that could hold 1024.
        let s = span(0x4000_0000, 0, 4);
        pm.install_span(&m, &s).unwrap();
        let size = size_class::usable_size(s.size_class());
        let base0 = s.object0_base().unwrap();
        let past = base0 + 10 * size; // well past object 3, still in the page
        assert!(matches!(
            classify_ptr(&pm, &m, past),
            PointerClass::Interior { .. }
        ));
    }

    #[test]
    fn large_base_and_interior() {
        let m = meta_arena(512 * 1024);
        let pm = PageMap::new();
        let d = LargeDescriptor::new(LargeId(1), ArenaId::DEFAULT, 0x1_0000_0000, 3_000_000, 4096);
        pm.install_large(&m, &d).unwrap();

        match classify_ptr(&pm, &m, d.base()) {
            PointerClass::Large { desc } => assert_eq!(desc, &d as *const LargeDescriptor),
            other => panic!("expected Large, got {other:?}"),
        }
        // One byte in is interior.
        match classify_ptr(&pm, &m, d.base() + 1) {
            PointerClass::Interior { base, offset } => {
                assert_eq!(base, d.base());
                assert_eq!(offset, 1);
            }
            other => panic!("expected Interior, got {other:?}"),
        }
    }

    #[test]
    fn released_pages_classify_released() {
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        s.set_state(SpanState::Released);
        pm.release_span(&s);
        assert!(matches!(
            classify_ptr(&pm, &m, s.base()),
            PointerClass::Released
        ));
        assert!(matches!(
            validate_free(&pm, &m, s.base()),
            Err(InvalidFree::Released)
        ));
    }

    #[test]
    fn released_state_classifies_released_even_before_entries_are_overwritten() {
        // W3-6 release window: the descriptor is marked Released first, before the
        // pagemap entries are overwritten. A classifier hitting a still-`Small`
        // entry must already see `Released` (from the descriptor state), consistent
        // with the eventual `ReleasedRetained` entry.
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        // Mark Released but do NOT call release_span yet (the entry is still Small).
        s.set_state(SpanState::Released);
        assert!(matches!(pm.lookup(s.base()), PageEntry::Small(_)));
        assert!(matches!(
            classify_ptr(&pm, &m, s.base()),
            PointerClass::Released
        ));
    }

    #[test]
    fn quarantined_span_classifies_quarantined() {
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        s.set_quarantined(true);
        let base0 = s.object0_base().unwrap();
        assert!(matches!(
            classify_ptr(&pm, &m, base0),
            PointerClass::Quarantined
        ));
        assert!(matches!(
            validate_free(&pm, &m, base0),
            Err(InvalidFree::DoubleFreeQuarantined)
        ));
    }

    #[test]
    fn metadata_pointers_are_distinguished_from_foreign() {
        // A pointer into the metadata arena classifies Metadata; an unrelated
        // address classifies External. Both are invalid frees, but distinguished
        // for diagnostics (§17.5).
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        // Pull a metadata allocation so the arena's used range is non-trivial.
        let node = m.alloc(64, 16).unwrap();
        let meta_addr = node.as_ptr() as usize;
        assert!(matches!(
            classify_ptr(&pm, &m, meta_addr),
            PointerClass::Metadata
        ));
        assert!(matches!(
            validate_free(&pm, &m, meta_addr),
            Err(InvalidFree::Metadata)
        ));
        // An address outside both the pagemap and the metadata arena is foreign.
        assert!(matches!(
            classify_ptr(&pm, &m, 0xffff_0000_0000),
            PointerClass::External
        ));
    }

    #[test]
    fn validate_free_accepts_base_and_null_only() {
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        let s = span(0x4000_0000, 0, 64);
        pm.install_span(&m, &s).unwrap();
        let base0 = s.object0_base().unwrap();

        // Null is a no-op; a base pointer is a valid Small free.
        assert!(matches!(validate_free(&pm, &m, 0), Ok(FreeTarget::Noop)));
        assert!(matches!(
            validate_free(&pm, &m, base0),
            Ok(FreeTarget::Small {
                object_index: 0,
                ..
            })
        ));
        // A foreign pointer is rejected.
        assert!(matches!(
            validate_free(&pm, &m, 0x10),
            Err(InvalidFree::Foreign)
        ));
    }

    #[test]
    fn interior_pointer_across_a_page_boundary_stays_interior() {
        // A multi-page span: an address in the second page that is not an object
        // base is still interior (the lookup masks to the page, then the layout
        // inverse decides).
        let m = meta_arena(256 * 1024);
        let pm = PageMap::new();
        // 16 KiB page / 1024-byte class is sc index 31 (size 1024). Use sc 0 (16B)
        // with 2 pages so object indices span the boundary.
        let s = SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            0x4000_0000,
            2,
            2048,
            0,
        );
        pm.install_span(&m, &s).unwrap();
        let size = size_class::usable_size(s.size_class());
        let base0 = s.object0_base().unwrap();
        // An object base in the second page classifies Small.
        let in_second_page = base0 + (PAGE_SIZE / size) * size;
        assert!(matches!(
            classify_ptr(&pm, &m, in_second_page),
            PointerClass::Small { .. }
        ));
        // A mid-object address in the second page is Interior.
        assert!(matches!(
            classify_ptr(&pm, &m, in_second_page + 3),
            PointerClass::Interior { .. }
        ));
    }
}

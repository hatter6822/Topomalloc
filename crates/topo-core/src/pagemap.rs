// SPDX-License-Identifier: MIT
//! The pagemap: address → descriptor, concurrently (§17.1/§17.2, DD-1, plan 03
//! W3-3 + W3-6).
//!
//! Every unsized `free`, `realloc`, `usable_size`, and debug check must map an
//! arbitrary address to the descriptor that owns it (§17.1) in O(1), correctly
//! classifying interior/foreign/released pointers, **while spans are concurrently
//! created and recycled**. The lookup is on the free hot path; the update races
//! span lifecycle.
//!
//! **Structure (W3-3a).** A fixed-fan-out, three-level radix over allocator-page
//! numbers (DD-1 choice (c)): O(1) worst-case, lazily populated, no resize, and
//! interior-pointer lookup by masking an address down to its page. A flat array
//! would waste virtual space on 64-bit; a hash map has worst-case and resize
//! hazards on the hot path. Interior nodes are allocated on first touch from the
//! [`MetadataAlloc`] seam (bootstrap metadata now, the normal metadata allocator
//! after hand-off, §17.4).
//!
//! **Entry encoding (W3-3b).** Each leaf slot is a tagged pointer: `Empty` (the
//! zero word — a non-owned page, P-Map-002), `Small`/`Large` (a pointer to the
//! owning [`SpanDescriptor`]/[`LargeDescriptor`]), or `ReleasedRetained` (a span
//! pointer whose state is `Released`, kept so the page cannot be reused without
//! recommit, P-Map-005). Descriptors are ≥ 8-aligned, so the low three bits carry
//! the tag.
//!
//! **Publish/read protocol (W3-3c / W3-6 / P-Map-006).** A leaf slot is filled
//! with a **release store** only *after* its descriptor (and generation) are fully
//! initialized; readers **acquire-load**. New radix nodes are zeroed (all-`Empty`)
//! *before* being linked with a release CAS, so a concurrent reader either misses
//! the node (→ `Empty`, correct: nothing is installed there yet) or sees it fully
//! formed — never a half-built node (failure mode F1). Because descriptors live in
//! monotonic metadata and are recycled in place with a generation bump (§27.5,
//! W3-5), a stale pointer is always dereferenceable and the generation flags the
//! reuse (failure mode F2). **This module is the single mutator** (failure mode
//! F3): span split/merge (W4-2b) and span lifecycle (W5-5) MUST route every
//! pagemap change through [`install_span`](PageMap::install_span) /
//! [`release_span`](PageMap::release_span) / [`retire_span`](PageMap::retire_span)
//! / [`install_large`](PageMap::install_large), never poking a leaf directly.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::PAGE_SIZE;
use crate::span::{LargeDescriptor, SpanDescriptor, SpanState};

// --- radix geometry (D4: chosen for the 16 KiB allocator page) --------------

/// `log2(PAGE_SIZE)` — the page-offset bit width (14 for a 16 KiB page).
pub const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();

/// Supported virtual-address width. 48 bits covers 4-level paging on x86-64 and
/// AArch64 (the deployment targets). An address at or beyond `2^VA_BITS` cannot be
/// allocator-owned — we never reserve there — so it classifies as `Empty`
/// (non-owned, P-Map-002) without indexing out of the radix.
pub const VA_BITS: u32 = 48;

/// Width of an allocator-page number (`VA_BITS - PAGE_SHIFT` = 34 bits).
pub const PAGENO_BITS: u32 = VA_BITS - PAGE_SHIFT;

/// Bits of the page number consumed by each radix level (high → low). The three
/// sum to [`PAGENO_BITS`]; the two interior levels share a width so root and mid
/// nodes are one type.
const ROOT_BITS: u32 = 11;
const MID_BITS: u32 = 11;
const LEAF_BITS: u32 = 12;
const _: () = assert!(ROOT_BITS + MID_BITS + LEAF_BITS == PAGENO_BITS);

/// Child slots per interior node (root and mid).
const INTERIOR_SLOTS: usize = 1 << ROOT_BITS; // == 1 << MID_BITS
const _: () = assert!((1usize << ROOT_BITS) == (1usize << MID_BITS));
/// Entries per leaf node (one per allocator page).
const LEAF_SLOTS: usize = 1 << LEAF_BITS;

/// One interior radix node: a lazily-populated array of child pointers (root → mid
/// nodes, mid → leaf nodes). Children are type-erased `*mut u8` and re-typed by
/// level; `null` means "not populated".
#[repr(C)]
struct Interior {
    slots: [AtomicPtr<u8>; INTERIOR_SLOTS],
}

/// One leaf radix node: a [`PageEntry`]-encoded word per allocator page. The
/// all-zero state is every-page-`Empty`, so a freshly zeroed leaf is valid.
#[repr(C)]
struct Leaf {
    entries: [AtomicUsize; LEAF_SLOTS],
}

// --- entry encoding (W3-3b) -------------------------------------------------

/// Low-bit tag width. Descriptor pointers are ≥ 8-aligned, so three low bits are
/// free for the tag.
const TAG_MASK: usize = 0b111;
const TAG_EMPTY: usize = 0; // the whole word is 0 ⇒ a non-owned page
const TAG_SMALL: usize = 1;
const TAG_LARGE: usize = 2;
const TAG_RELEASED: usize = 3;

/// A decoded leaf entry (W3-3b). The pointers are into monotonic metadata and stay
/// valid for the allocator's life (§27.5); a recycle is caught by the generation,
/// not by freeing the descriptor.
#[derive(Clone, Copy, Debug)]
pub enum PageEntry {
    /// The page is not owned by TopoMalloc (P-Map-002) — the zero word.
    Empty,
    /// A small-object slab page owned by this span (P-Map-003).
    Small(*const SpanDescriptor),
    /// A large-allocation page owned by this descriptor (P-Map-004).
    Large(*const LargeDescriptor),
    /// A released-but-retained page (P-Map-005): backing returned to the OS, the
    /// virtual range and descriptor kept so it cannot be reused without recommit.
    Released(*const SpanDescriptor),
}

impl PageEntry {
    /// Whether the page is non-owned.
    #[inline]
    pub fn is_empty(&self) -> bool {
        matches!(self, PageEntry::Empty)
    }

    /// The owning span pointer for a `Small`/`Released` page, else `None`.
    #[inline]
    pub fn span_ptr(&self) -> Option<*const SpanDescriptor> {
        match *self {
            PageEntry::Small(p) | PageEntry::Released(p) => Some(p),
            _ => None,
        }
    }

    /// The owning large-descriptor pointer for a `Large` page, else `None`.
    #[inline]
    pub fn large_ptr(&self) -> Option<*const LargeDescriptor> {
        match *self {
            PageEntry::Large(p) => Some(p),
            _ => None,
        }
    }

    /// Encode into the tagged-pointer word stored in a leaf slot.
    #[inline]
    fn encode(self) -> usize {
        match self {
            PageEntry::Empty => TAG_EMPTY,
            PageEntry::Small(p) => p as usize | TAG_SMALL,
            PageEntry::Large(p) => p as usize | TAG_LARGE,
            PageEntry::Released(p) => p as usize | TAG_RELEASED,
        }
    }

    /// Decode a leaf word back into a [`PageEntry`].
    #[inline]
    fn decode(bits: usize) -> PageEntry {
        let ptr = (bits & !TAG_MASK) as *const ();
        match bits & TAG_MASK {
            TAG_SMALL => PageEntry::Small(ptr.cast::<SpanDescriptor>()),
            TAG_LARGE => PageEntry::Large(ptr.cast::<LargeDescriptor>()),
            TAG_RELEASED => PageEntry::Released(ptr.cast::<SpanDescriptor>()),
            _ => PageEntry::Empty, // TAG_EMPTY, and any unexpected tag, is non-owned
        }
    }
}

/// A pagemap mutation that could not complete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PagemapError {
    /// The metadata allocator could not provide a radix node (§17.4 safe failure).
    /// The span lifecycle treats this like any other OOM: the install does not
    /// happen and the caller fails the allocation safely (§9.7).
    OutOfMetadata,
}

/// The multi-level radix pagemap (W3-3). `const`-constructible (so it can be a
/// process `static`); the root and interior nodes are allocated lazily on first
/// install from the [`MetadataAlloc`] seam.
pub struct PageMap {
    /// Root interior node, allocated on first install. `null` until then — a
    /// lookup before any install returns `Empty` (everything is non-owned).
    root: AtomicPtr<Interior>,
}

impl PageMap {
    /// An empty pagemap (no root yet). Every lookup returns `Empty` until a span is
    /// installed.
    pub const fn new() -> Self {
        Self {
            root: AtomicPtr::new(ptr::null_mut()),
        }
    }

    // --- lookup (hot path, allocation-free) ---------------------------------

    /// Look up the page containing `addr` and return its [`PageEntry`] (§17.1).
    /// O(1): three array indexes with acquire loads. Never allocates; an unmapped
    /// path or an address beyond the supported VA returns `Empty` (P-Map-002).
    ///
    /// Interior-pointer support: the lookup masks `addr` to its page, so any
    /// address *within* an owned page resolves to that page's descriptor — the
    /// caller (pointer classification, W3-4) decides base-vs-interior.
    #[inline]
    pub fn lookup(&self, addr: usize) -> PageEntry {
        let p = addr >> PAGE_SHIFT;
        // Beyond the supported VA ⇒ not owned (and not indexable). The shift is
        // safe: `PAGENO_BITS < usize::BITS`.
        if p >> PAGENO_BITS != 0 {
            return PageEntry::Empty;
        }
        let root = self.root.load(Ordering::Acquire);
        if root.is_null() {
            return PageEntry::Empty;
        }
        // SAFETY: a non-null `root` was published by `root_or_create` with a
        // release store after being zeroed; it is a valid `Interior` for the
        // pagemap's life (nodes are never freed).
        let mid = unsafe { (*root).slots[root_index(p)].load(Ordering::Acquire) };
        if mid.is_null() {
            return PageEntry::Empty;
        }
        // SAFETY: a non-null mid child is a zeroed-then-published `Interior`.
        let leaf = unsafe { (*mid.cast::<Interior>()).slots[mid_index(p)].load(Ordering::Acquire) };
        if leaf.is_null() {
            return PageEntry::Empty;
        }
        // SAFETY: a non-null leaf child is a zeroed-then-published `Leaf`.
        let bits = unsafe { (*leaf.cast::<Leaf>()).entries[leaf_index(p)].load(Ordering::Acquire) };
        PageEntry::decode(bits)
    }

    // --- the single mutator path (W3-6 / P-Map-006) -------------------------

    /// Install a span into the pagemap (P-Map-001/003): map every page the span
    /// covers to its descriptor with a **release store**, so a concurrent
    /// classifier that acquire-loads an entry sees the fully-initialized span. The
    /// caller MUST have finished initializing `span` (geometry, generation, state)
    /// before calling — the descriptor is the publish payload (W3-6).
    ///
    /// Returns [`PagemapError::OutOfMetadata`] if a radix node cannot be allocated;
    /// no partial install is observable as a wrong owner (each page is published
    /// atomically, and a page not yet reached stays `Empty`).
    ///
    /// SPEC-transition: pagemap publish for span activation (§17.2 P-Map-006)
    pub fn install_span(
        &self,
        meta: &dyn MetadataAlloc,
        span: &SpanDescriptor,
    ) -> Result<(), PagemapError> {
        let entry = PageEntry::Small(span as *const SpanDescriptor).encode();
        self.publish_range(meta, span.range(), entry)
    }

    /// Install a large allocation into the pagemap (P-Map-001/004).
    ///
    /// SPEC-transition: pagemap publish for large allocation (§17.2 P-Map-006)
    pub fn install_large(
        &self,
        meta: &dyn MetadataAlloc,
        large: &LargeDescriptor,
    ) -> Result<(), PagemapError> {
        let entry = PageEntry::Large(large as *const LargeDescriptor).encode();
        self.publish_range(meta, (large.base(), large.end()), entry)
    }

    /// Transition a span's pages to **released-but-retained** (P-Map-005): the
    /// entries keep pointing at the descriptor (now `state == Released`) so the
    /// pages cannot be reused without recommit. The caller MUST set the span's
    /// state to [`SpanState::Released`] *before* calling, so the published entry
    /// and the descriptor agree (§17.2 P-Map-006). Allocation-free — the nodes
    /// already exist from `install_span`.
    ///
    /// SPEC-transition: span `Active -> Released`, pagemap retained (§20.1)
    pub fn release_span(&self, span: &SpanDescriptor) {
        debug_assert_eq!(
            span.state(),
            SpanState::Released,
            "release_span requires the descriptor be marked Released first (W3-6 ordering)"
        );
        let entry = PageEntry::Released(span as *const SpanDescriptor).encode();
        self.overwrite_existing_range(span.range(), entry);
    }

    /// Retire a span's pages to `Empty` (the backing virtual range is being handed
    /// back, P-Map-002): no descriptor owns these pages any more. Allocation-free.
    /// After this, a classifier sees `Empty` (non-owned) for these pages.
    ///
    /// SPEC-transition: pagemap clear on span teardown (§17.2 P-Map-006)
    pub fn retire_span(&self, span: &SpanDescriptor) {
        self.overwrite_existing_range(span.range(), TAG_EMPTY);
    }

    /// Retire a large allocation's pages to `Empty`.
    ///
    /// SPEC-transition: pagemap clear on large free (§17.2 P-Map-006)
    pub fn retire_large(&self, large: &LargeDescriptor) {
        self.overwrite_existing_range((large.base(), large.end()), TAG_EMPTY);
    }

    // --- internal radix walk + publish --------------------------------------

    /// Publish `entry` to every page in `[base, stop)`, creating radix nodes as
    /// needed. The single allocating mutator; all installs funnel through here.
    fn publish_range(
        &self,
        meta: &dyn MetadataAlloc,
        (base, stop): (usize, usize),
        entry: usize,
    ) -> Result<(), PagemapError> {
        for p in page_range(base, stop) {
            let leaf = self.leaf_or_create(meta, p)?;
            // SAFETY: `leaf_or_create` returned a valid, published `Leaf`. The
            // release store publishes `entry` so an acquire-loading reader sees the
            // descriptor it encodes fully initialized (W3-3c).
            unsafe { (*leaf).entries[leaf_index(p)].store(entry, Ordering::Release) };
        }
        Ok(())
    }

    /// Overwrite every *already-mapped* page in `[base, stop)` with `entry`. Used
    /// by release/retire, which never need to create nodes (the pages were mapped
    /// at install). A page whose node is absent is skipped (defensive: it was
    /// already non-owned).
    fn overwrite_existing_range(&self, (base, stop): (usize, usize), entry: usize) {
        for p in page_range(base, stop) {
            if let Some(leaf) = self.find_leaf(p) {
                // SAFETY: `find_leaf` returned a valid, published `Leaf`.
                unsafe { (*leaf).entries[leaf_index(p)].store(entry, Ordering::Release) };
            }
        }
    }

    /// The root, allocating + publishing it on first use.
    fn root_or_create(&self, meta: &dyn MetadataAlloc) -> Result<*mut Interior, PagemapError> {
        let cur = self.root.load(Ordering::Acquire);
        if !cur.is_null() {
            return Ok(cur);
        }
        let node = alloc_interior(meta)?.cast::<Interior>();
        match self.root.compare_exchange(
            ptr::null_mut(),
            node,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(node),
            // Lost the race: another thread published a root first. Use it; our
            // node leaks (monotonic metadata — never freed, a rare bounded waste).
            Err(winner) => Ok(winner),
        }
    }

    /// Walk to the leaf for page `p`, creating the root/mid/leaf nodes as needed.
    fn leaf_or_create(
        &self,
        meta: &dyn MetadataAlloc,
        p: usize,
    ) -> Result<*mut Leaf, PagemapError> {
        let root = self.root_or_create(meta)?;
        // SAFETY: `root` is a valid published `Interior`.
        let mid_slot = unsafe { &(*root).slots[root_index(p)] };
        let mid = child_or_create(mid_slot, meta, alloc_interior)?.cast::<Interior>();
        // SAFETY: `mid` is a valid published `Interior`.
        let leaf_slot = unsafe { &(*mid).slots[mid_index(p)] };
        let leaf = child_or_create(leaf_slot, meta, alloc_leaf)?.cast::<Leaf>();
        Ok(leaf)
    }

    /// Walk to the leaf for page `p` without allocating; `None` if any node on the
    /// path is absent (the page is non-owned).
    fn find_leaf(&self, p: usize) -> Option<*mut Leaf> {
        if p >> PAGENO_BITS != 0 {
            return None;
        }
        let root = self.root.load(Ordering::Acquire);
        if root.is_null() {
            return None;
        }
        // SAFETY: non-null root is a valid published `Interior`.
        let mid = unsafe { (*root).slots[root_index(p)].load(Ordering::Acquire) };
        if mid.is_null() {
            return None;
        }
        // SAFETY: non-null mid child is a valid published `Interior`.
        let leaf = unsafe { (*mid.cast::<Interior>()).slots[mid_index(p)].load(Ordering::Acquire) };
        if leaf.is_null() {
            return None;
        }
        Some(leaf.cast::<Leaf>())
    }
}

impl Default for PageMap {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: all shared state is reached through atomics (the root pointer, every
// interior slot, every leaf entry); nodes are zeroed before being published with a
// release CAS/store and never freed, and the descriptors they point at are `Sync`.
// Concurrent lookups and the single mutator path are therefore data-race-free.
unsafe impl Sync for PageMap {}
// SAFETY: as the `Sync` impl above — the pagemap owns only atomics and pointers
// into `Sync` metadata, so it is sound to move across threads.
unsafe impl Send for PageMap {}

// --- node allocation (lazy, lock-free) --------------------------------------

/// Allocate and zero one interior node from `meta` (all children `null`).
fn alloc_interior(meta: &dyn MetadataAlloc) -> Result<*mut u8, PagemapError> {
    alloc_zeroed_node(
        meta,
        core::mem::size_of::<Interior>(),
        core::mem::align_of::<Interior>(),
    )
}

/// Allocate and zero one leaf node from `meta` (all entries `Empty`).
fn alloc_leaf(meta: &dyn MetadataAlloc) -> Result<*mut u8, PagemapError> {
    alloc_zeroed_node(
        meta,
        core::mem::size_of::<Leaf>(),
        core::mem::align_of::<Leaf>(),
    )
}

/// Allocate `size` bytes aligned `align` from `meta` and zero them, so the result
/// is a valid all-`null`/all-`Empty` node *before* it is linked (init-before-publish,
/// failure mode F1).
fn alloc_zeroed_node(
    meta: &dyn MetadataAlloc,
    size: usize,
    align: usize,
) -> Result<*mut u8, PagemapError> {
    let node = meta.alloc(size, align).ok_or(PagemapError::OutOfMetadata)?;
    // SAFETY: `meta.alloc` returned a fresh, exclusively-owned region of exactly
    // `size` bytes; zeroing it makes a valid node whose every atomic slot is the
    // zero (null / `Empty`) bit pattern. No other thread can observe it yet — it is
    // not linked into the radix until the caller's release CAS.
    unsafe { ptr::write_bytes(node.as_ptr(), 0, size) };
    Ok(node.as_ptr())
}

/// Return the child in `slot`, creating it with `make` on first touch. Lock-free:
/// the creator zeroes the node, then publishes it with a release CAS; a reader
/// acquire-loads. If the CAS loses, the winner's node is used and ours leaks
/// (bounded, rare — monotonic metadata is never freed).
fn child_or_create(
    slot: &AtomicPtr<u8>,
    meta: &dyn MetadataAlloc,
    make: fn(&dyn MetadataAlloc) -> Result<*mut u8, PagemapError>,
) -> Result<*mut u8, PagemapError> {
    let cur = slot.load(Ordering::Acquire);
    if !cur.is_null() {
        return Ok(cur);
    }
    let node = make(meta)?;
    match slot.compare_exchange(ptr::null_mut(), node, Ordering::Release, Ordering::Acquire) {
        Ok(_) => Ok(node),
        Err(winner) => Ok(winner),
    }
}

// --- page-number arithmetic -------------------------------------------------

/// The allocator-page number containing `addr`.
#[inline]
fn page_of(addr: usize) -> usize {
    addr >> PAGE_SHIFT
}

/// The inclusive range of page numbers covering `[base, stop)`. Empty if
/// `stop <= base`.
#[inline]
fn page_range(base: usize, stop: usize) -> core::ops::RangeInclusive<usize> {
    if stop <= base {
        // An empty byte range covers no pages: `1..=0` is an empty inclusive range.
        #[allow(clippy::reversed_empty_ranges)]
        return 1..=0;
    }
    page_of(base)..=page_of(stop - 1)
}

#[inline]
fn root_index(p: usize) -> usize {
    (p >> (MID_BITS + LEAF_BITS)) & (INTERIOR_SLOTS - 1)
}

#[inline]
fn mid_index(p: usize) -> usize {
    (p >> LEAF_BITS) & (INTERIOR_SLOTS - 1)
}

#[inline]
fn leaf_index(p: usize) -> usize {
    p & (LEAF_SLOTS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::ids::{ArenaId, LargeId, SizeClassId, SpanId};

    /// A heap-backed metadata arena big enough for a handful of radix nodes.
    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn small_span(id: u32, base: usize, pages: u32, objects: u32) -> SpanDescriptor {
        SpanDescriptor::new(
            SpanId(id),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            base,
            pages,
            objects,
            0,
        )
    }

    #[test]
    fn geometry_consts_are_consistent() {
        assert_eq!(PAGE_SHIFT, 14);
        assert_eq!(PAGENO_BITS, 34);
        assert_eq!(ROOT_BITS + MID_BITS + LEAF_BITS, PAGENO_BITS);
        // Interior nodes are uniform (root and mid share a type/width).
        assert_eq!(core::mem::size_of::<Interior>(), INTERIOR_SLOTS * 8);
        assert_eq!(core::mem::size_of::<Leaf>(), LEAF_SLOTS * 8);
        // A leaf covers exactly its page span; root covers the whole VA.
        assert_eq!(LEAF_SLOTS * PAGE_SIZE, 64 * 1024 * 1024); // 64 MiB / leaf
    }

    #[test]
    fn empty_pagemap_classifies_everything_external() {
        // P-Map-002: with nothing installed (no root), every address is non-owned.
        let pm = PageMap::new();
        assert!(pm.lookup(0).is_empty());
        assert!(pm.lookup(0x4000_0000).is_empty());
        assert!(pm.lookup(usize::MAX).is_empty());
    }

    #[test]
    fn entry_encoding_round_trips() {
        // W3-3b: every PageEntry round-trips through the tagged-pointer encoding.
        let s = small_span(1, 0x4000_0000, 1, 64);
        let sp = &s as *const SpanDescriptor;
        for e in [
            PageEntry::Empty,
            PageEntry::Small(sp),
            PageEntry::Released(sp),
        ] {
            let back = PageEntry::decode(e.encode());
            match (e, back) {
                (PageEntry::Empty, PageEntry::Empty) => {}
                (PageEntry::Small(a), PageEntry::Small(b)) => assert_eq!(a, b),
                (PageEntry::Released(a), PageEntry::Released(b)) => assert_eq!(a, b),
                _ => panic!("encoding did not round-trip"),
            }
        }
        let l = LargeDescriptor::new(LargeId(1), ArenaId::DEFAULT, 0x10_0000, 3_000_000, 4096);
        let lp = &l as *const LargeDescriptor;
        match PageEntry::decode(PageEntry::Large(lp).encode()) {
            PageEntry::Large(b) => assert_eq!(b, lp),
            _ => panic!("large did not round-trip"),
        }
    }

    #[test]
    fn install_then_lookup_returns_the_span_for_every_owned_page() {
        // P-Map-001/003: every page of an installed span maps to that span; the
        // pages just outside it stay non-owned (P-Map-002).
        let m = meta(256 * 1024);
        let pm = PageMap::new();
        let base = 0x4000_0000;
        let s = small_span(1, base, 2, 64); // a 2-page span
        pm.install_span(&m, &s).unwrap();

        let (lo, hi) = s.range();
        // Every address inside the span resolves to it.
        for addr in [lo, lo + 1, lo + PAGE_SIZE, hi - 1] {
            let sp = pm.lookup(addr).span_ptr().expect("owned page");
            assert_eq!(sp, &s as *const SpanDescriptor);
        }
        // The page just before and just after are non-owned.
        assert!(pm.lookup(lo - 1).is_empty());
        assert!(pm.lookup(hi).is_empty());
    }

    #[test]
    fn release_then_retire_transitions_entries() {
        // P-Map-005 then P-Map-002: Active → Released (retained) → Empty.
        let m = meta(256 * 1024);
        let pm = PageMap::new();
        let s = small_span(1, 0x4000_0000, 1, 64);
        pm.install_span(&m, &s).unwrap();
        let addr = s.base();
        assert!(matches!(pm.lookup(addr), PageEntry::Small(_)));

        // Mark the descriptor Released first (W3-6 ordering), then the pagemap.
        s.set_state(SpanState::Released);
        pm.release_span(&s);
        match pm.lookup(addr) {
            PageEntry::Released(p) => assert_eq!(p, &s as *const SpanDescriptor),
            other => panic!("expected Released, got {other:?}"),
        }

        // Retire hands the pages back: non-owned again.
        pm.retire_span(&s);
        assert!(pm.lookup(addr).is_empty());
    }

    #[test]
    fn two_spans_map_their_own_pages_only() {
        // P-Map-001: distinct spans own distinct pages; no page maps to two
        // descriptors. Place them in adjacent leaves and in the same leaf.
        let m = meta(512 * 1024);
        let pm = PageMap::new();
        let a = small_span(1, 0x4000_0000, 1, 64);
        let b = small_span(2, 0x4000_0000 + PAGE_SIZE, 1, 64); // adjacent page, same leaf
        let c = small_span(3, 0x8000_0000, 1, 64); // far away, different mid/leaf
        pm.install_span(&m, &a).unwrap();
        pm.install_span(&m, &b).unwrap();
        pm.install_span(&m, &c).unwrap();

        assert_eq!(
            pm.lookup(a.base()).span_ptr().unwrap(),
            &a as *const SpanDescriptor
        );
        assert_eq!(
            pm.lookup(b.base()).span_ptr().unwrap(),
            &b as *const SpanDescriptor
        );
        assert_eq!(
            pm.lookup(c.base()).span_ptr().unwrap(),
            &c as *const SpanDescriptor
        );
    }

    #[test]
    fn large_install_and_retire() {
        let m = meta(512 * 1024);
        let pm = PageMap::new();
        // A 3 MiB large allocation spanning many pages.
        let d = LargeDescriptor::new(LargeId(1), ArenaId::DEFAULT, 0x1_0000_0000, 3_000_000, 4096);
        pm.install_large(&m, &d).unwrap();
        let lp = &d as *const LargeDescriptor;
        assert_eq!(pm.lookup(d.base()).large_ptr().unwrap(), lp);
        assert_eq!(pm.lookup(d.end() - 1).large_ptr().unwrap(), lp);
        // The pagemap maps whole pages: `d.end()` lands in the last (partially
        // used) owned page, so it is still owned. The first page *after* the
        // allocation is non-owned.
        assert!(!pm.lookup(d.end()).is_empty());
        let first_unowned = (page_of(d.end() - 1) + 1) << PAGE_SHIFT;
        assert!(pm.lookup(first_unowned).is_empty());

        pm.retire_large(&d);
        assert!(pm.lookup(d.base()).is_empty());
    }

    #[test]
    fn install_out_of_metadata_fails_safely() {
        // §17.4 safe failure: a too-small metadata region makes install return
        // OutOfMetadata rather than panicking, and the pagemap stays consistent
        // (the pages it could not map remain non-owned).
        let m = meta(8 * 1024); // smaller than even one interior node (16 KiB)
        let pm = PageMap::new();
        let s = small_span(1, 0x4000_0000, 1, 64);
        assert_eq!(pm.install_span(&m, &s), Err(PagemapError::OutOfMetadata));
        assert!(pm.lookup(s.base()).is_empty());
    }

    #[test]
    fn concurrent_install_and_lookup_never_tears() {
        // W3-3c: installer threads race to create the *shared* leaf node (the
        // lock-free `child_or_create` CAS) and publish distinct entries, while
        // reader threads classify the same pages. A reader must only ever see
        // `Empty` (not yet installed) or the fully-formed span — never a half-built
        // node or a wrong descriptor. After the join, every span resolves to itself
        // (P-Map-001).
        const N: usize = 64;
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        // All N spans live in one leaf (adjacent pages), so installs contend on the
        // same radix nodes. Descriptors are fixed in place (no realloc) so the raw
        // pointers the pagemap stores stay valid.
        let spans: Vec<SpanDescriptor> = (0..N)
            .map(|i| small_span(i as u32, 0x4000_0000 + i * PAGE_SIZE, 1, 64))
            .collect();

        std::thread::scope(|s| {
            // Installers: each thread installs every span (idempotent races on the
            // same entries and nodes — the worst case for the publish protocol).
            for _ in 0..4 {
                s.spawn(|| {
                    for span in &spans {
                        // An install may transiently fail only on OOM; the arena is
                        // sized generously, so it succeeds.
                        pm.install_span(&m, span).unwrap();
                    }
                });
            }
            // Readers: classify the pages concurrently; any non-empty result must be
            // the correct span for that page.
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..50 {
                        for (i, span) in spans.iter().enumerate() {
                            if let Some(p) = pm.lookup(span.base()).span_ptr() {
                                assert_eq!(p, &spans[i] as *const SpanDescriptor);
                            }
                        }
                    }
                });
            }
        });

        // After all installs, every span is mapped to exactly itself.
        for (i, span) in spans.iter().enumerate() {
            assert_eq!(
                pm.lookup(span.base()).span_ptr().unwrap(),
                &spans[i] as *const SpanDescriptor
            );
        }
    }

    #[test]
    fn page_range_is_inclusive_and_empty_safe() {
        assert_eq!(page_range(0, 0).count(), 0);
        assert_eq!(page_range(PAGE_SIZE, PAGE_SIZE).count(), 0);
        assert_eq!(page_range(0, 1).count(), 1);
        assert_eq!(page_range(0, PAGE_SIZE).count(), 1);
        assert_eq!(page_range(0, PAGE_SIZE + 1).count(), 2);
    }
}

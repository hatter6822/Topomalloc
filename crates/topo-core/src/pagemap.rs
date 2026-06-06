// SPDX-License-Identifier: MIT
//! The pagemap: address → descriptor, concurrently (§17.1/§17.2, DD-1, plan 03
//! W3-3 + W3-6).
//!
//! Every unsized `free`, `realloc`, `usable_size`, and debug check must map an
//! arbitrary address to the descriptor that owns it (§17.1) in O(1), correctly
//! classifying interior/foreign/released pointers, **while spans are concurrently
//! created and recycled**. The lookup is on the free slow path; the update races
//! span lifecycle.
//!
//! **Structure (W3-3a).** A fixed-fan-out, multi-level radix over allocator-page
//! numbers (DD-1 choice (c)): O(1) worst-case, lazily populated, no resize, and
//! interior-pointer lookup by masking an address down to its page. A flat array
//! would waste virtual space on 64-bit; a hash map has worst-case and resize
//! hazards. The depth is **derived** from the page size and `usize::BITS`, so the
//! radix covers the **entire address space** with no virtual-address-width
//! assumption (an address from 5-level paging / a 57-bit VA maps as readily as a
//! 48-bit one). Nodes are a uniform 8 KiB (1024 slots) and lazily allocated on
//! first touch from the [`MetadataAlloc`] seam (bootstrap metadata now, the normal
//! metadata allocator after hand-off, §17.4) — small nodes keep the resident
//! footprint low (the `low-rss` profile's interest). The cost is a few extra
//! dependent loads on the *slow* path (5 levels on a 64-bit/16 KiB-page target),
//! which the free fast path never pays.
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
//! / [`install_large`](PageMap::install_large), never poking a leaf directly. The
//! mutators are lock-free (atomic publish), so they acquire no lock and cannot
//! violate the §27.2 hierarchy; they run inside the caller's span critical section.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::PAGE_SIZE;
use crate::span::{LargeDescriptor, SpanDescriptor, SpanState};

// --- radix geometry (D4: derived from the page size and the target word) -------

/// `log2(PAGE_SIZE)` — the page-offset bit width (14 for a 16 KiB page).
pub const PAGE_SHIFT: u32 = PAGE_SIZE.trailing_zeros();

/// Width of an allocator-page number: `usize::BITS - PAGE_SHIFT` (50 on a 64-bit
/// target, 18 on a 32-bit one). The radix covers all of it, so **every** address
/// is classifiable — there is no virtual-address-width assumption.
pub const PAGENO_BITS: u32 = usize::BITS - PAGE_SHIFT;

/// Bits of the page number resolved per radix level (the fan-out is `2^RADIX_BITS`
/// = 1024 slots, an 8 KiB node on a 64-bit target). Chosen to keep nodes small for
/// lazy population and the `low-rss` profile.
pub const RADIX_BITS: u32 = 10;

/// Radix depth: `ceil(PAGENO_BITS / RADIX_BITS)` (5 on 64-bit, 2 on 32-bit). At
/// least two so the root is always an interior node distinct from the leaf.
pub const LEVELS: u32 = PAGENO_BITS.div_ceil(RADIX_BITS);
const _: () = assert!(
    LEVELS >= 2,
    "the radix needs a root interior level and a leaf level"
);
const _: () = assert!(
    LEVELS * RADIX_BITS >= PAGENO_BITS,
    "the radix must cover the whole page-number range"
);

/// Slots per radix node (interior children or leaf entries).
const SLOTS: usize = 1 << RADIX_BITS;
const SLOT_MASK: usize = SLOTS - 1;

/// One interior radix node: a lazily-populated array of child pointers. Children
/// are type-erased `*mut u8` and re-typed by level; `null` means "not populated".
#[repr(C)]
struct Interior {
    slots: [AtomicPtr<u8>; SLOTS],
}

/// One leaf radix node: a [`PageEntry`]-encoded word per allocator page. The
/// all-zero state is every-page-`Empty`, so a freshly zeroed leaf is valid.
#[repr(C)]
struct Leaf {
    entries: [AtomicUsize; SLOTS],
}

const INTERIOR_SIZE: usize = core::mem::size_of::<Interior>();
const INTERIOR_ALIGN: usize = core::mem::align_of::<Interior>();
const LEAF_SIZE: usize = core::mem::size_of::<Leaf>();
const LEAF_ALIGN: usize = core::mem::align_of::<Leaf>();

// --- entry encoding (W3-3b) -------------------------------------------------

/// Low-bit tag mask. Descriptor pointers are ≥ 8-aligned, so three low bits are
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
    /// Total bytes of radix nodes allocated (the pagemap's metadata overhead, for
    /// the Appendix-D `metadata.bytes` stat and the W3-3a "overhead … documented"
    /// acceptance). Counts every node this pagemap caused to be allocated, including
    /// the rare node a lost lazy-creation CAS leaks.
    node_bytes: AtomicUsize,
}

impl PageMap {
    /// An empty pagemap (no root yet). Every lookup returns `Empty` until a span is
    /// installed.
    pub const fn new() -> Self {
        Self {
            root: AtomicPtr::new(ptr::null_mut()),
            node_bytes: AtomicUsize::new(0),
        }
    }

    /// Total bytes of radix nodes this pagemap has allocated (its metadata
    /// overhead). Bounded by the touched address range (W3-3a).
    #[inline]
    pub fn metadata_bytes(&self) -> usize {
        self.node_bytes.load(Ordering::Relaxed)
    }

    // --- lookup (slow path, allocation-free) --------------------------------

    /// Look up the page containing `addr` and return its [`PageEntry`] (§17.1).
    /// O(1): a fixed number of array indexes with acquire loads. Never allocates;
    /// an unmapped path returns `Empty` (P-Map-002). Total over the whole address
    /// space — there is no out-of-range address.
    ///
    /// Interior-pointer support: the lookup masks `addr` to its page, so any
    /// address *within* an owned page resolves to that page's descriptor — the
    /// caller (pointer classification, W3-4) decides base-vs-interior.
    #[inline]
    pub fn lookup(&self, addr: usize) -> PageEntry {
        match self.find_leaf(page_of(addr)) {
            Some(leaf) => {
                // SAFETY: `find_leaf` returned a valid, published `Leaf`.
                let bits = unsafe { (*leaf).entries[leaf_index(addr)].load(Ordering::Acquire) };
                PageEntry::decode(bits)
            }
            None => PageEntry::Empty,
        }
    }

    // --- the single mutator path (W3-6 / P-Map-006) -------------------------

    /// Install a span into the pagemap (P-Map-001/003): map every page the span
    /// covers to its descriptor with a **release store**, so a concurrent
    /// classifier that acquire-loads an entry sees the fully-initialized span. The
    /// caller MUST have finished initializing `span` before calling — the descriptor
    /// is the publish payload (W3-6).
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
        let (base, stop) = span.range();
        // Spans are page-aligned page runs, so every mapped page lies wholly inside
        // the span — the precondition the §17.2 soundness theorem assumes.
        debug_assert_eq!(base % PAGE_SIZE, 0, "span base must be page-aligned");
        let entry = PageEntry::Small(span as *const SpanDescriptor).encode();
        self.publish_range(meta, (base, stop), entry)
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
    /// pages cannot be reused without recommit. The caller MUST set the span's state
    /// to [`SpanState::Released`] *before* calling (§17.2 P-Map-006). Allocation-free.
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
    /// back, P-Map-002). Allocation-free. After this a classifier sees `Empty`.
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
            // SAFETY: `leaf_or_create` returned a valid, published `Leaf`.
            let slot = unsafe { &(*leaf).entries[p & SLOT_MASK] };
            // P-Map-001: a page maps to exactly one descriptor. An install over a
            // page already owned by a *different* descriptor is a bug (a fresh page
            // is `Empty`; a re-install of the same span publishes the same word).
            debug_assert!(
                {
                    let prior = slot.load(Ordering::Acquire);
                    prior == TAG_EMPTY || prior == entry
                },
                "P-Map-001: page already owned by a different descriptor"
            );
            // The release store publishes `entry` so an acquire-loading reader sees
            // the descriptor it encodes fully initialized (W3-3c).
            slot.store(entry, Ordering::Release);
        }
        Ok(())
    }

    /// Overwrite every *already-mapped* page in `[base, stop)` with `entry`. Used by
    /// release/retire, which never need to create nodes (the pages were mapped at
    /// install). A page whose node is absent is skipped (already non-owned).
    fn overwrite_existing_range(&self, (base, stop): (usize, usize), entry: usize) {
        for p in page_range(base, stop) {
            if let Some(leaf) = self.find_leaf(p) {
                // SAFETY: `find_leaf` returned a valid, published `Leaf`.
                unsafe { (*leaf).entries[p & SLOT_MASK].store(entry, Ordering::Release) };
            }
        }
    }

    /// The root, allocating + publishing it on first use.
    fn root_or_create(&self, meta: &dyn MetadataAlloc) -> Result<*mut Interior, PagemapError> {
        let cur = self.root.load(Ordering::Acquire);
        if !cur.is_null() {
            return Ok(cur);
        }
        let node = self
            .alloc_node(meta, INTERIOR_SIZE, INTERIOR_ALIGN)?
            .cast::<Interior>();
        match self.root.compare_exchange(
            ptr::null_mut(),
            node,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(node),
            // Lost the race: another thread published a root first. Use it; our node
            // leaks (monotonic metadata — never freed, a rare bounded waste).
            Err(winner) => Ok(winner),
        }
    }

    /// Walk to the leaf for page `p`, creating the root/interior/leaf nodes as
    /// needed. The walk descends from the root (top page-number group) to the leaf.
    fn leaf_or_create(
        &self,
        meta: &dyn MetadataAlloc,
        p: usize,
    ) -> Result<*mut Leaf, PagemapError> {
        let mut node = self.root_or_create(meta)?;
        // Follow children for groups LEVELS-1 .. 1; the group-1 child is the leaf.
        for k in (1..LEVELS).rev() {
            let idx = (p >> (k * RADIX_BITS)) & SLOT_MASK;
            // SAFETY: `node` is a valid published `Interior`.
            let slot = unsafe { &(*node).slots[idx] };
            if k == 1 {
                let leaf = self.child_or_create(slot, meta, LEAF_SIZE, LEAF_ALIGN)?;
                return Ok(leaf.cast::<Leaf>());
            }
            let child = self.child_or_create(slot, meta, INTERIOR_SIZE, INTERIOR_ALIGN)?;
            node = child.cast::<Interior>();
        }
        // Unreachable: LEVELS >= 2 guarantees the `k == 1` arm returns.
        unreachable!("radix has at least two levels")
    }

    /// Walk to the leaf for page `p` without allocating; `None` if any node on the
    /// path is absent (the page is non-owned).
    #[inline]
    fn find_leaf(&self, p: usize) -> Option<*mut Leaf> {
        let mut node = self.root.load(Ordering::Acquire);
        if node.is_null() {
            return None;
        }
        for k in (1..LEVELS).rev() {
            let idx = (p >> (k * RADIX_BITS)) & SLOT_MASK;
            // SAFETY: `node` is a valid published `Interior`.
            let child = unsafe { (*node).slots[idx].load(Ordering::Acquire) };
            if child.is_null() {
                return None;
            }
            if k == 1 {
                return Some(child.cast::<Leaf>());
            }
            node = child.cast::<Interior>();
        }
        unreachable!("radix has at least two levels")
    }

    /// Return the child in `slot`, creating it with a `size`/`align` node on first
    /// touch. Lock-free: the creator zeroes the node, then publishes it with a
    /// release CAS; a reader acquire-loads. If the CAS loses, the winner's node is
    /// used and ours leaks (bounded, rare — monotonic metadata is never freed).
    fn child_or_create(
        &self,
        slot: &AtomicPtr<u8>,
        meta: &dyn MetadataAlloc,
        size: usize,
        align: usize,
    ) -> Result<*mut u8, PagemapError> {
        let cur = slot.load(Ordering::Acquire);
        if !cur.is_null() {
            return Ok(cur);
        }
        let node = self.alloc_node(meta, size, align)?;
        match slot.compare_exchange(ptr::null_mut(), node, Ordering::Release, Ordering::Acquire) {
            Ok(_) => Ok(node),
            Err(winner) => Ok(winner),
        }
    }

    /// Allocate `size` bytes aligned `align` from `meta`, zero them so the result is
    /// a valid all-`null`/all-`Empty` node *before* it is linked (init-before-publish,
    /// failure mode F1), and account the bytes (W3-3a overhead metric).
    fn alloc_node(
        &self,
        meta: &dyn MetadataAlloc,
        size: usize,
        align: usize,
    ) -> Result<*mut u8, PagemapError> {
        let node = meta.alloc(size, align).ok_or(PagemapError::OutOfMetadata)?;
        // SAFETY: `meta.alloc` returned a fresh, exclusively-owned region of exactly
        // `size` bytes; zeroing it makes a valid node whose every atomic slot is the
        // zero (null / `Empty`) bit pattern. No other thread can observe it yet — it
        // is not linked into the radix until the caller's release CAS/store.
        unsafe { ptr::write_bytes(node.as_ptr(), 0, size) };
        self.node_bytes.fetch_add(size, Ordering::Relaxed);
        Ok(node.as_ptr())
    }
}

impl Default for PageMap {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: all shared state is reached through atomics (the root pointer, every
// interior slot, every leaf entry, the byte counter); nodes are zeroed before being
// published with a release CAS/store and never freed, and the descriptors they
// point at are `Sync`. Concurrent lookups and the single mutator path are therefore
// data-race-free.
unsafe impl Sync for PageMap {}
// SAFETY: as the `Sync` impl above — the pagemap owns only atomics and pointers
// into `Sync` metadata, so it is sound to move across threads.
unsafe impl Send for PageMap {}

// --- page-number arithmetic -------------------------------------------------

/// The allocator-page number containing `addr` (`< 2^PAGENO_BITS` by construction).
#[inline]
fn page_of(addr: usize) -> usize {
    addr >> PAGE_SHIFT
}

/// The leaf-entry index for `addr` (its low page-number group).
#[inline]
fn leaf_index(addr: usize) -> usize {
    page_of(addr) & SLOT_MASK
}

/// The inclusive range of page numbers covering `[base, stop)`. Empty if
/// `stop <= base`.
#[inline]
fn page_range(base: usize, stop: usize) -> core::ops::RangeInclusive<usize> {
    if stop <= base {
        #[allow(clippy::reversed_empty_ranges)]
        return 1..=0; // an empty byte range covers no pages
    }
    page_of(base)..=page_of(stop - 1)
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

    fn small_span(id: u32, base: usize, pages: u32, objects: u32, m: &BumpArena) -> SpanDescriptor {
        SpanDescriptor::new(
            SpanId(id),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            base,
            pages,
            objects,
            0,
            m,
        )
        .expect("bitmap")
    }

    #[test]
    fn geometry_consts_are_consistent() {
        assert_eq!(PAGE_SHIFT, 14);
        assert_eq!(PAGENO_BITS, usize::BITS - 14);
        assert_eq!(LEVELS, PAGENO_BITS.div_ceil(RADIX_BITS));
        // Full coverage and `LEVELS >= 2` are enforced at compile time (above); the
        // node geometry: uniform `usize`-sized slots (8 KiB nodes on a 64-bit target).
        assert_eq!(
            core::mem::size_of::<Interior>(),
            SLOTS * core::mem::size_of::<usize>()
        );
        assert_eq!(
            core::mem::size_of::<Leaf>(),
            SLOTS * core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn radix_index_decomposition_is_lossless() {
        // The radix's correctness rests on the per-level index extraction being a
        // *bijection* over `[0, 2^PAGENO_BITS)`: distinct page numbers must take
        // distinct paths, or two pages would alias one leaf entry. Reconstruct each
        // page number from its level indices and check equality — for every
        // single-bit value (covering each bit position) plus representative
        // multi-bit values. (Exhaustive over 2^50 is infeasible; single-bit coverage
        // proves no bit is dropped or duplicated by the shifts/masks.)
        let level_index = |p: usize, k: u32| (p >> (k * RADIX_BITS)) & SLOT_MASK;
        let reconstruct = |p: usize| -> usize {
            (0..LEVELS).fold(0usize, |acc, k| {
                acc | (level_index(p, k) << (k * RADIX_BITS))
            })
        };
        let max_p = (1u128 << PAGENO_BITS) - 1;
        let mut probes: Vec<usize> = (0..PAGENO_BITS).map(|s| 1usize << s).collect();
        probes.extend([
            0,
            1,
            (max_p) as usize,
            (max_p / 2) as usize,
            (max_p / 3) as usize,
            0xa5a5_a5a5 & max_p as usize,
        ]);
        for p in probes {
            assert!(p as u128 <= max_p);
            assert_eq!(
                reconstruct(p),
                p,
                "radix decomposition lost bits for p={p:#x}"
            );
            // Each level index is in bounds (no out-of-range node access).
            for k in 0..LEVELS {
                assert!(level_index(p, k) < SLOTS);
            }
        }
    }

    #[test]
    fn empty_pagemap_classifies_everything_external() {
        // P-Map-002: with nothing installed (no root), every address is non-owned —
        // including the very top of the address space (no VA cap).
        let pm = PageMap::new();
        assert!(pm.lookup(0).is_empty());
        assert!(pm.lookup(0x4000_0000).is_empty());
        assert!(pm.lookup(usize::MAX).is_empty());
        assert_eq!(pm.metadata_bytes(), 0);
    }

    #[test]
    fn entry_encoding_round_trips() {
        let m = meta(64 * 1024);
        let s = small_span(1, 0x4000_0000, 1, 64, &m);
        let sp = &s as *const SpanDescriptor;
        for e in [
            PageEntry::Empty,
            PageEntry::Small(sp),
            PageEntry::Released(sp),
        ] {
            match (e, PageEntry::decode(e.encode())) {
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
        let m = meta(256 * 1024);
        let pm = PageMap::new();
        let base = 0x4000_0000;
        let s = small_span(1, base, 2, 64, &m); // a 2-page span
        pm.install_span(&m, &s).unwrap();

        let (lo, hi) = s.range();
        for addr in [lo, lo + 1, lo + PAGE_SIZE, hi - 1] {
            let sp = pm.lookup(addr).span_ptr().expect("owned page");
            assert_eq!(sp, &s as *const SpanDescriptor);
        }
        assert!(pm.lookup(lo - 1).is_empty());
        assert!(pm.lookup(hi).is_empty());
        // The pagemap allocated some node bytes (overhead metric, W3-3a).
        assert!(pm.metadata_bytes() > 0);
    }

    #[test]
    fn high_address_space_is_mappable_no_va_cap() {
        // The full-range radix maps an address far above a 48-bit VA (e.g. a 5-level
        // paging / 57-bit-VA host), which the old 48-bit-capped design mis-classified
        // as foreign.
        let m = meta(512 * 1024);
        let pm = PageMap::new();
        let high = 1usize << 52; // well beyond 2^48
        let s = small_span(1, high, 1, 64, &m);
        pm.install_span(&m, &s).unwrap();
        assert_eq!(
            pm.lookup(high).span_ptr().unwrap(),
            &s as *const SpanDescriptor
        );
        assert!(pm.lookup(high + PAGE_SIZE).is_empty());
    }

    #[test]
    fn release_then_retire_transitions_entries() {
        let m = meta(256 * 1024);
        let pm = PageMap::new();
        let s = small_span(1, 0x4000_0000, 1, 64, &m);
        pm.install_span(&m, &s).unwrap();
        let addr = s.base();
        assert!(matches!(pm.lookup(addr), PageEntry::Small(_)));

        s.set_state(SpanState::Released);
        pm.release_span(&s);
        match pm.lookup(addr) {
            PageEntry::Released(p) => assert_eq!(p, &s as *const SpanDescriptor),
            other => panic!("expected Released, got {other:?}"),
        }

        pm.retire_span(&s);
        assert!(pm.lookup(addr).is_empty());
    }

    #[test]
    fn two_spans_map_their_own_pages_only() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_span(1, 0x4000_0000, 1, 64, &m);
        let b = small_span(2, 0x4000_0000 + PAGE_SIZE, 1, 64, &m); // adjacent page
        let c = small_span(3, 0x8000_0000, 1, 64, &m); // far away
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
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let d = LargeDescriptor::new(LargeId(1), ArenaId::DEFAULT, 0x1_0000_0000, 3_000_000, 4096);
        pm.install_large(&m, &d).unwrap();
        let lp = &d as *const LargeDescriptor;
        assert_eq!(pm.lookup(d.base()).large_ptr().unwrap(), lp);
        assert_eq!(pm.lookup(d.end() - 1).large_ptr().unwrap(), lp);
        // The pagemap maps whole pages: `d.end()` lands in the last (partially used)
        // owned page; the first page after the allocation is non-owned.
        assert!(!pm.lookup(d.end()).is_empty());
        let first_unowned = (page_of(d.end() - 1) + 1) << PAGE_SHIFT;
        assert!(pm.lookup(first_unowned).is_empty());

        pm.retire_large(&d);
        assert!(pm.lookup(d.base()).is_empty());
    }

    #[test]
    fn install_out_of_metadata_fails_safely() {
        // §17.4 safe failure: a too-small metadata region makes install return
        // OutOfMetadata rather than panicking, and the pagemap stays consistent.
        let m = meta(4 * 1024); // smaller than even one interior node (8 KiB)
        let pm = PageMap::new();
        let big = meta(64 * 1024); // build the descriptor (and its bitmap) elsewhere
        let s = small_span(1, 0x4000_0000, 1, 64, &big);
        assert_eq!(pm.install_span(&m, &s), Err(PagemapError::OutOfMetadata));
        assert!(pm.lookup(s.base()).is_empty());
    }

    #[test]
    fn concurrent_install_and_lookup_never_tears() {
        // W3-3c: installer threads race to create shared radix nodes (the lock-free
        // `child_or_create` CAS) and publish distinct entries, while reader threads
        // classify the same pages. A reader sees only `Empty` or the fully-formed
        // span — never a half-built node or a wrong descriptor.
        const N: usize = 64;
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let spans: Vec<SpanDescriptor> = (0..N)
            .map(|i| small_span(i as u32, 0x4000_0000 + i * PAGE_SIZE, 1, 64, &m))
            .collect();

        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for span in &spans {
                        pm.install_span(&m, span).unwrap();
                    }
                });
            }
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

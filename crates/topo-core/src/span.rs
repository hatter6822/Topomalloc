// SPDX-License-Identifier: MIT
//! Span and large-allocation descriptors (§16.2 / §17.2, plan 03 W3-2 + W3-5).
//!
//! A [`SpanDescriptor`] is the metadata record for one span — a contiguous page
//! run carved into equal-size small objects (§16.1). It carries the §16.2 fields
//! the rest of the allocator reads, and — crucially — enough of them to **derive
//! the §16.4 conservation law** that empty-span detection (§16.5, the hardest
//! accounting in the allocator) rests on:
//!
//! ```text
//! object_count = live + local_cached + transfer_cached + central_free + quarantined
//! central_free = popcount(free_bitmap)            -- authoritative central residency
//! ```
//!
//! The `free_bitmap` is authoritative only for **central-list residency**: an
//! object sitting in a per-CPU/thread/transfer cache is free yet has `bit(i) = 0`,
//! exactly like a live object (§8.5). The cached terms are therefore *logical*
//! quantities the descriptor does not track per-op; they are reconstructed in
//! debug (W5-3c) and are trivially zero before caches exist (M1), so this module
//! checks the central-only law exactly and the full law against caller-supplied
//! reconstructed terms.
//!
//! **§8.5 single critical section (per-span lock).** The bitmap and its cached
//! `central_free_count` MUST move together. The descriptor owns a lightweight
//! **span lock** (§27.2's "Span lock"); the *only* way to mutate the accounting is
//! through the [`SpanGuard`] returned by [`SpanDescriptor::lock`], so the
//! `central_free == popcount` invariant is updated atomically as a pair and can
//! never be observed torn by another lock-holder. (Lock-free *approximate* reads of
//! the cached count remain available for stats.)
//!
//! **Hybrid bitmap (footprint).** Small slabs keep the bitmap **inline**
//! ([`INLINE_BITS`] objects); larger slabs put it **out-of-line**, sized to the
//! class, from the [`MetadataAlloc`] seam — so a descriptor stays compact whatever
//! the class, and only the few high-count classes pay an extra (tiny) allocation.
//!
//! **Concurrency (W3-3c/W3-5).** A descriptor is published into the pagemap by a
//! release-store and reached by concurrent classifiers through a raw pointer
//! (§17.5). Descriptors are **never freed** — they live in monotonic metadata and
//! are *recycled in place* with a [`Generation`] bump (§27.5) — so a classifier's
//! pointer is always dereferenceable; the generation flags a logical reuse. Every
//! geometry field a classifier may read concurrently is atomic, so even a read
//! racing a recycle is well-defined and the generation guard ([`GenGuard`])
//! detects the identity change.

use core::ptr::{self, NonNull};
use core::sync::atomic::{
    AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::{PAGE_SIZE, SIZE_CLASSES};
use crate::ids::{ArenaId, Generation, LargeId, SizeClassId, SpanId};
use crate::lock::{LockRank, RankedLock};
use crate::overflow::align_up;
use crate::size_class;
use crate::slab::SlabLayout;

/// Largest `objects_per_slab` over every size class in the generated table — the
/// number of bits the widest slab's bitmap must cover. Computed from the table (a
/// `const` scan) so it cannot drift from the shipped size classes (DD-1).
pub(crate) const fn max_objects_per_slab() -> usize {
    let mut max = 0usize;
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        let o = SIZE_CLASSES[i].objects_per_slab as usize;
        if o > max {
            max = o;
        }
        i += 1;
    }
    max
}

/// Words of inline bitmap storage carried in every descriptor. Two `u64`s cover
/// 128 objects inline; in the shipped table only the seven highest-count classes
/// (sizes `< 128 B`) exceed this and spill out-of-line.
const INLINE_WORDS: usize = 2;

/// Objects addressable by the inline bitmap (`INLINE_WORDS * 64`). At/under this
/// the bitmap is inline; above it the bitmap is allocated out-of-line.
pub const INLINE_BITS: usize = INLINE_WORDS * 64;

/// Maximum bitmap words any class can need (`ceil(max_objects_per_slab / 64)`),
/// the largest out-of-line block.
pub const MAX_BITMAP_WORDS: usize = max_objects_per_slab().div_ceil(64);

/// Allocate `words` zeroed `AtomicU64`s of bitmap storage from `meta`, or `None`
/// on exhaustion. Zeroing makes a valid all-clear bitmap.
fn alloc_bitmap_words(meta: &dyn MetadataAlloc, words: usize) -> Option<NonNull<AtomicU64>> {
    let bytes = words.checked_mul(core::mem::size_of::<AtomicU64>())?;
    let mem = meta.alloc(bytes, core::mem::align_of::<AtomicU64>())?;
    // SAFETY: `mem` is a fresh, exclusively-owned, 8-aligned region of `bytes`
    // bytes; zeroing yields `words` valid `AtomicU64(0)`s (an all-clear bitmap).
    unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
    Some(mem.cast::<AtomicU64>())
}

/// The §16.4 free bitmap: `bit(i) = 1` ⟺ object `i` is **resident in this span's
/// central free list**. Storage is **hybrid** — inline for slabs of at most
/// [`INLINE_BITS`] objects, else an out-of-line, class-sized block from the
/// metadata seam (W3-2).
///
/// The bitmap is *unsynchronized on its own*; the owning [`SpanDescriptor`]'s span
/// lock makes a bitmap edit and the cached count one critical section (§8.5), so
/// these primitives are reachable only through a held [`SpanGuard`].
#[repr(C)]
pub struct FreeBitmap {
    /// Inline storage, used when `out_of_line` is null.
    inline: [AtomicU64; INLINE_WORDS],
    /// Out-of-line storage (null ⇒ inline). Points at `cap_words` `AtomicU64`s in
    /// monotonic metadata (never freed).
    out_of_line: AtomicPtrU64,
    /// Active word count (`ceil(object_count / 64)`); the bitmap addresses
    /// `words * 64` objects.
    words: AtomicU32,
    /// Allocated capacity in words (`INLINE_WORDS` while inline, else the
    /// out-of-line block size). A recycle grows the block only when it must.
    cap_words: AtomicU32,
}

/// Newtype around `AtomicPtr<AtomicU64>` so the file reads cleanly.
type AtomicPtrU64 = core::sync::atomic::AtomicPtr<AtomicU64>;

impl FreeBitmap {
    /// Build a bitmap sized for `object_count` objects (inline or out-of-line via
    /// `meta`). `None` if an out-of-line block is needed but cannot be allocated.
    fn new_for(object_count: u32, meta: &dyn MetadataAlloc) -> Option<FreeBitmap> {
        let words = (object_count as usize).div_ceil(64).max(1);
        let (out_of_line, cap) = if words <= INLINE_WORDS {
            (ptr::null_mut(), INLINE_WORDS)
        } else {
            (alloc_bitmap_words(meta, words)?.as_ptr(), words)
        };
        Some(FreeBitmap {
            inline: [const { AtomicU64::new(0) }; INLINE_WORDS],
            out_of_line: AtomicPtrU64::new(out_of_line),
            words: AtomicU32::new(words as u32),
            cap_words: AtomicU32::new(cap as u32),
        })
    }

    /// Re-point this bitmap at a slab of `object_count` objects, growing the
    /// out-of-line block only if the new size exceeds the current capacity (the old
    /// block then leaks — monotonic metadata is never freed). Clears all bits.
    /// `false` on a needed-but-failed growth (safe failure). Called under the span
    /// lock during recycle.
    fn recycle_to(&self, object_count: u32, meta: &dyn MetadataAlloc) -> bool {
        let new_words = (object_count as usize).div_ceil(64).max(1);
        if new_words > self.cap_words.load(Ordering::Acquire) as usize {
            let block = match alloc_bitmap_words(meta, new_words) {
                Some(b) => b,
                None => return false,
            };
            self.out_of_line.store(block.as_ptr(), Ordering::Release);
            self.cap_words.store(new_words as u32, Ordering::Release);
        }
        self.words.store(new_words as u32, Ordering::Release);
        self.clear();
        true
    }

    /// Number of active words.
    #[inline]
    fn active_words(&self) -> usize {
        self.words.load(Ordering::Acquire) as usize
    }

    /// Number of object slots the bitmap currently addresses.
    #[inline]
    pub fn capacity_bits(&self) -> usize {
        self.active_words() * 64
    }

    /// `&AtomicU64` for word `w` (`w < active_words()`), inline or out-of-line.
    #[inline]
    fn word(&self, w: usize) -> &AtomicU64 {
        let oo = self.out_of_line.load(Ordering::Acquire);
        if oo.is_null() {
            &self.inline[w]
        } else {
            // SAFETY: `oo` points at `cap_words` `AtomicU64`s in metadata (never
            // freed), and `w < active_words() <= cap_words`.
            unsafe { &*oo.add(w) }
        }
    }

    /// Mark object `i` resident in the central list. `true` iff newly set — a
    /// `false` is a **double insert** (the bitmap face of a double-free, plan 08
    /// W18-2).
    #[inline]
    pub fn insert(&self, i: usize) -> bool {
        debug_assert!(i < self.capacity_bits(), "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        self.word(w).fetch_or(bit, Ordering::Relaxed) & bit == 0
    }

    /// Clear object `i` from the central list. `true` iff it was set.
    #[inline]
    pub fn remove(&self, i: usize) -> bool {
        debug_assert!(i < self.capacity_bits(), "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        self.word(w).fetch_and(!bit, Ordering::Relaxed) & bit != 0
    }

    /// Whether object `i` is central-resident.
    #[inline]
    pub fn contains(&self, i: usize) -> bool {
        debug_assert!(i < self.capacity_bits(), "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        self.word(w).load(Ordering::Relaxed) & bit != 0
    }

    /// Number of central-resident objects (`popcount`). The value
    /// `central_free_count` must equal (§16.4).
    #[inline]
    pub fn count(&self) -> usize {
        (0..self.active_words())
            .map(|w| self.word(w).load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    /// Set objects `[0, n)` resident, clear the rest — the bitmap half of activating
    /// a freshly carved slab. `n <= capacity_bits()`.
    #[inline]
    pub fn fill_below(&self, n: usize) {
        debug_assert!(n <= self.capacity_bits(), "fill count out of bitmap range");
        for w in 0..self.active_words() {
            let lo = w * 64;
            let val = if n >= lo + 64 {
                u64::MAX
            } else if n <= lo {
                0
            } else {
                (1u64 << (n - lo)) - 1
            };
            self.word(w).store(val, Ordering::Relaxed);
        }
    }

    /// Clear every active bit.
    #[inline]
    fn clear(&self) {
        for w in 0..self.active_words() {
            self.word(w).store(0, Ordering::Relaxed);
        }
    }
}

/// The per-span spinlock (§27.2 "Span lock", rank [`LockRank::SPAN`]). The
/// critical section is a couple of atomic edits; it is the innermost lock in the
/// central data path (a central bin and the span descriptor pool both take it
/// while they are held), so a ranked test-and-test-and-set spinlock — the single
/// [`RankedLock`] primitive, routed through the W16-1b lock-order checker — is
/// the right tool.
type SpanLock = RankedLock<{ LockRank::SPAN }>;

/// Lifecycle state of a span (§7.3, what classification needs). Stored as a `u8`
/// so concurrent classifiers read it atomically (W3-3c).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SpanState {
    /// Active: backing committed, objects allocatable (the normal case).
    Active = 0,
    /// Released-but-retained (§20.1, P-Map-005): the backing was returned to the
    /// OS but the virtual range and descriptor are kept so the page cannot be
    /// reused without recommit. A pointer here classifies as `Released`.
    Released = 1,
}

impl SpanState {
    #[inline]
    const fn from_u8(v: u8) -> SpanState {
        match v {
            1 => SpanState::Released,
            _ => SpanState::Active,
        }
    }
}

/// Span flag bits (§16.2 `flags`). Only the bits classification needs are defined
/// here; the span lifecycle (W5) adds more.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpanFlags(pub u32);

impl SpanFlags {
    /// No flags.
    pub const NONE: SpanFlags = SpanFlags(0);
    /// The span's objects are in quarantine (plan 08): a pointer here classifies
    /// as `Quarantined` and a free is held, not actioned (§17.5).
    pub const QUARANTINED: u32 = 1 << 0;
    /// The span's advisory **placement class** (§24.6–§24.8, W14), packed into bits 1–2:
    /// the cold / hot / short-lived grouping pool it belongs to, so like-placed small
    /// objects cluster (and whole spans empty together). Purely advisory — it never affects
    /// object size, alignment, or validity (§24.5); it only steers which span a small
    /// allocation is carved from. Reset to `0` (Default) on recycle and re-set by the
    /// central path. Distinct bits from [`QUARANTINED`](Self::QUARANTINED), so the two are
    /// orthogonal.
    pub const PLACE_CLASS_SHIFT: u32 = 1;
    /// Mask for the 2-bit placement class in [`flags`](crate::SpanDescriptor).
    pub const PLACE_CLASS_MASK: u32 = 0b11 << Self::PLACE_CLASS_SHIFT;
}

/// The non-central terms of the §16.4 partition (`local_cached`, `transfer_cached`,
/// `quarantined`) — *logical* quantities the descriptor does not track per-op; a
/// caller reconstructs them in debug (W5-3c) and passes them to the
/// conservation/empty checks. All zero before caches exist (M1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NonCentralResidency {
    /// Objects held in per-CPU and thread caches.
    pub local_cached: u32,
    /// Objects held in transfer caches.
    pub transfer_cached: u32,
    /// Objects held in quarantine.
    pub quarantined: u32,
}

impl NonCentralResidency {
    /// Zero residency: all non-central terms are zero. Correct when
    /// `live_count` includes objects in caches (the current accounting
    /// model), reducing the law to `object_count = live + central_free`.
    pub const NONE: NonCentralResidency = NonCentralResidency {
        local_cached: 0,
        transfer_cached: 0,
        quarantined: 0,
    };

    /// Sum of the three non-central terms (saturating, to stay total).
    #[inline]
    pub const fn total(self) -> u32 {
        self.local_cached
            .saturating_add(self.transfer_cached)
            .saturating_add(self.quarantined)
    }
}

/// FNV-1a-style mix of metadata header fields into an integrity tag (§17.3
/// "checksums or generation tags for … headers in hardened mode"). Not a security
/// hash — a cheap corruption detector for read-mostly metadata.
#[inline]
fn header_checksum(parts: &[u64]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64; // FNV-1a 64-bit offset basis
    let mut i = 0;
    while i < parts.len() {
        h ^= parts[i];
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV-1a 64-bit prime
        i += 1;
    }
    h
}

/// A consistent snapshot of the geometry pointer classification needs (§16.3 /
/// §17.5), produced by [`SpanDescriptor::read_consistent_geometry`] under the
/// seqlock so every field belongs to one incarnation (W3-4).
#[derive(Clone, Copy, Debug)]
pub struct ClassifyGeometry {
    /// The span's base address.
    pub base: usize,
    /// Base address of object 0 (`align_up(base + slab_header, sc.align)`).
    pub object0: usize,
    /// Bytes per object (the class's usable size).
    pub object_size: usize,
    /// Objects carved from the span.
    pub object_count: usize,
    /// The span's size class.
    pub sc: SizeClassId,
    /// The owning arena.
    pub arena: ArenaId,
    /// The span's lifecycle state.
    pub state: SpanState,
    /// Whether the span is flagged quarantined.
    pub quarantined: bool,
}

/// The §16.2 span descriptor (W3-2). See the module docs for the conservation law,
/// the per-span lock (§8.5), the hybrid bitmap, and the concurrency model.
#[repr(C)]
pub struct SpanDescriptor {
    /// The span's base address `[base, base + page_count * PAGE_SIZE)`. Atomic so a
    /// recycle (which re-bases the slot) cannot tear a concurrent classifier's read;
    /// published before the pagemap entry (W3-6).
    base: AtomicUsize,
    /// Stable slot identity. Immutable for the descriptor's life (a recycle bumps
    /// `generation`, not `id`), so a plain read races no writer.
    id: SpanId,
    /// Owning arena (changes on recycle, §16.6).
    arena: AtomicU32,
    /// Pages backing this span.
    page_count: AtomicU32,
    /// Total objects carved from this span.
    object_count: AtomicU32,
    /// Bytes reserved at the span start before object 0 (`0` when metadata is
    /// out-of-line, §17.3 — the common case). Folds into the §16.3 slab layout.
    slab_header: AtomicU32,
    /// Span generation (§16.6 / §27.5). Bumped on recycle so a stale reference is
    /// detectable (the identity/ABA counter, distinct from `seq`).
    generation: AtomicU32,
    /// A **seqlock** version for consistent geometry reads (W3-4). Even when the
    /// geometry is stable, odd while a recycle is mid-update; a classifier brackets
    /// its geometry read between two equal even reads, so it never composes a result
    /// from two incarnations (a racing-free / use-after-free signal otherwise).
    seq: AtomicU32,
    /// §16.2 `flags` (e.g. `QUARANTINED`).
    flags: AtomicU32,
    /// Integrity tag over the geometry fields (§17.3), recomputed on new/recycle.
    integrity: AtomicU64,
    /// Objects currently owned by the application (span-lock protected).
    live_count: AtomicU32,
    /// `== popcount(free_bitmap)`: objects resident in the central free list (the
    /// cached aggregate of §8.5, span-lock protected alongside the bitmap).
    central_free_count: AtomicU32,
    /// Size class of every object in the span (changes on recycle, §16.6).
    sc: AtomicU16,
    /// [`SpanState`] as a `u8`.
    state: AtomicU8,
    /// The §27.2 span lock guarding `free_bitmap` + `central_free_count` +
    /// `live_count` as one critical section (§8.5).
    lock: SpanLock,
    /// Authoritative central-residency bitmap (§16.4), hybrid inline/out-of-line.
    free_bitmap: FreeBitmap,
    /// W18-3 (§29.4): per-object **quarantined** state — `bit(i) = 1` ⟺ object `i` is
    /// held in the delayed-reuse quarantine (app-freed, *not* central-resident, *not*
    /// re-vendable). Span-lock protected like `free_bitmap`, and **mutually exclusive**
    /// with it (an object is at most one of central-free / quarantined / live). This is
    /// the authoritative double-free marker that closes the eviction-window gap: a held
    /// object's bit stays set from hold until the drain transitions it, under the span
    /// lock, to central-free (**set free *before* clearing quarantined**, so a lock-free
    /// `is_quarantined || is_central_free` check always sees one bit set during the
    /// transition). Only present in `quarantine` builds, so `performance` pays nothing.
    #[cfg(feature = "quarantine")]
    quarantined_bitmap: FreeBitmap,
    /// Next span in the central free list's partial chain (W5-4a), or null.
    /// Protected by the owning [`CentralBin`](crate::central::CentralBin)'s
    /// lock, **not** the span lock — the span lock protects bitmap/counts, the
    /// central lock protects list membership.
    central_next: AtomicPtr<SpanDescriptor>,
}

// W3-2 acceptance: the descriptor footprint is asserted. `repr(C)` makes the
// layout deterministic; this compile-time bound guards against a footprint
// regression (the hybrid bitmap keeps the descriptor compact whatever the class),
// and `descriptor_footprint_is_pinned` pins the exact size.
#[cfg(not(feature = "quarantine"))]
const _: () = assert!(
    core::mem::size_of::<SpanDescriptor>() <= 128,
    "SpanDescriptor footprint regressed past its 128-byte budget"
);
// The `quarantine` build adds a second per-object bitmap (the §29.4 quarantined
// state) — a non-`performance` profile, so the footprint budget is relaxed by exactly
// one `FreeBitmap` (32 bytes); the default/`performance` build keeps the 128 budget.
#[cfg(feature = "quarantine")]
const _: () = assert!(
    core::mem::size_of::<SpanDescriptor>() <= 128 + core::mem::size_of::<FreeBitmap>(),
    "SpanDescriptor footprint regressed past its (quarantine) budget"
);
// Descriptors are pointed at by the pagemap, whose entry tag needs the low 3 bits
// (W3-3b), so the descriptor must be at least 8-aligned.
const _: () = assert!(core::mem::align_of::<SpanDescriptor>() >= 8);

/// How many times a consistent geometry read retries a racing recycle before
/// giving up and reporting the address as unowned (a pathological racing free).
const SEQLOCK_RETRIES: u32 = 64;

impl SpanDescriptor {
    /// Create a span descriptor for a freshly carved slab. The span starts with
    /// **no** central-resident objects and no live objects (counts zero, bitmap
    /// empty); the central list is populated by activation (W5-5). `generation`
    /// starts at [`Generation::FIRST`]. `None` if an out-of-line bitmap is needed
    /// but `meta` cannot supply it (safe failure).
    ///
    /// SPEC-transition: span `<new> -> Active` (§7.3)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: SpanId,
        arena: ArenaId,
        sc: SizeClassId,
        base: usize,
        page_count: u32,
        object_count: u32,
        slab_header: u32,
        meta: &dyn MetadataAlloc,
    ) -> Option<Self> {
        let free_bitmap = FreeBitmap::new_for(object_count, meta)?;
        #[cfg(feature = "quarantine")]
        let quarantined_bitmap = FreeBitmap::new_for(object_count, meta)?;
        let desc = Self {
            base: AtomicUsize::new(base),
            id,
            arena: AtomicU32::new(arena.0),
            page_count: AtomicU32::new(page_count),
            object_count: AtomicU32::new(object_count),
            slab_header: AtomicU32::new(slab_header),
            generation: AtomicU32::new(Generation::FIRST.0),
            seq: AtomicU32::new(0),
            flags: AtomicU32::new(SpanFlags::NONE.0),
            integrity: AtomicU64::new(0),
            live_count: AtomicU32::new(0),
            central_free_count: AtomicU32::new(0),
            sc: AtomicU16::new(sc.index() as u16),
            state: AtomicU8::new(SpanState::Active as u8),
            lock: SpanLock::new(),
            free_bitmap,
            #[cfg(feature = "quarantine")]
            quarantined_bitmap,
            central_next: AtomicPtr::new(ptr::null_mut()),
        };
        desc.refresh_integrity();
        Some(desc)
    }

    // --- immutable-after-publish geometry (read with Acquire by classifiers) ---

    /// The stable slot identity.
    #[inline]
    pub fn id(&self) -> SpanId {
        self.id
    }

    /// The span's base address.
    #[inline]
    pub fn base(&self) -> usize {
        self.base.load(Ordering::Acquire)
    }

    /// The owning arena.
    #[inline]
    pub fn arena(&self) -> ArenaId {
        ArenaId(self.arena.load(Ordering::Acquire))
    }

    /// The size class of every object in the span.
    #[inline]
    pub fn size_class(&self) -> SizeClassId {
        SizeClassId::new(self.sc.load(Ordering::Acquire) as usize)
    }

    /// The span's advisory placement class (§24.6–§24.8, W14) — the cold / hot / short-lived
    /// grouping pool it belongs to, as a `0..=3` tag (see [`PlaceClass`](crate::PlaceClass)).
    /// `0` (Default) unless the central path tagged it.
    #[inline]
    pub fn place_class(&self) -> u8 {
        ((self.flags.load(Ordering::Acquire) & SpanFlags::PLACE_CLASS_MASK)
            >> SpanFlags::PLACE_CLASS_SHIFT) as u8
    }

    /// Set the span's advisory placement class (§24.6–§24.8, W14), preserving the other flag
    /// bits. Set by the central path when a span is created or an empty span is re-tagged for
    /// reuse (both with no live objects, so re-tagging is safe). Advisory only — it never
    /// changes object size/alignment/validity (§24.5).
    #[inline]
    pub fn set_place_class(&self, class: u8) {
        let bits = ((class as u32) & 0b11) << SpanFlags::PLACE_CLASS_SHIFT;
        // CAS loop so a concurrent QUARANTINED set/clear (a different bit) is preserved.
        let _ = self
            .flags
            .fetch_update(Ordering::Release, Ordering::Acquire, |f| {
                Some((f & !SpanFlags::PLACE_CLASS_MASK) | bits)
            });
    }

    /// Pages backing this span.
    #[inline]
    pub fn page_count(&self) -> u32 {
        self.page_count.load(Ordering::Acquire)
    }

    /// Total objects carved from this span.
    #[inline]
    pub fn object_count(&self) -> u32 {
        self.object_count.load(Ordering::Acquire)
    }

    /// Bytes reserved at the span start before object 0 (§16.3). Zero when
    /// metadata is out-of-line (the common case); non-zero for inline metadata.
    #[inline]
    pub fn slab_header(&self) -> u32 {
        self.slab_header.load(Ordering::Acquire)
    }

    /// Byte length of the span (`page_count * PAGE_SIZE`). Never overflows for a
    /// real span (its bytes fit the address space).
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.page_count() as usize * PAGE_SIZE
    }

    /// The half-open address range `[base, base + byte_len)` the span covers.
    #[inline]
    pub fn range(&self) -> (usize, usize) {
        let base = self.base();
        (base, base + self.byte_len())
    }

    /// Base address of object 0 (§16.3): `align_up(base + slab_header, sc.align)`.
    /// `None` if the size class is out of range (a corrupted descriptor) or the
    /// rounding overflows `usize` — total on any input (§9.7), never a panic.
    #[inline]
    pub fn object0_base(&self) -> Option<usize> {
        let align = size_class::checked_row(self.size_class())?.align as usize;
        let header = self
            .base()
            .checked_add(self.slab_header.load(Ordering::Acquire) as usize)?;
        align_up(header, align)
    }

    /// A **consistent** snapshot of the geometry a classifier needs (W3-4), read
    /// under the `seq` seqlock so it never mixes two incarnations. A recycle racing
    /// the read is retried; `None` if the recycle is persistently in flight (the
    /// retry budget is exhausted — a pathological racing free), the §16.3 geometry
    /// overflows, the size class is out of range, or (hardened) the integrity tag
    /// fails. For a correct program (no concurrent recycle of a span being freed
    /// into) the first read is always consistent.
    ///
    /// **Total on any input and corruption-resistant.** The size-class lookup is
    /// bounds-checked, so a corrupted/invalid `sc` resolves to `None` instead of an
    /// out-of-bounds panic — classification stays total even over a corrupted
    /// descriptor. In a hardened build (`debug-checks`) the §17.3 integrity tag is
    /// validated, and a mismatch is disambiguated by re-reading the seqlock: an
    /// *unchanged* version means a genuine wild write (→ classify foreign), a changed
    /// one means a recycle merely raced the check (→ retry), so a benign recycle is
    /// never misreported as corruption.
    pub fn read_consistent_geometry(&self) -> Option<ClassifyGeometry> {
        for _ in 0..SEQLOCK_RETRIES {
            let s0 = self.seq.load(Ordering::Acquire);
            if s0 & 1 != 0 {
                core::hint::spin_loop(); // a recycle is mid-update; let it finish
                continue;
            }
            let base = self.base.load(Ordering::Acquire);
            let slab_header = self.slab_header.load(Ordering::Acquire) as usize;
            let sc = SizeClassId::new(self.sc.load(Ordering::Acquire) as usize);
            let arena = ArenaId(self.arena.load(Ordering::Acquire));
            let object_count = self.object_count.load(Ordering::Acquire) as usize;
            let state = SpanState::from_u8(self.state.load(Ordering::Acquire));
            let quarantined = self.flags.load(Ordering::Acquire) & SpanFlags::QUARANTINED != 0;
            // The read is consistent iff the version is unchanged across it.
            if self.seq.load(Ordering::Acquire) != s0 {
                continue; // recycled during the read; retry
            }
            // Hardened (§17.3): reject a corrupted read-mostly header. A tag mismatch
            // with an *unchanged* version (re-read) is genuine corruption; a changed
            // version means a recycle raced the check — retry rather than misreport.
            #[cfg(feature = "debug-checks")]
            if !self.validate_integrity() {
                if self.seq.load(Ordering::Acquire) == s0 {
                    return None; // genuine corruption
                }
                continue; // a recycle raced the integrity check; retry
            }
            // Bounds-checked size-class lookup: an out-of-range `sc` (a corrupted
            // descriptor) resolves to `None` (→ foreign) rather than panicking, so
            // classification is total on every input and in every profile.
            let row = size_class::checked_row(sc)?;
            let object0 = align_up(base.checked_add(slab_header)?, row.align as usize)?;
            return Some(ClassifyGeometry {
                base,
                object0,
                object_size: row.size as usize,
                object_count,
                sc,
                arena,
                state,
                quarantined,
            });
        }
        None
    }

    // --- lifecycle / ABA (atomic) ---

    /// The current [`SpanState`].
    #[inline]
    pub fn state(&self) -> SpanState {
        SpanState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set the span's state (e.g. `Active -> Released`). Used by the W3-6 sync
    /// protocol *before* it updates the pagemap entry.
    ///
    /// SPEC-transition: span state change (§7.3)
    #[inline]
    pub fn set_state(&self, state: SpanState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// The current generation (§16.6).
    #[inline]
    pub fn generation(&self) -> Generation {
        Generation(self.generation.load(Ordering::Acquire))
    }

    /// Whether the span is flagged quarantined (§17.5).
    #[inline]
    pub fn is_quarantined(&self) -> bool {
        self.flags.load(Ordering::Acquire) & SpanFlags::QUARANTINED != 0
    }

    /// Set or clear the quarantine flag.
    #[inline]
    pub fn set_quarantined(&self, on: bool) {
        if on {
            self.flags
                .fetch_or(SpanFlags::QUARANTINED, Ordering::Release);
        } else {
            self.flags
                .fetch_and(!SpanFlags::QUARANTINED, Ordering::Release);
        }
    }

    /// A snapshot of `(id, generation)` for stale-reference detection (W3-5). Pair
    /// it with [`GenGuard::matches`] after re-reading the descriptor to detect a
    /// recycle that happened in between.
    #[inline]
    pub fn gen_guard(&self) -> GenGuard {
        GenGuard {
            id: self.id,
            generation: self.generation(),
        }
    }

    /// Recompute and store the geometry integrity tag (§17.3). Called after every
    /// geometry change (new/recycle).
    #[inline]
    fn refresh_integrity(&self) {
        self.integrity
            .store(self.compute_integrity(), Ordering::Release);
    }

    /// The checksum the geometry fields currently imply. Reads with `Acquire` so a
    /// cross-thread [`validate_integrity`](Self::validate_integrity) that observes a
    /// recycled field also observes the recycle's seqlock bump (letting the caller
    /// distinguish corruption from a racing recycle).
    #[inline]
    fn compute_integrity(&self) -> u64 {
        header_checksum(&[
            self.id.0 as u64,
            self.base.load(Ordering::Acquire) as u64,
            self.arena.load(Ordering::Acquire) as u64,
            self.page_count.load(Ordering::Acquire) as u64,
            self.object_count.load(Ordering::Acquire) as u64,
            self.slab_header.load(Ordering::Acquire) as u64,
            self.sc.load(Ordering::Acquire) as u64,
            self.generation.load(Ordering::Acquire) as u64,
        ])
    }

    /// Whether the geometry integrity tag still matches the geometry (§17.3). A
    /// `false` means the read-mostly header was corrupted by a wild write. Run in
    /// debug/hardened when the span is quiescent (not mid-recycle).
    #[inline]
    pub fn validate_integrity(&self) -> bool {
        self.integrity.load(Ordering::Acquire) == self.compute_integrity()
    }

    /// The next pointer in the central list chain (partial or empty, W5-4a).
    ///
    /// # Lock contract
    ///
    /// Only valid under the owning [`CentralBin`](crate::central::CentralBin)'s
    /// lock. The span lock does NOT protect this field.
    #[inline]
    pub(crate) fn central_next_ptr(&self) -> *mut SpanDescriptor {
        self.central_next.load(Ordering::Relaxed)
    }

    /// Set the next pointer in the central list chain (partial or empty, W5-4a).
    ///
    /// # Lock contract
    ///
    /// Only valid under the owning [`CentralBin`](crate::central::CentralBin)'s
    /// lock. The span lock does NOT protect this field.
    #[inline]
    pub(crate) fn set_central_next(&self, next: *mut SpanDescriptor) {
        self.central_next.store(next, Ordering::Relaxed);
    }

    /// Recycle this descriptor slot for a different span (§16.6): re-base, re-class,
    /// re-arena, **bump the generation**, resize+clear the bitmap, and reset the
    /// accounting. `false` if the bitmap must grow but `meta` cannot supply it (safe
    /// failure — the slot is left usable for its old geometry). The caller MUST have
    /// removed every pagemap entry that pointed here (W3-6) so no classifier can
    /// observe the slot mid-recycle.
    ///
    /// SPEC-transition: span `Empty -> recycled` (§7.3 / §16.6)
    #[allow(clippy::too_many_arguments)]
    pub fn recycle(
        &self,
        arena: ArenaId,
        sc: SizeClassId,
        base: usize,
        page_count: u32,
        object_count: u32,
        slab_header: u32,
        meta: &dyn MetadataAlloc,
    ) -> bool {
        // Hold the span lock across the *entire* recycle (RAII, released on every
        // exit incl. a panic), so the bitmap resize and the geometry update are one
        // critical section: a lock-holder (a `SpanGuard` doing central accounting)
        // never observes a window where the bitmap is the new size but `object_count`
        // is still the old value (§8.5). Classifiers, which do not take the lock, are
        // kept consistent by the seqlock below.
        let _span_lock = self.lock();
        // Resize/clear the bitmap; if it cannot grow, fail before touching any
        // geometry so the slot stays usable for its old class (safe failure). The
        // guard releases the lock on this early return.
        if !self.free_bitmap.recycle_to(object_count, meta) {
            return false;
        }
        // W18-3: resize/clear the quarantined bitmap too. A recycle only happens for an
        // **empty** span (no live, no central-free, and — checked by the empty oracle —
        // no quarantined objects), so this clears an already-empty bitmap; a grow that
        // fails leaves the span usable for its old class (safe failure, like above).
        #[cfg(feature = "quarantine")]
        if !self.quarantined_bitmap.recycle_to(object_count, meta) {
            return false;
        }
        self.live_count.store(0, Ordering::Relaxed);
        self.central_free_count.store(0, Ordering::Relaxed);

        // Open the seqlock (odd ⇒ geometry update in progress): a classifier that
        // observes an odd version, or a version change across its read, retries
        // rather than composing a result from two incarnations (W3-4).
        self.seq.fetch_add(1, Ordering::AcqRel);
        // Bump the generation: a stale reference captured before the recycle now
        // mismatches (§16.6 / §27.5 ABA guard).
        let next = self.generation().next();
        self.generation.store(next.0, Ordering::Release);
        self.arena.store(arena.0, Ordering::Release);
        self.sc.store(sc.index() as u16, Ordering::Release);
        self.base.store(base, Ordering::Release);
        self.page_count.store(page_count, Ordering::Release);
        self.object_count.store(object_count, Ordering::Release);
        self.slab_header.store(slab_header, Ordering::Release);
        self.flags.store(SpanFlags::NONE.0, Ordering::Release);
        self.state.store(SpanState::Active as u8, Ordering::Release);
        // The caller MUST have removed this span from the central list before
        // recycling. Reset the link as defence in depth.
        debug_assert!(
            self.central_next.load(Ordering::Relaxed).is_null(),
            "recycle: span still linked in the central list"
        );
        self.central_next.store(ptr::null_mut(), Ordering::Relaxed);
        self.refresh_integrity();
        // Close the seqlock (even ⇒ stable): publishes every geometry store to a
        // classifier's acquire-load of the version. `_span_lock` then releases.
        self.seq.fetch_add(1, Ordering::AcqRel);
        true
    }

    // --- accounting: lock-free approximate reads (stats / classification) ---

    /// Whether object `i` is currently **central-free** (its free-bitmap bit is
    /// set) — a lock-free advisory probe for the W8 liveness checks
    /// (`Allocator::{owns, usable_size, realloc}`): exact when the caller owns
    /// the object or the span is quiescent; under a racing alloc/free of the
    /// same object (caller UB) either answer is acceptable. The authoritative
    /// check remains `central_insert` under the span lock (§8.5/W5-3b) — this
    /// never substitutes for it on the free path.
    #[inline]
    pub fn is_central_free(&self, i: usize) -> bool {
        self.free_bitmap.contains(i)
    }

    /// Whether object `i` is currently **quarantined** (W18-3, §29.4) — a lock-free
    /// read of the quarantined bitmap, the authoritative double-free marker for a held
    /// object. Sound to read lock-free on the free path because the drain transitions
    /// `quarantined → central-free` by setting the free bit **before** clearing the
    /// quarantined bit (both under the span lock), so a concurrent
    /// `is_quarantined(i) || is_central_free(i)` check always observes at least one bit
    /// set until the object is legitimately re-vended (§29.3 double-free detection).
    /// Always `false` without the `quarantine` feature (the bitmap does not exist).
    /// Distinct from the whole-span [`is_quarantined`](Self::is_quarantined) flag — this
    /// is the per-**object** W18-3 state.
    #[inline]
    pub fn is_object_quarantined(&self, i: usize) -> bool {
        #[cfg(feature = "quarantine")]
        {
            self.quarantined_bitmap.contains(i)
        }
        #[cfg(not(feature = "quarantine"))]
        {
            let _ = i;
            false
        }
    }

    /// Objects currently owned by the application (lock-free approximate read).
    #[inline]
    pub fn live_count(&self) -> u32 {
        self.live_count.load(Ordering::Relaxed)
    }

    /// Objects resident in the central free list (`== popcount(free_bitmap)`,
    /// lock-free approximate read). For a consistent snapshot use [`lock`](Self::lock).
    #[inline]
    pub fn central_free_count(&self) -> u32 {
        self.central_free_count.load(Ordering::Relaxed)
    }

    // --- accounting: the §8.5 critical section (via the span lock) ---

    /// Acquire the span lock (§27.2) and return a [`SpanGuard`]: the **only** way to
    /// mutate the accounting, so a bitmap edit and the cached count always move
    /// together (§8.5). Single-shot convenience wrappers below take and drop a guard
    /// internally; hold the guard yourself to compose several edits atomically (the
    /// W5 batch path).
    #[inline]
    pub fn lock(&self) -> SpanGuard<'_> {
        self.lock.acquire();
        SpanGuard { span: self }
    }

    /// Whether the cached `central_free_count` equals `popcount(free_bitmap)` (the
    /// §8.5 metadata-duplication invariant), read consistently under the lock.
    #[inline]
    pub fn central_count_matches_bitmap(&self) -> bool {
        self.lock().central_count_matches_bitmap()
    }

    /// Whether the §16.4 conservation law holds, read consistently under the lock.
    #[inline]
    pub fn conservation_holds(&self, non_central: NonCentralResidency) -> bool {
        self.lock().conservation_holds(non_central)
    }

    /// Conservation law where `live_count` includes all objects removed from
    /// central (including those held in CPU/transfer/thread caches). Correct
    /// as long as cache layers do not separately track per-span residency.
    #[inline]
    pub fn conservation_holds_central_only(&self) -> bool {
        self.conservation_holds(NonCentralResidency::NONE)
    }

    /// Whether the span is **empty** (§16.5), read consistently under the lock.
    #[inline]
    pub fn is_empty(&self, non_central: NonCentralResidency) -> bool {
        self.lock().is_empty(non_central)
    }

    /// Empty predicate where `live_count` includes cached objects. A span is
    /// empty only when all objects have been returned to central (`live_count == 0`
    /// and `central_free == object_count`). Objects in CPU/transfer/thread caches
    /// keep `live_count > 0`, preventing false-positive empty detection.
    #[inline]
    pub fn is_empty_central_only(&self) -> bool {
        self.is_empty(NonCentralResidency::NONE)
    }

    /// Reconstruct the non-central residency terms for this span.
    ///
    /// Currently returns `NONE`: `live_count` already includes objects held in
    /// CPU, transfer, and thread caches (they are counted as "removed from
    /// central"), so the conservation law holds without explicit non-central
    /// terms. If a future milestone redefines `live_count` to mean "in
    /// application code only" (excluding cached objects), this method must
    /// return actual cache/quarantine counts and all call sites using
    /// `NonCentralResidency::NONE` must migrate.
    #[inline]
    pub fn reconstruct_non_central_residency(&self) -> NonCentralResidency {
        NonCentralResidency::NONE
    }

    /// Appendix B.3 (span invariants, W19-1c): this span descriptor is internally
    /// well-formed. Acquires the span lock for a consistent snapshot of the
    /// accounting, then checks the §16.2/§16.4 clauses; total + side-effect-free.
    /// See [`check_invariants_locked`](Self::check_invariants_locked) for the
    /// clause list. The single span lock (rank `SPAN`) is the only lock taken, so
    /// calling this from a context that holds a lower-ranked lock (e.g. the central
    /// bin lock, rank `CENTRAL`) respects the §27.2 order.
    pub fn check_invariants(&self) -> bool {
        let g = self.lock();
        self.check_invariants_locked(&g)
    }

    /// Appendix B.3 (span invariants, W19-1c) under an already-held span lock —
    /// for a caller composing the check with other locked work (e.g. the central
    /// list walk). Total + side-effect-free. The clauses, mapped to Appendix B.3:
    ///
    /// * **size class valid + ranges fit** — the span's size class indexes a real
    ///   generated row, and the carved `object_count` does not exceed what the
    ///   §16.3 slab geometry ([`SlabLayout`]) admits at this base/header, so every
    ///   object range lies within the span (disjointness then holds by
    ///   construction: `size` is a multiple of `align`, no inter-object padding);
    /// * **free count == authoritative representation** — `central_free_count ==
    ///   popcount(free_bitmap)` (§8.5), the cached aggregate never drifts from the
    ///   bitmap;
    /// * **partition bound (§16.4)** — `live + central_free <= object_count`; the
    ///   implementation's `live_count` already subsumes the per-CPU/transfer/thread
    ///   cache *and* quarantine residency (an object removed from central but not
    ///   yet returned stays counted as live), so this is the exact law (an equality
    ///   once the span is fully activated), with the cache/quarantine terms folded
    ///   into `live`, never double-counted;
    /// * **quarantine exclusivity (W18-3)** — in a `quarantine` build, the held set
    ///   is a subset of `live` (`quarantined <= live`) and an object is never
    ///   simultaneously central-free and quarantined (the two bitmaps are
    ///   disjoint), so the authoritative double-free marker can never alias a
    ///   re-vendable object;
    /// * **generation integrity** — under `debug-checks`, the §17.3 integrity tag
    ///   (which covers the `generation` field, the §16.6/§27.5 stale-reuse guard)
    ///   still validates, so a corrupted descriptor header is caught.
    pub fn check_invariants_locked(&self, g: &SpanGuard<'_>) -> bool {
        let sc = self.size_class();
        // Size class must index a real generated row.
        if size_class::checked_row(sc).is_none() {
            return false;
        }
        let object_count = self.object_count() as usize;
        // The carved object count must fit the slab geometry at this base/header
        // (B.3 "object ranges fit within span"; disjointness is then structural).
        match SlabLayout::compute(sc, self.base(), self.slab_header() as usize) {
            Some(layout) if object_count <= layout.object_count => {}
            _ => return false,
        }
        // B.3: free count equals the authoritative bitmap popcount.
        if !g.central_count_matches_bitmap() {
            return false;
        }
        // B.3 / §16.4: the tracked terms never over-fill the span. `live_count`
        // is the implementation's "removed from central" aggregate — it already
        // subsumes the per-CPU/transfer/thread *and* quarantine residency (a held
        // object keeps `live_count` incremented so its slab cannot recycle, W18-3),
        // so the exact law is `live + central_free <= object_count` (equality once
        // the span is fully activated; the cache/quarantine terms are *inside*
        // `live`, never added again).
        let live = g.live_count() as usize;
        let central_free = g.central_free_count() as usize;
        if live + central_free > object_count {
            return false;
        }
        // W18-3: the quarantine state is a subset of `live` (a held object stays
        // counted as live), and central-free and quarantined are mutually
        // exclusive per object (the authoritative double-free marker can never
        // alias a re-vendable, central-resident object).
        #[cfg(feature = "quarantine")]
        {
            if g.quarantined_count() > live {
                return false;
            }
            for i in 0..object_count {
                if g.central_resident(i) && g.is_object_quarantined(i) {
                    return false;
                }
            }
        }
        // B.3 (generation): the §17.3 integrity tag covers `generation`, so a
        // validating tag witnesses an un-corrupted stale-reuse guard.
        #[cfg(feature = "debug-checks")]
        if !self.validate_integrity() {
            return false;
        }
        true
    }
}

/// A held [`SpanDescriptor`] span lock (§27.2 / §8.5). While alive, the accounting
/// (`free_bitmap`, `central_free_count`, `live_count`) is exclusively this holder's,
/// so a bitmap edit and the count update are one critical section and a reader
/// taking the lock sees them consistent. Released on drop.
pub struct SpanGuard<'a> {
    span: &'a SpanDescriptor,
}

impl SpanGuard<'_> {
    /// Return object `i` to the central free list: set its bitmap bit **and** bump
    /// `central_free_count` together (§8.5). `false` on a double insert (already
    /// resident — a double-free signal, plan 08 W18-2), with no count change.
    ///
    /// SPEC-transition: object `* -> FreeInCentral` (§7.2)
    #[inline]
    pub fn central_insert(&self, i: usize) -> bool {
        if i >= self.span.object_count() as usize {
            return false;
        }
        if self.span.free_bitmap.insert(i) {
            self.span.central_free_count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Remove object `i` from the central free list: clear its bitmap bit **and**
    /// decrement `central_free_count` together. `false` if it was not resident.
    ///
    /// SPEC-transition: object `FreeInCentral -> *` (§7.2)
    #[inline]
    pub fn central_remove(&self, i: usize) -> bool {
        if i >= self.span.object_count() as usize {
            return false;
        }
        if self.span.free_bitmap.remove(i) {
            // The bit was set, so the count is `>= 1` under the §8.5 invariant; guard
            // the subtraction in debug as defence-in-depth.
            debug_assert!(
                self.span.central_free_count.load(Ordering::Relaxed) > 0,
                "central_free_count underflow: count/bitmap diverged"
            );
            self.span.central_free_count.fetch_sub(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// W18-3 (§29.4): mark object `i` **quarantined** (live → held). Sets the
    /// quarantined bit without touching `live_count` (a held object stays counted as
    /// live so its slab cannot recycle while held — the separate `quarantine.bytes`
    /// ledger reports it). `false` if it was already quarantined (a redundant mark).
    /// The free bit is untouched (a held object is *not* central-resident).
    #[cfg(feature = "quarantine")]
    #[inline]
    pub fn mark_quarantined(&self, i: usize) -> bool {
        if i >= self.span.object_count() as usize {
            return false;
        }
        self.span.quarantined_bitmap.insert(i)
    }

    /// W18-3 (§29.4): clear object `i`'s **quarantined** bit (the second half of the
    /// drain transition; the first half — `central_insert`, setting the free bit — MUST
    /// run first under this same lock so a lock-free `is_quarantined || is_central_free`
    /// check never sees a gap). A no-op when not quarantined (so the normal free path
    /// may call it unconditionally). `true` if a bit was cleared.
    #[cfg(feature = "quarantine")]
    #[inline]
    pub fn clear_quarantined(&self, i: usize) -> bool {
        if i >= self.span.object_count() as usize {
            return false;
        }
        self.span.quarantined_bitmap.remove(i)
    }

    /// Whether object `i` is quarantined (consistent under the lock).
    #[cfg(feature = "quarantine")]
    #[inline]
    pub fn is_object_quarantined(&self, i: usize) -> bool {
        self.span.quarantined_bitmap.contains(i)
    }

    /// `popcount(quarantined_bitmap)` (consistent under the lock).
    #[cfg(feature = "quarantine")]
    #[inline]
    pub fn quarantined_count(&self) -> usize {
        self.span.quarantined_bitmap.count()
    }

    /// Set the live count (testing / W5 activation accounting).
    #[inline]
    pub fn set_live_count(&self, n: u32) {
        self.span.live_count.store(n, Ordering::Relaxed);
    }

    /// Objects owned by the application (consistent under the lock).
    #[inline]
    pub fn live_count(&self) -> u32 {
        self.span.live_count.load(Ordering::Relaxed)
    }

    /// Objects resident in the central list (consistent under the lock).
    #[inline]
    pub fn central_free_count(&self) -> u32 {
        self.span.central_free_count.load(Ordering::Relaxed)
    }

    /// Whether object `i` is central-resident (consistent under the lock).
    #[inline]
    pub fn central_resident(&self, i: usize) -> bool {
        self.span.free_bitmap.contains(i)
    }

    /// `popcount(free_bitmap)` (consistent under the lock).
    #[inline]
    pub fn bitmap_popcount(&self) -> usize {
        self.span.free_bitmap.count()
    }

    /// Whether `central_free_count == popcount(free_bitmap)` (§8.5), consistently.
    #[inline]
    pub fn central_count_matches_bitmap(&self) -> bool {
        self.central_free_count() as usize == self.bitmap_popcount()
    }

    /// The five-term partition of §16.4 as `(live, local_cached, transfer_cached,
    /// central_free, quarantined)`, given the reconstructed non-central terms.
    #[inline]
    pub fn partition(&self, non_central: NonCentralResidency) -> [u32; 5] {
        [
            self.live_count(),
            non_central.local_cached,
            non_central.transfer_cached,
            self.central_free_count(),
            non_central.quarantined,
        ]
    }

    /// Whether the §16.4 conservation law holds for the reconstructed non-central
    /// terms: the five terms sum to `object_count` and `central_free == popcount`.
    #[inline]
    pub fn conservation_holds(&self, non_central: NonCentralResidency) -> bool {
        if !self.central_count_matches_bitmap() {
            return false;
        }
        let sum = self
            .live_count()
            .saturating_add(non_central.total())
            .saturating_add(self.central_free_count());
        sum == self.span.object_count()
    }

    /// Whether the span is **empty** and may be returned to the backend (§16.5):
    /// every non-central term is zero **and** `central_free == object_count`. Never
    /// reads a cached free object as live — the cached terms are explicit inputs.
    #[inline]
    pub fn is_empty(&self, non_central: NonCentralResidency) -> bool {
        self.live_count() == 0
            && non_central.local_cached == 0
            && non_central.transfer_cached == 0
            && non_central.quarantined == 0
            && self.central_free_count() == self.span.object_count()
    }

    // --- batch operations for the central free list (W5-4b/c) ----------------

    /// Remove up to `max_count` central-free objects from the bitmap and write
    /// their indices into `out[..returned]`. Returns the number removed. Scans
    /// the bitmap word-by-word with `trailing_zeros` for efficiency. Does **not**
    /// update `live_count` — the caller does (the central list moves objects from
    /// central-free to live when it hands them out, W5-4b).
    ///
    /// SPEC-transition: object `FreeInCentral -> Allocated` batch (§7.2/§A.4)
    pub fn central_remove_batch(&self, out: &mut [u16], max_count: usize) -> usize {
        let max = max_count.min(out.len());
        if max == 0 {
            return 0;
        }
        let mut removed = 0;
        let words = self.span.free_bitmap.active_words();
        let obj_count = self.span.object_count() as usize;

        for w in 0..words {
            if removed >= max {
                break;
            }
            let word_ref = self.span.free_bitmap.word(w);
            let mut bits = word_ref.load(Ordering::Relaxed);
            let mut clear_mask = 0u64;

            while bits != 0 && removed < max {
                let bit_pos = bits.trailing_zeros();
                if bit_pos >= 64 {
                    break;
                }
                let idx = w * 64 + bit_pos as usize;
                if idx >= obj_count {
                    break;
                }
                let bit = 1u64 << bit_pos;
                clear_mask |= bit;
                bits &= !bit;
                out[removed] = idx as u16;
                removed += 1;
            }

            if clear_mask != 0 {
                let old = word_ref.load(Ordering::Relaxed);
                word_ref.store(old & !clear_mask, Ordering::Relaxed);
            }
        }

        if removed > 0 {
            let old = self.span.central_free_count.load(Ordering::Relaxed);
            debug_assert!(
                old >= removed as u32,
                "central_free_count underflow in batch removal"
            );
            self.span
                .central_free_count
                .store(old - removed as u32, Ordering::Relaxed);
        }
        removed
    }

    /// Fill the bitmap with the first `object_count` objects as central-free and
    /// set accounting for span activation (W5-5). After this the span has
    /// `live == 0`, `central_free == object_count`, and the conservation law
    /// holds.
    ///
    /// SPEC-transition: span activation (§14.6)
    pub fn activate(&self, object_count: u32) {
        self.span.free_bitmap.fill_below(object_count as usize);
        self.span
            .central_free_count
            .store(object_count, Ordering::Relaxed);
        self.span.live_count.store(0, Ordering::Relaxed);
        debug_assert!(self.central_count_matches_bitmap());
    }

    /// Clear the bitmap and reset all accounting (failed activation cleanup or
    /// span deactivation).
    pub fn deactivate(&self) {
        self.span.free_bitmap.clear();
        self.span.central_free_count.store(0, Ordering::Relaxed);
        self.span.live_count.store(0, Ordering::Relaxed);
    }
}

impl Drop for SpanGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.span.lock.release();
    }
}

/// A captured `(SpanId, Generation)` snapshot for stale-reference detection (§27.5,
/// W3-5). Code stashing a descriptor reference across a window where the span could
/// be recycled (sampled/debug paths) captures a `GenGuard` and re-validates with
/// [`matches`](Self::matches); a mismatch means the slot was recycled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GenGuard {
    id: SpanId,
    generation: Generation,
}

impl GenGuard {
    /// Whether `span` is still the same incarnation this guard was taken from
    /// (same id and generation). A `false` means the descriptor slot was recycled.
    #[inline]
    pub fn matches(&self, span: &SpanDescriptor) -> bool {
        span.id() == self.id && span.generation() == self.generation
    }
}

/// Lifecycle state of a large allocation (§17.2 P-Map-004).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum LargeState {
    /// Backing committed; the allocation is live.
    Active = 0,
    /// Released-but-retained (P-Map-005): a pointer here classifies as `Released`.
    Released = 1,
}

impl LargeState {
    #[inline]
    const fn from_u8(v: u8) -> LargeState {
        match v {
            1 => LargeState::Released,
            _ => LargeState::Active,
        }
    }
}

/// Descriptor for one large (`>= HUGE_THRESHOLD`) allocation (§17.2 P-Map-004): it
/// identifies the allocation base, usable size, alignment, arena, and state so an
/// unsized `free`/`realloc`/`usable_size` can recover them from the pagemap.
#[repr(C)]
pub struct LargeDescriptor {
    /// Allocation base address.
    base: AtomicUsize,
    /// Usable size in bytes (`>=` the request).
    usable_size: AtomicUsize,
    /// The **requested** byte size (`<= usable_size`), for the §31.5 exact internal-
    /// fragmentation metric (W17-4): `usable_size − requested` is this allocation's waste.
    /// Stats-only and deliberately **outside** the [`integrity`](Self::integrity) tag — a
    /// torn read or corruption only blurs a fragmentation stat, never a safety decision
    /// (free/realloc/usable_size never consult it). Defaults to `usable_size` (zero waste)
    /// until the engine records the real request via [`set_requested`](Self::set_requested).
    requested: AtomicUsize,
    /// Required alignment (a power of two).
    align: AtomicUsize,
    /// Integrity tag over the header fields (§17.3), recomputed on new/recycle.
    integrity: AtomicU64,
    /// Slot identity (immutable; a recycle bumps `generation`).
    id: LargeId,
    /// Owning arena.
    arena: AtomicU32,
    /// Generation (§27.5).
    generation: AtomicU32,
    /// Seqlock version for consistent classification reads (W3-4, mirroring the span
    /// seqlock): even when stable, odd while a recycle is mid-update.
    seq: AtomicU32,
    /// [`LargeState`] as a `u8`.
    state: AtomicU8,
}

// Like `SpanDescriptor`, a `LargeDescriptor` is reached through a 3-bit-tagged
// pagemap entry (W3-3b `TAG_LARGE`), so it must be at least 8-aligned for the tag
// bits and the pointer to coexist in one word. Pin it at compile time — mirroring
// the `SpanDescriptor` guard above — so a future field reorder, `repr` change, or
// narrow-`usize` target can never silently drop the alignment below 8 and make a
// tag bit collide with a real address bit (which `decode` would then mask off,
// corrupting the descriptor pointer `lookup` returns).
const _: () = assert!(core::mem::align_of::<LargeDescriptor>() >= 8);

impl LargeDescriptor {
    /// Create a descriptor for a live large allocation.
    pub fn new(id: LargeId, arena: ArenaId, base: usize, usable_size: usize, align: usize) -> Self {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        let desc = Self {
            base: AtomicUsize::new(base),
            usable_size: AtomicUsize::new(usable_size),
            requested: AtomicUsize::new(usable_size),
            align: AtomicUsize::new(align),
            integrity: AtomicU64::new(0),
            id,
            arena: AtomicU32::new(arena.0),
            generation: AtomicU32::new(Generation::FIRST.0),
            seq: AtomicU32::new(0),
            state: AtomicU8::new(LargeState::Active as u8),
        };
        desc.refresh_integrity();
        desc
    }

    /// A **consistent** `(base, state)` snapshot for classification (W3-4), read
    /// under the seqlock so a recycle race never composes the two from different
    /// incarnations. `None` if a recycle is persistently in flight or (hardened) the
    /// integrity tag fails. Mirrors [`SpanDescriptor::read_consistent_geometry`].
    pub fn read_consistent(&self) -> Option<(usize, LargeState)> {
        for _ in 0..SEQLOCK_RETRIES {
            let s0 = self.seq.load(Ordering::Acquire);
            if s0 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let base = self.base.load(Ordering::Acquire);
            let state = LargeState::from_u8(self.state.load(Ordering::Acquire));
            if self.seq.load(Ordering::Acquire) != s0 {
                continue;
            }
            // Hardened (§17.3): as in `read_consistent_geometry`, disambiguate a tag
            // mismatch by re-reading the version — corruption iff it is unchanged.
            #[cfg(feature = "debug-checks")]
            if !self.validate_integrity() {
                if self.seq.load(Ordering::Acquire) == s0 {
                    return None;
                }
                continue;
            }
            return Some((base, state));
        }
        None
    }

    /// Slot identity.
    #[inline]
    pub fn id(&self) -> LargeId {
        self.id
    }

    /// Allocation base address.
    #[inline]
    pub fn base(&self) -> usize {
        self.base.load(Ordering::Acquire)
    }

    /// Usable size in bytes.
    #[inline]
    pub fn usable_size(&self) -> usize {
        self.usable_size.load(Ordering::Acquire)
    }

    /// The requested size in bytes (`<= usable_size`), for the §31.5 exact internal-
    /// fragmentation metric (W17-4). Defaults to the usable size (zero waste) until
    /// [`set_requested`](Self::set_requested) records the real request.
    #[inline]
    pub fn requested(&self) -> usize {
        self.requested.load(Ordering::Relaxed)
    }

    /// Record the requested size (W17-4 stats only; relaxed — never a safety input). Clamped
    /// to `usable_size` so the fragmentation `usable − requested` can never underflow.
    #[inline]
    pub fn set_requested(&self, requested: usize) {
        let usable = self.usable_size.load(Ordering::Relaxed);
        self.requested
            .store(requested.min(usable), Ordering::Relaxed);
    }

    /// Required alignment.
    #[inline]
    pub fn align(&self) -> usize {
        self.align.load(Ordering::Acquire)
    }

    /// Owning arena.
    #[inline]
    pub fn arena(&self) -> ArenaId {
        ArenaId(self.arena.load(Ordering::Acquire))
    }

    /// End of the allocation (`base + usable_size`). Never overflows for a real
    /// allocation (it is the end of a reserved region).
    #[inline]
    pub fn end(&self) -> usize {
        self.base() + self.usable_size()
    }

    /// The current [`LargeState`].
    #[inline]
    pub fn state(&self) -> LargeState {
        LargeState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set the state (e.g. `Active -> Released`).
    #[inline]
    pub fn set_state(&self, state: LargeState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// The current generation.
    #[inline]
    pub fn generation(&self) -> Generation {
        Generation(self.generation.load(Ordering::Acquire))
    }

    /// Recycle this large descriptor slot for a different allocation (§27.5): re-set
    /// the header and **bump the generation** so a stale reference is detectable. The
    /// caller MUST have removed every pagemap entry pointing here first (W3-6).
    ///
    /// SPEC-transition: large descriptor recycled (§27.5)
    pub fn recycle(&self, arena: ArenaId, base: usize, usable_size: usize, align: usize) {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        // Open the seqlock (odd): a classifier reading through `read_consistent`
        // retries rather than mixing incarnations (W3-4).
        self.seq.fetch_add(1, Ordering::AcqRel);
        let next = self.generation().next();
        self.generation.store(next.0, Ordering::Release);
        self.arena.store(arena.0, Ordering::Release);
        self.base.store(base, Ordering::Release);
        self.usable_size.store(usable_size, Ordering::Release);
        // Reset the requested size to the usable size (zero waste) until the engine records
        // the real request for this incarnation (W17-4 stats only).
        self.requested.store(usable_size, Ordering::Relaxed);
        self.align.store(align, Ordering::Release);
        self.state
            .store(LargeState::Active as u8, Ordering::Release);
        self.refresh_integrity();
        // Close the seqlock (even): publish the new incarnation.
        self.seq.fetch_add(1, Ordering::AcqRel);
    }

    /// Shrink this **live** descriptor's usable size to `new_usable` in place — an
    /// in-place realloc shrink that returned the allocation's tail pages to the
    /// backend (§25.3, plan 06 W15-3b). The id, base, alignment, arena, and
    /// generation are all unchanged (this is the *same* allocation at the *same*
    /// address, just smaller); only the usable size and the integrity tag move.
    ///
    /// Run under the seqlock so a concurrent classifier reading through
    /// [`read_consistent`](Self::read_consistent) never composes a stale/new
    /// `usable_size` across the update (W3-4) — the same discipline as
    /// [`recycle`](Self::recycle), minus the generation bump (the allocation keeps
    /// its identity, so outstanding base pointers stay valid). The caller MUST have
    /// retired the returned tail's pagemap entries first (the
    /// [`PageMap::retire_large_range`](crate::PageMap) ordering).
    ///
    /// SPEC-transition: large in-place shrink (§25.3) — sequences the certified
    /// `extent_split` (§18.3) + `extent free` (§20.1); adds no abstract transition,
    /// the geometric premise pinned by the `realloc_shrink_inplace_tail_tiles_disjointly`
    /// theorem (`lean/TopoMalloc/Theorems/Realloc.lean`, plan 06 W15-3b).
    pub fn shrink_usable(&self, new_usable: usize) {
        debug_assert!(
            new_usable <= self.usable_size(),
            "shrink_usable must not grow the allocation"
        );
        // Open the seqlock (odd): a classifier reading through `read_consistent`
        // retries rather than mixing the old and new usable size (W3-4).
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.usable_size.store(new_usable, Ordering::Release);
        // The integrity tag (§17.3) covers `usable_size`, so recompute it.
        self.refresh_integrity();
        // Close the seqlock (even): publish the shrunk geometry.
        self.seq.fetch_add(1, Ordering::AcqRel);
    }

    /// Grow this **live** descriptor's usable size to `new_usable` in place — an
    /// in-place realloc grow that absorbed the adjacent free extent (§25.2,
    /// plan 06 W15-3a). The dual of [`shrink_usable`](Self::shrink_usable): the id,
    /// base, alignment, arena, and generation are all unchanged (the *same*
    /// allocation at the *same* address, just larger), so outstanding base pointers
    /// stay valid; only the usable size and the integrity tag move. The caller MUST
    /// have already grown the backing extent and installed the new pages' pagemap
    /// entries (the [`PageMap::install_large_range`](crate::PageMap) ordering).
    ///
    /// SPEC-transition: large in-place grow (§25.2) — composes `extent_merge`
    /// (§18.3); adds no abstract transition (the geometric premise is pinned by
    /// `realloc_grow_inplace_absorbs_disjointly`, plan 06 W15-3a).
    pub fn grow_usable(&self, new_usable: usize) {
        debug_assert!(
            new_usable >= self.usable_size(),
            "grow_usable must not shrink the allocation"
        );
        // Open the seqlock (odd): a classifier reading through `read_consistent`
        // retries rather than mixing the old and new usable size (W3-4).
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.usable_size.store(new_usable, Ordering::Release);
        // The integrity tag (§17.3) covers `usable_size`, so recompute it.
        self.refresh_integrity();
        // Close the seqlock (even): publish the grown geometry.
        self.seq.fetch_add(1, Ordering::AcqRel);
    }

    #[inline]
    fn refresh_integrity(&self) {
        self.integrity
            .store(self.compute_integrity(), Ordering::Release);
    }

    /// The checksum the header fields currently imply (reads `Acquire`; see
    /// [`SpanDescriptor::compute_integrity`] for the ordering rationale).
    #[inline]
    fn compute_integrity(&self) -> u64 {
        header_checksum(&[
            self.id.0 as u64,
            self.base.load(Ordering::Acquire) as u64,
            self.usable_size.load(Ordering::Acquire) as u64,
            self.align.load(Ordering::Acquire) as u64,
            self.arena.load(Ordering::Acquire) as u64,
            self.generation.load(Ordering::Acquire) as u64,
        ])
    }

    /// Whether the header integrity tag still matches (§17.3) — a corruption check
    /// for the read-mostly large header, run in debug/hardened.
    #[inline]
    pub fn validate_integrity(&self) -> bool {
        self.integrity.load(Ordering::Acquire) == self.compute_integrity()
    }
}

// SAFETY: every mutable field of `FreeBitmap` is atomic, and `out_of_line` points
// at `Sync` metadata that is never freed, so concurrent `&` access (always under
// the owning span lock) is data-race-free.
unsafe impl Sync for FreeBitmap {}
// SAFETY: as the `Sync` impl — the bitmap is atomics plus a pointer into `Sync`,
// never-freed metadata.
unsafe impl Send for FreeBitmap {}
// SAFETY: every mutable field of `SpanDescriptor` is atomic, `id` is immutable, and
// the bitmap is `Sync`; concurrent `&` access (a classifier through the pagemap
// plus the owner under the span lock) is data-race-free.
unsafe impl Sync for SpanDescriptor {}
// SAFETY: as the `Sync` impl — the descriptor is atomics and immutable data.
unsafe impl Send for SpanDescriptor {}
// SAFETY: every mutable field of `LargeDescriptor` is atomic and `id` is immutable,
// so concurrent `&` access is data-race-free.
unsafe impl Sync for LargeDescriptor {}
// SAFETY: as the `Sync` impl above.
unsafe impl Send for LargeDescriptor {}

/// Test-only fault injection: simulate a wild write to a read-mostly header field
/// *without* refreshing the integrity tag, to exercise the total/hardened
/// classification paths (W3-4 / §17.3).
#[cfg(test)]
impl SpanDescriptor {
    /// Corrupt the size-class field to an arbitrary (possibly out-of-range) value.
    pub(crate) fn corrupt_sc_for_test(&self, raw: u16) {
        self.sc.store(raw, Ordering::Relaxed);
    }

    /// Corrupt the object-count field (a valid-range value the integrity tag, but
    /// not the bounds check, will catch). Only the hardened profile validates the
    /// tag on classification, so this helper is exercised there.
    #[cfg(feature = "debug-checks")]
    pub(crate) fn corrupt_object_count_for_test(&self, raw: u32) {
        self.object_count.store(raw, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::ids::{ArenaId, LargeId, SizeClassId, SpanId};

    /// A leaked heap arena for out-of-line bitmap allocation in tests.
    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn span(object_count: u32, m: &BumpArena) -> SpanDescriptor {
        SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            0x4000_0000,
            1,
            object_count,
            0,
            m,
        )
        .expect("bitmap")
    }

    #[test]
    fn bitmap_covers_widest_slab_inline_and_out_of_line() {
        assert!(MAX_BITMAP_WORDS * 64 >= max_objects_per_slab());
        assert_eq!(MAX_BITMAP_WORDS, max_objects_per_slab().div_ceil(64));
        let m = meta(64 * 1024);
        // A small slab stays inline (no out-of-line allocation).
        let used_before = m.used();
        let b_small = FreeBitmap::new_for(64, &m).unwrap();
        assert_eq!(m.used(), used_before, "small bitmap must be inline");
        assert!(b_small.capacity_bits() >= 64);
        // A 1024-object slab spills out-of-line (consumes metadata).
        let b_big = FreeBitmap::new_for(1024, &m).unwrap();
        assert!(m.used() > used_before, "large bitmap must be out-of-line");
        assert!(b_big.capacity_bits() >= 1024);
    }

    #[test]
    fn bitmap_insert_remove_count_inline_and_out_of_line() {
        let m = meta(64 * 1024);
        for objects in [64u32, 1024] {
            let b = FreeBitmap::new_for(objects, &m).unwrap();
            let hi = (objects - 1) as usize; // distinct from 0 for both cases
            assert_eq!(b.count(), 0);
            assert!(b.insert(0));
            assert!(b.insert(hi));
            assert!(!b.insert(0)); // double insert detected, no double-count
            assert_eq!(b.count(), 2);
            assert!(b.contains(hi));
            assert!(b.remove(hi));
            assert!(!b.remove(hi)); // already clear
            assert_eq!(b.count(), 1);
            // The out-of-line case must also handle a cross-word bit.
            if objects > 64 {
                assert!(b.insert(64));
                assert_eq!(b.count(), 2);
            }
        }
    }

    #[test]
    fn bitmap_fill_below_sets_exact_prefix_out_of_line() {
        let m = meta(64 * 1024);
        let b = FreeBitmap::new_for(1024, &m).unwrap();
        b.fill_below(200);
        assert_eq!(b.count(), 200);
        for i in 0..200 {
            assert!(b.contains(i), "bit {i} should be set");
        }
        assert!(!b.contains(200));
    }

    #[test]
    fn descriptor_footprint_is_pinned() {
        // W3-2 "struct size asserted": the hybrid bitmap keeps the descriptor a
        // fixed, compact size for every class — a 32-byte control block (inline
        // 2-word bitmap + out-of-line pointer + word counts) plus a 64-byte header.
        assert_eq!(core::mem::size_of::<FreeBitmap>(), 32);
        // The default/`performance` descriptor is 104 bytes; a `quarantine` build adds
        // exactly one more `FreeBitmap` (the §29.4 per-object quarantined state, W18-3).
        #[cfg(not(feature = "quarantine"))]
        assert_eq!(core::mem::size_of::<SpanDescriptor>(), 104);
        #[cfg(feature = "quarantine")]
        assert_eq!(
            core::mem::size_of::<SpanDescriptor>(),
            104 + core::mem::size_of::<FreeBitmap>()
        );
        // The old design carried a 128-byte inline bitmap in *every* descriptor; the
        // hybrid is well under that whatever the class (plus one bitmap for quarantine).
        #[cfg(not(feature = "quarantine"))]
        assert!(core::mem::size_of::<SpanDescriptor>() <= 128);
        assert!(core::mem::align_of::<SpanDescriptor>() >= 8);
        assert!(core::mem::align_of::<LargeDescriptor>() >= 8);
    }

    /// W18-3 (§29.4): the per-object quarantined state and its **free-bit-first** drain
    /// transition — the invariant that makes a lock-free `is_object_quarantined ||
    /// is_central_free` double-free check sound through the eviction window. At no point
    /// from hold to re-vend are both bits clear, so a held/draining object is always
    /// detectable as a double free; only a legitimate re-vend (central-free → live,
    /// under the span lock) clears the last bit.
    #[cfg(feature = "quarantine")]
    #[test]
    fn quarantined_drain_is_free_bit_first() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        {
            let sg = s.lock();
            sg.set_live_count(10); // pretend 10 objects are live
                                   // Hold object 3 (live → quarantined): bit set, NOT central-free.
            assert!(sg.mark_quarantined(3));
            assert!(sg.is_object_quarantined(3));
            assert!(!sg.central_resident(3), "a held object is not central-free");
            assert!(!sg.mark_quarantined(3), "re-mark is a no-op (already held)");
        }
        // Lock-free reads see the held state.
        assert!(s.is_object_quarantined(3));
        assert!(!s.is_central_free(3));

        // Drain: free-bit-first. `central_insert` sets the free bit BEFORE
        // `clear_quarantined` clears the quarantined bit, so a lock-free check always
        // observes at least one bit set.
        {
            let sg = s.lock();
            assert!(sg.central_insert(3)); // free bit set
            sg.set_live_count(9); // the drain decrements live (as insert_batch does)
                                  // Mid-transition: free bit set, quarantined bit STILL set — a double-free
                                  // check here sees BOTH (never a gap).
            assert!(sg.central_resident(3));
            assert!(sg.is_object_quarantined(3));
            assert!(sg.clear_quarantined(3)); // now clear the quarantined bit
            assert!(!sg.is_object_quarantined(3));
            assert!(sg.central_resident(3));
        }
        // Final: object 3 is central-free (re-vendable), not quarantined.
        assert!(s.is_central_free(3));
        assert!(!s.is_object_quarantined(3));
    }

    #[test]
    fn place_class_round_trips_and_is_orthogonal_to_quarantine() {
        // W14: the advisory placement class packs into spare flag bits, so it round-trips,
        // survives a QUARANTINED set/clear (different bits), and defaults to 0.
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert_eq!(s.place_class(), 0, "fresh span defaults to class 0");
        for class in [0u8, 1, 2, 3] {
            s.set_place_class(class);
            assert_eq!(s.place_class(), class);
        }
        s.set_place_class(2);
        // Quarantining and un-quarantining must not disturb the placement class.
        s.set_quarantined(true);
        assert!(s.is_quarantined());
        assert_eq!(s.place_class(), 2, "quarantine flag is orthogonal");
        s.set_quarantined(false);
        assert!(!s.is_quarantined());
        assert_eq!(s.place_class(), 2);
        // Setting the class back must not re-quarantine.
        s.set_place_class(1);
        assert!(!s.is_quarantined());
        assert_eq!(s.place_class(), 1);
    }

    #[test]
    fn central_insert_remove_keep_count_and_bitmap_in_lockstep() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert!(s.central_count_matches_bitmap());
        {
            let g = s.lock();
            assert!(g.central_insert(3));
            assert!(g.central_insert(10));
            assert_eq!(g.central_free_count(), 2);
            assert!(g.central_count_matches_bitmap());
            assert!(!g.central_insert(3)); // double insert does not double-count
            assert_eq!(g.central_free_count(), 2);
            assert!(g.central_remove(3));
            assert_eq!(g.central_free_count(), 1);
            assert!(g.central_count_matches_bitmap());
        }
    }

    #[test]
    fn conservation_law_central_only_holds() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        {
            let g = s.lock();
            for i in 0..64 {
                assert!(g.central_insert(i));
            }
            g.set_live_count(0);
        }
        assert!(s.conservation_holds_central_only());
        // All 64 objects central, none live ⇒ the span is empty (§16.5).
        assert!(s.is_empty_central_only());

        {
            let g = s.lock();
            for i in 0..20 {
                assert!(g.central_remove(i));
            }
            g.set_live_count(20);
        }
        assert!(s.conservation_holds_central_only());
        assert_eq!(s.live_count(), 20);
        assert_eq!(s.central_free_count(), 44);
        assert!(!s.is_empty_central_only());
    }

    #[test]
    fn conservation_law_with_reconstructed_cached_terms() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        {
            let g = s.lock();
            for i in 0..47 {
                assert!(g.central_insert(i));
            }
            g.set_live_count(10);
        }
        let nc = NonCentralResidency {
            local_cached: 4,
            transfer_cached: 2,
            quarantined: 1,
        };
        assert!(s.conservation_holds(nc));
        assert_eq!(s.lock().partition(nc), [10, 4, 2, 47, 1]);
        assert!(!s.is_empty(nc));
    }

    #[test]
    fn empty_detection_never_reads_a_cached_object_as_live() {
        let m = meta(64 * 1024);
        let s = span(8, &m);
        {
            let g = s.lock();
            for i in 0..7 {
                assert!(g.central_insert(i));
            }
            g.set_live_count(0);
        }
        let cached = NonCentralResidency {
            transfer_cached: 1,
            ..NonCentralResidency::NONE
        };
        assert!(!s.is_empty(cached));
        assert!(s.conservation_holds(cached)); // 0 + 0 + 1 + 7 + 0 == 8 ✓
    }

    #[test]
    fn empty_detection_accounts_for_every_non_central_cache_term() {
        // B.3 (W19-1c) "empty-span detection accounts for local, transfer, central,
        // and quarantine states": a span with objects held in *any* cache term is
        // not empty (returning it would recycle live memory), and the §16.4
        // partition reconciles for each term independently and combined. This is
        // the across-caches clause exercised with synthetic non-central terms (the
        // live cache layer that supplies them lands at M2; the reasoning is proven
        // here regardless).
        let m = meta(64 * 1024);
        let s = span(8, &m);
        {
            let g = s.lock();
            for i in 0..5 {
                assert!(g.central_insert(i)); // 5 central-free
            }
            g.set_live_count(0);
        }
        // 5 central-free + 3 held across the cache terms = the 8 objects.
        let combos = [
            NonCentralResidency {
                local_cached: 3,
                ..NonCentralResidency::NONE
            },
            NonCentralResidency {
                transfer_cached: 3,
                ..NonCentralResidency::NONE
            },
            NonCentralResidency {
                quarantined: 3,
                ..NonCentralResidency::NONE
            },
            NonCentralResidency {
                local_cached: 1,
                transfer_cached: 1,
                quarantined: 1,
            },
        ];
        for nc in combos {
            assert!(
                !s.is_empty(nc),
                "a span with cached objects ({nc:?}) is not empty"
            );
            assert!(
                s.conservation_holds(nc),
                "the §16.4 partition reconciles with the cache terms ({nc:?})"
            );
        }
        // With *no* cached objects, the same span is empty only once every object
        // is central-free (the central-only relaxation the M1 checker uses).
        {
            let g = s.lock();
            for i in 5..8 {
                assert!(g.central_insert(i));
            }
        }
        assert!(s.is_empty(NonCentralResidency::NONE));
        assert!(s.is_empty_central_only());
    }

    #[test]
    fn object0_base_is_base_for_page_aligned_span() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert_eq!(s.object0_base(), Some(s.base()));
    }

    #[test]
    fn recycle_bumps_generation_and_resets_accounting() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        let guard = s.gen_guard();
        assert!(guard.matches(&s));
        {
            let g = s.lock();
            for i in 0..64 {
                g.central_insert(i);
            }
        }

        // Recycle the slot for a different class/arena/base (a smaller slab — no
        // bitmap growth needed).
        assert!(s.recycle(ArenaId(2), SizeClassId::new(5), 0x8000_0000, 1, 32, 0, &m));
        assert!(!guard.matches(&s)); // captured guard is now stale (W3-5)
        assert_eq!(s.generation(), Generation::FIRST.next());
        assert_eq!(s.arena(), ArenaId(2));
        assert_eq!(s.object_count(), 32);
        assert_eq!(s.central_free_count(), 0);
        assert_eq!(s.live_count(), 0);
        assert!(s.central_count_matches_bitmap());
        assert!(s.validate_integrity());
    }

    #[test]
    fn recycle_grows_bitmap_from_inline_to_out_of_line() {
        let m = meta(64 * 1024);
        // Start inline (small slab), recycle to a 1024-object slab needing an
        // out-of-line bitmap.
        let s = span(8, &m);
        assert!(s.recycle(
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            0x4000_0000,
            1,
            1024,
            0,
            &m
        ));
        assert_eq!(s.object_count(), 1024);
        {
            let g = s.lock();
            for i in 0..1024 {
                assert!(g.central_insert(i));
            }
            assert_eq!(g.central_free_count(), 1024);
            assert!(g.central_count_matches_bitmap());
        }
        assert!(s.is_empty_central_only());
    }

    #[test]
    fn integrity_tag_detects_geometry_corruption() {
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert!(s.validate_integrity());
        // Simulate a wild write corrupting the object_count without updating the tag.
        s.object_count.store(999, Ordering::Relaxed);
        assert!(
            !s.validate_integrity(),
            "corruption must be detected (§17.3)"
        );
    }

    #[test]
    fn geometry_read_is_total_over_a_corrupted_size_class() {
        // W3-4 totality (every profile): a wild write setting `sc` out of the table's
        // range makes the geometry read resolve to `None` (→ a foreign pointer)
        // rather than indexing the size-class table out of bounds and panicking.
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert!(s.read_consistent_geometry().is_some());
        s.corrupt_sc_for_test(40_000); // far beyond the 72-class table
        assert!(s.read_consistent_geometry().is_none());
        assert!(s.object0_base().is_none());
    }

    #[cfg(feature = "debug-checks")]
    #[test]
    fn hardened_geometry_read_rejects_a_corrupted_header() {
        // §17.3 hardened: the integrity tag is validated on the classification path,
        // so a wild write to a (valid-range) header field — which the bounds check
        // alone would miss — makes the geometry read resolve to `None`.
        let m = meta(64 * 1024);
        let s = span(64, &m);
        assert!(s.read_consistent_geometry().is_some());
        s.corrupt_object_count_for_test(12_345);
        assert!(
            s.read_consistent_geometry().is_none(),
            "hardened classification must reject a corrupted header (§17.3)"
        );
    }

    #[test]
    fn large_descriptor_records_p_map_004_fields_and_recycles() {
        let d = LargeDescriptor::new(LargeId(7), ArenaId(1), 0x10_0000, 3_000_000, 4096);
        assert_eq!(d.id(), LargeId(7));
        assert_eq!(d.base(), 0x10_0000);
        assert_eq!(d.usable_size(), 3_000_000);
        assert_eq!(d.align(), 4096);
        assert_eq!(d.arena(), ArenaId(1));
        assert_eq!(d.end(), 0x10_0000 + 3_000_000);
        assert_eq!(d.state(), LargeState::Active);
        assert!(d.validate_integrity());
        d.set_state(LargeState::Released);
        assert_eq!(d.state(), LargeState::Released);

        // Recycle bumps the generation and re-sets the header (W3-5).
        d.recycle(ArenaId(2), 0x20_0000, 5_000_000, 8192);
        assert_eq!(d.generation(), Generation::FIRST.next());
        assert_eq!(d.base(), 0x20_0000);
        assert_eq!(d.usable_size(), 5_000_000);
        assert_eq!(d.state(), LargeState::Active);
        assert!(d.validate_integrity());
    }

    #[test]
    fn span_lock_serializes_concurrent_central_inserts() {
        // Each thread inserts a disjoint set of objects under the span lock; the
        // count and bitmap stay in lockstep and end consistent (§8.5).
        let m = meta(64 * 1024);
        let span = span(256, &m);
        let s = &span; // shared reference (SpanDescriptor is Sync), copied per thread
        std::thread::scope(|sc| {
            for t in 0..4u32 {
                sc.spawn(move || {
                    for i in 0..64u32 {
                        let obj = (t * 64 + i) as usize;
                        let g = s.lock();
                        assert!(g.central_insert(obj));
                        // Invariant holds at every critical-section boundary.
                        assert!(g.central_count_matches_bitmap());
                    }
                });
            }
        });
        let g = span.lock();
        assert_eq!(g.central_free_count(), 256);
        assert!(g.central_count_matches_bitmap());
    }

    #[test]
    fn central_insert_and_remove_reject_out_of_range_index() {
        let m = meta(256 * 1024);
        let sc = SizeClassId::new(0);
        let row = size_class::row(sc);
        let span = SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            sc,
            0x4000_0000,
            row.slab_pages,
            row.objects_per_slab,
            0,
            &m,
        )
        .unwrap();

        let g = span.lock();
        let obj_count = span.object_count() as usize;

        // In-range insert succeeds.
        assert!(g.central_insert(0));
        assert_eq!(g.central_free_count(), 1);

        // Out-of-range insert is rejected without UB or count change.
        assert!(!g.central_insert(obj_count));
        assert!(!g.central_insert(obj_count + 100));
        assert!(!g.central_insert(usize::MAX));
        assert_eq!(g.central_free_count(), 1);

        // In-range remove succeeds.
        assert!(g.central_remove(0));
        assert_eq!(g.central_free_count(), 0);

        // Out-of-range remove is rejected.
        assert!(!g.central_remove(obj_count));
        assert!(!g.central_remove(usize::MAX));
        assert_eq!(g.central_free_count(), 0);
    }
}

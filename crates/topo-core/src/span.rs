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
//! **Concurrency (W3-3c/W3-5).** A descriptor is published into the pagemap by a
//! release-store and reached by concurrent classifiers through a raw pointer
//! (§17.5). Descriptors are **never freed** — they live in monotonic metadata and
//! are *recycled in place* with a [`Generation`] bump (§27.5) — so a classifier's
//! pointer is always dereferenceable; the generation flags a logical reuse. Every
//! field a classifier may read concurrently is therefore atomic, so even a read
//! racing a recycle is well-defined and the generation guard ([`GenGuard`])
//! detects the identity change.

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::generated::tables::{PAGE_SIZE, SIZE_CLASSES};
use crate::ids::{ArenaId, Generation, LargeId, SizeClassId, SpanId};
use crate::overflow::align_up;
use crate::size_class;

/// Largest `objects_per_slab` over every size class in the generated table — the
/// number of bits a [`FreeBitmap`] must cover. Computed from the table (a `const`
/// scan) so it cannot drift from the shipped size classes (DD-1).
const fn max_objects_per_slab() -> usize {
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

/// Number of `u64` words a [`FreeBitmap`] needs to cover every class's slab.
pub const BITMAP_WORDS: usize = max_objects_per_slab().div_ceil(64);

/// Number of object slots a [`FreeBitmap`] can address (`BITMAP_WORDS * 64`).
pub const BITMAP_CAPACITY: usize = BITMAP_WORDS * 64;

/// The §16.4 free bitmap: `bit(i) = 1` ⟺ object `i` is **resident in this span's
/// central free list**. Fixed inline storage sized to the widest slab in the
/// generated table (W3-2).
///
/// Each word is atomic so an individual set/clear is well-defined under concurrent
/// reads (e.g. a debug [`count`](Self::count) sweep). The *pairing* of a bitmap
/// edit with the cached `central_free_count` into one critical section is the span
/// lock's job (§8.5, W5-2); [`SpanDescriptor::central_insert`] /
/// [`central_remove`](SpanDescriptor::central_remove) update both together so the
/// `central_free == popcount` invariant can never tear within the descriptor.
///
/// > **Footprint (M1 trade-off).** The widest slab is the 16-byte class: 1024
/// > objects ⇒ a 128-byte bitmap inline in every descriptor. M1 favours a simple,
/// > self-contained descriptor over footprint; a later revision can move large
/// > bitmaps out-of-line (the `central_free == popcount` contract is unchanged).
#[repr(C)]
pub struct FreeBitmap {
    words: [AtomicU64; BITMAP_WORDS],
}

impl FreeBitmap {
    /// An empty bitmap (no object resident in the central list).
    #[inline]
    pub const fn new() -> Self {
        // `[const { … }; N]` initializes each element with a fresh atomic.
        Self {
            words: [const { AtomicU64::new(0) }; BITMAP_WORDS],
        }
    }

    /// Mark object `i` resident in the central list. Returns `true` iff the bit
    /// was previously clear — a `false` signals a **double insert** (the same
    /// object returned to the central list twice), the bitmap face of a
    /// double-free (caught by debug/hardened, plan 08 W18-2).
    #[inline]
    pub fn insert(&self, i: usize) -> bool {
        debug_assert!(i < BITMAP_CAPACITY, "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        let prev = self.words[w].fetch_or(bit, Ordering::Relaxed);
        prev & bit == 0
    }

    /// Clear object `i` from the central list (it is being carved into a batch).
    /// Returns `true` iff the bit was previously set — a `false` signals removing
    /// an object that was not central-resident.
    #[inline]
    pub fn remove(&self, i: usize) -> bool {
        debug_assert!(i < BITMAP_CAPACITY, "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        let prev = self.words[w].fetch_and(!bit, Ordering::Relaxed);
        prev & bit != 0
    }

    /// Whether object `i` is currently resident in the central list.
    #[inline]
    pub fn contains(&self, i: usize) -> bool {
        debug_assert!(i < BITMAP_CAPACITY, "object index out of bitmap range");
        let (w, bit) = (i / 64, 1u64 << (i % 64));
        self.words[w].load(Ordering::Relaxed) & bit != 0
    }

    /// Number of objects resident in the central list (`popcount`). This is the
    /// authoritative central count `central_free_count` must equal (§16.4).
    #[inline]
    pub fn count(&self) -> usize {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    /// Mark objects `[0, n)` resident and clear the rest — the bitmap half of
    /// activating a freshly carved slab (every object starts in the central list).
    /// `n` must be `<= BITMAP_CAPACITY`.
    #[inline]
    pub fn fill_below(&self, n: usize) {
        debug_assert!(n <= BITMAP_CAPACITY, "fill count out of bitmap range");
        for (w, word) in self.words.iter().enumerate() {
            let lo = w * 64;
            let val = if n >= lo + 64 {
                u64::MAX
            } else if n <= lo {
                0
            } else {
                (1u64 << (n - lo)) - 1
            };
            word.store(val, Ordering::Relaxed);
        }
    }
}

impl Default for FreeBitmap {
    fn default() -> Self {
        Self::new()
    }
}

/// Lifecycle state of a span (§7.3, the slice classification needs). Stored as a
/// `u8` so it is read atomically by concurrent classifiers (W3-3c).
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
}

/// The non-central terms of the §16.4 partition (`local_cached`, `transfer_cached`,
/// `quarantined`). These are *logical* quantities the descriptor does not track
/// per-op; a caller reconstructs them in debug (W5-3c) and passes them to the
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
    /// The trivial residency before any cache exists (M1): every cached/quarantined
    /// term is zero, so the conservation law reduces to `object_count = live +
    /// central_free`.
    pub const NONE: NonCentralResidency = NonCentralResidency {
        local_cached: 0,
        transfer_cached: 0,
        quarantined: 0,
    };

    /// Sum of the three non-central terms.
    #[inline]
    pub const fn total(self) -> u32 {
        // None of these can realistically overflow `u32` (each is bounded by a
        // span's object count), but use a saturating sum to stay total.
        self.local_cached
            .saturating_add(self.transfer_cached)
            .saturating_add(self.quarantined)
    }
}

/// The §16.2 span descriptor (W3-2). See the module docs for the conservation law
/// and the concurrency model. Every concurrently-mutable field is atomic so a
/// classifier reaching this descriptor through the pagemap races nothing (W3-3c).
#[repr(C)]
pub struct SpanDescriptor {
    /// The span's base address `[base, base + page_count * PAGE_SIZE)`. Atomic so
    /// a recycle (which re-bases the slot) cannot tear a concurrent classifier's
    /// read; published before the pagemap entry (W3-6).
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
    /// detectable.
    generation: AtomicU32,
    /// §16.2 `flags` (e.g. `QUARANTINED`).
    flags: AtomicU32,
    /// Objects currently owned by the application.
    live_count: AtomicU32,
    /// `== popcount(free_bitmap)`: objects resident in the central free list (the
    /// cached aggregate of §8.5, kept in lockstep with the bitmap).
    central_free_count: AtomicU32,
    /// Size class of every object in the span (changes on recycle, §16.6).
    sc: AtomicU16,
    /// [`SpanState`] as a `u8`.
    state: AtomicU8,
    /// Explicit padding so the fixed header is a clean size (asserted below).
    _pad: u8,
    /// Authoritative central-residency bitmap (§16.4).
    free_bitmap: FreeBitmap,
}

/// Fixed (non-bitmap) header size of [`SpanDescriptor`]. Pinning it — rather than
/// the whole struct — keeps the W3-2 size assertion meaningful when the table (and
/// thus the bitmap width) is retuned.
const SPAN_DESC_HEADER: usize = 48;

// W3-2 acceptance: the descriptor's footprint is asserted. `repr(C)` makes the
// layout deterministic; the descriptor is exactly its fixed header plus the
// table-sized bitmap, with no hidden padding.
const _: () = assert!(
    core::mem::size_of::<SpanDescriptor>() == SPAN_DESC_HEADER + core::mem::size_of::<FreeBitmap>(),
    "SpanDescriptor layout changed: re-check the W3-2 footprint assertion"
);

impl SpanDescriptor {
    /// Create a span descriptor for a freshly carved slab. The span starts with
    /// **no** central-resident objects and no live objects (counts zero, bitmap
    /// empty); the central list is populated by activation (W5-5). `generation`
    /// starts at [`Generation::FIRST`].
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
    ) -> Self {
        Self {
            base: AtomicUsize::new(base),
            id,
            arena: AtomicU32::new(arena.0),
            page_count: AtomicU32::new(page_count),
            object_count: AtomicU32::new(object_count),
            slab_header: AtomicU32::new(slab_header),
            generation: AtomicU32::new(Generation::FIRST.0),
            flags: AtomicU32::new(SpanFlags::NONE.0),
            live_count: AtomicU32::new(0),
            central_free_count: AtomicU32::new(0),
            sc: AtomicU16::new(sc.index() as u16),
            state: AtomicU8::new(SpanState::Active as u8),
            _pad: 0,
            free_bitmap: FreeBitmap::new(),
        }
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

    /// Byte length of the span (`page_count * PAGE_SIZE`). Never overflows: a span
    /// of `u32::MAX` pages would exceed the address space, which the backend never
    /// reserves; the product is a `usize` on every supported target.
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
    /// `None` only if the rounding overflows `usize` (impossible for a real span;
    /// kept total per §9.7).
    #[inline]
    pub fn object0_base(&self) -> Option<usize> {
        let align = size_class::align(self.size_class());
        let header = self
            .base()
            .checked_add(self.slab_header.load(Ordering::Acquire) as usize)?;
        align_up(header, align)
    }

    // --- lifecycle / ABA (atomic) ---

    /// The current [`SpanState`].
    #[inline]
    pub fn state(&self) -> SpanState {
        SpanState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Set the span's state (e.g. `Active -> Released` at release-to-OS). Used by
    /// the W3-6 sync protocol *before* it updates the pagemap entry.
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

    /// Recycle this descriptor slot for a different span (§16.6): re-base, re-class,
    /// re-arena, reset the accounting, and **bump the generation** so any reference
    /// captured before this call is detectably stale (§27.5). The caller MUST have
    /// already removed every pagemap entry that pointed here (W3-6) so no
    /// classifier can observe the slot mid-recycle.
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
    ) {
        // Bump the generation first: a racing stale reader that re-reads the
        // generation after this point sees the new value and reports "stale".
        let next = self.generation().next();
        self.generation.store(next.0, Ordering::Release);
        self.arena.store(arena.0, Ordering::Release);
        self.sc.store(sc.index() as u16, Ordering::Release);
        self.base.store(base, Ordering::Release);
        self.page_count.store(page_count, Ordering::Release);
        self.object_count.store(object_count, Ordering::Release);
        self.slab_header.store(slab_header, Ordering::Release);
        self.live_count.store(0, Ordering::Release);
        self.central_free_count.store(0, Ordering::Release);
        self.flags.store(SpanFlags::NONE.0, Ordering::Release);
        self.free_bitmap.fill_below(0);
        self.state.store(SpanState::Active as u8, Ordering::Release);
    }

    // --- accounting (the conservation-law fields, §16.4) ---

    /// Objects currently owned by the application.
    #[inline]
    pub fn live_count(&self) -> u32 {
        self.live_count.load(Ordering::Relaxed)
    }

    /// Objects resident in the central free list (`== popcount(free_bitmap)`).
    #[inline]
    pub fn central_free_count(&self) -> u32 {
        self.central_free_count.load(Ordering::Relaxed)
    }

    /// Read-only view of the free bitmap (for debug checks / classification).
    #[inline]
    pub fn free_bitmap(&self) -> &FreeBitmap {
        &self.free_bitmap
    }

    /// Set the live count (testing / W5 activation accounting).
    #[inline]
    pub fn set_live_count(&self, n: u32) {
        self.live_count.store(n, Ordering::Relaxed);
    }

    /// Return object `i` to the central free list: set its bitmap bit **and** bump
    /// `central_free_count` so the `central_free == popcount` invariant holds across
    /// the pair (§8.5). Returns `false` on a double insert (the object was already
    /// central-resident) — a double-free signal (plan 08 W18-2). The span lock
    /// makes the pair one critical section (W5-2).
    ///
    /// SPEC-transition: object `* -> FreeInCentral` (§7.2)
    #[inline]
    pub fn central_insert(&self, i: usize) -> bool {
        if self.free_bitmap.insert(i) {
            self.central_free_count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false // already resident — do not double-count
        }
    }

    /// Remove object `i` from the central free list (it is being carved out): clear
    /// its bitmap bit **and** decrement `central_free_count` together. Returns
    /// `false` if the object was not central-resident.
    ///
    /// SPEC-transition: object `FreeInCentral -> *` (§7.2)
    #[inline]
    pub fn central_remove(&self, i: usize) -> bool {
        if self.free_bitmap.remove(i) {
            self.central_free_count.fetch_sub(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Whether the cached `central_free_count` equals `popcount(free_bitmap)` — the
    /// §8.5 metadata-duplication invariant. Always true outside a torn update;
    /// debug checks assert it after every central transition (B.3, plan 08).
    #[inline]
    pub fn central_count_matches_bitmap(&self) -> bool {
        self.central_free_count() as usize == self.free_bitmap.count()
    }

    /// The five-term partition of §16.4 as `(live, local_cached, transfer_cached,
    /// central_free, quarantined)`, given the caller's reconstructed non-central
    /// terms. The central terms are read from the descriptor; the rest are supplied.
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

    /// Whether the §16.4 conservation law holds for this span given the
    /// reconstructed non-central terms: the five terms **sum to `object_count`**
    /// and `central_free == popcount(free_bitmap)`. No object is double-counted by
    /// construction (the terms partition the slab).
    #[inline]
    pub fn conservation_holds(&self, non_central: NonCentralResidency) -> bool {
        if !self.central_count_matches_bitmap() {
            return false;
        }
        let sum = self
            .live_count()
            .saturating_add(non_central.total())
            .saturating_add(self.central_free_count());
        sum == self.object_count()
    }

    /// The central-only form of the law (M1: no caches, so the cached/quarantined
    /// terms are zero): `object_count == live + central_free` and
    /// `central_free == popcount`.
    #[inline]
    pub fn conservation_holds_central_only(&self) -> bool {
        self.conservation_holds(NonCentralResidency::NONE)
    }

    /// Whether the span is **empty** and may be returned to the backend (§16.5):
    /// every non-central term is zero **and** `central_free == object_count`. It
    /// never reads a cached free object as live — the cached terms are explicit
    /// inputs, not inferred from the bitmap (§8.4).
    ///
    /// The central-only convenience [`is_empty_central_only`](Self::is_empty_central_only)
    /// is the M1 form (no caches).
    #[inline]
    pub fn is_empty(&self, non_central: NonCentralResidency) -> bool {
        self.live_count() == 0
            && non_central.local_cached == 0
            && non_central.transfer_cached == 0
            && non_central.quarantined == 0
            && self.central_free_count() == self.object_count()
    }

    /// The M1 empty predicate (no caches): `live == 0 && central_free ==
    /// object_count`.
    #[inline]
    pub fn is_empty_central_only(&self) -> bool {
        self.is_empty(NonCentralResidency::NONE)
    }
}

/// A captured `(SpanId, Generation)` snapshot for stale-reference detection (§27.5,
/// W3-5). Code that stashes a descriptor reference across a window where the span
/// could be recycled (sampled/debug paths) captures a `GenGuard` and re-validates
/// with [`matches`](Self::matches); a mismatch means the slot was recycled and the
/// stashed reference is stale.
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
    base: usize,
    /// Usable size in bytes (`>=` the request).
    usable_size: usize,
    /// Required alignment (a power of two).
    align: usize,
    /// Slot identity (immutable; a recycle bumps `generation`).
    id: LargeId,
    /// Owning arena.
    arena: AtomicU32,
    /// Generation (§27.5).
    generation: AtomicU32,
    /// [`LargeState`] as a `u8`.
    state: AtomicU8,
}

impl LargeDescriptor {
    /// Create a descriptor for a live large allocation.
    pub fn new(id: LargeId, arena: ArenaId, base: usize, usable_size: usize, align: usize) -> Self {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        Self {
            base,
            usable_size,
            align,
            id,
            arena: AtomicU32::new(arena.0),
            generation: AtomicU32::new(Generation::FIRST.0),
            state: AtomicU8::new(LargeState::Active as u8),
        }
    }

    /// Slot identity.
    #[inline]
    pub fn id(&self) -> LargeId {
        self.id
    }

    /// Allocation base address.
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// Usable size in bytes.
    #[inline]
    pub fn usable_size(&self) -> usize {
        self.usable_size
    }

    /// Required alignment.
    #[inline]
    pub fn align(&self) -> usize {
        self.align
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
        self.base + self.usable_size
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
}

// SAFETY: every mutable field of `SpanDescriptor` is atomic and `id` is immutable,
// so concurrent `&` access (a classifier through the pagemap plus the owner) is
// data-race-free; it holds no non-atomic interior mutability or thread-affine state.
unsafe impl Sync for SpanDescriptor {}
// SAFETY: as the `Sync` impl — the descriptor is a bag of atomics and immutable
// data, so moving it across threads is sound.
unsafe impl Send for SpanDescriptor {}
// SAFETY: every mutable field of `LargeDescriptor` is atomic and `id`/geometry are
// immutable, so concurrent `&` access is data-race-free.
unsafe impl Sync for LargeDescriptor {}
// SAFETY: as the `Sync` impl above.
unsafe impl Send for LargeDescriptor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ArenaId, LargeId, SizeClassId, SpanId};

    fn span(object_count: u32) -> SpanDescriptor {
        // Class 0 (16-byte), one 16 KiB page, base page-aligned. object0 == base
        // since align (16) divides a page-aligned base and the header is 0.
        SpanDescriptor::new(
            SpanId(1),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            0x4000_0000,
            1,
            object_count,
            0,
        )
    }

    #[test]
    fn bitmap_covers_widest_slab() {
        // The inline bitmap must address every object of the widest class (1024 for
        // the 16-byte class in the shipped table), so no slab outruns it.
        assert!(BITMAP_CAPACITY >= max_objects_per_slab());
        assert_eq!(BITMAP_WORDS, max_objects_per_slab().div_ceil(64));
    }

    #[test]
    fn bitmap_insert_remove_count() {
        let b = FreeBitmap::new();
        assert_eq!(b.count(), 0);
        assert!(b.insert(0));
        assert!(b.insert(63));
        assert!(b.insert(64)); // crosses a word boundary
        assert!(!b.insert(0)); // double insert detected
        assert_eq!(b.count(), 3);
        assert!(b.contains(64));
        assert!(b.remove(64));
        assert!(!b.remove(64)); // already clear
        assert_eq!(b.count(), 2);
    }

    #[test]
    fn bitmap_fill_below_sets_exact_prefix() {
        let b = FreeBitmap::new();
        b.fill_below(70);
        assert_eq!(b.count(), 70);
        for i in 0..70 {
            assert!(b.contains(i), "bit {i} should be set");
        }
        assert!(!b.contains(70));
        b.fill_below(0);
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn descriptor_footprint_is_header_plus_bitmap() {
        // W3-2 acceptance, restated as a runtime check for visibility.
        assert_eq!(
            core::mem::size_of::<SpanDescriptor>(),
            SPAN_DESC_HEADER + core::mem::size_of::<FreeBitmap>()
        );
        // Descriptors are pointed at by the pagemap; their pointers must be at
        // least 8-aligned so the tag bits in a `PageEntry` are free (W3-3b).
        assert!(core::mem::align_of::<SpanDescriptor>() >= 8);
        assert!(core::mem::align_of::<LargeDescriptor>() >= 8);
    }

    #[test]
    fn central_insert_remove_keep_count_and_bitmap_in_lockstep() {
        let s = span(64);
        assert!(s.central_count_matches_bitmap());
        assert!(s.central_insert(3));
        assert!(s.central_insert(10));
        assert_eq!(s.central_free_count(), 2);
        assert!(s.central_count_matches_bitmap());
        // Double insert does not double-count.
        assert!(!s.central_insert(3));
        assert_eq!(s.central_free_count(), 2);
        assert!(s.central_remove(3));
        assert_eq!(s.central_free_count(), 1);
        assert!(s.central_count_matches_bitmap());
    }

    #[test]
    fn conservation_law_central_only_holds() {
        // A 64-object span: carve all into the central list, then "allocate" 20
        // (live) by removing them from central. The central-only law must hold at
        // each step: object_count == live + central_free.
        let s = span(64);
        for i in 0..64 {
            assert!(s.central_insert(i));
        }
        s.set_live_count(0);
        assert!(s.conservation_holds_central_only());
        // All 64 objects central, none live ⇒ the span is empty (§16.5).
        assert!(s.is_empty_central_only());

        // Hand out 20 objects: remove from central, bump live.
        for i in 0..20 {
            assert!(s.central_remove(i));
        }
        s.set_live_count(20);
        assert!(s.conservation_holds_central_only());
        assert_eq!(s.live_count(), 20);
        assert_eq!(s.central_free_count(), 44);
        assert!(!s.is_empty_central_only());
    }

    #[test]
    fn conservation_law_with_reconstructed_cached_terms() {
        // The full §16.4 law with non-zero cached terms (the M2 shape). A 64-object
        // span: 10 live, 4 local-cached, 2 transfer-cached, 1 quarantined ⇒ 47
        // central. The five terms must sum to object_count, and a cached object is
        // NOT read as live (the empty predicate stays false).
        let s = span(64);
        for i in 0..47 {
            assert!(s.central_insert(i));
        }
        s.set_live_count(10);
        let nc = NonCentralResidency {
            local_cached: 4,
            transfer_cached: 2,
            quarantined: 1,
        };
        assert!(s.conservation_holds(nc));
        assert_eq!(s.partition(nc), [10, 4, 2, 47, 1]);
        // Even though only 10 are live and 47 central, the 7 cached/quarantined
        // objects keep the span non-empty (§16.5 must account for all caches).
        assert!(!s.is_empty(nc));
    }

    #[test]
    fn empty_detection_never_reads_a_cached_object_as_live() {
        // The catastrophe guard (DD-3 F2): a span with 0 live and central_free ==
        // object_count looks empty by the central-only test, but if a cache still
        // holds one of its objects the FULL predicate must keep it non-empty.
        let s = span(8);
        // 7 central, 0 live — but 1 object sits in a transfer cache.
        for i in 0..7 {
            assert!(s.central_insert(i));
        }
        s.set_live_count(0);
        let cached = NonCentralResidency {
            transfer_cached: 1,
            ..NonCentralResidency::NONE
        };
        // central_free (7) != object_count (8), and a cache holds the 8th ⇒ not empty.
        assert!(!s.is_empty(cached));
        assert!(s.conservation_holds(cached)); // 0 + 0 + 1 + 7 + 0 == 8 ✓
    }

    #[test]
    fn object0_base_is_base_for_page_aligned_span() {
        let s = span(64);
        // align (16) divides the page-aligned base and header is 0, so object 0
        // starts at the span base (§16.3).
        assert_eq!(s.object0_base(), Some(s.base()));
    }

    #[test]
    fn recycle_bumps_generation_and_resets_accounting() {
        let s = span(64);
        let guard = s.gen_guard();
        assert!(guard.matches(&s));
        for i in 0..64 {
            s.central_insert(i);
        }
        s.set_live_count(0);

        // Recycle the slot for a different class/arena/base.
        s.recycle(ArenaId(2), SizeClassId::new(5), 0x8000_0000, 2, 32, 0);
        // The captured guard is now stale (generation bumped, W3-5).
        assert!(!guard.matches(&s));
        assert_eq!(s.generation(), Generation::FIRST.next());
        assert_eq!(s.arena(), ArenaId(2));
        assert_eq!(s.object_count(), 32);
        assert_eq!(s.central_free_count(), 0);
        assert_eq!(s.live_count(), 0);
        assert!(s.central_count_matches_bitmap());
    }

    #[test]
    fn large_descriptor_records_p_map_004_fields() {
        let d = LargeDescriptor::new(LargeId(7), ArenaId(1), 0x10_0000, 3_000_000, 4096);
        assert_eq!(d.id(), LargeId(7));
        assert_eq!(d.base(), 0x10_0000);
        assert_eq!(d.usable_size(), 3_000_000);
        assert_eq!(d.align(), 4096);
        assert_eq!(d.arena(), ArenaId(1));
        assert_eq!(d.end(), 0x10_0000 + 3_000_000);
        assert_eq!(d.state(), LargeState::Active);
        d.set_state(LargeState::Released);
        assert_eq!(d.state(), LargeState::Released);
    }
}

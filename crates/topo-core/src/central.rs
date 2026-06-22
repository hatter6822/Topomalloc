// SPDX-License-Identifier: MIT
//! Central free list (§14.5, plan 03 W5-4 + W5-5).
//!
//! The central free list is the allocation layer between the size-class classifier
//! (W2) and the caches (plan 05): it owns **partial spans** — spans with
//! central-resident free objects — and serves/absorbs object batches.
//!
//! **Structure (W5-4a).** Keyed `(node, arena, label, sc)`: at M1 the node is
//! `DEFAULT` (single NUMA), label is `PUBLIC` (single authority), and arenas are
//! verified but not sharded. Each key maps to a [`CentralBin`] holding a
//! partial-span list, an empty-span cache (DD-4), occupancy counters, and a
//! per-bin spinlock.
//!
//! **Remove (W5-4b, §A.4/§A.2).** [`CentralCache::remove_batch`] pulls objects from
//! a partial span matching the requested arena (scanning the list). If no partial
//! span is available, it tries the empty-span cache — reactivating a recently
//! emptied span without a backend round-trip. Only if both are exhausted does it
//! return [`RemoveResult::NeedSpan`] so the **caller** creates a new span and
//! retries (§A.2 OOM-retry loop) — span creation stays outside the locked central
//! critical section (DD-4).
//!
//! **Insert (W5-4c).** [`CentralCache::insert_batch`] returns objects, updating bitmap
//! + count atomically (W5-3b), then runs the empty-detection trigger (W5-3e): if the
//!   span is empty, it is removed from the partial list and the caller returns it to
//!   the backend (W5-5). A span that was exhausted (not in the partial list) and now has
//!   central-free objects is re-added.
//!
//! **Lock hierarchy.** The central lock is the **outer** lock; the span lock (§27.2)
//! is the **inner** lock. No code path takes them in reverse order. At M2, W5-4d adds
//! per-`(node, sc)` shards.
//!
//! **Intrusive linking.** Both the partial-span list and the empty-span cache are
//! singly-linked through `SpanDescriptor::central_next`. A span can only be in one
//! list at a time (partial xor empty-cached xor exhausted-but-tracked), so the
//! single pointer suffices. All mutations are under the per-bin central lock.
//!
//! **Span activation / return-to-backend (W5-5).** [`CentralCache::activate_span`]
//! fills the bitmap, installs the span in the pagemap (W3-6), and pushes it to the
//! partial list. [`CentralCache::deactivate_span`] removes it from the partial list,
//! transitions the pagemap, and signals the caller. Neither returns a non-empty span
//! (C-005 acceptance).

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::{ArenaId, Label, NodeId, SizeClassId};
use crate::lock::{LockRank, RankedGuard, RankedLock};
use crate::pagemap::{PageMap, PagemapError};
use crate::slab::SlabLayout;
use crate::span::{NonCentralResidency, SpanDescriptor, SpanState};

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

/// Placement-class sentinel meaning "any class" in [`CentralCache::remove_batch`] (§24, W14):
/// the §24.6–§24.8 span grouping is advisory, so the allocator falls back to this to reuse
/// any partial span before reporting OOM. Distinct from every real
/// [`PlaceClass`](crate::PlaceClass) tag (`0..=3`).
pub const ANY_PLACE_CLASS: u8 = u8::MAX;

/// Maximum batch size across all size classes (the largest `batch` field).
pub const MAX_BATCH_LEN: usize = max_batch_in_table();

const fn max_batch_in_table() -> usize {
    let mut max = 0usize;
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        let b = SIZE_CLASSES[i].batch as usize;
        if b > max {
            max = b;
        }
        i += 1;
    }
    max
}

// Object indices are stored as u16 in Batch::indices and central_remove_batch.
// This compile-time guard prevents silent truncation if a future table revision
// pushes objects_per_slab past 65535.
const _: () = assert!(
    crate::span::max_objects_per_slab() <= u16::MAX as usize,
    "objects_per_slab exceeds u16::MAX; Batch indices would truncate"
);

/// Maximum empty spans cached per bin (LIFO, bounded). Avoids a backend
/// round-trip when a recently-emptied span can be reused immediately.
const MAX_EMPTY_CACHED_PER_BIN: u32 = 1;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// An error from a central-list operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CentralError {
    /// A pagemap node could not be allocated (the span is not activated).
    OutOfMetadata,
}

impl From<PagemapError> for CentralError {
    fn from(e: PagemapError) -> Self {
        match e {
            PagemapError::OutOfMetadata => CentralError::OutOfMetadata,
        }
    }
}

// ---------------------------------------------------------------------------
// Batch
// ---------------------------------------------------------------------------

/// A batch of object indices from a single span (§A.4, W5-4a). All objects
/// belong to the same `(arena, label, sc)`. The indices are 0-based positions
/// within the span's slab; convert to addresses with [`SlabLayout::object_addr`].
pub struct Batch {
    /// The span these objects belong to.
    span: *const SpanDescriptor,
    /// Object indices within the span.
    indices: [u16; MAX_BATCH_LEN],
    /// Number of valid entries in `indices`.
    len: u16,
}

impl Batch {
    /// An empty batch for the given span.
    #[inline]
    fn empty(span: *const SpanDescriptor) -> Self {
        Self {
            span,
            indices: [0; MAX_BATCH_LEN],
            len: 0,
        }
    }

    /// The span the batch's objects belong to.
    #[inline]
    pub fn span(&self) -> *const SpanDescriptor {
        self.span
    }

    /// Number of objects in the batch.
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the batch is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Object index at position `i`. Panics if `i >= len()`.
    #[inline]
    pub fn index(&self, i: usize) -> u16 {
        self.indices()[i]
    }

    /// The valid indices as a slice.
    #[inline]
    pub fn indices(&self) -> &[u16] {
        &self.indices[..self.len as usize]
    }

    /// Convert batch index `i` to its address using the given slab layout.
    #[inline]
    pub fn object_addr(&self, i: usize, layout: &SlabLayout) -> Option<usize> {
        if i >= self.len as usize {
            return None;
        }
        layout.object_addr(self.indices[i] as usize)
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Result of [`CentralCache::remove_batch`].
pub enum RemoveResult {
    /// A batch of objects was successfully removed.
    Ok(Batch),
    /// No partial spans available for this `(node, arena, label, sc)`. The
    /// caller should create a new span from the backend, activate it with
    /// [`CentralCache::activate_span`] (W5-5), and retry (§A.2 OOM-retry).
    NeedSpan,
}

/// Result of [`CentralCache::insert_batch`].
#[derive(Clone, Copy, Debug)]
pub struct InsertResult {
    /// Number of objects successfully inserted.
    pub inserted: usize,
    /// Whether the span is empty **and** the empty-span cache is full, so the
    /// caller must call [`CentralCache::deactivate_span`] (W5-5, C-003). When
    /// the span is empty but could be cached internally for reuse, this is
    /// `false` — the central list handled the transition.
    pub span_empty: bool,
}

// ---------------------------------------------------------------------------
// CentralLock — per-bin spinlock (§27.2, rank `CENTRAL`)
// ---------------------------------------------------------------------------

/// The per-bin central lock is rank [`LockRank::CENTRAL`] in the §27.2 hierarchy:
/// outer to the per-span lock (this bin's batch ops take a span's lock while
/// holding it), inner to transfer (hand-over-hand) and the arena locks. Routed
/// through [`RankedLock`] so the W16-1b checker sees every acquisition.
type CentralLock = RankedLock<{ LockRank::CENTRAL }>;
type CentralGuard<'a> = RankedGuard<'a, { LockRank::CENTRAL }>;

// ---------------------------------------------------------------------------
// CentralBin — per-sc structure (W5-4a)
// ---------------------------------------------------------------------------

/// Per-size-class central structure (W5-4a): a partial-span list, an
/// empty-span cache, occupancy counters, and a lock (DD-4). At M1 this is the
/// only allocation path; at M2, caches drain/refill through it.
pub struct CentralBin {
    /// Spinlock protecting all mutable state below.
    lock: CentralLock,
    /// Head of the partial-span singly-linked list (null when empty). Spans
    /// in this list have at least one central-free object (`central_free > 0`).
    partial_head: AtomicPtr<SpanDescriptor>,
    /// Number of spans in the partial list.
    partial_count: AtomicU32,
    /// Head of the empty-span LIFO cache (null when empty). Spans here have
    /// been fully returned (`is_empty() == true`, bitmap full) but not yet
    /// deactivated. They can be reused immediately, avoiding a backend
    /// round-trip. Bounded by [`MAX_EMPTY_CACHED_PER_BIN`].
    empty_head: AtomicPtr<SpanDescriptor>,
    /// Number of spans in the empty cache.
    empty_count: AtomicU32,
    /// Number of active spans (partial + exhausted + empty-cached) tracked
    /// by this bin.
    span_count: AtomicU32,
    /// Sum of `central_free_count()` across every span tracked by this bin
    /// (partial-list + empty-cache + exhausted). Adjusted by `activate_span`
    /// (+object_count), `remove_batch` (-removed), `insert_batch` (+inserted),
    /// and `deactivate_span` (-free_before).
    total_central_free: AtomicU64,
}

impl CentralBin {
    const fn new() -> Self {
        Self {
            lock: CentralLock::new(),
            partial_head: AtomicPtr::new(core::ptr::null_mut()),
            partial_count: AtomicU32::new(0),
            empty_head: AtomicPtr::new(core::ptr::null_mut()),
            empty_count: AtomicU32::new(0),
            span_count: AtomicU32::new(0),
            total_central_free: AtomicU64::new(0),
        }
    }

    /// Acquire the bin's lock.
    #[inline]
    fn lock(&self) -> CentralGuard<'_> {
        self.lock.lock()
    }

    // --- partial-list operations (all under the bin lock) --------------------

    /// Push `span` to the head of the partial list.
    fn push_partial(&self, span: &SpanDescriptor) {
        let old_head = self.partial_head.load(Ordering::Relaxed);
        span.set_central_next(old_head);
        self.partial_head.store(
            span as *const SpanDescriptor as *mut SpanDescriptor,
            Ordering::Relaxed,
        );
        self.partial_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Pop the head of the partial list. Returns null if empty.
    fn pop_partial(&self) -> *const SpanDescriptor {
        let head = self.partial_head.load(Ordering::Relaxed);
        if head.is_null() {
            return core::ptr::null();
        }
        // SAFETY: head was installed by push_partial from a valid &SpanDescriptor;
        // metadata is never freed (§27.5).
        let span = unsafe { &*head };
        let next = span.central_next_ptr();
        span.set_central_next(core::ptr::null_mut());
        self.partial_head.store(next, Ordering::Relaxed);
        self.partial_count.fetch_sub(1, Ordering::Relaxed);
        head
    }

    /// Remove `target` from the partial list. Returns `true` if found.
    fn remove_partial(&self, target: *const SpanDescriptor) -> bool {
        let head = self.partial_head.load(Ordering::Relaxed);
        if head.is_null() {
            return false;
        }
        // Head match — pop.
        if core::ptr::eq(head, target) {
            self.pop_partial();
            return true;
        }
        // Scan for the predecessor, bounded to guard against a corrupted cycle.
        let budget = self.partial_count.load(Ordering::Relaxed) as usize;
        let mut prev = head;
        for _ in 0..budget {
            // SAFETY: prev was installed from a valid &SpanDescriptor; metadata
            // is never freed.
            let prev_span = unsafe { &*prev };
            let next = prev_span.central_next_ptr();
            if next.is_null() {
                return false;
            }
            if core::ptr::eq(next, target) {
                // SAFETY: target is a valid &SpanDescriptor.
                let target_span = unsafe { &*target };
                let after = target_span.central_next_ptr();
                prev_span.set_central_next(after);
                target_span.set_central_next(core::ptr::null_mut());
                self.partial_count.fetch_sub(1, Ordering::Relaxed);
                return true;
            }
            prev = next;
        }
        false
    }

    // --- empty-cache operations (all under the bin lock) ----------------------

    /// Push `span` to the head of the empty cache.
    fn push_empty(&self, span: &SpanDescriptor) {
        let old_head = self.empty_head.load(Ordering::Relaxed);
        span.set_central_next(old_head);
        self.empty_head.store(
            span as *const SpanDescriptor as *mut SpanDescriptor,
            Ordering::Relaxed,
        );
        self.empty_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Pop the head of the empty cache. Returns null if empty.
    fn pop_empty(&self) -> *const SpanDescriptor {
        let head = self.empty_head.load(Ordering::Relaxed);
        if head.is_null() {
            return core::ptr::null();
        }
        // SAFETY: head was installed by push_empty from a valid &SpanDescriptor;
        // metadata is never freed (§27.5).
        let span = unsafe { &*head };
        let next = span.central_next_ptr();
        span.set_central_next(core::ptr::null_mut());
        self.empty_head.store(next, Ordering::Relaxed);
        self.empty_count.fetch_sub(1, Ordering::Relaxed);
        head
    }

    /// Remove `target` from the empty cache. Returns `true` if found.
    fn remove_empty(&self, target: *const SpanDescriptor) -> bool {
        let head = self.empty_head.load(Ordering::Relaxed);
        if head.is_null() {
            return false;
        }
        if core::ptr::eq(head, target) {
            self.pop_empty();
            return true;
        }
        let budget = self.empty_count.load(Ordering::Relaxed) as usize;
        let mut prev = head;
        for _ in 0..budget {
            // SAFETY: prev was installed from a valid &SpanDescriptor; metadata
            // is never freed.
            let prev_span = unsafe { &*prev };
            let next = prev_span.central_next_ptr();
            if next.is_null() {
                return false;
            }
            if core::ptr::eq(next, target) {
                // SAFETY: target is a valid &SpanDescriptor.
                let target_span = unsafe { &*target };
                let after = target_span.central_next_ptr();
                prev_span.set_central_next(after);
                target_span.set_central_next(core::ptr::null_mut());
                self.empty_count.fetch_sub(1, Ordering::Relaxed);
                return true;
            }
            prev = next;
        }
        false
    }

    // --- accessors (relaxed reads, no lock needed) ---------------------------

    /// Number of partial spans (approximate without the lock).
    #[inline]
    pub fn partial_count(&self) -> u32 {
        self.partial_count.load(Ordering::Relaxed)
    }

    /// Number of empty-cached spans (approximate without the lock).
    #[inline]
    pub fn empty_count(&self) -> u32 {
        self.empty_count.load(Ordering::Relaxed)
    }

    /// Number of active spans tracked by this bin.
    #[inline]
    pub fn span_count(&self) -> u32 {
        self.span_count.load(Ordering::Relaxed)
    }

    /// Total central-free objects across all spans (approximate).
    #[inline]
    pub fn total_central_free(&self) -> u64 {
        self.total_central_free.load(Ordering::Relaxed)
    }

    /// Test-only: force the tracked `span_count` to an arbitrary value, to
    /// construct a deliberately-inconsistent bin and prove
    /// [`check_invariants`](Self::check_invariants) catches the miscount (W19-1
    /// negative test). Never compiled into a shipping build.
    #[cfg(test)]
    pub(crate) fn corrupt_span_count_for_test(&self, n: u32) {
        self.span_count.store(n, Ordering::Relaxed);
    }

    /// Appendix B.1/B.3 (free-structure reachability + empty-detection across
    /// caches, W19-1a/c): this bin's span lists are well-formed and its accounting
    /// reconciles. Acquires the bin lock for a consistent snapshot; for each
    /// tracked span it then takes the span lock (rank `SPAN` > `CENTRAL`, the
    /// §27.2 order, so this is deadlock-free when called from an unlocked context).
    /// Total + side-effect-free.
    ///
    /// Mapped to Appendix B:
    /// * **B.1 "every free object is reachable from exactly one free structure"** —
    ///   the partial and empty lists are acyclic (a bounded walk reaches null in
    ///   exactly `partial_count` / `empty_count` steps), and `Σ central_free` over
    ///   the two lists equals `total_central_free` (an exhausted span — tracked but
    ///   in neither list — contributes 0), so every central-free object is reached
    ///   from exactly one structure, none unreachable or double-counted;
    /// * **B.3 "span free count equals authoritative representation"** — every
    ///   listed span passes [`SpanDescriptor::check_invariants_locked`];
    /// * **B.3 "empty-span detection accounts for … central"** — a span on the
    ///   empty list is central-empty (`central_free == object_count`, `live == 0`)
    ///   and one on the partial list has `central_free > 0`, so emptiness is
    ///   classified consistently with central residency;
    /// * **B.3 "span size class matches all contained objects"** — every listed
    ///   span's class equals `sc` (the bins are an `sc`-indexed array, so the bin
    ///   does not store its own class — the caller supplies it).
    ///
    /// Returns `false` on any cycle, miscount, class mismatch, malformed span, or
    /// accounting drift.
    pub fn check_invariants(&self, sc: SizeClassId) -> bool {
        let _guard = self.lock();
        let partial_count = self.partial_count.load(Ordering::Relaxed) as usize;
        let empty_count = self.empty_count.load(Ordering::Relaxed) as usize;
        let span_count = self.span_count.load(Ordering::Relaxed) as usize;
        let total_free = self.total_central_free.load(Ordering::Relaxed);

        // The reachable lists hold at most `span_count` spans; the rest are
        // exhausted spans (central_free == 0), tracked but in neither list.
        if partial_count + empty_count > span_count {
            return false;
        }

        let mut sum_free: u64 = 0;

        // --- partial list: acyclic, `central_free > 0`, well-formed, right class.
        let mut p = self.partial_head.load(Ordering::Relaxed);
        let mut seen = 0usize;
        while !p.is_null() {
            if seen >= partial_count {
                // More nodes than the counter admits ⇒ a cycle or a miscount.
                return false;
            }
            // SAFETY: list pointers are installed from valid `&SpanDescriptor` in
            // monotonic metadata (never freed, §27.5); the bin lock pins list
            // membership for the walk's duration.
            let span = unsafe { &*p };
            if span.size_class() != sc {
                return false;
            }
            let next = span.central_next_ptr();
            let g = span.lock();
            let cf = g.central_free_count();
            let ok = cf > 0 && span.check_invariants_locked(&g);
            drop(g);
            if !ok {
                return false;
            }
            sum_free += cf as u64;
            p = next;
            seen += 1;
        }
        if seen != partial_count {
            return false;
        }

        // --- empty list: acyclic, central-empty, well-formed, right class.
        let mut e = self.empty_head.load(Ordering::Relaxed);
        let mut seen_e = 0usize;
        while !e.is_null() {
            if seen_e >= empty_count {
                return false;
            }
            // SAFETY: as the partial walk above.
            let span = unsafe { &*e };
            if span.size_class() != sc {
                return false;
            }
            let next = span.central_next_ptr();
            let g = span.lock();
            let cf = g.central_free_count();
            let ok = span.check_invariants_locked(&g)
                && g.live_count() == 0
                && cf == span.object_count();
            drop(g);
            if !ok {
                return false;
            }
            sum_free += cf as u64;
            e = next;
            seen_e += 1;
        }
        if seen_e != empty_count {
            return false;
        }

        // B.1: Σ central_free over the reachable lists equals the bin aggregate.
        sum_free == total_free
    }

    /// Appendix B (§30.2 *"redzones are intact"*, W19-1c): under junk-fill, every
    /// central-free object in **every span of this bin** still reads as
    /// `harden::FREE_PATTERN` (§29.6). Total + side-effect-free; a no-op returning
    /// `true` when junk-fill is compiled out (no span lock taken, no memory read).
    ///
    /// **Reads object backing**, so it is reached only from
    /// [`Allocator::check_invariants`](crate::Allocator::check_invariants) — where
    /// every central span is backed — never from a synthetic fake-base span test,
    /// which is why it is deliberately *not* folded into
    /// [`check_invariants`](Self::check_invariants) (that runs on fake-base spans).
    /// Walks the partial and empty lists (both can hold central-free objects); the
    /// per-list count bounds the walk (cycle/miscount safe, as in `check_invariants`).
    pub fn verify_free_patterns(&self) -> bool {
        if !crate::harden::junk_fill_enabled() {
            return true;
        }
        let _guard = self.lock();
        for (head, count) in [
            (
                self.partial_head.load(Ordering::Relaxed),
                self.partial_count.load(Ordering::Relaxed) as usize,
            ),
            (
                self.empty_head.load(Ordering::Relaxed),
                self.empty_count.load(Ordering::Relaxed) as usize,
            ),
        ] {
            let mut p = head;
            let mut seen = 0usize;
            while !p.is_null() {
                if seen >= count {
                    return false; // a cycle/miscount (also caught by check_invariants)
                }
                // SAFETY: list pointers reach valid `&SpanDescriptor` in monotonic
                // metadata (never freed, §27.5); the bin lock pins membership.
                let span = unsafe { &*p };
                let next = span.central_next_ptr();
                let g = span.lock();
                let ok = g.verify_free_patterns();
                drop(g);
                if !ok {
                    return false;
                }
                p = next;
                seen += 1;
            }
        }
        true
    }
}

// SAFETY: all shared state is behind atomics or the spinlock; the raw pointers
// in both the partial and empty lists reach `Sync` descriptors in monotonic
// metadata (never freed, §27.5).
unsafe impl Sync for CentralBin {}
// SAFETY: CentralBin fields are atomics and a spinlock; the AtomicPtrs
// (partial_head, empty_head) reach Sync descriptors that are never freed.
unsafe impl Send for CentralBin {}

// ---------------------------------------------------------------------------
// CentralCache — the top-level container
// ---------------------------------------------------------------------------

/// The top-level central free list cache (§14.5, W5-4a). Contains a
/// [`CentralBin`] per size class; at M1 the `(node, arena, label)` key
/// collapses to `(DEFAULT, DEFAULT, PUBLIC)`.
pub struct CentralCache {
    /// One bin per size class. At M1 the full key is `(node, arena, label, sc)`
    /// but `node` is DEFAULT (single NUMA), `label` is PUBLIC, and `arena` is
    /// filtered at lookup time. **M2 action (W5-4d):** expand to a
    /// `[[CentralBin; NUM_SIZE_CLASSES]; NUM_NODES]` or equivalent per-node
    /// sharding to reduce cross-NUMA contention.
    bins: [CentralBin; NUM_SIZE_CLASSES],
}

impl CentralCache {
    /// A fresh, empty central cache (no spans, no objects).
    pub const fn new() -> Self {
        Self {
            bins: [const { CentralBin::new() }; NUM_SIZE_CLASSES],
        }
    }

    /// The bin for a size class (bounds-checked).
    #[inline]
    pub fn bin(&self, sc: SizeClassId) -> Option<&CentralBin> {
        self.bins.get(sc.index())
    }

    /// Appendix B.1/B.3 (W19-1a/c): **every** central bin is well-formed — its
    /// span lists are acyclic and reachable, its accounting reconciles, and every
    /// listed span is internally consistent (see [`CentralBin::check_invariants`]).
    /// Total + side-effect-free; the `debug-checks`/test oracle for the central
    /// free-list layer, and the runtime counterpart of the plan 03 W5-3
    /// conservation law and the W21-2 differential test.
    pub fn check_invariants(&self) -> bool {
        self.bins
            .iter()
            .enumerate()
            .all(|(i, bin)| bin.check_invariants(SizeClassId::new(i)))
    }

    /// Appendix B (§30.2 "redzones are intact", W19-1c): the free-pattern sweep over
    /// every bin — under junk-fill, every central-free object reads as
    /// `harden::FREE_PATTERN`. See [`CentralBin::verify_free_patterns`]. A no-op when
    /// junk-fill is off. Reads object backing, so call only over a backed engine
    /// (from [`Allocator::check_invariants`](crate::Allocator::check_invariants)).
    pub fn verify_free_patterns(&self) -> bool {
        self.bins.iter().all(|bin| bin.verify_free_patterns())
    }

    // --- remove batch (W5-4b) ------------------------------------------------

    /// Remove up to `desired` objects of size class `sc` (§A.4, W5-4b), preferring a span of
    /// the requested **placement class** `place_class` (§24.6–§24.8, W14) so cold / hot /
    /// short-lived small objects cluster; pass [`ANY_PLACE_CLASS`] to ignore the class
    /// (the availability fallback).
    ///
    /// Tries, in order: (1) a partial span matching the arena **and** `place_class`,
    /// (2) an empty-cached span (re-tagged to `place_class`), (3) returns
    /// [`RemoveResult::NeedSpan`] so the caller can get a new span from the
    /// backend, [`activate_span`](Self::activate_span) it, and retry (§A.2).
    ///
    /// Class grouping is **advisory** (§24.5): it changes only *which* span an object is
    /// carved from, never the object's size/alignment/validity. A class never causes a
    /// spurious failure — the caller's [`ANY_PLACE_CLASS`] fallback reuses any partial before
    /// reporting OOM.
    ///
    /// **C-001/C-002:** the returned batch is single-arena, single-label,
    /// correct-size (every object comes from one span of the right class).
    ///
    /// Lock order: `central_lock → span_lock` (W5-4d).
    pub fn remove_batch(
        &self,
        _node: NodeId,
        arena: ArenaId,
        label: Label,
        sc: SizeClassId,
        place_class: u8,
        desired: usize,
    ) -> RemoveResult {
        // C-002: at M1 only Label::PUBLIC is supported. Per-label partitioning
        // arrives with plan 09 (seLe4n, M2+).
        debug_assert_eq!(
            label,
            Label::PUBLIC,
            "M1: only Label::PUBLIC is supported; per-label bins arrive at M2"
        );

        let bin = match self.bins.get(sc.index()) {
            Some(b) => b,
            None => return RemoveResult::NeedSpan,
        };
        let _guard = bin.lock();

        // --- step 1: find a partial span matching the arena and placement class -
        let span_ptr = self.find_partial_for_arena(bin, arena, place_class);

        // --- step 2: if no partial match, try the empty cache ----------------
        let span_ptr = if span_ptr.is_null() {
            // §30.4 (W19-3): deterministic mode can force the slow path — decline
            // the empty-span cache reuse so the allocation takes the full
            // backend/activation round-trip (a `NeedSpan` to the caller). The
            // empty span stays cached and correctly accounted; only *this*
            // request skips the fast reuse. Off by default (one relaxed load).
            if crate::deterministic::force_slow_path() {
                return RemoveResult::NeedSpan;
            }
            let empty = bin.pop_empty();
            if empty.is_null() {
                return RemoveResult::NeedSpan;
            }
            // SAFETY: empty was installed by push_empty from a valid
            // &SpanDescriptor; metadata is never freed (§27.5).
            let empty_span = unsafe { &*empty };
            if empty_span.arena() != arena {
                // Wrong arena — put it back (M1: can't happen).
                bin.push_empty(empty_span);
                return RemoveResult::NeedSpan;
            }
            // Re-tag the empty span (no live objects ⇒ safe) to the requested class, so it
            // joins the right grouping pool (§24.6–§24.8). `ANY_PLACE_CLASS` leaves the tag.
            if place_class != ANY_PLACE_CLASS {
                empty_span.set_place_class(place_class);
            }
            // The empty span has a full bitmap (all central-free). Push to
            // partial so the carve logic below can process it uniformly.
            bin.push_partial(empty_span);
            empty
        } else {
            span_ptr
        };

        // --- step 3: carve from the span ------------------------------------
        // SAFETY: span_ptr came from a list installed from a valid &SpanDescriptor.
        let span = unsafe { &*span_ptr };
        let sg = span.lock();

        // Defensive: if the span is unexpectedly exhausted, pop and report empty.
        if sg.central_free_count() == 0 {
            drop(sg);
            bin.pop_partial();
            return RemoveResult::NeedSpan;
        }

        let count = desired
            .min(sg.central_free_count() as usize)
            .min(MAX_BATCH_LEN);

        let mut batch = Batch::empty(span_ptr);
        let removed = sg.central_remove_batch(&mut batch.indices, count);
        batch.len = removed as u16;

        let old_live = sg.live_count();
        let removed_u32 = removed as u32;
        debug_assert!(
            old_live.checked_add(removed_u32).is_some(),
            "live_count overflow: {old_live} + {removed_u32} exceeds u32"
        );
        sg.set_live_count(old_live + removed_u32);

        // B.3 (W19-1c): the span is internally well-formed after the carve —
        // `central_free == popcount(bitmap)`, the §16.4 partition bound, the slab
        // geometry, and (hardened) the integrity tag. Run as a runtime assertion at
        // this transition (debug/`debug-checks`), under the held span lock so it
        // neither re-locks nor races (the extent/huge-checker pattern).
        debug_assert!(span.check_invariants_locked(&sg));

        if sg.central_free_count() == 0 {
            drop(sg);
            bin.pop_partial();
        } else {
            drop(sg);
        }

        bin.total_central_free
            .fetch_sub(removed as u64, Ordering::Relaxed);

        if batch.is_empty() {
            RemoveResult::NeedSpan
        } else {
            RemoveResult::Ok(batch)
        }
    }

    /// Scan the partial list for the first span matching `arena` **and** `place_class`
    /// (`ANY_PLACE_CLASS` matches any class — the §24 grouping is advisory). Returns a
    /// pointer to it (leaving it in the list), or null if none matches. The matching span is
    /// moved to the head for efficient future access.
    fn find_partial_for_arena(
        &self,
        bin: &CentralBin,
        arena: ArenaId,
        place_class: u8,
    ) -> *const SpanDescriptor {
        let matches = |s: &SpanDescriptor| {
            s.arena() == arena && (place_class == ANY_PLACE_CLASS || s.place_class() == place_class)
        };
        let head = bin.partial_head.load(Ordering::Relaxed);
        if head.is_null() {
            return core::ptr::null();
        }

        // Fast path: head matches.
        // SAFETY: head was installed from a valid &SpanDescriptor; metadata
        // is never freed (§27.5).
        let head_span = unsafe { &*head };
        if matches(head_span) {
            return head;
        }

        // Slow path: scan the rest of the list. If found, move the matching
        // span to the head (so subsequent removes hit the fast path).
        // Bounded to partial_count to guard against a corrupted cycle.
        let budget = bin.partial_count.load(Ordering::Relaxed) as usize;
        let mut prev = head;
        for _ in 0..budget {
            // SAFETY: `prev` came from the partial list, which only stores
            // pointers installed from valid &SpanDescriptor references;
            // descriptors are never freed (§27.5 monotonic metadata).
            let prev_span = unsafe { &*prev };
            let next = prev_span.central_next_ptr();
            if next.is_null() {
                return core::ptr::null();
            }
            // SAFETY: same invariant — `next` was written by set_central_next
            // from a valid descriptor pointer.
            let next_span = unsafe { &*next };
            if matches(next_span) {
                // Unlink `next` from its current position.
                let after = next_span.central_next_ptr();
                prev_span.set_central_next(after);
                // Push it to the head.
                next_span.set_central_next(head);
                bin.partial_head.store(next, Ordering::Relaxed);
                // partial_count is unchanged (same number of spans).
                return next;
            }
            prev = next;
        }
        core::ptr::null()
    }

    // --- insert batch (W5-4c) ------------------------------------------------

    /// Return objects to the central free list (W5-4c). The objects (given by
    /// `indices[..count]`) are inserted into `span`'s bitmap + count atomically
    /// (W5-3b), and the live count is decremented. Empty detection (W5-3d/3e)
    /// runs after the insert: if the span is now empty, it is moved to the
    /// empty-span cache if room exists; otherwise [`InsertResult::span_empty`]
    /// is `true` and the caller should [`deactivate_span`](Self::deactivate_span).
    ///
    /// **C-003/C-004:** an empty span is detected; a non-empty span is never
    /// returned.
    ///
    /// Lock order: `central_lock → span_lock` (W5-4d).
    ///
    /// This is the no-scrub form (no backing writes), used by the rollback paths
    /// and the synthetic-address unit tests; the live free path uses
    /// [`insert_batch_scrubbing`](Self::insert_batch_scrubbing).
    pub fn insert_batch(
        &self,
        span: &SpanDescriptor,
        indices: &[u16],
        count: usize,
    ) -> InsertResult {
        self.insert_batch_inner(span, indices, count, None)
    }

    /// Like [`insert_batch`](Self::insert_batch) but, in `junk-fill` builds, scrubs
    /// each object that *genuinely* transitions live→central-free to the
    /// [`FREE_PATTERN`](crate::harden::FREE_PATTERN) **under the span lock** (§29.6,
    /// W18-5) — the unique race-free point, since a concurrent `remove_batch` reuse
    /// and the span's retirement take this same lock. Scrubbing under the lock and
    /// only on the real 0→1 bit flip is what makes it sound: a double/UAF free never
    /// scrubs a *different* owner's live object, and a concurrent retire cannot
    /// decommit the slab out from under the scrub (W18-2: detection/scrub never
    /// corrupts unrelated state; never faults on a clean double free). The scrub
    /// also re-arms the use-after-free canary the next hand-out verifies.
    ///
    /// The span geometry is read from `span` itself (its base was exposed from the
    /// provider mapping), so this MUST be called only with a span over real backing
    /// (the live allocator path); the geometry-only unit tests use
    /// [`insert_batch`](Self::insert_batch). A true no-op (identical to
    /// `insert_batch`) unless `junk-fill` is compiled in.
    pub fn insert_batch_scrubbing(
        &self,
        span: &SpanDescriptor,
        indices: &[u16],
        count: usize,
    ) -> InsertResult {
        let scrub = if crate::harden::junk_fill_enabled() {
            let sc = span.size_class();
            SlabLayout::compute(sc, span.base(), span.slab_header() as usize)
                .map(|layout| (layout, crate::size_class::usable_size(sc)))
        } else {
            None
        };
        self.insert_batch_inner(span, indices, count, scrub)
    }

    /// Shared body of [`insert_batch`](Self::insert_batch) /
    /// [`insert_batch_scrubbing`](Self::insert_batch_scrubbing). `scrub`, when
    /// `Some((layout, obj_size))`, scrubs each genuinely-inserted object's
    /// `obj_size` usable bytes under the span lock (§29.6, W18-5).
    fn insert_batch_inner(
        &self,
        span: &SpanDescriptor,
        indices: &[u16],
        count: usize,
        scrub: Option<(SlabLayout, usize)>,
    ) -> InsertResult {
        let sc = span.size_class();
        let bin = match self.bins.get(sc.index()) {
            Some(b) => b,
            None => {
                return InsertResult {
                    inserted: 0,
                    span_empty: false,
                };
            }
        };
        let _guard = bin.lock();
        let sg = span.lock();

        // W8 free-path hardening: a span that is no longer `Active` was
        // deactivated (its bitmap cleared) by the thread that emptied it. A
        // stale/double free racing past that point would otherwise "insert"
        // into the cleared bitmap and then underflow `live_count` (0 - 1) —
        // silent accounting corruption in release mode. `deactivate_span`
        // writes the state under this same bin lock, so the check is race-free
        // (C-004); the stale free is rejected with nothing inserted.
        if span.state() != SpanState::Active {
            debug_assert!(
                false,
                "insert_batch: span {} is not Active (stale/double free raced deactivation)",
                span.id().0
            );
            return InsertResult {
                inserted: 0,
                span_empty: false,
            };
        }

        // Insert objects into the bitmap (§8.5: bitmap + count move together).
        let max = count.min(indices.len());
        let mut inserted = 0u32;
        for &obj_idx in &indices[..max] {
            let idx = obj_idx as usize;
            if sg.central_insert(idx) {
                inserted += 1;
                // W18-5 (§29.6): scrub this object now that its bit has flipped
                // live→free under the span lock (no-op unless `junk-fill` + a real
                // span geometry were supplied by `insert_batch_scrubbing`).
                if let Some((layout, obj_size)) = scrub {
                    if let Some(addr) = layout.object_addr(idx) {
                        // SAFETY: `addr` is the exposed-provenance address of object
                        // `idx`'s `obj_size` usable bytes in this span's committed slab
                        // (the arithmetic `object_ptr` uses). The span lock is held and
                        // the object just flipped live→free, so no concurrent reuse or
                        // retire can observe a torn scrub or decommit it underneath us.
                        unsafe {
                            crate::harden::fill_on_free(addr as *mut u8, obj_size);
                        }
                    }
                }
                // W18-3 (§29.4): complete a drain's `quarantined → central-free`
                // transition by clearing the quarantined bit **after** the free bit was
                // set above (free-bit-first), so a concurrent lock-free
                // `is_quarantined || is_central_free` double-free check always observes
                // at least one bit set. A no-op for a normal (never-quarantined) free.
                #[cfg(feature = "quarantine")]
                sg.clear_quarantined(idx);
            } else {
                // central_insert returns false for two reasons:
                //   (a) idx >= object_count (out-of-range index), or
                //   (b) the bit was already set (double-free / double-insert).
                // Release-mode double-free detection is plan 08 W18-2. At M1
                // this is debug-only; the hardened profile will upgrade this to
                // a recorded event with diagnostic context.
                debug_assert!(
                    false,
                    "insert_batch: object {idx} rejected \
                     (out-of-range or already central-free, object_count={})",
                    span.object_count()
                );
            }
        }

        // Transition from live to central-free.
        if inserted > 0 {
            let old_live = sg.live_count();
            debug_assert!(
                old_live >= inserted,
                "live_count underflow: {old_live} < {inserted}"
            );
            sg.set_live_count(old_live - inserted);

            bin.total_central_free
                .fetch_add(inserted as u64, Ordering::Relaxed);
        }

        // B.3 (W19-1c): the span is well-formed after the central insert (see
        // `remove_batch`); a runtime assertion at this transition, under the lock.
        debug_assert!(span.check_invariants_locked(&sg));

        // W5-3e trigger: was this span exhausted (not in the partial list)?
        // If it now has central-free objects, add it back.
        //
        // `central_free_count() == inserted` means the span had zero central-free
        // objects before this insert — i.e., it was removed from the partial list
        // by remove_batch when it was fully drained.  A span with 0 central-free
        // cannot remain on the partial list (remove_batch pops it on exhaustion),
        // so the head-pointer check is defence-in-depth, not the primary guard.
        let was_exhausted = {
            let head = bin.partial_head.load(Ordering::Relaxed);
            sg.central_free_count() == inserted && inserted > 0 && !core::ptr::eq(head, span)
        };

        // W5-3d/3e: empty detection trigger.  This is one of two trigger
        // points in the W5-3e protocol:
        //   1. central insert (here) — fires on every batch return.
        //   2. cache drain (M2, plan 05) — fires on idle-CPU flush, thread
        //      exit, arena reset.  Not yet implemented; no caches exist at M1.
        // M2 action: replace NONE with span.reconstruct_non_central_residency().
        let is_empty = sg.is_empty(NonCentralResidency::NONE);
        drop(sg);

        let mut caller_must_deactivate = false;

        // The empty-span transition is owned by the insert that *made* the span
        // empty (`inserted > 0`). A racing stale/double free of an already-empty
        // span inserts nothing (every bit is already set), and MUST NOT re-link
        // the span or claim the deactivation: the span became empty exactly once,
        // so exactly one caller — the one whose insert completed it — is told to
        // deactivate (W5-5 exclusivity; a second deactivator would corrupt
        // `span_count` and re-release retired pagemap entries).
        if is_empty && inserted > 0 {
            // C-003: the span is empty. Remove from partial list if present.
            bin.remove_partial(span as *const SpanDescriptor);

            // Try to cache the empty span for reuse (DD-4 SpanCache).
            if bin.empty_count.load(Ordering::Relaxed) < MAX_EMPTY_CACHED_PER_BIN {
                bin.push_empty(span);
            } else {
                // Cache full — the caller must deactivate.
                caller_must_deactivate = true;
            }
        } else if was_exhausted {
            bin.push_partial(span);
        }

        InsertResult {
            inserted: inserted as usize,
            span_empty: caller_must_deactivate,
        }
    }

    // --- span activation (W5-5) -----------------------------------------------

    /// Activate a new span in the central list (W5-5): fill the bitmap so all
    /// objects are central-free, install the span in the pagemap (W3-6), and
    /// push it to the partial list. The span transitions to `Active` with
    /// `live == 0` and `central_free == object_count`.
    ///
    /// Returns [`CentralError::OutOfMetadata`] if the pagemap install fails.
    /// On failure the span is left with a cleared bitmap (safe failure, no
    /// partial state).
    ///
    /// SPEC-transition: span activation (§14.6, C-005)
    pub fn activate_span(
        &self,
        span: &SpanDescriptor,
        pagemap: &PageMap,
        meta: &dyn MetadataAlloc,
    ) -> Result<(), CentralError> {
        let sc = span.size_class();
        let object_count = span.object_count();

        // Step 1: fill bitmap + set counts under the span lock.
        {
            let sg = span.lock();
            debug_assert!(
                sg.live_count() == 0 && sg.central_free_count() == 0,
                "activate_span: span already has live or central-free objects \
                 (live={}, central_free={}); double-activation would destroy accounting",
                sg.live_count(),
                sg.central_free_count()
            );
            sg.activate(object_count);
            // B.3 (W19-1c): the freshly-activated span is well-formed (full bitmap,
            // `central_free == object_count`, geometry fits). A runtime assertion at
            // this transition, under the held lock. Subsumes the §16.4 conservation
            // (M2 action: pass `span.reconstruct_non_central_residency()` once caches
            // track per-span residency).
            debug_assert!(span.check_invariants_locked(&sg));
        }

        // Step 2: install in pagemap (W3-6).
        if let Err(e) = pagemap.install_span(meta, span) {
            // Undo: clear the bitmap so the span is not in an inconsistent state.
            let sg = span.lock();
            sg.deactivate();
            return Err(e.into());
        }

        // Step 3: add to partial list under central lock.
        let bin = self
            .bins
            .get(sc.index())
            .expect("activate_span: invalid size class");
        let _guard = bin.lock();
        bin.push_partial(span);
        bin.span_count.fetch_add(1, Ordering::Relaxed);
        bin.total_central_free
            .fetch_add(object_count as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Deactivate an empty span (W5-5): remove it from the partial or empty
    /// list, transition its pagemap entries to `Released` (W3-6), and decrement
    /// the bin's span count. The caller is responsible for returning the backing
    /// extent to the backend.
    ///
    /// **C-004/C-005 acceptance:** panics if the span is not empty. Releasing
    /// a non-empty span would recycle live memory — a catastrophic failure mode.
    ///
    /// SPEC-transition: span `Active -> Released` (§7.3, §14.6)
    pub fn deactivate_span(&self, span: &SpanDescriptor, pagemap: &PageMap) {
        let sc = span.size_class();
        let bin = self
            .bins
            .get(sc.index())
            .expect("deactivate_span: invalid size class");
        let _guard = bin.lock();

        debug_assert_eq!(
            span.state(),
            SpanState::Active,
            "deactivate_span: span is not Active (state={:?}); \
             double-deactivation or deactivation of a released span",
            span.state()
        );

        // C-004/C-005: verify emptiness under both locks. The central lock
        // serializes against insert_batch, preventing a TOCTOU race.
        let free_before = {
            let sg = span.lock();
            assert!(
                sg.is_empty(NonCentralResidency::NONE),
                "W5-5/C-004: deactivate_span called on a non-empty span \
                 (live={}, central_free={}, object_count={})",
                sg.live_count(),
                sg.central_free_count(),
                span.object_count()
            );
            // B.3 (W19-1c): the empty span is internally well-formed before its
            // bitmap is torn down — a runtime assertion at this transition.
            debug_assert!(span.check_invariants_locked(&sg));
            let free = sg.central_free_count();
            sg.deactivate();
            free
        };

        // Remove from whichever list the span is in.
        if !bin.remove_partial(span as *const SpanDescriptor) {
            bin.remove_empty(span as *const SpanDescriptor);
        }
        bin.span_count.fetch_sub(1, Ordering::Relaxed);

        // Adjust total_central_free for the deactivated objects.
        if free_before > 0 {
            bin.total_central_free
                .fetch_sub(free_before as u64, Ordering::Relaxed);
        }

        // W3-6: transition pagemap entries.
        span.set_state(SpanState::Released);
        pagemap.release_span(span);
    }

    /// Forcibly deactivate a span **regardless of liveness** — the arena
    /// reset/destroy teardown (plan 06 W9-4b/W9-6b). Unlike
    /// [`deactivate_span`](Self::deactivate_span), this does **not** require the
    /// span be empty: arena reset "discards all extant allocations" (§22.5), so a
    /// span with live objects is torn down and its backing reclaimed, the live
    /// objects abandoned (the caller has accepted that outstanding pointers
    /// become invalid). It removes the span from whichever central list holds it,
    /// drops its central-free contribution, and transitions its pagemap entries
    /// to `Released`.
    ///
    /// Soundness rests on the §22.5 precondition the lifecycle enforces: the
    /// arena is `Resetting`/`Draining`, so no thread is allocating from or freeing
    /// into it concurrently — the only mutator of these spans is this drain. A
    /// span already torn down (state != `Active`) is skipped, so a double drain is
    /// harmless.
    ///
    /// Returns the number of objects that were still live when torn down (for the
    /// caller's quota credit). The caller returns the backing extent afterwards.
    ///
    /// SPEC-transition: span `Active -> Released` (forced, §22.5/§36.13)
    pub fn deactivate_span_forced(&self, span: &SpanDescriptor, pagemap: &PageMap) -> u32 {
        let sc = span.size_class();
        let bin = self
            .bins
            .get(sc.index())
            .expect("deactivate_span_forced: invalid size class");
        let _guard = bin.lock();

        // A span that is no longer Active was already torn down (by a prior drain
        // step or a normal retirement); skip it idempotently.
        if span.state() != SpanState::Active {
            return 0;
        }

        let (free_before, live_before) = {
            let sg = span.lock();
            // B.3 (W19-1c): the still-Active span is well-formed before this forced
            // teardown clears its bitmap — a runtime assertion at the transition.
            debug_assert!(span.check_invariants_locked(&sg));
            let free = sg.central_free_count();
            let live = sg.live_count();
            sg.deactivate();
            (free, live)
        };

        // Remove from whichever list the span is in (partial or empty cache).
        if !bin.remove_partial(span as *const SpanDescriptor) {
            bin.remove_empty(span as *const SpanDescriptor);
        }
        bin.span_count.fetch_sub(1, Ordering::Relaxed);

        if free_before > 0 {
            bin.total_central_free
                .fetch_sub(free_before as u64, Ordering::Relaxed);
        }

        span.set_state(SpanState::Released);
        pagemap.release_span(span);
        live_before
    }
}

impl Default for CentralCache {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every field of CentralCache is Sync (CentralBin is Sync — backed by
// atomics + a spinlock; the raw pointers reach Sync descriptors in monotonic
// metadata, protected by the lock).
unsafe impl Sync for CentralCache {}
// SAFETY: CentralCache is an array of CentralBin; each bin's interior state
// is guarded by atomics and a spinlock, and the pointed-to descriptors are Sync.
unsafe impl Send for CentralCache {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::generated::tables::PAGE_SIZE;
    use crate::ids::{ArenaId, SpanId};
    use crate::size_class;
    use crate::span::SpanDescriptor;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn make_span(id: u32, sc: SizeClassId, base: usize, m: &BumpArena) -> SpanDescriptor {
        make_span_arena(id, ArenaId::DEFAULT, sc, base, m)
    }

    fn make_span_arena(
        id: u32,
        arena: ArenaId,
        sc: SizeClassId,
        base: usize,
        m: &BumpArena,
    ) -> SpanDescriptor {
        let row = size_class::row(sc);
        SpanDescriptor::new(
            SpanId(id),
            arena,
            sc,
            base,
            row.slab_pages,
            row.objects_per_slab,
            0,
            m,
        )
        .expect("span creation failed")
    }

    fn drain_all(cache: &CentralCache, sc: SizeClassId) -> Vec<u16> {
        let mut all = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all.push(batch.index(i));
            }
        }
        all
    }

    #[test]
    fn max_batch_len_matches_table() {
        let expected = SIZE_CLASSES.iter().map(|r| r.batch as usize).max().unwrap();
        assert_eq!(MAX_BATCH_LEN, expected);
        const { assert!(MAX_BATCH_LEN > 0) };
    }

    #[test]
    fn empty_cache_returns_need_span() {
        let cache = CentralCache::new();
        let sc = SizeClassId::new(0);
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            1,
        ) {
            RemoveResult::NeedSpan => {}
            RemoveResult::Ok(_) => panic!("expected NeedSpan from empty cache"),
        }
    }

    #[test]
    fn activate_then_remove_returns_objects() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3); // 64 bytes
        let base = 0x4000_0000usize;
        let span = make_span(1, sc, base, &m);
        let row = size_class::row(sc);

        cache.activate_span(&span, &pm, &m).unwrap();

        // The span should be in the partial list with all objects central-free.
        let bin = cache.bin(sc).unwrap();
        assert_eq!(bin.partial_count(), 1);
        assert_eq!(bin.span_count(), 1);
        assert_eq!(bin.total_central_free(), row.objects_per_slab as u64);
        assert!(span.conservation_holds_central_only());

        // Remove a batch.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.len(), 4);
                // Verify batch addresses are valid.
                let layout = SlabLayout::compute(sc, base, 0).unwrap();
                for i in 0..batch.len() {
                    let addr = batch.object_addr(i, &layout).unwrap();
                    assert_eq!(addr % layout.align, 0);
                    assert!(addr >= layout.object0);
                    assert!(addr < base + layout.slab_bytes);
                }
            }
            RemoveResult::NeedSpan => panic!("expected batch"),
        }

        // Conservation law still holds.
        assert!(span.conservation_holds_central_only());
        assert_eq!(span.live_count(), 4);
        assert_eq!(span.central_free_count(), row.objects_per_slab - 4);
    }

    #[test]
    fn insert_batch_returns_objects_and_detects_empty() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let base = 0x4000_0000usize;
        let span = make_span(1, sc, base, &m);
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as usize;

        cache.activate_span(&span, &pm, &m).unwrap();

        let all_indices = drain_all(&cache, sc);
        assert_eq!(all_indices.len(), obj_count);
        assert_eq!(span.live_count() as usize, obj_count);
        assert_eq!(span.central_free_count(), 0);
        assert!(span.conservation_holds_central_only());

        // Return all objects — the span is empty, cached internally.
        let result = cache.insert_batch(&span, &all_indices, all_indices.len());
        assert_eq!(result.inserted, obj_count);
        // The empty cache has room → span is cached, not signalled for deactivation.
        assert!(!result.span_empty);
        assert!(span.is_empty_central_only());
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);
    }

    #[test]
    fn insert_partial_does_not_report_empty() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        // Remove 4 objects.
        let batch = match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(b) => b,
            RemoveResult::NeedSpan => panic!("expected batch"),
        };
        assert_eq!(batch.len(), 4);

        // Return 2 of the 4 — span is not empty, still has 2 live.
        let two_indices: Vec<u16> = batch.indices()[..2].to_vec();
        let result = cache.insert_batch(&span, &two_indices, 2);
        assert_eq!(result.inserted, 2);
        assert!(!result.span_empty);
        assert_eq!(span.live_count(), 2);
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn exhausted_span_is_readded_on_insert() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        let all = drain_all(&cache, sc);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);

        // Return some (not all) objects. The span should re-enter the partial list.
        let some: Vec<u16> = all[..4].to_vec();
        let result = cache.insert_batch(&span, &some, 4);
        assert_eq!(result.inserted, 4);
        assert!(!result.span_empty);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 1);

        // We can now remove from it again.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            2,
        ) {
            RemoveResult::Ok(batch) => assert_eq!(batch.len(), 2),
            RemoveResult::NeedSpan => panic!("expected batch after re-add"),
        }
    }

    #[test]
    fn deactivate_span_removes_from_list_and_transitions_pagemap() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();
        assert_eq!(cache.bin(sc).unwrap().span_count(), 1);

        // Return all objects to make it empty.
        let all = drain_all(&cache, sc);
        let _result = cache.insert_batch(&span, &all, all.len());
        // The span is empty and cached internally.
        assert!(span.is_empty_central_only());

        // Deactivate removes from whichever list (partial or empty cache).
        cache.deactivate_span(&span, &pm);
        assert_eq!(cache.bin(sc).unwrap().span_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 0);
        assert_eq!(span.state(), SpanState::Released);

        // Pagemap should show Released.
        match pm.lookup(span.base()) {
            crate::pagemap::PageEntry::Released(p) => {
                assert_eq!(p, &span as *const SpanDescriptor);
            }
            other => panic!("expected Released, got {other:?}"),
        }
    }

    #[test]
    fn multiple_spans_in_partial_list() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);

        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;
        let s1 = make_span(1, sc, 0x4000_0000, &m);
        let s2 = make_span(2, sc, 0x4000_0000 + span_bytes, &m);
        let s3 = make_span(3, sc, 0x4000_0000 + 2 * span_bytes, &m);

        cache.activate_span(&s1, &pm, &m).unwrap();
        cache.activate_span(&s2, &pm, &m).unwrap();
        cache.activate_span(&s3, &pm, &m).unwrap();

        assert_eq!(cache.bin(sc).unwrap().partial_count(), 3);
        assert_eq!(cache.bin(sc).unwrap().span_count(), 3);

        // Remove from the head (s3, last pushed).
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            1,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.span(), &s3 as *const SpanDescriptor);
            }
            RemoveResult::NeedSpan => panic!("expected batch"),
        }
    }

    #[test]
    fn conservation_law_holds_through_remove_insert_cycle() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(0); // 16-byte class, 1024 objects

        let span = make_span(1, sc, 0x4000_0000, &m);
        cache.activate_span(&span, &pm, &m).unwrap();

        // Remove, check, insert, check — repeatedly.
        for _ in 0..10 {
            let batch = match cache.remove_batch(
                NodeId::DEFAULT,
                ArenaId::DEFAULT,
                Label::PUBLIC,
                sc,
                ANY_PLACE_CLASS,
                8,
            ) {
                RemoveResult::Ok(b) => b,
                RemoveResult::NeedSpan => break,
            };
            assert!(span.conservation_holds_central_only());

            let indices: Vec<u16> = batch.indices().to_vec();
            cache.insert_batch(&span, &indices, indices.len());
            assert!(span.conservation_holds_central_only());
        }
    }

    #[test]
    fn batch_addresses_match_slab_layout() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(5); // 96-byte class
        let base = 0x4000_0000usize;
        let span = make_span(1, sc, base, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        let layout = SlabLayout::compute(sc, base, 0).unwrap();

        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            8,
        ) {
            RemoveResult::Ok(batch) => {
                for i in 0..batch.len() {
                    let addr = batch.object_addr(i, &layout).unwrap();
                    let idx = batch.index(i) as usize;
                    assert_eq!(addr, layout.object_addr(idx).unwrap());
                    assert_eq!(layout.addr_to_index(addr), Some(idx));
                }
            }
            RemoveResult::NeedSpan => panic!("expected batch"),
        }
    }

    #[test]
    fn activate_fails_safely_on_oom() {
        let m_small = meta(64); // too small for pagemap nodes
        let m_big = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(0);
        let span = make_span(1, sc, 0x4000_0000, &m_big);

        // Pagemap install will fail (too little metadata for radix nodes).
        let result = cache.activate_span(&span, &pm, &m_small);
        assert!(result.is_err());

        // The span should be cleanly deactivated (bitmap cleared).
        assert_eq!(span.central_free_count(), 0);
        assert_eq!(span.live_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);
    }

    #[test]
    fn concurrent_remove_from_same_bin() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(0);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        let cache_ref = &cache;
        let span_ref = &span;

        let results: Vec<Vec<u16>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(move || {
                        let mut got = Vec::new();
                        for _ in 0..100 {
                            if let RemoveResult::Ok(batch) = cache_ref.remove_batch(
                                NodeId::DEFAULT,
                                ArenaId::DEFAULT,
                                Label::PUBLIC,
                                sc,
                                ANY_PLACE_CLASS,
                                1,
                            ) {
                                for i in 0..batch.len() {
                                    got.push(batch.index(i));
                                }
                            }
                        }
                        got
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let all: Vec<u16> = results.into_iter().flatten().collect();
        // Every index is unique (no double-allocations).
        let unique: std::collections::BTreeSet<u16> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "duplicate object index allocated");

        // Conservation law holds.
        assert!(span_ref.conservation_holds_central_only());
        assert_eq!(span_ref.live_count() as usize, all.len());
    }

    #[test]
    fn last_cache_flush_detects_empty_span() {
        // W5-3e: a span emptied only by the last cache flush IS detected.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as usize;

        cache.activate_span(&span, &pm, &m).unwrap();

        let all = drain_all(&cache, sc);
        assert_eq!(all.len(), obj_count);

        // Return all but the last one — NOT empty.
        let (most, last) = all.split_at(obj_count - 1);
        let result = cache.insert_batch(&span, most, most.len());
        assert_eq!(result.inserted, obj_count - 1);
        assert!(!result.span_empty);

        // Return the last one — NOW empty. The span is detected as empty and
        // moved to the empty cache.
        let result = cache.insert_batch(&span, last, 1);
        assert_eq!(result.inserted, 1);
        assert!(span.is_empty_central_only());
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);
    }

    // --- new tests closing W5 gaps -------------------------------------------

    #[test]
    fn oom_retry_integration() {
        // W5-4b acceptance: "OOM-retry path exercised."
        // Simulate the full caller retry loop: NeedSpan → activate → retry → Ok.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);

        // Step 1: empty cache returns NeedSpan.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::NeedSpan => {}
            RemoveResult::Ok(_) => panic!("expected NeedSpan from empty cache"),
        }

        // Step 2: caller creates a span from the backend and activates it.
        let span = make_span(1, sc, 0x4000_0000, &m);
        cache.activate_span(&span, &pm, &m).unwrap();

        // Step 3: retry succeeds.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.len(), 4);
                assert!(span.conservation_holds_central_only());
            }
            RemoveResult::NeedSpan => panic!("expected batch after retry"),
        }
    }

    #[test]
    fn arena_mismatch_returns_need_span() {
        // C-001: a span from arena A should not serve a request for arena B.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);

        let arena_a = ArenaId(1);
        let arena_b = ArenaId(2);

        let span_a = make_span_arena(1, arena_a, sc, 0x4000_0000, &m);
        cache.activate_span(&span_a, &pm, &m).unwrap();

        // Request for arena B should get NeedSpan (no matching span).
        match cache.remove_batch(
            NodeId::DEFAULT,
            arena_b,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::NeedSpan => {}
            RemoveResult::Ok(_) => panic!("should not get objects from wrong arena"),
        }

        // Request for arena A should succeed.
        match cache.remove_batch(
            NodeId::DEFAULT,
            arena_a,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(batch) => assert_eq!(batch.len(), 4),
            RemoveResult::NeedSpan => panic!("expected batch for matching arena"),
        }
    }

    #[test]
    fn arena_scan_finds_matching_span_behind_head() {
        // C-001: remove_batch should scan past non-matching arenas.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;

        let arena_a = ArenaId(1);
        let arena_b = ArenaId(2);

        // Activate: A first (pushed deep), then B (becomes head).
        let span_a = make_span_arena(1, arena_a, sc, 0x4000_0000, &m);
        let span_b = make_span_arena(2, arena_b, sc, 0x4000_0000 + span_bytes, &m);
        cache.activate_span(&span_a, &pm, &m).unwrap();
        cache.activate_span(&span_b, &pm, &m).unwrap();

        // Head is B. Request for A should scan past B and find A.
        match cache.remove_batch(
            NodeId::DEFAULT,
            arena_a,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.span(), &span_a as *const SpanDescriptor);
                assert_eq!(batch.len(), 4);
            }
            RemoveResult::NeedSpan => panic!("expected batch from scanned arena A"),
        }
    }

    #[test]
    fn remove_batch_prefers_a_class_matching_span() {
        // W14 (§24.6–§24.8): with two partial spans of the same size class but different
        // placement classes, `remove_batch(class)` carves from the class-matching span, so
        // cold / hot / short-lived small objects cluster into separate spans.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;

        let cold = make_span(1, sc, 0x5000_0000, &m);
        let hot = make_span(2, sc, 0x5000_0000 + span_bytes, &m);
        cold.set_place_class(1); // PlaceClass::Cold
        hot.set_place_class(2); // PlaceClass::Hot
        cache.activate_span(&cold, &pm, &m).unwrap();
        cache.activate_span(&hot, &pm, &m).unwrap();

        // A cold request lands in the cold span; a hot request in the hot span — regardless
        // of which is at the list head.
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 1, 1) {
            RemoveResult::Ok(b) => assert_eq!(b.span(), &cold as *const SpanDescriptor),
            RemoveResult::NeedSpan => panic!("cold class should match the cold span"),
        }
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 2, 1) {
            RemoveResult::Ok(b) => assert_eq!(b.span(), &hot as *const SpanDescriptor),
            RemoveResult::NeedSpan => panic!("hot class should match the hot span"),
        }
        // A class with no matching span (Short=3) and no empty cache reports NeedSpan rather
        // than mixing into a different-class span — the caller then creates a class-tagged
        // span (or, on OOM, falls back via ANY_PLACE_CLASS).
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 3, 1) {
            RemoveResult::NeedSpan => {}
            RemoveResult::Ok(_) => panic!("a non-matching class must not silently mix spans"),
        }
        // The ANY fallback still reuses an existing partial (availability before policy).
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            1,
        ) {
            RemoveResult::Ok(_) => {}
            RemoveResult::NeedSpan => panic!("ANY_PLACE_CLASS should reuse any partial"),
        }
    }

    #[test]
    fn concurrent_mixed_remove_and_insert() {
        // Concurrent remove + insert from multiple threads.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(0); // 1024 objects
        let span = make_span(1, sc, 0x4000_0000, &m);
        cache.activate_span(&span, &pm, &m).unwrap();

        let cache_ref = &cache;
        let span_ref = &span;

        std::thread::scope(|s| {
            // 2 threads removing, 2 threads inserting what they took.
            let handles: Vec<_> = (0..4)
                .map(|t| {
                    s.spawn(move || {
                        for _ in 0..50 {
                            if let RemoveResult::Ok(batch) = cache_ref.remove_batch(
                                NodeId::DEFAULT,
                                ArenaId::DEFAULT,
                                Label::PUBLIC,
                                sc,
                                ANY_PLACE_CLASS,
                                2,
                            ) {
                                // Some threads return immediately.
                                if t % 2 == 0 {
                                    let indices: Vec<u16> = batch.indices().to_vec();
                                    cache_ref.insert_batch(span_ref, &indices, indices.len());
                                }
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });

        // The conservation law must hold after the storm.
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn full_lifecycle_conservation() {
        // Activate → drain → return all → deactivate. Conservation at every step.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as usize;

        // Step 1: activate. conservation holds.
        cache.activate_span(&span, &pm, &m).unwrap();
        assert!(span.conservation_holds_central_only());
        assert_eq!(span.live_count(), 0);
        assert_eq!(span.central_free_count() as usize, obj_count);

        // Step 2: drain all. conservation holds.
        let all = drain_all(&cache, sc);
        assert_eq!(all.len(), obj_count);
        assert!(span.conservation_holds_central_only());
        assert_eq!(span.live_count() as usize, obj_count);
        assert_eq!(span.central_free_count(), 0);

        // Step 3: return all. conservation holds. Span detected as empty.
        cache.insert_batch(&span, &all, all.len());
        assert!(span.conservation_holds_central_only());
        assert!(span.is_empty_central_only());
        assert_eq!(span.live_count(), 0);
        assert_eq!(span.central_free_count() as usize, obj_count);

        // Step 4: deactivate. All counts zero.
        cache.deactivate_span(&span, &pm);
        assert_eq!(span.live_count(), 0);
        assert_eq!(span.central_free_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().span_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().total_central_free(), 0);
    }

    #[test]
    #[should_panic(expected = "W5-5/C-004")]
    fn deactivate_non_empty_span_panics() {
        // C-004/C-005: deactivating a non-empty span is a hard failure.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        // Remove some objects (span is NOT empty).
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(_) => {}
            RemoveResult::NeedSpan => panic!("expected batch"),
        }

        // This must panic: the span has live objects.
        cache.deactivate_span(&span, &pm);
    }

    #[test]
    fn partition_terms_sum_to_object_count() {
        // W5-3a: the five-term partition sums correctly, no double-counts.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        let obj_count = size_class::row(sc).objects_per_slab;

        cache.activate_span(&span, &pm, &m).unwrap();

        // After activation: live=0, central_free=all, cached/quarantined=0.
        let sg = span.lock();
        let terms = sg.partition(NonCentralResidency::NONE);
        assert_eq!(terms, [0, 0, 0, obj_count, 0]);
        assert_eq!(terms.iter().sum::<u32>(), obj_count);
        drop(sg);

        // After removing 8 objects: live=8, central_free=all-8.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            8,
        ) {
            RemoveResult::Ok(batch) => assert_eq!(batch.len(), 8),
            RemoveResult::NeedSpan => panic!("expected batch"),
        }
        let sg = span.lock();
        let terms = sg.partition(NonCentralResidency::NONE);
        assert_eq!(terms[0], 8); // live
        assert_eq!(terms[3], obj_count - 8); // central_free
        assert_eq!(terms.iter().sum::<u32>(), obj_count);
        drop(sg);
    }

    #[test]
    fn empty_cache_reuses_span_without_backend() {
        // DD-4 SpanCache: an empty span is cached and reused by remove_batch
        // without going back to the backend.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        // Drain all objects.
        let all = drain_all(&cache, sc);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);

        // Return all → span is empty, cached.
        cache.insert_batch(&span, &all, all.len());
        assert!(span.is_empty_central_only());
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);

        // Next remove should pull from the empty cache, NOT return NeedSpan.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.len(), 4);
                assert_eq!(batch.span(), &span as *const SpanDescriptor);
            }
            RemoveResult::NeedSpan => panic!("empty cache should have provided a span"),
        }

        // The span moved from empty cache to partial (then possibly exhausted
        // if all objects were carved, but we only took 4).
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 0);
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn empty_cache_overflow_signals_caller() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;
        let obj_count = row.objects_per_slab as usize;

        let s1 = make_span(1, sc, 0x4000_0000, &m);
        let s2 = make_span(2, sc, 0x4000_0000 + span_bytes, &m);

        // Step 1: fill the empty cache with s1.
        cache.activate_span(&s1, &pm, &m).unwrap();
        let all1 = drain_all(&cache, sc);
        assert_eq!(all1.len(), obj_count);
        let r1 = cache.insert_batch(&s1, &all1, all1.len());
        assert!(!r1.span_empty, "s1 should be cached, not signalled");
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);

        // Step 2: activate s2 and drain exactly its objects (avoiding the
        // empty-cache pop that drain_all would trigger).
        cache.activate_span(&s2, &pm, &m).unwrap();
        let mut s2_objects = Vec::new();
        while s2_objects.len() < obj_count {
            let want = (obj_count - s2_objects.len()).min(MAX_BATCH_LEN);
            match cache.remove_batch(
                NodeId::DEFAULT,
                ArenaId::DEFAULT,
                Label::PUBLIC,
                sc,
                ANY_PLACE_CLASS,
                want,
            ) {
                RemoveResult::Ok(batch) => {
                    for i in 0..batch.len() {
                        s2_objects.push(batch.index(i));
                    }
                }
                _ => break,
            }
        }
        assert_eq!(s2_objects.len(), obj_count);
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1, "s1 still cached");

        // Step 3: return all objects to s2 → empty, but cache is full (MAX=1).
        let r2 = cache.insert_batch(&s2, &s2_objects, obj_count);
        assert!(r2.span_empty, "cache full (MAX=1) → caller must deactivate");
    }

    #[test]
    fn total_central_free_tracks_every_lifecycle_step() {
        // Gap 5: verify total_central_free is accurate at every intermediate step.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as u64;

        // After activation: all objects are central-free.
        cache.activate_span(&span, &pm, &m).unwrap();
        assert_eq!(cache.bin(sc).unwrap().total_central_free(), obj_count);

        // After removing a batch: total decreases.
        let batch = match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::Ok(b) => b,
            _ => panic!("expected batch"),
        };
        let removed = batch.len() as u64;
        assert_eq!(
            cache.bin(sc).unwrap().total_central_free(),
            obj_count - removed
        );

        // After inserting them back: total increases.
        let indices: Vec<u16> = batch.indices().to_vec();
        cache.insert_batch(&span, &indices, indices.len());
        assert_eq!(cache.bin(sc).unwrap().total_central_free(), obj_count);

        // After deactivation: total is zero.
        let all = drain_all(&cache, sc);
        cache.insert_batch(&span, &all, all.len());
        cache.deactivate_span(&span, &pm);
        assert_eq!(cache.bin(sc).unwrap().total_central_free(), 0);
    }

    #[test]
    fn slab_header_through_central_cache_path() {
        // Gap 6: exercise a non-zero slab_header through the full central path.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);

        let base = 0x4000_0000usize;
        let span = {
            let slab_header = 128u32;
            // The header-adjusted object count keeps the span geometrically valid
            // (B.3 "object ranges fit"); `objects_per_slab` assumes a zero header.
            let object_count = SlabLayout::compute(sc, base, slab_header as usize)
                .map_or(row.objects_per_slab, |l| l.object_count as u32);
            SpanDescriptor::new(
                SpanId(1),
                ArenaId::DEFAULT,
                sc,
                base,
                row.slab_pages,
                object_count,
                slab_header,
                &m,
            )
            .unwrap()
        };

        cache.activate_span(&span, &pm, &m).unwrap();
        let obj_count = span.object_count() as usize;
        assert!(obj_count > 0);

        // Drain and return all objects.
        let all = drain_all(&cache, sc);
        assert_eq!(all.len(), obj_count);
        assert!(span.conservation_holds_central_only());

        // Return all → empty → cached.
        let r = cache.insert_batch(&span, &all, all.len());
        assert!(!r.span_empty);
        assert!(span.is_empty_central_only());

        cache.deactivate_span(&span, &pm);
        assert_eq!(span.central_free_count(), 0);
    }

    #[test]
    fn remove_batch_returns_partial_when_span_has_fewer_than_desired() {
        // Gap 7: requesting more objects than the span has should return
        // only what's available.
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();

        // Drain everything, then return exactly 2 objects to make
        // central_free_count == 2.
        let all = drain_all(&cache, sc);
        assert_eq!(span.central_free_count(), 0);

        let two = &all[..2];
        cache.insert_batch(&span, two, 2);
        assert_eq!(span.central_free_count(), 2);

        // Request a large batch when only 2 are available.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            RemoveResult::Ok(batch) => {
                assert_eq!(batch.len(), 2, "should return only 2 available objects");
            }
            _ => panic!("expected Ok with partial batch"),
        }

        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn empty_cache_arena_mismatch_returns_need_span() {
        // Gap 8: empty cache pop with wrong arena puts the span back.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);

        let arena_a = ArenaId(1);
        let arena_b = ArenaId(2);

        // Create and empty a span for arena_a → cached.
        let s1 = make_span_arena(1, arena_a, sc, 0x4000_0000, &m);
        cache.activate_span(&s1, &pm, &m).unwrap();

        let all1 = {
            let mut v = Vec::new();
            while let RemoveResult::Ok(batch) = cache.remove_batch(
                NodeId::DEFAULT,
                arena_a,
                Label::PUBLIC,
                sc,
                ANY_PLACE_CLASS,
                MAX_BATCH_LEN,
            ) {
                for i in 0..batch.len() {
                    v.push(batch.index(i));
                }
            }
            v
        };
        cache.insert_batch(&s1, &all1, all1.len());
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);

        // Request from arena_b: the empty cache has arena_a's span.
        // Should put it back and return NeedSpan.
        match cache.remove_batch(
            NodeId::DEFAULT,
            arena_b,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            4,
        ) {
            RemoveResult::NeedSpan => {}
            RemoveResult::Ok(_) => panic!("should not serve arena_b from arena_a's cache"),
        }
        // The span should have been put back in the empty cache.
        assert_eq!(cache.bin(sc).unwrap().empty_count(), 1);
    }

    #[test]
    fn concurrent_stress_multiple_spans() {
        // Gap 9: more thorough concurrency test with multiple spans and threads.
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;

        let s1 = make_span(1, sc, 0x4000_0000, &m);
        let s2 = make_span(2, sc, 0x4000_0000 + span_bytes, &m);
        let s3 = make_span(3, sc, 0x4000_0000 + 2 * span_bytes, &m);

        cache.activate_span(&s1, &pm, &m).unwrap();
        cache.activate_span(&s2, &pm, &m).unwrap();
        cache.activate_span(&s3, &pm, &m).unwrap();

        let cache_ref = &cache;
        let spans = [&s1, &s2, &s3];

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_t| {
                    s.spawn(move || {
                        for _ in 0..200 {
                            if let RemoveResult::Ok(batch) = cache_ref.remove_batch(
                                NodeId::DEFAULT,
                                ArenaId::DEFAULT,
                                Label::PUBLIC,
                                sc,
                                ANY_PLACE_CLASS,
                                MAX_BATCH_LEN,
                            ) {
                                // SAFETY: batch.span() is a valid pointer to a SpanDescriptor
                                // allocated and pinned in the BumpArena for this test's lifetime.
                                let span = unsafe { &*batch.span() };
                                let indices: Vec<u16> = batch.indices().to_vec();
                                cache_ref.insert_batch(span, &indices, indices.len());
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });

        // Conservation law holds for all spans.
        for span in &spans {
            assert!(span.conservation_holds_central_only());
        }
    }

    /// W8 free-path hardening: an insert into a span that was already
    /// deactivated (a stale/double free racing past the empty-span teardown)
    /// is rejected loudly in debug builds — the state guard fires before any
    /// bitmap or count mutation.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "is not Active")]
    fn stale_free_into_deactivated_span_panics_in_debug() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();
        let all = drain_all(&cache, sc);
        cache.insert_batch(&span, &all, all.len());
        cache.deactivate_span(&span, &pm); // span is empty: legal teardown

        // The stale free: the span is `Released` now. The state guard rejects it.
        cache.insert_batch(&span, &[0], 1);
    }

    /// W8 free-path hardening (release semantics): the same stale free inserts
    /// nothing and leaves the deactivated span untouched — no `live_count`
    /// underflow, no re-link, no second deactivation claim.
    #[test]
    #[cfg(not(debug_assertions))]
    fn stale_free_into_deactivated_span_is_ignored_in_release() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();
        let all = drain_all(&cache, sc);
        cache.insert_batch(&span, &all, all.len());
        cache.deactivate_span(&span, &pm);

        let r = cache.insert_batch(&span, &[0], 1);
        assert_eq!(r.inserted, 0, "stale free must insert nothing");
        assert!(!r.span_empty, "stale free must not claim the deactivation");
        // The span's teardown state is untouched.
        assert_eq!(span.state(), SpanState::Released);
        assert_eq!(span.live_count(), 0);
        assert_eq!(span.central_free_count(), 0);
        let bin = cache.bin(sc).unwrap();
        assert_eq!(bin.span_count(), 0);
        assert_eq!(bin.partial_count(), 0);
    }

    /// W8 free-path hardening (release semantics): a double free of an object
    /// in an *already-empty* span inserts nothing (`inserted == 0`) and must
    /// not eject the span from the empty cache or claim the deactivation —
    /// only the insert that *made* the span empty owns that transition (W5-5
    /// exclusivity). In debug builds the same call panics at the per-index
    /// double-insert assert, which `double_free_is_rejected_loudly_in_debug`
    /// covers.
    #[test]
    #[cfg(not(debug_assertions))]
    fn double_free_into_empty_span_does_not_claim_deactivation() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let row = size_class::row(sc);
        let span_bytes = row.slab_pages as usize * PAGE_SIZE;
        let a = make_span(1, sc, 0x4000_0000, &m);
        let b = make_span(2, sc, 0x4000_0000 + span_bytes, &m);

        cache.activate_span(&a, &pm, &m).unwrap();
        cache.activate_span(&b, &pm, &m).unwrap();

        // Drain both spans, tracking which indices came from which span.
        let mut from_a = Vec::new();
        let mut from_b = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            let dst = if core::ptr::eq(batch.span(), &a) {
                &mut from_a
            } else {
                &mut from_b
            };
            dst.extend_from_slice(batch.indices());
        }
        assert_eq!(
            from_a.len() + from_b.len(),
            2 * row.objects_per_slab as usize
        );

        // Empty A first: it lands in the (size-1) empty cache, span_empty=false.
        let ra = cache.insert_batch(&a, &from_a, from_a.len());
        assert!(!ra.span_empty);
        // Empty B: the cache is full, so the emptier is told to deactivate.
        let rb = cache.insert_batch(&b, &from_b, from_b.len());
        assert!(rb.span_empty, "the emptying insert owns the deactivation");

        // A double free into B (already empty, still Active, unlinked): nothing
        // inserted, and crucially span_empty=false — B's deactivation belongs to
        // the caller above, exactly once.
        let dup = cache.insert_batch(&b, &[from_b[0]], 1);
        assert_eq!(dup.inserted, 0);
        assert!(!dup.span_empty);

        // A double free into the *cached* A must not eject it either.
        let dup_a = cache.insert_batch(&a, &[from_a[0]], 1);
        assert_eq!(dup_a.inserted, 0);
        assert!(!dup_a.span_empty);
        let bin = cache.bin(sc).unwrap();
        assert_eq!(bin.empty_count(), 1, "A stays in the empty cache");

        // The single legal teardown of B proceeds normally.
        cache.deactivate_span(&b, &pm);
        assert_eq!(bin.span_count(), 1, "only A remains tracked");

        // A is still reusable straight from the empty cache.
        match cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            ANY_PLACE_CLASS,
            1,
        ) {
            RemoveResult::Ok(batch) => assert!(core::ptr::eq(batch.span(), &a)),
            RemoveResult::NeedSpan => panic!("cached empty span A must be reusable"),
        }
    }

    /// In debug builds, a double free (an index whose bit is already set) is
    /// caught loudly by the per-index assert — the W18-2 (plan 08) debug
    /// behaviour the M1 free path relies on.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "rejected")]
    fn double_free_is_rejected_loudly_in_debug() {
        let m = meta(2 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);

        cache.activate_span(&span, &pm, &m).unwrap();
        let all = drain_all(&cache, sc);
        // Return object 0 twice: the second insert's bit is already set.
        cache.insert_batch(&span, &[all[0]], 1);
        cache.insert_batch(&span, &[all[0]], 1);
    }

    #[test]
    fn central_checker_catches_accounting_drift() {
        // B.1 (W19-1a) negative test: the reachability/accounting law
        // (Σ central_free over the reachable lists == the bin aggregate) must be
        // *enforced*, not merely asserted. We desync it deliberately and confirm
        // the checker rejects the bin.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        cache.activate_span(&span, &pm, &m).unwrap();
        assert!(cache.check_invariants(), "an activated bin reconciles");

        // Remove one object straight from the span's bitmap, bypassing the bin
        // accounting that `remove_batch` maintains — so the span now reports one
        // fewer central-free object than the bin's `total_central_free` claims.
        {
            let sg = span.lock();
            let mut idx = [0u16; 1];
            assert_eq!(sg.central_remove_batch(&mut idx, 1), 1);
        }
        assert!(
            !cache.check_invariants(),
            "the checker must catch Σ central_free != total_central_free"
        );
    }

    #[test]
    fn central_checker_catches_a_span_count_miscount() {
        // B.1 (W19-1a) negative test: the list lengths must never exceed the
        // tracked span count (partial_count + empty_count <= span_count). Force a
        // miscount and confirm the checker rejects it.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cache = CentralCache::new();
        let sc = SizeClassId::new(3);
        let span = make_span(1, sc, 0x4000_0000, &m);
        cache.activate_span(&span, &pm, &m).unwrap();
        assert!(cache.check_invariants());

        // The bin tracks one span (partial_count == 1, span_count == 1). Drop
        // span_count to 0 so the partial list is "longer" than the count admits.
        cache.bin(sc).unwrap().corrupt_span_count_for_test(0);
        assert!(
            !cache.check_invariants(),
            "the checker must catch partial_count + empty_count > span_count"
        );
    }
}

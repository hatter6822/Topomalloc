// SPDX-License-Identifier: MIT
//! Central free list (§14.5, plan 03 W5-4 + W5-5).
//!
//! The central free list is the allocation layer between the size-class classifier
//! (W2) and the caches (plan 05): it owns **partial spans** — spans with
//! central-resident free objects — and serves/absorbs object batches.
//!
//! **Structure (W5-4a).** Keyed `(node, arena, label, sc)`: at M1 the node is
//! `DEFAULT` (single NUMA), label is `PUBLIC` (single authority), and arenas are
//! verified but not sharded. Each key maps to a [`CentralBin`] holding a partial-span
//! list, occupancy counters, and a per-bin spinlock.
//!
//! **Remove (W5-4b, §A.4/§A.2).** [`CentralCache::remove_batch`] pulls objects from
//! the head partial span. If no partial span exists, it returns
//! [`RemoveResult::NeedSpan`] so the **caller** creates a new span and retries (§A.2
//! OOM-retry loop) — span creation stays outside the locked central critical section
//! (DD-4).
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
//! **Span activation / return-to-backend (W5-5).** [`CentralCache::activate_span`]
//! fills the bitmap, installs the span in the pagemap (W3-6), and pushes it to the
//! partial list. [`CentralCache::deactivate_span`] removes it from the partial list,
//! transitions the pagemap, and signals the caller. Neither returns a non-empty span
//! (C-005 acceptance).

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use crate::bootstrap::MetadataAlloc;
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::{ArenaId, Label, NodeId, SizeClassId};
use crate::pagemap::{PageMap, PagemapError};
use crate::slab::SlabLayout;
use crate::span::{NonCentralResidency, SpanDescriptor, SpanState};

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

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

    /// Object index at position `i`.
    #[inline]
    pub fn index(&self, i: usize) -> u16 {
        debug_assert!(i < self.len as usize);
        self.indices[i]
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
    /// Whether the span is now **empty** and should be returned to the backend
    /// (W5-5, C-003). If `true`, the caller should call
    /// [`CentralCache::deactivate_span`].
    pub span_empty: bool,
}

// ---------------------------------------------------------------------------
// CentralLock — per-bin spinlock (§27.2)
// ---------------------------------------------------------------------------

struct CentralLock {
    locked: AtomicBool,
}

impl CentralLock {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    #[inline]
    fn acquire(&self) -> CentralGuard<'_> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        CentralGuard { lock: self }
    }
}

struct CentralGuard<'a> {
    lock: &'a CentralLock,
}

impl Drop for CentralGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// CentralBin — per-sc structure (W5-4a)
// ---------------------------------------------------------------------------

/// Per-size-class central structure (W5-4a): a partial-span list, occupancy
/// counters, and a lock. At M1 this is the only allocation path; at M2, caches
/// drain/refill through it.
pub struct CentralBin {
    /// Spinlock protecting all mutable state below.
    lock: CentralLock,
    /// Head of the partial-span singly-linked list (null when empty). Spans
    /// in this list have at least one central-free object (`central_free > 0`).
    partial_head: AtomicPtr<SpanDescriptor>,
    /// Number of spans in the partial list.
    partial_count: AtomicU32,
    /// Number of active spans (partial + exhausted) tracked by this bin.
    span_count: AtomicU32,
    /// Total central-free objects across all spans in this bin.
    total_central_free: AtomicU64,
}

impl CentralBin {
    const fn new() -> Self {
        Self {
            lock: CentralLock::new(),
            partial_head: AtomicPtr::new(core::ptr::null_mut()),
            partial_count: AtomicU32::new(0),
            span_count: AtomicU32::new(0),
            total_central_free: AtomicU64::new(0),
        }
    }

    /// Acquire the bin's lock.
    #[inline]
    fn lock(&self) -> CentralGuard<'_> {
        self.lock.acquire()
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
        // Scan for the predecessor.
        let mut prev = head;
        loop {
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
    }

    /// Number of partial spans (approximate without the lock).
    #[inline]
    pub fn partial_count(&self) -> u32 {
        self.partial_count.load(Ordering::Relaxed)
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
}

// SAFETY: all shared state is behind atomics or the spinlock; the raw pointers
// in the list reach `Sync` descriptors in monotonic metadata.
unsafe impl Sync for CentralBin {}
// SAFETY: CentralBin fields are atomics and a spinlock; the AtomicPtr reaches
// Sync descriptors that are never freed (monotonic metadata, §27.5).
unsafe impl Send for CentralBin {}

// ---------------------------------------------------------------------------
// CentralCache — the top-level container
// ---------------------------------------------------------------------------

/// The top-level central free list cache (§14.5, W5-4a). Contains a
/// [`CentralBin`] per size class; at M1 the `(node, arena, label)` key
/// collapses to `(DEFAULT, DEFAULT, PUBLIC)`.
pub struct CentralCache {
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

    // --- remove batch (W5-4b) ------------------------------------------------

    /// Remove up to `desired` objects of size class `sc` (§A.4, W5-4b).
    ///
    /// Returns [`RemoveResult::Ok`] with the batch, or [`RemoveResult::NeedSpan`]
    /// if no partial span is available — the caller should then get a new span
    /// from the backend, [`activate_span`](Self::activate_span) it, and retry
    /// (§A.2 OOM-retry).
    ///
    /// **C-001/C-002:** the returned batch is single-arena, single-label,
    /// correct-size (every object comes from one span of the right class).
    ///
    /// Lock order: `central_lock → span_lock` (W5-4d).
    pub fn remove_batch(
        &self,
        _node: NodeId,
        arena: ArenaId,
        _label: Label,
        sc: SizeClassId,
        desired: usize,
    ) -> RemoveResult {
        let bin = match self.bins.get(sc.index()) {
            Some(b) => b,
            None => return RemoveResult::NeedSpan,
        };
        let _guard = bin.lock();

        let head = bin.partial_head.load(Ordering::Relaxed);
        if head.is_null() {
            return RemoveResult::NeedSpan;
        }

        // SAFETY: head was installed by activate_span/push_partial from a valid
        // &SpanDescriptor; metadata is never freed (§27.5).
        let span = unsafe { &*head };

        // C-001: verify the span belongs to the requested arena.
        if span.arena() != arena {
            return RemoveResult::NeedSpan;
        }

        let sg = span.lock();

        // If the head span is unexpectedly exhausted, pop it and report empty.
        if sg.central_free_count() == 0 {
            drop(sg);
            bin.pop_partial();
            return RemoveResult::NeedSpan;
        }

        let count = desired
            .min(sg.central_free_count() as usize)
            .min(MAX_BATCH_LEN);

        let mut batch = Batch::empty(head as *const SpanDescriptor);
        let removed = sg.central_remove_batch(&mut batch.indices, count);
        batch.len = removed as u16;

        // Move the removed objects from central-free to live.
        let old_live = sg.live_count();
        sg.set_live_count(old_live + removed as u32);

        debug_assert!(sg.central_count_matches_bitmap());

        // If the span is now exhausted, pop it from the partial list.
        if sg.central_free_count() == 0 {
            drop(sg);
            bin.pop_partial();
        } else {
            drop(sg);
        }

        // Update the bin's aggregate counter.
        bin.total_central_free
            .fetch_sub(removed as u64, Ordering::Relaxed);

        if batch.is_empty() {
            RemoveResult::NeedSpan
        } else {
            RemoveResult::Ok(batch)
        }
    }

    // --- insert batch (W5-4c) ------------------------------------------------

    /// Return objects to the central free list (W5-4c). The objects (given by
    /// `indices[..count]`) are inserted into `span`'s bitmap + count atomically
    /// (W5-3b), and the live count is decremented. Empty detection (W5-3d/3e)
    /// runs after the insert: if the span is now empty, [`InsertResult::span_empty`]
    /// is `true` and the caller should [`deactivate_span`](Self::deactivate_span).
    ///
    /// **C-003/C-004:** an empty span is detected; a non-empty span is never
    /// returned.
    ///
    /// Lock order: `central_lock → span_lock` (W5-4d).
    pub fn insert_batch(
        &self,
        span: &SpanDescriptor,
        indices: &[u16],
        count: usize,
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

        // Insert objects into the bitmap (§8.5: bitmap + count move together).
        let max = count.min(indices.len());
        let mut inserted = 0u32;
        for &obj_idx in &indices[..max] {
            let idx = obj_idx as usize;
            if sg.central_insert(idx) {
                inserted += 1;
            } else {
                debug_assert!(
                    false,
                    "double insert in insert_batch: object {idx} already central-free"
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

        debug_assert!(sg.central_count_matches_bitmap());

        // W5-3e trigger: was this span exhausted (not in the partial list)?
        // If it now has central-free objects, add it back.
        let was_exhausted = {
            let head = bin.partial_head.load(Ordering::Relaxed);
            // A span is "in the partial list" if it's reachable from the head.
            // For efficiency, we check central_free_count: if it was 0 before
            // this insert but is now >0, the span was exhausted and needs
            // re-adding. We know it was exhausted if the count equals exactly
            // what we just inserted (it was 0 before).
            sg.central_free_count() == inserted && inserted > 0 && {
                // Double-check: not already the head (defensive).
                !core::ptr::eq(head, span)
            }
        };

        // W5-3d/3e: empty detection trigger.
        let span_empty = sg.is_empty(NonCentralResidency::NONE);

        if span_empty {
            // C-003: the span is empty. Remove from partial list if present.
            drop(sg);
            bin.remove_partial(span as *const SpanDescriptor);
        } else if was_exhausted {
            // The span was exhausted, now has free objects — re-add to partial.
            drop(sg);
            bin.push_partial(span);
        } else {
            drop(sg);
        }

        InsertResult {
            inserted: inserted as usize,
            span_empty,
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
            sg.activate(object_count);
            debug_assert!(sg.conservation_holds(NonCentralResidency::NONE));
        }

        // Step 2: install in pagemap (W3-6).
        if let Err(e) = pagemap.install_span(meta, span) {
            // Undo: clear the bitmap so the span is not in an inconsistent state.
            let sg = span.lock();
            sg.deactivate();
            return Err(e.into());
        }

        // Step 3: add to partial list under central lock.
        let bin = &self.bins[sc.index()];
        let _guard = bin.lock();
        bin.push_partial(span);
        bin.span_count.fetch_add(1, Ordering::Relaxed);
        bin.total_central_free
            .fetch_add(object_count as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Deactivate an empty span (W5-5): remove it from the partial list,
    /// transition its pagemap entries to `Released` (W3-6), and decrement the
    /// bin's span count. The caller is responsible for returning the backing
    /// extent to the backend.
    ///
    /// **C-005 acceptance:** never returns a non-empty span. The span MUST be
    /// empty before calling (debug-asserted).
    ///
    /// SPEC-transition: span `Active -> Released` (§7.3, §14.6)
    pub fn deactivate_span(&self, span: &SpanDescriptor, pagemap: &PageMap) {
        debug_assert!(
            span.is_empty_central_only(),
            "W5-5: deactivate_span called on a non-empty span"
        );

        let sc = span.size_class();
        let bin = &self.bins[sc.index()];
        let _guard = bin.lock();

        bin.remove_partial(span as *const SpanDescriptor);
        bin.span_count.fetch_sub(1, Ordering::Relaxed);

        // Clear the accounting under the span lock so the span is clean for
        // recycling.
        {
            let sg = span.lock();
            sg.deactivate();
        }

        // W3-6: transition pagemap entries.
        span.set_state(SpanState::Released);
        pagemap.release_span(span);
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
        let row = size_class::row(sc);
        SpanDescriptor::new(
            SpanId(id),
            ArenaId::DEFAULT,
            sc,
            base,
            row.slab_pages,
            row.objects_per_slab,
            0,
            m,
        )
        .expect("span creation failed")
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
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 1) {
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
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 4) {
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

        // Remove all objects.
        let mut all_indices = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all_indices.push(batch.index(i));
            }
        }
        assert_eq!(all_indices.len(), obj_count);
        assert_eq!(span.live_count() as usize, obj_count);
        assert_eq!(span.central_free_count(), 0);
        assert!(span.conservation_holds_central_only());

        // Return all objects — the span should become empty.
        let result = cache.insert_batch(&span, &all_indices, all_indices.len());
        assert_eq!(result.inserted, obj_count);
        assert!(
            result.span_empty,
            "span should be empty after returning all objects"
        );
        assert!(span.is_empty_central_only());
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
        let batch =
            match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 4) {
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

        // Drain all objects so the span is exhausted (popped from partial list).
        let mut all = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all.push(batch.index(i));
            }
        }
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);

        // Return some (not all) objects. The span should re-enter the partial list.
        let some: Vec<u16> = all[..4].to_vec();
        let result = cache.insert_batch(&span, &some, 4);
        assert_eq!(result.inserted, 4);
        assert!(!result.span_empty);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 1);

        // We can now remove from it again.
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 2) {
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

        // Return all objects to make it empty, then deactivate.
        let mut all = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all.push(batch.index(i));
            }
        }
        let result = cache.insert_batch(&span, &all, all.len());
        assert!(result.span_empty);

        cache.deactivate_span(&span, &pm);
        assert_eq!(cache.bin(sc).unwrap().span_count(), 0);
        assert_eq!(cache.bin(sc).unwrap().partial_count(), 0);
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
        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 1) {
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
            let batch =
                match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 8) {
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

        match cache.remove_batch(NodeId::DEFAULT, ArenaId::DEFAULT, Label::PUBLIC, sc, 8) {
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

        // Drain all objects.
        let mut all = Vec::new();
        while let RemoveResult::Ok(batch) = cache.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all.push(batch.index(i));
            }
        }
        assert_eq!(all.len(), obj_count);

        // Return all but the last one — NOT empty.
        let (most, last) = all.split_at(obj_count - 1);
        let result = cache.insert_batch(&span, most, most.len());
        assert_eq!(result.inserted, obj_count - 1);
        assert!(!result.span_empty);

        // Return the last one — NOW empty.
        let result = cache.insert_batch(&span, last, 1);
        assert_eq!(result.inserted, 1);
        assert!(
            result.span_empty,
            "span should be empty after the last object returns"
        );
    }
}

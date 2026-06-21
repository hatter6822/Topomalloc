// SPDX-License-Identifier: MIT
//! Refill / flush operations connecting the cache layers (W6-3a/3b/3c, plan 05).
//!
//! These functions orchestrate the hand-over-hand lock protocol between the
//! per-CPU cache, the transfer cache, and the central free list. The critical
//! rule (SS27.2) is that no two middle-end locks are ever held simultaneously:
//!
//! - **Refill** (W6-3a): lock transfer -> pop batch -> unlock transfer ->
//!   (if empty) central.remove_batch -> convert indices to addresses ->
//!   lock CPU -> push into slot -> unlock CPU.
//!
//! - **Flush** (W6-3b): lock CPU -> pop batch from slot -> unlock CPU ->
//!   lock transfer -> push batch -> unlock transfer ->
//!   (if transfer full) classify by span -> central.insert_batch per span.
//!
//! - **Flush-to-central with empty detection** (W6-3c): when flushing to
//!   central, after insert_batch, check if `span_empty` is true. If so,
//!   the caller should deactivate the span.
//!
//! - **flush_idle_cpu** (W6-7): flush all slots of a specific CPU.
//!
//! **Lock ordering.** Transfer (rank 3) < Central (rank 4) < Span (rank 5).
//! The transfer lock is always released BEFORE the central lock is acquired.

use crate::bootstrap::MetadataAlloc;
use crate::central::{CentralCache, InsertResult, RemoveResult};
use crate::cpu_cache::CpuCache;
use crate::fe::CoreId;
use crate::ids::{ArenaId, Label, NodeId, SizeClassId};
use crate::pagemap::{PageEntry, PageMap};
use crate::slab::SlabLayout;
use crate::span::SpanDescriptor;
use crate::transfer_cache::TransferCache;
use crate::{size_class, MAX_BATCH_LEN};

/// Result of a refill operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefillResult {
    /// Number of objects pushed into the CPU cache slot.
    pub filled: usize,
    /// Whether a new span is needed (central returned NeedSpan).
    pub need_span: bool,
}

/// Result of a flush operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushResult {
    /// Number of objects flushed from the CPU cache slot.
    pub flushed: usize,
    /// Spans detected as empty during the flush-to-central path (W6-3c).
    /// The caller should deactivate these spans.
    pub spans_emptied: usize,
}

/// Refill the per-CPU cache for `(core, sc)` from the transfer cache or
/// central free list (W6-3a).
///
/// Algorithm:
/// 1. Lock transfer -> try_pop_batch -> unlock transfer
/// 2. If got objects: lock CPU -> push into slot -> unlock CPU -> return
/// 3. If transfer empty: central.remove_batch -> convert to addresses ->
///    lock CPU -> push into slot -> unlock CPU -> return
///
/// Returns the number of objects refilled, and whether a new span is needed.
///
/// **Hand-over-hand:** the transfer lock is released BEFORE the central lock
/// is acquired.
///
/// **OOM-retry contract.** When `need_span` is `true`, the central free list
/// is empty for this `(node, arena, label, sc)`. The caller must:
/// 1. Create a new span from the backend (§A.2 OOM-retry).
/// 2. Activate it via [`CentralCache::activate_span`].
/// 3. Call `refill` again.
///
/// Use [`refill_with_retry`] for a convenience wrapper that automates
/// the retry loop with a caller-provided span-creation callback.
#[allow(clippy::too_many_arguments)]
pub fn refill(
    core: CoreId,
    node: NodeId,
    arena: ArenaId,
    label: Label,
    sc: SizeClassId,
    cpu_cache: &CpuCache,
    transfer: &TransferCache,
    central: &CentralCache,
    _pagemap: &PageMap,
    meta: &dyn MetadataAlloc,
) -> RefillResult {
    let batch_size = size_class::batch(sc);
    let mut buf = [0usize; MAX_BATCH_LEN];

    // §30.4 (W19-3): deterministic mode can force the slow path — skip the transfer
    // cache and refill straight from the central free list, so the slower central
    // path is exercised every time. Off by default (one relaxed load).
    let popped = if crate::deterministic::force_slow_path() {
        0
    } else {
        // Step 1: try the transfer cache (lock rank 3, released before step 3).
        transfer.try_pop_batch(arena, sc, &mut buf, batch_size, meta)
    };

    if popped > 0 {
        // Step 2: push into CPU cache slot (per-CPU lock, released on return).
        let pushed = cpu_cache.push_batch(core, sc, &buf[..popped]);
        // Return any unpushed objects to the transfer cache to prevent leaks.
        if pushed < popped {
            transfer.try_push_batch(arena, sc, &buf[pushed..popped], meta);
        }
        return RefillResult {
            filled: pushed,
            need_span: false,
        };
    }

    // Step 3: transfer was empty. Try the central free list.
    // The transfer lock was released in step 1 (by try_pop_batch dropping
    // its guard). Now acquire the central lock. The cache-refill path carries no per-request
    // placement class (it batches across requests), so it does not bias span grouping
    // (`ANY_PLACE_CLASS`); per-request §24 grouping happens on the central small path. Cache-
    // layer grouping is an M2 concern (W6).
    match central.remove_batch(
        node,
        arena,
        label,
        sc,
        crate::central::ANY_PLACE_CLASS,
        batch_size,
    ) {
        RemoveResult::NeedSpan => RefillResult {
            filled: 0,
            need_span: true,
        },
        RemoveResult::Ok(batch) => {
            // Convert batch indices to addresses using SlabLayout.
            // SAFETY: batch.span() is a valid pointer to a SpanDescriptor
            // that is live (it came from the central free list, which only
            // holds active spans in monotonic metadata).
            let span = unsafe { &*batch.span() };
            let base = span.base();
            let slab_header = span.slab_header() as usize;
            let layout = match SlabLayout::compute(sc, base, slab_header) {
                Some(l) => l,
                None => {
                    return RefillResult {
                        filled: 0,
                        need_span: true,
                    }
                }
            };

            let mut addr_count = 0usize;
            let mut addrs = [0usize; MAX_BATCH_LEN];
            for i in 0..batch.len() {
                if let Some(addr) = layout.object_addr(batch.index(i) as usize) {
                    if addr_count < MAX_BATCH_LEN {
                        addrs[addr_count] = addr;
                        addr_count += 1;
                    }
                }
            }
            // B.2 (cache refill preserves object count, W19-1b): every object the
            // central list handed out converts to an address — the index→address
            // mapping never drops an object (the indices are in `0..object_count`,
            // so `object_addr` always succeeds and `batch.len() <= batch_size <=
            // MAX_BATCH_LEN` never trips the cap). A mismatch is a refill leak.
            debug_assert_eq!(
                addr_count,
                batch.len(),
                "refill lost objects converting central indices to addresses"
            );

            // Push addresses into the CPU cache slot.
            let pushed = cpu_cache.push_batch(core, sc, &addrs[..addr_count]);
            // Return any unpushed objects to the transfer cache to prevent leaks.
            if pushed < addr_count {
                transfer.try_push_batch(arena, sc, &addrs[pushed..addr_count], meta);
            }
            RefillResult {
                filled: pushed,
                need_span: false,
            }
        }
    }
}

/// Convenience wrapper around [`refill`] that retries when the central free
/// list is empty, using a caller-provided callback to create and activate
/// new spans.
///
/// `create_span` is called with `sc` and should return `true` if a span was
/// successfully created and activated in the central cache. The loop retries
/// up to `max_retries` times. No locks are held during the callback.
#[allow(clippy::too_many_arguments)]
pub fn refill_with_retry<F>(
    core: CoreId,
    node: NodeId,
    arena: ArenaId,
    label: Label,
    sc: SizeClassId,
    cpu_cache: &CpuCache,
    transfer: &TransferCache,
    central: &CentralCache,
    pagemap: &PageMap,
    meta: &dyn MetadataAlloc,
    max_retries: usize,
    mut create_span: F,
) -> RefillResult
where
    F: FnMut(SizeClassId) -> bool,
{
    for _ in 0..=max_retries {
        let r = refill(
            core, node, arena, label, sc, cpu_cache, transfer, central, pagemap, meta,
        );
        if !r.need_span || r.filled > 0 {
            return r;
        }
        if !create_span(sc) {
            return r;
        }
    }
    RefillResult {
        filled: 0,
        need_span: true,
    }
}

/// Flush the per-CPU cache for `(core, sc)` to the transfer cache or central
/// free list (W6-3b, W6-3c).
///
/// Algorithm:
/// 1. Lock CPU -> pop batch_size objects from slot -> unlock CPU
/// 2. Lock transfer -> try_push_batch -> unlock transfer
/// 3. If transfer full: classify each address via pagemap -> group by span ->
///    for each span: compute indices, central.insert_batch
///
/// Returns the number of objects flushed and the number of spans emptied.
///
/// **Hand-over-hand:** the transfer lock is released BEFORE the central lock
/// is acquired (step 3).
#[allow(clippy::too_many_arguments)]
pub fn flush(
    core: CoreId,
    arena: ArenaId,
    sc: SizeClassId,
    cpu_cache: &CpuCache,
    transfer: &TransferCache,
    central: &CentralCache,
    pagemap: &PageMap,
    meta: &dyn MetadataAlloc,
) -> FlushResult {
    let batch_size = size_class::batch(sc);
    let mut buf = [0usize; MAX_BATCH_LEN];

    // Step 1: pop from CPU cache slot (per-CPU lock).
    let popped = cpu_cache.pop_batch(core, sc, &mut buf, batch_size);
    if popped == 0 {
        return FlushResult {
            flushed: 0,
            spans_emptied: 0,
        };
    }

    // Step 2: try pushing to transfer cache (lock rank 3).
    let pushed_to_transfer = transfer.try_push_batch(arena, sc, &buf[..popped], meta);

    if pushed_to_transfer >= popped {
        return FlushResult {
            flushed: popped,
            spans_emptied: 0,
        };
    }

    // Step 3: transfer was full (partially or fully). Flush remainder to
    // central free list. The transfer lock was released in step 2.
    let remainder = &buf[pushed_to_transfer..popped];
    // B.2 (cache flush preserves object count, W19-1b): every object popped from
    // the CPU cache is routed exactly once — `pushed_to_transfer` to the transfer
    // cache and the rest to central — so the two destinations partition `popped`
    // with nothing created or dropped at the routing step.
    debug_assert_eq!(
        pushed_to_transfer + remainder.len(),
        popped,
        "flush routing did not preserve the popped object count"
    );
    let spans_emptied = flush_addrs_to_central(remainder, sc, central, pagemap);

    FlushResult {
        flushed: popped,
        spans_emptied,
    }
}

/// Flush all slots of a specific CPU (W6-7 idle-CPU flush; W7-4 coordination).
///
/// Drains every initialized slot of `core` into the transfer cache (overflow to
/// central), returning the total number of objects flushed. **Plan 07 hook site:**
/// when background memory management lands, this is the natural place to notify
/// the extent manager about emptied spans and reclaimable memory.
///
/// **W7-4.** This goes through [`CpuCache::drain_cpu`], which holds the per-CPU
/// lock for the whole drain and issues the RSEQ fence **once** when `core` is a
/// non-owner CPU — rather than one membarrier per size class (a membarrier is an
/// all-CPU IPI). The transfer→central moves run hand-over-hand under the
/// (outermost) per-CPU lock.
pub fn flush_idle_cpu(
    core: CoreId,
    arena: ArenaId,
    cpu_cache: &CpuCache,
    transfer: &TransferCache,
    central: &CentralCache,
    pagemap: &PageMap,
    meta: &dyn MetadataAlloc,
) -> usize {
    let mut buf = [0usize; MAX_BATCH_LEN];
    cpu_cache.drain_cpu(core, &mut buf, |sc, batch| {
        // Push to the transfer cache; overflow goes to central (empty-span
        // detection fires there, W6-3c). Same hand-over-hand discipline as `flush`.
        let pushed = transfer.try_push_batch(arena, sc, batch, meta);
        if pushed < batch.len() {
            let _ = flush_addrs_to_central(&batch[pushed..], sc, central, pagemap);
        }
    })
}

/// Flush a set of addresses to the central free list, classifying each by
/// span via the pagemap. Returns the number of spans detected as empty
/// (W6-3c).
///
/// Processes addresses in chunks of `MAX_BATCH_LEN` to handle arbitrarily
/// large input slices.
fn flush_addrs_to_central(
    addrs: &[usize],
    sc: SizeClassId,
    central: &CentralCache,
    pagemap: &PageMap,
) -> usize {
    let mut spans_emptied = 0usize;
    let mut offset = 0usize;

    while offset < addrs.len() {
        let chunk_end = (offset + MAX_BATCH_LEN).min(addrs.len());
        let chunk = &addrs[offset..chunk_end];
        offset = chunk_end;

        // Collect (span_ptr, index) pairs for this chunk.
        let mut entries: [SpanIndex; MAX_BATCH_LEN] = [SpanIndex::EMPTY; MAX_BATCH_LEN];
        let mut entry_count = 0usize;

        for &addr in chunk {
            if let PageEntry::Small(span_ptr) = pagemap.lookup(addr) {
                if span_ptr.is_null() {
                    continue;
                }
                // SAFETY: span_ptr is a valid pointer to a SpanDescriptor in
                // monotonic metadata (never freed), obtained from the pagemap
                // which only stores valid pointers.
                let span = unsafe { &*span_ptr };
                let base = span.base();
                let slab_hdr = span.slab_header() as usize;
                let layout = match SlabLayout::compute(sc, base, slab_hdr) {
                    Some(l) => l,
                    None => continue,
                };
                if let Some(idx) = layout.addr_to_index(addr) {
                    entries[entry_count] = SpanIndex {
                        span_ptr,
                        index: idx as u16,
                    };
                    entry_count += 1;
                }
            }
        }

        // Group by span and insert batches.
        // Simple O(n^2) grouping -- fine for batch sizes <= 32.
        let mut processed = [false; MAX_BATCH_LEN];
        for i in 0..entry_count {
            if processed[i] {
                continue;
            }
            let target_span = entries[i].span_ptr;
            let mut indices = [0u16; MAX_BATCH_LEN];
            let mut idx_count = 0usize;

            // Gather all entries for this span.
            for j in i..entry_count {
                if !processed[j] && core::ptr::eq(entries[j].span_ptr, target_span) {
                    indices[idx_count] = entries[j].index;
                    idx_count += 1;
                    processed[j] = true;
                }
            }

            if idx_count > 0 {
                // SAFETY: target_span is a valid pointer to a SpanDescriptor
                // in monotonic metadata, obtained from the pagemap.
                let span = unsafe { &*target_span };
                let result: InsertResult =
                    central.insert_batch(span, &indices[..idx_count], idx_count);

                // W6-3c: empty detection.
                if result.span_empty {
                    spans_emptied = spans_emptied.saturating_add(1);
                }
            }
        }
    }

    spans_emptied
}

/// A (span pointer, object index) pair for flush grouping.
#[derive(Clone, Copy)]
struct SpanIndex {
    span_ptr: *const SpanDescriptor,
    index: u16,
}

impl SpanIndex {
    const EMPTY: SpanIndex = SpanIndex {
        span_ptr: core::ptr::null(),
        index: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::central::CentralCache;
    use crate::cpu_cache::CpuCache;
    use crate::ids::{ArenaId, SpanId};
    use crate::pagemap::PageMap;
    use crate::span::SpanDescriptor;
    use crate::transfer_cache::TransferCache;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn make_span(id: u32, sc: SizeClassId, base: usize, m: &BumpArena) -> SpanDescriptor {
        make_span_with_header(id, sc, base, 0, m)
    }

    fn make_span_with_header(
        id: u32,
        sc: SizeClassId,
        base: usize,
        slab_header: u32,
        m: &BumpArena,
    ) -> SpanDescriptor {
        let row = size_class::row(sc);
        // A non-zero header shifts object0 forward and shrinks the usable region,
        // so the geometrically-valid object count is the header-adjusted fit
        // (B.3 "object ranges fit within span") — exactly what the real span-
        // creation path derives via `SlabLayout`. Using `objects_per_slab` here
        // would over-claim the count past what fits.
        let object_count = SlabLayout::compute(sc, base, slab_header as usize)
            .map_or(row.objects_per_slab, |l| l.object_count as u32);
        SpanDescriptor::new(
            SpanId(id),
            ArenaId::DEFAULT,
            sc,
            base,
            row.slab_pages,
            object_count,
            slab_header,
            m,
        )
        .expect("span creation failed")
    }

    #[test]
    fn flush_refill_round_trip_conserves_objects_and_invariants() {
        // B.2 (W19-1c): a refill (central → CPU) followed by a flush (CPU →
        // central) neither creates nor destroys an object, and every layer's
        // Appendix-B checker holds at each step. The span's §16.4 law
        // `live + central_free == object_count` is the conservation witness
        // (cache residency keeps `live_count` incremented).
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3); // 64-byte class
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        let span = make_span(1, sc, base, &m);
        let obj_count = size_class::row(sc).objects_per_slab;
        central.activate_span(&span, &pm, &m).unwrap();
        cc.init_slot(core, sc, &m, 32);

        // Invariants hold and everything is central-free initially.
        assert!(cc.check_invariants() && tc.check_invariants() && central.check_invariants());
        assert_eq!(span.central_free_count(), obj_count);
        assert_eq!(span.live_count(), 0);

        // Refill: pull a batch from central into the CPU cache.
        let r = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert!(r.filled > 0 && !r.need_span);
        // Conservation: the objects pulled into the cache are now "live" (held by
        // the cache), the rest remain central-free, and the two sum to the slab.
        assert_eq!(span.live_count(), r.filled as u32);
        assert_eq!(span.central_free_count(), obj_count - r.filled as u32);
        assert_eq!(span.live_count() + span.central_free_count(), obj_count);
        assert!(cc.check_invariants() && tc.check_invariants() && central.check_invariants());
        assert!(span.check_invariants());

        // Flush: push the CPU cache's objects back down (to transfer here, since
        // it is not full). No object is created or lost.
        let f = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert_eq!(f.flushed, r.filled);
        // The objects are now in the transfer cache; conservation still holds.
        assert!(cc.check_invariants() && tc.check_invariants() && central.check_invariants());
        assert_eq!(span.live_count() + span.central_free_count(), obj_count);
        assert!(span.check_invariants());
    }

    #[test]
    fn refill_from_transfer_cache() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;

        // Pre-populate the transfer cache.
        let addrs: Vec<usize> = (1000..1032).collect();
        let pushed = tc.try_push_batch(ArenaId::DEFAULT, sc, &addrs, &m);
        assert!(pushed > 0);

        // Init the CPU slot.
        cc.init_slot(core, sc, &m, 32);

        // Refill should pull from transfer cache.
        let result = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert!(result.filled > 0);
        assert!(!result.need_span);
    }

    #[test]
    fn refill_from_central_when_transfer_empty() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        // Activate a span in central.
        let span = make_span(1, sc, base, &m);
        central.activate_span(&span, &pm, &m).unwrap();

        // Init the CPU slot.
        cc.init_slot(core, sc, &m, 32);

        // Transfer cache is empty, so refill should go to central.
        let result = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert!(result.filled > 0);
        assert!(!result.need_span);

        // Conservation law holds.
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn refill_need_span_when_central_empty() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;

        cc.init_slot(core, sc, &m, 32);

        // Both transfer and central are empty.
        let result = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert_eq!(result.filled, 0);
        assert!(result.need_span);
    }

    #[test]
    fn flush_to_transfer_cache() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;

        cc.init_slot(core, sc, &m, 32);

        // Push some addresses into the CPU cache.
        let addrs: Vec<usize> = (2000..2016).collect();
        cc.push_batch(core, sc, &addrs);

        // Flush should push to transfer cache.
        let result = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert!(result.flushed > 0);

        // Transfer cache should have the addresses.
        let bin = tc.bin(sc).unwrap();
        assert!(!bin.is_empty());
    }

    #[test]
    fn flush_to_central_when_transfer_full() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        // Activate a span so central.insert_batch has somewhere to go.
        let span = make_span(1, sc, base, &m);
        central.activate_span(&span, &pm, &m).unwrap();

        // Remove all objects from central to simulate live objects.
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as usize;
        let mut all_indices = Vec::new();
        while let RemoveResult::Ok(batch) = central.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            crate::central::ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all_indices.push(batch.index(i));
            }
        }
        assert_eq!(all_indices.len(), obj_count);

        // Fill the transfer cache to capacity.
        let transfer_cap = size_class::batch(sc) * 4; // default capacity
        let dummy_addrs: Vec<usize> = (5000..5000 + transfer_cap).collect();
        tc.try_push_batch(ArenaId::DEFAULT, sc, &dummy_addrs, &m);

        // Compute addresses from indices and put some in the CPU cache.
        let layout = SlabLayout::compute(sc, base, 0).unwrap();
        cc.init_slot(core, sc, &m, 32);

        let batch_size = size_class::batch(sc).min(all_indices.len());
        let mut addrs_to_flush = Vec::new();
        for &idx in &all_indices[..batch_size] {
            if let Some(addr) = layout.object_addr(idx as usize) {
                addrs_to_flush.push(addr);
            }
        }
        cc.push_batch(core, sc, &addrs_to_flush);

        // Flush -- transfer is full, so objects go to central.
        let result = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert!(result.flushed > 0);

        // Conservation law should hold on the span.
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn flush_idle_cpu_drains_all_slots() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let core = CoreId::DEFAULT;

        // Initialize and populate two size classes.
        let sc0 = SizeClassId::new(0);
        let sc1 = SizeClassId::new(1);
        cc.init_slot(core, sc0, &m, 32);
        cc.init_slot(core, sc1, &m, 32);

        cc.push_batch(core, sc0, &[100, 200, 300]);
        cc.push_batch(core, sc1, &[400, 500]);

        let total = flush_idle_cpu(core, ArenaId::DEFAULT, &cc, &tc, &central, &pm, &m);
        assert_eq!(total, 5);

        // CPU slots should be empty.
        let cpu = cc.per_cpu(core).unwrap();
        assert_eq!(cpu.slot(sc0).unwrap().len(), 0);
        assert_eq!(cpu.slot(sc1).unwrap().len(), 0);
    }

    #[test]
    fn empty_span_detection_on_flush() {
        // W6-3c: flush to central detects empty span.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let base = 0x4000_0000usize;

        // Activate a span.
        let span = make_span(1, sc, base, &m);
        central.activate_span(&span, &pm, &m).unwrap();

        // Remove all objects from central.
        let row = size_class::row(sc);
        let obj_count = row.objects_per_slab as usize;
        let mut all_indices = Vec::new();
        while let RemoveResult::Ok(batch) = central.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            crate::central::ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all_indices.push(batch.index(i));
            }
        }
        assert_eq!(all_indices.len(), obj_count);

        // Convert all indices to addresses.
        let layout = SlabLayout::compute(sc, base, 0).unwrap();
        let all_addrs: Vec<usize> = all_indices
            .iter()
            .filter_map(|&idx| layout.object_addr(idx as usize))
            .collect();
        assert_eq!(all_addrs.len(), obj_count);

        // Flush all addresses to central via the internal helper.
        let spans_emptied = flush_addrs_to_central(&all_addrs, sc, &central, &pm);

        // The span should be detected as empty.
        assert!(span.is_empty_central_only());
        // Note: spans_emptied may or may not be 1 depending on whether the
        // empty-span cache was full. The important thing is the span is empty.
        // (With MAX_EMPTY_CACHED_PER_BIN=1, the first empty span is cached,
        // so span_empty is false. But the span IS empty.)
        let _ = spans_emptied; // the span emptiness is confirmed above
    }

    #[test]
    fn refill_flush_cycle_preserves_conservation() {
        // Refill from central, then flush back -- conservation should hold.
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        let span = make_span(1, sc, base, &m);
        central.activate_span(&span, &pm, &m).unwrap();
        cc.init_slot(core, sc, &m, 32);

        // Refill from central.
        let r1 = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert!(r1.filled > 0);
        assert!(span.conservation_holds_central_only());

        // Flush back (via transfer or central).
        let r2 = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert!(r2.flushed > 0);

        // The transfer cache has some objects now; they are logically in the
        // transfer bucket. The conservation law still holds on the span
        // because live_count tracks objects removed from central (including
        // those that are now in caches).
        // Note: after flush to transfer, the span's live_count is unchanged
        // (objects moved from CPU cache to transfer cache, not back to central).
        // Conservation holds because live_count includes cached objects.
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn refill_with_retry_creates_span_and_succeeds() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        cc.init_slot(core, sc, &m, 32);

        // First attempt: central is empty → need_span.
        // The create_span callback activates a span, so the retry succeeds.
        let span = make_span(1, sc, base, &m);
        let span_ref = &span;
        let central_ref = &central;
        let pm_ref = &pm;
        let m_ref = &m;
        let mut called = 0u32;

        let result = refill_with_retry(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            central_ref,
            pm_ref,
            m_ref,
            3,
            |_sc| {
                called += 1;
                central_ref.activate_span(span_ref, pm_ref, m_ref).is_ok()
            },
        );

        assert!(result.filled > 0);
        assert!(!result.need_span);
        assert_eq!(called, 1);
    }

    #[test]
    fn refill_with_retry_respects_max_retries() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;

        cc.init_slot(core, sc, &m, 32);

        let mut called = 0u32;
        let result = refill_with_retry(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
            3,
            |_sc| {
                called += 1;
                false // always fail
            },
        );

        assert_eq!(result.filled, 0);
        assert!(result.need_span);
        assert_eq!(called, 1); // fails on first attempt, stops
    }

    #[test]
    fn refill_partial_push_returns_remainder_to_transfer() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let hard_cap = size_class::max_local_capacity(sc);

        // Init CPU slot normally (hard_cap from size class table).
        cc.init_slot(core, sc, &m, hard_cap as u32);

        // Pre-fill the slot to near capacity, leaving room for only 4 objects.
        let fill_count = hard_cap - 4;
        let fill_addrs: Vec<usize> = (10_000..10_000 + fill_count).collect();
        cc.push_batch(core, sc, &fill_addrs);

        // Pre-populate transfer cache with a full batch (32 addresses).
        let batch_size = size_class::batch(sc);
        let addrs: Vec<usize> = (1000..1000 + batch_size).collect();
        let pushed = tc.try_push_batch(ArenaId::DEFAULT, sc, &addrs, &m);
        assert_eq!(pushed, batch_size);

        // Refill: pops batch_size from transfer, but CPU slot can only fit 4.
        // The remaining (batch_size - 4) should be returned to transfer.
        let result = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert_eq!(result.filled, 4);
        assert!(!result.need_span);

        // The remainder should be back in the transfer cache.
        let bin = tc.bin(sc).unwrap();
        assert!(
            !bin.is_empty(),
            "unpushed objects should be returned to transfer"
        );
        assert_eq!(bin.len() as usize, batch_size - 4);
    }

    #[test]
    fn flush_partial_transfer_push_goes_to_central() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;

        // Activate a span in central.
        let span = make_span(1, sc, base, &m);
        central.activate_span(&span, &pm, &m).unwrap();

        // Remove objects from central, convert to addresses.
        let mut all_indices = Vec::new();
        while let RemoveResult::Ok(batch) = central.remove_batch(
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            crate::central::ANY_PLACE_CLASS,
            MAX_BATCH_LEN,
        ) {
            for i in 0..batch.len() {
                all_indices.push(batch.index(i));
            }
        }

        // Partially fill the transfer cache so it can only accept a few more.
        let transfer_cap = size_class::batch(sc) * 4;
        let fill_count = transfer_cap - 2; // leave room for only 2
        let dummy_addrs: Vec<usize> = (5000..5000 + fill_count).collect();
        tc.try_push_batch(ArenaId::DEFAULT, sc, &dummy_addrs, &m);

        // Put some real addresses into the CPU cache.
        let layout = SlabLayout::compute(sc, base, 0).unwrap();
        cc.init_slot(core, sc, &m, 32);
        let batch_size = size_class::batch(sc).min(all_indices.len());
        let mut addrs_to_flush = Vec::new();
        for &idx in &all_indices[..batch_size] {
            if let Some(addr) = layout.object_addr(idx as usize) {
                addrs_to_flush.push(addr);
            }
        }
        cc.push_batch(core, sc, &addrs_to_flush);

        // Flush: transfer can only accept 2, rest goes to central.
        let result = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert!(result.flushed > 0);

        // Conservation law should hold.
        assert!(span.conservation_holds_central_only());
    }

    #[test]
    fn flush_idle_cpu_handles_transfer_full() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let core = CoreId::DEFAULT;

        // Initialize and populate one SC.
        let sc0 = SizeClassId::new(0);
        cc.init_slot(core, sc0, &m, 32);
        cc.push_batch(core, sc0, &[100, 200, 300]);

        // Fill transfer cache so objects must go to central.
        let transfer_cap = size_class::batch(sc0) * 4;
        let dummy: Vec<usize> = (5000..5000 + transfer_cap).collect();
        tc.try_push_batch(ArenaId::DEFAULT, sc0, &dummy, &m);

        // flush_idle_cpu should still drain the slot even if transfer is full.
        let total = flush_idle_cpu(core, ArenaId::DEFAULT, &cc, &tc, &central, &pm, &m);
        assert_eq!(total, 3);

        let cpu = cc.per_cpu(core).unwrap();
        assert_eq!(cpu.slot(sc0).unwrap().len(), 0);
    }

    #[test]
    fn nonzero_slab_header_refill_flush_round_trip() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let cc = CpuCache::new();
        let tc = TransferCache::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3);
        let core = CoreId::DEFAULT;
        let base = 0x4000_0000usize;
        let slab_header = 64u32;

        let span = make_span_with_header(1, sc, base, slab_header, &m);
        central.activate_span(&span, &pm, &m).unwrap();

        let layout_hdr = SlabLayout::compute(sc, base, slab_header as usize).unwrap();
        let layout_no_hdr = SlabLayout::compute(sc, base, 0).unwrap();
        assert!(
            layout_hdr.object0 > layout_no_hdr.object0,
            "header should shift object0 forward"
        );

        cc.init_slot(core, sc, &m, 32);

        // Refill from central — addresses must use the header-shifted layout.
        let r1 = refill(
            core,
            NodeId::DEFAULT,
            ArenaId::DEFAULT,
            Label::PUBLIC,
            sc,
            &cc,
            &tc,
            &central,
            &pm,
            &m,
        );
        assert!(r1.filled > 0);

        // Every address in the CPU slot must be valid under the header layout.
        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        let mut popped = [0usize; MAX_BATCH_LEN];
        let n = cc.pop_batch(core, sc, &mut popped, slot.len() as usize);
        for &addr in &popped[..n] {
            assert!(
                layout_hdr.addr_to_index(addr).is_some(),
                "addr {:#x} should be valid under header layout",
                addr
            );
        }

        // Push them back and flush to central.
        cc.push_batch(core, sc, &popped[..n]);
        let r2 = flush(core, ArenaId::DEFAULT, sc, &cc, &tc, &central, &pm, &m);
        assert_eq!(r2.flushed, n);

        assert!(span.conservation_holds_central_only());
    }
}

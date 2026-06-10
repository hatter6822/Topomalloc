// SPDX-License-Identifier: MIT
//! The M1 integrated allocator over the **central path** (plan 06 W8-1a).
//!
//! This composes the proven plan-03/04 parts into one allocator — the
//! "minimal correct sequential allocator" of SPEC §37.1:
//!
//! * **classify** (W2) splits a request into Small / Medium / Large (§A.1);
//! * **Small** is served object-at-a-time from the [`CentralCache`] (W5) —
//!   `remove_batch(desired = 1)` on `malloc`, `insert_batch` on `free` — with
//!   spans carved from a dedicated [`ExtentManager`] region and recycled
//!   through a metadata-backed span pool (§27.5: descriptors are never
//!   freed, only recycled behind a generation bump);
//! * **Medium and Large** are page-rounded extents served by the
//!   [`LargeAllocator`] (W4-4), classifiable via their [`LargeDescriptor`]s;
//! * **free** validates ownership through the pagemap first
//!   ([`validate_free`], W3-4): foreign, interior, metadata, released and
//!   quarantined pointers are rejected without touching any state (§35.2).
//!
//! **This is the M1 path: no front-end caches.** Every operation goes through
//! the central list's locks (the "global lock" of the milestone ladder, §5).
//! Plan 05's per-CPU/thread caches are wired *under* the same API at M2
//! (W16-4 owns that transition); nothing here changes when they arrive.
//!
//! **Concurrency.** The allocator is thread-safe: the central cache, the
//! extent managers, and the large allocator each carry their own §27.2 locks,
//! and the span pool is guarded by a backend-class lock. The only cross-
//! structure protocol this module adds is the span lifecycle, and both of its
//! transitions are single-owner by construction: a span is *created* by the
//! thread that observed `NeedSpan` (a lost race merely activates one extra
//! span), and *retired* by exactly the thread whose insert emptied it
//! ([`InsertResult::span_empty`] is reported to at most one caller — W5-5
//! exclusivity, enforced in `central.rs`).
//!
//! [`LargeDescriptor`]: crate::span::LargeDescriptor
//! [`InsertResult::span_empty`]: crate::central::InsertResult

use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::backend::TopoBackingProvider;
use crate::bootstrap::{BumpArena, MetadataAlloc};
use crate::central::{CentralCache, RemoveResult};
use crate::classify::{classify, RequestKind};
use crate::error::BackendError;
use crate::extent::{BackendLock, ExtentError, ExtentId, ExtentManager, ExtentRef, Fit};
use crate::flags::RequestFlags;
use crate::generated::tables::PAGE_SIZE;
use crate::ids::{ArenaId, Label, NodeId, SizeClassId, SpanId};
use crate::large::{LargeAllocator, LargeConfig};
use crate::overflow::align_up;
use crate::pagemap::PageMap;
use crate::ptr_class::{
    classify_ptr, validate_free, FreeTarget, InvalidFree, MetadataRegion, PointerClass,
};
use crate::size_class;
use crate::skeleton::MIN_ALIGN;
use crate::slab::SlabLayout;
use crate::span::SpanDescriptor;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Sizing for an [`Allocator`]: the two reserved regions and the descriptor
/// pool capacities. All regions are *virtual* reservations — on the POSIX
/// performance backend they are lazily faulted, so a generous default costs
/// address space, not RSS. The pools are fixed-capacity metadata allocations
/// (the §27.5 monotonic-metadata discipline: never freed, recycled in place),
/// so they bound the number of *concurrently live* spans and medium/large
/// allocations; exhaustion is a clean `null` (ENOMEM at the ABI), never a wrap.
#[derive(Clone, Copy, Debug)]
pub struct AllocatorConfig {
    /// Bytes of virtual region reserved for small-object spans.
    pub span_region_bytes: usize,
    /// Extent-descriptor pool capacity for the span region.
    pub span_extent_slots: usize,
    /// Span-descriptor pool capacity (max concurrently active spans).
    pub span_slots: usize,
    /// Bytes of virtual region reserved for medium/large allocations.
    pub large_region_bytes: usize,
    /// Extent-descriptor pool capacity for the medium/large region.
    pub large_extent_slots: usize,
    /// Large-descriptor pool capacity (max concurrently live medium/large
    /// allocations).
    pub large_slots: usize,
}

impl AllocatorConfig {
    /// A small configuration for tests and the seLe4n simulator, whose
    /// authorized-untyped pool is charged eagerly for every reservation
    /// (regions must stay modest there, unlike lazy POSIX mappings).
    pub const fn small() -> Self {
        Self {
            span_region_bytes: 16 * 1024 * 1024,
            span_extent_slots: 512,
            span_slots: 512,
            large_region_bytes: 32 * 1024 * 1024,
            large_extent_slots: 256,
            large_slots: 256,
        }
    }

    /// Metadata bytes the fixed pools of this configuration consume up front
    /// (span pool + both extent pools + large pool), excluding pagemap nodes
    /// and out-of-line span bitmaps which grow with use. Useful for sizing the
    /// metadata arena; saturates rather than wrapping on absurd configs.
    pub const fn fixed_pool_metadata_bytes(&self) -> usize {
        let span_pool = self
            .span_slots
            .saturating_mul(core::mem::size_of::<SpanSlot>());
        // The extent Slot and LargeSlot types are private to their modules;
        // 128 bytes is a deliberately generous per-slot bound for both (the
        // extent slot is ~88 B, the large slot ~104 B), kept conservative so
        // arena sizing computed from this can never under-provision.
        let extent_slots = self
            .span_extent_slots
            .saturating_add(self.large_extent_slots)
            .saturating_mul(128);
        let large_pool = self.large_slots.saturating_mul(128);
        span_pool
            .saturating_add(extent_slots)
            .saturating_add(large_pool)
    }
}

impl Default for AllocatorConfig {
    /// Production defaults for 64-bit hosts: ~0.5 GiB of span region and
    /// 2 GiB of medium/large region (virtual; lazily faulted on POSIX).
    /// 32-bit targets get a much smaller address-space footprint.
    fn default() -> Self {
        #[cfg(target_pointer_width = "64")]
        {
            Self {
                span_region_bytes: 512 * 1024 * 1024,
                span_extent_slots: 16 * 1024,
                span_slots: 16 * 1024,
                large_region_bytes: 2 * 1024 * 1024 * 1024,
                large_extent_slots: 32 * 1024,
                large_slots: 32 * 1024,
            }
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            Self {
                span_region_bytes: 64 * 1024 * 1024,
                span_extent_slots: 4 * 1024,
                span_slots: 4 * 1024,
                large_region_bytes: 128 * 1024 * 1024,
                large_extent_slots: 4 * 1024,
                large_slots: 4 * 1024,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free outcome
// ---------------------------------------------------------------------------

/// The outcome of [`Allocator::free`]. The allocator reports — it does not
/// decide policy: the C ABI maps every non-[`Freed`](FreeOutcome::Freed)
/// outcome to a safe no-op (§35.2 "fail safely"), the Rust `GlobalAlloc`
/// adapter routes [`Invalid(Foreign)`](FreeOutcome::Invalid) pointers back to
/// the system allocator they came from, and the hardened profile (plan 08
/// W18) will escalate. Nothing is ever freed, recycled, or written through on
/// a non-`Freed` outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeOutcome {
    /// The pointer was a live allocation of this allocator and is now free.
    Freed,
    /// `free(NULL)` — a no-op (§9.6).
    Null,
    /// The pointer failed validation (foreign, interior, metadata, released,
    /// or quarantined — §17.5); no state was touched.
    Invalid(InvalidFree),
    /// The pointer classified as one of ours but its object was already free
    /// (a double free, or a stale free racing the span's teardown); no state
    /// was corrupted. Debug builds abort inside the central list instead.
    DoubleFree,
}

// ---------------------------------------------------------------------------
// Span pool — recycling span descriptors (§27.5)
// ---------------------------------------------------------------------------

/// One slot of the span pool. The descriptor MUST stay the first field of a
/// `#[repr(C)]` slot so a `*const SpanDescriptor` from the pagemap converts
/// back to its slot by address arithmetic (the same layout contract
/// `LargeSlot` relies on in `large.rs`).
#[repr(C)]
struct SpanSlot {
    /// The descriptor (lives in never-freed metadata; recycled in place).
    desc: SpanDescriptor,
    /// The span's base pointer, kept as a *pointer* (not an address) so object
    /// pointers handed to the application derive their provenance from the
    /// provider's region mapping via `add`, never from a bare integer.
    base: *mut u8,
    /// Backing extent (id + generation) to return on retirement.
    extent_id: u32,
    extent_gen: u32,
    /// Unused-slot stack link (valid while the slot is on the free stack).
    free_next: u32,
}

const _: () = assert!(
    core::mem::offset_of!(SpanSlot, desc) == 0,
    "SpanSlot::desc must be the first field: the pagemap's span pointer is \
     converted back to its slot by address identity"
);

/// Sentinel for "no slot" in the pool links.
const NIL: u32 = u32::MAX;

/// The mutable head-state of the pool, guarded by the allocator's span lock.
struct SpanPoolInner {
    /// Free-slot stack head (previously used slots, ready to recycle).
    free_head: u32,
    /// Next never-used slot (initialized with `SpanDescriptor::new`).
    high_water: u32,
    /// Live (activated, not yet retired) span count, for stats/tests.
    live: u32,
}

/// A fixed-capacity, metadata-backed pool of recycling [`SpanDescriptor`]s,
/// mirroring `LargePool` (large.rs): slots are never freed; reuse recycles the
/// descriptor in place behind a generation bump (§16.6/§27.5).
struct SpanPool {
    /// Slot storage in never-freed metadata. `slots`/`cap` are immutable after
    /// construction, so address↔slot conversion needs no lock.
    slots: NonNull<SpanSlot>,
    cap: u32,
    inner: UnsafeCell<SpanPoolInner>,
}

impl SpanPool {
    /// Allocate and zero a pool of `cap` slots from `meta`. Zeroed slots are
    /// valid: every field is an integer/atomic whose all-zero pattern is legal,
    /// and a slot's descriptor is fully written before first use.
    fn new(meta: &dyn MetadataAlloc, cap: usize) -> Option<SpanPool> {
        if cap == 0 || cap >= NIL as usize {
            return None;
        }
        let bytes = cap.checked_mul(core::mem::size_of::<SpanSlot>())?;
        let mem = meta.alloc(bytes, core::mem::align_of::<SpanSlot>())?;
        // SAFETY: `mem` is a fresh, exclusively-owned, aligned region of
        // exactly `bytes` bytes; zeroing it yields `cap` valid `SpanSlot`s.
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        Some(SpanPool {
            slots: mem.cast::<SpanSlot>(),
            cap: cap as u32,
            inner: UnsafeCell::new(SpanPoolInner {
                free_head: NIL,
                high_water: 0,
                live: 0,
            }),
        })
    }

    /// A raw pointer to slot `i` (`i < cap`). Pool memory is never freed
    /// (monotonic metadata), so the pointer is valid for the process lifetime.
    #[inline]
    fn slot_ptr(&self, i: u32) -> *mut SpanSlot {
        debug_assert!(i < self.cap);
        // SAFETY: `i < cap`, so the offset stays inside the pool allocation.
        unsafe { self.slots.as_ptr().add(i as usize) }
    }

    /// Map a `*const SpanDescriptor` (from the pagemap / central list) back to
    /// its slot index, or `None` if it does not point at a slot of this pool.
    fn index_of(&self, desc: *const SpanDescriptor) -> Option<u32> {
        let base = self.slots.as_ptr() as usize;
        let p = desc as usize;
        if p < base {
            return None;
        }
        let stride = core::mem::size_of::<SpanSlot>();
        let off = p - base;
        if !off.is_multiple_of(stride) {
            return None;
        }
        let idx = off / stride;
        if idx < self.cap as usize {
            Some(idx as u32)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The allocator
// ---------------------------------------------------------------------------

/// Reserve and commit a provider region and build the allocator's metadata
/// arena over it. The arena is the [`MetadataAlloc`] + [`MetadataRegion`]
/// source an [`Allocator`] borrows.
///
/// **Lifetime contract:** the returned arena aliases provider-owned memory.
/// The caller must keep `provider` alive (and never `release` the region) for
/// as long as the arena — and any allocator built over it — exists. Metadata
/// is monotonic (§27.5): in production both are process-lived; tests keep the
/// provider in scope or leak it.
pub fn reserve_meta_arena<P: TopoBackingProvider>(
    provider: &P,
    arena: ArenaId,
    bytes: usize,
) -> Result<BumpArena, BackendError> {
    let region = provider.reserve(arena, bytes, PAGE_SIZE)?;
    provider.commit(region, 0, region.len)?;
    // SAFETY: `region` is a committed, exclusively-owned mapping of
    // `region.len` bytes that the caller keeps alive for the arena's lifetime
    // (the contract above); the arena never outgrows it (BumpArena bounds
    // every carve).
    Ok(unsafe { BumpArena::new(region.base, region.len) })
}

/// The M1 allocator over the central path (W8-1a): classify → central list /
/// large path, under the structures' own locks. See the module docs for the
/// composition and the concurrency argument.
///
/// `'a` is the lifetime of the borrowed pagemap and metadata source — both
/// process-lived in production (the ABI leaks them at first use, §35.5) and
/// scoped in tests.
pub struct Allocator<'a, P: TopoBackingProvider> {
    /// Metadata source for span descriptors, bitmaps, and pagemap nodes.
    meta: &'a dyn MetadataAlloc,
    /// The metadata address range(s), for §17.5 metadata-pointer rejection.
    meta_region: &'a (dyn MetadataRegion + Sync),
    /// The pagemap every owned page is published in (W3-6 single mutator).
    pagemap: &'a PageMap,
    /// The single (default) arena of M1. Plan 06 W9 adds real arena objects.
    arena: ArenaId,
    /// Central free lists: the small-object allocation layer (W5).
    central: CentralCache,
    /// Backing region for small-object spans.
    span_extents: ExtentManager<P>,
    /// Medium/large allocations (page-rounded extents + descriptors).
    large: LargeAllocator<'a, P>,
    /// Recycling span-descriptor pool.
    spans: SpanPool,
    /// Guards `spans.inner` (acquire/release; lowest lock class, §27.2 —
    /// never held across a central-list or provider call).
    span_lock: BackendLock,
}

// SAFETY: `central`, `span_extents`, `large`, and `pagemap` carry their own
// synchronization; `meta`/`meta_region` are `Sync` by bound; the span pool's
// mutable state (`spans.inner`) is only touched under `span_lock`, and its
// immutable `slots`/`cap` support lock-free address↔slot conversion. So
// concurrent `&self` use is data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for Allocator<'_, P> {}
// SAFETY: the allocator owns its pool and `Send` extent/large managers; the
// borrowed pagemap/meta references are `Sync`, so they may be used from
// whichever thread owns the allocator.
unsafe impl<P: TopoBackingProvider + Send> Send for Allocator<'_, P> {}

impl<'a, P: TopoBackingProvider> Allocator<'a, P> {
    /// Build an allocator from two provider instances (one region each for
    /// spans and medium/large), a metadata source, and the pagemap.
    ///
    /// `meta` and `meta_region` are usually the same object (a [`BumpArena`]
    /// from [`reserve_meta_arena`], or the [`Bootstrap`](crate::Bootstrap));
    /// they are separate parameters so a composite region
    /// ([`AnyMetadataRegion`](crate::AnyMetadataRegion)) can cover several
    /// metadata sources after a hand-off.
    pub fn new(
        span_provider: P,
        large_provider: P,
        meta: &'a dyn MetadataAlloc,
        meta_region: &'a (dyn MetadataRegion + Sync),
        pagemap: &'a PageMap,
        arena: ArenaId,
        cfg: AllocatorConfig,
    ) -> Result<Self, ExtentError> {
        let span_extents = ExtentManager::new(
            span_provider,
            meta,
            arena,
            cfg.span_region_bytes,
            PAGE_SIZE,
            cfg.span_extent_slots,
        )?;
        let large = LargeAllocator::new(
            large_provider,
            meta,
            pagemap,
            arena,
            LargeConfig {
                region_bytes: cfg.large_region_bytes,
                region_align: PAGE_SIZE,
                extent_slots: cfg.large_extent_slots,
                large_slots: cfg.large_slots,
            },
        )?;
        let spans = SpanPool::new(meta, cfg.span_slots).ok_or(ExtentError::Exhausted)?;
        Ok(Self {
            meta,
            meta_region,
            pagemap,
            arena,
            central: CentralCache::new(),
            span_extents,
            large,
            spans,
            span_lock: BackendLock::new(),
        })
    }

    /// The backend name of the providers (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        self.span_extents.backend_name()
    }

    /// The arena this allocator serves.
    pub fn arena(&self) -> ArenaId {
        self.arena
    }

    // -- allocation ----------------------------------------------------------

    /// Allocate `size` bytes with at least `MIN_ALIGN` alignment. Null on
    /// failure (§9.7: any overflow fails safely, never wraps).
    pub fn malloc(&self, size: usize) -> *mut u8 {
        self.allocate(size, MIN_ALIGN, RequestFlags::NONE)
    }

    /// Allocate `size` bytes aligned to `align` under the validated `flags`
    /// (§A.1/§10.4). Null on invalid alignment (not a power of two), on any
    /// size/alignment rounding overflow, or on out-of-memory.
    ///
    /// Zero-size requests follow the engine-internal `zero_unique` rule
    /// (§9.6): they behave as 1-byte requests, so the result is a unique,
    /// freeable pointer. The public ABI applies the configurable
    /// `zero_null` policy *before* calling in (plan 06 W8-4).
    ///
    /// The `TOPO_ZERO` hint zeroes the **entire usable size** of the result,
    /// so the calloc contract (§26.2) holds even for callers that write up to
    /// `malloc_usable_size`.
    pub fn allocate(&self, size: usize, align: usize, flags: RequestFlags) -> *mut u8 {
        let Some(req) = classify(size, align, flags.raw()) else {
            return ptr::null_mut();
        };
        // M1: a single default arena. An explicit-arena request for anything
        // else fails deterministically (§10.4) until W9 lands real arenas.
        if req.arena != self.arena {
            return ptr::null_mut();
        }
        let (p, usable) = match req.kind {
            RequestKind::Small { sc, usable } => (self.alloc_small(sc), usable),
            RequestKind::Medium { .. } | RequestKind::Large { .. } => {
                // Round the *size* alone (not `max(size, align)` — the carve
                // honours the alignment without over-allocating the length).
                // `classify` already checked the larger `max(size, align)`
                // rounding, so this smaller one cannot overflow; stay total
                // anyway.
                let effective = if size == 0 { 1 } else { size };
                let Some(bytes) = align_up(effective, PAGE_SIZE) else {
                    return ptr::null_mut();
                };
                (self.large.allocate(bytes, req.align), bytes)
            }
        };
        if !p.is_null() && req.flags.hints().zero {
            // SAFETY: `p` is a live allocation with at least `usable`
            // writable bytes (the class's usable size, or the page-rounded
            // extent length).
            unsafe { ptr::write_bytes(p, 0, usable) };
        }
        p
    }

    /// Small-object allocation: pull one object from the central list,
    /// creating and activating a new span when none is available (§A.2's
    /// OOM-retry loop).
    ///
    /// Termination: each iteration either returns an object or creates a new
    /// span; span creation draws from a finite region/pool, so a thread can
    /// loop only while *other* threads consume the objects it activates —
    /// i.e. only while the system as a whole makes progress.
    fn alloc_small(&self, sc: SizeClassId) -> *mut u8 {
        loop {
            match self
                .central
                .remove_batch(NodeId::DEFAULT, self.arena, Label::PUBLIC, sc, 1)
            {
                RemoveResult::Ok(batch) => {
                    debug_assert_eq!(batch.len(), 1);
                    // SAFETY: the batch's span pointer was installed from a
                    // valid descriptor in never-freed metadata (§27.5).
                    let span = unsafe { &*batch.span() };
                    return self.object_ptr(span, batch.index(0));
                }
                RemoveResult::NeedSpan => {
                    if self.create_span(sc).is_err() {
                        return ptr::null_mut();
                    }
                }
            }
        }
    }

    /// Convert an object index in `span` into a provenance-correct pointer.
    ///
    /// Stability: the caller owns at least one live object in `span` (its
    /// `remove_batch` raised `live_count`), so the span can be neither
    /// deactivated (requires empty) nor recycled (requires deactivation) —
    /// its geometry and its slot's `base` pointer are stable for this read.
    fn object_ptr(&self, span: &SpanDescriptor, index: u16) -> *mut u8 {
        let Some(layout) =
            SlabLayout::compute(span.size_class(), span.base(), span.slab_header() as usize)
        else {
            // Unreachable for a span this allocator activated (the same
            // computation succeeded at creation); recover by returning the
            // object rather than leaking it.
            debug_assert!(false, "object_ptr: span geometry no longer computes");
            self.central.insert_batch(span, &[index], 1);
            return ptr::null_mut();
        };
        let Some(addr) = layout.object_addr(index as usize) else {
            debug_assert!(false, "object_ptr: index {index} out of range");
            self.central.insert_batch(span, &[index], 1);
            return ptr::null_mut();
        };
        let Some(idx) = self.spans.index_of(span as *const SpanDescriptor) else {
            debug_assert!(false, "object_ptr: span not from this allocator's pool");
            self.central.insert_batch(span, &[index], 1);
            return ptr::null_mut();
        };
        // SAFETY: `idx` is a live slot (its span holds our live object; see
        // the stability note above), so reading its immutable-while-live
        // `base` field is race-free.
        let base = unsafe { (*self.spans.slot_ptr(idx)).base };
        debug_assert_eq!(base as usize, span.base());
        // SAFETY: `addr` lies inside the span (`SlabLayout` is bounds-checked),
        // and `base` is the provider mapping's pointer for the span base, so
        // the offset stays inside one committed allocation.
        unsafe { base.add(addr - span.base()) }
    }

    /// Create, activate, and publish a new span for `sc` (W5-5). On any
    /// failure every partial step is rolled back: the extent is freed, the
    /// pool slot released, and `Err` returned so `malloc` fails with null.
    fn create_span(&self, sc: SizeClassId) -> Result<(), ExtentError> {
        let row = size_class::row(sc);
        let bytes = (row.slab_pages as usize)
            .checked_mul(PAGE_SIZE)
            .ok_or(ExtentError::Exhausted)?;
        let ext = self.span_extents.alloc(bytes, PAGE_SIZE, Fit::Best)?;
        let region = match self.span_extents.region_of(ext) {
            Some(r) => r,
            None => return Err(ExtentError::Stale), // unreachable: ext is fresh
        };
        let base_ptr = region.base;
        let base = base_ptr as usize;
        let Some(layout) = SlabLayout::compute(sc, base, 0) else {
            let _ = self.span_extents.free(ext);
            return Err(ExtentError::Exhausted);
        };
        let Ok(object_count) = u32::try_from(layout.object_count) else {
            let _ = self.span_extents.free(ext);
            return Err(ExtentError::Exhausted);
        };

        // Acquire a descriptor slot and initialize/recycle it in place.
        self.span_lock.acquire();
        // SAFETY: span lock held ⇒ exclusive access to the pool head-state.
        let inner = unsafe { &mut *self.spans.inner.get() };
        let (idx, fresh) = if inner.free_head != NIL {
            let i = inner.free_head;
            // SAFETY: `i` is a valid slot index pushed by a previous release.
            inner.free_head = unsafe { (*self.spans.slot_ptr(i)).free_next };
            (i, false)
        } else if inner.high_water < self.spans.cap {
            let i = inner.high_water;
            inner.high_water += 1;
            (i, true)
        } else {
            self.span_lock.release();
            let _ = self.span_extents.free(ext);
            return Err(ExtentError::Exhausted);
        };

        let slot = self.spans.slot_ptr(idx);
        let initialized = if fresh {
            match SpanDescriptor::new(
                SpanId(idx),
                self.arena,
                sc,
                base,
                row.slab_pages,
                object_count,
                0,
                self.meta,
            ) {
                Some(desc) => {
                    // SAFETY: lock held; `slot` is a never-used slot whose
                    // zeroed contents need no drop; write installs the
                    // fully-formed descriptor.
                    unsafe { ptr::write(&raw mut (*slot).desc, desc) };
                    true
                }
                None => {
                    // The never-initialized slot must NOT go on the free
                    // stack (a recycle would read garbage): un-advance the
                    // high-water mark instead — `idx` was `high_water - 1`.
                    debug_assert_eq!(idx + 1, inner.high_water);
                    inner.high_water = idx;
                    false
                }
            }
        } else {
            // SAFETY: lock held; a recycled slot holds a previously
            // initialized descriptor; `recycle` rewrites it under its own
            // span-lock + seqlock discipline.
            let ok = unsafe {
                (*slot).desc.recycle(
                    self.arena,
                    sc,
                    base,
                    row.slab_pages,
                    object_count,
                    0,
                    self.meta,
                )
            };
            if !ok {
                // The slot keeps its old (valid) geometry; back on the stack.
                // SAFETY: lock held; the slot is unused after this.
                unsafe { (*slot).free_next = inner.free_head };
                inner.free_head = idx;
            }
            ok
        };
        if initialized {
            // SAFETY: lock held; the slot is ours until activation publishes
            // the span. The base pointer is recorded before publication so
            // every later `object_ptr` read sees it (happens-before via the
            // central lock pair in activate/remove).
            unsafe {
                (*slot).base = base_ptr;
                (*slot).extent_id = ext.id.0;
                (*slot).extent_gen = ext.generation;
            }
            inner.live += 1;
        }
        self.span_lock.release();
        if !initialized {
            let _ = self.span_extents.free(ext);
            return Err(ExtentError::Exhausted);
        }

        // Publish: fill the bitmap, install in the pagemap, join the partial
        // list (W5-5). On failure, unwind everything.
        // SAFETY: the slot is live (acquired above) and its descriptor fully
        // initialized; descriptors live in never-freed metadata.
        let span = unsafe { &(*slot).desc };
        match self.central.activate_span(span, self.pagemap, self.meta) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.span_lock.acquire();
                // SAFETY: span lock held ⇒ exclusive pool head-state access;
                // the slot is ours (never published).
                unsafe {
                    let inner = &mut *self.spans.inner.get();
                    (*slot).free_next = inner.free_head;
                    inner.free_head = idx;
                    inner.live -= 1;
                }
                self.span_lock.release();
                let _ = self.span_extents.free(ext);
                Err(ExtentError::Exhausted)
            }
        }
    }

    // -- deallocation ---------------------------------------------------------

    /// Free `ptr`. Validates ownership through the pagemap first (§17.5):
    /// every pointer that is null, foreign, interior, metadata, released, or
    /// already free is rejected with **no state change** (§35.2); see
    /// [`FreeOutcome`] for the policy split between callers.
    ///
    /// # Safety
    ///
    /// `ptr` must be **either** (a) a pointer this allocator returned that the
    /// caller still owns (not yet freed), **or** (b) any pointer that was
    /// *never* returned by this allocator (null included) — those are
    /// detected and rejected harmlessly. What the classifier cannot detect —
    /// and what this contract excludes — is a *stale* pointer that aliases a
    /// recycled, currently-live allocation (the classic double-free): freeing
    /// it would release another owner's object and invalidate that owner's
    /// outstanding pointer.
    pub unsafe fn free(&self, ptr: *mut u8) -> FreeOutcome {
        if ptr.is_null() {
            return FreeOutcome::Null;
        }
        match validate_free(self.pagemap, self.meta_region, ptr as usize) {
            Ok(FreeTarget::Noop) => FreeOutcome::Null,
            Ok(FreeTarget::Small { span, object_index }) => {
                // SAFETY: the pagemap only holds descriptors in never-freed
                // metadata (§27.5).
                let span = unsafe { &*span };
                let Ok(idx) = u16::try_from(object_index) else {
                    // objects_per_slab ≤ u16::MAX is a compile-time table
                    // invariant (central.rs); stay total regardless.
                    return FreeOutcome::Invalid(InvalidFree::Foreign);
                };
                let r = self.central.insert_batch(span, &[idx], 1);
                if r.inserted == 0 {
                    // Already central-free (double free) or the span raced
                    // its teardown; nothing was mutated (W8 hardening).
                    return FreeOutcome::DoubleFree;
                }
                if r.span_empty {
                    self.retire_span(span);
                }
                FreeOutcome::Freed
            }
            Ok(FreeTarget::Large { .. }) => {
                // The large allocator re-resolves under its own lock, so a
                // concurrent double free of the same pointer settles on
                // exactly one winner (large.rs DD-1).
                if self.large.free(ptr) {
                    FreeOutcome::Freed
                } else {
                    FreeOutcome::DoubleFree
                }
            }
            Err(e) => FreeOutcome::Invalid(e),
        }
    }

    /// Retire an empty span (W5-5): deactivate it, clear its pagemap entries,
    /// return the backing extent, and recycle the descriptor slot.
    ///
    /// Exclusivity: only the thread whose insert emptied the span is told to
    /// deactivate (`span_empty` is reported once — central.rs), the span is
    /// already unlinked from every central list, and an empty span has no
    /// live objects, so no other thread can reach it.
    ///
    /// Ordering is load-bearing: the pagemap entries MUST be cleared
    /// (`retire_span`) *before* the extent is freed, so a re-allocation of
    /// the same address range can never collide with stale entries
    /// (P-Map-001: a page maps to at most one descriptor).
    fn retire_span(&self, span: &SpanDescriptor) {
        self.central.deactivate_span(span, self.pagemap);
        self.pagemap.retire_span(span);

        let Some(idx) = self.spans.index_of(span as *const SpanDescriptor) else {
            debug_assert!(false, "retire_span: span not from this allocator's pool");
            return;
        };
        let slot = self.spans.slot_ptr(idx);
        // Capture the backing and release the slot under the lock; the
        // provider-facing free happens outside it (§27.2: the backend call is
        // the slow step — same shape as large.rs `free_with`).
        self.span_lock.acquire();
        // SAFETY: span lock held ⇒ exclusive pool head-state and slot access
        // (the span is retired: no live objects, unlinked, pagemap cleared).
        let ext = unsafe {
            let ext = ExtentRef {
                id: ExtentId((*slot).extent_id),
                generation: (*slot).extent_gen,
            };
            let inner = &mut *self.spans.inner.get();
            (*slot).free_next = inner.free_head;
            inner.free_head = idx;
            inner.live -= 1;
            ext
        };
        self.span_lock.release();
        // A failed extent free leaves the manager well-formed (W4-5).
        let _ = self.span_extents.free(ext);
    }

    // -- introspection --------------------------------------------------------

    /// The usable size of the allocation at `ptr` (§10.1
    /// `malloc_usable_size`): the class's object size for small allocations,
    /// the page-rounded extent length for medium/large. `None` for null,
    /// foreign, interior, metadata, or released pointers.
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        if ptr.is_null() {
            return None;
        }
        match validate_free(self.pagemap, self.meta_region, ptr as usize) {
            Ok(FreeTarget::Small { span, .. }) => {
                // SAFETY: descriptors live in never-freed metadata.
                let span = unsafe { &*span };
                Some(size_class::usable_size(span.size_class()))
            }
            Ok(FreeTarget::Large { .. }) => self.large.usable_size(ptr),
            _ => None,
        }
    }

    /// Whether `ptr` is a live allocation of this allocator (a base pointer
    /// of a small object or a medium/large allocation). Interior, metadata,
    /// released, foreign, and null pointers are not owned.
    pub fn owns(&self, ptr: *mut u8) -> bool {
        !ptr.is_null() && validate_free(self.pagemap, self.meta_region, ptr as usize).is_ok()
    }

    // -- reallocation (§25, the W8-1a/W15-1/2/3a contract) ---------------------

    /// Reallocate `ptr` to `new_size` bytes (§25.1) with at least `min_align`
    /// alignment on the result. Null on failure — **and on failure the
    /// original allocation remains valid and untouched** (§25.4:
    /// allocate-before-free).
    ///
    /// Alignment policy (§25.4 "respect alignment"): the result satisfies the
    /// alignment *this* call states (`min_align`), exactly like C `realloc`
    /// (fundamental alignment) and jemalloc's `rallocx` (`MALLOCX_ALIGN`
    /// from flags). The original allocation's larger alignment is **not**
    /// silently inherited — callers that need it pass it (`topo_rallocx` +
    /// `TOPO_ALIGN_LG`), keeping the storage family a pure function of the
    /// stated request and the sized-free hints coherent.
    ///
    /// Path selection (plan 06 DD-1 state machine):
    /// * `ptr == NULL` → plain allocation.
    /// * same small class → in-place, return `ptr` (W15-3a; commits nothing,
    ///   so it cannot fail).
    /// * medium/large whose rounded `new_size` fits the current extent and
    ///   whose base satisfies the alignment → in-place, return `ptr`.
    /// * otherwise → move: allocate, copy `min(old_usable, new_size)`, free
    ///   the original last (W15-2).
    ///
    /// `new_size == 0` is treated as 1 (the engine-internal `zero_unique`
    /// rule); the ABI applies the platform `realloc(p, 0)` policy before
    /// calling in. With the `TOPO_ZERO` hint, bytes beyond the preserved
    /// prefix are zero in the result (zero-then-copy on the move path; an
    /// in-place result exposes no bytes beyond the old usable size).
    ///
    /// # Safety
    ///
    /// Same contract as [`free`](Self::free): `ptr` must be null, a live
    /// pointer of this allocator that the caller owns, or never-owned garbage
    /// (rejected). A stale pointer aliasing a recycled live allocation would
    /// read and free another owner's object.
    pub unsafe fn realloc(
        &self,
        ptr: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        if ptr.is_null() {
            return self.allocate(new_size, min_align, flags);
        }
        if !min_align.is_power_of_two() {
            return ptr::null_mut();
        }

        // Classify the existing allocation; reject anything we do not own
        // without touching it (§35.2).
        let old_usable = match classify_ptr(self.pagemap, self.meta_region, ptr as usize) {
            PointerClass::Small { sc, .. } => {
                let row = match size_class::checked_row(sc) {
                    Some(r) => r,
                    None => return ptr::null_mut(),
                };
                // In-place fast path (W15-3a): the new request lands in the
                // same class and the (≤ MAX_ALIGN) alignment already holds.
                // Returning `ptr` commits nothing, so this path cannot fail.
                if let Some(req) = classify(new_size, min_align, flags.raw()) {
                    if let RequestKind::Small { sc: new_sc, .. } = req.kind {
                        if new_sc == sc && (ptr as usize).is_multiple_of(min_align) {
                            return ptr;
                        }
                    }
                }
                row.size as usize
            }
            PointerClass::Large { .. } => {
                // The pointer is live (the caller's contract for realloc; a
                // racing free of the same pointer is C UB and is caught by
                // the lock-validated read here).
                let Some(usable) = self.large.usable_size(ptr) else {
                    return ptr::null_mut();
                };
                // In-place fast path: the new request is still medium/large
                // (a small-class request moves, so the extent is returned
                // rather than pinned under a tiny object), its page-rounded
                // size still fits this extent, and the base satisfies the
                // requested alignment. Commits nothing; cannot fail.
                if let Some(req) = classify(new_size, min_align, flags.raw()) {
                    if !matches!(req.kind, RequestKind::Small { .. }) {
                        let effective = if new_size == 0 { 1 } else { new_size };
                        if let Some(rounded) = align_up(effective, PAGE_SIZE) {
                            if rounded <= usable && (ptr as usize).is_multiple_of(min_align) {
                                return ptr;
                            }
                        }
                    }
                }
                usable
            }
            _ => return ptr::null_mut(),
        };

        // Move path (W15-2): allocate first — the original is untouched until
        // the new object exists — copy min(old_usable, new_size), free last.
        let q = self.allocate(new_size, min_align.max(MIN_ALIGN), flags);
        if q.is_null() {
            return ptr::null_mut(); // §25.1: the original remains valid
        }
        let effective_new = if new_size == 0 { 1 } else { new_size };
        let copy = old_usable.min(effective_new);
        // SAFETY: `q` is a fresh live allocation of at least
        // `max(new_size, 1) ≥ copy` usable bytes; `ptr` is live with
        // `old_usable ≥ copy` readable bytes; live objects are disjoint
        // (§8.3), so the ranges cannot overlap.
        unsafe { ptr::copy_nonoverlapping(ptr.cast_const(), q, copy) };
        // SAFETY: `ptr` is a live pointer of this allocator owned by the
        // caller (this function's own contract), released here exactly once.
        let freed = unsafe { self.free(ptr) };
        debug_assert_eq!(freed, FreeOutcome::Freed, "realloc freed a non-live ptr");
        q
    }

    // -- stats / invariants (tests, plan 07 wiring) ----------------------------

    /// Number of currently live medium/large allocations.
    pub fn live_large_count(&self) -> usize {
        self.large.live_count()
    }

    /// Number of currently active (not retired) spans.
    pub fn live_span_count(&self) -> usize {
        self.span_lock.acquire();
        // SAFETY: span lock held ⇒ exclusive pool head-state access.
        let n = unsafe { (*self.spans.inner.get()).live } as usize;
        self.span_lock.release();
        n
    }

    /// Committed bytes in the span region (small-object backing).
    pub fn span_committed_bytes(&self) -> usize {
        self.span_extents.committed_bytes()
    }

    /// Whether both back-ends are well-formed (W4-5 oracle; debug/tests).
    pub fn check_invariants(&self) -> bool {
        self.span_extents.check_invariants() && self.large.check_invariants()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::tables::{HUGE_THRESHOLD, SMALL_MAX};
    use crate::size_class::{objects_per_slab, size_class, usable_size};
    use host_provider::HostProvider;

    /// The POSIX provider is a separate crate (it depends on this one), so the
    /// in-crate tests use a minimal host-allocation provider with the same
    /// observable behaviour: committed RW memory, page-aligned reservations.
    /// The real-provider (mmap) integration runs in `tests/` (plan 08).
    mod host_provider {
        use super::*;
        use crate::backend::Region;
        use std::alloc::{alloc, dealloc, Layout};
        use std::sync::Mutex;

        pub(super) struct HostProvider {
            owned: Mutex<Vec<(usize, Layout)>>,
        }

        impl HostProvider {
            pub(super) fn new() -> Self {
                Self {
                    owned: Mutex::new(Vec::new()),
                }
            }
        }

        impl TopoBackingProvider for HostProvider {
            fn reserve(
                &self,
                _arena: ArenaId,
                size: usize,
                align: usize,
            ) -> Result<Region, BackendError> {
                if size == 0 || !align.is_power_of_two() {
                    return Err(BackendError::InvalidRequest);
                }
                let layout = Layout::from_size_align(size, align.max(PAGE_SIZE))
                    .map_err(|_| BackendError::InvalidRequest)?;
                // SAFETY: layout has nonzero size.
                let p = unsafe { alloc(layout) };
                let Some(base) = (!p.is_null()).then_some(p) else {
                    return Err(BackendError::OutOfMemory);
                };
                self.owned.lock().unwrap().push((base as usize, layout));
                Ok(Region { base, len: size })
            }

            fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
                Ok(())
            }
            fn decommit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
                Ok(())
            }
            fn purge_lazy(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
                Ok(())
            }
            fn purge_forced(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
                Ok(())
            }
            fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
                let mut owned = self.owned.lock().unwrap();
                if let Some(i) = owned.iter().position(|(b, _)| *b == region.base as usize) {
                    let (base, layout) = owned.swap_remove(i);
                    // SAFETY: (base, layout) came from `alloc` above.
                    unsafe { dealloc(base as *mut u8, layout) };
                }
                Ok(())
            }
            fn name(&self) -> &'static str {
                "posix"
            }
        }
    }

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box,
        // leaked for the test's (process) lifetime.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn small_allocator<'a>(m: &'a BumpArena, pm: &'a PageMap) -> Allocator<'a, HostProvider> {
        Allocator::new(
            HostProvider::new(),
            HostProvider::new(),
            m,
            m,
            pm,
            ArenaId::DEFAULT,
            AllocatorConfig::small(),
        )
        .expect("allocator")
    }

    /// Test wrapper for [`Allocator::free`]: every pointer the tests free is
    /// either one they just received from `a` and still own, null, or memory
    /// that was *never* returned by `a` (the rejected-input cases) — exactly
    /// the safety contract.
    fn tfree<P: TopoBackingProvider>(a: &Allocator<'_, P>, p: *mut u8) -> FreeOutcome {
        // SAFETY: the caller-ownership contract above.
        unsafe { a.free(p) }
    }

    /// Test wrapper for [`Allocator::realloc`] under the same contract.
    fn trealloc<P: TopoBackingProvider>(
        a: &Allocator<'_, P>,
        p: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        // SAFETY: the caller-ownership contract of `tfree`.
        unsafe { a.realloc(p, new_size, min_align, flags) }
    }

    #[test]
    fn malloc_returns_aligned_writable_distinct_objects() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let mut seen = std::collections::BTreeSet::new();
        for size in [0usize, 1, 8, 16, 24, 100, 1024, 4096, SMALL_MAX] {
            let p = a.malloc(size);
            assert!(!p.is_null(), "malloc({size})");
            assert_eq!(p as usize % MIN_ALIGN, 0);
            assert!(seen.insert(p as usize), "duplicate live pointer");
            // SAFETY: at least max(size,1) writable bytes.
            unsafe { ptr::write_bytes(p, 0xa5, size.max(1)) };
        }
        for p in &seen {
            assert_eq!(tfree(&a, *p as *mut u8), FreeOutcome::Freed);
        }
        assert!(a.check_invariants());
    }

    #[test]
    fn small_free_recycles_the_same_object() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(64);
        assert!(!p.is_null());
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        // The single-object hole is the next thing the central list vends.
        let q = a.malloc(64);
        assert_eq!(p, q, "freed object must be reused (no leak)");
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
    }

    #[test]
    fn medium_and_large_roundtrip_and_recycle_address_space() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // Medium: above SMALL_MAX, below HUGE_THRESHOLD.
        let p = a.malloc(SMALL_MAX + 1);
        assert!(!p.is_null());
        assert_eq!(p as usize % PAGE_SIZE, 0);
        assert_eq!(
            a.usable_size(p),
            Some(align_up(SMALL_MAX + 1, PAGE_SIZE).unwrap())
        );
        // SAFETY: usable bytes are writable.
        unsafe { ptr::write_bytes(p, 0x7e, SMALL_MAX + 1) };
        assert_eq!(a.live_large_count(), 1);
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert_eq!(a.live_large_count(), 0);

        // Large: at/above HUGE_THRESHOLD.
        let q = a.malloc(HUGE_THRESHOLD);
        assert!(!q.is_null());
        assert_eq!(a.usable_size(q), Some(HUGE_THRESHOLD));
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn over_aligned_request_is_honored_not_ignored() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // Beyond MAX_ALIGN: routed to the extent path, never a shared slab.
        for align in [4096usize, 65536, 262144] {
            let p = a.allocate(100, align, RequestFlags::NONE);
            assert!(!p.is_null(), "allocate(100, {align})");
            assert_eq!(p as usize % align, 0, "alignment {align} not honored");
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        // Invalid alignment fails deterministically.
        assert!(a.allocate(8, 24, RequestFlags::NONE).is_null());
    }

    #[test]
    fn zero_flag_zeroes_the_whole_usable_size() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // Dirty an object first so recycled memory is genuinely non-zero.
        let p = a.malloc(256);
        // SAFETY: 256 usable bytes.
        unsafe { ptr::write_bytes(p, 0xff, 256) };
        tfree(&a, p);

        // Same class ⇒ the central list vends the dirty object back.
        let q = a.allocate(256, MIN_ALIGN, RequestFlags::NONE.with_zero());
        assert_eq!(q, p, "expect the dirty object back");
        let usable = a.usable_size(q).unwrap();
        // SAFETY: `usable` readable bytes.
        unsafe {
            for i in 0..usable {
                assert_eq!(q.add(i).read(), 0, "byte {i} not zeroed");
            }
        }
        tfree(&a, q);
    }

    #[test]
    fn invalid_frees_are_rejected_without_state_change() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        assert_eq!(tfree(&a, ptr::null_mut()), FreeOutcome::Null);

        // Foreign pointer.
        let foreign = Box::into_raw(Box::new(7u64)).cast::<u8>();
        assert!(matches!(
            tfree(&a, foreign),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        // SAFETY: reclaiming the foreign test Box (untouched by the rejected free).
        drop(unsafe { Box::from_raw(foreign.cast::<u64>()) });

        // Interior pointer of a live small object.
        let p = a.malloc(64);
        // SAFETY: 64 usable bytes; p+8 stays in the object.
        let interior = unsafe { p.add(8) };
        assert!(matches!(
            tfree(&a, interior),
            FreeOutcome::Invalid(InvalidFree::Interior { .. })
        ));
        assert_eq!(
            tfree(&a, p),
            FreeOutcome::Freed,
            "object still live after rejects"
        );

        // Interior pointer of a live large allocation.
        let q = a.malloc(SMALL_MAX + 1);
        // SAFETY: in-bounds of the live allocation.
        let q_in = unsafe { q.add(PAGE_SIZE) };
        assert!(matches!(
            tfree(&a, q_in),
            FreeOutcome::Invalid(InvalidFree::Interior { .. })
        ));
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn double_free_is_reported_not_corrupting_in_release() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(64);
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert_eq!(tfree(&a, p), FreeOutcome::DoubleFree);
        // The object is still allocatable exactly once.
        let q = a.malloc(64);
        assert_eq!(p, q);
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
    }

    #[test]
    fn empty_spans_are_retired_and_address_space_reused() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let sc = size_class(64, 1).unwrap();
        let per_span = objects_per_slab(sc) as usize;

        // Fill three spans' worth, then free everything: one span stays in
        // the central empty cache, the rest are retired through the extent
        // manager (their pagemap entries cleared).
        let mut ptrs = Vec::new();
        for _ in 0..(3 * per_span) {
            let p = a.malloc(64);
            assert!(!p.is_null());
            ptrs.push(p);
        }
        assert_eq!(a.live_span_count(), 3);
        for p in ptrs.drain(..) {
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        // 3 spans emptied; 1 cached per bin; 2 retired.
        assert_eq!(a.live_span_count(), 1);
        assert!(a.check_invariants());

        // Allocating again reuses the cached span and then fresh/recycled ones.
        let p = a.malloc(64);
        assert!(!p.is_null());
        tfree(&a, p);
    }

    #[test]
    fn realloc_contract_grow_shrink_preserve_fail() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // realloc(NULL, n) == malloc(n).
        let p = trealloc(&a, ptr::null_mut(), 100, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());

        // Fill with a pattern; grow within the same class is in-place.
        // SAFETY: 100 usable bytes.
        unsafe {
            for i in 0..100 {
                p.add(i).write(i as u8);
            }
        }
        let same = trealloc(&a, p, 112, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(same, p, "same-class growth must be in-place (W15-3a)");

        // Cross-class growth moves and preserves the prefix.
        let q = trealloc(&a, p, 4096, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q.is_null());
        assert_ne!(q, p);
        // SAFETY: 4096 usable bytes at q; the first 100 carry the pattern.
        unsafe {
            for i in 0..100 {
                assert_eq!(q.add(i).read(), i as u8, "content lost at {i}");
            }
        }

        // Growth into the medium path preserves content again.
        let r = trealloc(&a, q, SMALL_MAX + 100, MIN_ALIGN, RequestFlags::NONE);
        assert!(!r.is_null());
        // SAFETY: prefix readable at r.
        unsafe {
            for i in 0..100 {
                assert_eq!(r.add(i).read(), i as u8);
            }
        }

        // Medium shrink within the same extent stays in place.
        let s = trealloc(&a, r, SMALL_MAX + 50, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(s, r, "page-rounded shrink stays in place");

        // Shrink back to small moves (frees the extent) and preserves content.
        let t = trealloc(&a, s, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!t.is_null());
        assert_ne!(t, s);
        assert_eq!(
            a.live_large_count(),
            0,
            "extent returned on shrink-to-small"
        );
        // SAFETY: 64 usable bytes carry the prefix.
        unsafe {
            for i in 0..64 {
                assert_eq!(t.add(i).read(), i as u8);
            }
        }

        // Failure preserves the original: a request beyond the region fails…
        let huge = trealloc(&a, t, usize::MAX - 4096, MIN_ALIGN, RequestFlags::NONE);
        assert!(huge.is_null());
        // …and the original is still live with its content.
        // SAFETY: t is still live.
        unsafe {
            assert_eq!(t.read(), 0);
            assert_eq!(t.add(63).read(), 63);
        }
        assert_eq!(tfree(&a, t), FreeOutcome::Freed);

        // Invalid old pointers fail without side effects.
        let foreign = Box::into_raw(Box::new(0u64)).cast::<u8>();
        assert!(trealloc(&a, foreign, 100, MIN_ALIGN, RequestFlags::NONE).is_null());
        // SAFETY: reclaiming the untouched foreign Box.
        drop(unsafe { Box::from_raw(foreign.cast::<u64>()) });
    }

    #[test]
    fn usable_size_matches_class_and_rounding() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(100);
        let sc = size_class(100, 1).unwrap();
        assert_eq!(a.usable_size(p), Some(usable_size(sc)));
        tfree(&a, p);

        let q = a.malloc(SMALL_MAX + 1);
        assert_eq!(
            a.usable_size(q),
            Some(align_up(SMALL_MAX + 1, PAGE_SIZE).unwrap())
        );
        tfree(&a, q);

        assert_eq!(a.usable_size(ptr::null_mut()), None);
    }

    #[test]
    fn classification_sees_engine_objects() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(64);
        match classify_ptr(&pm, &m, p as usize) {
            PointerClass::Small { object_index, .. } => {
                // The object index is consistent with the slab layout inverse.
                let _ = object_index;
            }
            other => panic!("expected Small, got {other:?}"),
        }
        tfree(&a, p);
    }

    #[test]
    fn oom_is_a_clean_null_and_state_stays_well_formed() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        // A deliberately tiny configuration: one span fits, little else.
        let cfg = AllocatorConfig {
            span_region_bytes: 4 * PAGE_SIZE,
            span_extent_slots: 8,
            span_slots: 8,
            large_region_bytes: 8 * PAGE_SIZE,
            large_extent_slots: 8,
            large_slots: 8,
        };
        let a = Allocator::new(
            HostProvider::new(),
            HostProvider::new(),
            &m,
            &m,
            &pm,
            ArenaId::DEFAULT,
            cfg,
        )
        .expect("allocator");

        // Exhaust the span region: each 16 KiB class-?? span takes a page.
        let mut ptrs = Vec::new();
        loop {
            let p = a.malloc(8192); // class with few objects per span
            if p.is_null() {
                break;
            }
            ptrs.push(p);
        }
        assert!(!ptrs.is_empty());
        assert!(a.check_invariants(), "OOM left the backend well-formed");

        // Everything frees cleanly and is allocatable again.
        for p in ptrs.drain(..) {
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        let p = a.malloc(8192);
        assert!(!p.is_null(), "memory is reusable after OOM recovery");
        tfree(&a, p);

        // Large-region exhaustion is also a clean null.
        let too_big = a.malloc(16 * PAGE_SIZE);
        assert!(too_big.is_null());
        assert!(a.check_invariants());
    }

    #[test]
    fn concurrent_malloc_free_conserves_objects() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let a = &a;

        std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|t| {
                    s.spawn(move || {
                        let mut live = Vec::new();
                        for i in 0..400usize {
                            let size = 16 + ((i * 37 + t * 101) % 2048);
                            let p = a.malloc(size);
                            assert!(!p.is_null());
                            // SAFETY: `size` usable bytes.
                            unsafe { ptr::write_bytes(p, t as u8, size) };
                            live.push(p);
                            if i % 3 == 0 {
                                let q = live.swap_remove((i * 7) % live.len());
                                assert_eq!(tfree(a, q), FreeOutcome::Freed);
                            }
                        }
                        for p in live {
                            assert_eq!(tfree(a, p), FreeOutcome::Freed);
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
        });
        assert!(a.check_invariants());
        assert_eq!(a.live_large_count(), 0);
    }
}

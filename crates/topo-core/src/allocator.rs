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
//! **Trace spine (§33.7 / principle 6).** The allocator does not emit trace
//! lines itself; it exposes everything an emitter needs — addresses,
//! [`usable_size`](Allocator::usable_size), [`classify`] for the request
//! side, [`stats`](Allocator::stats) for conservation — and the differential
//! tests compose those with [`crate::trace`]'s grammar writers and replay the
//! stream through the `LiveModel`/Lean oracles. In-engine emission hooks (a
//! sink threaded through the hot paths) arrive with the deterministic-replay
//! work that owns them (plan 08 W21), so the hot path carries no tracing
//! cost until a consumer exists.
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
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arena::{
    ArenaConfig, ArenaError, ArenaPolicy, ArenaStats, ArenaTable, Delegation, HookFailureStats,
};
use crate::backend::TopoBackingProvider;
use crate::bootstrap::{BumpArena, MetadataAlloc};
use crate::central::{CentralCache, RemoveResult};
use crate::classify::{classify, Request, RequestKind};
use crate::error::BackendError;
use crate::extent::{
    ExtentBacking, ExtentError, ExtentId, ExtentManager, ExtentRef, Fit, RegionCacheHook,
    StateBytes,
};
use crate::flags::{Hints, Lifetime, RequestFlags};
use crate::generated::tables::PAGE_SIZE;
use crate::generated::tables::SIZE_CLASSES;
use crate::hooks::{ExtentHooks, HookProvider};
use crate::huge::Hotness;
use crate::ids::{ArenaId, Generation, Label, NodeId, SizeClassId, SpanId};
use crate::large::{LargeAllocator, LargeBacking, LargeConfig};
use crate::lock::{LockRank, RankedLock};
use crate::overflow::align_up;
use crate::pagemap::PageMap;
use crate::placement::{LearnedHints, PlaceClass, SizeClassDist};
use crate::ptr_class::{
    classify_ptr, validate_free, FreeTarget, InvalidFree, MetadataRegion, PointerClass,
};
use crate::size_class;
use crate::skeleton::MIN_ALIGN;
use crate::slab::SlabLayout;
use crate::span::{SpanDescriptor, SpanState};

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
    ///
    /// The extent and large slot types are private to their modules; their
    /// sizes are bounded by a named per-slot constant that a compile-time
    /// assertion pins to the real `size_of` values — a future field addition
    /// that crosses the bound fails the build instead of silently
    /// under-provisioning arenas sized from this.
    pub const fn fixed_pool_metadata_bytes(&self) -> usize {
        let span_pool = self
            .span_slots
            .saturating_mul(core::mem::size_of::<SpanSlot>());
        let extent_slots = self
            .span_extent_slots
            .saturating_add(self.large_extent_slots)
            .saturating_mul(FOREIGN_SLOT_BOUND);
        let large_pool = self.large_slots.saturating_mul(FOREIGN_SLOT_BOUND);
        span_pool
            .saturating_add(extent_slots)
            .saturating_add(large_pool)
    }
}

/// Conservative per-slot byte bound for the (module-private) extent and large
/// descriptor slots, used by [`AllocatorConfig::fixed_pool_metadata_bytes`].
/// Pinned to the real sizes below so it can never silently under-provision.
const FOREIGN_SLOT_BOUND: usize = 128;
const _: () = assert!(
    crate::extent::EXTENT_SLOT_BYTES <= FOREIGN_SLOT_BOUND
        && crate::large::LARGE_SLOT_BYTES <= FOREIGN_SLOT_BOUND,
    "a descriptor slot outgrew FOREIGN_SLOT_BOUND; raise the bound so      fixed_pool_metadata_bytes keeps over-approximating"
);

impl AllocatorConfig {
    /// Production defaults: ~0.5 GiB of span region and 2 GiB of medium/large
    /// region on 64-bit hosts (virtual; lazily faulted on POSIX); a much
    /// smaller address-space footprint on 32-bit targets. A `const` so
    /// consumers (the ABI's metadata-arena sizing) can pin headroom against
    /// it at compile time.
    #[cfg(target_pointer_width = "64")]
    pub const DEFAULT: AllocatorConfig = AllocatorConfig {
        span_region_bytes: 512 * 1024 * 1024,
        span_extent_slots: 16 * 1024,
        span_slots: 16 * 1024,
        large_region_bytes: 2 * 1024 * 1024 * 1024,
        large_extent_slots: 32 * 1024,
        large_slots: 32 * 1024,
    };
    /// See the 64-bit variant; 32-bit address space is the scarce resource.
    #[cfg(not(target_pointer_width = "64"))]
    pub const DEFAULT: AllocatorConfig = AllocatorConfig {
        span_region_bytes: 64 * 1024 * 1024,
        span_extent_slots: 4 * 1024,
        span_slots: 4 * 1024,
        large_region_bytes: 128 * 1024 * 1024,
        large_extent_slots: 4 * 1024,
        large_slots: 4 * 1024,
    };
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::DEFAULT
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
// Stats (§8.6/§31, the W8 reconciliation surface for plan 07)
// ---------------------------------------------------------------------------

/// An instantaneous statistics snapshot of an [`Allocator`] (§31.1 "where is
/// the memory?"). Produced by [`Allocator::stats`]; `topo-stats` maps it into
/// the Appendix-D JSON via `Stats::record_allocator`.
///
/// Counter identities (§8.6, asserted by the reconciliation tests):
/// `live_bytes == allocated_bytes_total - freed_bytes_total` holds by
/// construction; in a quiescent state every live small byte is the complement
/// of `central_free_bytes` within its spans' object bytes. Individual fields
/// are relaxed-atomic reads, so a snapshot taken *during* concurrent
/// operations is per-field accurate but not globally instantaneous
/// (epoch-consistent snapshots are the plan 07 W17 surface).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Usable bytes currently live in the application.
    pub live_bytes: u64,
    /// Cumulative usable bytes ever handed out.
    pub allocated_bytes_total: u64,
    /// Cumulative usable bytes ever returned.
    pub freed_bytes_total: u64,
    /// Free object bytes resident in the central lists (Σ over classes of
    /// `central_free_count × object_size`).
    pub central_free_bytes: u64,
    /// §20.1 physical-state breakdown of the span (small-object) region.
    pub span_backend: StateBytes,
    /// §20.1 physical-state breakdown of the medium/large region.
    pub large_backend: StateBytes,
    /// Bytes of pagemap radix nodes allocated so far (metadata overhead).
    pub pagemap_metadata_bytes: u64,
    /// Spans currently active (created and not yet retired).
    pub live_spans: u64,
    /// Medium/large allocations currently live.
    pub live_large: u64,
    /// Arenas currently registered, including the always-present default
    /// (§22, plan 06 W9).
    pub live_arenas: u64,
    /// Cumulative NUMA binding failures across all arenas (§15.5, W9-7).
    pub numa_bind_failures: u64,
    /// Cumulative custom-backing (extent-hook) failures across **all** hooked
    /// arenas, per kind (§23, plan 06 W10). All zero unless a hooked arena's backing
    /// has faulted (and zero entirely for a program with no hooked arenas).
    pub hook_failures: HookFailureStats,
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

/// A provider-backed metadata arena: the [`MetadataAlloc`] + [`MetadataRegion`]
/// source an [`Allocator`] borrows, **owning** the provider whose reservation
/// backs it.
///
/// Owning the provider is what makes this safe by construction: both shipped
/// providers reclaim their reservations on `Drop`, so an arena that merely
/// *borrowed* one could be left dangling by safe code. Here the borrow checker
/// enforces the §27.5 lifetime story instead — every `Allocator` borrows the
/// `MetaArena`, so the arena (and the provider inside it) must outlive the
/// allocator, and the backing cannot be reclaimed while any metadata is
/// reachable. In production the arena is process-lived (the ABI leaks it at
/// first use, §35.5); tests scope it around the allocator.
pub struct MetaArena<P: TopoBackingProvider> {
    /// The bump carver over the committed reservation.
    arena: BumpArena,
    /// Keeps the reservation mapped: the provider's `Drop` is the only
    /// reclaimer, and it cannot run before `self` (and every borrower) is gone.
    _provider: P,
}

impl<P: TopoBackingProvider> MetaArena<P> {
    /// Reserve and commit `bytes` of metadata backing from `provider`, taking
    /// ownership of it. Fails cleanly if the provider cannot reserve or
    /// commit (the provider's own `Drop` reclaims any partial reservation).
    pub fn reserve(provider: P, arena: ArenaId, bytes: usize) -> Result<Self, BackendError> {
        let region = provider.reserve(arena, bytes, PAGE_SIZE)?;
        provider.commit(region, 0, region.len)?;
        // SAFETY: `region` is a committed, exclusively-owned mapping of
        // `region.len` bytes. It stays valid for the arena's whole lifetime
        // because `self` owns `provider` — the only reclaimer of the mapping
        // is the provider's `Drop`, which cannot run before `self` is
        // dropped, and the borrow checker keeps every user of the arena
        // alive no longer than `self`. `BumpArena` bounds every carve to
        // `region.len`.
        let bump = unsafe { BumpArena::new(region.base, region.len) };
        Ok(Self {
            arena: bump,
            _provider: provider,
        })
    }

    /// Metadata bytes carved so far.
    pub fn used(&self) -> usize {
        self.arena.used()
    }

    /// Total metadata capacity.
    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }
}

impl<P: TopoBackingProvider + Sync> MetadataAlloc for MetaArena<P> {
    fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        self.arena.alloc(size, align)
    }
}

impl<P: TopoBackingProvider> MetadataRegion for MetaArena<P> {
    fn contains(&self, addr: usize) -> bool {
        self.arena.contains(addr)
    }
}

/// The page-rounded byte length a medium/large allocation actually carves
/// for a request of `size` bytes — the *size* alone, not classify's
/// `max(size, align)` span: the extent carve honours the alignment by
/// placement, without over-allocating the length. `None` only on rounding
/// overflow (§9.7). The single formula shared by [`Allocator::allocate`] and
/// [`predicted_usable_size`], so prediction and reality cannot drift (PR #11
/// review).
#[inline]
fn medium_large_usable(size: usize) -> Option<usize> {
    let effective = if size == 0 { 1 } else { size };
    align_up(effective, PAGE_SIZE)
}

/// The usable size a successful [`Allocator::allocate`]`(size, align, flags)`
/// would return, computed without allocating — the §10.3 `nallocx` oracle.
/// `None` exactly when `allocate` would fail classification (invalid
/// alignment/flags or rounding overflow). Pure: no allocator state is
/// consulted, so the prediction is identical for every engine instance.
pub fn predicted_usable_size(size: usize, align: usize, flags: RequestFlags) -> Option<usize> {
    let req = classify(size, align, flags.raw())?;
    match req.kind {
        RequestKind::Small { usable, .. } => Some(usable),
        RequestKind::Medium { .. } | RequestKind::Large { .. } => medium_large_usable(size),
    }
}

// ---------------------------------------------------------------------------
// Per-arena hooked backing regions (plan 06 W10, §22.2/§22.4)
// ---------------------------------------------------------------------------

/// Maximum number of arenas that can carry their **own** extent hooks at once
/// (§22.2 `ExtentHooks hooks`). Each consumes a backing region + descriptor pools
/// from the metadata arena, so the count is bounded (§27.5); the registry itself is
/// a fixed inline array in the (singleton) allocator, kept within a size budget by
/// the assertion below. This covers the custom-backing use cases at scale (per-
/// device/DMA, shared-memory, debug, and per-subsystem arenas). A `u8` slot index
/// (`+1`, `0` = none) bounds it at 255; the size budget below bounds it well first —
/// the registry is constructed **inline on the stack** (the allocator is built by
/// value before being boxed/leaked), so the budget is also a stack-safety ceiling.
pub const MAX_HOOK_BACKENDS: usize = 32;

/// The borrowed, type-erased hooks an arena's backing region is served from. The
/// caller owns the hooks (they outlive the allocator, exactly as the metadata and
/// pagemap do), so the core stays allocation-free (no `Box`).
type ArenaHooks<'a> = &'a (dyn ExtentHooks + Send + Sync);

/// A per-arena hooked backing (§22.2): a dedicated span-extent region **and** a
/// dedicated large allocator, both served by the arena's own [`ExtentHooks`]
/// through a [`HookProvider`]. Disjoint from every other arena's region by
/// construction, so §22.7 isolation holds geometrically. Built before the arena's
/// id is published (§22.4 "install hooks before first extent allocation"); torn
/// down (its regions returned to the hooks via `dealloc`) on destroy.
struct ArenaHookBackend<'a> {
    /// The owning arena's id (the registry key).
    arena: ArenaId,
    /// The arena's span-extent backing region.
    span_extents: ExtentManager<HookProvider<ArenaHooks<'a>>>,
    /// The arena's large-allocation backing (own descriptor pool; shared pagemap).
    large: LargeAllocator<'a, HookProvider<ArenaHooks<'a>>>,
}

// The hook registry is a fixed inline array in the (singleton) allocator, built on
// the stack at construction; keep it within a **stack-safe** size budget so neither a
// raised `MAX_HOOK_BACKENDS` nor a field added to `ArenaHookBackend` (nor a larger
// `RESERVATION_CAP`) can overflow the stack at init or silently bloat every allocator
// (~0.6 KiB/backend today; the dual-backend `AnyAllocator` holds one inline).
const _: () = assert!(
    core::mem::size_of::<[Option<ArenaHookBackend<'static>>; MAX_HOOK_BACKENDS]>() <= 48 * 1024,
    "hook registry exceeds its size budget; reduce MAX_HOOK_BACKENDS or RESERVATION_CAP",
);

impl ArenaHookBackend<'_> {
    /// **Explicitly** return both backing regions to the hooks (W10 strict
    /// teardown), surfacing a refusal instead of letting `Drop` swallow it. Both
    /// regions are torn down **even if the first refuses**, so neither is leaked
    /// gratuitously; the first error is reported. `Drop` is a no-op afterward (each
    /// manager has recorded its region as already released), so the hooks see each
    /// region returned exactly once.
    fn teardown(&mut self) -> Result<(), BackendError> {
        let span = self.span_extents.teardown();
        let large = self.large.teardown();
        span.and(large)
    }

    /// Aggregate this backing's per-kind hook-failure counts over its **two**
    /// providers (span-extent + large) for [`ArenaStats::hooks`] (W10 observability).
    fn hook_failure_stats(&self) -> HookFailureStats {
        let sp = self.span_extents.provider();
        let lp = self.large.provider();
        HookFailureStats {
            commit: sp.commit_hook_failures() + lp.commit_hook_failures(),
            release: sp.release_hook_failures() + lp.release_hook_failures(),
            split: sp.split_hook_failures() + lp.split_hook_failures(),
            merge: sp.merge_hook_failures() + lp.merge_hook_failures(),
        }
    }
}

/// The fixed-capacity registry of [`ArenaHookBackend`]s. Slots are assigned on
/// arena-create-with-hooks and cleared **in place** on destroy (never moved), so a
/// reference obtained for one arena's op stays valid under the §22.5/§36.13
/// quiescence contract (an arena's create/destroy does not race its own
/// alloc/free, and a *different* arena's destroy clears only its own slot). The
/// lock-free `count` is the fast path: `0` ⇒ no hooked arena exists, so every
/// allocation/free uses the shared backend with **zero** registry access.
struct HookRegistry<'a> {
    lock: RankedLock<{ LockRank::ARENA }>,
    slots: UnsafeCell<[Option<ArenaHookBackend<'a>>; MAX_HOOK_BACKENDS]>,
    count: AtomicUsize,
}

impl HookRegistry<'_> {
    const fn new() -> Self {
        Self {
            lock: RankedLock::new(),
            slots: UnsafeCell::new([const { None }; MAX_HOOK_BACKENDS]),
            count: AtomicUsize::new(0),
        }
    }
}

/// Persistent, lock-free cumulative hook-failure counters (W10). A torn-down hooked
/// backend's counts are folded in here **before it drops**, so the global stats
/// total survives an arena's destroy — the per-arena [`HookProvider`] counters drop
/// with the backend, but `release` failures in particular fire *during* teardown, so
/// they would otherwise be unobservable. Live backends' counts are added on top in
/// [`Allocator::stats`].
struct AtomicHookFailures {
    commit: AtomicU64,
    release: AtomicU64,
    split: AtomicU64,
    merge: AtomicU64,
}

impl AtomicHookFailures {
    const fn new() -> Self {
        Self {
            commit: AtomicU64::new(0),
            release: AtomicU64::new(0),
            split: AtomicU64::new(0),
            merge: AtomicU64::new(0),
        }
    }

    /// Fold a retiring backend's counts in (relaxed — pure monotone counters).
    fn accumulate(&self, s: &HookFailureStats) {
        self.commit.fetch_add(s.commit, Ordering::Relaxed);
        self.release.fetch_add(s.release, Ordering::Relaxed);
        self.split.fetch_add(s.split, Ordering::Relaxed);
        self.merge.fetch_add(s.merge, Ordering::Relaxed);
    }

    /// Read the cumulative counts so far.
    fn snapshot(&self) -> HookFailureStats {
        HookFailureStats {
            commit: self.commit.load(Ordering::Relaxed),
            release: self.release.load(Ordering::Relaxed),
            split: self.split.load(Ordering::Relaxed),
            merge: self.merge.load(Ordering::Relaxed),
        }
    }
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
    /// The arena under which the shared backing **regions** are reserved (the
    /// ambient default, §35.4 phase 3). Individual spans/large allocations are
    /// tagged with the *requesting* arena (§22.7), which may differ.
    arena: ArenaId,
    /// The arena registry (plan 06 W9): authority, quota, label, NUMA policy, and
    /// the §22.3/§36.13 lifecycle for every arena. The default arena (id 0) is
    /// registered `Active`/ambient at construction; explicit arenas are created
    /// through [`Allocator::arena_create`]. The allocation path gates and charges
    /// against it; reset/destroy drives it.
    arenas: ArenaTable,
    /// Central free lists: the small-object allocation layer (W5).
    central: CentralCache,
    /// Backing region for small-object spans.
    span_extents: ExtentManager<P>,
    /// Medium/large allocations (page-rounded extents + descriptors).
    large: LargeAllocator<'a, P>,
    /// Recycling span-descriptor pool.
    spans: SpanPool,
    /// Guards `spans.inner` ([`LockRank::SPAN_POOL`], rank 5 — a §27.2 refinement
    /// *outer* to the per-span lock, since `create_span` recycles a descriptor
    /// (which takes the per-span lock) while holding this; never held across a
    /// central-list or provider call). See the [`crate::lock`] module docs.
    span_lock: RankedLock<{ LockRank::SPAN_POOL }>,
    /// Cumulative usable bytes ever handed out (§31.1; relaxed — stats are
    /// monotone counters, not synchronization).
    allocated_bytes: AtomicU64,
    /// Cumulative usable bytes ever returned. `live = allocated - freed` by
    /// construction, so the §8.6 application-side identity cannot drift.
    freed_bytes: AtomicU64,
    /// Per-arena hooked backing regions (plan 06 W10, §22.2): an arena created with
    /// [`arena_create_hooked`](Self::arena_create_hooked) serves its span/large
    /// allocations from its own [`ExtentHooks`] instead of the shared backend.
    /// Empty (zero-overhead) for programs that never use custom backings.
    hooks: HookRegistry<'a>,
    /// Cumulative hook failures from **torn-down** hooked backings (W10), so the
    /// global stats total survives an arena's destroy. Live backings are summed on
    /// top in [`stats`](Self::stats).
    retired_hooks: AtomicHookFailures,
    /// Learned per-bucket placement hints (§24, plan 07 W14): the live end of the
    /// learn → place loop. The heap sampler publishes confident, consistent per-site
    /// profiles into this lock-free table ([`publish_learned_hints`](Self::publish_learned_hints));
    /// the medium/large path reads it (one atomic acquire load) to place a
    /// *placement-unhinted* allocation by its learned profile, the explicit hint always
    /// winning. Empty (and the read short-circuits to neutral) until a confident hint is
    /// published, so the medium/large placement outcome is unchanged when nothing is learned.
    learned: LearnedHints,
}

// SAFETY: `central`, `span_extents`, `large`, `pagemap`, and `arenas` carry
// their own synchronization; `meta`/`meta_region` are `Sync` by bound; the span
// pool's mutable state (`spans.inner`) is only touched under `span_lock`, and its
// immutable `slots`/`cap` support lock-free address↔slot conversion. The `hooks`
// registry's `UnsafeCell` slot array is touched only under its own `lock` (the
// §22.5/§36.13 quiescence contract keeps a slot stable for an arena's own ops, and
// destroy clears a slot in place — never moves it). So concurrent `&self` use is
// data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for Allocator<'_, P> {}
// SAFETY: the allocator owns its pool and `Send` extent/large managers; the
// borrowed pagemap/meta references are `Sync`, so they may be used from
// whichever thread owns the allocator.
unsafe impl<P: TopoBackingProvider + Send> Send for Allocator<'_, P> {}

impl<'a, P: TopoBackingProvider> Allocator<'a, P> {
    /// Build an allocator from two provider instances (one region each for
    /// spans and medium/large), a metadata source, and the pagemap.
    ///
    /// `meta` and `meta_region` are usually the same object (a [`MetaArena`],
    /// a [`BumpArena`], or the [`Bootstrap`](crate::Bootstrap));
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
        Self::new_inner(
            span_provider,
            large_provider,
            None,
            meta,
            meta_region,
            pagemap,
            arena,
            cfg,
        )
    }

    /// Build an allocator whose **medium/large path is served by a hugepage backend**
    /// (W11, the `hugepage_optimized` profile). `huge` is a §18.6
    /// [`RegionCacheHook`] — typically a
    /// [`HugePageBackend`](crate::HugePageBackend) — owned by the caller and living at
    /// least as long as the allocator (like the pagemap/metadata), so the engine and
    /// the hugepage backend are siblings, not a self-reference. Every medium/large
    /// allocation is then packed into hugepages (or served as a whole-hugepage run);
    /// the small-object path, the free path, and arenas are **unchanged** — the
    /// hugepage backend rides the existing large region-cache seam. The default
    /// [`new`](Self::new) keeps the M1 extent-backed large path.
    ///
    /// The caller reads the §19.7 coverage from its `huge` backend
    /// ([`HugePageBackend::coverage`](crate::HugePageBackend::coverage)) and records it
    /// into the stats snapshot ([`Stats::record_huge`](https://docs.rs/topo-stats)) —
    /// the hugepage backend is a sibling component, so its coverage is composed
    /// alongside [`stats`](Self::stats), as the W11 integration tests show.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_huge(
        span_provider: P,
        large_provider: P,
        huge: &'a (dyn RegionCacheHook + Sync),
        meta: &'a dyn MetadataAlloc,
        meta_region: &'a (dyn MetadataRegion + Sync),
        pagemap: &'a PageMap,
        arena: ArenaId,
        cfg: AllocatorConfig,
    ) -> Result<Self, ExtentError> {
        Self::new_inner(
            span_provider,
            large_provider,
            Some(huge),
            meta,
            meta_region,
            pagemap,
            arena,
            cfg,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        span_provider: P,
        large_provider: P,
        large_region_cache: Option<&'a (dyn RegionCacheHook + Sync)>,
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
        let mut large = LargeAllocator::new(
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
        if let Some(hook) = large_region_cache {
            large = large.with_region_cache(hook);
        }
        let spans = SpanPool::new(meta, cfg.span_slots).ok_or(ExtentError::Exhausted)?;
        Ok(Self {
            meta,
            meta_region,
            pagemap,
            arena,
            arenas: ArenaTable::new(),
            central: CentralCache::new(),
            span_extents,
            large,
            spans,
            span_lock: RankedLock::new(),
            allocated_bytes: AtomicU64::new(0),
            freed_bytes: AtomicU64::new(0),
            hooks: HookRegistry::new(),
            retired_hooks: AtomicHookFailures::new(),
            learned: LearnedHints::new(),
        })
    }

    /// The backend name of the providers (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        self.span_extents.backend_name()
    }

    // -- per-arena hooked backing routing (W10) -------------------------------

    /// Raw pointer to hooked-backend registry slot `i` (`i < MAX_HOOK_BACKENDS`).
    /// **Per-element** raw access is load-bearing for soundness: it keeps a borrow
    /// of one slot disjoint from a concurrent mutation of another, so a thread
    /// holding `&backend_X` (slot i) while a *different* arena's destroy clears slot
    /// j never has its borrow invalidated by a whole-array `&mut`. This is the same
    /// slot-pool discipline `ExtentMap`/`SpanPool`/`ArenaTable` use — never form a
    /// `&[Option; N]` / `&mut [Option; N]` over the registry.
    #[inline]
    fn hook_slot(&self, i: usize) -> *mut Option<ArenaHookBackend<'a>> {
        debug_assert!(i < MAX_HOOK_BACKENDS);
        // SAFETY: `i < MAX_HOOK_BACKENDS` (the array length); casting `*mut [T; N]`
        // to `*mut T` views the first element, and `.add(i)` indexes element `i`,
        // all within the same allocation.
        unsafe {
            self.hooks
                .slots
                .get()
                .cast::<Option<ArenaHookBackend<'a>>>()
                .add(i)
        }
    }

    /// The hooked backend serving `arena`, or `None` (the shared backend serves
    /// it). Fast path: with no hooked arenas the atomic `count` short-circuits
    /// before any lock. The returned reference is valid for the current op under
    /// the §22.5/§36.13 quiescence contract (see [`HookRegistry`]).
    #[inline]
    fn hook_backend(&self, arena: ArenaId) -> Option<&ArenaHookBackend<'a>> {
        // Common-case fast path: no hooked arena exists ⇒ zero registry/table access.
        if self.hooks.count.load(Ordering::Acquire) == 0 {
            return None;
        }
        // O(1): the arena records its own registry slot (W10), so route directly
        // instead of scanning. `0` ⇒ no hooked backing (the shared backend serves it).
        // The `count` acquire above synchronises with the registering `count` release,
        // which is program-ordered *after* `set_hook_slot`, so this read sees it.
        let slot_plus1 = self.arenas.hook_slot_of(arena);
        if slot_plus1 == 0 {
            return None;
        }
        let i = (slot_plus1 - 1) as usize;
        if i >= MAX_HOOK_BACKENDS {
            debug_assert!(false, "hook_slot out of range (W10)");
            return None;
        }
        self.hooks.lock.acquire();
        // SAFETY: the registry lock is held (no concurrent mutator of slot `i`);
        // `(*slot).as_ref()` borrows only element `i`. The recorded slot is confirmed
        // to still hold *this* arena's backing — a defensive identity check that
        // fails closed; under §22.5/§36.13 quiescence the arena's own create/destroy
        // cannot race this op, so the mapping is stable.
        let found = unsafe { (*self.hook_slot(i)).as_ref() }
            .filter(|b| b.arena == arena)
            .map(|b| b as *const ArenaHookBackend<'a>);
        self.hooks.lock.release();
        // SAFETY: the matched slot is stable for this op (see above); a *different*
        // arena's destroy mutates only its own slot (per-element), so it cannot move
        // or invalidate this one.
        found.map(|p| unsafe { &*p })
    }

    /// The span-extent backing for `arena`: its own hooked region, or the shared.
    #[inline]
    fn span_backing(&self, arena: ArenaId) -> &dyn ExtentBacking {
        match self.hook_backend(arena) {
            Some(b) => &b.span_extents,
            None => &self.span_extents,
        }
    }

    /// The large backing for `arena`: its own hooked region, or the shared.
    #[inline]
    fn large_backing(&self, arena: ArenaId) -> &dyn LargeBacking {
        match self.hook_backend(arena) {
            Some(b) => &b.large,
            None => &self.large,
        }
    }

    /// The large backend that **owns** allocations made in `arena` — its own hooked
    /// backing if it has one, else the shared backend. O(1): a large `free` /
    /// `realloc` / `usable_size` resolves the owning arena straight from the
    /// allocation's descriptor (`FreeTarget::Large { desc }` carries
    /// [`LargeDescriptor::arena`]), so there is no descriptor-pool search — the
    /// owning arena's hooked pool *is* where its descriptors live (W10). A non-hooked
    /// arena (including a foreign / already-freed pointer routed to the default
    /// arena) resolves to the shared backend.
    #[inline]
    fn large_owner_for(&self, arena: ArenaId) -> &dyn LargeBacking {
        match self.hook_backend(arena) {
            Some(b) => &b.large,
            None => &self.large,
        }
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
    ///
    /// The request routes to the arena named by `flags` (`TOPO_ARENA`, default
    /// arena 0). The arena must be registered and [`Active`](crate::ArenaState);
    /// the allocation is charged against its quota and refused if the arena lacks
    /// the [`ALLOC`](crate::CapRights::ALLOC) right or the charge would exceed the
    /// quota (§36.4) — every such failure is a clean null (the ABI maps it to
    /// `EINVAL`/`ENOMEM`).
    pub fn allocate(&self, size: usize, align: usize, flags: RequestFlags) -> *mut u8 {
        self.allocate_in(flags.arena(), size, align, flags)
    }

    /// W16-6 (§28.3, Appendix-F): whether this thread is currently inside an
    /// extent-hook call, so a re-entrant allocator operation must be **declined
    /// before any lock**. The hook runs under the non-re-entrant back-end lock;
    /// re-entering would deadlock on it (a large op) or trip the lock-order checker
    /// / take locks out of order (a small op). Declining is a *recoverable*
    /// defined-behaviour failure (like OOM) — **never** a panic, since we are inside
    /// the hook's locked context where an unwind could strand a held lock.
    ///
    /// The signal is the per-thread `hook_reentry` domain **alone** (a single
    /// Local-Exec TLS read): `HookGuard` sets it for *every* [`HookProvider`] hook
    /// call — whether the hooks back a per-arena region (W10 `arena_create_hooked`)
    /// **or the whole allocator** ([`Allocator::new`](Self::new) directly over a
    /// `HookProvider`, a documented custom-backing path). It deliberately does **not**
    /// pre-gate on `self.hooks.count` (per-arena registrations only): that count is 0
    /// for a direct-`HookProvider` backend even while its hook is live, so the
    /// re-entrant op would slip through to the back-end lock. `active()` is false on
    /// the common (no-hook) path, so the hot path still pays only one always-false
    /// TLS read.
    #[inline]
    fn hook_reentry_declines(&self) -> bool {
        crate::hooks::hook_reentry::active()
    }

    /// Allocate from an **explicit** arena (plan 06 W9), overriding the arena the
    /// `flags` encode. [`allocate`](Self::allocate) is `allocate_in(flags.arena(),
    /// …)`; `realloc` uses this to preserve the original allocation's arena across
    /// a move (§25.4), and the public arena API routes through it.
    ///
    /// SPEC-transition: object `→ Live` charged to `arena` (§7.2/§36.4)
    pub fn allocate_in(
        &self,
        arena: ArenaId,
        size: usize,
        align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        // A re-entrant hook allocation is declined cleanly (null) before any lock.
        if self.hook_reentry_declines() {
            return ptr::null_mut();
        }
        let Some(req) = classify(size, align, flags.raw()) else {
            return ptr::null_mut();
        };
        // The byte count charged to the arena and recorded — the class's usable
        // size, or the page-rounded extent length. Compute it (and fail on any
        // medium/large rounding overflow) *before* any state change.
        let usable = match req.kind {
            RequestKind::Small { usable, .. } => usable,
            RequestKind::Medium { .. } | RequestKind::Large { .. } => {
                match medium_large_usable(size) {
                    Some(bytes) => bytes,
                    None => return ptr::null_mut(),
                }
            }
        };
        // Gate + charge the arena (§22.3 state, §36.4 rights/quota). The ambient
        // default arena always admits (active, all rights, unlimited quota); an
        // explicit arena that is draining/over-quota/right-less refuses here.
        if self.arenas.try_charge(arena, usable as u64).is_err() {
            return ptr::null_mut();
        }
        // The shared medium/large path honours the `TOPO_ZERO` hint **itself** (so it
        // can elide the `memset` when its extent is freshly OS-zeroed, W15-5); the
        // engine then zeroes only the small + hooked-arena cases below.
        let mut large_self_zeroed = false;
        let p = match req.kind {
            RequestKind::Small { sc, .. } => {
                self.alloc_small(arena, sc, self.small_place_class(&req))
            }
            RequestKind::Medium { .. } | RequestKind::Large { .. } => {
                // Route to the arena's own hooked large backing if it has one (W10);
                // otherwise the shared backend, carrying the request's placement hints
                // so a wired hugepage backend packs by hotness/lifetime (W11) and a live
                // NUMA router places by the arena's policy (§15.5, W13 — resolved here so
                // the router need not know the arena). A hooked arena's custom backing
                // does not place by hints (it is the user's memory source), so it takes
                // the hint-less trait path — and the engine zeroes it (no commit-zeroed
                // promise from a user backing).
                match self.hook_backend(arena) {
                    Some(b) => b.large.allocate_in(arena, usable, req.align),
                    None => {
                        let mut hints = req.flags.hints_with_numa(self.arenas.numa_of(arena));
                        // W14: a placement-unhinted request adopts its bucket's learned
                        // profile (the live learn → place loop); an explicit hint wins.
                        let bucket = match req.kind {
                            RequestKind::Medium { .. } => SizeClassDist::BUCKET_MEDIUM,
                            _ => SizeClassDist::BUCKET_LARGE,
                        };
                        Self::apply_learned(&self.learned, &mut hints, bucket);
                        large_self_zeroed = true;
                        self.large
                            .allocate_in_hinted(arena, usable, req.align, hints)
                    }
                }
            }
        };
        if p.is_null() {
            // Storage exhaustion after a successful charge: undo it so the arena's
            // accounting stays exact (§36.17).
            self.arenas.credit(arena, usable as u64);
            return ptr::null_mut();
        }
        self.allocated_bytes
            .fetch_add(usable as u64, Ordering::Relaxed);
        // §26.2 zeroing: small objects (recycled slab memory, always dirty) and
        // hooked-arena large allocations are zeroed here; the shared medium/large path
        // already zeroed itself, eliding the `memset` when its extent was freshly
        // OS-zeroed (W15-5).
        if req.flags.hints().zero && !large_self_zeroed {
            // SAFETY: `p` is a live allocation with at least `usable` writable
            // bytes (the class's usable size, or the page-rounded extent length).
            unsafe { ptr::write_bytes(p, 0, usable) };
        }
        p
    }

    /// The learned per-bucket placement-hint table (§24, W14). The heap sampler publishes
    /// into it via [`publish_learned_hints`](Self::publish_learned_hints).
    #[inline]
    pub fn learned_hints(&self) -> &LearnedHints {
        &self.learned
    }

    /// Publish a [`SiteProfileTable`](crate::SiteProfileTable)'s confident, consistent
    /// per-bucket consensus into this engine's learned-hint table, so the allocation path
    /// places by it (the live end of the W14 learn → place loop). Called by the heap sampler
    /// from its (rare) sampled path. Lock-free for readers (§31.4).
    #[inline]
    pub fn publish_learned_hints<const CAP: usize>(
        &self,
        table: &crate::placement::SiteProfileTable<CAP>,
    ) {
        table.write_learned_hints(&self.learned);
    }

    /// Merge the learned hint for `bucket` into a **placement-unhinted** request's `hints`
    /// (§24, W14): if the request carries any explicit hotness/lifetime hint, it wins
    /// untouched; otherwise, when a confident, consistent learned hint exists for the bucket,
    /// adopt it (encoded back into the `u8`/[`Lifetime`] hint the filler decodes). When no
    /// hint is learned (the default / profiling-off case) the request's `hints` are left
    /// exactly as-is, so the resulting placement is unchanged. The result is advisory —
    /// it can only steer placement, never size/alignment/validity (§24.5).
    #[inline]
    fn apply_learned(learned: &LearnedHints, hints: &mut Hints, bucket: u16) {
        // "Placement-unhinted" = no explicit hotness (0) and no explicit lifetime. (A bare
        // `TOPO_COLD`, which is also hotness 0, is indistinguishable from unhinted here and
        // may adopt a learned hint — documented; the hint is advisory regardless.)
        if hints.hotness != 0 || hints.lifetime != Lifetime::Unspecified {
            return;
        }
        let lh = learned.lookup(bucket);
        if lh == crate::huge::PlaceHints::default() {
            return; // nothing confident learned ⇒ leave the request untouched.
        }
        // Encode the learned `Hotness` back into the `0..=255` hint the filler's `from_hint`
        // decodes (Cold→0, Neutral→64, Hot→200), and adopt the learned lifetime.
        hints.hotness = match lh.hotness {
            Hotness::Cold => 0,
            Hotness::Neutral => 64,
            Hotness::Hot => 200,
        };
        hints.lifetime = lh.lifetime;
    }

    /// The §24.6–§24.8 placement class for a **small** request: its explicit hint merged
    /// with the bucket's learned hint (explicit wins; learned applies only to a
    /// placement-unhinted request), distilled to a [`PlaceClass`] tag for span grouping. The
    /// default/unhinted case maps to a single class (consistent with the filler's
    /// `from_hint(0)`), so an all-default program keeps one span pool per size class (no
    /// fragmentation increase); only differing hints segregate spans. Advisory (§24.5).
    #[inline]
    fn small_place_class(&self, req: &Request) -> u8 {
        let mut hints = req.flags.hints();
        let bucket = match req.kind {
            RequestKind::Small { sc, .. } => sc.index() as u16,
            RequestKind::Medium { .. } => SizeClassDist::BUCKET_MEDIUM,
            RequestKind::Large { .. } => SizeClassDist::BUCKET_LARGE,
        };
        Self::apply_learned(&self.learned, &mut hints, bucket);
        let ph = crate::huge::PlaceHints {
            hotness: Hotness::from_hint(hints.hotness),
            lifetime: hints.lifetime,
        };
        PlaceClass::from_hints(ph).as_u8()
    }

    /// Small-object allocation: pull one object from the central list, **preferring a span
    /// of placement class `class`** (§24.6–§24.8, W14) so cold / hot / short-lived small
    /// objects cluster; creating and activating a new (class-tagged) span when none is
    /// available (§A.2's OOM-retry loop).
    ///
    /// Class grouping is advisory and never causes a spurious OOM: if a class-tagged span
    /// cannot be created (backend exhausted), the loop falls back to reusing *any* partial
    /// span ([`ANY_PLACE_CLASS`](crate::ANY_PLACE_CLASS)) before failing — availability
    /// before policy (§2.4).
    ///
    /// Termination: each iteration either returns an object or creates a new
    /// span; span creation draws from a finite region/pool, so a thread can
    /// loop only while *other* threads consume the objects it activates —
    /// i.e. only while the system as a whole makes progress.
    fn alloc_small(&self, arena: ArenaId, sc: SizeClassId, class: u8) -> *mut u8 {
        loop {
            match self
                .central
                .remove_batch(NodeId::DEFAULT, arena, Label::PUBLIC, sc, class, 1)
            {
                RemoveResult::Ok(batch) => {
                    debug_assert_eq!(batch.len(), 1);
                    // SAFETY: the batch's span pointer was installed from a
                    // valid descriptor in never-freed metadata (§27.5).
                    let span = unsafe { &*batch.span() };
                    return self.object_ptr(span, batch.index(0));
                }
                RemoveResult::NeedSpan => {
                    if self.create_span(arena, sc, class).is_err() {
                        // Backend exhausted creating a class-tagged span: as a last resort,
                        // reuse any existing partial span regardless of class, so grouping
                        // never turns into a spurious OOM (§2.4 availability-first).
                        return match self.central.remove_batch(
                            NodeId::DEFAULT,
                            arena,
                            Label::PUBLIC,
                            sc,
                            crate::central::ANY_PLACE_CLASS,
                            1,
                        ) {
                            RemoveResult::Ok(batch) => {
                                // SAFETY: as above — a live descriptor in never-freed metadata.
                                let span = unsafe { &*batch.span() };
                                self.object_ptr(span, batch.index(0))
                            }
                            RemoveResult::NeedSpan => ptr::null_mut(),
                        };
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
    fn create_span(&self, arena: ArenaId, sc: SizeClassId, class: u8) -> Result<(), ExtentError> {
        let row = size_class::row(sc);
        let bytes = (row.slab_pages as usize)
            .checked_mul(PAGE_SIZE)
            .ok_or(ExtentError::Exhausted)?;
        // The span's backing extent comes from the arena's own hooked region when it
        // has one (W10), else the shared span region. Captured once: the reference is
        // stable for this create under the §22.5/§36.13 quiescence contract.
        let backing = self.span_backing(arena);
        let ext = backing.alloc(bytes, PAGE_SIZE, Fit::Best)?;
        let region = match backing.region_of(ext) {
            Some(r) => r,
            None => return Err(ExtentError::Stale), // unreachable: ext is fresh
        };
        let base_ptr = region.base;
        let base = base_ptr as usize;
        let Some(layout) = SlabLayout::compute(sc, base, 0) else {
            let _ = backing.free(ext);
            return Err(ExtentError::Exhausted);
        };
        let Ok(object_count) = u32::try_from(layout.object_count) else {
            let _ = backing.free(ext);
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
            let _ = backing.free(ext);
            return Err(ExtentError::Exhausted);
        };

        let slot = self.spans.slot_ptr(idx);
        let initialized = if fresh {
            match SpanDescriptor::new(
                SpanId(idx),
                arena,
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
                (*slot)
                    .desc
                    .recycle(arena, sc, base, row.slab_pages, object_count, 0, self.meta)
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
            let _ = backing.free(ext);
            return Err(ExtentError::Exhausted);
        }

        // Publish: fill the bitmap, install in the pagemap, join the partial
        // list (W5-5). On failure, unwind everything.
        // SAFETY: the slot is live (acquired above) and its descriptor fully
        // initialized; descriptors live in never-freed metadata.
        let span = unsafe { &(*slot).desc };
        // Tag the fresh/recycled span with its placement class before it joins the central
        // partial list, so it enters the right §24.6–§24.8 grouping pool (advisory, §24.5).
        span.set_place_class(class);
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
                let _ = backing.free(ext);
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
        // W16-6 (Appendix-F): a hook that re-enters `free` would take a central/span
        // lock while holding the hook's back-end lock — a lock-order inversion (and
        // a same-arena large free deadlocks). Decline as a no-op *before* any lock;
        // the object leaks (the hook's contract violation), which is the §2.4 safe
        // degradation versus a deadlock or a checker-panic in the locked context.
        if self.hook_reentry_declines() {
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
                // Capture the class size *and the owning arena* before the
                // insert: once the object is free, the span may empty, retire,
                // and recycle under a racing thread, and its class/arena with it.
                let usable = size_class::usable_size(span.size_class());
                let arena = span.arena();
                let r = self.central.insert_batch(span, &[idx], 1);
                if r.inserted == 0 {
                    // Already central-free (double free) or the span raced
                    // its teardown; nothing was mutated (W8 hardening).
                    return FreeOutcome::DoubleFree;
                }
                self.freed_bytes.fetch_add(usable as u64, Ordering::Relaxed);
                // Credit the owning arena's quota (§36.17 exact accounting).
                self.arenas.credit(arena, usable as u64);
                if r.span_empty {
                    self.retire_span(span);
                }
                FreeOutcome::Freed
            }
            Ok(FreeTarget::Large { desc }) => {
                // SAFETY: `desc` is a live large descriptor — the pagemap classified
                // `ptr` as a live large allocation pointing to it, and descriptors
                // live in never-freed metadata. Its `arena` routes the free to the
                // owning backend in O(1): a hooked arena's large descriptors live in
                // its own pool, the rest in the shared pool (W10).
                let arena = unsafe { (*desc).arena() };
                let owner = self.large_owner_for(arena);
                // The owner re-resolves `ptr` under its own lock, so a concurrent
                // double free of the same pointer settles on exactly one winner
                // (large.rs DD-1) — only the winner (the call that returns `true`)
                // records the freed bytes, so the counter cannot double-count. The
                // usable size is captured first: after the free the descriptor may
                // recycle.
                let usable = owner.usable_size(ptr).unwrap_or(0);
                // SAFETY: this method's own contract is the large path's
                // contract, forwarded unchanged.
                if unsafe { owner.free(ptr) } {
                    self.freed_bytes.fetch_add(usable as u64, Ordering::Relaxed);
                    // Credit the owning arena's quota (§36.17 exact accounting).
                    self.arenas.credit(arena, usable as u64);
                    FreeOutcome::Freed
                } else {
                    FreeOutcome::DoubleFree
                }
            }
            Err(e) => FreeOutcome::Invalid(e),
        }
    }

    /// Retire an empty span (W5-5): deactivate it, clear its pagemap entries,
    /// revoke the backing's capability descendants, return the backing extent,
    /// and recycle the descriptor slot.
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
    ///
    /// **Revoke discipline (§36.6/§36.13).** The retired extent goes back to the
    /// single shared `span_extents` pool, from which a *later* `create_span` may
    /// satisfy a **different** arena — there is no per-arena span-extent pool. So
    /// the backing crosses authority domains, and recycling it while the owning
    /// arena's capability descendants still exist would hand live authority to
    /// another security domain. Normal retirement therefore revokes exactly as
    /// the reset/destroy teardown does (`Some(arena)`); on POSIX the revoke is the
    /// ambient-authority no-op (D2). A revoke failure here is unexpected (the
    /// owning arena is `Active`, not draining) and leaves the extent allocated and
    /// well-formed — a safe leak, never a cross-domain reuse. (A per-arena pool,
    /// which would make same-domain reuse revoke-free, is a plan-04 performance
    /// concern, M3+; "safety before policy" ships the correct revoke now.)
    fn retire_span(&self, span: &SpanDescriptor) {
        // Capture the owning arena before deactivation, mirroring
        // `force_retire_span` (the descriptor stays readable in never-freed
        // metadata, but the capture keeps the two retirement paths uniform).
        let arena = span.arena();
        self.central.deactivate_span(span, self.pagemap);
        let revoked = self.reclaim_span_slot(span, arena);
        debug_assert!(
            revoked,
            "retire_span: revoking an Active arena's retired backing must succeed"
        );
    }

    /// **Forcibly** retire a span regardless of liveness — the arena
    /// reset/destroy teardown (plan 06 W9-4b/W9-6b). Unlike
    /// [`retire_span`](Self::retire_span) this does not require the span be empty:
    /// any still-live objects are *discarded* (their pointers become invalid,
    /// §22.5), the span is unlinked and its pagemap entries cleared, and its
    /// backing extent is returned. Sound only when the owning arena is quiesced
    /// (`Resetting`/`Draining`), which the lifecycle enforces.
    ///
    /// SPEC-transition: span `Active -> Released` (forced, §22.5/§36.13)
    ///
    /// Returns whether the backing was **revoked and recycled** — `false` means a
    /// capability revoke failed (the extent stays allocated and well-formed), the
    /// §36.13 partial-failure signal the drain turns into a quarantine.
    fn force_retire_span(&self, span: &SpanDescriptor) -> bool {
        // Capture the class size + owning arena before the teardown clears the
        // bitmap; the descriptor (in never-freed metadata) stays readable.
        let usable = size_class::usable_size(span.size_class());
        let arena = span.arena();
        let discarded_live = self.central.deactivate_span_forced(span, self.pagemap);
        // The discarded live objects never pass through `free`, so account their
        // bytes as freed here — otherwise the cumulative `freed_bytes` (and thus
        // global `live_bytes = allocated - freed`) would overcount them as still
        // live after the arena's `used` is zeroed by reset/destroy. This keeps
        // `Σ arena.used == live_bytes` reconciling (§8.6/§36.17).
        if discarded_live > 0 {
            self.freed_bytes
                .fetch_add(discarded_live as u64 * usable as u64, Ordering::Relaxed);
        }
        // Drain returns the backing to a pool that may serve another arena, so
        // revoke its descendants before recycling (§36.6/§36.13).
        self.reclaim_span_slot(span, arena)
    }

    /// Clear `span`'s pagemap entries, recycle its descriptor slot, and revoke +
    /// return its backing extent — the shared tail of
    /// [`retire_span`](Self::retire_span) and
    /// [`force_retire_span`](Self::force_retire_span), run **after** the span has
    /// been deactivated in the central list.
    ///
    /// The §36.6/§36.13 recycle discipline is **unconditional**: the retired
    /// backing returns to the single shared `span_extents` pool, which a later
    /// `create_span` may hand to a *different* arena, so its capability
    /// descendants under `arena` are revoked before it is recycled — on every
    /// retirement, normal or forced. On POSIX the revoke is the ambient no-op
    /// (D2). Returns whether the revoke + recycle **succeeded**: `false` means the
    /// revoke failed, the extent stays allocated and well-formed (W4-5), and the
    /// caller turns that into a §36.13 quarantine (drain) or a safe leak (normal
    /// retirement). (No per-arena pool exists yet — were one added for
    /// same-domain reuse, M3+, this revoke could be elided for that path.)
    fn reclaim_span_slot(&self, span: &SpanDescriptor, arena: ArenaId) -> bool {
        self.pagemap.retire_span(span);

        let Some(idx) = self.spans.index_of(span as *const SpanDescriptor) else {
            debug_assert!(false, "reclaim_span_slot: span not from this pool");
            return true;
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
        // Revoke-before-recycle; a revoke failure leaves the extent allocated
        // (well-formed) and is reported so a drain quarantines (§36.13). Route to
        // the arena's own hooked backing if it has one (W10) — the same backing the
        // extent was carved from in `create_span`.
        self.span_backing(arena).free_revoking(ext, arena).is_ok()
    }

    // -- introspection --------------------------------------------------------

    /// The usable size of the allocation at `ptr` (§10.1
    /// `malloc_usable_size`): the class's object size for small allocations,
    /// the page-rounded extent length for medium/large. `None` for null,
    /// foreign, interior, metadata, released, **and freed** pointers — a
    /// small object whose free-bitmap bit is set is not a live allocation
    /// (the W8 liveness probe; freed large allocations already resolve
    /// `None` because their pagemap entries are retired).
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        if ptr.is_null() {
            return None;
        }
        match validate_free(self.pagemap, self.meta_region, ptr as usize) {
            Ok(FreeTarget::Small { span, object_index }) => {
                // SAFETY: descriptors live in never-freed metadata.
                let span = unsafe { &*span };
                if span.is_central_free(object_index) {
                    return None; // freed, awaiting reuse: not a live object
                }
                Some(size_class::usable_size(span.size_class()))
            }
            // Route to the backend that owns `ptr` (shared or a hooked arena's),
            // since each resolves only its own descriptor pool (W10). O(1): the
            // descriptor names its arena directly.
            // SAFETY: `desc` is a live large descriptor (never-freed metadata).
            Ok(FreeTarget::Large { desc }) => self
                .large_owner_for(unsafe { (*desc).arena() })
                .usable_size(ptr),
            _ => None,
        }
    }

    /// Whether `ptr` is a **live** allocation of this allocator (the base
    /// pointer of a live small object or a medium/large allocation).
    /// Interior, metadata, released, foreign, null, and freed-awaiting-reuse
    /// pointers are not owned. For "is this address ours at all (live or
    /// not)?", see [`recognizes`](Self::recognizes).
    pub fn owns(&self, ptr: *mut u8) -> bool {
        if ptr.is_null() {
            return false;
        }
        match validate_free(self.pagemap, self.meta_region, ptr as usize) {
            Ok(FreeTarget::Small { span, object_index }) => {
                // SAFETY: descriptors live in never-freed metadata.
                !unsafe { &*span }.is_central_free(object_index)
            }
            Ok(FreeTarget::Large { .. }) => true,
            _ => false,
        }
    }

    /// Whether `addr` lies in memory this allocator manages **at all** — a
    /// live object, a freed object awaiting reuse, an interior byte, a
    /// released-but-retained page, or allocator metadata. The complement is
    /// the §35.2 mixed-allocator routing predicate: an address this returns
    /// `false` for was never produced by this allocator, so an adapter that
    /// layers TopoMalloc over another allocator (the `GlobalAlloc` bootstrap
    /// window) may hand it back to that other allocator.
    pub fn recognizes(&self, ptr: *mut u8) -> bool {
        !ptr.is_null()
            && (!self.pagemap.lookup(ptr as usize).is_empty()
                || self.meta_region.contains(ptr as usize))
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
    /// * medium/large grow whose rounded `new_size` still fits the current
    ///   extent and whose base satisfies the alignment → in-place, return `ptr`
    ///   (W15-3a).
    /// * medium/large **shrink** across a page boundary → in-place, splitting
    ///   off and returning the tail pages to the backend, return `ptr` (W15-3b;
    ///   best-effort — a cache-served region or an exhausted split keeps the
    ///   allocation whole, which is still correct).
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
        // W16-6 (Appendix-F): decline a re-entrant hook `realloc` cleanly (null);
        // the §25.1 contract is "failure preserves the original", so nothing leaks.
        if self.hook_reentry_declines() {
            return ptr::null_mut();
        }
        if ptr.is_null() {
            return self.allocate(new_size, min_align, flags);
        }
        if !min_align.is_power_of_two() {
            return ptr::null_mut();
        }

        // Classify the existing allocation; reject anything we do not own
        // without touching it (§35.2). Capture the owning arena alongside the
        // usable size so the move path preserves it (§25.4 "arena preserved").
        let (old_usable, old_arena) =
            match classify_ptr(self.pagemap, self.meta_region, ptr as usize) {
                PointerClass::Small {
                    sc,
                    span,
                    object_index,
                    ..
                } => {
                    // SAFETY: descriptors live in never-freed metadata.
                    let span_ref = unsafe { &*span };
                    // W8 liveness probe: a freed object awaiting reuse is not a
                    // valid realloc source (resizing it "in place" would alias the
                    // central free list; copying from it would read stale bytes).
                    if span_ref.is_central_free(object_index) {
                        return ptr::null_mut();
                    }
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
                    (row.size as usize, span_ref.arena())
                }
                PointerClass::Large { desc } => {
                    // Route to the backend that owns `ptr` — the shared large region
                    // or the original arena's own hooked region (W10); each resolves
                    // only its own descriptor pool, so the shared one would miss a
                    // hooked-arena large object. O(1): the descriptor names its arena.
                    // SAFETY: `desc` is a live large descriptor (never-freed metadata).
                    let arena = unsafe { (*desc).arena() };
                    let owner = self.large_owner_for(arena);
                    // The pointer is live (the caller's contract for realloc; a
                    // racing free of the same pointer is C UB and is caught by
                    // the lock-validated read here).
                    let Some(usable) = owner.usable_size(ptr) else {
                        return ptr::null_mut();
                    };
                    // In-place fast path (W15-3a grow / W15-3b shrink): the new request
                    // is still medium/large (a small-class request moves, so the extent
                    // is returned rather than pinned under a tiny object) and the base
                    // satisfies the requested alignment (in-place can never re-base).
                    // `apply_inplace_large_resize` shrinks (returns the tail), stays, or
                    // grows (absorbs the adjacent free extent) toward `rounded`, with the
                    // exact §8.6/§36.17 accounting and the arena preserved (§25.4); it is
                    // best-effort, so it never fails — it returns the achieved usable size.
                    // We keep `ptr` iff that satisfies the request; otherwise we move.
                    if let Some(req) = classify(new_size, min_align, flags.raw()) {
                        if !matches!(req.kind, RequestKind::Small { .. })
                            && (ptr as usize).is_multiple_of(min_align)
                        {
                            let effective = if new_size == 0 { 1 } else { new_size };
                            if let Some(rounded) = align_up(effective, PAGE_SIZE) {
                                // SAFETY: `ptr` is the live base pointer being realloc'd
                                // (this function's own contract); not concurrently freed.
                                let achieved = unsafe {
                                    self.apply_inplace_large_resize(
                                        arena,
                                        owner,
                                        ptr,
                                        usable,
                                        rounded,
                                        flags.hints().zero,
                                    )
                                };
                                if achieved >= rounded {
                                    // Satisfied in place (shrank, stayed, or grew); the
                                    // result holds the old prefix at the same base.
                                    return ptr;
                                }
                                // Could not grow in place ⇒ fall to the always-correct move.
                            }
                        }
                    }
                    // The owning arena was resolved from the descriptor above.
                    (usable, arena)
                }
                _ => return ptr::null_mut(),
            };

        // Move path (W15-2): allocate first — the original is untouched until
        // the new object exists — copy min(old_usable, new_size), free last.
        // The new object is allocated in the *original's* arena (§25.4), not the
        // arena the `flags` happen to name.
        let q = self.allocate_in(old_arena, new_size, min_align.max(MIN_ALIGN), flags);
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

    /// Apply an in-place medium/large resize of the live large allocation `ptr`
    /// (served by `owner`, charged to `arena`) toward `rounded` page-aligned bytes,
    /// returning the **achieved** usable size. Shared by `realloc` (which moves when
    /// the achieved size is short of the request) and `resize_in_place`/`xallocx`
    /// (which never moves). It performs the §8.6/§36.17 accounting for whichever path
    /// it takes — a tail-returning **shrink** (W15-3b) credits the freed tail, an
    /// adjacent-extent-absorbing **grow** (W15-3a) charges the growth (preserving the
    /// arena, §25.4) — and is **best-effort**: a shrink/grow the backend declines
    /// (cache-served, slot/commit/metadata exhaustion, or no adjacent free for a grow)
    /// leaves the allocation whole and returns the unchanged usable size, so a grow
    /// that could not be satisfied surfaces as `achieved < rounded`.
    ///
    /// # Safety
    ///
    /// `ptr` is the live base pointer being resized (the §25 realloc / §10.3 xallocx
    /// contract); the caller does not concurrently free or realloc it.
    unsafe fn apply_inplace_large_resize(
        &self,
        arena: ArenaId,
        owner: &dyn LargeBacking,
        ptr: *mut u8,
        usable: usize,
        rounded: usize,
        zero: bool,
    ) -> usize {
        if rounded < usable {
            // SAFETY: `ptr` is the live base pointer being resized (forwarded contract).
            if let Some(freed) = unsafe { owner.shrink(ptr, rounded) } {
                self.freed_bytes.fetch_add(freed as u64, Ordering::Relaxed);
                self.arenas.credit(arena, freed as u64);
                return usable - freed;
            }
        } else if rounded > usable {
            // Charge the growth before absorbing (so the arena's quota is honored,
            // §36.4); refund if the backend cannot grow in place.
            let extra = (rounded - usable) as u64;
            if self.arenas.try_charge(arena, extra).is_ok() {
                // SAFETY: as above.
                if let Some(grown) = unsafe { owner.grow(ptr, rounded) } {
                    self.allocated_bytes
                        .fetch_add(grown as u64, Ordering::Relaxed);
                    // §26.2 zeroing: an in-place grow absorbs the address-adjacent free
                    // extent, which may be a **dirty** (retained) range carrying recycled
                    // bytes. A `TOPO_ZERO` realloc/xallocx must hand back the newly exposed
                    // suffix `[usable, usable+grown)` zeroed — exactly as the move path does
                    // (allocate-zeroed then copy the prefix) — never leaking those bytes
                    // (the gap this closes; the old prefix `[0, usable)` is untouched).
                    if zero {
                        // SAFETY: the grow published `usable + grown` committed, writable
                        // usable bytes at `ptr` (the extent is committed and the new pages
                        // pagemap-mapped *before* `grow_usable`); `[usable, usable+grown)`
                        // is in bounds and exclusively owned (the realloc/xallocx contract).
                        unsafe { ptr::write_bytes(ptr.add(usable), 0, grown) };
                    }
                    return usable + grown;
                }
                self.arenas.credit(arena, extra);
            }
        }
        usable
    }

    /// Resize `ptr` **in place only** (never move, never free) toward `new_size`
    /// bytes at `min_align`, returning the resulting usable size — the engine behind
    /// the extended `xallocx` (§10.3). A small object's slab slot is fixed-size, so
    /// it cannot resize: its usable size is reported unchanged (the caller's
    /// `result >= size` test decides success). A medium/large allocation shrinks
    /// (returns the tail, W15-3b) or grows (absorbs the adjacent free extent, W15-3a)
    /// toward the page-rounded `new_size`, best-effort. `None` only for a pointer
    /// this allocator does not own, an interior pointer, a non-power-of-two
    /// `min_align`, or a base that does not already satisfy `min_align` (in-place can
    /// never re-base) — the deterministic `xallocx` invalid-argument cases.
    ///
    /// # Safety
    ///
    /// As [`realloc`](Self::realloc): `ptr` is null, an owned live allocation, or
    /// never-owned memory (rejected with `None`); not concurrently freed/realloc'd.
    pub unsafe fn resize_in_place(
        &self,
        ptr: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> Option<usize> {
        // W16-6 (Appendix-F): decline a re-entrant hook in-place resize (`None`,
        // no resize) before any lock — the allocation is unchanged.
        if self.hook_reentry_declines() {
            return None;
        }
        if ptr.is_null() || !min_align.is_power_of_two() {
            return None;
        }
        match classify_ptr(self.pagemap, self.meta_region, ptr as usize) {
            PointerClass::Small {
                sc,
                span,
                object_index,
                ..
            } => {
                // SAFETY: descriptors live in never-freed metadata.
                let span_ref = unsafe { &*span };
                if span_ref.is_central_free(object_index) {
                    return None; // a freed object awaiting reuse is not a resize source
                }
                // In-place can never re-base, so the requested alignment must hold.
                if !(ptr as usize).is_multiple_of(min_align) {
                    return None;
                }
                // A fixed-size slab slot cannot resize in place; report its usable size.
                size_class::checked_row(sc).map(|r| r.size as usize)
            }
            PointerClass::Large { desc } => {
                // SAFETY: `desc` is a live large descriptor (never-freed metadata).
                let arena = unsafe { (*desc).arena() };
                let owner = self.large_owner_for(arena);
                let usable = owner.usable_size(ptr)?;
                if !(ptr as usize).is_multiple_of(min_align) {
                    return None;
                }
                let effective = if new_size == 0 { 1 } else { new_size };
                let rounded = align_up(effective, PAGE_SIZE)?;
                // SAFETY: `ptr` is the live base pointer being resized (this fn's contract).
                Some(unsafe {
                    self.apply_inplace_large_resize(
                        arena,
                        owner,
                        ptr,
                        usable,
                        rounded,
                        flags.hints().zero,
                    )
                })
            }
            _ => None,
        }
    }

    // -- arena lifecycle & authority (plan 06 W9) ------------------------------

    /// The arena registry (§22, §36.4) — its authority lattice, quotas, labels,
    /// NUMA policy, and lifecycle state. The allocation path gates and charges
    /// against it; the lifecycle methods below drive it.
    pub fn arenas(&self) -> &ArenaTable {
        &self.arenas
    }

    /// Create a new explicit arena from `policy` (§22.4, W9-3). Validates the
    /// policy, claims a slot, and publishes the id only after initialization
    /// completes. Returns the new [`ArenaId`], routable through `TOPO_ARENA` (and
    /// [`allocate_in`](Self::allocate_in)).
    ///
    /// SPEC-transition: arena create `Initializing → Active` (§22.4)
    pub fn arena_create(&self, policy: &ArenaPolicy) -> Result<ArenaId, ArenaError> {
        self.arenas.create(policy)
    }

    /// Create an explicit arena whose span/large allocations are served from its
    /// **own** [`ExtentHooks`] backing region (§22.2/§22.4 per-arena hooks, plan 06
    /// W10) — a custom memory source / OS policy private to this arena, isolated
    /// from every other arena's region by construction (§22.7). `region_cfg` sizes
    /// the arena's backing (its `span_region_bytes` / `large_region_bytes` and the
    /// descriptor-pool capacities). The hooks must outlive the allocator (they are
    /// borrowed, like the metadata/pagemap), so the core stays allocation-free.
    ///
    /// The §22.4 order is honoured: the hooked backing is reserved and **registered
    /// before** the arena id is published `Active` (the id is private to this call
    /// until then, so the unpublished window is race-free). On any failure the
    /// pending arena is abandoned and nothing leaks.
    ///
    /// Returns [`ArenaError::Exhausted`] if the hooked-backend registry is full
    /// ([`MAX_HOOK_BACKENDS`]) or the hooks cannot reserve the backing.
    ///
    /// SPEC-transition: arena create with hooks `Initializing → Active` (§22.4)
    pub fn arena_create_hooked(
        &self,
        policy: &ArenaPolicy,
        hooks: ArenaHooks<'a>,
        region_cfg: AllocatorConfig,
    ) -> Result<ArenaId, ArenaError> {
        let id = self.arenas.create_pending(policy)?;
        match self.build_and_register_hook_backend(id, hooks, region_cfg) {
            Ok(()) => match self.arenas.publish(id) {
                Ok(()) => Ok(id),
                Err(e) => {
                    // Unreachable (id is Initializing), but stay leak-free. The
                    // rollback ignores a teardown error: there is no clean arena to
                    // quarantine here, and the regions are returned best-effort.
                    let _ = self.teardown_hook_backend(id);
                    let _ = self.arenas.abandon_pending(id);
                    Err(e)
                }
            },
            Err(e) => {
                let _ = self.arenas.abandon_pending(id);
                Err(e)
            }
        }
    }

    /// Build the per-arena hooked backing (reserving its regions via `hooks`) and
    /// register it in the [`HookRegistry`]. Builds **first** (so two concurrent
    /// creations never claim the same slot), then finds a free slot and inserts
    /// atomically under the registry lock; on no free slot the just-built backing is
    /// dropped, returning its regions to the hooks.
    fn build_and_register_hook_backend(
        &self,
        id: ArenaId,
        hooks: ArenaHooks<'a>,
        cfg: AllocatorConfig,
    ) -> Result<(), ArenaError> {
        let span_extents = ExtentManager::new(
            HookProvider::new(hooks),
            self.meta,
            id,
            cfg.span_region_bytes,
            PAGE_SIZE,
            cfg.span_extent_slots,
        )
        .map_err(|_| ArenaError::Exhausted)?;
        let large = LargeAllocator::new(
            HookProvider::new(hooks),
            self.meta,
            self.pagemap,
            id,
            LargeConfig {
                region_bytes: cfg.large_region_bytes,
                region_align: PAGE_SIZE,
                extent_slots: cfg.large_extent_slots,
                large_slots: cfg.large_slots,
            },
        )
        .map_err(|_| ArenaError::Exhausted)?;
        // §23.3 no-overlap **across the two reservations**: the span and large
        // regions are served by separate `HookProvider`s, so neither provider's
        // own tracker sees the other's range. Check disjointness explicitly here —
        // a hook that returns an overlapping span/large region would otherwise let
        // the arena's small-object spans and large allocations alias. On overlap,
        // both `span_extents` and `large` drop, returning their regions to the hooks.
        {
            let (sb, se) = span_extents.reserved_region().addr_range();
            let (lb, le) = large.reserved_region().addr_range();
            let disjoint = se <= lb || le <= sb;
            debug_assert!(
                disjoint,
                "§23.3: a hooked arena's span and large regions overlap (W10)"
            );
            if !disjoint {
                return Err(ArenaError::Exhausted);
            }
        }
        // Held in an `Option` so a failed insert (registry full) drops the built
        // backing here, returning its regions to the hooks — and so the move into a
        // slot is a single, unconditional `take`.
        let mut backend = Some(ArenaHookBackend {
            arena: id,
            span_extents,
            large,
        });
        self.hooks.lock.acquire();
        for i in 0..MAX_HOOK_BACKENDS {
            let slot = self.hook_slot(i);
            // SAFETY: registry lock held ⇒ no concurrent access to slot `i`; the
            // per-element borrow never forms a whole-array reference.
            if unsafe { (*slot).is_none() } {
                // SAFETY: as above; overwrites the `None` slot with the backing.
                unsafe { *slot = backend.take() };
                // Record the arena's slot for O(1) routing (W10), program-ordered
                // **before** the `count` release below — so a reader that observes
                // `count ≥ 1` (Acquire) also observes this slot index.
                self.arenas.set_hook_slot(id, (i + 1) as u8);
                // Release: pairs with the `Acquire` in `hook_backend`; the later
                // arena `publish` (a Release on the arena state) program-orders after
                // this, so a thread that sees the arena `Active` also sees the slot.
                self.hooks.count.fetch_add(1, Ordering::Release);
                break;
            }
        }
        self.hooks.lock.release();
        if backend.is_none() {
            Ok(()) // inserted
        } else {
            Err(ArenaError::Exhausted) // registry full; `backend` drops → regions freed
        }
    }

    /// Remove `arena`'s hooked backing from the registry (if any) and **tear it
    /// down**, returning its span/large regions to the hooks via `dealloc` and
    /// surfacing a refusal (W10 strict teardown). Called on a *completed* drain,
    /// after every span/large of the arena was retired. The backing is moved out
    /// under the registry lock and torn down *outside* it, so the user `dealloc`
    /// hook never runs while the lock is held.
    ///
    /// Returns `Ok(())` for a non-hooked arena (a no-op) or a clean teardown, and
    /// `Err` (the first backing error) if a hook refused a region return — the
    /// destroy path routes that into the §36.13 quarantine; the (unreachable)
    /// create-rollback path ignores it.
    fn teardown_hook_backend(&self, arena: ArenaId) -> Result<(), BackendError> {
        if self.hooks.count.load(Ordering::Acquire) == 0 {
            return Ok(());
        }
        self.hooks.lock.acquire();
        let mut taken: Option<ArenaHookBackend<'a>> = None;
        for i in 0..MAX_HOOK_BACKENDS {
            let slot = self.hook_slot(i);
            // SAFETY: registry lock held; per-element borrow of slot `i` only.
            let matches = unsafe { (*slot).as_ref().is_some_and(|b| b.arena == arena) };
            if matches {
                // Clear the arena's O(1) routing index (W10), then move the backing
                // out of its slot (leaving `None`). The arena is quiesced (§22.5/
                // §36.13), so no concurrent op of its own can race this.
                self.arenas.set_hook_slot(arena, 0);
                // SAFETY: as above; moves the backing out of its slot (leaving `None`).
                taken = unsafe { (*slot).take() };
                self.hooks.count.fetch_sub(1, Ordering::Release);
                break;
            }
        }
        self.hooks.lock.release();
        // Tear the regions down (via `hooks.dealloc`) outside the lock; `Drop` then
        // no-ops. `None` (non-hooked arena) is a clean no-op.
        match taken {
            Some(mut backend) => {
                let r = backend.teardown();
                // Fold the retiring backing's cumulative hook failures into the
                // persistent total before it drops, so they survive the destroy —
                // a `release` failure in particular fires *during* the teardown above
                // (W10 observability).
                self.retired_hooks.accumulate(&backend.hook_failure_stats());
                r
            }
            None => Ok(()),
        }
    }

    /// Reconfigure an arena's non-authority policy (§22.4 *configure*, F-005):
    /// decay, hugepage policy, NUMA preference, soft cache budget, and name. The
    /// authority (rights/label) and quota ceiling are immutable here (a configure
    /// can never widen authority — §36.4); change authority by delegation.
    ///
    /// SPEC-transition: arena configure (§22.4)
    pub fn arena_configure(&self, arena: ArenaId, cfg: &ArenaConfig) -> Result<(), ArenaError> {
        self.arenas.configure(arena, cfg)
    }

    /// Delegate an attenuated child arena from `parent` (§36.4/§36.14, W9-5).
    /// Enforces authority/quota/label monotonicity — a delegation can only
    /// narrow rights, cap quota at the parent's remaining budget, and preserve
    /// the label (§36.16).
    ///
    /// SPEC-transition: arena delegate (§36.4, attenuation-only)
    pub fn arena_delegate(&self, parent: ArenaId, del: &Delegation) -> Result<ArenaId, ArenaError> {
        self.arenas.delegate(parent, del)
    }

    /// Whether `arena` currently has a registered hooked backing (W10): `true` from
    /// a successful [`arena_create_hooked`](Self::arena_create_hooked) until the
    /// backing is torn down. Lets a caller (the C ABI) reclaim a hook adapter exactly
    /// when the allocator no longer references it — after a clean destroy **or** a
    /// teardown-failure quarantine (the backing was dropped ⇒ `false`), but *not* a
    /// drain-failure quarantine (the backing, and its borrow of the adapter, are
    /// retained ⇒ `true`).
    pub fn arena_has_hook_backend(&self, arena: ArenaId) -> bool {
        self.arenas.hook_slot_of(arena) != 0
    }

    /// A snapshot of `arena`'s authority + accounting (§22.2/§36.4), or `None`
    /// for an out-of-range id. For a hooked arena (W10), the
    /// [`hooks`](ArenaStats::hooks) field carries its custom backing's per-kind
    /// hook-failure counts, aggregated over the arena's span + large providers.
    pub fn arena_stats(&self, arena: ArenaId) -> Option<ArenaStats> {
        let mut s = self.arenas.stats(arena)?;
        // Read the hooked backing's failure counts **under the registry lock** (unlike
        // `hook_backend`, which releases it and returns a reference for the alloc/free
        // path, safe only because an arena's own op cannot race its destroy, §22.5).
        // `arena_stats` is introspection — it *can* race `arena_destroy(arena)` — so it
        // must hold the lock across the read, which blocks the teardown from removing
        // and dropping the backing mid-read (teardown takes the same lock to remove a
        // slot before dropping its backing outside the lock).
        if self.hooks.count.load(Ordering::Acquire) != 0 {
            let slot_plus1 = self.arenas.hook_slot_of(arena);
            if slot_plus1 != 0 && (slot_plus1 as usize) <= MAX_HOOK_BACKENDS {
                let i = (slot_plus1 - 1) as usize;
                self.hooks.lock.acquire();
                // SAFETY: the registry lock is held (so no concurrent mutator of slot
                // `i`, and no teardown can drop the backing during the read); borrows
                // only element `i`, confirmed to be this arena's (fails closed).
                if let Some(b) = unsafe { (*self.hook_slot(i)).as_ref() } {
                    if b.arena == arena {
                        s.hooks = Some(b.hook_failure_stats());
                    }
                }
                self.hooks.lock.release();
            }
        }
        Some(s)
    }

    /// **Reset** an explicit arena (§22.5, W9-4): discard all its extant
    /// allocations, return their backing to the provider, zero its accounting,
    /// bump its reset generation, and leave it `Active`. The default arena cannot
    /// be reset ([`ArenaError::IsDefault`]).
    ///
    /// Ordered, partial-failure-safe: `Active → Resetting`, drain (the internal
    /// `drain_arena`), then `Resetting → Active` on success or `→
    /// ErrorQuarantined` on a partial drain failure (never silently "reset").
    /// Returns the new generation.
    ///
    /// # Safety
    ///
    /// The caller guarantees the arena is quiesced — no thread is allocating from
    /// or freeing into it (§22.5 precondition) — and accepts that every
    /// outstanding pointer into the arena becomes invalid.
    ///
    /// SPEC-transition: arena reset (§22.5)
    pub unsafe fn arena_reset(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        self.arenas.begin_reset(arena)?;
        // SAFETY: the arena is now `Resetting` (no new allocations admitted) and
        // the caller guarantees quiescence — the drain precondition.
        if unsafe { self.drain_arena(arena) } {
            self.arenas.finish_reset(arena)
        } else {
            // Partial failure: quarantine rather than report a clean reset (§36.13).
            self.arenas.quarantine(arena)?;
            Err(ArenaError::IllegalTransition)
        }
    }

    /// **Destroy** an explicit arena (§22.6/§36.13, W9-4d/W9-6): reset it, then
    /// remove its metadata and retire its id behind a generation bump (§B.5 id
    /// non-reuse-while-stale). The default arena cannot be destroyed.
    ///
    /// Ordered, partial-failure-safe: `Active → Draining` (reject new
    /// allocations/delegations, §36.13 step 1), drain caches + return/unmap +
    /// revoke + recycle (the internal `drain_arena` — the POSIX collapse of the
    /// §36.13 unmap→revoke→recycle order; real capability revocation drops in at
    /// plan 09), return any per-arena hooked backing region to its hooks (W10), then
    /// `Draining → Destroyed` on success or `→ ErrorQuarantined` on partial failure
    /// — **never** `Destroyed` on failure (§36.13). A partial failure is either a
    /// drain that could not revoke a backing **or** a custom backing that refused
    /// its region return on teardown. Returns the new generation.
    ///
    /// # Safety
    ///
    /// As [`arena_reset`](Self::arena_reset): the arena must be quiesced and the
    /// caller accepts that outstanding pointers become invalid.
    ///
    /// SPEC-transition: arena destroy (§22.6/§36.13)
    pub unsafe fn arena_destroy(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        self.arenas.begin_destroy(arena)?;
        // SAFETY: the arena is now `Draining` (no new allocations/delegations)
        // and the caller guarantees quiescence.
        if unsafe { self.drain_arena(arena) } {
            // The drain retired every span/large of the arena (their backing extents
            // are now free in its hooked region, if any); return that region to the
            // hooks **before** the terminal `Destroyed` step (W10 strict teardown),
            // so a custom backing that refuses the return routes into the §36.13
            // quarantine instead of being lost after a clean destroy. A no-op (Ok)
            // for a non-hooked arena.
            if self.teardown_hook_backend(arena).is_err() {
                // Objects all drained, but the backing refused a region return:
                // quarantine (Draining → ErrorQuarantined) rather than report a clean
                // destroy (§36.13). The id is NOT retired/reused; the refused region
                // is a leak confined to the user's backing (its `dealloc` chose so).
                self.arenas.quarantine(arena)?;
                return Err(ArenaError::IllegalTransition);
            }
            let gen = self.arenas.finish_destroy(arena)?;
            Ok(gen)
        } else {
            // Partial drain failure: quarantine and KEEP the hooked backing for
            // recovery (never tear it down on a non-clean drain, §36.13).
            self.arenas.quarantine(arena)?;
            Err(ArenaError::IllegalTransition)
        }
    }

    /// Reclaim every span and large allocation belonging to `arena` (W9-4b/4c/6b:
    /// "no cache holds an arena object" after drain, B.5), revoking each backing's
    /// descendant capabilities before recycling (§36.6/§36.13). Every object is
    /// retired (so the arena's live bytes go to zero); returns `true` if **every**
    /// backing was also revoked and recycled, `false` on a partial failure (a
    /// revoke the provider refused — the §36.13 quarantine signal). It drains
    /// *all* objects regardless, accumulating revoke failures, so a partial
    /// failure leaks only the un-revoked backing, never live objects.
    ///
    /// On POSIX the §36.13 unmap→revoke→recycle steps collapse — `revoke` is a
    /// no-op that always succeeds — so the *structure* is identical and the
    /// seLe4n capability provider (plan 09) is a drop-in with real revocation.
    ///
    /// # Safety
    ///
    /// The arena must be in a draining/resetting state with no concurrent
    /// allocation or free of its objects (the §22.5/§36.13 precondition the
    /// lifecycle transition established).
    unsafe fn drain_arena(&self, arena: ArenaId) -> bool {
        let mut all_revoked = true;
        // Step 1 (small): force-retire every active span of this arena. Walk the
        // descriptor pool; a slot's descriptor is stable to read because slots
        // live in never-freed metadata (§27.5) and only *this* arena's spans are
        // mutated here (the arena is quiesced). Snapshot the high-water mark so a
        // concurrent create for *another* arena does not extend the scan under us.
        self.span_lock.acquire();
        // SAFETY: span lock held ⇒ exclusive pool head-state access.
        let high_water = unsafe { (*self.spans.inner.get()).high_water };
        self.span_lock.release();
        for idx in 0..high_water {
            let slot = self.spans.slot_ptr(idx);
            // SAFETY: `idx < high_water ≤ cap`, and pool slots are never freed, so
            // the descriptor reference is valid for the process lifetime.
            let span = unsafe { &(*slot).desc };
            if span.arena() == arena && span.state() == SpanState::Active {
                all_revoked &= self.force_retire_span(span);
            }
        }
        // Step 2 (large): free every live large allocation of this arena.
        // `free_arena` frees at the large-allocator level (below the engine's
        // counters), so account the discarded bytes as freed here to keep the
        // global `live_bytes = allocated - freed` accurate (§8.6/§36.17) — the
        // arena's own `used` is zeroed by the reset/destroy completion.
        // SAFETY: the arena is quiesced (its objects are not concurrently freed),
        // the reset/destroy precondition. Route to the arena's own hooked large
        // backing if it has one (W10).
        let (_, large_bytes, large_revoked) =
            unsafe { self.large_backing(arena).free_arena(arena) };
        if large_bytes > 0 {
            self.freed_bytes
                .fetch_add(large_bytes as u64, Ordering::Relaxed);
        }
        all_revoked && large_revoked
    }

    /// Record a NUMA binding failure for `arena` (§15.5, W9-7): surfaced in
    /// [`ArenaStats::numa_bind_failures`]. The placement layer (plan 04 W13)
    /// calls this when it cannot honour an arena's NUMA policy; the failure is
    /// never fatal, only visible.
    pub fn record_numa_bind_failure(&self, arena: ArenaId) {
        self.arenas.record_numa_bind_failure(arena);
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

    /// Whether both shared back-ends, **every per-arena hooked backing**, and the
    /// arena registry are well-formed (W4-5 + B.5 oracle; debug/tests).
    pub fn check_invariants(&self) -> bool {
        let mut ok = self.span_extents.check_invariants()
            && self.large.check_invariants()
            && self.arenas.check_invariants();
        if self.hooks.count.load(Ordering::Acquire) > 0 {
            self.hooks.lock.acquire();
            for i in 0..MAX_HOOK_BACKENDS {
                // SAFETY: registry lock held; per-element borrow of slot `i` only.
                if let Some(b) = unsafe { (*self.hook_slot(i)).as_ref() } {
                    ok = ok && b.span_extents.check_invariants() && b.large.check_invariants();
                }
            }
            self.hooks.lock.release();
        }
        ok
    }

    /// Take a statistics snapshot (§31.1; the W8 DoD "state exposes stats and
    /// reconciles" surface). See [`AllocatorStats`] for the field semantics
    /// and consistency model.
    pub fn stats(&self) -> AllocatorStats {
        let allocated = self.allocated_bytes.load(Ordering::Relaxed);
        let freed = self.freed_bytes.load(Ordering::Relaxed);
        // Σ over classes: central-resident objects × object size. Bin reads
        // are lock-free approximations, exact when quiescent (§31.1).
        let mut central_free_bytes = 0u64;
        for (i, row) in SIZE_CLASSES.iter().enumerate() {
            if let Some(bin) = self.central.bin(SizeClassId::new(i)) {
                central_free_bytes += bin.total_central_free() * row.size as u64;
            }
        }
        // Aggregate the shared backend with every per-arena hooked region (W10), so
        // the §20.1 physical-state breakdown and the live-large count reflect *all*
        // backends — not just the shared one (an arena's own region is still this
        // allocator's memory). A no-op when no hooked arena exists.
        let mut span_backend = self.span_extents.state_bytes();
        let mut large_backend = self.large.state_bytes();
        let mut live_large = self.large.live_count() as u64;
        // Cumulative hook failures: the persistent total from torn-down backings
        // plus every live backing's current counts (W10 observability).
        let mut hf = self.retired_hooks.snapshot();
        if self.hooks.count.load(Ordering::Acquire) > 0 {
            self.hooks.lock.acquire();
            for i in 0..MAX_HOOK_BACKENDS {
                // SAFETY: registry lock held; per-element borrow of slot `i` only.
                if let Some(b) = unsafe { (*self.hook_slot(i)).as_ref() } {
                    span_backend = span_backend.add(b.span_extents.state_bytes());
                    large_backend = large_backend.add(b.large.state_bytes());
                    live_large += b.large.live_count() as u64;
                    let s = b.hook_failure_stats();
                    hf.commit += s.commit;
                    hf.release += s.release;
                    hf.split += s.split;
                    hf.merge += s.merge;
                }
            }
            self.hooks.lock.release();
        }
        AllocatorStats {
            // Saturating: `freed` can transiently exceed `allocated` only in
            // a torn relaxed read during concurrent ops; never report a wrap.
            live_bytes: allocated.saturating_sub(freed),
            allocated_bytes_total: allocated,
            freed_bytes_total: freed,
            central_free_bytes,
            span_backend,
            large_backend,
            pagemap_metadata_bytes: self.pagemap.metadata_bytes() as u64,
            live_spans: self.live_span_count() as u64,
            live_large,
            live_arenas: self.arenas.live_count() as u64,
            numa_bind_failures: self.arenas.total_numa_bind_failures(),
            hook_failures: hf,
        }
    }

    /// A minimal allocator summary safe for a crash/signal handler (W16-6, §28.4):
    /// it reads **only** the process-lifetime cumulative-byte atomics (relaxed,
    /// **no lock**), so it cannot deadlock on a structure lock held by the faulting
    /// thread, and it allocates nothing. Object/state breakdowns (which take locks)
    /// are deliberately omitted — "full stats may be unavailable in crash context"
    /// (§28.4). Combine with [`crate::init::INIT_PHASE`] /
    /// [`crate::fork::background_enabled`] for the process-wide fields.
    pub fn crash_summary(&self) -> crate::init::CrashSummary {
        let allocated = self.allocated_bytes.load(Ordering::Relaxed);
        let freed = self.freed_bytes.load(Ordering::Relaxed);
        crate::init::CrashSummary {
            init_phase: crate::init::INIT_PHASE.current() as u8,
            allocated_bytes: allocated,
            freed_bytes: freed,
            // Saturating: a torn relaxed read during concurrent ops could make
            // `freed` transiently exceed `allocated`; never report a wrap.
            live_bytes: allocated.saturating_sub(freed),
            in_flight_ops: crate::fork::in_flight_operations(),
            background_enabled: crate::fork::background_enabled(),
        }
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
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::{Arc, Mutex};

        pub(super) struct HostProvider {
            owned: Mutex<Vec<(usize, Layout)>>,
            /// When set, `revoke_descendants` fails — modeling a seLe4n
            /// capability-revoke failure so the arena-destroy drain quarantines
            /// (§36.13), the partial-failure path POSIX cannot otherwise reach.
            fail_revoke: AtomicBool,
            /// Count of `revoke_descendants` calls — lets a test observe that the
            /// recycle discipline (§36.6) revokes on *every* retirement, including
            /// the normal empty-span path, where POSIX/SIM revoke is a silent
            /// no-op and so otherwise unobservable. Behind an `Arc` so a test can
            /// keep a handle after the provider is moved into the allocator.
            revokes: Arc<AtomicU64>,
        }

        impl HostProvider {
            pub(super) fn new() -> Self {
                Self {
                    owned: Mutex::new(Vec::new()),
                    fail_revoke: AtomicBool::new(false),
                    revokes: Arc::new(AtomicU64::new(0)),
                }
            }

            /// Make every `revoke_descendants` fail (fault injection for the
            /// arena-drain quarantine path).
            pub(super) fn fail_revoke(&self) {
                self.fail_revoke.store(true, Ordering::Relaxed);
            }

            /// A handle to the revoke counter — cloned *before* the provider is
            /// moved into the allocator so a test can read it afterward.
            pub(super) fn revokes_handle(&self) -> Arc<AtomicU64> {
                Arc::clone(&self.revokes)
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
            fn revoke_descendants(
                &self,
                _arena: ArenaId,
                _region: Region,
            ) -> Result<(), BackendError> {
                self.revokes.fetch_add(1, Ordering::Relaxed);
                if self.fail_revoke.load(Ordering::Relaxed) {
                    Err(BackendError::Unsupported)
                } else {
                    Ok(())
                }
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

    /// W15-3b: a medium/large in-place **shrink** across a page boundary keeps
    /// the same base pointer but returns the tail pages to the backend (§25.3,
    /// DD-1 "shrink, large tail → split + return tail"). The usable size drops to
    /// the page-rounded new size, the backend's `active` bytes drop by exactly the
    /// returned tail, the preserved prefix survives, the default arena's `used`
    /// and the global `live_bytes` both reconcile (§8.6/§36.17), and the engine
    /// stays well-formed. A shrink that does *not* cross a page boundary keeps the
    /// whole extent (no tail to give back).
    #[test]
    fn realloc_large_shrink_returns_the_tail_pages() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // A medium allocation (> SMALL_MAX) is extent-backed.
        let old = 80_000usize;
        let p = a.malloc(old);
        assert!(!p.is_null());
        let old_usable = a.usable_size(p).unwrap();
        assert_eq!(old_usable, align_up(old, PAGE_SIZE).unwrap());
        // Fill the whole usable size with a recognizable pattern.
        // SAFETY: `old_usable` writable bytes.
        unsafe {
            for i in 0..old_usable {
                p.add(i).write((i as u8) ^ 0x3c);
            }
        }
        let active_before = a.stats().large_backend.active;
        assert_eq!(active_before, old_usable, "the one live medium is Active");
        assert_eq!(a.stats().live_bytes, old_usable as u64);

        // Shrink to a smaller medium size that crosses a page boundary.
        let new = 40_000usize;
        let new_usable = align_up(new, PAGE_SIZE).unwrap();
        assert!(
            new_usable < old_usable,
            "the test must cross a page boundary"
        );
        let q = trealloc(&a, p, new, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(q, p, "in-place shrink keeps the base pointer");
        assert_eq!(
            a.usable_size(q).unwrap(),
            new_usable,
            "usable drops to the page-rounded new size"
        );

        // The tail pages went back to the backend: `active` dropped by exactly the
        // returned tail; the engine is well-formed and the accounting reconciles.
        let freed = old_usable - new_usable;
        assert_eq!(
            a.stats().large_backend.active,
            active_before - freed,
            "the tail's bytes are no longer Active (returned to the backend)"
        );
        assert_eq!(
            a.stats().live_bytes,
            new_usable as u64,
            "global live_bytes reconciles after the shrink (§8.6)"
        );
        assert_eq!(
            a.arena_stats(ArenaId::DEFAULT).unwrap().used,
            new_usable as u64,
            "the arena's quota is credited the freed tail (§36.17)"
        );
        assert_eq!(a.live_large_count(), 1, "still one live large allocation");
        assert!(a.check_invariants());

        // The preserved prefix survived the shrink (§25.4 content preservation).
        // SAFETY: `new_usable` readable bytes.
        unsafe {
            for i in 0..new_usable {
                assert_eq!(q.add(i).read(), (i as u8) ^ 0x3c, "content lost at {i}");
            }
        }

        // A shrink that stays within the same page count keeps the whole extent
        // (nothing to return): the base and usable size are unchanged.
        let s = trealloc(&a, q, new_usable - 1, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(s, q, "a sub-page shrink stays in place");
        assert_eq!(
            a.usable_size(s).unwrap(),
            new_usable,
            "a sub-page shrink keeps the extent whole"
        );
        assert_eq!(a.stats().live_bytes, new_usable as u64);

        // Free reconciles back to zero — the descriptor now frees only the prefix.
        assert_eq!(tfree(&a, s), FreeOutcome::Freed);
        assert_eq!(a.stats().live_bytes, 0);
        assert_eq!(a.live_large_count(), 0);
        let st = a.stats();
        assert_eq!(
            st.allocated_bytes_total, st.freed_bytes_total,
            "allocated == freed after the final free"
        );
        assert!(a.check_invariants());
    }

    /// W15-3b under an **explicit arena** (§25.4 arena preservation + §36.17
    /// exactness): a shrink credits the freed tail back to the allocation's
    /// *original* arena, never the default arena the realloc flags name, and the
    /// shrunk object can still be freed cleanly with the arena drained to zero.
    #[test]
    fn realloc_large_shrink_credits_the_owning_arena() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();

        // Allocate a medium object in the explicit arena.
        let p = a.allocate_in(id, 90_000, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());
        let old_usable = a.usable_size(p).unwrap();
        assert_eq!(a.arena_stats(id).unwrap().used, old_usable as u64);

        // Shrink across a page boundary (flags name NONE ⇒ the default arena; the
        // shrink must still credit `id`, the original arena, per §25.4).
        let q = trealloc(&a, p, 50_000, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(q, p);
        let new_usable = a.usable_size(q).unwrap();
        assert!(new_usable < old_usable);
        assert_eq!(
            a.arena_stats(id).unwrap().used,
            new_usable as u64,
            "the freed tail is credited to the original arena (§25.4/§36.17)"
        );
        assert_eq!(
            a.arena_stats(ArenaId::DEFAULT).unwrap().used,
            0,
            "the shrink did not migrate accounting to the default arena"
        );
        assert!(a.check_invariants());

        // Free drains the arena to zero.
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert_eq!(a.arena_stats(id).unwrap().used, 0);
        assert_eq!(a.stats().live_bytes, 0);
        assert!(a.check_invariants());
    }

    /// W15-3a: a medium/large in-place **grow** absorbs the address-adjacent free
    /// extent (no copy) — the base pointer is unchanged, the preserved prefix
    /// survives, the backend's `active` bytes grow by exactly the absorbed amount,
    /// the arena's `used` and the global `live_bytes` reconcile (§8.6/§36.17), and
    /// the grown region is writable. The symmetric counterpart to the shrink.
    #[test]
    fn realloc_large_grow_absorbs_adjacent_free_in_place() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // A medium allocation; the rest of the large region is free right after it.
        let old = 80_000usize;
        let p = a.malloc(old);
        assert!(!p.is_null());
        let old_usable = a.usable_size(p).unwrap();
        // SAFETY: `old_usable` writable bytes.
        unsafe {
            for i in 0..old_usable {
                p.add(i).write((i as u8) ^ 0x5e);
            }
        }
        let active_before = a.stats().large_backend.active;
        assert_eq!(active_before, old_usable, "one live medium is Active");
        assert_eq!(a.stats().live_bytes, old_usable as u64);

        // Grow to a larger medium size — the adjacent free remainder is absorbed.
        let new = 150_000usize;
        let new_usable = align_up(new, PAGE_SIZE).unwrap();
        assert!(
            new_usable > old_usable,
            "the test must cross a page boundary upward"
        );
        let q = trealloc(&a, p, new, MIN_ALIGN, RequestFlags::NONE);
        assert_eq!(
            q, p,
            "in-place grow keeps the base pointer (absorbed adjacent free)"
        );
        assert_eq!(
            a.usable_size(q).unwrap(),
            new_usable,
            "usable grew to the rounded new size"
        );

        // The absorbed bytes are now Active; the accounting reconciles.
        let grown = new_usable - old_usable;
        assert_eq!(
            a.stats().large_backend.active,
            active_before + grown,
            "the absorbed free extent's bytes are now Active"
        );
        assert_eq!(
            a.stats().live_bytes,
            new_usable as u64,
            "global live_bytes reconciles (§8.6)"
        );
        assert_eq!(
            a.arena_stats(ArenaId::DEFAULT).unwrap().used,
            new_usable as u64,
            "the arena is charged the growth (§36.17)"
        );
        assert_eq!(a.live_large_count(), 1, "still one live large allocation");
        assert!(a.check_invariants());

        // The old prefix survived (no copy), and the grown region is writable.
        // SAFETY: `new_usable` readable+writable bytes.
        unsafe {
            for i in 0..old_usable {
                assert_eq!(q.add(i).read(), (i as u8) ^ 0x5e, "content lost at {i}");
            }
            for i in old_usable..new_usable {
                q.add(i).write(0xcd);
            }
            assert_eq!(q.add(new_usable - 1).read(), 0xcd, "grown tail unwritable");
        }

        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert_eq!(a.stats().live_bytes, 0);
        assert_eq!(a.live_large_count(), 0);
        assert!(a.check_invariants());
    }

    /// W15 §26.2 (PR #19 review): a `TOPO_ZERO` realloc that grows a large
    /// allocation **in place** by absorbing an adjacent **dirty** free extent must
    /// return the newly exposed suffix zeroed — the in-place fast path must honor
    /// the zero hint exactly as the move path does, never handing back the
    /// recycled neighbour's bytes.
    #[test]
    fn realloc_inplace_grow_zeroes_the_exposed_tail_under_topo_zero() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // A: the allocation we will grow. B: carved immediately after A, filled with
        // a non-zero pattern, then freed so it is a **dirty** (retained, committed)
        // free extent — exactly the recycled memory the grow will absorb.
        let pa = a.malloc(80_000);
        let pb = a.malloc(80_000);
        assert!(!pa.is_null() && !pb.is_null());
        let old_usable = a.usable_size(pa).unwrap();
        // SAFETY: each has its full usable size writable; dirty B with 0xff.
        unsafe {
            ptr::write_bytes(pb, 0xff, a.usable_size(pb).unwrap());
        }
        assert_eq!(tfree(&a, pb), FreeOutcome::Freed); // B is now dirty free, A's neighbour

        // Grow A with TOPO_ZERO into B's reclaimed (dirty) pages.
        let new = 120_000usize;
        let new_usable = align_up(new, PAGE_SIZE).unwrap();
        assert!(new_usable > old_usable, "must cross a page boundary upward");
        let q = trealloc(&a, pa, new, MIN_ALIGN, RequestFlags::NONE.with_zero());
        assert_eq!(
            q, pa,
            "absorbed the adjacent dirty free extent in place (no move)"
        );
        assert_eq!(a.usable_size(q).unwrap(), new_usable);

        // The crux: the grown suffix `[old_usable, new_usable)` reads as zero — the
        // recycled 0xff bytes were scrubbed (without the fix it would be 0xff).
        // SAFETY: `new_usable` readable bytes at `q`.
        unsafe {
            for i in old_usable..new_usable {
                assert_eq!(
                    q.add(i).read(),
                    0,
                    "TOPO_ZERO grow leaked recycled byte at {i}"
                );
            }
        }
        assert!(a.check_invariants());
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    /// W15-3a safety: when the adjacent extent is **not** free (a second live
    /// allocation sits right after), the in-place grow declines and `realloc`
    /// falls to the always-correct move — content preserved, the neighbour
    /// untouched, accounting exact.
    #[test]
    fn realloc_large_grow_falls_to_move_when_blocked() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let pa = a.malloc(80_000);
        let pb = a.malloc(80_000); // carved immediately after `pa` ⇒ pa's neighbour is Active
        assert!(!pa.is_null() && !pb.is_null());
        // SAFETY: 200 writable bytes in each.
        unsafe {
            for i in 0..200u8 {
                pa.add(i as usize).write(i ^ 0x33);
            }
        }
        let live_before = a.stats().live_bytes;

        // Grow `pa`: its address-adjacent neighbour is `pb` (Active), so the in-place
        // grow declines and realloc moves.
        let q = trealloc(&a, pa, 150_000, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q.is_null());
        assert_ne!(q, pa, "no adjacent free ⇒ realloc moves");
        // The prefix survived the move.
        // SAFETY: q has ≥ 200 readable bytes carrying the pattern.
        unsafe {
            for i in 0..200u8 {
                assert_eq!(
                    q.add(i as usize).read(),
                    i ^ 0x33,
                    "content lost on move at {i}"
                );
            }
        }
        // Net live bytes = (new usable of q) - (old usable of pa) added to the prior total.
        let q_usable = a.usable_size(q).unwrap() as u64;
        assert_eq!(
            a.stats().live_bytes,
            live_before - align_up(80_000, PAGE_SIZE).unwrap() as u64 + q_usable
        );
        assert!(a.check_invariants());

        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert_eq!(tfree(&a, pb), FreeOutcome::Freed);
        assert_eq!(a.stats().live_bytes, 0);
        assert!(a.check_invariants());
    }

    /// W15-3a/b via the `xallocx` engine (`resize_in_place`): a medium/large
    /// allocation shrinks (returns the tail) and grows back (absorbs the adjacent
    /// free extent) **in place** — same base, prefix preserved, no move; a small
    /// object's fixed slot reports its usable size unchanged; a foreign pointer is
    /// `None`.
    #[test]
    fn resize_in_place_shrinks_grows_and_reports_small_unchanged() {
        let m = meta(64 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // Large: shrink in place, then grow back into the just-returned tail.
        let p = a.malloc(200_000);
        assert!(!p.is_null());
        let big = a.usable_size(p).unwrap();
        // SAFETY: `big` writable bytes.
        unsafe {
            for i in 0..200u8 {
                p.add(i as usize).write(i ^ 0x44);
            }
        }
        // SAFETY: `p` is a live owned allocation; not concurrently freed.
        let shrunk =
            unsafe { a.resize_in_place(p, 100_000, MIN_ALIGN, RequestFlags::NONE) }.unwrap();
        assert!(
            shrunk >= 100_000 && shrunk < big,
            "shrank in place to {shrunk} (was {big})"
        );
        assert_eq!(a.usable_size(p).unwrap(), shrunk);
        // SAFETY: as above.
        let grown = unsafe { a.resize_in_place(p, big, MIN_ALIGN, RequestFlags::NONE) }.unwrap();
        assert_eq!(grown, big, "grew back in place by absorbing the freed tail");
        assert_eq!(a.usable_size(p).unwrap(), big);
        // The prefix survived every in-place resize (no copy).
        // SAFETY: `big` readable bytes.
        unsafe {
            for i in 0..200u8 {
                assert_eq!(p.add(i as usize).read(), i ^ 0x44, "content lost at {i}");
            }
        }
        assert!(a.check_invariants());
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);

        // Small: a fixed slab slot cannot resize in place; report its usable size.
        let s = a.malloc(100);
        let su = a.usable_size(s).unwrap();
        // SAFETY: `s` is a live owned small allocation.
        let shrink = unsafe { a.resize_in_place(s, 50, MIN_ALIGN, RequestFlags::NONE) };
        assert_eq!(
            shrink,
            Some(su),
            "small shrink-request reports the unchanged slot size"
        );
        // SAFETY: `s` is a live owned small allocation.
        let grow = unsafe { a.resize_in_place(s, su + 1, MIN_ALIGN, RequestFlags::NONE) };
        assert_eq!(
            grow,
            Some(su),
            "small grow-request cannot resize in place (fixed slot)"
        );
        assert_eq!(tfree(&a, s), FreeOutcome::Freed);

        // A foreign pointer is not resizable: None (the xallocx deterministic invalid arg).
        let mut local = 0u64;
        // SAFETY: a never-owned probe pointer is the documented rejected input.
        let foreign = unsafe {
            a.resize_in_place(
                (&mut local as *mut u64).cast::<u8>(),
                8,
                MIN_ALIGN,
                RequestFlags::NONE,
            )
        };
        assert_eq!(foreign, None);
        assert!(a.check_invariants());
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

    /// W8 liveness probes: a freed small object awaiting reuse is not live —
    /// `owns`/`usable_size` say so, and `realloc` refuses it as a source —
    /// while `recognizes` still claims the address (it is allocator memory).
    #[test]
    fn freed_small_object_is_recognized_but_not_owned() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(64);
        assert!(a.owns(p));
        assert!(a.recognizes(p));
        assert_eq!(a.usable_size(p), Some(64));

        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        // Freed: no longer a live object…
        assert!(!a.owns(p), "freed object must not be owned");
        assert_eq!(a.usable_size(p), None, "freed object has no usable size");
        // …its bytes can no longer seed a realloc…
        assert!(
            trealloc(&a, p, 128, MIN_ALIGN, RequestFlags::NONE).is_null(),
            "realloc of a freed object must fail, not resurrect it"
        );
        // …but the address is still recognized as allocator-managed (§35.2
        // routing: it must never be handed to a foreign allocator).
        assert!(a.recognizes(p));

        // Foreign and metadata addresses split the same way.
        let mut local = 0u8;
        let foreign = &mut local as *mut u8;
        assert!(!a.owns(foreign));
        assert!(!a.recognizes(foreign));
        assert!(!a.recognizes(ptr::null_mut()));
        let meta_ptr = m.alloc(16, 8).unwrap().as_ptr();
        assert!(!a.owns(meta_ptr), "metadata is not an application object");
        assert!(a.recognizes(meta_ptr), "metadata is allocator-managed");
    }

    /// A freed *large* allocation behaves the same: its pagemap entries are
    /// retired, so every probe (and realloc) rejects it, while the span
    /// region keeps recognizing retained small pages.
    #[test]
    fn freed_large_allocation_is_fully_retired() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let p = a.malloc(SMALL_MAX + 1);
        assert!(a.owns(p) && a.recognizes(p));
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert!(!a.owns(p));
        assert_eq!(a.usable_size(p), None);
        assert!(trealloc(&a, p, 64, MIN_ALIGN, RequestFlags::NONE).is_null());
    }

    /// `MetaArena` owns its provider, so the borrow checker enforces the
    /// §27.5 lifetime story end to end: build a full allocator over one,
    /// exercise it, and drop everything in order.
    #[test]
    fn meta_arena_backs_a_full_allocator_lifecycle() {
        let m = MetaArena::reserve(HostProvider::new(), ArenaId::DEFAULT, 4 * 1024 * 1024)
            .expect("meta arena");
        assert_eq!(m.used(), 0);
        assert!(m.capacity() >= 4 * 1024 * 1024);
        let pm = PageMap::new();
        let a = Allocator::new(
            HostProvider::new(),
            HostProvider::new(),
            &m,
            &m,
            &pm,
            ArenaId::DEFAULT,
            AllocatorConfig::small(),
        )
        .expect("allocator");

        let p = a.malloc(100);
        assert!(!p.is_null());
        // The engine's metadata genuinely came from the arena.
        assert!(m.used() > 0, "pools and descriptors carve from the arena");
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert!(a.check_invariants());
        // Drop order (a, then pm, then m) is enforced by the borrows.
    }

    /// §8.6 reconciliation (the W8 stats DoD): the counters balance against
    /// ground truth at every quiescent point of an alloc/free cycle.
    #[test]
    fn stats_reconcile_through_an_alloc_free_cycle() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let sc = size_class(64, 1).unwrap();
        let per_span = objects_per_slab(sc) as u64;

        let s0 = a.stats();
        assert_eq!(s0.live_bytes, 0);
        assert_eq!(s0.allocated_bytes_total, 0);
        assert_eq!(s0.central_free_bytes, 0);
        assert_eq!(s0.live_spans, 0);

        // Allocate one span's worth of 64-byte objects plus one medium.
        let mut ptrs = Vec::new();
        for _ in 0..per_span {
            ptrs.push(a.malloc(64));
        }
        let big = a.malloc(SMALL_MAX + 1);
        let big_usable = a.usable_size(big).unwrap() as u64;

        let s1 = a.stats();
        assert_eq!(s1.allocated_bytes_total, per_span * 64 + big_usable);
        assert_eq!(s1.live_bytes, per_span * 64 + big_usable);
        assert_eq!(s1.freed_bytes_total, 0);
        assert_eq!(s1.central_free_bytes, 0, "span fully drained");
        assert_eq!(s1.live_spans, 1);
        assert_eq!(s1.live_large, 1);
        assert!(s1.span_backend.active > 0, "span backing is active");

        // Free half the small objects: they become central-free bytes.
        let half = per_span / 2;
        for p in ptrs.drain(..half as usize) {
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        let s2 = a.stats();
        assert_eq!(s2.freed_bytes_total, half * 64);
        assert_eq!(s2.live_bytes, (per_span - half) * 64 + big_usable);
        assert_eq!(s2.central_free_bytes, half * 64);

        // Free everything: live returns to zero and the identities hold.
        for p in ptrs.drain(..) {
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        assert_eq!(tfree(&a, big), FreeOutcome::Freed);
        let s3 = a.stats();
        assert_eq!(s3.live_bytes, 0);
        assert_eq!(s3.allocated_bytes_total, s3.freed_bytes_total);
        // The emptied span is cached (1 per bin), all its objects central-free.
        assert_eq!(s3.central_free_bytes, per_span * 64);
        assert_eq!(s3.live_spans, 1);
        assert_eq!(s3.live_large, 0);
        // Realloc accounts through its legs: a move adds then frees.
        let q = a.malloc(100);
        let q2 = trealloc(&a, q, 5000, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q2.is_null());
        let s4 = a.stats();
        assert_eq!(
            s4.live_bytes,
            a.usable_size(q2).unwrap() as u64,
            "after a realloc move only the new object is live"
        );
        assert_eq!(tfree(&a, q2), FreeOutcome::Freed);
        assert_eq!(a.stats().live_bytes, 0);
    }

    /// PR #11 review (P1): the `nallocx` oracle must equal the usable size a
    /// real allocation gets — across small, medium, large, **and the
    /// over-aligned requests where classify's span-rounding diverges from the
    /// carve's size-rounding** (align > PAGE_SIZE was the reported gap).
    #[test]
    fn predicted_usable_size_matches_real_allocations() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        for (size, align) in [
            (1usize, 1usize),
            (0, 16),
            (100, 16),
            (4096, 16),
            (SMALL_MAX, 16),
            (SMALL_MAX + 1, 16),
            (100, 4096),          // over-aligned, sub-page
            (1, 65_536),          // align 4× PAGE_SIZE: the reported case
            (100, 262_144),       // align ≫ size
            (200_000, 65_536),    // medium with large alignment
            (HUGE_THRESHOLD, 16), // large family
        ] {
            let predicted = predicted_usable_size(size, align, RequestFlags::NONE);
            let p = a.allocate(size, align, RequestFlags::NONE);
            assert!(!p.is_null(), "allocate({size}, {align})");
            assert_eq!(
                predicted,
                a.usable_size(p),
                "prediction diverged for (size={size}, align={align})"
            );
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        // Prediction fails exactly where allocation would: invalid alignment
        // and rounding overflow.
        assert_eq!(predicted_usable_size(64, 24, RequestFlags::NONE), None);
        assert_eq!(
            predicted_usable_size(usize::MAX - 5, 16, RequestFlags::NONE),
            None
        );
    }

    // --- W9: multi-arena data path -------------------------------------------

    use crate::arena::{ArenaError, ArenaPolicy, ArenaState, CapRights, Delegation};

    /// Reset/destroy test helper: the test owns every pointer into `arena` and
    /// never touches them concurrently, satisfying the §22.5 quiescence contract.
    fn treset<P: TopoBackingProvider>(
        a: &Allocator<'_, P>,
        arena: ArenaId,
    ) -> Result<Generation, ArenaError> {
        // SAFETY: single-threaded test; the arena's pointers are not used after.
        unsafe { a.arena_reset(arena) }
    }
    fn tdestroy<P: TopoBackingProvider>(
        a: &Allocator<'_, P>,
        arena: ArenaId,
    ) -> Result<Generation, ArenaError> {
        // SAFETY: as `treset`.
        unsafe { a.arena_destroy(arena) }
    }

    #[test]
    fn explicit_arena_allocation_accounts_per_arena() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let id = a
            .arena_create(&ArenaPolicy::explicit().with_name("scratch"))
            .expect("create");
        assert_ne!(id, ArenaId::DEFAULT);
        assert_eq!(a.arena_stats(id).unwrap().used, 0);

        // Allocate from the explicit arena (a small object + a large one).
        let p = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());
        let big = a.allocate_in(id, SMALL_MAX + 1, MIN_ALIGN, RequestFlags::NONE);
        assert!(!big.is_null());
        let big_usable = a.usable_size(big).unwrap();

        // The arena's accounting reflects exactly its live bytes; the default
        // arena is untouched (§22.7 isolation, §36.17 exact accounting).
        assert_eq!(a.arena_stats(id).unwrap().used, 64 + big_usable as u64);
        assert_eq!(a.arena_stats(ArenaId::DEFAULT).unwrap().used, 0);

        // Free returns the bytes to the *owning* arena, not the default.
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert_eq!(tfree(&a, big), FreeOutcome::Freed);
        assert_eq!(a.arena_stats(id).unwrap().used, 0);
        assert!(a.check_invariants());
    }

    #[test]
    fn arena_quota_is_enforced_on_the_live_path() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // A quota of exactly two 64-byte objects.
        let id = a
            .arena_create(&ArenaPolicy::explicit().with_quota(128))
            .expect("create");
        let p1 = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        let p2 = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p1.is_null() && !p2.is_null());
        // The third exceeds the quota: a clean null, no over-charge.
        let p3 = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(
            p3.is_null(),
            "quota must cap the live allocation path (§36.4)"
        );
        assert_eq!(a.arena_stats(id).unwrap().used, 128);
        // Freeing one frees the budget for another.
        assert_eq!(tfree(&a, p1), FreeOutcome::Freed);
        let p4 = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p4.is_null(), "freed budget is reusable");
        assert_eq!(tfree(&a, p2), FreeOutcome::Freed);
        assert_eq!(tfree(&a, p4), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn allocation_from_an_unregistered_or_draining_arena_is_null() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        // An id that was never created is not allocatable.
        assert!(a
            .allocate_in(ArenaId(7), 64, MIN_ALIGN, RequestFlags::NONE)
            .is_null());
        // A draining arena rejects new allocations (§36.13). Drive it to
        // Draining via the table without finishing, then probe the gate.
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        a.arenas().begin_destroy(id).unwrap();
        assert_eq!(a.arena_stats(id).unwrap().state, ArenaState::Draining);
        assert!(a
            .allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE)
            .is_null());
    }

    #[test]
    fn arena_reset_discards_only_its_own_allocations() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let rg0 = a.arena_stats(id).unwrap().reset_generation;

        // A default-arena allocation we will verify survives the reset.
        let keep = a.malloc(128);
        assert!(!keep.is_null());
        // SAFETY: 128 usable bytes.
        unsafe { ptr::write_bytes(keep, 0x5a, 128) };

        // Several allocations in the explicit arena (small spans + a large).
        let mut owned = Vec::new();
        for _ in 0..50 {
            let p = a.allocate_in(id, 200, MIN_ALIGN, RequestFlags::NONE);
            assert!(!p.is_null());
            owned.push(p);
        }
        let big = a.allocate_in(id, SMALL_MAX + 1, MIN_ALIGN, RequestFlags::NONE);
        assert!(!big.is_null());
        assert!(a.owns(big) && a.owns(owned[0]));
        assert!(a.arena_stats(id).unwrap().used > 0);

        // Reset: the arena's allocations are discarded and its backing returned;
        // the default arena's object is untouched (§22.7 isolation).
        let rg1 = treset(&a, id).expect("reset");
        assert!(
            a.arenas().is_active(id),
            "reset returns the arena to Active"
        );
        assert_ne!(rg1, rg0, "reset bumps the reset generation (§22.5)");
        assert_eq!(
            a.arena_stats(id).unwrap().used,
            0,
            "B.5: no live arena bytes"
        );
        for p in &owned {
            assert!(!a.owns(*p), "reset invalidated the arena's small objects");
        }
        assert!(!a.owns(big), "reset invalidated the arena's large object");

        // The default arena's allocation survived with its contents.
        assert!(a.owns(keep), "reset must not touch another arena (§22.7)");
        // SAFETY: `keep` is still live.
        unsafe {
            assert_eq!(keep.read(), 0x5a);
            assert_eq!(keep.add(127).read(), 0x5a);
        }
        // The arena is reusable after reset.
        let q = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q.is_null());
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert_eq!(tfree(&a, keep), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn arena_destroy_retires_the_id_and_invalidates_its_pointers() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let keep = a.malloc(64);
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let p = a.allocate_in(id, 100, MIN_ALIGN, RequestFlags::NONE);
        let big = a.allocate_in(id, HUGE_THRESHOLD, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null() && !big.is_null());

        let g1 = tdestroy(&a, id).expect("destroy");
        // The arena is gone: not registered, not active, generation bumped.
        assert_eq!(a.arena_stats(id).unwrap().state, ArenaState::Destroyed);
        assert!(!a.arenas().is_registered(id));
        assert!(g1.0 > 0);
        // Its pointers are invalid (pagemap entries retired) — no longer owned
        // or even recognized as allocator memory (§22.6 / §36.13).
        assert!(!a.owns(p) && !a.recognizes(p));
        assert!(!a.owns(big) && !a.recognizes(big));
        // Allocation from the destroyed id fails until it is recreated.
        assert!(a
            .allocate_in(id, 16, MIN_ALIGN, RequestFlags::NONE)
            .is_null());
        // The default arena is untouched.
        assert!(a.owns(keep));
        assert_eq!(tfree(&a, keep), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn default_arena_cannot_be_reset_or_destroyed() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        assert_eq!(treset(&a, ArenaId::DEFAULT), Err(ArenaError::IsDefault));
        assert_eq!(tdestroy(&a, ArenaId::DEFAULT), Err(ArenaError::IsDefault));
        // The default arena is unaffected and still serves allocations.
        let p = a.malloc(64);
        assert!(!p.is_null());
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
    }

    #[test]
    fn realloc_preserves_the_owning_arena() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let p = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());
        // SAFETY: 64 usable bytes.
        unsafe {
            for i in 0..64 {
                p.add(i).write(i as u8);
            }
        }
        // A cross-class grow forces a move. The new object must stay in arena
        // `id`, not migrate to the default arena the NONE flags name (§25.4).
        let q = trealloc(&a, p, 4096, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q.is_null());
        assert_ne!(q, p);
        // SAFETY: 4096 usable bytes; the prefix survived the move.
        unsafe {
            for i in 0..64 {
                assert_eq!(q.add(i).read(), i as u8);
            }
        }
        let q_usable = a.usable_size(q).unwrap() as u64;
        assert_eq!(
            a.arena_stats(id).unwrap().used,
            q_usable,
            "the moved object stays charged to its original arena"
        );
        assert_eq!(
            a.arena_stats(ArenaId::DEFAULT).unwrap().used,
            0,
            "the move did not migrate the object to the default arena"
        );
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn per_arena_used_reconciles_with_global_live_bytes() {
        // §8.6/§36.17: the sum of every arena's `used` equals the engine's global
        // `live_bytes` — at every quiescent point, including across a reset that
        // *discards* (rather than frees) an arena's live objects.
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();

        let sum_used = |a: &Allocator<'_, HostProvider>| -> u64 {
            a.arena_stats(ArenaId::DEFAULT).unwrap().used + a.arena_stats(id).unwrap().used
        };

        // Mix default-arena and explicit-arena allocations (small + large).
        let d1 = a.malloc(64);
        let d2 = a.malloc(SMALL_MAX + 1);
        let mut owned = Vec::new();
        for _ in 0..30 {
            owned.push(a.allocate_in(id, 300, MIN_ALIGN, RequestFlags::NONE));
        }
        let e_big = a.allocate_in(id, HUGE_THRESHOLD, MIN_ALIGN, RequestFlags::NONE);
        assert!(!d1.is_null() && !d2.is_null() && !e_big.is_null());
        assert_eq!(
            sum_used(&a),
            a.stats().live_bytes,
            "Σ arena.used == live_bytes after mixed allocation"
        );

        // Free one default object: both sides drop together.
        assert_eq!(tfree(&a, d1), FreeOutcome::Freed);
        assert_eq!(sum_used(&a), a.stats().live_bytes, "after a free");

        // Reset the explicit arena: its `used` → 0 and `live_bytes` drops by the
        // same amount (discarded objects are accounted as freed).
        let _ = treset(&a, id).expect("reset");
        assert_eq!(a.arena_stats(id).unwrap().used, 0);
        assert_eq!(
            sum_used(&a),
            a.stats().live_bytes,
            "Σ arena.used == live_bytes after a reset discards the arena"
        );
        // Only the default arena's surviving object remains live.
        assert_eq!(
            a.stats().live_bytes,
            a.arena_stats(ArenaId::DEFAULT).unwrap().used
        );

        assert_eq!(tfree(&a, d2), FreeOutcome::Freed);
        assert_eq!(a.stats().live_bytes, 0);
        assert_eq!(sum_used(&a), 0);
        assert!(a.check_invariants());
    }

    #[test]
    fn arena_reset_drains_an_empty_cached_span() {
        // An arena whose objects were all freed leaves an *empty-cached* span
        // (state Active, in the central bin's empty cache, not the partial list).
        // Reset must still find and reclaim it (the `remove_empty` drain branch),
        // not just live/partial spans — otherwise that span would leak.
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let sc = size_class(64, 1).unwrap();
        let per_span = objects_per_slab(sc) as usize;

        // Fill exactly one span, then free every object → the span empties and is
        // cached for reuse (not deactivated).
        let ptrs: Vec<_> = (0..per_span)
            .map(|_| a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE))
            .collect();
        assert!(ptrs.iter().all(|p| !p.is_null()));
        for p in &ptrs {
            assert_eq!(tfree(&a, *p), FreeOutcome::Freed);
        }
        assert_eq!(a.arena_stats(id).unwrap().used, 0, "all freed");
        assert!(
            a.live_span_count() >= 1,
            "the emptied span is cached, not gone"
        );

        // Reset drains the empty-cached span; afterwards the arena holds nothing
        // and the engine reconciles.
        let _ = treset(&a, id).expect("reset");
        assert_eq!(a.arena_stats(id).unwrap().used, 0);
        assert_eq!(
            a.arena_stats(ArenaId::DEFAULT).unwrap().used + a.arena_stats(id).unwrap().used,
            a.stats().live_bytes
        );
        assert!(a.check_invariants());
        // The arena is reusable.
        let q = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!q.is_null());
        assert_eq!(tfree(&a, q), FreeOutcome::Freed);
    }

    #[test]
    fn arena_destroy_quarantines_on_a_revoke_failure() {
        // §36.13 partial-failure path (W9-6e): when the backend cannot revoke an
        // arena's backing capabilities, destroy must land the arena in
        // ErrorQuarantined — never DESTROYED — while still retiring every object.
        // POSIX revoke never fails, so we inject the failure with a provider whose
        // `revoke_descendants` returns Err (the seLe4n revoke-failure shape).
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let span_p = HostProvider::new();
        span_p.fail_revoke();
        let large_p = HostProvider::new();
        large_p.fail_revoke();
        let a = Allocator::new(
            span_p,
            large_p,
            &m,
            &m,
            &pm,
            ArenaId::DEFAULT,
            AllocatorConfig::small(),
        )
        .expect("allocator");

        let keep = a.malloc(64); // a default-arena object that must survive
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let p = a.allocate_in(id, 200, MIN_ALIGN, RequestFlags::NONE);
        let big = a.allocate_in(id, HUGE_THRESHOLD, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null() && !big.is_null());

        // Destroy: the drain retires every object but every revoke fails, so the
        // arena is quarantined, not destroyed.
        let outcome = tdestroy(&a, id);
        assert!(outcome.is_err(), "a revoke failure must not report success");
        assert_eq!(
            a.arena_stats(id).unwrap().state,
            ArenaState::ErrorQuarantined,
            "§36.13: partial failure ⇒ ERROR_QUARANTINED, never DESTROYED"
        );
        // Every object was still retired (gone), so no live bytes remain and the
        // §8.6/§36.17 reconciliation holds — only backing leaked.
        assert_eq!(
            a.arena_stats(id).unwrap().used,
            0,
            "objects retired on drain"
        );
        assert!(
            !a.owns(p) && !a.owns(big),
            "the arena's objects are invalid"
        );
        // The default arena is untouched and still serves.
        assert!(a.owns(keep));
        let sum = a.arena_stats(ArenaId::DEFAULT).unwrap().used + a.arena_stats(id).unwrap().used;
        assert_eq!(
            sum,
            a.stats().live_bytes,
            "Σ used == live_bytes after quarantine"
        );
        assert_eq!(tfree(&a, keep), FreeOutcome::Freed);
        // A quarantined arena can never be finalized as destroyed.
        // SAFETY: the arena is quarantined with no live objects (the drain
        // retired them all); the call is rejected before touching anything.
        let outcome = unsafe { a.arena_destroy(id) };
        assert_eq!(outcome, Err(ArenaError::IllegalTransition));
    }

    #[test]
    fn normal_span_retirement_revokes_backing_before_cross_arena_recycle() {
        // §36.6/§36.13 recycle discipline on the *normal* path (PR #13 review):
        // when an empty span's backing leaves the arena-tagged empty-span cache
        // (DD-4, capacity 1/bin) and returns to the single shared `span_extents`
        // pool, a *later* `create_span` may hand it to a different arena — so the
        // owning arena's capability descendants must be revoked first, exactly as
        // the reset/destroy drain does. POSIX/SIM revoke is a silent no-op, so a
        // counting provider is the only way to observe the discipline holds.
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let span_p = HostProvider::new();
        let revokes = span_p.revokes_handle();
        let a = Allocator::new(
            span_p,
            HostProvider::new(),
            &m,
            &m,
            &pm,
            ArenaId::DEFAULT,
            AllocatorConfig::small(),
        )
        .expect("allocator");

        // The 8192-byte class holds 2 objects per slab, so 16 allocations create
        // 8 distinct spans of one bin. Freeing them all empties every span: one
        // stays in the empty-span cache (DD-4), the other seven retire to the
        // shared pool — and each of those must revoke its backing first.
        let before = revokes.load(Ordering::Relaxed);
        let mut ptrs = Vec::new();
        for _ in 0..16 {
            let p = a.allocate_in(ArenaId::DEFAULT, 8192, MIN_ALIGN, RequestFlags::NONE);
            assert!(!p.is_null());
            ptrs.push(p);
        }
        for p in ptrs {
            assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        }
        let after = revokes.load(Ordering::Relaxed);
        assert!(
            after > before,
            "normal empty-span retirement must revoke its backing before recycle \
             (revoke calls: {before} -> {after})"
        );
        assert!(a.check_invariants());
    }

    #[test]
    fn concurrent_allocation_across_distinct_arenas_is_isolated() {
        // Multiple threads each hammer their *own* explicit arena concurrently;
        // objects never cross arenas, per-arena accounting stays exact, and the
        // engine ends well-formed (the multi-arena hot path under contention).
        let m = meta(32 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let arenas: Vec<ArenaId> = (0..4)
            .map(|_| a.arena_create(&ArenaPolicy::explicit()).unwrap())
            .collect();
        let a = &a;
        let arenas = &arenas;

        std::thread::scope(|s| {
            for (t, &arena) in arenas.iter().enumerate() {
                s.spawn(move || {
                    let mut live = Vec::new();
                    for i in 0..300usize {
                        let size = 16 + ((i * 41 + t * 97) % 3000);
                        let p = a.allocate_in(arena, size, MIN_ALIGN, RequestFlags::NONE);
                        assert!(!p.is_null());
                        // SAFETY: `size` usable bytes; stamp the arena index.
                        unsafe { ptr::write_bytes(p, t as u8, size) };
                        live.push((p, size));
                        if i % 3 == 0 {
                            let (q, qs) = live.swap_remove((i * 7) % live.len());
                            // SAFETY: still-live, owned by this thread; verify the
                            // stamp survived (no cross-arena bleed) then free.
                            unsafe {
                                assert_eq!(q.read(), t as u8, "cross-arena corruption");
                                assert_eq!(q.add(qs - 1).read(), t as u8);
                            }
                            assert_eq!(tfree(a, q), FreeOutcome::Freed);
                        }
                    }
                    for (p, _) in live {
                        assert_eq!(tfree(a, p), FreeOutcome::Freed);
                    }
                });
            }
        });

        // Every arena drained back to zero; the engine is well-formed.
        for &arena in arenas {
            assert_eq!(a.arena_stats(arena).unwrap().used, 0);
        }
        assert_eq!(a.stats().live_bytes, 0);
        assert!(a.check_invariants());
    }

    #[test]
    fn default_arena_allocations_survive_a_concurrent_arena_destroy() {
        // §36.13 "emergency allocations MUST NOT depend on an arena being
        // destroyed": the default arena is undestroyable, so it remains the
        // always-available fallback while an explicit arena is torn down.
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let victim = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let _ = a.allocate_in(victim, 256, MIN_ALIGN, RequestFlags::NONE);

        // A default-arena ("emergency") allocation made before the destroy.
        let e0 = a.malloc(64);
        assert!(!e0.is_null());
        tdestroy(&a, victim).expect("destroy");
        // The default arena still serves after the destroy, and the pre-destroy
        // object is untouched (it never depended on the destroyed arena).
        assert!(a.owns(e0));
        let e1 = a.malloc(64);
        assert!(!e1.is_null());
        assert_eq!(tfree(&a, e0), FreeOutcome::Freed);
        assert_eq!(tfree(&a, e1), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn arena_configure_changes_policy_through_the_engine() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);
        let id = a.arena_create(&ArenaPolicy::explicit()).unwrap();
        let before = a.arena_stats(id).unwrap();
        let cfg = crate::arena::ArenaConfig::from_stats(&before)
            .with_numa(crate::arena::NumaPolicy::Interleave);
        a.arena_configure(id, &cfg).unwrap();
        assert_eq!(
            a.arena_stats(id).unwrap().numa,
            crate::arena::NumaPolicy::Interleave
        );
        // Allocation still works after reconfiguration.
        let p = a.allocate_in(id, 64, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
    }

    #[test]
    fn delegated_arena_allocates_within_its_attenuated_authority() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        let parent = a
            .arena_create(&ArenaPolicy::explicit().with_quota(4096))
            .unwrap();
        let pstats = a.arena_stats(parent).unwrap();
        // Delegate a child that can allocate/free but not destroy, with a capped
        // quota — attenuation only (§36.4).
        let child = a
            .arena_delegate(
                parent,
                &Delegation::inheriting(&pstats, 256, "child")
                    .with_rights(CapRights::ALLOC.union(CapRights::FREE)),
            )
            .expect("delegate");
        // The child allocates within its 256-byte quota; beyond it is refused.
        let p = a.allocate_in(child, 200, MIN_ALIGN, RequestFlags::NONE);
        assert!(!p.is_null());
        assert!(a
            .allocate_in(child, 200, MIN_ALIGN, RequestFlags::NONE)
            .is_null());
        assert_eq!(tfree(&a, p), FreeOutcome::Freed);
        assert!(a.check_invariants());
    }

    #[test]
    fn small_place_class_applies_learned_hint_to_unhinted_requests() {
        // W14 small-path learn → place loop: after a confident cold profile is published for
        // a small bucket, an *unhinted* request of that size class is tagged with the learned
        // class — while an explicitly-hinted request keeps its own class (explicit wins).
        use crate::placement::{PlaceClass, SiteProfileTable};
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let a = small_allocator(&m, &pm);

        // Pick a real small size class and its bucket.
        let req = classify(64, MIN_ALIGN, 0).expect("classify");
        let sc = match req.kind {
            RequestKind::Small { sc, .. } => sc,
            other => panic!("expected small, got {other:?}"),
        };
        let bucket = sc.index() as u16;

        // Baseline: nothing learned ⇒ the unhinted small request maps to one class (the
        // default pool); record it so we can prove the learned hint *changes* it.
        let baseline = a.small_place_class(&req);

        // Teach a confident cold + short profile for that bucket and publish it.
        let mut table: SiteProfileTable<64> = SiteProfileTable::new();
        let mut clk = 0u64;
        for _ in 0..crate::placement::CONFIDENT_SAMPLES {
            table.record_alloc(crate::StackId(7), bucket, 64, 0 /* cold */, clk);
            clk += 1;
            table.record_free(crate::StackId(7), 5 /* short */, 64, clk);
            clk += 1;
        }
        a.publish_learned_hints(&table);

        // The unhinted request now adopts the learned class (Cold ⇒ class tag 1).
        let learned = a.small_place_class(&req);
        assert_eq!(
            learned,
            PlaceClass::Cold.as_u8(),
            "unhinted small request adopts the learned cold class (was {baseline})"
        );

        // An explicit hot hint wins over the learned cold hint (explicit always overrides).
        let hot_raw = RequestFlags::NONE.with_hotness(255).raw();
        let hot_req = classify(64, MIN_ALIGN, hot_raw).expect("classify hot");
        assert_eq!(
            a.small_place_class(&hot_req),
            PlaceClass::Hot.as_u8(),
            "explicit hot hint overrides the learned cold hint"
        );

        // Clearing the learned hints reverts the unhinted request to its baseline class.
        a.learned_hints().reset();
        assert_eq!(
            a.small_place_class(&req),
            baseline,
            "clear reverts to default"
        );
    }
}

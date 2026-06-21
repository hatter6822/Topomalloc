// SPDX-License-Identifier: MIT
//! TopoMalloc core — classifier, size classes, the backing-provider seam, and
//! the M0 walking-skeleton allocator.
//!
//! This crate is `#![no_std]`-capable (the hot path must run without `std`,
//! D1/§6). The test harness pulls in `std` automatically; production builds do
//! not. It contains no OS/kernel calls: all backing memory comes through the
//! [`TopoBackingProvider`] seam, so POSIX and seLe4n are co-equal behind it.
//!
//! See [`SPEC.md`](../planning/SPEC.md) §9 (size classes), §3/§36.6 (the seam),
//! and §33.7 (trace grammar); and plan 01 (W0) for how this fits the M0
//! walking skeleton.
#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod allocator;
pub mod arena;
pub mod backend;
pub mod bootstrap;
pub mod budget;
pub mod cache_ops;
pub mod central;
pub mod classify;
pub mod compat;
pub mod cpu_cache;
pub mod debug;
pub mod error;
pub mod extent;
pub mod fe;
pub mod flags;
pub mod fork;
pub mod generated;
pub mod harden;
pub mod hooks;
pub mod huge;
pub mod ids;
pub mod init;
pub mod large;
pub mod lock;
pub mod node_router;
pub mod overflow;
pub mod pagemap;
pub mod pinned;
pub mod placement;
pub mod profile;
pub mod ptr_class;
pub mod release;
pub mod sampling;
pub mod size_class;
pub mod skeleton;
pub mod slab;
pub mod span;
pub mod thread_cache;
pub mod topology;
pub mod trace;
pub mod transfer_cache;

/// The TopoMalloc version string, reported by stats JSON (Appendix D) and the
/// control namespace `topo.version` (W0-13). Sourced from the crate version so
/// the ABI series and the reported version can never drift.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Convenience re-exports for the common surface.
pub use allocator::{
    predicted_usable_size, Allocator, AllocatorConfig, AllocatorStats, FreeOutcome, MetaArena,
    MAX_HOOK_BACKENDS,
};
pub use arena::{
    ArenaConfig, ArenaError, ArenaPolicy, ArenaState, ArenaStats, ArenaTable, CapRights,
    DecayConfig, Delegation, HookFailureStats, NumaPolicy, RevocationPhase, ARENA_NAME_LEN,
    MAX_ARENAS, QUOTA_UNLIMITED,
};
pub use backend::{
    CachePolicy, FrameCap, MappedRange, ProviderState, ProviderStateMachine, Region, Rights,
    TopoBackingProvider, VWindow,
};
pub use bootstrap::{Bootstrap, BumpArena, MetadataAlloc};
pub use central::{
    Batch, CentralCache, CentralError, InsertResult, RemoveResult, ANY_PLACE_CLASS, MAX_BATCH_LEN,
};
pub use classify::{classify, Request, RequestKind};
pub use debug::{check_b2_cache, runtime_checks_enabled, Group as BCheckGroup};
pub use compat::{set_zero_size_policy, zero_size_policy, ZeroSizePolicy};
pub use error::BackendError;
pub use extent::{
    Extent, ExtentBacking, ExtentError, ExtentFlags, ExtentId, ExtentManager, ExtentMap,
    ExtentNotify, ExtentRef, ExtentState, Fit, HugeRange, NoNotify, NoRegionCache, RegionCacheHook,
    RetainPolicy, StateBytes,
};
pub use flags::{Hints, HugepagePolicy, Lifetime, RequestFlags};
pub use harden::{
    guard_pages_enabled, junk_fill_enabled, quarantine_enabled, secure_scrub_enabled, EvictBatch,
    GuardSampler, Offer, Quarantine, QuarantineEntry, QuarantinePolicy, ALLOC_PATTERN,
    FREE_PATTERN, QUARANTINE_CAP,
};
pub use hooks::{ExtentHooks, HookProvider};
pub use huge::{
    classify_bin, FreeReport, Hotness, HugeBin, HugeConfig, HugeError, HugePageBackend,
    HugePageFiller, HugeRun, HugeStats, PlaceHints, Placement, Subrelease, HUGEPAGE_SIZE,
    PAGES_PER_HUGEPAGE,
};
pub use ids::{ArenaId, Generation, Label, LargeId, NodeId, SizeClassId, SpanId};
pub use large::{LargeAllocator, LargeBacking, LargeConfig};
pub use node_router::{NodeRouter, NodeRouterStats, RouterControl};
pub use pagemap::{PageEntry, PageMap, PagemapError};
pub use placement::{
    bucket_index, AllocationSiteProfile, LearnedHints, LifetimeClass, LifetimeHistogram,
    PlaceClass, PlacementStats, SiteProfileTable, SizeClassDist, StackId, CONFIDENT_SAMPLES,
    DEFAULT_MIN_CONFIDENCE_BP, NUM_BUCKETS, SIZE_DIST_K,
};
pub use profile::{active_profile, debug_checks_enabled};
pub use ptr_class::{
    classify_ptr, validate_free, AnyMetadataRegion, FreeTarget, InvalidFree, MetadataRegion,
    NoMetadata, PointerClass,
};
pub use release::{
    demand_reserve, LatencyClass, PressureMode, PressureThresholds, ReleaseController,
    ReleaseInputs, ReleasePlan, ReleaseStats,
};
pub use sampling::{
    Rng, SampleBloom, SampleConfig, SampledObjects, SampledRecord, Sampler, StackBuf, BLOOM_WORDS,
    MAX_STACK_FRAMES,
};
pub use size_class::{size_class, usable_size, SizeClassRow};
pub use skeleton::{SkeletonAllocator, MIN_ALIGN};
pub use slab::SlabLayout;
pub use span::{
    ClassifyGeometry, FreeBitmap, GenGuard, LargeDescriptor, LargeState, NonCentralResidency,
    SpanDescriptor, SpanFlags, SpanGuard, SpanState, INLINE_BITS, MAX_BITMAP_WORDS,
};
pub use topology::{
    NodePressure, RebalanceMove, RebalanceTier, Rebalancer, Topology, TopologyBuilder,
    DISTANCE_LOCAL, DISTANCE_REMOTE, MAX_NODES,
};

// W16 concurrency re-exports (plan 05): the ranked lock hierarchy + checker,
// the fork coordinator, init phases, re-entrancy guard, and crash summary.
pub use fork::{
    background_enabled, fork_in_progress, in_flight_operations, maintenance_guard, operation_guard,
    postfork_child, postfork_parent, prefork, probe_and_set_sharding, set_background_enabled,
    set_sharded, OperationGuard,
};
pub use init::{CrashSummary, InitPhase, PhaseTracker, INIT_PHASE};
pub use lock::{held_lock_count, reset_lock_checker, LockRank, RankedGuard, RankedLock};

// W6 cache layer re-exports (plan 05).
pub use budget::{CacheBudget, SlotStats};
pub use cache_ops::{flush, flush_idle_cpu, refill, refill_with_retry, FlushResult, RefillResult};
pub use cpu_cache::CpuCache;
pub use fe::{CoreId, FeOutcome};
pub use pinned::{CoreProvider, FixedCore};
#[cfg(any(test, feature = "std"))]
pub use thread_cache::init_thread_cache;
pub use thread_cache::{with_thread_cache, FlushHookFn, ThreadCache};
pub use transfer_cache::TransferCache;

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_nonempty() {
        assert!(!super::VERSION.is_empty());
    }
}

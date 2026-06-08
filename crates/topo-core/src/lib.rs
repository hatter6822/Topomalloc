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

pub mod backend;
pub mod bootstrap;
pub mod budget;
pub mod cache_ops;
pub mod central;
pub mod classify;
pub mod cpu_cache;
pub mod error;
pub mod extent;
pub mod fe;
pub mod flags;
pub mod generated;
pub mod ids;
pub mod large;
pub mod overflow;
pub mod pagemap;
pub mod profile;
pub mod ptr_class;
pub mod size_class;
pub mod skeleton;
pub mod slab;
pub mod span;
pub mod thread_cache;
pub mod trace;
pub mod transfer_cache;

/// The TopoMalloc version string, reported by stats JSON (Appendix D) and the
/// control namespace `topo.version` (W0-13). Sourced from the crate version so
/// the ABI series and the reported version can never drift.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Convenience re-exports for the common surface.
pub use backend::{
    CachePolicy, FrameCap, MappedRange, ProviderState, ProviderStateMachine, Region, Rights,
    TopoBackingProvider, VWindow,
};
pub use bootstrap::{Bootstrap, BumpArena, MetadataAlloc};
pub use central::{Batch, CentralCache, CentralError, InsertResult, RemoveResult, MAX_BATCH_LEN};
pub use classify::{classify, Request, RequestKind};
pub use error::BackendError;
pub use extent::{
    Extent, ExtentError, ExtentFlags, ExtentId, ExtentManager, ExtentMap, ExtentRef, ExtentState,
    Fit, HugeRange, NoRegionCache, RegionCacheHook, RetainPolicy, StateBytes,
};
pub use flags::{Hints, HugepagePolicy, Lifetime, RequestFlags};
pub use ids::{ArenaId, Generation, Label, LargeId, NodeId, SizeClassId, SpanId};
pub use large::{LargeAllocator, LargeConfig};
pub use pagemap::{PageEntry, PageMap, PagemapError};
pub use profile::{active_profile, debug_checks_enabled};
pub use ptr_class::{
    classify_ptr, validate_free, AnyMetadataRegion, FreeTarget, InvalidFree, MetadataRegion,
    NoMetadata, PointerClass,
};
pub use size_class::{size_class, usable_size, SizeClassRow};
pub use skeleton::{SkeletonAllocator, MIN_ALIGN};
pub use slab::SlabLayout;
pub use span::{
    ClassifyGeometry, FreeBitmap, GenGuard, LargeDescriptor, LargeState, NonCentralResidency,
    SpanDescriptor, SpanFlags, SpanGuard, SpanState, INLINE_BITS, MAX_BITMAP_WORDS,
};

// W6 cache layer re-exports (plan 05).
pub use budget::{CacheBudget, SlotStats};
pub use cache_ops::{flush, flush_idle_cpu, refill, refill_with_retry, FlushResult, RefillResult};
pub use cpu_cache::CpuCache;
pub use fe::{CoreId, FeOutcome};
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

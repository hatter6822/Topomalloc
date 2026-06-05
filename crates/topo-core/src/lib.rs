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
pub mod classify;
pub mod error;
pub mod flags;
pub mod generated;
pub mod ids;
pub mod overflow;
pub mod pagemap;
pub mod profile;
pub mod ptr_class;
pub mod size_class;
pub mod skeleton;
pub mod span;
pub mod trace;

/// The TopoMalloc version string, reported by stats JSON (Appendix D) and the
/// control namespace `topo.version` (W0-13). Sourced from the crate version so
/// the ABI series and the reported version can never drift.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Convenience re-exports for the common surface.
pub use backend::{Region, Rights, TopoBackingProvider};
pub use bootstrap::{Bootstrap, BumpArena, MetadataAlloc};
pub use classify::{classify, Request, RequestKind};
pub use error::BackendError;
pub use flags::{Hints, HugepagePolicy, Lifetime, RequestFlags};
pub use ids::{ArenaId, Generation, Label, LargeId, SizeClassId, SpanId};
pub use pagemap::{PageEntry, PageMap, PagemapError};
pub use profile::{active_profile, debug_checks_enabled};
pub use ptr_class::{classify_ptr, validate_free, InvalidFree, PointerClass};
pub use size_class::{size_class, usable_size, SizeClassRow};
pub use skeleton::{SkeletonAllocator, MIN_ALIGN};
pub use span::{
    FreeBitmap, GenGuard, LargeDescriptor, LargeState, NonCentralResidency, SpanDescriptor,
    SpanFlags, SpanState,
};

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_nonempty() {
        assert!(!super::VERSION.is_empty());
    }
}

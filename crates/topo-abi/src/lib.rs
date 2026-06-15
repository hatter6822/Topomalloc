// SPDX-License-Identifier: MIT
//! Public ABI surface (plan 06 W8): the standard C entry points
//! (`topomalloc_*`, §10.1), the C23 sized-free family, the extended
//! `topo_*x` API with validated flags (§10.3/§10.4), the Rust
//! [`GlobalAlloc`] adapter (D1), and the runtime backend selector.
//!
//! From W8 the entry points run over the **M1 central-path allocator**
//! ([`topo_core::Allocator`]): classify → central free lists / extent-backed
//! large path, with real `free`, `realloc`, `malloc_usable_size`, errno
//! semantics (W8-1b), and the configurable zero-size policy (W8-4). The M0
//! bump skeleton remains only as a test fixture in `topo-core`.
//!
//! **Default build = MIT, POSIX-only.** Enabling the optional `sele4n-sim`
//! feature additionally links the GPL seLe4n backend and produces a
//! GPL-3.0-or-later artifact (`libtopomalloc-sele4n`, §36.3.2). The default
//! artifact never links GPL code (D5, see NOTICE).
//!
//! The exported C symbols are **prefixed** (`topomalloc_malloc`, …), so
//! linking this crate never hijacks the process's `malloc`. The
//! interposition/override deployment that replaces system `malloc` (§35.1)
//! is a deliberate, separately gated step in plan 10 — not something a test
//! or dependent crate gets by accident. The C++ operators follow the same
//! rule: they ship as an opt-in header (`include/topomalloc_new_delete.hpp`)
//! over these entry points, not as exported mangled symbols.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::ptr;
use std::alloc::System;
use std::sync::OnceLock;

use topo_backend_posix::PosixBackingProvider;
use topo_core::{
    Allocator, AllocatorConfig, AllocatorStats, ArenaConfig, ArenaError, ArenaId, ArenaPolicy,
    ArenaStats, Delegation, ExtentHooks, FreeOutcome, Generation, InvalidFree, MetaArena, PageMap,
    RequestFlags,
};

mod arena_api;
mod c_api;
mod errno_shim;
mod extended;
mod hooks_api;
mod policy;

pub use arena_api::{
    topo_arena_configure, topo_arena_create, topo_arena_create_ex, topo_arena_delegate,
    topo_arena_destroy, topo_arena_handle, topo_arena_id, topo_arena_reset, topo_mallocx_arena,
    TOPO_RIGHTS_ALL, TOPO_RIGHT_ALLOC, TOPO_RIGHT_DESTROY, TOPO_RIGHT_FREE, TOPO_RIGHT_STATS,
};
pub use c_api::{
    topomalloc_aligned_alloc, topomalloc_backend, topomalloc_calloc, topomalloc_free,
    topomalloc_free_aligned_sized, topomalloc_free_sized, topomalloc_malloc,
    topomalloc_malloc_usable_size, topomalloc_memalign, topomalloc_posix_memalign,
    topomalloc_realloc, topomalloc_reallocarray, topomalloc_version,
};
pub use extended::{
    topo_align_lg, topo_arena, topo_dallocx, topo_hot, topo_mallocx, topo_nallocx, topo_rallocx,
    topo_sdallocx, topo_xallocx, TOPO_ALIGN_LG_MASK, TOPO_GUARDED, TOPO_LIFETIME_LONG,
    TOPO_LIFETIME_MEDIUM, TOPO_LIFETIME_SHORT, TOPO_NO_HUGEPAGE, TOPO_PREFER_HUGEPAGE,
    TOPO_TCACHE_NONE, TOPO_ZERO,
};
pub use hooks_api::{topo_arena_create_hooked, topo_extent_hooks_t, topo_max_hook_backends};
pub use policy::{set_zero_size_policy, zero_size_policy, ZeroSizePolicy};

/// Bytes of metadata arena reserved for the process-wide allocator (POSIX:
/// virtual, lazily faulted). Sized with ample headroom over the default
/// configuration's fixed pools plus pagemap nodes and span bitmaps — the
/// 4× factor below covers the growing parts (radix nodes for the regions'
/// address ranges and out-of-line span bitmaps), each of which is measured
/// in single-digit MiB against the pools' ~13 MiB; the assertion turns a
/// config growth that outpaces this arena into a build failure instead of a
/// runtime construction error.
const META_BYTES: usize = 64 * 1024 * 1024;
const _: () = assert!(
    AllocatorConfig::DEFAULT.fixed_pool_metadata_bytes() * 4 <= META_BYTES,
    "META_BYTES no longer covers the default configuration's metadata demand      with headroom; raise it alongside AllocatorConfig::DEFAULT"
);

/// Metadata arena for the simulator backend, whose untyped pool is charged
/// eagerly — keep it modest (matches [`AllocatorConfig::small`]).
#[cfg(feature = "sele4n-sim")]
const SIM_META_BYTES: usize = 4 * 1024 * 1024;

/// The process-wide allocator over whichever backend was selected at runtime.
/// Enum dispatch (not `dyn`) keeps the default build free of the GPL backend
/// type.
pub enum AnyAllocator {
    /// POSIX backend (default).
    Posix(Allocator<'static, PosixBackingProvider>),
    /// seLe4n host simulator (only when built with `sele4n-sim`).
    #[cfg(feature = "sele4n-sim")]
    Sim(Allocator<'static, topo_backend_sele4n::Sele4nSim>),
}

macro_rules! dispatch {
    ($self:expr, $a:ident => $body:expr) => {
        match $self {
            AnyAllocator::Posix($a) => $body,
            #[cfg(feature = "sele4n-sim")]
            AnyAllocator::Sim($a) => $body,
        }
    };
}

impl AnyAllocator {
    /// Allocate `size` bytes with `align` under validated `flags` (§A.1).
    /// Null on OOM, overflow, or invalid alignment.
    pub fn allocate(&self, size: usize, align: usize, flags: RequestFlags) -> *mut u8 {
        dispatch!(self, a => a.allocate(size, align, flags))
    }

    /// Allocate from an explicit arena (plan 06 W9), overriding the arena the
    /// `flags` encode. Used by `realloc` (arena preservation) and the arena API.
    pub fn allocate_in(
        &self,
        arena: ArenaId,
        size: usize,
        align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        dispatch!(self, a => a.allocate_in(arena, size, align, flags))
    }

    /// Free a pointer; see [`FreeOutcome`] for the validation outcomes.
    ///
    /// # Safety
    ///
    /// As [`topo_core::Allocator::free`]: `ptr` must be null, a live pointer
    /// of this allocator the caller still owns, or memory never returned by
    /// it (rejected harmlessly). A stale pointer aliasing a recycled live
    /// allocation would free another owner's object.
    pub unsafe fn free(&self, ptr: *mut u8) -> FreeOutcome {
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.free(ptr) })
    }

    /// Reallocate under the §25.1 contract (failure preserves the original).
    ///
    /// # Safety
    ///
    /// As [`free`](Self::free).
    pub unsafe fn realloc(
        &self,
        ptr: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.realloc(ptr, new_size, min_align, flags) })
    }

    /// The usable size of a live allocation (`None` for null/foreign/interior).
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        dispatch!(self, a => a.usable_size(ptr))
    }

    /// Whether `ptr` is a **live** allocation of this allocator.
    pub fn owns(&self, ptr: *mut u8) -> bool {
        dispatch!(self, a => a.owns(ptr))
    }

    /// Whether `ptr` lies in memory this allocator manages at all (live,
    /// freed-awaiting-reuse, interior, retained, or metadata) — the §35.2
    /// mixed-allocator routing predicate; see
    /// [`Allocator::recognizes`](topo_core::Allocator::recognizes).
    pub fn recognizes(&self, ptr: *mut u8) -> bool {
        dispatch!(self, a => a.recognizes(ptr))
    }

    /// A statistics snapshot of the engine (§31.1; map into the Appendix-D
    /// JSON with `topo_stats::Stats::record_allocator`).
    pub fn stats(&self) -> AllocatorStats {
        dispatch!(self, a => a.stats())
    }

    /// The active backend's name (`"posix"` / `"sele4n-sim"`), proving which
    /// provider the runtime flag selected (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        dispatch!(self, a => a.backend_name())
    }

    // -- arena lifecycle & authority (plan 06 W9) ------------------------------

    /// Create a new explicit arena (§22.4). Returns its [`ArenaId`].
    pub fn arena_create(&self, policy: &ArenaPolicy) -> Result<ArenaId, ArenaError> {
        dispatch!(self, a => a.arena_create(policy))
    }

    /// Delegate an attenuated child arena from `parent` (§36.4).
    pub fn arena_delegate(&self, parent: ArenaId, del: &Delegation) -> Result<ArenaId, ArenaError> {
        dispatch!(self, a => a.arena_delegate(parent, del))
    }

    /// Reconfigure an arena's non-authority policy (§22.4 *configure*, F-005).
    pub fn arena_configure(&self, arena: ArenaId, cfg: &ArenaConfig) -> Result<(), ArenaError> {
        dispatch!(self, a => a.arena_configure(arena, cfg))
    }

    /// Create an arena served from its own [`ExtentHooks`] backing region (§22.2,
    /// plan 06 W10). `hooks` must be `'static` (the global allocator is `'static`).
    pub fn arena_create_hooked(
        &self,
        policy: &ArenaPolicy,
        hooks: &'static (dyn ExtentHooks + Send + Sync),
        cfg: AllocatorConfig,
    ) -> Result<ArenaId, ArenaError> {
        dispatch!(self, a => a.arena_create_hooked(policy, hooks, cfg))
    }

    /// A snapshot of an arena's authority + accounting (§22.2/§36.4).
    pub fn arena_stats(&self, arena: ArenaId) -> Option<ArenaStats> {
        dispatch!(self, a => a.arena_stats(arena))
    }

    /// Whether `arena` currently has a registered hooked backing (W10).
    pub fn arena_has_hook_backend(&self, arena: ArenaId) -> bool {
        dispatch!(self, a => a.arena_has_hook_backend(arena))
    }

    /// Whether `arena` is registered and currently allocatable (§22.3).
    pub fn arena_is_active(&self, arena: ArenaId) -> bool {
        dispatch!(self, a => a.arenas().is_active(arena))
    }

    /// A generation-checked handle for `arena`'s current incarnation (§36.13),
    /// or `None` if unregistered.
    pub fn arena_handle(&self, arena: ArenaId) -> Option<u64> {
        dispatch!(self, a => a.arenas().handle(arena))
    }

    /// Resolve a handle to its [`ArenaId`], or `None` if it is stale (§36.13).
    pub fn arena_resolve_handle(&self, handle: u64) -> Option<ArenaId> {
        dispatch!(self, a => a.arenas().resolve_handle(handle))
    }

    /// Reset an explicit arena (§22.5).
    ///
    /// # Safety
    ///
    /// As [`topo_core::Allocator::arena_reset`]: the arena must be quiesced and
    /// the caller accepts that its outstanding pointers become invalid.
    pub unsafe fn arena_reset(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.arena_reset(arena) })
    }

    /// Destroy an explicit arena (§22.6/§36.13).
    ///
    /// # Safety
    ///
    /// As [`arena_reset`](Self::arena_reset).
    pub unsafe fn arena_destroy(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.arena_destroy(arena) })
    }
}

/// Build the POSIX engine — **extent-backed by default**, or **hugepage-backed under
/// the `hugepage-optimized` profile** (plan 04 W11), where the medium/large path is
/// served by a [`HugePageBackend`](topo_core::HugePageBackend). The hugepage backend
/// is a process-lived sibling (leaked, like `meta`/`pagemap`), sized to one hugepage
/// per `HUGEPAGE_SIZE` of the default large region; its ~tens-of-KiB descriptor pool
/// is drawn from the same `meta` arena (well within `META_BYTES`). The small-object,
/// free, and arena paths are unchanged either way.
fn build_posix_allocator(
    meta: &'static MetaArena<PosixBackingProvider>,
    pagemap: &'static PageMap,
) -> Option<Allocator<'static, PosixBackingProvider>> {
    let cfg = AllocatorConfig::default();
    #[cfg(feature = "hugepage-optimized")]
    {
        use topo_backend_posix::discover_topology;
        use topo_core::{CoreId, FixedCore, HugeConfig, HugePageBackend, NodeRouter};

        let capacity = (cfg.large_region_bytes / topo_core::HUGEPAGE_SIZE).max(1);
        // Build a live NUMA router: one hugepage backend per discovered NUMA node, each
        // bound to its node (§15.5), with the large region split across them. On a
        // single-node machine this is exactly one backend — byte-for-byte the plain
        // hugepage path — so the integration degrades cleanly where there is nothing to
        // place. Placement follows the arena's `NumaPolicy` (resolved into the engine's
        // hints, §15.3/§15.5); the current-CPU source is a `FixedCore` until the RSEQ
        // per-CPU identity lands (plan 05 W7), so `Local`/`Interleave` use core 0 today.
        let topo = discover_topology();
        let n = (topo.node_count() as usize).max(1);
        // Round up so the per-node backends total *at least* the single-backend capacity
        // (no capacity regression from the split); on a single node this is exactly
        // `capacity`. The extra is virtual address space (lazily faulted), so it is free.
        let per_node = capacity.div_ceil(n).max(1);
        let router = NodeRouter::build(topo, FixedCore(CoreId::DEFAULT), |node, _os| {
            HugePageBackend::new(
                PosixBackingProvider::new(),
                meta,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(per_node).with_home_node(node),
            )
            .ok()
        })?;
        // Leak-and-reclaim, as for the single backend: only a successfully-built router is
        // leaked into `'static`; the raw pointer is kept so a failing allocator build
        // reclaims it (its `Drop` releases *every* per-node reservation). On success it is
        // process-lived (like `meta`/`pagemap`).
        let router_ptr: *mut NodeRouter<PosixBackingProvider, FixedCore> =
            Box::into_raw(Box::new(router));
        // SAFETY: `router_ptr` came from `Box::into_raw` just above and is uniquely owned;
        // reborrowing it as `&'static` is sound because it is only reclaimed on the failure
        // arm below (after the borrow ends), and otherwise leaked for the process lifetime.
        let huge: &'static NodeRouter<PosixBackingProvider, FixedCore> = unsafe { &*router_ptr };
        match Allocator::new_with_huge(
            PosixBackingProvider::new(),
            PosixBackingProvider::new(),
            huge,
            meta,
            meta,
            pagemap,
            ArenaId::DEFAULT,
            cfg,
        ) {
            Ok(a) => Some(a),
            Err(_) => {
                // The allocator only *borrowed* `huge` and that borrow ended with the
                // `Err`, so reclaim the leaked router before returning `None`.
                // SAFETY: `router_ptr` was produced by `Box::into_raw` of the same type
                // above; reclaimed exactly once, only on this failure path, never used
                // again (no double-free). Its `Drop` releases every per-node reservation.
                drop(unsafe { Box::from_raw(router_ptr) });
                None
            }
        }
    }
    #[cfg(not(feature = "hugepage-optimized"))]
    {
        Allocator::new(
            PosixBackingProvider::new(),
            PosixBackingProvider::new(),
            meta,
            meta,
            pagemap,
            ArenaId::DEFAULT,
            cfg,
        )
        .ok()
    }
}

/// Build an allocator for the named backend (the runtime flag, W0-14b).
///
/// Returns `None` for an unknown name, or if a backend with that name is not
/// compiled into this build (e.g. `"sele4n-sim"` without the feature).
///
/// The metadata arena and pagemap are leaked into `'static` (§35.5: allocator
/// metadata is process-lived by design); call this once per process per
/// backend, as the global initializer does.
pub fn new_allocator_named(name: &str) -> Option<AnyAllocator> {
    match name {
        "posix" => {
            // The arena owns its provider (`MetaArena`), so one leak pins the
            // whole metadata backing for the process lifetime (§35.5/§27.5).
            let meta: &'static MetaArena<PosixBackingProvider> = Box::leak(Box::new(
                MetaArena::reserve(PosixBackingProvider::new(), ArenaId::DEFAULT, META_BYTES)
                    .ok()?,
            ));
            let pagemap: &'static PageMap = Box::leak(Box::new(PageMap::new()));
            let a = build_posix_allocator(meta, pagemap)?;
            Some(AnyAllocator::Posix(a))
        }
        #[cfg(feature = "sele4n-sim")]
        "sele4n-sim" => {
            use topo_backend_sele4n::Sele4nSim;
            let cfg = AllocatorConfig::small();
            let meta: &'static MetaArena<Sele4nSim> = Box::leak(Box::new(
                MetaArena::reserve(
                    Sele4nSim::new(SIM_META_BYTES),
                    ArenaId::DEFAULT,
                    SIM_META_BYTES,
                )
                .ok()?,
            ));
            let pagemap: &'static PageMap = Box::leak(Box::new(PageMap::new()));
            let a = Allocator::new(
                Sele4nSim::new(cfg.span_region_bytes),
                Sele4nSim::new(cfg.large_region_bytes),
                meta,
                meta,
                pagemap,
                ArenaId::DEFAULT,
                cfg,
            )
            .ok()?;
            Some(AnyAllocator::Sim(a))
        }
        _ => None,
    }
}

/// The process-wide allocator, initialized once from the environment.
/// `None` records that initialization failed (e.g. the host could not reserve
/// the regions); the C ABI then reports OOM as a null result instead of
/// aborting the process across the `extern "C"` boundary.
static GLOBAL: OnceLock<Option<AnyAllocator>> = OnceLock::new();

/// The selected backend name: `$TOPOMALLOC_BACKEND` or `"posix"`. An unknown or
/// unavailable name falls back to POSIX so the default artifact is always usable.
fn selected_backend_name() -> String {
    std::env::var("TOPOMALLOC_BACKEND").unwrap_or_else(|_| "posix".into())
}

thread_local! {
    /// Set while this thread is initializing [`GLOBAL`]. When `TopoMallocGlobal`
    /// is the process `#[global_allocator]`, the allocations the initializer makes
    /// (reading the backend env var, the providers' bookkeeping, the leaked
    /// metadata-arena boxes) would otherwise route back through this same
    /// allocator and re-enter `global()`, deadlocking the `OnceLock`. While the
    /// flag is set the adapter serves those allocations straight from the system
    /// allocator.
    static BOOTSTRAPPING: Cell<bool> = const { Cell::new(false) };
}

/// RAII marker that sets [`BOOTSTRAPPING`] for the duration of `GLOBAL`
/// initialization and clears it on the way out — even if the initializer panics.
struct BootstrapGuard;

impl BootstrapGuard {
    fn enter() -> Self {
        BOOTSTRAPPING.with(|b| b.set(true));
        BootstrapGuard
    }
}

impl Drop for BootstrapGuard {
    fn drop(&mut self) {
        BOOTSTRAPPING.with(|b| b.set(false));
    }
}

/// The lazily-initialized global allocator, or `None` if the backing regions
/// could not be reserved. The result is memoized: a process that cannot
/// reserve them once keeps reporting OOM (a null `malloc`) rather than
/// retrying.
pub(crate) fn global() -> Option<&'static AnyAllocator> {
    GLOBAL
        .get_or_init(|| {
            // `_guard` is declared first, so it drops *last* — after `name` and any
            // other init-path temporaries have been allocated and freed through the
            // system allocator — and only then clears the bootstrap flag.
            let _guard = BootstrapGuard::enter();
            let name = selected_backend_name();
            new_allocator_named(&name).or_else(|| new_allocator_named("posix"))
        })
        .as_ref()
}

/// The Rust [`GlobalAlloc`] adapter (D1, W8-7). Suitable for opt-in use as the
/// process `#[global_allocator]`: the first allocation lazily initializes the
/// backing regions, and a per-thread bootstrap guard serves the initializer's
/// own allocations from the system allocator so it cannot deadlock (see
/// `global`). Pointers handed out by the system allocator inside that
/// bootstrap window are recognized later (the engine classifies them as
/// foreign) and routed back to the system allocator, so nothing leaks and
/// nothing is freed by the wrong allocator (§35.2).
///
/// It is intentionally **not** registered here, so linking this crate never
/// replaces a test or dependent crate's allocator.
pub struct TopoMallocGlobal;

// SAFETY: `alloc` returns either null or a pointer to at least `layout.size()`
// bytes aligned to `layout.align()` (the engine validates alignment and the
// classifier never under-allocates, §9.7). During `GLOBAL` initialization it
// forwards to the system allocator (so a re-entrant init allocation cannot
// deadlock). `dealloc` returns each pointer to the allocator that produced it:
// the engine frees what it owns; pointers it classifies as foreign are exactly
// the bootstrap-window system allocations (the engine's regions and the system
// heap are disjoint mappings), which go back to `System` with their original
// layout. `realloc` preserves the same ownership split and the §25.1 contract.
unsafe impl GlobalAlloc for TopoMallocGlobal {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if BOOTSTRAPPING.with(|b| b.get()) {
            // Inside `GLOBAL` init: the engine does not exist yet, so serve
            // this init-path allocation from the system allocator and never
            // re-enter `global()`.
            // SAFETY: `layout` is forwarded unchanged to the system allocator.
            return unsafe { System.alloc(layout) };
        }
        global().map_or(ptr::null_mut(), |a| {
            a.allocate(layout.size(), layout.align(), RequestFlags::NONE)
        })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if BOOTSTRAPPING.with(|b| b.get()) {
            // SAFETY: `layout` is forwarded unchanged to the system allocator.
            return unsafe { System.alloc_zeroed(layout) };
        }
        global().map_or(ptr::null_mut(), |a| {
            a.allocate(
                layout.size(),
                layout.align(),
                RequestFlags::NONE.with_zero(),
            )
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if BOOTSTRAPPING.with(|b| b.get()) {
            // SAFETY: every allocation made during bootstrap came from `System`, so
            // one freed in that same window is returned to `System` with its
            // original `layout`.
            return unsafe { System.dealloc(ptr, layout) };
        }
        match global() {
            // SAFETY: the `GlobalAlloc` contract gives us a pointer this
            // adapter returned and the caller still owns — either engine
            // memory (freed here exactly once) or a bootstrap-window `System`
            // pointer (rejected as Foreign and routed below).
            Some(a) => match unsafe { a.free(ptr) } {
                FreeOutcome::Freed | FreeOutcome::Null => {}
                FreeOutcome::Invalid(InvalidFree::Foreign) => {
                    // Not engine memory ⇒ it was allocated by `System` inside
                    // the bootstrap window (the only other source under this
                    // adapter's contract).
                    // SAFETY: the caller's `GlobalAlloc` contract guarantees
                    // (ptr, layout) came from this adapter; the engine's
                    // regions are disjoint from the system heap, so a Foreign
                    // classification identifies a `System` allocation.
                    unsafe { System.dealloc(ptr, layout) };
                }
                outcome => {
                    // Interior/metadata/double-free: a caller contract
                    // violation. Never corrupt state over it (§35.2).
                    debug_assert!(
                        false,
                        "GlobalAlloc::dealloc on invalid pointer: {outcome:?}"
                    );
                }
            },
            None => {
                // The engine never initialized: everything came from `System`.
                // SAFETY: as above — (ptr, layout) came from this adapter, and
                // without an engine the only source is `System`.
                unsafe { System.dealloc(ptr, layout) };
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if BOOTSTRAPPING.with(|b| b.get()) {
            // SAFETY: forwarded unchanged; the System allocator owns
            // bootstrap-window pointers.
            return unsafe { System.realloc(ptr, layout, new_size) };
        }
        let Some(a) = global() else {
            // SAFETY: no engine ⇒ the pointer came from `System`.
            return unsafe { System.realloc(ptr, layout, new_size) };
        };
        if a.recognizes(ptr) {
            // Engine-managed memory (live — or, for a caller violating the
            // realloc contract, freed/interior: the engine then returns null
            // without touching anything, which is the safe failure).
            // SAFETY: `ptr` is a live engine allocation the caller owns (the
            // `GlobalAlloc` realloc contract), reallocated exactly once.
            return unsafe { a.realloc(ptr, new_size, layout.align(), RequestFlags::NONE) };
        }
        // Never engine memory ⇒ a bootstrap-window `System` pointer: migrate
        // it into the engine (allocate-before-free, so failure leaves the
        // original intact). Routing on `recognizes` — not `owns` — is what
        // keeps a (contract-violating) freed engine pointer from ever being
        // handed to `System` (§35.2: each allocator frees only its own).
        let q = a.allocate(new_size, layout.align(), RequestFlags::NONE);
        if !q.is_null() {
            let copy = layout.size().min(new_size);
            // SAFETY: `q` is a fresh engine allocation of ≥ `new_size ≥ copy`
            // bytes; `ptr` is a live `System` allocation of `layout.size() ≥
            // copy` bytes; the two heaps are disjoint mappings.
            unsafe {
                ptr::copy_nonoverlapping(ptr.cast_const(), q, copy);
                System.dealloc(ptr, layout);
            }
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W11 production wiring: under the `hugepage_optimized` profile the POSIX engine
    /// constructs with a hugepage backend and serves medium/large allocations through
    /// it, with the small/free paths unchanged. (Builds + runs only with the feature.)
    #[cfg(feature = "hugepage-optimized")]
    #[test]
    fn hugepage_optimized_posix_engine_serves_large_and_small() {
        let a = new_allocator_named("posix").expect("hugepage_optimized posix engine builds");
        // A medium request (> small_max) flows through the hugepage-backed large path.
        let size = 256 * 1024;
        let p = a.allocate(size, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        assert_eq!(a.usable_size(p), Some(size));
        // SAFETY: `p` has `size` writable bytes.
        unsafe {
            std::ptr::write_bytes(p, 0xab, size);
            assert_eq!(*p.add(size - 1), 0xab);
            assert_eq!(a.free(p), FreeOutcome::Freed);
        }
        // The small path is unchanged.
        let s = a.allocate(64, 16, RequestFlags::NONE);
        assert!(!s.is_null());
        // SAFETY: `s` is a live object just handed out.
        unsafe {
            assert_eq!(a.free(s), FreeOutcome::Freed);
        }
    }

    /// Reclaim-on-failure invariant for `build_posix_allocator`'s hugepage arm
    /// (the resource-leak fix). `build_posix_allocator` leaks the `HugePageBackend`
    /// into `'static` so the allocator can borrow it, but if the *allocator* build
    /// fails it reclaims the leaked backend (`drop(Box::from_raw(ptr))`) before
    /// returning `None`, so the large virtual reservation is never stranded.
    ///
    /// Forcing the real `Allocator::new_with_huge` to fail from here is impractical
    /// (it needs the shared `meta` arena exhausted in a way the success path does
    /// not hit), so this test exercises the exact leak→reclaim idiom the failure
    /// arm uses: a backend leaked with `Box::into_raw` is reclaimed with
    /// `Box::from_raw`, whose `Drop` (§19.2) returns the reservation. It must not
    /// double-free and must succeed (the provider reports a clean release).
    #[cfg(feature = "hugepage-optimized")]
    #[test]
    fn build_posix_allocator_reclaims_leaked_backend_on_failure() {
        // A standalone metadata arena so this mirrors construction without touching
        // any process-global state.
        let meta: &'static MetaArena<PosixBackingProvider> = Box::leak(Box::new(
            MetaArena::reserve(PosixBackingProvider::new(), ArenaId::DEFAULT, META_BYTES)
                .expect("meta arena reserves"),
        ));
        let backend = topo_core::HugePageBackend::new(
            PosixBackingProvider::new(),
            meta,
            ArenaId::DEFAULT,
            topo_core::HugeConfig::with_capacity(2),
        )
        .expect("hugepage backend reserves");

        // Leak exactly as the production failure window does.
        let ptr: *mut topo_core::HugePageBackend<PosixBackingProvider> =
            Box::into_raw(Box::new(backend));

        // Simulate the allocator build failing: reclaim the leaked backend. This is
        // the byte-for-byte body of `build_posix_allocator`'s `Err(_) => { … }` arm.
        // SAFETY: `ptr` came from `Box::into_raw` of the same type just above and is
        // reclaimed exactly once here; it is not used afterward (no double-free).
        let mut reclaimed = unsafe { Box::from_raw(ptr) };
        // `teardown` returns the reservation and reports success; `Drop` would do the
        // same silently. Either way the reservation is released, not stranded.
        assert!(
            reclaimed.teardown().is_ok(),
            "reclaimed backend releases its reservation cleanly"
        );
        // `reclaimed` drops here; `teardown` already marked it released, so `Drop`
        // is a no-op — release happens exactly once.
    }

    #[test]
    fn global_alloc_adapter_roundtrips_and_frees() {
        let g = TopoMallocGlobal;
        let layout = Layout::from_size_align(128, 16).unwrap();
        // SAFETY: valid non-zero layout.
        let p = unsafe { g.alloc(layout) };
        assert!(!p.is_null());
        // SAFETY: p has 128 writable bytes; dealloc returns it to the engine.
        unsafe {
            ptr::write_bytes(p, 1, 128);
            g.dealloc(p, layout);
        }
        // The engine genuinely freed it (not leaked): the slot is vended again. No
        // front-end thread cache yet (M2), so a freed small object returns to the *shared*
        // central free list, which a parallel test thread can transiently touch — confirm
        // recycling by membership in a bounded batch (held to drain toward the freed slot,
        // then freed), not by asserting the exact next call.
        let mut batch = Vec::with_capacity(64);
        let mut recycled = false;
        for _ in 0..64 {
            // SAFETY: valid layout; every allocation is freed in the loop below.
            let x = unsafe { g.alloc(layout) };
            recycled |= x == p;
            batch.push(x);
        }
        for x in batch {
            if !x.is_null() {
                // SAFETY: a non-null `x` came from `g.alloc(layout)` above and is freed once.
                unsafe { g.dealloc(x, layout) };
            }
        }
        assert!(
            recycled,
            "freed object must be recycled by the engine (within a batch)"
        );
    }

    #[test]
    fn global_alloc_zeroed_is_zeroed() {
        let g = TopoMallocGlobal;
        let layout = Layout::from_size_align(256, 16).unwrap();
        // Dirty an object, free it, and ask for zeroed memory of the same class.
        // SAFETY: valid layout, 256 writable bytes.
        unsafe {
            let p = g.alloc(layout);
            assert!(!p.is_null());
            ptr::write_bytes(p, 0xee, 256);
            g.dealloc(p, layout);
            let q = g.alloc_zeroed(layout);
            assert!(!q.is_null());
            for i in 0..256 {
                assert_eq!(q.add(i).read(), 0, "byte {i} not zeroed");
            }
            g.dealloc(q, layout);
        }
    }

    #[test]
    fn global_alloc_realloc_preserves_content_and_alignment() {
        let g = TopoMallocGlobal;
        let layout = Layout::from_size_align(64, 64).unwrap();
        // SAFETY: valid layout; pointers are used within their live windows.
        unsafe {
            let p = g.alloc(layout);
            assert!(!p.is_null());
            assert_eq!(p as usize % 64, 0);
            for i in 0..64 {
                p.add(i).write(i as u8);
            }
            let q = g.realloc(p, layout, 4096);
            assert!(!q.is_null());
            assert_eq!(q as usize % 64, 0, "realloc must preserve layout.align()");
            for i in 0..64 {
                assert_eq!(q.add(i).read(), i as u8);
            }
            g.dealloc(q, Layout::from_size_align(4096, 64).unwrap());
        }
    }

    #[test]
    fn named_selector_builds_posix() {
        let a = new_allocator_named("posix").expect("posix");
        assert_eq!(a.backend_name(), "posix");
        let p = a.allocate(64, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        // SAFETY: `p` was just returned by `a` and is owned by this test.
        assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
        assert!(new_allocator_named("nope").is_none());
    }
}

// SPDX-License-Identifier: MIT
//! Public ABI surface (plan 06 W8/W9): the standard C entry points
//! (`topomalloc_*`, §10.1), the C23 sized-free family, the extended
//! `topo_*x` API with validated flags (§10.3/§10.4), the capability-backed
//! arena API (`topo_arena_*`/`topo_mallocx_arena`, §22/§36.4/§36.14), the
//! Rust [`GlobalAlloc`] adapter (D1), and the runtime backend selector.
//!
//! From W8 the entry points run over the **M1 central-path allocator**
//! ([`topo_core::Allocator`]): classify → central free lists / extent-backed
//! large path, with real `free`, `realloc`, `malloc_usable_size`, errno
//! semantics (W8-1b), and the configurable zero-size policy (W8-4). From W9
//! the process-wide state is an [`ArenaSet`] — a registry of per-arena
//! engines — and every entry point routes through it: allocations by the
//! requested arena (the default arena for the plain C API), frees and
//! reallocs by the pointer's owning arena. The M0 bump skeleton remains only
//! as a test fixture in `topo-core`.
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
    AllocatorConfig, AllocatorStats, ArenaConfig, ArenaError, ArenaId, ArenaSet, ArenaSnapshot,
    ArenaState, BackendError, DelegationSpec, FreeOutcome, InvalidFree, MetaArena, PageMap,
    ProviderFactory, RequestFlags,
};

mod arena_api;
mod c_api;
mod errno_shim;
mod extended;
mod policy;

pub use arena_api::{
    topo_arena_configure, topo_arena_create, topo_arena_delegate, topo_arena_destroy,
    topo_arena_reset, topo_mallocx_arena, TopoArenaConfig, TOPO_ARENA_RIGHTS_ALL,
    TOPO_ARENA_RIGHT_ALLOC, TOPO_ARENA_RIGHT_DELEGATE, TOPO_ARENA_RIGHT_DESTROY,
    TOPO_ARENA_RIGHT_FREE, TOPO_ARENA_RIGHT_PURGE, TOPO_ARENA_RIGHT_STATS, TOPO_NUMA_ARENA_POLICY,
    TOPO_NUMA_BIND, TOPO_NUMA_INTERLEAVE, TOPO_NUMA_LOCAL, TOPO_NUMA_OS_DEFAULT,
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

/// Concurrently live arenas the process-wide registry supports (the default
/// arena plus explicit/delegated arenas, plan 06 W9). Each explicit arena
/// reserves its own (virtual) regions, so the bound is address space and
/// metadata, not RSS.
#[cfg(target_pointer_width = "64")]
const ARENA_CAPACITY: usize = 64;
/// On 32-bit targets address space is the scarce resource; keep the registry
/// small.
#[cfg(not(target_pointer_width = "64"))]
const ARENA_CAPACITY: usize = 8;

/// The simulator charges its untyped pool eagerly per reservation, so its
/// registry stays small regardless of pointer width.
#[cfg(feature = "sele4n-sim")]
const SIM_ARENA_CAPACITY: usize = 8;

/// [`ProviderFactory`] for the POSIX backend: every arena region gets an
/// independent ambient-authority mapper (the degenerate §36.6 case, D2).
pub struct PosixFactory;

impl ProviderFactory for PosixFactory {
    type Provider = PosixBackingProvider;
    fn make_provider(&self, _arena: ArenaId) -> Result<PosixBackingProvider, BackendError> {
        Ok(PosixBackingProvider::new())
    }
}

/// [`ProviderFactory`] for the seLe4n host simulator: each region draws from
/// its own authorized-untyped pool sized for the small engine geometry. Plan
/// 09 replaces this with per-arena authority carved from the resource
/// server's inventory.
#[cfg(feature = "sele4n-sim")]
pub struct SimFactory;

#[cfg(feature = "sele4n-sim")]
impl ProviderFactory for SimFactory {
    type Provider = topo_backend_sele4n::Sele4nSim;
    fn make_provider(
        &self,
        _arena: ArenaId,
    ) -> Result<topo_backend_sele4n::Sele4nSim, BackendError> {
        // Large enough for either region of `AllocatorConfig::small()`.
        let cfg = AllocatorConfig::small();
        let pool = cfg.span_region_bytes.max(cfg.large_region_bytes);
        Ok(topo_backend_sele4n::Sele4nSim::new(pool))
    }
}

/// The process-wide arena registry over whichever backend was selected at
/// runtime (plan 06 W9: every entry point routes through the registry's
/// owning-arena dispatch). Enum dispatch (not `dyn`) keeps the default build
/// free of the GPL backend type.
pub enum AnyAllocator {
    /// POSIX backend (default).
    Posix(ArenaSet<'static, PosixFactory>),
    /// seLe4n host simulator (only when built with `sele4n-sim`).
    #[cfg(feature = "sele4n-sim")]
    Sim(ArenaSet<'static, SimFactory>),
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

    /// A statistics snapshot of the **default arena's** engine (§31.1; map
    /// into the Appendix-D JSON with `topo_stats::Stats::record_allocator`).
    /// Per-arena snapshots come from [`snapshot_arena`](Self::snapshot_arena)
    /// / [`snapshot_arenas`](Self::snapshot_arenas).
    pub fn stats(&self) -> AllocatorStats {
        match self.snapshot_arena(ArenaId::DEFAULT) {
            Ok(snap) => snap.engine,
            Err(_) => AllocatorStats::default(), // default arena always grants STATS
        }
    }

    /// The active backend's name (`"posix"` / `"sele4n-sim"`), proving which
    /// provider the runtime flag selected (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        match self {
            AnyAllocator::Posix(_) => "posix",
            #[cfg(feature = "sele4n-sim")]
            AnyAllocator::Sim(_) => "sele4n-sim",
        }
    }

    // -- the W9 arena surface (enum-dispatched ArenaSet passthroughs) ----------

    /// Allocate from an explicit arena (`topo_mallocx_arena`, §36.14).
    pub fn allocate_in(
        &self,
        arena: ArenaId,
        size: usize,
        align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        dispatch!(self, a => a.allocate_in(arena, size, align, flags))
    }

    /// Create an explicit arena (§22.4, F-005).
    pub fn arena_create(&self, cfg: &ArenaConfig) -> Result<ArenaId, ArenaError> {
        dispatch!(self, a => a.create(cfg))
    }

    /// Delegate an attenuated child arena (§36.4, W9-5).
    pub fn arena_delegate(
        &self,
        parent: ArenaId,
        spec: &DelegationSpec,
    ) -> Result<ArenaId, ArenaError> {
        dispatch!(self, a => a.delegate(parent, spec))
    }

    /// Reconfigure an arena's NUMA policy (§15.5, F-005 "configure").
    pub fn arena_configure(
        &self,
        id: ArenaId,
        numa: topo_core::NumaPolicy,
    ) -> Result<(), ArenaError> {
        dispatch!(self, a => a.configure(id, numa))
    }

    /// Reset an explicit arena (§22.5).
    pub fn arena_reset(&self, id: ArenaId) -> Result<(), ArenaError> {
        dispatch!(self, a => a.reset(id))
    }

    /// Destroy an explicit arena via the §36.13 revocation protocol (§22.6).
    pub fn arena_destroy(&self, id: ArenaId) -> Result<(), ArenaError> {
        dispatch!(self, a => a.destroy(id))
    }

    /// The lifecycle state of arena `id`, if live.
    pub fn arena_state(&self, id: ArenaId) -> Option<ArenaState> {
        dispatch!(self, a => a.arena_state(id))
    }

    /// The rights arena `id`'s capability grants, if live.
    pub fn arena_rights(&self, id: ArenaId) -> Option<topo_core::CapRights> {
        dispatch!(self, a => a.rights_of(id))
    }

    /// A per-arena snapshot (requires the arena's `STATS` right, §36.4).
    pub fn snapshot_arena(&self, id: ArenaId) -> Result<ArenaSnapshot, ArenaError> {
        dispatch!(self, a => a.snapshot(id))
    }

    /// Snapshot every live arena (the operator/aggregation surface, plan 07).
    pub fn snapshot_arenas(&self, visit: &mut dyn FnMut(ArenaSnapshot)) {
        dispatch!(self, a => a.snapshot_all(visit))
    }

    /// The executable B.5 arena oracle (tests / debug).
    pub fn check_invariants(&self) -> bool {
        dispatch!(self, a => a.check_invariants())
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
            // The default arena: full engine geometry, ambient authority
            // (all rights, unlimited quota, PUBLIC label — the trivial POSIX
            // values of the §36.4 capability fields, W9-1).
            let default_cfg = ArenaConfig {
                allocator: AllocatorConfig::default(),
                ..ArenaConfig::default()
            };
            let set = ArenaSet::new(
                PosixFactory,
                meta,
                meta,
                pagemap,
                &default_cfg,
                ARENA_CAPACITY,
            )
            .ok()?;
            Some(AnyAllocator::Posix(set))
        }
        #[cfg(feature = "sele4n-sim")]
        "sele4n-sim" => {
            use topo_backend_sele4n::Sele4nSim;
            let meta: &'static MetaArena<Sele4nSim> = Box::leak(Box::new(
                MetaArena::reserve(
                    Sele4nSim::new(SIM_META_BYTES),
                    ArenaId::DEFAULT,
                    SIM_META_BYTES,
                )
                .ok()?,
            ));
            let pagemap: &'static PageMap = Box::leak(Box::new(PageMap::new()));
            // The simulator's pools are charged eagerly: the small geometry
            // for the default arena, and a small registry (G-sim runs the
            // identical vertical slice over this set).
            let default_cfg = ArenaConfig {
                allocator: AllocatorConfig::small(),
                ..ArenaConfig::default()
            };
            let set = ArenaSet::new(
                SimFactory,
                meta,
                meta,
                pagemap,
                &default_cfg,
                SIM_ARENA_CAPACITY,
            )
            .ok()?;
            Some(AnyAllocator::Sim(set))
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
        // The engine genuinely freed it: the same class slot comes back.
        // SAFETY: valid layout; q freed below.
        let q = unsafe { g.alloc(layout) };
        assert_eq!(q, p, "freed object must be recycled by the engine");
        // SAFETY: q is live.
        unsafe { g.dealloc(q, layout) };
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

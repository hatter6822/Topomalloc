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
mod cache_api;
mod debug_api;
mod deterministic_api;
mod entropy;
mod errno_shim;
mod extended;
mod fork_api;
mod hooks_api;
mod numa_api;
mod policy;
mod profile_api;
mod quarantine_api;
pub mod sampling;
mod stats_api;

pub use arena_api::{
    topo_arena_configure, topo_arena_create, topo_arena_create_ex, topo_arena_delegate,
    topo_arena_destroy, topo_arena_handle, topo_arena_id, topo_arena_reset, topo_mallocx_arena,
    TOPO_RIGHTS_ALL, TOPO_RIGHT_ALLOC, TOPO_RIGHT_DESTROY, TOPO_RIGHT_FREE, TOPO_RIGHT_STATS,
};
pub use c_api::{
    topomalloc_aligned_alloc, topomalloc_backend, topomalloc_calloc, topomalloc_free,
    topomalloc_free_aligned_sized, topomalloc_free_sized, topomalloc_malloc,
    topomalloc_malloc_usable_size, topomalloc_memalign, topomalloc_posix_memalign,
    topomalloc_pvalloc, topomalloc_realloc, topomalloc_reallocarray, topomalloc_valloc,
    topomalloc_version,
};
pub use cache_api::{
    topomalloc_cache_budget_tick, topomalloc_cache_flush_all, topomalloc_cache_flush_core,
    topomalloc_cache_register_thread, topomalloc_cache_rseq_active,
};
pub use debug_api::{topomalloc_debug_check_now, topomalloc_debug_checks_enabled};
pub use deterministic_api::{
    topomalloc_deterministic_enabled, topomalloc_deterministic_next_trace_id,
    topomalloc_deterministic_seed, topomalloc_deterministic_set_enabled,
    topomalloc_deterministic_set_force_purge, topomalloc_deterministic_set_force_slow_path,
    topomalloc_deterministic_set_seed,
};
pub use extended::{
    topo_align_lg, topo_arena, topo_dallocx, topo_hot, topo_mallocx, topo_nallocx, topo_rallocx,
    topo_sdallocx, topo_xallocx, TOPO_ALIGN_LG_MASK, TOPO_GUARDED, TOPO_LIFETIME_LONG,
    TOPO_LIFETIME_MEDIUM, TOPO_LIFETIME_SHORT, TOPO_NO_HUGEPAGE, TOPO_PREFER_HUGEPAGE,
    TOPO_TCACHE_NONE, TOPO_ZERO,
};
pub use fork_api::topomalloc_crash_summary;
pub use hooks_api::{topo_arena_create_hooked, topo_extent_hooks_t, topo_max_hook_backends};
pub use numa_api::{
    topomalloc_numa_bind_failures, topomalloc_numa_nodes, topomalloc_numa_rebalance_moves,
    topomalloc_numa_rebalance_tick, topomalloc_numa_refresh, topomalloc_numa_release,
    topomalloc_numa_spillovers,
};
pub use policy::{set_zero_size_policy, zero_size_policy, ZeroSizePolicy};
pub use profile_api::{
    topomalloc_profile_confident_sites, topomalloc_profile_dump_json, topomalloc_profile_enabled,
    topomalloc_profile_rate, topomalloc_profile_set_rate, topomalloc_profile_sites,
};
pub use quarantine_api::{
    topomalloc_guard_sample_rate, topomalloc_guard_set_sample_rate, topomalloc_quarantine_bytes,
    topomalloc_quarantine_converge, topomalloc_quarantine_enabled, topomalloc_quarantine_max_bytes,
    topomalloc_quarantine_max_objects, topomalloc_quarantine_objects,
    topomalloc_quarantine_set_enabled, topomalloc_quarantine_set_limits,
};
pub use stats_api::{
    topomalloc_explain_memory, topomalloc_stats_json, topomalloc_stats_json_for_label,
    topomalloc_stats_print, topomalloc_stats_snapshot, topomalloc_stats_t,
};

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
        // W16-5: count this operation in-flight so a concurrent `fork()` quiesces
        // it before forking (no internal lock held at fork). One `SeqCst` RMW pair.
        let _op = topo_core::fork::operation_guard();
        let p = dispatch!(self, a => a.allocate(size, align, flags));
        // W17-3: feed live placement profiles. A single relaxed load when profiling is off.
        crate::sampling::on_alloc(p, size, align, flags);
        p
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
        let _op = topo_core::fork::operation_guard();
        let p = dispatch!(self, a => a.allocate_in(arena, size, align, flags));
        crate::sampling::on_alloc(p, size, align, flags);
        p
    }

    /// Publish the heap sampler's confident per-bucket placement consensus into the engine's
    /// learned-hint table (the live W14 learn → place loop). Lock-free for the allocation
    /// path that reads it.
    pub fn publish_learned_hints<const CAP: usize>(
        &self,
        table: &topo_core::SiteProfileTable<CAP>,
    ) {
        dispatch!(self, a => a.publish_learned_hints(table))
    }

    /// Clear the engine's learned-hint table, so the allocation path stops applying learned
    /// placement (called when profiling is disabled, restoring the default placement path).
    pub fn clear_learned_hints(&self) {
        dispatch!(self, a => a.learned_hints().reset())
    }

    /// Free a pointer; see [`FreeOutcome`] for the validation outcomes.
    ///
    /// # Safety
    ///
    /// As [`topo_core::Allocator::free`]: `ptr` must be null, a live pointer
    /// of this allocator the caller still owns, or memory never returned by
    /// it (rejected harmlessly). A stale pointer aliasing a recycled live
    /// allocation would free another owner's object.
    #[inline]
    pub unsafe fn free(&self, ptr: *mut u8) -> FreeOutcome {
        // SAFETY: this method's own contract is the one being forwarded.
        unsafe { self.free_with(ptr, false) }
    }

    /// [`free`](Self::free), honouring `TOPO_TCACHE_NONE` (§10.3): with `cache_bypass`
    /// set, the object goes straight to the central free list instead of the running
    /// core's front-end slot. The route `topo_dallocx`/`topo_sdallocx` take for the flag.
    ///
    /// # Safety
    ///
    /// As [`free`](Self::free).
    pub unsafe fn free_with(&self, ptr: *mut u8, cache_bypass: bool) -> FreeOutcome {
        let _op = topo_core::fork::operation_guard();
        // W17-3: resolve a sampled object's lifetime *before* the free; lock-free
        // (a Bloom reject) for the common non-sampled pointer, and a no-op when off.
        crate::sampling::on_free(ptr);
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.free_with(ptr, cache_bypass) })
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
        let _op = topo_core::fork::operation_guard();
        // W17-3: realloc retires the old object and creates a new one — but **only on
        // success**. A failed realloc leaves the original live (§25.1), so retiring its
        // sampled record would fold a bogus completed lifetime into the site's histogram,
        // drop a still-live object from `sampled_live_bytes` / `internal_sampled_bytes`
        // permanently, and (via the W14 learn→place loop) steer real placement from
        // corrupted data.
        //
        // The record is taken **before** the call, not resolved after it. The core frees
        // `ptr` inside `realloc`, so a post-hoc `on_free(ptr)` races another thread
        // allocating that same address: the sampled set is keyed by address, so the
        // removal would retire *that thread's still-live* record instead. Taking it up
        // front makes the hand-off transactional — retire it on success, put it back on
        // failure. All three are no-ops when sampling is off.
        let taken = crate::sampling::take_for_realloc(ptr);
        // SAFETY: the caller upholds this method's identical contract.
        let p = dispatch!(self, a => unsafe { a.realloc(ptr, new_size, min_align, flags) });
        match (p.is_null(), taken) {
            (false, Some(rec)) => crate::sampling::retire_taken(rec),
            (true, Some(rec)) => crate::sampling::restore_taken(ptr, rec),
            _ => {}
        }
        if !p.is_null() {
            crate::sampling::on_alloc(p, new_size, min_align, flags);
        }
        p
    }

    /// Resize `ptr` **in place only** (the extended `xallocx`, §10.3): shrink or
    /// grow toward `new_size` without moving, returning the achieved usable size, or
    /// `None` for a non-owned/interior pointer or an alignment the base cannot
    /// satisfy in place. See [`topo_core::Allocator::resize_in_place`].
    ///
    /// # Safety
    ///
    /// As [`realloc`](Self::realloc): `ptr` is a live resize target the caller owns;
    /// not concurrently freed/realloc'd.
    pub unsafe fn resize_in_place(
        &self,
        ptr: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> Option<usize> {
        let _op = topo_core::fork::operation_guard();
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.resize_in_place(ptr, new_size, min_align, flags) })
    }

    /// The usable size of a live allocation (`None` for null/foreign/interior).
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.usable_size(ptr))
    }

    /// Whether `ptr` is a **live** allocation of this allocator.
    ///
    /// **Not** fork-gated: it is lock-free (a pagemap seqlock read + atomic
    /// bitmap bit), so it holds no internal lock at any point and a `fork()`
    /// racing it is harmless — gating it would be pure overhead (W16-5 audit).
    pub fn owns(&self, ptr: *mut u8) -> bool {
        dispatch!(self, a => a.owns(ptr))
    }

    /// Whether `ptr` lies in memory this allocator manages at all (live,
    /// freed-awaiting-reuse, interior, retained, or metadata) — the §35.2
    /// mixed-allocator routing predicate; see
    /// [`Allocator::recognizes`](topo_core::Allocator::recognizes).
    /// **Not** fork-gated: lock-free (a pagemap lookup + metadata-range check), so
    /// it holds no internal lock and needs no quiesce (W16-5 audit).
    pub fn recognizes(&self, ptr: *mut u8) -> bool {
        dispatch!(self, a => a.recognizes(ptr))
    }

    /// A statistics snapshot of the engine (§31.1; map into the Appendix-D
    /// JSON with `topo_stats::Stats::record_allocator`).
    pub fn stats(&self) -> AllocatorStats {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.stats())
    }

    /// Enable/disable the security quarantine at runtime (§29.4, W18-3). No-op
    /// without the `quarantine` feature; disabling drains the held objects.
    pub fn set_quarantine_enabled(&self, on: bool) {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.set_quarantine_enabled(on))
    }

    /// Whether the quarantine is currently active (feature **and** runtime switch).
    pub fn quarantine_active(&self) -> bool {
        dispatch!(self, a => a.quarantine_active())
    }

    /// Bytes currently held in the quarantine (§29.4; `quarantine.bytes`).
    pub fn quarantine_bytes(&self) -> u64 {
        dispatch!(self, a => a.quarantine_bytes())
    }

    /// Objects currently held in the quarantine (§29.4).
    pub fn quarantine_objects(&self) -> u32 {
        dispatch!(self, a => a.quarantine_objects())
    }

    /// Install a new quarantine policy (§29.4 knobs).
    #[cfg(feature = "quarantine")]
    pub fn set_quarantine_policy(&self, policy: topo_core::QuarantinePolicy) {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.set_quarantine_policy(policy))
    }

    /// The current quarantine policy (§29.4).
    #[cfg(feature = "quarantine")]
    pub fn quarantine_policy(&self) -> topo_core::QuarantinePolicy {
        dispatch!(self, a => a.quarantine_policy())
    }

    /// Background convergence (§29.4, W18-3): really free any held objects beyond the
    /// current budget. Idempotent; a no-op without the `quarantine` feature.
    pub fn converge_quarantine(&self) {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.converge_quarantine())
    }

    /// Set the W18-4 guarded-allocation sampling rate (§29.5): ~1 in `rate`
    /// allocations is guarded (`0` = explicit `TOPO_GUARDED` only). No-op without the
    /// `guard-pages` feature.
    pub fn set_guard_sample_rate(&self, rate: u64) {
        dispatch!(self, a => a.set_guard_sample_rate(rate))
    }

    /// The current guarded-allocation sampling rate (`0` = off).
    pub fn guard_sample_rate(&self) -> u64 {
        dispatch!(self, a => a.guard_sample_rate())
    }

    /// §30.4 (W19-3): if deterministic mode is active, reseed the randomized
    /// security samplers (guard-page sampler, quarantine evictor) from the global
    /// deterministic seed so their choices are reproducible. A no-op when
    /// deterministic mode is off or the hardening feature is not compiled in.
    pub fn apply_deterministic_seed(&self) {
        dispatch!(self, a => a.apply_deterministic_seed())
    }

    /// W18 (§29.4/§29.5): re-seed the randomized **security** samplers from the process
    /// entropy the crate installs at start-up (`entropy::install_process_entropy`), so the
    /// guarded slots and the quarantine's eviction order are unpredictable rather than
    /// the same publicly-computable sequence in every process of this binary. A no-op
    /// when no entropy source answered or the hardening feature is not compiled in.
    pub fn seed_security_samplers(&self) {
        dispatch!(self, a => a.seed_security_samplers())
    }

    // -- the §11 front end (plan 05 W6/W7) -----------------------------------

    /// Publish the host's CPU count to the front end and size the §11.5 cache budget
    /// from it (W6-4/W6-5).
    pub fn set_front_end_cpus(&self, cpus: u32) {
        dispatch!(self, a => a.set_front_end_cpus(cpus))
    }

    /// Enable the W7 RSEQ fast path if the platform supports it; `false` leaves the
    /// front end on the always-correct locked baseline (P-003).
    pub fn enable_front_end_rseq(&self) -> bool {
        dispatch!(self, a => a.enable_front_end_rseq())
    }

    /// Revert the front end to the locked baseline — the §28.1 child-fork
    /// conservative mode.
    pub fn disable_front_end_rseq(&self) {
        dispatch!(self, a => a.disable_front_end_rseq())
    }

    /// Reset the front end in a freshly forked child (§28.1): locked baseline **and**
    /// forget that RSEQ ever ran, since neither membarrier's registration of intent nor
    /// the rseq area survives `fork` and a single-threaded child has no sequence left to
    /// fence against.
    ///
    /// # Safety
    ///
    /// Inherited from [`topo_core::Allocator::reset_front_end_after_fork`]: only valid in
    /// a quiesced single-threaded `fork` child.
    pub unsafe fn reset_front_end_after_fork(&self) {
        // SAFETY: the caller's identical obligation, forwarded.
        unsafe { dispatch!(self, a => a.reset_front_end_after_fork()) }
    }

    /// Whether the W7 RSEQ fast path is active.
    pub fn front_end_rseq_active(&self) -> bool {
        dispatch!(self, a => a.front_end_rseq_active())
    }

    /// Register the calling thread with the RSEQ fast path (§27.6, W7-1).
    pub fn register_front_end_thread(&self) -> bool {
        dispatch!(self, a => a.register_front_end_thread())
    }

    /// Run one W6-5 cache-budget adaptation cycle; returns the total soft capacity
    /// after adaptation, in objects. Host-driven, never on the allocation path.
    pub fn cache_budget_tick(&self) -> usize {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.cache_budget_tick())
    }

    /// Drain the whole front end — every core's slots *and* the transfer cache — back
    /// into the central free lists (W6-7), the §21.3 "drain caches" rung. Returns the
    /// objects that were resident when the drain began.
    pub fn flush_front_end_all(&self) -> usize {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.flush_front_end_all())
    }

    /// Drain **one** core's front-end slots into the transfer cache, overflowing to
    /// central (W6-7 idle-CPU flush). Returns the objects moved out of that core.
    pub fn flush_front_end_core(&self, core: topo_core::CoreId) -> usize {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.flush_front_end_core(core))
    }

    /// Run the **full Appendix-B invariant check** over the live engine on demand
    /// (W19-1, §30.2, DD-2): the shared span/large back-ends and every hooked
    /// backing (B.1/B.4), the central free-list layer and its spans (B.1/B.3), and
    /// the arena registry (B.5). Returns `true` if every invariant holds. This is
    /// the operator/test-triggered home for the O(state) sweeps that DD-2 keeps
    /// off the per-transition hot path (failure-mode F1); the cheap per-span B.3
    /// check already runs as a `debug_assert!` at every central transition. Wrapped
    /// in the fork operation guard like every public op; read-only and lock-safe
    /// (it walks under the §27.2 lock order).
    pub fn check_invariants(&self) -> bool {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.check_invariants())
    }

    /// Visit each core holding front-end residency (§31.2 `BY_CPU` detail, W17-2 / W6):
    /// `f(core, objects, bytes)`. Cores holding nothing are skipped.
    pub fn for_each_cpu_cache<F: FnMut(u32, u64, u64)>(&self, f: F) {
        dispatch!(self, a => a.for_each_cpu_cache(f))
    }

    /// Visit each size class's central-resident free bytes (§31.2 `BY_SIZE_CLASS`
    /// detail, plan 07 W17-2): `f(class_index, object_size, central_free_bytes)`.
    /// The per-class decomposition the stats composer renders under the flag.
    pub fn for_each_size_class_central_free<F: FnMut(usize, u32, u64)>(&self, f: F) {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.for_each_size_class_central_free(f))
    }

    /// Reset the §31.3 peak-live high-water mark to the current live bytes (the §31.2
    /// `RESET_PEAKS` control, plan 07 W17-2).
    pub fn reset_peak_live(&self) {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.reset_peak_live())
    }

    /// A lock-free, allocation-free crash/signal-handler summary (§28.4, W16-6):
    /// reads only cumulative-byte atomics + the process init phase / background
    /// flag, so it is safe to call from a context where structure locks may be
    /// held by the faulting thread. **Not** wrapped in an operation guard — it
    /// takes no lock, so it must remain callable even mid-fork.
    pub fn crash_summary(&self) -> topo_core::CrashSummary {
        dispatch!(self, a => a.crash_summary())
    }

    /// The active backend's name (`"posix"` / `"sele4n-sim"`), proving which
    /// provider the runtime flag selected (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        dispatch!(self, a => a.backend_name())
    }

    // -- arena lifecycle & authority (plan 06 W9) ------------------------------

    /// Create a new explicit arena (§22.4). Returns its [`ArenaId`].
    pub fn arena_create(&self, policy: &ArenaPolicy) -> Result<ArenaId, ArenaError> {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.arena_create(policy))
    }

    /// Delegate an attenuated child arena from `parent` (§36.4).
    pub fn arena_delegate(&self, parent: ArenaId, del: &Delegation) -> Result<ArenaId, ArenaError> {
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.arena_delegate(parent, del))
    }

    /// Reconfigure an arena's non-authority policy (§22.4 *configure*, F-005).
    pub fn arena_configure(&self, arena: ArenaId, cfg: &ArenaConfig) -> Result<(), ArenaError> {
        let _op = topo_core::fork::operation_guard();
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
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.arena_create_hooked(policy, hooks, cfg))
    }

    /// A snapshot of an arena's authority + accounting (§22.2/§36.4).
    pub fn arena_stats(&self, arena: ArenaId) -> Option<ArenaStats> {
        let _op = topo_core::fork::operation_guard();
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
        // W16-5: `handle` resolves through `ArenaRegistry::stats`, which takes the
        // arena-registry lock (rank `ARENA_REGISTRY`), so a `fork()` during it must
        // quiesce it — gate like the sibling `arena_stats`.
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.arenas().handle(arena))
    }

    /// Resolve a handle to its [`ArenaId`], or `None` if it is stale (§36.13).
    pub fn arena_resolve_handle(&self, handle: u64) -> Option<ArenaId> {
        // W16-5: `resolve_handle` also reads through the lock-taking `stats` path.
        let _op = topo_core::fork::operation_guard();
        dispatch!(self, a => a.arenas().resolve_handle(handle))
    }

    /// Reset an explicit arena (§22.5).
    ///
    /// # Safety
    ///
    /// As [`topo_core::Allocator::arena_reset`]: the arena must be quiesced and
    /// the caller accepts that its outstanding pointers become invalid.
    pub unsafe fn arena_reset(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        let _op = topo_core::fork::operation_guard();
        // SAFETY: the caller upholds this method's identical contract.
        dispatch!(self, a => unsafe { a.arena_reset(arena) })
    }

    /// Destroy an explicit arena (§22.6/§36.13).
    ///
    /// # Safety
    ///
    /// As [`arena_reset`](Self::arena_reset).
    pub unsafe fn arena_destroy(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        let _op = topo_core::fork::operation_guard();
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
        use topo_backend_posix::{discover_topology, OsCore};
        use topo_core::{HugeConfig, HugePageBackend, NodeRouter};

        let capacity = (cfg.large_region_bytes / topo_core::HUGEPAGE_SIZE).max(1);
        // Build a live NUMA router: one `mbind`-bound hugepage backend per discovered NUMA
        // node (serving explicit Local/Bind/Interleave), plus — on a multi-node host — an
        // **unbound default backend** serving `OsDefault`/`ArenaPolicy` so the kernel places
        // those pages first-touch (the node of the using thread, the right default). On a
        // single-node machine this is exactly one backend — byte-for-byte the plain hugepage
        // path. The current-CPU source is the real `OsCore` (`sched_getcpu`), so `Local`
        // tracks the running thread; placement follows the arena's `NumaPolicy` (§15.3/§15.5).
        let topo = discover_topology();
        let n = (topo.node_count() as usize).max(1);
        // Per **bound** backend, round up so the per-node set totals at least `capacity`; the
        // unbound default gets the full `capacity` (it serves the common first-touch traffic).
        // The extra is virtual address space (lazily faulted), so it is free.
        let per_node = capacity.div_ceil(n).max(1);
        let router = NodeRouter::build(topo, OsCore, |target| {
            let cap = if target.is_some() { per_node } else { capacity };
            HugePageBackend::new(
                PosixBackingProvider::new(),
                meta,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(cap),
            )
            .ok()
        })?;
        // Leak-and-reclaim, as for the single backend: only a successfully-built router is
        // leaked into `'static`; the raw pointer is kept so a failing allocator build
        // reclaims it (its `Drop` releases *every* reservation). On success it is
        // process-lived (like `meta`/`pagemap`) and published for the C control surface.
        let router_ptr: *mut NodeRouter<PosixBackingProvider, OsCore> =
            Box::into_raw(Box::new(router));
        // SAFETY: `router_ptr` came from `Box::into_raw` just above and is uniquely owned;
        // reborrowing it as `&'static` is sound because it is only reclaimed on the failure
        // arm below (after the borrow ends), and otherwise leaked for the process lifetime.
        let huge: &'static NodeRouter<PosixBackingProvider, OsCore> = unsafe { &*router_ptr };
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
            Ok(a) => {
                // Publish the live router for the host-driven C NUMA control surface
                // (rebalance / refresh / release / stats). The `&'static` outlives the
                // process; `OnceLock::set` ignores a second build (the first allocator wins).
                crate::numa_api::publish_router(huge);
                Some(a)
            }
            Err(_) => {
                // The allocator only *borrowed* `huge` and that borrow ended with the
                // `Err`, so reclaim the leaked router before returning `None`.
                // SAFETY: `router_ptr` was produced by `Box::into_raw` of the same type
                // above; reclaimed exactly once, only on this failure path, never used
                // again (no double-free). Its `Drop` releases every reservation.
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
    // §29 / the glibc `MALLOC_*` convention: environment-derived configuration is ignored
    // in a **secure execution** (setuid/setgid, file capabilities, `AT_SECURE=1`). The
    // backend is configuration like any other knob, and a more consequential one than the
    // sampling and hardening knobs already gated: in a build carrying an alternate backend
    // (`sele4n-sim`), this variable lets whoever controls the environment replace a
    // privileged process's allocator wholesale — changing its backing semantics, or
    // selecting a bounded simulation backend to force allocation failure at a chosen
    // moment. The gate has to sit *here*, before selection, because the allocator is built
    // from this name well before the phase-5 env handling runs.
    if crate::entropy::is_secure_execution() {
        return "posix".into();
    }
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

/// The global allocator **only if already initialized**, without triggering the
/// lazy init (which allocates and takes locks). Used by the crash-summary path
/// (§28.4, W16-6), which must be safe to call from a signal/crash handler where
/// initialization-time allocation is forbidden.
pub(crate) fn global_if_init() -> Option<&'static AnyAllocator> {
    GLOBAL.get().and_then(Option::as_ref)
}

/// The lazily-initialized global allocator, or `None` if the backing regions
/// could not be reserved. The result is memoized: a process that cannot
/// reserve them once keeps reporting OOM (a null `malloc`) rather than
/// retrying.
pub(crate) fn global() -> Option<&'static AnyAllocator> {
    use topo_core::{InitPhase, INIT_PHASE};
    // Fast path: already initialized. The steady-state allocation path pays no
    // fork-gate cost here — the per-operation gate lives in the allocator methods.
    if let Some(inner) = GLOBAL.get() {
        // One relaxed load, then a perfectly-predicted branch that is taken exactly
        // once per successful registration. It is the *only* place a lost registration
        // can be recovered: `pthread_atfork` can fail transiently with `ENOMEM`, and
        // once `GLOBAL` is initialized no caller reaches the slow path below ever
        // again — so without this retry a single transient failure during bring-up
        // costs the process its fork safety permanently, and every later `fork()`
        // leaves the child on inherited allocator state with no quiesce handler.
        // Cheap because it asks whether registration *completed*, not whether someone
        // is attempting it; a call that races an in-flight attempt returns at once
        // rather than blocking (see `fork_api::register_atfork_handlers`).
        if !crate::fork_api::atfork_registered() {
            crate::fork_api::register_atfork_handlers();
        }
        return inner.as_ref();
    }
    // Slow path: the first allocation builds the global allocator. Two #4
    // precautions wrap the lazy init so a `fork()` racing it cannot strand the child
    // on a half-constructed `OnceLock` (the forking thread vanishes in the child):
    //
    //  1. Ensure the `pthread_atfork` handlers are installed *before* the gated init
    //     below, so a concurrent `fork()` is intercepted. The ELF `.init_array` ctor
    //     installs them at load already; this idempotent, re-entrancy-safe call is
    //     the fallback for a build where the ctor was elided (e.g. an rlib linked
    //     into a test binary).
    //  2. Count the init itself in the fork gate, so the now-registered `prefork`
    //     drains-and-waits for it to finish instead of forking mid-construction.
    //
    // The fast path above means neither cost is paid once initialization completes.
    crate::fork_api::register_atfork_handlers();
    let _op = topo_core::fork::operation_guard();
    GLOBAL
        .get_or_init(|| {
            // `_guard` is declared first, so it drops *last* — after `name` and any
            // other init-path temporaries have been allocated and freed through the
            // system allocator — and only then clears the bootstrap flag.
            let _guard = BootstrapGuard::enter();
            // W16-7 (§35.4): advance the observable init phases through **each**
            // boundary as bring-up proceeds (a reentrant caller can read the exact
            // phase via the crash summary / control plane; behaviour is gated on it
            // — e.g. background maintenance cannot run before phase 5).
            //
            // Phase 1: the bootstrap metadata allocator is the floor every
            // reservation stands on (already live as a static, S-007). The
            // `pthread_atfork` handlers are already installed (at load by the ctor,
            // or just above on the fallback path), so a fork mid-init is quiesced.
            INIT_PHASE.advance_to(InitPhase::BootstrapMetadata);
            // Phase 2: OS feature discovery (backend selection; topology/hugepage
            // probing happens inside `new_allocator_named` for the hugepage profile).
            INIT_PHASE.advance_to(InitPhase::OsDiscovery);
            let name = selected_backend_name();
            let allocator = new_allocator_named(&name).or_else(|| new_allocator_named("posix"));
            if allocator.is_some() {
                // Phase 3: the engine exists, so the arena registry + default arena
                // (§22, §35.4 phase 3) are now live.
                INIT_PHASE.advance_to(InitPhase::ArenaRegistry);
                // Phase 4: per-CPU / RSEQ front-end setup (§35.4).
                if let Some(eng) = allocator.as_ref() {
                    // W7-1: bring up the RSEQ fast path *first*. It is what makes
                    // `rseq::current_cpu()` answer, so everything downstream that wants a
                    // cheap per-CPU id — the fork gate's sharding immediately below, and
                    // the front end's own slot selection — gets a real CPU id instead of
                    // falling back to a per-thread key. On a platform without RSEQ this
                    // is a no-op returning `false` and the locked baseline stands (P-003).
                    eng.enable_front_end_rseq();
                    // Register the thread performing the very first allocation — it is
                    // also the first user of the fast path.
                    //
                    // What happens to *later* threads depends on the registration model
                    // (§27.6), and only one of the two is automatic:
                    //
                    // * **glibc ≥ 2.35** registers every thread's rseq area itself, so
                    //   this call is a presence check and later threads are already set
                    //   up — nothing else to do.
                    // * **self-registered** (musl, older glibc): `topo-arch` registers at
                    //   the point it is asked to and never lazily, and the allocation
                    //   fast path deliberately does not attempt it (registration is a
                    //   syscall; retrying it per allocation on every unregistered thread
                    //   would cost more than the path saves). A thread that has not
                    //   called `topomalloc_cache_register_thread` therefore runs the
                    //   **locked baseline** — always correct (P-003), just without the
                    //   restartable sequences. §27.6 makes that the embedder's call to
                    //   make at thread start, and the C surface exposes it for exactly
                    //   this.
                    eng.register_front_end_thread();
                    // W6-4/W6-5: publish the online CPU count so the front end spreads
                    // over the real machine and the §11.5 budget is sized to it.
                    eng.set_front_end_cpus(topo_backend_posix::online_cpus());
                }
                // Enable the per-CPU sharded fork gate iff a cheap per-CPU id is
                // available (the glibc/rseq area); a safe no-op otherwise (single-shard).
                topo_core::probe_and_set_sharding();
                INIT_PHASE.advance_to(InitPhase::PerCpuSetup);
                // W18 (§29.4/§29.5): install real OS entropy for the randomized
                // *security* samplers before anything can be sampled, so the guarded
                // slots and the quarantine's eviction order are not the same
                // publicly-computable sequence in every process of this binary. Runs
                // under the bootstrap guard and allocates nothing (raw `libc` only).
                // Deterministic mode re-seeds from the explicit seed just below, so a
                // seeded replay still reproduces exactly.
                crate::entropy::install_process_entropy();
                if let Some(eng) = allocator.as_ref() {
                    eng.seed_security_samplers();
                }
                // Phase 5: background threads + profiling. Honour `$TOPOMALLOC_SAMPLE_RATE`
                // (§32.1) now, under the bootstrap guard, so the sampler's one-time setup
                // allocations are served by the system allocator and the first real sample
                // is already armed. Only arms when the engine built.
                // Environment-derived configuration is **skipped in a secure execution**
                // (setuid/setgid or `AT_SECURE`): the environment belongs to whoever
                // exec'd us, so honouring it in a privileged process would let an
                // attacker pin the guard/quarantine RNG to a seed they compute offline,
                // switch guard sampling off, or force stack unwinding on every
                // allocation. Same rule glibc's malloc applies to `MALLOC_*`. The
                // compiled-in defaults (and any explicit `topomalloc_*` API call by the
                // program itself) still apply.
                //
                // Each hook takes the just-built engine **by reference**: they run inside
                // `GLOBAL.get_or_init`, so a nested `global()` would re-enter the still
                // -running `OnceLock` and park the thread on its own initialization — a
                // hang at the process's very first allocation, triggered by nothing more
                // than one of these environment variables being set.
                if let Some(eng) = allocator.as_ref() {
                    if !crate::entropy::is_secure_execution() {
                        crate::sampling::init_from_env(eng);
                        // W18-3 (§29.4): honour `$TOPOMALLOC_QUARANTINE` (§32.1) under the same
                        // guard, so an operator can arm the security quarantine at load. Off
                        // unless the env var requests it (and the `quarantine` feature is built).
                        crate::quarantine_api::init_from_env(eng);
                        crate::quarantine_api::guard_init_from_env(eng);
                        // W19-3 (§30.4): honour `$TOPOMALLOC_DETERMINISTIC_SEED` (§32.1) under
                        // the same guard, so a deterministic run seeds its randomized samplers
                        // before the first real allocation. Off unless requested (or the
                        // `deterministic-test` profile is built).
                        crate::deterministic_api::init_from_env(eng);
                    }
                }
                INIT_PHASE.advance_to(InitPhase::BackgroundAndProfiling);
                // Phase 6: every subsystem is up — open for normal operation.
                INIT_PHASE.advance_to(InitPhase::Operational);
            }
            allocator
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

    /// W15-3b D: an in-place **shrink** of a *cache-served* (hugepage-filler)
    /// medium allocation trims its tail pages back to the filler — same base, usable
    /// drops, prefix preserved, `live_bytes` reconciles — the hugepage-profile
    /// counterpart of the extent-path tail return.
    #[cfg(feature = "hugepage-optimized")]
    #[test]
    fn hugepage_realloc_shrink_trims_the_cache_served_tail() {
        let a = new_allocator_named("posix").expect("hugepage_optimized posix engine builds");
        // A sub-hugepage medium allocation is cache-served by the filler.
        let p = a.allocate(200 * 1024, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        let old = a.usable_size(p).unwrap();
        assert_eq!(a.stats().live_bytes, old as u64);
        // SAFETY: `old` writable bytes.
        unsafe { std::ptr::write_bytes(p, 0x5a, old) };

        // Shrink across a page boundary: the filler trims the tail in place.
        // SAFETY: `p` is the live allocation this test owns.
        let q = unsafe { a.realloc(p, 100 * 1024, 16, RequestFlags::NONE) };
        assert_eq!(q, p, "cache-served shrink stays in place");
        let new = a.usable_size(q).unwrap();
        assert!(
            new < old,
            "the cache-served tail was trimmed: {old} -> {new}"
        );
        assert!(new >= 100 * 1024);
        // The prefix survived (no move), and the accounting reconciles (§8.6/§36.17).
        // SAFETY: `new` readable bytes carry the pattern.
        unsafe {
            assert_eq!(*q, 0x5a);
            assert_eq!(*q.add(new - 1), 0x5a);
        }
        assert_eq!(a.stats().live_bytes, new as u64);
        // SAFETY: `q` is the live shrunk allocation.
        unsafe { assert_eq!(a.free(q), FreeOutcome::Freed) };
        assert_eq!(a.stats().live_bytes, 0);
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
        // The engine genuinely freed it (not leaked): the slot is vended again. Every
        // front-end layer is *shared* (a per-CPU slot serves whichever threads run on that
        // core; the transfer cache is process-wide), so a parallel test thread can
        // transiently take the freed object — confirm recycling by membership in a bounded
        // batch (held to drain toward the freed slot, then freed), not by asserting the
        // exact next call.
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

    /// W16-7 (§35.4): the lazy global initializer drives the init phases all the
    /// way to `Operational`, and the lock-free crash summary reflects that phase.
    /// (Triggering `global()` here also covers the phased bring-up end to end.)
    #[test]
    fn global_init_reaches_operational_and_crash_summary_reflects_it() {
        use topo_core::{InitPhase, INIT_PHASE};
        // Force the lazy init (allocate one block through the process allocator).
        let g = TopoMallocGlobal;
        let layout = Layout::from_size_align(96, 16).unwrap();
        // SAFETY: valid non-zero layout; freed below.
        let p = unsafe { g.alloc(layout) };
        assert!(!p.is_null());
        // SAFETY: `p` is a live 96-byte allocation from `g`.
        unsafe { g.dealloc(p, layout) };

        // The phased bring-up reached normal operation.
        assert_eq!(INIT_PHASE.current(), InitPhase::Operational);
        assert!(INIT_PHASE.is_operational());

        // The lock-free crash summary (W16-6) reports the phase + nonzero allocation
        // history, with no lock taken and no allocation.
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid 256-byte writable region. `c_char` is `i8` on
        // some targets and `u8` on others (AArch64), so cast the byte pointer to
        // `c_char` rather than assuming the buffer's element type matches.
        let n = unsafe {
            topomalloc_crash_summary(buf.as_mut_ptr().cast::<core::ffi::c_char>(), buf.len())
        };
        // SAFETY: `n <= buf.len()`; the written bytes are ASCII.
        let text =
            core::str::from_utf8(unsafe { core::slice::from_raw_parts(buf.as_ptr(), n) }).unwrap();
        assert!(
            text.contains(&format!("init_phase={}", InitPhase::Operational as u8)),
            "crash summary must report the operational phase: {text:?}"
        );
        assert!(text.contains("allocated_bytes="));
    }

    /// W16-1b regression guard: the lock-order checker must be **genuinely active**
    /// in the real `topo-abi` artifact (debug), not a silent no-op. This is the
    /// whole point of enabling `topo-core/std` here — without it the checker (and
    /// the S-007 / hook re-entrancy guards) compile out everywhere except
    /// `topo-core`'s own tests. We confirm by tripping it: acquiring a lower rank
    /// while holding a higher one must panic (debug only). If this test ever passes
    /// *without* a panic, the checker has been silently disabled in the artifact.
    #[cfg(debug_assertions)]
    #[test]
    fn lock_order_checker_is_active_in_this_artifact() {
        use topo_core::{LockRank, RankedLock};
        let high: RankedLock<{ LockRank::BACKEND }> = RankedLock::new();
        let low: RankedLock<{ LockRank::CENTRAL }> = RankedLock::new();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            high.acquire();
            low.acquire(); // backend(8) held, acquiring central(5) — a violation
        }));
        std::panic::set_hook(prev);
        assert!(
            caught.is_err(),
            "the lock-order checker is NOT active in topo-abi — `topo-core/std` must be enabled"
        );
        // We panicked mid-`low.acquire`, so `high` is still recorded as held; clear
        // this thread's bookkeeping for the rest of the test binary.
        topo_core::reset_lock_checker();
    }
}

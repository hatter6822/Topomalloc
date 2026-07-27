// SPDX-License-Identifier: MIT
//! The C front-end control surface (§11, plan 05 W6/W7) over the live per-CPU and
//! transfer caches.
//!
//! Caching itself is automatic: every cacheable small allocation and free goes through
//! the running core's slot with no host involvement. What a host *does* drive — on its
//! own cadence, off the allocation fast path, with no background thread — is the same
//! two maintenance operations the release controller and the NUMA router expose:
//!
//! * **drain** ([`topomalloc_cache_flush_all`]) — return front-end residency to the
//!   central free lists, retiring the spans that empty. This is the §21.3 release
//!   ladder's "drain caches" rung, and the operation a host runs before measuring the
//!   heap or under memory pressure.
//! * **adapt** ([`topomalloc_cache_budget_tick`]) — one W6-5 budget cycle: grow the
//!   slots a workload keeps missing on, shrink the ones it keeps overflowing, then
//!   enforce the §11.5 global ceiling.
//!
//! Front-end *residency* is observable through the ordinary stats surface
//! (`topomalloc_stats_snapshot`'s `cache_bytes`, and the `cache.per_cpu_bytes` /
//! `cache.transfer_bytes` keys in the JSON), so this module adds no reporting of its own.
//! Both entry points are always present and safe to call; each returns `0` when the
//! allocator has not been initialised.

use topo_core::CoreId;

/// `size_t topomalloc_cache_flush_all(void)` (§11, W6-7): drain the **whole** front end —
/// every core's per-CPU slots *and* the transfer cache — back into the central free
/// lists, retiring every span that empties.
///
/// Returns the number of objects that were resident in the front end when the drain
/// began, i.e. how much residency it returned to central. Approximate under concurrent
/// load (a free racing the drain may repopulate a slot behind it), exact when quiescent —
/// the §31.1 convention.
///
/// This is the §21.3 "drain caches" rung. It moves no live object and can never fail: a
/// cached object is one the application has already freed, so returning it to central is
/// always safe and always reversible by the next allocation.
#[no_mangle]
pub extern "C" fn topomalloc_cache_flush_all() -> usize {
    // W16-5: the drain takes the front-end, transfer, central, span and backend locks
    // (hand-over-hand), so it runs inside the fork gate — a `fork()` then quiesces it and
    // no internal lock is held at the fork. A rare host-driven call, so the gate costs
    // nothing that matters.
    let _op = topo_core::fork::operation_guard();
    crate::global().map_or(0, |a| a.flush_front_end_all())
}

/// `size_t topomalloc_cache_flush_core(unsigned core)` (§11, W6-7): drain **one** core's
/// per-CPU slots into the transfer cache, overflowing to the central free lists and
/// retiring any span that empties. Returns the number of objects moved out of that core's
/// slots.
///
/// The per-core form is the idle-CPU flush: a host that knows a core has gone quiet can
/// reclaim just that core's residency without disturbing the ones still running. Note
/// that it drains one layer only — the objects land in the transfer cache, not in
/// central — so a host that wants residency genuinely returned uses
/// [`topomalloc_cache_flush_all`]. A `core` beyond the supported maximum moves nothing.
#[no_mangle]
pub extern "C" fn topomalloc_cache_flush_core(core: u32) -> usize {
    let _op = topo_core::fork::operation_guard();
    crate::global().map_or(0, |a| a.flush_front_end_core(CoreId(core)))
}

/// `size_t topomalloc_cache_budget_tick(void)` (§11.5, W6-5): run one cache-budget
/// adaptation cycle and return the total per-CPU soft capacity afterwards, in objects.
///
/// Reads each initialised slot's miss/overflow counters (and resets them), grows the
/// classes a workload keeps missing on, shrinks the ones it keeps overflowing, then
/// shrinks slots in index order until the total is within the §11.5 global budget — which
/// the engine sized from the host's CPU count at start-up. Never on the allocation path;
/// the host picks the cadence (the release pump's tick is the natural one).
#[no_mangle]
pub extern "C" fn topomalloc_cache_budget_tick() -> usize {
    let _op = topo_core::fork::operation_guard();
    crate::global().map_or(0, |a| a.cache_budget_tick())
}

/// `int topomalloc_cache_rseq_active(void)` (§27.4, W7): whether the RSEQ lock-free fast
/// path is currently serving the front end (`1`) or it is on the always-correct locked
/// baseline (`0`).
///
/// Diagnostic only — both modes are correct and observationally identical (that
/// equivalence is what the G-fast battery proves); this answers "did the fast path come
/// up on this machine?" for an operator. It reads `0` before initialisation, on a
/// platform without RSEQ support, under Address/MemorySanitizer, and in a forked child
/// (which reverts to the locked baseline, §28.1).
#[no_mangle]
pub extern "C" fn topomalloc_cache_rseq_active() -> core::ffi::c_int {
    crate::global().map_or(0, |a| core::ffi::c_int::from(a.front_end_rseq_active()))
}

/// `int topomalloc_cache_register_thread(void)` (§27.6, W7-1): register the calling
/// thread with the RSEQ fast path, returning whether it can use it.
///
/// A no-op beyond a presence check in the glibc registration model (the common Linux
/// case), where every thread's rseq area is registered by the C library before it runs
/// any user code. It exists for the self-registration model and for a runtime that
/// creates threads without libc's help; calling it is harmless and idempotent otherwise.
#[no_mangle]
pub extern "C" fn topomalloc_cache_register_thread() -> core::ffi::c_int {
    crate::global().map_or(0, |a| core::ffi::c_int::from(a.register_front_end_thread()))
}

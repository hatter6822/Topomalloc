// SPDX-License-Identifier: MIT
//! The C NUMA control surface (§15, plan 04 W13) over the live placement
//! [`NodeRouter`](topo_core::NodeRouter).
//!
//! Placement itself is automatic (every large/medium allocation routes through the live
//! router under the `hugepage-optimized` profile). The router's **maintenance** operations
//! — the §15.4 cross-domain rebalancer, the §15.2 topology refresh, and the W12 idle-memory
//! release — are **host-driven** (off the allocation fast path, no background thread): a host
//! drives them on its own cadence through these entry points. Each is a no-op (returning 0)
//! when no router is wired (the default extent build, or a single-node host with no NUMA to
//! manage), so the symbols are always present and safe to call.
//!
//! The router is published once at construction via [`publish_router`]; it is reached through
//! a type-erased [`RouterControl`] handle so this layer never names the router's provider /
//! core-source type parameters.

use core::ffi::c_int;
use std::sync::OnceLock;

use topo_core::RouterControl;

/// The live router's type-erased control handle, set once when the `hugepage-optimized`
/// POSIX allocator is built (see `build_posix_allocator`). `None` ⇒ no router (the C entry
/// points below are then no-ops).
static NUMA_ROUTER: OnceLock<&'static dyn RouterControl> = OnceLock::new();

/// Publish the live router for the C control surface (called once at construction, only under
/// the `hugepage-optimized` profile that builds a router). A second call is ignored (the
/// first allocator built wins), matching the global-allocator policy.
#[cfg(feature = "hugepage-optimized")]
pub(crate) fn publish_router(router: &'static dyn RouterControl) {
    let _ = NUMA_ROUTER.set(router);
}

/// `int topomalloc_numa_rebalance_tick(void)` (§15.4, W13-3): drive **one** cross-domain
/// rebalancer tick — move a donor node's idle memory toward a starved node, returning the
/// idle hugepages to the OS. Returns `1` if a move was executed this tick, else `0` (nothing
/// stranded, or no router). The host calls it on its own cadence.
#[no_mangle]
pub extern "C" fn topomalloc_numa_rebalance_tick() -> c_int {
    // W16-5: the router's control ops take the router lock + per-node backend
    // locks (rank `BACKEND`), so they run inside the fork gate — a `fork()` then
    // quiesces them (no router/backend lock is held at fork). Rare host-driven
    // calls, so the gate cost is irrelevant.
    let _op = topo_core::fork::operation_guard();
    NUMA_ROUTER
        .get()
        .map_or(0, |r| c_int::from(r.rebalance_tick().is_some()))
}

/// `int topomalloc_numa_refresh(void)` (§15.2, W13-4): re-discover the platform topology and
/// swap it into the live router (the host-driven hotplug / affinity / cgroup refresh).
/// Returns `1` if a router was refreshed, else `0` (no router). Updates only the CPU→node
/// placement map; the per-node backend set is fixed at construction.
#[no_mangle]
pub extern "C" fn topomalloc_numa_refresh() -> c_int {
    let _op = topo_core::fork::operation_guard(); // W16-5: takes the router lock
    match NUMA_ROUTER.get() {
        Some(r) => {
            r.refresh(topo_backend_posix::discover_topology());
            1
        }
        None => 0,
    }
}

/// `size_t topomalloc_numa_release(size_t reserve_per_node)` (§20–§21, W12/W13): return idle
/// empty hugepages **beyond `reserve_per_node`** (per backend) to the OS — the W11→W12
/// idle-release handoff, driven router-wide. Returns the bytes released (`0` if no router).
/// `reserve_per_node` keeps that many empty hugepages warm per backend for burst reuse.
#[no_mangle]
pub extern "C" fn topomalloc_numa_release(reserve_per_node: usize) -> usize {
    let _op = topo_core::fork::operation_guard(); // W16-5: takes per-node backend locks
    NUMA_ROUTER
        .get()
        .map_or(0, |r| r.release_idle(reserve_per_node))
}

/// `size_t topomalloc_numa_nodes(void)` (§15.5): the number of NUMA nodes the live router
/// places across — `0` when no router is wired (so a caller can detect whether NUMA-aware
/// placement is active), `1` on a single-node host, `n` on a multi-node host.
#[no_mangle]
pub extern "C" fn topomalloc_numa_nodes() -> usize {
    let _op = topo_core::fork::operation_guard(); // W16-5: `stats()` takes the router lock
    NUMA_ROUTER.get().map_or(0, |r| r.stats().nodes as usize)
}

/// `uint64_t topomalloc_numa_bind_failures(void)` (§15.5 "binding failures MUST be visible"):
/// the cumulative count of per-node `mbind` failures (locality lost, never correctness).
#[no_mangle]
pub extern "C" fn topomalloc_numa_bind_failures() -> u64 {
    let _op = topo_core::fork::operation_guard(); // W16-5: `stats()` takes the router lock
    NUMA_ROUTER.get().map_or(0, |r| r.stats().bind_failures)
}

/// `uint64_t topomalloc_numa_rebalance_moves(void)` (§15.4): cumulative rebalancer moves
/// executed across all `topomalloc_numa_rebalance_tick` calls.
#[no_mangle]
pub extern "C" fn topomalloc_numa_rebalance_moves() -> u64 {
    let _op = topo_core::fork::operation_guard(); // W16-5: `stats()` takes the router lock
    NUMA_ROUTER.get().map_or(0, |r| r.stats().rebalance_moves)
}

/// `uint64_t topomalloc_numa_spillovers(void)` (§15.4): cumulative spillover allocations
/// (served off the preferred node because it was full — remote reuse under pressure).
#[no_mangle]
pub extern "C" fn topomalloc_numa_spillovers() -> u64 {
    let _op = topo_core::fork::operation_guard(); // W16-5: `stats()` takes the router lock
    NUMA_ROUTER.get().map_or(0, |r| r.stats().spillovers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numa_control_surface_is_total_and_feature_consistent() {
        // Touch the global allocator first: under `hugepage-optimized` this builds the POSIX
        // allocator and **publishes the router**; otherwise no router is wired.
        let p = crate::topomalloc_malloc(64);
        if !p.is_null() {
            // SAFETY: `p` is a live allocation we just made and now own.
            unsafe { crate::topomalloc_free(p) };
        }
        // Every entry point is safe to call and never panics.
        let nodes = topomalloc_numa_nodes();
        let _ = topomalloc_numa_rebalance_tick();
        let refreshed = topomalloc_numa_refresh();
        let _ = topomalloc_numa_release(0);
        let _ = topomalloc_numa_bind_failures();
        let _ = topomalloc_numa_rebalance_moves();
        let _ = topomalloc_numa_spillovers();

        #[cfg(feature = "hugepage-optimized")]
        {
            assert!(
                nodes >= 1,
                "the live router is wired under hugepage-optimized"
            );
            assert_eq!(refreshed, 1, "refresh succeeds when a router is present");
        }
        #[cfg(not(feature = "hugepage-optimized"))]
        {
            assert_eq!(nodes, 0, "no router wired ⇒ 0 nodes");
            assert_eq!(refreshed, 0, "no router wired ⇒ refresh is a no-op");
        }
    }
}

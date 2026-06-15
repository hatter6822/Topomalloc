// SPDX-License-Identifier: MIT
//! The live NUMA placement router (§15.3/§15.4/§15.5, plan 04 W13).
//!
//! `preferred_node` and the [`Rebalancer`] are pure policy; this is what makes them
//! **affect real allocations**. A [`NodeRouter`] is a [`RegionCacheHook`] that owns one
//! [`HugePageBackend`] per NUMA node (a fixed `[…; MAX_NODES]` array — the core is
//! `no_std` without `alloc`), each [`bound`](HugePageBackend::bind_region) to its node, and
//! routes every large/medium request to the **preferred node's** backend:
//!
//! 1. resolve the request's [`NumaPolicy`] (carried in [`Hints::numa`], set by the engine
//!    from the arena's policy) through [`Topology::preferred_node_at`] using a
//!    [`CoreProvider`] for the running CPU and an atomic interleave counter;
//! 2. serve from that node's backend; on a miss, record a per-node **demand** signal and
//!    spill over to another node (NUMA never fails an allocation — §2.4 safety-first);
//! 3. on free, route the region back to its **owning** backend (by address).
//!
//! It is installed via the existing `Allocator::new_with_huge(&dyn RegionCacheHook)` seam,
//! so the engine is unchanged and the **default (non-router) path is byte-for-byte the
//! same**. On a single-node machine the router holds exactly one backend — identical to
//! the plain hugepage backend — so the integration degrades cleanly where there is nothing
//! to place. It also drives the §15.4 rebalancer live ([`rebalance_tick`](NodeRouter::rebalance_tick))
//! and the §15.2 host-driven refresh ([`refresh`](NodeRouter::refresh)).
//!
//! Placement is **policy, not safety** (§2.4): a misrouted or unbound allocation hurts
//! locality but is always a sound, committed pointer — so there is no Lean obligation.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arena::NumaPolicy;
use crate::backend::{Region, TopoBackingProvider};
use crate::extent::{BackendLock, RegionCacheHook};
use crate::flags::Hints;
use crate::huge::{HugePageBackend, HUGEPAGE_SIZE};
use crate::ids::{ArenaId, NodeId};
use crate::pinned::CoreProvider;
use crate::topology::{NodePressure, RebalanceMove, Rebalancer, Topology, MAX_NODES};

/// A snapshot of the router's running counters (§15.5 observability, W13). The host
/// composes these into the stats snapshot alongside the per-backend coverage.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeRouterStats {
    /// Number of NUMA nodes the router places across (= backend count, ≥ 1).
    pub nodes: u32,
    /// Cumulative NUMA bind failures (§15.5 "failures MUST be visible in stats") — a
    /// per-node `mbind` that did not take at construction (locality lost, not correctness).
    pub bind_failures: u64,
    /// Cumulative rebalancer moves executed (§15.4).
    pub rebalance_moves: u64,
    /// Cumulative bytes returned to the OS by rebalancer moves.
    pub rebalance_released_bytes: u64,
    /// Cumulative spillover allocations (served off the preferred node because it was
    /// full) — the §15.4 "remote reuse … if pressure demands it" at allocation time.
    pub spillovers: u64,
}

/// The §15 **live NUMA placement router** (W13). Owns one [`HugePageBackend`] per node and
/// routes the large path to the preferred node's backend. `P` is the backing provider; `C`
/// the [`CoreProvider`] supplying the running CPU for [`Local`](NumaPolicy::Local) /
/// [`Interleave`](NumaPolicy::Interleave) placement.
pub struct NodeRouter<P: TopoBackingProvider, C: CoreProvider> {
    /// Per-node backends; `backends[i]` is `Some` for `i < n_nodes`, each homed + bound to
    /// node `i`. Fixed-size (no `alloc` in the core).
    backends: [Option<HugePageBackend<P>>; MAX_NODES],
    /// Number of backends (= the topology's node count at construction, ≥ 1). Immutable;
    /// a later [`refresh`](Self::refresh) only remaps CPUs→nodes, not the backend set.
    n_nodes: usize,
    /// The topology snapshot, swappable on refresh under `lock`.
    topo: UnsafeCell<Topology>,
    /// The running-CPU oracle for `Local`/`Interleave`.
    core: C,
    /// Round-robin counter for `Interleave` (advanced only for that mode).
    interleave: AtomicU32,
    /// Guards `topo` (placement reads it briefly; refresh swaps it). Placement is the
    /// large/medium slow path, so a short critical section here is not on the fast path.
    lock: BackendLock,
    /// Per-node recent allocation-failure count — the rebalancer's **demand** approximation
    /// (reset each [`rebalance_tick`](Self::rebalance_tick), so it is "demand since the
    /// last tick"). The allocator has no first-class per-node demand signal; this is the
    /// honest stand-in until real per-node demand accounting (M5).
    alloc_failures: [AtomicU64; MAX_NODES],
    bind_failures: AtomicU64,
    rebalance_moves: AtomicU64,
    rebalance_released_bytes: AtomicU64,
    spillovers: AtomicU64,
}

// SAFETY: `topo` is only ever read/written under `lock` (the §27.2 backend lock); the
// per-node `backends` are each `Sync`, the counters are atomics, and `core`/`n_nodes` are
// immutable after construction. So concurrent `&self` use is data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync, C: CoreProvider + Sync> Sync
    for NodeRouter<P, C>
{
}
// SAFETY: the router owns its backends (each `Send`) and a `Send` core oracle; moving it
// across threads moves both with no shared aliasing.
unsafe impl<P: TopoBackingProvider + Send, C: CoreProvider + Send> Send for NodeRouter<P, C> {}

impl<P: TopoBackingProvider, C: CoreProvider> NodeRouter<P, C> {
    /// Build a router over `topo` with `core`, constructing one backend per node via the
    /// `make(node, os_node)` factory (the caller supplies the provider/metadata/config) and
    /// **binding each backend's region to its OS node** (§15.5). `make` is called for dense
    /// nodes `0..topo.node_count()`; a `None` from it (or zero nodes) fails the whole build,
    /// so the router is all-or-nothing (no partially-built placement). A bind that does not
    /// take is counted (not fatal). On a single-node `topo` this is exactly one backend.
    pub fn build<F>(topo: Topology, core: C, mut make: F) -> Option<Self>
    where
        F: FnMut(NodeId, u32) -> Option<HugePageBackend<P>>,
    {
        let n = topo.node_count() as usize;
        if n == 0 || n > MAX_NODES {
            return None;
        }
        let mut backends: [Option<HugePageBackend<P>>; MAX_NODES] = core::array::from_fn(|_| None);
        let mut bind_failures = 0u64;
        for (i, slot) in backends.iter_mut().enumerate().take(n) {
            let node = NodeId(i as u32);
            let os_node = topo.os_node_of(node);
            let backend = make(node, os_node)?;
            if !backend.bind_region(os_node) {
                bind_failures += 1;
            }
            *slot = Some(backend);
        }
        Some(NodeRouter {
            backends,
            n_nodes: n,
            topo: UnsafeCell::new(topo),
            core,
            interleave: AtomicU32::new(0),
            lock: BackendLock::new(),
            alloc_failures: core::array::from_fn(|_| AtomicU64::new(0)),
            bind_failures: AtomicU64::new(bind_failures),
            rebalance_moves: AtomicU64::new(0),
            rebalance_released_bytes: AtomicU64::new(0),
            spillovers: AtomicU64::new(0),
        })
    }

    /// The number of nodes the router places across (≥ 1).
    #[inline]
    pub fn node_count(&self) -> usize {
        self.n_nodes
    }

    /// Backend for dense node `i` (`i < n_nodes` ⇒ `Some`).
    #[inline]
    fn backend(&self, i: usize) -> Option<&HugePageBackend<P>> {
        self.backends.get(i).and_then(|b| b.as_ref())
    }

    /// The backend that owns `addr` (the one whose reservation contains it), or `None` for
    /// a foreign address — how a freed region is routed home.
    fn owner(&self, addr: usize) -> Option<&HugePageBackend<P>> {
        (0..self.n_nodes).find_map(|i| self.backend(i).filter(|b| b.owns_addr(addr)))
    }

    /// The dense node id whose backend owns `addr` (which node an allocation landed on),
    /// or `None` for a foreign address — diagnostics, and the way a test confirms routing.
    pub fn node_of_addr(&self, addr: usize) -> Option<usize> {
        (0..self.n_nodes).find(|&i| self.backend(i).is_some_and(|b| b.owns_addr(addr)))
    }

    /// Resolve the preferred dense node for `policy` (§15.3/§15.5), clamped to a real
    /// backend. `OsDefault`/`ArenaPolicy` (no override) fall to node 0. Reads the topology
    /// under `lock` (refresh-safe); advances the interleave counter only for `Interleave`.
    fn choose_node(&self, policy: NumaPolicy) -> usize {
        let cpu = self.core.current_core().0;
        let il = if matches!(policy, NumaPolicy::Interleave) {
            self.interleave.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        };
        self.lock.acquire();
        // SAFETY: `lock` held ⇒ exclusive access to `topo` (refresh takes the same lock).
        let node = unsafe { &*self.topo.get() }
            .preferred_node_at(policy, cpu, il)
            .map_or(0, |n| n.0 as usize);
        self.lock.release();
        // Clamp into the fixed backend set (a refreshed topology may name more nodes than
        // there are backends; route the overflow to an existing backend rather than panic).
        node.min(self.n_nodes - 1)
    }

    /// **Swap in a fresh topology snapshot (§15.2 host-driven refresh, W13-4).** The host
    /// calls this after a hotplug/affinity/cgroup change (or a periodic
    /// [`detect_mismatch`](Topology::detect_mismatch)) rebuilds the snapshot. Updates only
    /// the CPU→node *mapping* used for placement; the per-node backend set is fixed at
    /// construction (adding physical nodes needs a router rebuild), so a snapshot naming
    /// more nodes simply routes the extras to existing backends.
    pub fn refresh(&self, new_topo: Topology) {
        self.lock.acquire();
        // SAFETY: `lock` held ⇒ exclusive access to `topo`.
        unsafe { *self.topo.get() = new_topo };
        self.lock.release();
    }

    /// The §15.2 periodic-refresh primitive (W13-4): if the current snapshot disagrees with
    /// a freshly-observed `(cpu, observed_node)`, rebuild via `rebuild` and
    /// [`refresh`](Self::refresh). Returns whether a refresh happened. The host runs this on
    /// its own cadence (matching the release pump's host-driven model — no background
    /// thread inside the allocator).
    pub fn refresh_if_mismatch(
        &self,
        cpu: u32,
        observed_node: NodeId,
        rebuild: impl FnOnce() -> Topology,
    ) -> bool {
        self.lock.acquire();
        // SAFETY: `lock` held ⇒ exclusive access to `topo`.
        let mismatch = unsafe { &*self.topo.get() }.detect_mismatch(cpu, observed_node);
        self.lock.release();
        if mismatch {
            self.refresh(rebuild());
        }
        mismatch
    }

    /// Sample each node's current pressure: **free** = its empty-backed (releasable)
    /// hugepage bytes; **demand** ≈ recent allocation failures × a hugepage (the §15.4
    /// approximation). Returns the array and zeroes the failure counters ("demand since the
    /// last tick").
    fn sample_pressure(&self) -> [NodePressure; MAX_NODES] {
        let mut pressures = [NodePressure::default(); MAX_NODES];
        for (i, p) in pressures.iter_mut().enumerate().take(self.n_nodes) {
            if let Some(b) = self.backend(i) {
                p.free_bytes = b.coverage().empty_backed_bytes;
            }
            let fails = self.alloc_failures[i].swap(0, Ordering::Relaxed);
            p.demand_bytes = fails.saturating_mul(HUGEPAGE_SIZE as u64);
        }
        pressures
    }

    /// **Drive one §15.4 rebalancer tick (W13-3, live execution).** Samples per-node
    /// free/demand, plans the most valuable move ([`Rebalancer::plan`] — surplus-only, so
    /// it never strands the donor), and **executes it**: returns the donor's idle empty
    /// hugepages (the move's `bytes`) to the OS via
    /// [`release_empty_excess`](HugePageBackend::release_empty_excess), relieving global
    /// pressure so the starved node can fault in its own. Returns the executed move (with
    /// `bytes` updated to what was actually released), or `None` if nothing is stranded.
    /// The host calls this on its own cadence (off the allocation fast path).
    pub fn rebalance_tick(&self) -> Option<RebalanceMove> {
        let pressures = self.sample_pressure();
        self.lock.acquire();
        // SAFETY: `lock` held ⇒ exclusive access to `topo`; clone so planning runs unlocked.
        let topo = unsafe { &*self.topo.get() }.clone();
        self.lock.release();

        let mut plan = Rebalancer::plan(&pressures[..self.n_nodes], &topo)?;
        let src = plan.src.0 as usize;
        let donor_free = pressures[src].free_bytes;
        // Keep everything below the move; release the move's worth of empty hugepages.
        let keep = donor_free.saturating_sub(plan.bytes);
        let reserve_hugepages = (keep / HUGEPAGE_SIZE as u64) as usize;
        let released = match self.backend(src) {
            Some(b) => b.release_empty_excess(reserve_hugepages) as u64,
            None => 0,
        };
        plan.bytes = released;
        self.rebalance_moves.fetch_add(1, Ordering::Relaxed);
        self.rebalance_released_bytes
            .fetch_add(released, Ordering::Relaxed);
        Some(plan)
    }

    /// A snapshot of the router's running counters (§15.5 observability).
    pub fn stats(&self) -> NodeRouterStats {
        NodeRouterStats {
            nodes: self.n_nodes as u32,
            bind_failures: self.bind_failures.load(Ordering::Relaxed),
            rebalance_moves: self.rebalance_moves.load(Ordering::Relaxed),
            rebalance_released_bytes: self.rebalance_released_bytes.load(Ordering::Relaxed),
            spillovers: self.spillovers.load(Ordering::Relaxed),
        }
    }

    /// Whether every per-node backend is well-formed (delegates to each filler's §19.8
    /// invariant check) — for debug assertions and tests.
    pub fn check_invariants(&self) -> bool {
        (0..self.n_nodes).all(|i| self.backend(i).is_none_or(|b| b.check_invariants()))
    }

    /// Return every node-backend's whole reservation to the provider (idempotent with
    /// `Drop`). Surfaces the first failure; the rest are still attempted.
    pub fn teardown(&mut self) -> Result<(), crate::error::BackendError> {
        let mut first_err = Ok(());
        for b in self.backends.iter_mut().take(self.n_nodes).flatten() {
            let r = b.teardown();
            if first_err.is_ok() {
                first_err = r;
            }
        }
        first_err
    }
}

impl<P: TopoBackingProvider, C: CoreProvider> RegionCacheHook for NodeRouter<P, C> {
    fn try_alloc(&self, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        // The per-node backends each re-check `NO_HUGEPAGE` (`HugepagePolicy::Avoid`), so a
        // request that must avoid hugepages declines here too and falls to the extent path.
        let node = self.choose_node(hints.numa);
        // Serve from the preferred node; the backend mbind keeps its faults node-local.
        if let Some(b) = self.backend(node) {
            if let Some(r) = b.try_alloc(bytes, align, hints) {
                return Some(r);
            }
            // The preferred node could not satisfy it ⇒ a demand signal for the rebalancer.
            self.alloc_failures[node].fetch_add(1, Ordering::Relaxed);
        }
        // Spillover: best-effort serve from another node rather than fail (§2.4 / §15.4
        // "remote reuse … if pressure demands it"). A cross-node fault is slower but sound.
        for i in 0..self.n_nodes {
            if i != node {
                if let Some(r) = self
                    .backend(i)
                    .and_then(|b| b.try_alloc(bytes, align, hints))
                {
                    self.spillovers.fetch_add(1, Ordering::Relaxed);
                    return Some(r);
                }
            }
        }
        None
    }

    fn try_cache(&self, region: Region) -> bool {
        // Route the freed region back to its owning node-backend (by address).
        self.owner(region.base as usize)
            .is_some_and(|b| b.try_cache(region))
    }

    fn try_cache_revoking(&self, region: Region, arena: ArenaId) -> bool {
        self.owner(region.base as usize)
            .is_some_and(|b| b.try_cache_revoking(region, arena))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::fe::CoreId;
    use crate::generated::tables::PAGE_SIZE;
    use crate::huge::HugeConfig;
    use crate::pinned::FixedCore;
    use crate::topology::TopologyBuilder;
    use crate::BackendError;
    use std::alloc::{alloc, dealloc, Layout};
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    /// A host-backed provider with a **configurable bind failure** — so the router's
    /// best-effort NUMA-bind accounting (§15.5) is exercised deterministically (the real
    /// provider's `mbind` failure is environment-dependent). Each per-node backend gets its
    /// own instance.
    struct MockProvider {
        owned: Mutex<Vec<(usize, Layout)>>,
        fail_bind: AtomicBool,
    }
    impl MockProvider {
        fn new(fail_bind: bool) -> Self {
            Self {
                owned: Mutex::new(Vec::new()),
                fail_bind: AtomicBool::new(fail_bind),
            }
        }
    }
    impl TopoBackingProvider for MockProvider {
        fn reserve(&self, _a: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
            if size == 0 || !align.is_power_of_two() {
                return Err(BackendError::InvalidRequest);
            }
            let layout =
                Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
            // SAFETY: nonzero size + valid power-of-two align.
            let base = unsafe { alloc(layout) };
            if base.is_null() {
                return Err(BackendError::OutOfMemory);
            }
            self.owned.lock().unwrap().push((base as usize, layout));
            Ok(Region { base, len: size })
        }
        fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
            // SAFETY: in-bounds sub-range of the reservation; model DONTNEED by zeroing.
            unsafe { std::ptr::write_bytes(region.base.add(offset), 0, len) };
            Ok(())
        }
        fn release(&self, _a: ArenaId, region: Region) -> Result<(), BackendError> {
            let mut o = self.owned.lock().unwrap();
            let base = region.base as usize;
            let i = o
                .iter()
                .position(|&(b, _)| b == base)
                .ok_or(BackendError::InvalidRequest)?;
            let (_, l) = o.swap_remove(i);
            // SAFETY: exactly the pointer/layout from `reserve`.
            unsafe { dealloc(base as *mut u8, l) };
            Ok(())
        }
        fn bind_node(&self, _region: Region, _os_node: u32) -> Result<(), BackendError> {
            if self.fail_bind.load(std::sync::atomic::Ordering::Relaxed) {
                Err(BackendError::OutOfMemory)
            } else {
                Ok(())
            }
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }
    impl Drop for MockProvider {
        fn drop(&mut self) {
            for (b, l) in self.owned.get_mut().unwrap().drain(..) {
                // SAFETY: as `release`.
                unsafe { dealloc(b as *mut u8, l) };
            }
        }
    }

    fn meta(bytes: usize) -> &'static BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is valid for the process.
        Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
    }

    fn two_node_topo() -> Topology {
        let mut b = TopologyBuilder::new(2);
        b.set_cpu(0, 0, 0).set_cpu(1, 1, 1);
        b.build()
    }

    /// A router of `cap`-hugepage backends over `topo`; node 1's bind fails if requested.
    fn router(
        topo: Topology,
        cap: usize,
        fail_bind_node1: bool,
    ) -> NodeRouter<MockProvider, FixedCore> {
        let m = meta(1 << 21);
        NodeRouter::build(topo, FixedCore(CoreId(0)), |node, _os| {
            let fail = fail_bind_node1 && node.0 == 1;
            HugePageBackend::new(
                MockProvider::new(fail),
                m,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(cap).with_home_node(node),
            )
            .ok()
        })
        .expect("router")
    }

    fn hints(numa: NumaPolicy) -> Hints {
        Hints {
            numa,
            ..Default::default()
        }
    }

    #[test]
    fn routes_to_the_bound_node_and_frees_home() {
        let r = router(two_node_topo(), 8, false);
        assert_eq!(r.node_count(), 2);
        let p0 = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
            .expect("a0");
        let p1 = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("a1");
        assert_eq!(
            r.node_of_addr(p0.base as usize),
            Some(0),
            "Bind(0) → node 0"
        );
        assert_eq!(
            r.node_of_addr(p1.base as usize),
            Some(1),
            "Bind(1) → node 1"
        );
        // Free routes each region home (to its owning backend).
        assert!(r.try_cache(p0));
        assert!(r.try_cache(p1));
        assert!(r.check_invariants());
        assert_eq!(r.stats().bind_failures, 0);
    }

    #[test]
    fn single_node_router_places_everything_on_node_0() {
        let r = router(Topology::single_domain(4), 4, false);
        assert_eq!(r.node_count(), 1);
        // A bind to a node that does not exist clamps to the only backend (total).
        let p = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(3))))
            .expect("a");
        assert_eq!(r.node_of_addr(p.base as usize), Some(0));
        assert!(r.try_cache(p));
    }

    #[test]
    fn bind_failure_is_counted_but_not_fatal() {
        // node 1's bind fails ⇒ exactly one bind failure recorded, yet placement on node 1
        // still works (best-effort: locality lost, not correctness, §2.4).
        let r = router(two_node_topo(), 4, true);
        assert_eq!(r.stats().bind_failures, 1);
        let p = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("a");
        assert_eq!(
            r.node_of_addr(p.base as usize),
            Some(1),
            "still serves node 1"
        );
        assert!(r.try_cache(p));
    }

    #[test]
    fn refresh_changes_local_placement() {
        // Local placement follows the running CPU's node. cpu 0 is on node 0; after a
        // refresh moving cpu 0 to node 1, the same Local request lands on node 1.
        let r = router(two_node_topo(), 4, false);
        let a = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Local))
            .expect("a");
        assert_eq!(r.node_of_addr(a.base as usize), Some(0));
        assert!(r.try_cache(a));
        // Refresh: cpu 0 now on node 1 (still a two-node snapshot).
        let mut b = TopologyBuilder::new(2);
        b.set_cpu(0, 1, 1).set_cpu(1, 0, 0);
        assert!(
            r.refresh_if_mismatch(0, NodeId(1), || b.build()),
            "a moved cpu refreshes"
        );
        let a2 = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Local))
            .expect("a2");
        assert_eq!(
            r.node_of_addr(a2.base as usize),
            Some(1),
            "Local follows the refreshed map"
        );
        assert!(r.try_cache(a2));
    }

    #[test]
    fn interleave_round_robins_across_backends() {
        let r = router(two_node_topo(), 8, false);
        let mut nodes = Vec::new();
        let mut regions = Vec::new();
        for _ in 0..4 {
            let p = r
                .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Interleave))
                .expect("a");
            nodes.push(r.node_of_addr(p.base as usize).unwrap());
            regions.push(p);
        }
        assert_eq!(nodes, vec![0, 1, 0, 1], "interleave alternates nodes");
        for p in regions {
            assert!(r.try_cache(p));
        }
    }

    #[test]
    fn rebalance_tick_releases_idle_donor_memory() {
        // Node 0 holds an idle empty-backed hugepage (free); node 1 is starved (an alloc
        // failure → demand). A rebalance tick plans 0→1 and releases node 0's idle memory.
        let r = router(two_node_topo(), 4, false);
        // Make node 0 hold one empty-backed (committed-then-freed) hugepage.
        let big = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
            .expect("whole hugepage on node 0");
        assert_eq!(r.node_of_addr(big.base as usize), Some(0));
        assert!(r.try_cache(big), "freed ⇒ empty-backed on node 0");
        // Starve node 1: fill it, then a Bind(1) fails and is recorded as demand. With a
        // 4-hugepage capacity, reserve all of it first.
        let mut fillers = Vec::new();
        for _ in 0..4 {
            if let Some(p) =
                r.try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            {
                // A spillover to node 0 does not signal node-1 demand; keep only node-1 ones.
                if r.node_of_addr(p.base as usize) == Some(1) {
                    fillers.push(p);
                } else {
                    assert!(r.try_cache(p));
                }
            }
        }
        // Now node 1 is full: this Bind(1) misses node 1 (demand++), spilling to node 0.
        if let Some(p) = r.try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1)))) {
            assert!(r.try_cache(p));
        }
        let m = r
            .rebalance_tick()
            .expect("node 0 free + node 1 demand ⇒ a move");
        assert_eq!(m.src, NodeId(0), "donor is the idle node");
        assert_eq!(m.dst, NodeId(1), "recipient is the starved node");
        let s = r.stats();
        assert_eq!(s.rebalance_moves, 1);
        assert!(
            s.rebalance_released_bytes > 0,
            "idle donor memory returned to the OS"
        );
        for p in fillers {
            assert!(r.try_cache(p));
        }
        assert!(r.check_invariants());
    }
}

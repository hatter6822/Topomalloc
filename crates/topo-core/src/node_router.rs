// SPDX-License-Identifier: MIT
//! The live NUMA placement router (§15.3/§15.4/§15.5, plan 04 W13).
//!
//! `preferred_node` and the [`Rebalancer`] are pure policy; this is what makes them
//! **affect real allocations**. A [`NodeRouter`] is a [`RegionCacheHook`] that owns one
//! `mbind`-bound [`HugePageBackend`] per NUMA node (a fixed `[…; MAX_NODES]` array — the core
//! is `no_std` without `alloc`) **plus an unbound default backend**, and routes every
//! large/medium request by its [`NumaPolicy`] (carried in [`Hints::numa`], set by the engine
//! from the arena's policy):
//!
//! * **`OsDefault`/`ArenaPolicy`** → the **unbound** default backend, so the kernel places the
//!   pages **first-touch** (on the node of the thread that touches them) — the right default,
//!   never pinning the common case to node 0;
//! * **`Local`** → the running thread's node (via the [`CoreProvider`]);
//! * **`Bind(n)`** → node `n` (stale ⇒ node 0);
//! * **`Interleave`** → round-robin over the **backend count** (so a refresh that changes the
//!   node count can never bias it);
//! * on a miss, record a per-node **demand** signal and spill over to the **nearest** other
//!   node, then the default backend (NUMA never fails an allocation — §2.4 safety-first);
//! * on free, route the region back to its **owning** backend (by address).
//!
//! It is installed via the existing `Allocator::new_with_huge(&dyn RegionCacheHook)` seam, so
//! the engine is unchanged and the **default (non-router) path is byte-for-byte the same**. On
//! a single node the router holds exactly one backend (binding to the only node is a no-op, so
//! it *is* first-touch) — identical to the plain hugepage backend. It also drives the §15.4
//! rebalancer live ([`rebalance_tick`](NodeRouter::rebalance_tick)), the W12 idle-release
//! ([`release_idle`](NodeRouter::release_idle)), and the §15.2 host-driven
//! [`refresh`](NodeRouter::refresh) — exposed to the C ABI through the type-erased
//! [`RouterControl`] surface.
//!
//! Placement is **policy, not safety** (§2.4): a misrouted or unbound allocation hurts
//! locality but is always a sound, committed pointer — so there is no Lean obligation.
//! That claim is not left to prose: the fixed-wall test
//! `placement_never_breaks_the_allocation_contract` (this module's tests) pins the §2.4
//! boundary (size / alignment / full-range validity / free-home are invariant under every
//! NUMA policy *and* a bind failure), the W13 analogue of W14's hint-invariance wall.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arena::NumaPolicy;
use crate::backend::{Region, TopoBackingProvider};
use crate::extent::{BackendLock, RegionCacheHook};
use crate::flags::{Hints, HugepagePolicy};
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

/// The §15 **live NUMA placement router** (W13). Owns one [`HugePageBackend`] per node
/// (each `mbind`-bound to it, serving explicit Local/Bind/Interleave) plus, on a multi-node
/// machine, an **unbound default backend** that serves `OsDefault`/`ArenaPolicy` so the
/// kernel's first-touch policy places those pages on the node of the using thread (the
/// industry default). `P` is the backing provider; `C` the [`CoreProvider`] supplying the
/// running CPU for [`Local`](NumaPolicy::Local) / [`Interleave`](NumaPolicy::Interleave).
pub struct NodeRouter<P: TopoBackingProvider, C: CoreProvider> {
    /// Per-node bound backends; `backends[i]` is `Some` for `i < n_nodes`, each `mbind`-bound
    /// to node `i`. Serve explicit Local/Bind/Interleave. Fixed-size (no `alloc` in the core).
    backends: [Option<HugePageBackend<P>>; MAX_NODES],
    /// The **unbound** backend (no `mbind`) serving `OsDefault`/`ArenaPolicy` — so the OS
    /// places each page first-touch (on the touching thread's node). `Some` only when
    /// `n_nodes > 1`; on a single node the lone (effectively-unbound) backend is the default.
    default_backend: Option<HugePageBackend<P>>,
    /// Number of bound per-node backends (= the topology's node count at construction, ≥ 1).
    /// Immutable; a later [`refresh`](Self::refresh) only remaps CPUs→nodes, not the set.
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
    /// Build a router over `topo` with `core`. The `make(target)` factory builds one backend
    /// at a time (the caller supplies the provider/metadata/config and may size each
    /// differently): `make(Some(node))` builds a per-node backend (the router then
    /// **`mbind`-binds it to that node's OS id**, §15.5), and `make(None)` builds the
    /// **unbound** default backend (no bind — the OS places it first-touch). `make(None)` is
    /// only called when `n_nodes > 1`; on a single node the lone per-node backend is the
    /// default. A `None` from `make` (or zero/over-`MAX_NODES` nodes) fails the whole build —
    /// all-or-nothing (no partially-built placement). A bind that does not take is counted
    /// (not fatal). On a single-node `topo` this is exactly one backend (unchanged).
    pub fn build<F>(topo: Topology, core: C, mut make: F) -> Option<Self>
    where
        F: FnMut(Option<NodeId>) -> Option<HugePageBackend<P>>,
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
            let backend = make(Some(node))?;
            if !backend.bind_region(os_node) {
                bind_failures += 1;
            }
            *slot = Some(backend);
        }
        // The unbound default backend (first-touch) — only on a multi-node machine.
        let default_backend = if n > 1 { Some(make(None)?) } else { None };
        Some(NodeRouter {
            backends,
            default_backend,
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

    /// Bound per-node backend for dense node `i` (`i < n_nodes` ⇒ `Some`).
    #[inline]
    fn backend(&self, i: usize) -> Option<&HugePageBackend<P>> {
        self.backends.get(i).and_then(|b| b.as_ref())
    }

    /// The backend that owns `addr` — a per-node backend or the unbound default — or `None`
    /// for a foreign address. How a freed region is routed home (`try_cache`).
    fn owner(&self, addr: usize) -> Option<&HugePageBackend<P>> {
        (0..self.n_nodes)
            .find_map(|i| self.backend(i).filter(|b| b.owns_addr(addr)))
            .or_else(|| self.default_backend.as_ref().filter(|b| b.owns_addr(addr)))
    }

    /// The dense node id whose **bound** backend owns `addr` (which node an allocation landed
    /// on), or `None` if it is in the default backend or foreign — diagnostics + tests.
    pub fn node_of_addr(&self, addr: usize) -> Option<usize> {
        (0..self.n_nodes).find(|&i| self.backend(i).is_some_and(|b| b.owns_addr(addr)))
    }

    /// Whether `addr` is in the unbound default (first-touch) backend — tests confirm an
    /// `OsDefault` allocation landed there rather than on a bound node.
    pub fn addr_in_default(&self, addr: usize) -> bool {
        self.default_backend
            .as_ref()
            .is_some_and(|b| b.owns_addr(addr))
    }

    /// Resolve the target node for `policy` (§15.3/§15.5): `Some(node)` for the explicit
    /// modes (`Local`/`Bind`/`Interleave`), or **`None`** for `OsDefault`/`ArenaPolicy`,
    /// which the caller serves from the unbound first-touch backend. `Local` reads the
    /// running CPU's node (refresh-safe under `lock`); `Interleave` round-robins over the
    /// **backend count** (so a node-count-changing refresh can never bias it); `Bind` clamps
    /// a stale node to node 0. The result is always a real backend index.
    fn choose_node(&self, policy: NumaPolicy) -> Option<usize> {
        let n = self.n_nodes;
        match policy {
            NumaPolicy::OsDefault | NumaPolicy::ArenaPolicy => None,
            NumaPolicy::Interleave => {
                let il = self.interleave.fetch_add(1, Ordering::Relaxed) as usize;
                Some(il % n)
            }
            NumaPolicy::Local => {
                let cpu = self.core.current_core().0;
                self.lock.acquire();
                // SAFETY: `lock` held ⇒ exclusive access to `topo` (refresh takes it too).
                let node = unsafe { &*self.topo.get() }.node_of_cpu(cpu).0 as usize;
                self.lock.release();
                Some(node.min(n - 1))
            }
            NumaPolicy::Bind(node) => {
                let node = node.0 as usize;
                Some(if node < n { node } else { 0 })
            }
        }
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

    /// **Drive W12 idle-memory release across every backend (the W11→W12 handoff).** Returns
    /// each backend's empty-backed hugepages **beyond `reserve_per_node`** to the OS (via
    /// [`release_empty_excess`](HugePageBackend::release_empty_excess)) — the per-node bound
    /// backends *and* the unbound default — so a host purge pump can reclaim idle hugepage
    /// memory router-wide on its own cadence (off the alloc fast path). Returns the total
    /// bytes released. `reserve_per_node` keeps that many empty hugepages warm per backend
    /// for burst reuse (the §21.4 demand-reserve choice is the release controller's, W12).
    pub fn release_idle(&self, reserve_per_node: usize) -> usize {
        let mut released = 0;
        for i in 0..self.n_nodes {
            if let Some(b) = self.backend(i) {
                released += b.release_empty_excess(reserve_per_node);
            }
        }
        if let Some(def) = self.default_backend.as_ref() {
            released += def.release_empty_excess(reserve_per_node);
        }
        released
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

    /// Whether every backend is well-formed — the per-node backends and the unbound default
    /// (delegates to each filler's §19.8 invariant check) — for debug assertions and tests.
    pub fn check_invariants(&self) -> bool {
        (0..self.n_nodes).all(|i| self.backend(i).is_none_or(|b| b.check_invariants()))
            && self
                .default_backend
                .as_ref()
                .is_none_or(|b| b.check_invariants())
    }

    /// Return every backend's whole reservation to the provider (per-node + the default;
    /// idempotent with `Drop`). Surfaces the first failure; the rest are still attempted.
    pub fn teardown(&mut self) -> Result<(), crate::error::BackendError> {
        let mut first_err = Ok(());
        for b in self
            .backends
            .iter_mut()
            .take(self.n_nodes)
            .flatten()
            .chain(self.default_backend.iter_mut())
        {
            let r = b.teardown();
            if first_err.is_ok() {
                first_err = r;
            }
        }
        first_err
    }

    /// Serve a request bound to node `node` (Local/Bind/Interleave): try its backend, then,
    /// on a miss (recorded as per-node demand), spill to the **nearest other node** (§15.4
    /// "remote reuse"), and finally to the unbound default backend. NUMA never fails an
    /// allocation outright (§2.4): a cross-node fault is slower but sound.
    fn alloc_bound(&self, node: usize, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        if let Some(b) = self.backend(node) {
            if let Some(r) = b.try_alloc(bytes, align, hints) {
                return Some(r);
            }
            self.alloc_failures[node].fetch_add(1, Ordering::Relaxed);
        }
        // Read the distance from `node` to each other backend under the lock; sort + allocate
        // unlocked. `node` itself keeps distance `MAX`, so it is never re-tried.
        let n = self.n_nodes;
        let mut dist = [u8::MAX; MAX_NODES];
        self.lock.acquire();
        // SAFETY: `lock` held ⇒ exclusive access to `topo`.
        let topo = unsafe { &*self.topo.get() };
        for (i, d) in dist.iter_mut().enumerate().take(n) {
            if i != node {
                *d = topo.distance(NodeId(node as u32), NodeId(i as u32));
            }
        }
        self.lock.release();
        // Try the other backends nearest-first (selection over a ≤ MAX_NODES array).
        for _ in 0..n {
            let mut best = usize::MAX;
            let mut best_dist = u8::MAX;
            for (i, &d) in dist.iter().enumerate().take(n) {
                if d < best_dist {
                    best_dist = d;
                    best = i;
                }
            }
            if best == usize::MAX {
                break; // every other backend tried (all distances exhausted)
            }
            dist[best] = u8::MAX; // mark tried
            if let Some(r) = self
                .backend(best)
                .and_then(|b| b.try_alloc(bytes, align, hints))
            {
                self.spillovers.fetch_add(1, Ordering::Relaxed);
                return Some(r);
            }
        }
        // Last resort: the unbound default backend (multi-node only).
        if let Some(r) = self
            .default_backend
            .as_ref()
            .and_then(|b| b.try_alloc(bytes, align, hints))
        {
            self.spillovers.fetch_add(1, Ordering::Relaxed);
            return Some(r);
        }
        None
    }

    /// Serve an `OsDefault`/`ArenaPolicy` request first-touch: from the unbound default
    /// backend (multi-node), spilling to the per-node backends on a full default; on a single
    /// node the lone backend serves it (binding to the only node is a no-op, so it *is*
    /// first-touch).
    fn alloc_default(&self, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        match self.default_backend.as_ref() {
            Some(def) => {
                if let Some(r) = def.try_alloc(bytes, align, hints) {
                    return Some(r);
                }
                for i in 0..self.n_nodes {
                    if let Some(r) = self
                        .backend(i)
                        .and_then(|b| b.try_alloc(bytes, align, hints))
                    {
                        self.spillovers.fetch_add(1, Ordering::Relaxed);
                        return Some(r);
                    }
                }
                None
            }
            None => self
                .backend(0)
                .and_then(|b| b.try_alloc(bytes, align, hints)),
        }
    }
}

impl<P: TopoBackingProvider, C: CoreProvider> RegionCacheHook for NodeRouter<P, C> {
    fn try_alloc(&self, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        // Honor `NO_HUGEPAGE` (§10.4) **here**, before choosing a node or touching a
        // backend: an avoid-decline is policy, not an allocation failure, so it must not
        // pollute the per-node demand signal. Declining lets the large path fall through to
        // the plain extent manager. (Each backend also re-checks this defensively.)
        if matches!(hints.hugepage, HugepagePolicy::Avoid) {
            return None;
        }
        // Explicit policy → the bound node's backend; OsDefault/ArenaPolicy → first-touch.
        match self.choose_node(hints.numa) {
            Some(node) => self.alloc_bound(node, bytes, align, hints),
            None => self.alloc_default(bytes, align, hints),
        }
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

    fn try_trim(&self, region: Region, new_len: usize) -> Option<usize> {
        // Route the in-place tail trim (W15-3b cache-served shrink) to the node-backend
        // that owns the allocation (by address) — placement never changes the address,
        // so the owner is unchanged across the shrink.
        self.owner(region.base as usize)
            .and_then(|b| b.try_trim(region, new_len))
    }
}

/// A **type-erased control surface** over a [`NodeRouter`] (W13), so a host — e.g. the C ABI,
/// which cannot name the router's `P`/`C` type parameters — can drive the §15.4 rebalancer,
/// the §15.2 refresh, and the W12 idle-release, and read the §15.5 counters, through a
/// `&dyn RouterControl`. These are **host-driven** maintenance operations (off the alloc fast
/// path, no background thread): the host calls them on its own cadence.
pub trait RouterControl: Send + Sync {
    /// Drive one §15.4 cross-domain rebalancer tick; the executed move, or `None` if nothing
    /// is stranded. See [`NodeRouter::rebalance_tick`].
    fn rebalance_tick(&self) -> Option<RebalanceMove>;
    /// Swap in a fresh topology snapshot (§15.2 host-driven refresh). See
    /// [`NodeRouter::refresh`].
    fn refresh(&self, topo: Topology);
    /// Return idle empty hugepages beyond `reserve_per_node` (per backend) to the OS (W12);
    /// the total bytes released. See [`NodeRouter::release_idle`].
    fn release_idle(&self, reserve_per_node: usize) -> usize;
    /// A snapshot of the router's running counters (§15.5). See [`NodeRouter::stats`].
    fn stats(&self) -> NodeRouterStats;
}

impl<P: TopoBackingProvider + Send + Sync, C: CoreProvider + Send + Sync> RouterControl
    for NodeRouter<P, C>
{
    #[inline]
    fn rebalance_tick(&self) -> Option<RebalanceMove> {
        NodeRouter::rebalance_tick(self)
    }
    #[inline]
    fn refresh(&self, topo: Topology) {
        NodeRouter::refresh(self, topo)
    }
    #[inline]
    fn release_idle(&self, reserve_per_node: usize) -> usize {
        NodeRouter::release_idle(self, reserve_per_node)
    }
    #[inline]
    fn stats(&self) -> NodeRouterStats {
        NodeRouter::stats(self)
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

    /// A router of `cap`-hugepage backends over `topo` (per-node + the unbound default);
    /// node 1's bind fails if requested.
    fn router(
        topo: Topology,
        cap: usize,
        fail_bind_node1: bool,
    ) -> NodeRouter<MockProvider, FixedCore> {
        let m = meta(1 << 21);
        NodeRouter::build(topo, FixedCore(CoreId(0)), |target| {
            let fail = matches!(target, Some(n) if fail_bind_node1 && n.0 == 1);
            HugePageBackend::new(
                MockProvider::new(fail),
                m,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(cap),
            )
            .ok()
        })
        .expect("router")
    }

    /// A two-node router with an explicit per-node hugepage capacity (so a test can make
    /// one node trivially fillable for deterministic demand). The unbound default backend
    /// gets the larger capacity.
    fn router_caps(cap0: usize, cap1: usize) -> NodeRouter<MockProvider, FixedCore> {
        let m = meta(1 << 21);
        NodeRouter::build(two_node_topo(), FixedCore(CoreId(0)), |target| {
            let cap = match target {
                Some(n) if n.0 == 0 => cap0,
                Some(_) => cap1,
                None => cap0.max(cap1), // the unbound default backend
            };
            HugePageBackend::new(
                MockProvider::new(false),
                m,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(cap),
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
    fn avoid_hugepage_declines_without_polluting_demand() {
        // A NO_HUGEPAGE request must decline (so the large path uses the extent manager)
        // WITHOUT recording a per-node demand signal — an avoid is policy, not an
        // allocation failure. Regression: a decline used to increment alloc_failures.
        let r = router_caps(2, 2);
        // Give node 1 an idle empty-backed hugepage (a would-be donor).
        let big = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("hugepage on node 1");
        assert!(r.try_cache(big), "freed ⇒ empty-backed on node 1");
        // Several NO_HUGEPAGE requests: each declines, none spills.
        let avoid = Hints {
            hugepage: HugepagePolicy::Avoid,
            ..Default::default()
        };
        for _ in 0..4 {
            assert!(
                r.try_alloc(64 * 1024, PAGE_SIZE, avoid).is_none(),
                "NO_HUGEPAGE declines the hugepage backend"
            );
        }
        assert_eq!(
            r.stats().spillovers,
            0,
            "an avoid-decline is not a spillover"
        );
        // No demand was fabricated, so nothing is stranded ⇒ no rebalance. (With the bug
        // the four avoids would have inflated node-0 demand and triggered a spurious move.)
        assert!(
            r.rebalance_tick().is_none(),
            "an avoid never fabricates rebalancer demand"
        );
        assert_eq!(r.stats().rebalance_moves, 0);
    }

    #[test]
    fn router_scales_to_max_nodes() {
        // Build a router across the full MAX_NODES bound backends (+ the unbound default) and
        // route a request to each node and the default, confirming all are built + addressable.
        let m = meta(1 << 22);
        let mut tb = TopologyBuilder::new(MAX_NODES as u32);
        for c in 0..MAX_NODES as u32 {
            tb.set_cpu(c, c, c);
        }
        let r = NodeRouter::build(tb.build(), FixedCore(CoreId(0)), |_target| {
            HugePageBackend::new(
                MockProvider::new(false),
                m,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(1),
            )
            .ok()
        })
        .expect("MAX_NODES router");
        assert_eq!(r.node_count(), MAX_NODES);
        let mut regions = Vec::new();
        for node in 0..MAX_NODES {
            let p = r
                .try_alloc(
                    64 * 1024,
                    PAGE_SIZE,
                    hints(NumaPolicy::Bind(NodeId(node as u32))),
                )
                .expect("bind alloc");
            assert_eq!(
                r.node_of_addr(p.base as usize),
                Some(node),
                "Bind({node}) → node {node}"
            );
            regions.push(p);
        }
        // OsDefault lands in the unbound default backend (distinct from every bound node).
        let d = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::OsDefault))
            .expect("default");
        assert!(r.addr_in_default(d.base as usize));
        regions.push(d);
        for p in regions {
            assert!(r.try_cache(p));
        }
        assert!(r.check_invariants());
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

    /// **The §2.4 placement safety boundary, as a fixed wall** — the W13 analogue of
    /// W14's `engine_size_align_validity_free_are_invariant_under_hints`. A routed
    /// allocation's *size, alignment, full-range validity, and free-home* are invariant
    /// under **every** NUMA policy **and even a bind failure**: placement steers
    /// locality, never correctness (§2.4). W13 is pure policy, so it carries no Lean
    /// obligation — and **this test pins the machine-checked boundary that licenses that
    /// claim**, rather than leaving "placement is policy" a prose assertion. (`MockProvider`
    /// backs each reservation with real `alloc`, so the writes below exercise genuine
    /// committed memory.)
    #[test]
    fn placement_never_breaks_the_allocation_contract() {
        let policies = [
            NumaPolicy::Local,
            NumaPolicy::Bind(NodeId(0)),
            NumaPolicy::Bind(NodeId(1)),
            NumaPolicy::Bind(NodeId(99)), // a stale id must clamp to a real node, never fail/escape
            NumaPolicy::Interleave,
            NumaPolicy::OsDefault,
            NumaPolicy::ArenaPolicy,
        ];
        // Medium (sub-hugepage), exactly one hugepage, and just-over-a-hugepage (the
        // awkward region-cache size) — the families the router serves.
        let sizes = [
            64 * 1024usize,
            200 * 1024,
            HUGEPAGE_SIZE,
            HUGEPAGE_SIZE + PAGE_SIZE,
        ];
        for fail_bind in [false, true] {
            // Ample per-node capacity so the contract sweep never spill-OOMs — capacity is
            // W11's concern (tested there); here we isolate the placement boundary, freeing
            // each region before the next so one hugepage per node would in fact suffice.
            let r = router(two_node_topo(), 16, fail_bind);
            for &policy in &policies {
                for &size in &sizes {
                    let region = r.try_alloc(size, PAGE_SIZE, hints(policy)).unwrap_or_else(|| {
                        panic!("policy {policy:?} size {size} (fail_bind={fail_bind}) was not served")
                    });
                    // Size invariant: the served region covers the request (never under-allocates).
                    assert!(
                        region.len >= size,
                        "{policy:?} size {size}: region len {} < requested",
                        region.len
                    );
                    // Alignment invariant: page-aligned regardless of which node served it.
                    assert_eq!(
                        region.base as usize % PAGE_SIZE,
                        0,
                        "{policy:?} size {size}: region misaligned"
                    );
                    // Validity invariant: the whole region is writable and reads back — a
                    // bind failure (locality lost) must still yield committed, usable memory.
                    // SAFETY: `region` is a live region of `region.len` writable bytes the
                    // router just handed out and still owns (freed below).
                    unsafe {
                        core::ptr::write_bytes(region.base, 0xC3, region.len);
                        assert_eq!(
                            *region.base, 0xC3,
                            "{policy:?} size {size}: head unreadable"
                        );
                        assert_eq!(
                            *region.base.add(region.len - 1),
                            0xC3,
                            "{policy:?} size {size}: tail unreadable"
                        );
                    }
                    // Free-home invariant: the region returns to its owning node's backend.
                    assert!(
                        r.try_cache(region),
                        "{policy:?} size {size}: free-home failed"
                    );
                }
            }
            assert!(
                r.check_invariants(),
                "router well-formed after the contract sweep (fail_bind={fail_bind})"
            );
        }
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
        // OsDefault on a single node also uses the lone backend (no separate default backend
        // — binding to the only node is a no-op, so it *is* first-touch).
        let d = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::OsDefault))
            .expect("default");
        assert_eq!(r.node_of_addr(d.base as usize), Some(0));
        assert!(
            !r.addr_in_default(d.base as usize),
            "no separate default on a single node"
        );
        assert!(r.try_cache(d));
    }

    #[test]
    fn os_default_uses_the_unbound_first_touch_backend() {
        // On a multi-node machine, OsDefault/ArenaPolicy serve from the UNBOUND default
        // backend (so the kernel places each page first-touch on the using thread's node),
        // NOT a bound per-node backend. Explicit Bind still routes to the bound node.
        let r = router(two_node_topo(), 8, false);
        assert_eq!(r.node_count(), 2);
        let d = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::OsDefault))
            .expect("default");
        assert!(
            r.addr_in_default(d.base as usize),
            "OsDefault → the unbound default backend"
        );
        assert_eq!(
            r.node_of_addr(d.base as usize),
            None,
            "not a bound per-node backend"
        );
        let a = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::ArenaPolicy))
            .expect("arena");
        assert!(
            r.addr_in_default(a.base as usize),
            "ArenaPolicy is first-touch too"
        );
        let b1 = r
            .try_alloc(64 * 1024, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("bind");
        assert_eq!(
            r.node_of_addr(b1.base as usize),
            Some(1),
            "Bind still routes to the node"
        );
        assert!(!r.addr_in_default(b1.base as usize));
        for p in [d, a, b1] {
            assert!(
                r.try_cache(p),
                "each region frees home to its owning backend"
            );
        }
        assert!(r.check_invariants());
    }

    #[test]
    fn spillover_prefers_the_nearest_node() {
        // Three nodes; node 0 is full and node 1 is *nearer* to node 0 than node 2. A Bind(0)
        // that misses spills to the nearest node (1), not the farther one (2) — §15.4.
        let m = meta(1 << 21);
        let mut tb = TopologyBuilder::new(3);
        tb.set_cpu(0, 0, 0).set_cpu(1, 1, 1).set_cpu(2, 2, 2);
        tb.set_distance(0, 1, 12).set_distance(0, 2, 30); // node 1 nearer to node 0
        let topo = tb.build();
        // node 0 cap 1 (trivially full); nodes 1 & 2 (and the default) cap 4.
        let r = NodeRouter::build(topo, FixedCore(CoreId(0)), |target| {
            let cap = match target {
                Some(n) if n.0 == 0 => 1,
                _ => 4,
            };
            HugePageBackend::new(
                MockProvider::new(false),
                m,
                ArenaId::DEFAULT,
                HugeConfig::with_capacity(cap),
            )
            .ok()
        })
        .expect("router");
        let fill = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
            .expect("fill node 0");
        assert_eq!(r.node_of_addr(fill.base as usize), Some(0));
        let spill = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
            .expect("spill");
        assert_eq!(
            r.node_of_addr(spill.base as usize),
            Some(1),
            "spilled to the nearer node 1, not the farther node 2"
        );
        assert_eq!(r.stats().spillovers, 1);
        for p in [fill, spill] {
            assert!(r.try_cache(p));
        }
        assert!(r.check_invariants());
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
        // Deterministic capacities: node 1 holds exactly one hugepage (trivially full), node
        // 0 has room so its donated empty hugepage cannot be consumed by the spillover.
        let r = router_caps(3, 1);
        // Make node 0 hold one empty-backed (committed-then-freed) hugepage.
        let big = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(0))))
            .expect("whole hugepage on node 0");
        assert_eq!(r.node_of_addr(big.base as usize), Some(0));
        assert!(r.try_cache(big), "freed ⇒ empty-backed on node 0");
        // Fill node 1's single hugepage and hold it, so node 1 is genuinely full.
        let held = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("node 1's one hugepage");
        assert_eq!(r.node_of_addr(held.base as usize), Some(1));
        // A Bind(1) now misses node 1 (demand++), spilling to node 0; free the spillover.
        let spill = r
            .try_alloc(HUGEPAGE_SIZE, PAGE_SIZE, hints(NumaPolicy::Bind(NodeId(1))))
            .expect("a spillover to node 0");
        assert_eq!(
            r.node_of_addr(spill.base as usize),
            Some(0),
            "spilled to node 0"
        );
        assert!(r.try_cache(spill));
        assert_eq!(r.stats().spillovers, 1, "the miss was a recorded spillover");
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
        let fillers = vec![held];
        for p in fillers {
            assert!(r.try_cache(p));
        }
        assert!(r.check_invariants());
    }
}

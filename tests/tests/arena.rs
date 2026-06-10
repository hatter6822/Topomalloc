// SPDX-License-Identifier: MIT
//! Cross-crate arena tests (plan 06 W9; the G-arena gate's executable side):
//! the capability-backed arena lifecycle over the **real POSIX provider**,
//! the B.5 cache-drain acceptance over **real cache components**, the stats
//! integration, and — under `--features sele4n-sim` — the identical vertical
//! slice over the seLe4n simulator (G-sim, D2).

use std::sync::Mutex;

use topo_backend_posix::PosixBackingProvider;
use topo_core::{
    AllocatorConfig, ArenaCacheDrain, ArenaConfig, ArenaError, ArenaId, ArenaName, ArenaSet,
    BackendError, CapRights, DelegationSpec, FreeOutcome, Label, NumaPolicy, PageMap,
    ProviderFactory, RequestFlags, SizeClassId, ThreadCache, TransferCache,
};

/// A modest engine geometry: regions are virtual on POSIX and eagerly charged
/// on the simulator, so the same sizing works for both backends.
fn tiny_engine() -> AllocatorConfig {
    AllocatorConfig {
        span_region_bytes: 1024 * 1024,
        span_extent_slots: 64,
        span_slots: 64,
        large_region_bytes: 4 * 1024 * 1024,
        large_extent_slots: 64,
        large_slots: 64,
    }
}

fn tiny_cfg() -> ArenaConfig {
    ArenaConfig {
        allocator: tiny_engine(),
        ..ArenaConfig::default()
    }
}

fn meta(bytes: usize) -> &'static topo_core::BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: a valid, owned allocation of `len` bytes, leaked for the test.
    Box::leak(Box::new(unsafe { topo_core::BumpArena::new(ptr, len) }))
}

struct PosixFactory;

impl ProviderFactory for PosixFactory {
    type Provider = PosixBackingProvider;
    fn make_provider(&self, _arena: ArenaId) -> Result<PosixBackingProvider, BackendError> {
        Ok(PosixBackingProvider::new())
    }
}

/// Test wrapper for `ArenaSet::free`: every pointer freed here is one the
/// test just received and still owns, null, or memory never returned by the
/// set — exactly the safety contract.
fn tfree<F: ProviderFactory>(s: &ArenaSet<'_, F>, p: *mut u8) -> FreeOutcome {
    // SAFETY: the caller-ownership contract above.
    unsafe { s.free(p) }
}

/// The W9 vertical slice, written once so POSIX and the simulator run the
/// **identical** script (D2/G-sim). Returns the abstract outcome trail:
/// `(step name, success)` — concrete addresses never enter it.
fn arena_vertical_slice<F: ProviderFactory>(set: &ArenaSet<'_, F>) -> Vec<(&'static str, bool)> {
    let mut trail = Vec::new();
    let mut record = |step: &'static str, ok: bool| trail.push((step, ok));

    // Create, route by flag word and by handle, isolate.
    let a = set.create(&tiny_cfg()).expect("create a");
    let b = set.create(&tiny_cfg()).expect("create b");
    let fa = RequestFlags::NONE.with_arena(a.0).unwrap();
    let pa = set.allocate(256, 16, fa);
    let pb = set.allocate_in(b, 256, 16, RequestFlags::NONE);
    let pd = set.allocate(256, 16, RequestFlags::NONE);
    record(
        "alloc a/b/default",
        !pa.is_null() && !pb.is_null() && !pd.is_null(),
    );
    record(
        "conflicting routing fails",
        set.allocate_in(b, 64, 16, fa).is_null(),
    );
    record("isolation invariants", set.check_invariants());

    // Quota-bounded delegation with attenuation (§36.4).
    let parent = set
        .create(&ArenaConfig {
            quota_limit: 512 * 1024,
            ..tiny_cfg()
        })
        .expect("create parent");
    let widened = DelegationSpec {
        name: ArenaName::new("child").unwrap(),
        rights: CapRights::ALL,
        quota_limit: 256 * 1024,
        label: Label(1), // wrong label: must be rejected
        allocator: tiny_engine(),
    };
    record(
        "label violation rejected",
        set.delegate(parent, &widened) == Err(ArenaError::LabelViolation),
    );
    let spec = DelegationSpec {
        label: Label::PUBLIC,
        rights: CapRights::ALLOC.union(CapRights::FREE),
        ..widened
    };
    let child = set.delegate(parent, &spec).expect("delegate child");
    let pc = set.allocate_in(child, 1024, 16, RequestFlags::NONE);
    record("child allocates", !pc.is_null());
    record(
        "child cannot delegate",
        set.delegate(child, &spec) == Err(ArenaError::AuthorityDenied),
    );
    record(
        "second delegation over remaining quota rejected",
        set.delegate(
            parent,
            &DelegationSpec {
                quota_limit: 512 * 1024,
                ..spec
            },
        ) == Err(ArenaError::QuotaExceeded),
    );

    // Reset semantics (§22.5).
    record("reset a", set.reset(a).is_ok());
    record(
        "stale pointer rejected",
        matches!(tfree(set, pa), FreeOutcome::Invalid(_)),
    );
    record("b survives a's reset", tfree(set, pb) == FreeOutcome::Freed);
    record(
        "default survives a's reset",
        tfree(set, pd) == FreeOutcome::Freed,
    );
    let pa2 = set.allocate_in(a, 64, 16, RequestFlags::NONE);
    record("a reusable after reset", !pa2.is_null());
    record("free after reset", tfree(set, pa2) == FreeOutcome::Freed);
    record(
        "default arena protected",
        set.reset(ArenaId::DEFAULT) == Err(ArenaError::DefaultArenaProtected),
    );

    // Destroy/revocation (§22.6/§36.13): the parent cascade revokes the child.
    record("destroy parent", set.destroy(parent).is_ok());
    record(
        "child revoked with parent",
        set.arena_state(child).is_none(),
    );
    record(
        "stale child pointer rejected",
        matches!(tfree(set, pc), FreeOutcome::Invalid(_)),
    );
    record("destroy a", set.destroy(a).is_ok());
    record("destroy b", set.destroy(b).is_ok());
    record(
        "ids retired",
        set.arena_state(a).is_none() && set.arena_state(b).is_none(),
    );
    record(
        "destroyed id is one deterministic error",
        set.destroy(a) == Err(ArenaError::NoSuchArena),
    );
    record("post-teardown invariants", set.check_invariants());
    trail
}

#[test]
fn arena_vertical_slice_over_posix() {
    let m = meta(16 * 1024 * 1024);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    let set = ArenaSet::new(PosixFactory, m, m, pm, &tiny_cfg(), 8).expect("posix arena set");
    for (step, ok) in arena_vertical_slice(&set) {
        assert!(ok, "POSIX vertical slice failed at: {step}");
    }
}

// ---------------------------------------------------------------------------
// W9-4b / B.5: "post-drain: no cache holds an arena object", over the real
// cache components (ThreadCache + TransferCache) wired through the drain seam.
// ---------------------------------------------------------------------------

/// A drain hook over real front-end components: a thread cache and a transfer
/// cache holding free-object addresses from several arenas. Draining an arena
/// **invalidates** (discards) exactly its addresses — the §22.5 "invalidated
/// safely" choice; the teardown reclaims the backing wholesale.
struct RealCacheDrain {
    thread: Mutex<ThreadCache>,
    transfer: TransferCache,
    sc: SizeClassId,
    meta: &'static topo_core::BumpArena,
}

impl ArenaCacheDrain for RealCacheDrain {
    fn drain(&self, arena: ArenaId, classify: &dyn Fn(usize) -> Option<ArenaId>) {
        // Thread cache: the in-tree arena-selective drain (W6), discarding
        // matches (the drain_fn drops the addresses).
        self.thread
            .lock()
            .unwrap()
            .drain_arena(arena, classify, |_sc, _addrs| {});
        // Transfer cache (sc-keyed at M1): pop everything, re-push only the
        // entries that do NOT belong to the draining arena.
        let mut held = [0usize; 64];
        let max = held.len();
        let n = self
            .transfer
            .try_pop_batch(arena, self.sc, &mut held, max, self.meta);
        for &addr in held[..n].iter() {
            if classify(addr) != Some(arena) {
                let pushed = self
                    .transfer
                    .try_push_batch(arena, self.sc, &[addr], self.meta);
                assert_eq!(pushed, 1, "re-push of a surviving entry must fit");
            }
        }
    }
}

#[test]
fn b5_post_drain_no_cache_holds_an_arena_object() {
    let m = meta(16 * 1024 * 1024);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    let sc = topo_core::size_class(64, 16).expect("a 64-byte class");
    let drain = RealCacheDrain {
        thread: Mutex::new(ThreadCache::with_default_budget()),
        transfer: TransferCache::new(),
        sc,
        meta: m,
    };
    let set = ArenaSet::new(PosixFactory, m, m, pm, &tiny_cfg(), 8).expect("arena set");
    set.set_cache_drain(&drain);
    let a = set.create(&tiny_cfg()).unwrap();
    let b = set.create(&tiny_cfg()).unwrap();

    // Fill both caches with a mix of arena-A and arena-B objects (the cache
    // layer stores free-object addresses; these objects are donated to the
    // caches, exactly what a front-end free would do at M2).
    let mut a_addrs = Vec::new();
    let mut b_addrs = Vec::new();
    for i in 0..12 {
        let pa = set.allocate_in(a, 64, 16, RequestFlags::NONE);
        let pb = set.allocate_in(b, 64, 16, RequestFlags::NONE);
        assert!(!pa.is_null() && !pb.is_null());
        a_addrs.push(pa as usize);
        b_addrs.push(pb as usize);
        let mut tc = drain.thread.lock().unwrap();
        if i % 2 == 0 {
            assert!(tc.push(sc, pa as usize));
            assert!(tc.push(sc, pb as usize));
        } else {
            drop(tc);
            assert_eq!(drain.transfer.try_push_batch(a, sc, &[pa as usize], m), 1);
            assert_eq!(drain.transfer.try_push_batch(b, sc, &[pb as usize], m), 1);
        }
    }

    // Reset arena A: the drain must remove every A address from every cache.
    set.reset(a).unwrap();

    // B.5: pop the caches dry — no address of arena A may remain; arena B's
    // entries survive intact.
    let mut survivors = Vec::new();
    {
        let mut tc = drain.thread.lock().unwrap();
        while let Some(addr) = tc.pop(sc) {
            survivors.push(addr);
        }
    }
    let mut held = [0usize; 64];
    let max = held.len();
    let n = drain.transfer.try_pop_batch(b, sc, &mut held, max, m);
    survivors.extend_from_slice(&held[..n]);

    for addr in &survivors {
        assert!(
            !a_addrs.contains(addr),
            "B.5 violated: arena A address {addr:#x} survived the drain"
        );
        assert!(
            b_addrs.contains(addr),
            "unexpected address {addr:#x} appeared in the caches"
        );
    }
    let b_survivors = survivors.len();
    assert_eq!(
        b_survivors,
        b_addrs.len(),
        "every B entry must survive A's drain"
    );

    // The surviving B objects are still live allocations: free them for real.
    for addr in survivors {
        assert_eq!(tfree(&set, addr as *mut u8), FreeOutcome::Freed);
    }
    assert!(set.check_invariants());
}

// ---------------------------------------------------------------------------
// Stats integration (plan 07 wiring): registry snapshots render as JSON.
// ---------------------------------------------------------------------------

#[test]
fn arena_snapshots_flow_into_stats_and_control() {
    let m = meta(16 * 1024 * 1024);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    let set = ArenaSet::new(PosixFactory, m, m, pm, &tiny_cfg(), 8).expect("arena set");
    let a = set
        .create(&ArenaConfig {
            name: ArenaName::new("requests").unwrap(),
            quota_limit: 1024 * 1024,
            numa: NumaPolicy::Local,
            ..tiny_cfg()
        })
        .unwrap();
    let p = set.allocate_in(a, 100, 16, RequestFlags::NONE);
    assert!(!p.is_null());
    set.record_numa_binding_failure(a);

    let mut snaps = Vec::new();
    set.snapshot_all(&mut |s| snaps.push(s));
    assert_eq!(snaps.len(), 2, "default + explicit");

    let json = topo_stats::Stats::arenas_to_json(&snaps);
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid arena JSON");
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    let entry = arr
        .iter()
        .find(|e| e["id"] == a.0)
        .expect("the explicit arena is rendered");
    assert_eq!(entry["name"], "requests");
    assert_eq!(entry["state"], "active");
    assert_eq!(entry["numa"]["mode"], "local");
    assert_eq!(entry["numa"]["binding_failures"], 1);
    assert_eq!(
        entry["quota"]["used"], entry["live_bytes"],
        "ArenaQuotaExact is visible in the rendered stats"
    );

    let mut control = topo_control::Control::new(topo_stats::Profile::active());
    control.set_arenas(snaps);
    assert_eq!(control.get("topo.arenas.count").as_deref(), Some("2"));
    let cj = control.get("topo.arenas.json").unwrap();
    assert!(cj.contains("\"name\": \"requests\""));

    assert_eq!(tfree(&set, p), FreeOutcome::Freed);
    set.destroy(a).unwrap();
}

// ---------------------------------------------------------------------------
// G-sim (D2): the identical vertical slice over the seLe4n simulator. GPL
// linkage, so gated exactly like dual_backend.rs.
// ---------------------------------------------------------------------------

#[cfg(feature = "sele4n-sim")]
mod sim {
    use super::*;
    use topo_backend_sele4n::Sele4nSim;

    struct SimFactory;

    impl ProviderFactory for SimFactory {
        type Provider = Sele4nSim;
        fn make_provider(&self, _arena: ArenaId) -> Result<Sele4nSim, BackendError> {
            let cfg = tiny_engine();
            Ok(Sele4nSim::new(
                cfg.span_region_bytes.max(cfg.large_region_bytes),
            ))
        }
    }

    #[test]
    fn arena_vertical_slice_is_identical_over_sim() {
        // The same script, both backends, identical abstract trails (G-sim).
        let mp = meta(16 * 1024 * 1024);
        let pmp: &'static PageMap = Box::leak(Box::new(PageMap::new()));
        let posix = ArenaSet::new(PosixFactory, mp, mp, pmp, &tiny_cfg(), 8).expect("posix set");
        let posix_trail = arena_vertical_slice(&posix);

        let ms = meta(16 * 1024 * 1024);
        let pms: &'static PageMap = Box::leak(Box::new(PageMap::new()));
        let sim = ArenaSet::new(SimFactory, ms, ms, pms, &tiny_cfg(), 8).expect("sim set");
        let sim_trail = arena_vertical_slice(&sim);

        assert_eq!(
            posix_trail, sim_trail,
            "the W9 lifecycle diverged between POSIX and the simulator"
        );
    }

    #[test]
    fn destroy_walks_the_sim_revocation_chain_and_returns_the_pool() {
        // Capability-specific behaviour POSIX cannot show: after a destroy,
        // the simulator's per-provider untyped pools are made whole again —
        // the §36.6 recycle actually returned the backing (the runtime face
        // of `destroy_revokes_descendants`).
        let m = meta(16 * 1024 * 1024);
        let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
        let set = ArenaSet::new(SimFactory, m, m, pm, &tiny_cfg(), 4).expect("sim set");
        let a = set.create(&tiny_cfg()).unwrap();
        let p = set.allocate_in(a, 64 * 1024, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        // The full protocol must succeed over the simulator's §36.6 state
        // machine (its release asserts the legal dirty→…→recycle tail).
        set.destroy(a).unwrap();
        assert_eq!(set.arena_state(a), None);
        assert!(set.check_invariants());
    }
}

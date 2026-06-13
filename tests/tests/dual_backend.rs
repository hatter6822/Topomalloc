// SPDX-License-Identifier: GPL-3.0-or-later
//! Dual-backend equivalence — the M0 seed of the G-sim gate (W0-14b, D2): the
//! allocator core must run *identically* over the POSIX backend and the seLe4n
//! simulator. This test is compiled only with `--features sele4n-sim`, which
//! links the GPL seLe4n backend; the resulting test binary is GPL-3.0-or-later.
//!
//! Run via `cargo test -p topo-tests --features sele4n-sim` (what `xtask test`
//! and CI do).
#![cfg(feature = "sele4n-sim")]

use topo_backend_posix::PosixBackingProvider;
use topo_backend_sele4n::Sele4nSim;
use topo_core::{SkeletonAllocator, TopoBackingProvider};
use topo_test_support::{gen_ops, DetRng, Op};

/// Run a fixed op stream over an allocator and record the *abstract* outcome of
/// each op (success + usable size), independent of the concrete address.
fn run_outcomes<P: TopoBackingProvider>(
    alloc: &SkeletonAllocator<P>,
    ops: &[Op],
) -> Vec<(bool, usize)> {
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        let (size, align) = match *op {
            Op::Malloc { size, align } => (size, align),
            Op::Calloc { n, size } => (n.saturating_mul(size).max(1), 16),
            Op::Free { .. } => {
                // The skeleton leaks; a free has no abstract outcome to compare.
                out.push((true, 0));
                continue;
            }
        };
        let before = alloc.used_bytes();
        let p = alloc.malloc(size, align);
        let after = alloc.used_bytes();
        if p.is_null() {
            out.push((false, 0));
        } else {
            assert_eq!(
                p as usize % align.max(16),
                0,
                "alignment must hold on every backend"
            );
            out.push((true, after - before));
        }
    }
    out
}

#[test]
fn posix_and_sim_produce_identical_abstract_outcomes() {
    const HEAP: usize = 4 * 1024 * 1024;
    let ops = gen_ops(&mut DetRng::new(0xA11C_A704), 500);

    let posix = SkeletonAllocator::new(PosixBackingProvider::new(), HEAP).expect("posix heap");
    let sim = SkeletonAllocator::new(Sele4nSim::new(HEAP), HEAP).expect("sim heap");

    assert_eq!(posix.backend_name(), "posix");
    assert_eq!(sim.backend_name(), "sele4n-sim");

    let posix_outcomes = run_outcomes(&posix, &ops);
    let sim_outcomes = run_outcomes(&sim, &ops);

    // Co-equal behind one trait: the same requests yield the same success
    // pattern and the same per-op byte usage on both backends (G-sim).
    assert_eq!(
        posix_outcomes, sim_outcomes,
        "backends diverged on identical input"
    );
}

#[test]
fn sim_untyped_pool_accounting_is_exact() {
    // A capability-specific behaviour POSIX does not have: the simulator's
    // authorized untyped pool is charged and recycled exactly.
    let sim = Sele4nSim::new(8192);
    let total = sim.pool_total();
    let r = sim
        .reserve(topo_core::ArenaId::DEFAULT, 4096, 16)
        .expect("retype");
    assert_eq!(sim.pool_remaining(), total - 4096);
    sim.release(topo_core::ArenaId::DEFAULT, r)
        .expect("recycle");
    assert_eq!(sim.pool_remaining(), total);
}

// ---------------------------------------------------------------------------
// W8 (plan 06): the M1 central-path allocator must be co-equal over both
// backends too — the G-sim gate for the real allocation engine, not just the
// M0 skeleton above.
// ---------------------------------------------------------------------------

use topo_core::{
    Allocator, AllocatorConfig, ArenaId, ArenaPolicy, ArenaState, FreeOutcome, MetaArena, PageMap,
    RequestFlags,
};

/// Build an engine over `provider`-style backends with identical, modest
/// sizing on both sides (the simulator's untyped pool is charged eagerly).
fn engine_outcomes<P, F>(mk: F, ops: &[Op]) -> Vec<(bool, usize)>
where
    P: TopoBackingProvider + Sync,
    F: Fn(usize) -> P,
{
    let cfg = AllocatorConfig::small();
    // The arena owns its provider, so the borrow checker pins the drop order:
    // the allocator (borrower) goes first, then the pagemap, then the arena
    // with its backing (§27.5).
    let meta = MetaArena::reserve(mk(4 * 1024 * 1024), ArenaId::DEFAULT, 4 * 1024 * 1024)
        .expect("meta arena");
    let pagemap = PageMap::new();
    let a = Allocator::new(
        mk(cfg.span_region_bytes),
        mk(cfg.large_region_bytes),
        &meta,
        &meta,
        &pagemap,
        ArenaId::DEFAULT,
        cfg,
    )
    .expect("engine");

    // Replay the op stream, recording the *abstract* outcome of each op:
    // (success, usable size) for allocations, (freed, 0) for frees —
    // address-independent, so POSIX and the simulator must agree exactly.
    let mut live: Vec<*mut u8> = Vec::new();
    let mut out = Vec::with_capacity(ops.len());
    for op in ops {
        match *op {
            Op::Malloc { size, align } => {
                let p = a.allocate(size, align, RequestFlags::NONE);
                if p.is_null() {
                    out.push((false, 0));
                } else {
                    assert_eq!(p as usize % align.max(16), 0, "alignment must hold");
                    let usable = a.usable_size(p).expect("live object has a usable size");
                    out.push((true, usable));
                    live.push(p);
                }
            }
            Op::Calloc { n, size } => {
                let total = n.saturating_mul(size).max(1);
                let p = a.allocate(total, 16, RequestFlags::NONE.with_zero());
                if p.is_null() {
                    out.push((false, 0));
                } else {
                    out.push((true, a.usable_size(p).expect("usable")));
                    live.push(p);
                }
            }
            Op::Free { which } => {
                if live.is_empty() {
                    out.push((true, 0));
                } else {
                    let p = live.swap_remove(which % live.len());
                    // SAFETY: `p` is live and owned by this replay.
                    let freed = unsafe { a.free(p) };
                    out.push((freed == FreeOutcome::Freed, 0));
                }
            }
        }
    }
    for p in live {
        // SAFETY: `p` is live and owned by this replay.
        assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
    }
    assert!(a.check_invariants(), "backend left well-formed");
    out
}

#[test]
fn m1_engine_is_co_equal_over_posix_and_sim() {
    let ops = gen_ops(&mut DetRng::new(0x700D_0C0D), 600);
    let posix = engine_outcomes(|_| PosixBackingProvider::new(), &ops);
    let sim = engine_outcomes(Sele4nSim::new, &ops);
    assert_eq!(
        posix, sim,
        "the M1 allocator diverged between POSIX and the seLe4n simulator"
    );
}

// ---------------------------------------------------------------------------
// W9 (plan 06): explicit-arena lifecycle co-equal over both backends — the
// G-sim arena slice (§22/§36.4/§36.13). Create explicit arenas, allocate from
// each, snapshot per-arena accounting, then reset one and destroy the other;
// the abstract outcomes (ids, success, usable sizes, used counters, lifecycle
// states) must match exactly on POSIX and the seLe4n simulator.
// ---------------------------------------------------------------------------

/// Record the abstract outcome of a fixed multi-arena lifecycle script over one
/// backend, address-independent so the two backends must agree exactly.
fn arena_lifecycle_outcomes<P, F>(mk: F) -> Vec<(bool, u64)>
where
    P: TopoBackingProvider + Sync,
    F: Fn(usize) -> P,
{
    let cfg = AllocatorConfig::small();
    let meta = MetaArena::reserve(mk(4 * 1024 * 1024), ArenaId::DEFAULT, 4 * 1024 * 1024)
        .expect("meta arena");
    let pagemap = PageMap::new();
    let a = Allocator::new(
        mk(cfg.span_region_bytes),
        mk(cfg.large_region_bytes),
        &meta,
        &meta,
        &pagemap,
        ArenaId::DEFAULT,
        cfg,
    )
    .expect("engine");

    let mut out: Vec<(bool, u64)> = Vec::new();

    // Two explicit arenas; ids are deterministic (1, then 2) on both backends.
    let scratch = a
        .arena_create(
            &ArenaPolicy::explicit()
                .with_quota(1 << 20)
                .with_name("scratch"),
        )
        .expect("create scratch");
    let temp = a
        .arena_create(&ArenaPolicy::explicit().with_name("temp"))
        .expect("create temp");
    out.push((true, scratch.0 as u64));
    out.push((true, temp.0 as u64));

    // A deterministic mix across default / scratch / temp (small + large).
    let mut default_live: Vec<*mut u8> = Vec::new();
    for i in 0..48usize {
        let arena = match i % 3 {
            0 => ArenaId::DEFAULT,
            1 => scratch,
            _ => temp,
        };
        let size = 16 + (i * 53) % 5000;
        let p = a.allocate_in(arena, size, 16, RequestFlags::NONE);
        out.push((!p.is_null(), a.usable_size(p).unwrap_or(0) as u64));
        if !p.is_null() && arena == ArenaId::DEFAULT {
            default_live.push(p);
        }
    }

    // Per-arena accounting snapshots (the §36.17 used counters).
    out.push((true, a.arena_stats(scratch).unwrap().used));
    out.push((true, a.arena_stats(temp).unwrap().used));

    // Reset `scratch`: it stays Active with zero used (§22.5).
    // SAFETY: this script holds no live pointers into `scratch` past this point.
    let _ = unsafe { a.arena_reset(scratch) };
    out.push((
        a.arenas().is_active(scratch),
        a.arena_stats(scratch).unwrap().used,
    ));

    // Destroy `temp`: it is retired (§22.6).
    // SAFETY: this script holds no live pointers into `temp` past this point.
    let _ = unsafe { a.arena_destroy(temp) };
    out.push((
        a.arena_stats(temp).unwrap().state == ArenaState::Destroyed,
        a.arena_stats(temp).unwrap().used,
    ));

    // Free the surviving default-arena objects; the engine ends well-formed.
    for p in default_live {
        // SAFETY: `p` is a live default-arena pointer owned by this script.
        assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
    }
    out.push((a.check_invariants(), a.stats().live_bytes));
    out
}

#[test]
fn arena_lifecycle_is_co_equal_over_posix_and_sim() {
    let posix = arena_lifecycle_outcomes(|_| PosixBackingProvider::new());
    let sim = arena_lifecycle_outcomes(Sele4nSim::new);
    assert_eq!(
        posix, sim,
        "the W9 arena lifecycle diverged between POSIX and the seLe4n simulator"
    );
    // Sanity: the script ends with a well-formed engine and zero live bytes.
    assert_eq!(posix.last(), Some(&(true, 0)));
}

/// W8 follow-up (self-audit): the *named selector's* simulator arm —
/// `new_allocator_named("sele4n-sim")`, the construction the
/// `TOPOMALLOC_BACKEND` env path runs — is executed, not merely compiled:
/// build it, allocate across the families, free, and verify the stats
/// identities hold over the GPL backend too.
#[test]
fn named_selector_builds_and_runs_the_sim_engine() {
    let a = topo_abi::new_allocator_named("sele4n-sim").expect("sim engine via the named selector");
    assert_eq!(a.backend_name(), "sele4n-sim");

    let small = a.allocate(100, 16, RequestFlags::NONE);
    let medium = a.allocate(40_000, 16, RequestFlags::NONE);
    assert!(!small.is_null() && !medium.is_null());
    let small_usable = a.usable_size(small).expect("live small");
    let medium_usable = a.usable_size(medium).expect("live medium");
    assert!(a.owns(small) && a.recognizes(small));

    let s = a.stats();
    assert_eq!(s.live_bytes, (small_usable + medium_usable) as u64);
    assert_eq!(s.live_large, 1);

    // SAFETY: both pointers are live and owned by this test.
    unsafe {
        assert_eq!(a.free(small), FreeOutcome::Freed);
        assert_eq!(a.free(medium), FreeOutcome::Freed);
    }
    assert_eq!(a.stats().live_bytes, 0);
}

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

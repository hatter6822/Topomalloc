// SPDX-License-Identifier: MIT
//! Criterion micro-benchmark for the W16-5 fork-quiesce gate (§28.1, plan 05).
//! Non-gating: `cargo xtask bench` runs and reports. The gate runs on **every**
//! public allocator operation to make `fork()` safe; this measures its hot-path
//! cost — both the single-shard fallback and the per-CPU sharded mode — so the
//! "correct before fast" trade-off is *quantified*, not asserted.
//!
//! `operation_guard()` is one `fetch_add` (reading the fork bit from the returned
//! value — no CAS retry, no `membarrier`) on the running CPU's own shard, plus the
//! matching decrement on drop. The "sharded" group reflects the production path on
//! a host with a cheap per-CPU id (glibc/rseq); the "single" group is the fallback.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// One in/out of the gate (the per-op cost wrapped around real work).
#[inline]
fn one_op() {
    let g = topo_core::operation_guard();
    black_box(&g);
    drop(g);
}

fn bench_gate(c: &mut Criterion) {
    // Single-shard fallback: leave sharding off (the default before the probe).
    topo_core::set_sharded(false);
    c.bench_function("fork_gate/uncontended/single_shard", |b| {
        b.iter(one_op);
    });

    // Sharded mode (the production path where a cheap CPU id is available). The
    // probe enables it iff `current_cpu()` works; otherwise this measures the same
    // single-shard path (every op lands on shard 0) — the harness still reports.
    topo_core::probe_and_set_sharding();
    c.bench_function("fork_gate/uncontended/sharded", |b| {
        b.iter(one_op);
    });

    // Contended: N threads hammering the gate. Single-shard puts them all on one
    // cacheline; sharded spreads them by CPU. Reports the scalability difference.
    for &threads in &[2usize, 4, 8] {
        c.bench_function(&format!("fork_gate/contended_{threads}t/sharded"), |b| {
            b.iter_custom(|iters| {
                let stop = Arc::new(AtomicBool::new(false));
                let workers: Vec<_> = (0..threads - 1)
                    .map(|_| {
                        let stop = stop.clone();
                        std::thread::spawn(move || {
                            while !stop.load(Ordering::Relaxed) {
                                one_op();
                            }
                        })
                    })
                    .collect();
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    one_op();
                }
                let elapsed = start.elapsed();
                stop.store(true, Ordering::Relaxed);
                for w in workers {
                    w.join().unwrap();
                }
                elapsed
            });
        });
    }
}

criterion_group!(benches, bench_gate);
criterion_main!(benches);

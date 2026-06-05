// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the hot classification/allocation path
//! (W0-8, §34.6). Non-gating: `xtask bench` runs and reports, but CI does not
//! fail on a regression yet (a results schema + workload replay arrive with
//! plan 08 W21-6). These exist so the harness is wired from M0.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

use topo_abi::new_allocator_named;
use topo_core::{classify, size_class, RequestFlags, MIN_ALIGN};

fn bench_size_class(c: &mut Criterion) {
    c.bench_function("size_class(64,16)", |b| {
        b.iter(|| size_class(black_box(64), black_box(16)))
    });
}

fn bench_classify(c: &mut Criterion) {
    c.bench_function("classify(100,16)", |b| {
        b.iter(|| classify(black_box(100), black_box(16), 0))
    });
}

fn bench_classify_flags(c: &mut Criterion) {
    // A non-trivial valid flag word, so the §10.4 flag validation + arena/hint
    // decode cost is measured — not just the flags == 0 fast case.
    let flags = RequestFlags::NONE
        .with_zero()
        .with_arena(1)
        .expect("arena 1 encodes")
        .raw();
    c.bench_function("classify(100,16,flags)", |b| {
        b.iter(|| classify(black_box(100), black_box(16), black_box(flags)))
    });
}

fn bench_malloc(c: &mut Criterion) {
    // The M0 skeleton leaks on free, so a single shared heap would be exhausted by
    // a long run and we'd end up timing the OOM/null path. Measure each `malloc`
    // against a freshly reserved local heap instead: the bump allocator's cost is
    // independent of how full it is, so one allocation per fresh heap is
    // representative and never starves. A reclaiming steady-state bench arrives
    // with plan 08 W21-6.
    c.bench_function("malloc(64) [fresh skeleton]", |b| {
        b.iter_batched_ref(
            || new_allocator_named("posix", 16 * 1024).expect("reserve skeleton heap"),
            |a| black_box(a.malloc(black_box(64), MIN_ALIGN)),
            BatchSize::NumIterations(256),
        )
    });
}

criterion_group!(
    benches,
    bench_size_class,
    bench_classify,
    bench_classify_flags,
    bench_malloc
);
criterion_main!(benches);

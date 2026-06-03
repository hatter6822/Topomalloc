// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the hot classification/allocation path
//! (W0-8, §34.6). Non-gating: `xtask bench` runs and reports, but CI does not
//! fail on a regression yet (a results schema + workload replay arrive with
//! plan 08 W21-6). These exist so the harness is wired from M0.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use topo_abi::topomalloc_malloc;
use topo_core::{classify, size_class};

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

fn bench_c_abi_malloc(c: &mut Criterion) {
    c.bench_function("topomalloc_malloc(64)", |b| {
        b.iter(|| {
            let p = topomalloc_malloc(black_box(64));
            black_box(p);
        })
    });
}

criterion_group!(
    benches,
    bench_size_class,
    bench_classify,
    bench_c_abi_malloc
);
criterion_main!(benches);

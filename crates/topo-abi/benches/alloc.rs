// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the hot classification/allocation path
//! (W0-8, §34.6). Non-gating: `xtask bench` runs and reports, but CI does not
//! fail on a regression yet (a results schema + workload replay arrive with
//! plan 08 W21-6). These exist so the harness is wired from M0.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use topo_abi::{topomalloc_free, topomalloc_malloc, topomalloc_profile_set_rate};
use topo_core::{classify, size_class, RequestFlags};

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

fn bench_malloc_free(c: &mut Criterion) {
    // The W8 engine reclaims on free, so the natural steady-state benchmark is
    // a malloc/free round-trip against the process-wide allocator (the M1
    // central path under its locks; the M2/M3 caches will drop this number).
    c.bench_function("malloc(64)+free [central path]", |b| {
        b.iter(|| {
            let p = topomalloc_malloc(black_box(64));
            // SAFETY: `p` was just returned by `topomalloc_malloc` and is
            // owned by this iteration; freed exactly once.
            unsafe { topomalloc_free(black_box(p)) };
        })
    });
}

/// The W17-3 sampling overhead budget (§31.4 / DD-1 *F3*): the hot-path cost of the
/// sampling decision + the lock-free free-side membership reject, *enabled* vs *off*
/// (the default). The two lines should sit close together — sampling adds a thread-local
/// counter decrement on malloc and a couple of relaxed atomic loads on free, with the
/// capture/record work only on the rare fired sample. Non-gating; `xtask bench` reports it.
fn bench_sampling_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("sampling_overhead");
    // Baseline: sampling off (the default artifact's path).
    topomalloc_profile_set_rate(0);
    group.bench_function("malloc(64)+free [sampling off]", |b| {
        b.iter(|| {
            let p = topomalloc_malloc(black_box(64));
            // SAFETY: just-returned, owned, freed once.
            unsafe { topomalloc_free(black_box(p)) };
        })
    });
    // Enabled at a realistic 1 MiB mean interval (samples fire rarely vs. 64-byte allocs).
    topomalloc_profile_set_rate(1 << 20);
    group.bench_function("malloc(64)+free [sampling on, 1MiB]", |b| {
        b.iter(|| {
            let p = topomalloc_malloc(black_box(64));
            // SAFETY: just-returned, owned, freed once.
            unsafe { topomalloc_free(black_box(p)) };
        })
    });
    topomalloc_profile_set_rate(0); // restore the default
    group.finish();
}

criterion_group!(
    benches,
    bench_size_class,
    bench_classify,
    bench_classify_flags,
    bench_malloc_free,
    bench_sampling_overhead
);
criterion_main!(benches);

// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the W3 metadata paths (§34.6, plan 03 W3-3a).
//! Non-gating: `cargo xtask bench` runs and reports; a results schema + regression
//! gate arrive with plan 08 W21-6. These exist so the metadata overhead and lookup
//! latency the plan asks to be "bounded + documented" are *measured*, not asserted.
//!
//! Reported footprint (compile-time, 64-bit target): `SpanDescriptor` = 96 B,
//! `LargeDescriptor` = 56 B, each radix node = 8 KiB (1024 slots). The lazily-grown
//! node total is queryable at runtime via `PageMap::metadata_bytes()`.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::ids::{ArenaId, SizeClassId, SpanId};
use topo_core::pagemap::PageMap;
use topo_core::ptr_class::{classify_ptr, NoMetadata};
use topo_core::span::SpanDescriptor;

/// Leak a metadata arena live for the whole process (benches never tear down).
fn leak_arena(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

/// A pagemap with `n` single-page small spans installed, spread one leaf (16 MiB)
/// apart so the lookup exercises the full multi-level walk. Returns the map plus a
/// representative hit address and the bounded node overhead.
fn build(n: usize) -> (PageMap, usize, usize) {
    let meta = leak_arena(8 * 1024 * 1024);
    let mut spans = Vec::with_capacity(n);
    for i in 0..n {
        let base = 0x4000_0000usize + i * 16 * 1024 * 1024;
        spans.push(
            SpanDescriptor::new(
                SpanId(i as u32),
                ArenaId::DEFAULT,
                SizeClassId::new(0),
                base,
                1,
                64,
                0,
                meta,
            )
            .expect("bitmap"),
        );
    }
    let spans: &'static [SpanDescriptor] = Box::leak(spans.into_boxed_slice());
    let pm = PageMap::new();
    for s in spans {
        pm.install_span(meta, s).expect("install");
    }
    // A base pointer to object 0 of a middle span (a representative `Small` hit).
    let hit = spans[n / 2].object0_base().unwrap();
    let node_bytes = pm.metadata_bytes();
    (pm, hit, node_bytes)
}

fn bench_pagemap_lookup(c: &mut Criterion) {
    let (pm, hit, node_bytes) = build(64);
    // `eprintln` so the measured overhead is visible alongside the timings.
    eprintln!("pagemap: 64 spans installed, node overhead = {node_bytes} bytes");
    let miss = 0x7fff_dead_0000usize; // far from any installed span

    c.bench_function("pagemap_lookup_hit", |b| {
        b.iter(|| pm.lookup(black_box(hit)))
    });
    c.bench_function("pagemap_lookup_miss", |b| {
        b.iter(|| pm.lookup(black_box(miss)))
    });
}

fn bench_classify(c: &mut Criterion) {
    let (pm, hit, _) = build(64);
    let interior = hit + 4; // a mid-object pointer (interior)

    c.bench_function("classify_ptr_small_base", |b| {
        b.iter(|| classify_ptr(&pm, &NoMetadata, black_box(hit)))
    });
    c.bench_function("classify_ptr_interior", |b| {
        b.iter(|| classify_ptr(&pm, &NoMetadata, black_box(interior)))
    });
    c.bench_function("classify_ptr_foreign", |b| {
        b.iter(|| classify_ptr(&pm, &NoMetadata, black_box(0x7fff_dead_0000usize)))
    });
}

fn bench_install_warm(c: &mut Criterion) {
    // Re-installing a span over an already-mapped page is the steady-state cost: no
    // node creation, just the per-page release store (the common W5-5 path once the
    // radix is warm).
    let meta = leak_arena(2 * 1024 * 1024);
    let span: &'static SpanDescriptor = Box::leak(Box::new(
        SpanDescriptor::new(
            SpanId(0),
            ArenaId::DEFAULT,
            SizeClassId::new(0),
            0x4000_0000,
            2,
            64,
            0,
            meta,
        )
        .expect("bitmap"),
    ));
    let pm = PageMap::new();
    pm.install_span(meta, span).expect("warm the radix");
    assert_eq!(span.byte_len(), 2 * PAGE_SIZE);

    c.bench_function("pagemap_install_span_warm_2pages", |b| {
        b.iter(|| pm.install_span(black_box(meta), black_box(span)).unwrap())
    });
}

criterion_group!(
    benches,
    bench_pagemap_lookup,
    bench_classify,
    bench_install_warm
);
criterion_main!(benches);

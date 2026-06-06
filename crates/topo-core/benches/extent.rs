// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the W4 back-end extent paths (§18, plan 04
//! W4-2). Non-gating: `cargo xtask bench` runs and reports. These measure the
//! DD-1 "cheap coalescing / fast fit" claims — `carve` (best/first-fit + split),
//! `free` (coalesce), and `split`/`merge` round-trips — over the pure
//! [`ExtentMap`] bookkeeping (no provider, no syscalls), so the numbers reflect the
//! index data structure itself.
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{ExtentId, ExtentMap, ExtentState, Fit};

const PAGE: usize = PAGE_SIZE;

/// Leak a metadata arena live for the whole process (benches never tear down).
fn leak_arena(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

fn fresh_map(pages: usize) -> ExtentMap {
    ExtentMap::new(leak_arena(8 << 20), 0x1_0000_0000, pages * PAGE, 1 << 16).expect("map")
}

fn bench_extent(c: &mut Criterion) {
    // carve + free churn: each iteration carves a few extents (split off the free
    // pool) and frees them (coalesce back) — the common back-end refill/return path.
    c.bench_function("extent_carve_free_churn_bestfit", |b| {
        let mut m = fresh_map(4096);
        b.iter(|| {
            let mut ids = [ExtentId(0); 8];
            for (k, slot) in ids.iter_mut().enumerate() {
                *slot = m.carve((k % 4 + 1) * PAGE, PAGE, Fit::Best).expect("carve");
            }
            for id in ids {
                // Unbacked carves free to the unbacked free state; coalesce runs here.
                black_box(m.free(id, ExtentState::Released));
            }
        });
    });

    c.bench_function("extent_carve_free_churn_firstfit", |b| {
        let mut m = fresh_map(4096);
        b.iter(|| {
            let id = m.carve(2 * PAGE, PAGE, Fit::First).expect("carve");
            black_box(m.free(id, ExtentState::Released));
        });
    });

    // split + merge round-trip: the §18.4 geometric core (DD-1 boundary-tag).
    c.bench_function("extent_split_merge_roundtrip", |b| {
        let mut m = fresh_map(4096);
        let head = m.carve(8 * PAGE, PAGE, Fit::First).expect("carve");
        // Put it back as a single free extent to split/merge repeatedly.
        m.free(head, ExtentState::Released);
        b.iter(|| {
            // Re-carve the whole pool head, split it, then free both (merge).
            let left = m.carve(8 * PAGE, PAGE, Fit::First).expect("carve");
            m.free(left, ExtentState::Released);
            let l = m
                .carve(8 * PAGE, PAGE, Fit::First)
                .expect("carve for split");
            let right = m.split(l, 4 * PAGE).expect("split");
            black_box(m.merge(l, right));
            m.free(l, ExtentState::Released);
        });
    });
}

criterion_group!(benches, bench_extent);
criterion_main!(benches);

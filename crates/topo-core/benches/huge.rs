// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the W11 hugepage filler (§19, plan 04 W11).
//! Non-gating: `cargo xtask bench` runs and reports. These measure the DD-2 claims —
//! **packing-ordered placement with a bounded scan** (§19.3 "no full scan"), the
//! **bin re-file on occupancy change** (H-003), and **partial subrelease** (§19.6) —
//! over the pure [`HugePageFiller`] bookkeeping (no provider, no syscalls), so the
//! numbers reflect the placement/scoring data structure itself. A flat profile across
//! filler sizes is the evidence that the candidate scan is bounded, not O(hugepages).
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{Hotness, HugePageFiller, PlaceHints};

const PAGE: usize = PAGE_SIZE;

/// Leak a metadata arena live for the whole process (benches never tear down).
fn leak_arena(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

fn fresh_filler(capacity: usize) -> HugePageFiller {
    // Synthetic hugepage-aligned base — the filler does pure address bookkeeping.
    HugePageFiller::new(leak_arena(8 << 20), 0x1000_0000_0000, capacity).expect("filler")
}

/// Pre-fragment a filler: fill every hugepage to ~half occupancy with 4-page runs,
/// then free every other run, so each hugepage is `Sparse`/`Medium` with interleaved
/// holes — the worst case the placement scan must navigate.
fn fragmented_filler(capacity: usize) -> HugePageFiller {
    let mut f = fresh_filler(capacity);
    let mut live = Vec::new();
    // Fill to ~half across the region.
    for _ in 0..(capacity * 16) {
        if let Some(p) = f.place(4, PAGE, PlaceHints::default()) {
            f.mark_committed(&p);
            live.push((p.base, p.pages));
        }
    }
    // Punch holes (free every other run) to interleave free/live pages.
    for (i, &(base, pages)) in live.iter().enumerate() {
        if i % 2 == 0 {
            f.free(base, pages);
        }
    }
    f
}

fn bench_huge(c: &mut Criterion) {
    // place + free churn: place a run, confirm its commit, free it — the common
    // pack/unpack path. Balanced per iteration so the filler does not run out.
    c.bench_function("huge_place_free_churn", |b| {
        let mut f = fresh_filler(64);
        b.iter(|| {
            let p = f.place(8, PAGE, PlaceHints::default()).expect("place");
            f.mark_committed(&p);
            black_box(f.free(p.base, p.pages));
        });
    });

    // Placement into a fragmented region, swept across filler sizes: a flat curve is
    // the evidence that the candidate scan is bounded (SCAN_CAP), not O(hugepages).
    let mut g = c.benchmark_group("huge_place_into_fragmented");
    for &capacity in &[16usize, 128, 1024] {
        g.bench_with_input(
            BenchmarkId::from_parameter(capacity),
            &capacity,
            |b, &capacity| {
                let mut f = fragmented_filler(capacity);
                b.iter(|| {
                    // Place a small run into the fragmented state, then free it (keep
                    // the filler balanced so the scan always has the same shape).
                    let p = f
                        .place(2, PAGE, PlaceHints::hot(Hotness::Hot))
                        .expect("place");
                    f.mark_committed(&p);
                    black_box(f.free(p.base, p.pages));
                });
            },
        );
    }
    g.finish();

    // partial subrelease: place two halves, free one, subrelease the freed (sparse)
    // run, then re-place into it (recommit) — the §19.6 release/reuse cycle.
    c.bench_function("huge_subrelease_recommit_cycle", |b| {
        let mut f = fresh_filler(1);
        let keep = f
            .place(4, PAGE, PlaceHints::hot(Hotness::Cold))
            .expect("keep");
        f.mark_committed(&keep);
        b.iter(|| {
            let p = f
                .place(4, PAGE, PlaceHints::hot(Hotness::Cold))
                .expect("place");
            f.mark_committed(&p);
            f.free(p.base, p.pages);
            // Sparse hugepage ⇒ the cold free run subreleases.
            black_box(f.subrelease(p.base, p.pages, false));
        });
    });

    // whole-hugepage run reserve/free (the large/awkward path).
    c.bench_function("huge_reserve_free_run", |b| {
        let mut f = fresh_filler(8);
        b.iter(|| {
            let run = f.reserve_hugepages(2).expect("run");
            f.mark_run_committed(&run);
            black_box(f.free_hugepages(run.base, run.hugepages));
        });
    });
}

criterion_group!(benches, bench_huge);
criterion_main!(benches);

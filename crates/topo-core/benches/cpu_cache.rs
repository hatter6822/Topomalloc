// SPDX-License-Identifier: MIT
//! Criterion micro-benchmarks for the W6/W7 front-end fast path (§34.6, plan 05).
//! Non-gating: `cargo xtask bench` runs and reports. These exist so the central
//! promise of W7 — that the RSEQ restartable sequence is *faster* than the locked
//! per-CPU baseline while staying behaviourally identical — is **measured**, not
//! asserted, alongside the correctness/equivalence battery.
//!
//! The thread is pinned so the RSEQ path is active and the current CPU is stable.
//! Where RSEQ is unavailable (sandboxes, qemu-user) the "rseq" group transparently
//! runs the locked fallback, so the harness still reports numbers (they will simply
//! match the "locked" group there).
#![allow(missing_docs)] // criterion's generated harness fns have no public API

use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

use topo_core::bootstrap::BumpArena;
use topo_core::ids::{ArenaId, SizeClassId};
use topo_core::{size_class, CoreId, CpuCache};

const A: ArenaId = ArenaId::DEFAULT;

/// Leak a metadata arena live for the whole process (benches never tear down).
fn leak_arena(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

/// Pin the calling thread to `cpu`. Returns whether it succeeded.
fn pin_to(cpu: usize) -> bool {
    #[repr(C)]
    struct CpuSet {
        bits: [u64; 16],
    }
    extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const CpuSet) -> i32;
    }
    let mut set = CpuSet { bits: [0; 16] };
    set.bits[cpu / 64] |= 1u64 << (cpu % 64);
    // SAFETY: `set` is a valid cpu_set_t of the given size.
    unsafe { sched_setaffinity(0, core::mem::size_of::<CpuSet>(), &set) == 0 }
}

fn current_cpu() -> usize {
    extern "C" {
        fn sched_getcpu() -> i32;
    }
    // SAFETY: no arguments; reads kernel state only.
    let c = unsafe { sched_getcpu() };
    if c < 0 {
        0
    } else {
        c as usize
    }
}

/// Build a cache in the requested mode with `(core, sc)` pre-initialised to a
/// generous soft capacity (so push/pop pairs never hit the refill/flush slow path).
fn make_cache(rseq: bool, core: CoreId, sc: SizeClassId, m: &'static BumpArena) -> CpuCache {
    let cc = CpuCache::new();
    cc.set_active_cpus(1);
    if rseq {
        cc.enable_rseq();
        cc.register_current_thread();
    }
    let hard = size_class::max_local_capacity(sc) as u32;
    cc.init_slot(core, sc, m, hard);
    cc
}

fn bench_fast_path(c: &mut Criterion) {
    let m = leak_arena(8 * 1024 * 1024);
    let sc = SizeClassId::new(3);
    // Pin so the RSEQ path uses a stable CPU; use that CPU as the locked core too.
    let pinned = pin_to(current_cpu());
    let core = CoreId(current_cpu() as u32);

    let rseq_on = {
        let probe = CpuCache::new();
        probe.enable_rseq()
    };
    let mode_note = if rseq_on && pinned {
        "RSEQ active"
    } else {
        "locked fallback"
    };
    eprintln!("cpu_cache bench: rseq_on={rseq_on} pinned={pinned} ({mode_note})");

    // --- single push/pop round-trip (the hot path) ---
    let mut g = c.benchmark_group("push_pop_roundtrip");
    {
        let cc = make_cache(false, core, sc, m);
        g.bench_function("locked", |b| {
            b.iter(|| {
                let _ = black_box(cc.fe_push(core, A, sc, black_box(0xABCD), m));
                black_box(cc.fe_pop(core, A, sc, m))
            });
        });
    }
    {
        let cc = make_cache(true, core, sc, m);
        g.bench_function("rseq", |b| {
            b.iter(|| {
                let _ = black_box(cc.fe_push(core, A, sc, black_box(0xABCD), m));
                black_box(cc.fe_pop(core, A, sc, m))
            });
        });
    }
    g.finish();

    // --- a burst of pushes then pops (amortised, cache-resident) ---
    let burst = (size_class::batch(sc)).min(16);
    let mut g = c.benchmark_group("push_pop_burst");
    {
        let cc = make_cache(false, core, sc, m);
        g.bench_function("locked", |b| {
            b.iter_batched(
                || (),
                |_| {
                    for i in 0..burst {
                        let _ = cc.fe_push(core, A, sc, black_box(i + 1), m);
                    }
                    for _ in 0..burst {
                        black_box(cc.fe_pop(core, A, sc, m));
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    {
        let cc = make_cache(true, core, sc, m);
        g.bench_function("rseq", |b| {
            b.iter_batched(
                || (),
                |_| {
                    for i in 0..burst {
                        let _ = cc.fe_push(core, A, sc, black_box(i + 1), m);
                    }
                    for _ in 0..burst {
                        black_box(cc.fe_pop(core, A, sc, m));
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

criterion_group!(benches, bench_fast_path);
criterion_main!(benches);

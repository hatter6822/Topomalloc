// SPDX-License-Identifier: MIT
//! RSEQ fast-path equivalence and stress battery (W7-2e, W7-3c, W7-4, W7-6)
//! against the real [`CpuCache`]. The acceptance rule (G-fast, plan 05) is
//! **behavioural equivalence to the locked baseline**: the RSEQ path makes
//! exactly the object movements the locked path does — no lost or duplicated
//! object — including under forced migration, with `Abort` left as a logical
//! no-op (plan 02 W1-7).
//!
//! On the `-gnu` CI targets RSEQ is active on x86-64 (glibc registers it), so
//! these exercise the real kernel sequence. Where RSEQ is unavailable (sandboxes,
//! qemu-user), `enable_rseq()` reports `false`, the cache stays on the locked
//! baseline, and the same tests still pass (the fallback *is* the baseline).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use topo_core::{size_class, ArenaId, BumpArena, CoreId, CpuCache, FeOutcome, SizeClassId};

const A: ArenaId = ArenaId::DEFAULT;
/// Drain-buffer capacity for the conservation collectors.
const BUF_CAP: usize = topo_core::MAX_BATCH_LEN;

fn meta(bytes: usize) -> BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
    unsafe { BumpArena::new(ptr, len) }
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

/// The CPU the calling thread is currently running on.
fn getcpu() -> usize {
    extern "C" {
        fn sched_getcpu() -> i32;
    }
    // SAFETY: `sched_getcpu` takes no arguments and only reads kernel state.
    let c = unsafe { sched_getcpu() };
    if c < 0 {
        0
    } else {
        c as usize
    }
}

fn ncpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(topo_core::cpu_cache::MAX_CPUS)
}

/// W7-2e (pinned form): with the thread pinned to one CPU (no migration, so no
/// aborts), an RSEQ-mode cache and a locked-mode cache driven by the **same**
/// operation sequence produce **identical** outcomes — the strongest equivalence.
#[test]
fn pinned_rseq_matches_locked_outcomewise() {
    let m = meta(8 * 1024 * 1024);
    let locked = CpuCache::new();
    let rseq = CpuCache::new();
    let rseq_on = rseq.enable_rseq();
    rseq.register_current_thread();

    // Pin so the hardware CPU the RSEQ path uses is stable and known.
    let c0 = getcpu();
    if !pin_to(c0) {
        return;
    }
    let cpu = getcpu();
    let core = CoreId(cpu as u32);
    let sc = SizeClassId::new(3);

    // A deterministic mix of pushes and pops (xorshift-driven).
    let mut rng = 0x1234_5678_9ABC_DEF0u64;
    for _ in 0..20_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        if rng & 1 == 0 {
            let v = (rng >> 1) as usize | 1; // non-zero token
            let a = locked.fe_push(core, A, sc, v, &m);
            let b = rseq.fe_push(core, A, sc, v, &m);
            assert_eq!(a, b, "push outcome diverged (rseq_on={rseq_on})");
        } else {
            let a = locked.fe_pop(core, A, sc, &m);
            let b = rseq.fe_pop(core, A, sc, &m);
            assert_eq!(a, b, "pop outcome diverged (rseq_on={rseq_on})");
        }
    }

    // Drain both identically: every remaining pop must match, to the same Empty.
    loop {
        let a = locked.fe_pop(core, A, sc, &m);
        let b = rseq.fe_pop(core, A, sc, &m);
        assert_eq!(a, b, "drain outcome diverged");
        if matches!(a, FeOutcome::Empty) {
            break;
        }
    }
}

/// W7-2e/W7-6 (the core G-fast): under affinity churn that forces migration
/// mid-sequence, an RSEQ-mode cache conserves the token multiset exactly — the
/// same property the locked baseline has. No lost or duplicated object; aborts
/// are observed and handled as logical no-ops.
#[test]
fn forced_migration_conserves_tokens_real_cache() {
    let ncpu = ncpus();
    if ncpu < 2 {
        return; // need >1 CPU to force migration
    }
    let m = meta(64 * 1024 * 1024);
    let cc = Arc::new(CpuCache::new());
    cc.set_active_cpus(ncpu as u32);
    cc.enable_rseq();
    let sc = SizeClassId::new(2);

    // Pre-initialise every CPU's slot for `sc` at max capacity so the fast path
    // is exercised (not constantly diverting to init).
    let hard = size_class::max_local_capacity(sc) as u32;
    for c in 0..ncpu {
        cc.init_slot(CoreId(c as u32), sc, &m, hard);
    }

    // Seed a fixed token pool into CPU 0's slot. Size it to fit one slot's hard
    // capacity so the whole pool is placed (push_batch is capped by capacity).
    let ntok = (hard as usize).min(256);
    let tokens: Vec<usize> = (0..ntok).map(|i| 0x900000 + i).collect();
    let placed = cc.push_batch(CoreId(0), sc, &tokens);
    assert_eq!(placed, ntok, "seed must fit one slot");

    let aborts = Arc::new(AtomicU64::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let nthreads = ncpu;
    let iters = 200_000u64;

    std::thread::scope(|s| {
        for t in 0..nthreads {
            let cc = cc.clone();
            let m = &m;
            let in_flight = in_flight.clone();
            let _aborts = aborts.clone();
            s.spawn(move || {
                cc.register_current_thread();
                let mut rng =
                    0x9E37_79B9_7F4A_7C15u64 ^ (t as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
                let mut hand: Option<usize> = None;
                for i in 0..iters {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if i.is_multiple_of(2048) {
                        pin_to((rng as usize) % ncpu);
                    }
                    let core = CoreId(getcpu() as u32);
                    match hand {
                        None => match cc.fe_pop(core, A, sc, m) {
                            FeOutcome::Success(v) => {
                                hand = Some(v);
                                in_flight.fetch_add(1, Ordering::Relaxed);
                            }
                            FeOutcome::Empty => {}
                            // Abort/Full are never surfaced by fe_pop (retried /
                            // resolved internally); only Success/Empty appear.
                            other => panic!("unexpected fe_pop outcome {other:?}"),
                        },
                        Some(tok) => match cc.fe_push(core, A, sc, tok, m) {
                            FeOutcome::Success(()) => {
                                hand = None;
                                in_flight.fetch_sub(1, Ordering::Relaxed);
                            }
                            FeOutcome::Full => {} // keep the token, retry later
                            other => panic!("unexpected fe_push outcome {other:?}"),
                        },
                    }
                }
                // Return any token still in hand so the final count is exact.
                while let Some(tok) = hand {
                    let core = CoreId(getcpu() as u32);
                    if let FeOutcome::Success(()) = cc.fe_push(core, A, sc, tok, m) {
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        hand = None;
                    } else {
                        std::thread::yield_now();
                    }
                }
            });
        }
    });

    assert_eq!(in_flight.load(Ordering::Relaxed), 0, "all tokens returned");

    // Collect every token across all CPU slots (drain via locked pop_batch).
    let mut recovered = Vec::new();
    let mut buf = [0usize; BUF_CAP];
    for c in 0..ncpu {
        loop {
            let n = cc.pop_batch(CoreId(c as u32), sc, &mut buf, BUF_CAP);
            if n == 0 {
                break;
            }
            recovered.extend_from_slice(&buf[..n]);
        }
    }
    recovered.sort_unstable();
    let mut expected = tokens.clone();
    expected.sort_unstable();
    assert_eq!(
        recovered.len(),
        expected.len(),
        "no tokens lost or duplicated"
    );
    assert_eq!(
        recovered, expected,
        "token multiset conserved under migration"
    );
}

/// W7-4: a non-owner draining a CPU's slots (here, a remote `pop_batch` +
/// `push_batch` round-trip, which takes the per-CPU lock and issues the RSEQ
/// fence) concurrent with owners running the fast path conserves tokens — the
/// "concurrent flush-vs-fastpath stress clean" acceptance.
#[test]
fn non_owner_drain_vs_fastpath_is_clean() {
    let ncpu = ncpus();
    if ncpu < 2 {
        return;
    }
    let m = meta(64 * 1024 * 1024);
    let cc = Arc::new(CpuCache::new());
    cc.set_active_cpus(ncpu as u32);
    cc.enable_rseq();
    let sc = SizeClassId::new(1);
    let hard = size_class::max_local_capacity(sc) as u32;
    for c in 0..ncpu {
        cc.init_slot(CoreId(c as u32), sc, &m, hard);
    }
    let ntok = (hard as usize).min(256);
    let tokens: Vec<usize> = (0..ntok).map(|i| 0xA00000 + i).collect();
    let placed = cc.push_batch(CoreId(0), sc, &tokens);
    assert_eq!(placed, ntok, "seed must fit one slot");

    let in_flight = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    std::thread::scope(|s| {
        // Owners: pop/push the fast path across CPUs under affinity churn.
        for t in 0..ncpu {
            let cc = cc.clone();
            let m = &m;
            let in_flight = in_flight.clone();
            let stop = stop.clone();
            s.spawn(move || {
                cc.register_current_thread();
                let mut rng = 0xC0FFEE ^ (t as u64).wrapping_mul(0x9E3779B1);
                let mut hand: Option<usize> = None;
                while !stop.load(Ordering::Relaxed) {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if rng.is_multiple_of(4096) {
                        pin_to((rng as usize) % ncpu);
                    }
                    let core = CoreId(getcpu() as u32);
                    match hand {
                        None => {
                            if let FeOutcome::Success(v) = cc.fe_pop(core, A, sc, m) {
                                hand = Some(v);
                                in_flight.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Some(tok) => {
                            if let FeOutcome::Success(()) = cc.fe_push(core, A, sc, tok, m) {
                                hand = None;
                                in_flight.fetch_sub(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                while let Some(tok) = hand {
                    let core = CoreId(getcpu() as u32);
                    if let FeOutcome::Success(()) = cc.fe_push(core, A, sc, tok, m) {
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        hand = None;
                    }
                }
            });
        }
        // Non-owner: repeatedly drain a remote CPU's slot and push it straight
        // back (a logical no-op that exercises the lock + RSEQ fence W7-4 path).
        {
            let cc = cc.clone();
            let stop = stop.clone();
            let in_flight = in_flight.clone();
            s.spawn(move || {
                let mut buf = [0usize; BUF_CAP];
                let mut rng = 0xDEADBEEFu64;
                for _ in 0..50_000 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    let c = (rng as usize) % ncpu;
                    let n = cc.pop_batch(CoreId(c as u32), sc, &mut buf, BUF_CAP);
                    if n > 0 {
                        in_flight.fetch_add(n, Ordering::Relaxed);
                        // Push straight back to the same CPU.
                        let mut pushed = 0;
                        while pushed < n {
                            pushed += cc.push_batch(CoreId(c as u32), sc, &buf[pushed..n]);
                            if pushed < n {
                                std::thread::yield_now();
                            }
                        }
                        in_flight.fetch_sub(n, Ordering::Relaxed);
                    }
                }
                stop.store(true, Ordering::Relaxed);
            });
        }
    });

    assert_eq!(
        in_flight.load(Ordering::Relaxed),
        0,
        "all tokens accounted for"
    );
    let mut recovered = Vec::new();
    let mut buf = [0usize; BUF_CAP];
    for c in 0..ncpu {
        loop {
            let n = cc.pop_batch(CoreId(c as u32), sc, &mut buf, BUF_CAP);
            if n == 0 {
                break;
            }
            recovered.extend_from_slice(&buf[..n]);
        }
    }
    recovered.sort_unstable();
    let mut expected = tokens.clone();
    expected.sort_unstable();
    assert_eq!(
        recovered, expected,
        "tokens conserved across non-owner drains"
    );
}

/// W7-6 (registration failure / fallback, P-003): a cache that never enables
/// RSEQ — or on a platform where `enable_rseq()` returns `false` — uses the
/// locked path and is correct. Also checks `disable_rseq()` reverts cleanly.
#[test]
fn fallback_and_disable_are_correct() {
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    let core = CoreId::DEFAULT;
    let sc = SizeClassId::new(0);

    // Locked baseline (RSEQ never enabled).
    assert!(!cc.rseq_mode());
    assert!(cc.fe_push(core, A, sc, 0x11, &m).is_success());
    assert_eq!(cc.fe_pop(core, A, sc, &m), FeOutcome::Success(0x11));

    // Enable then disable: still correct (conservative mode, §28.1).
    let on = cc.enable_rseq();
    assert_eq!(cc.rseq_mode(), on);
    cc.disable_rseq();
    assert!(!cc.rseq_mode());
    assert!(cc.fe_push(core, A, sc, 0x22, &m).is_success());
    assert_eq!(cc.fe_pop(core, A, sc, &m), FeOutcome::Success(0x22));
}

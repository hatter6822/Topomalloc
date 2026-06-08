// SPDX-License-Identifier: MIT
//! seLe4n pinned-thread per-core fast-path tests (W7-5, §36.10 option 1).
//!
//! Exercises the per-core push/pop behind the shared [`FeOutcome`] contract on
//! the host (seLe4n itself is not available here): the success path with a
//! perfectly-pinned thread, the **abort/no-change** path when a migration is
//! simulated (mirroring `per_core_cache_abort_no_change`, plan 02 W1-12d), and
//! the migration flush/hand-off.

use core::sync::atomic::{AtomicU32, Ordering};

use topo_core::{
    size_class, ArenaId, BumpArena, CoreId, CoreProvider, CpuCache, FeOutcome, FixedCore,
    SizeClassId,
};

const A: ArenaId = ArenaId::DEFAULT;

fn meta(bytes: usize) -> BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
    unsafe { BumpArena::new(ptr, len) }
}

/// A provider that reports `expected` for the first `migrate_at` calls and
/// `other` thereafter — simulates a thread migrating off its pinned core after
/// a chosen number of core reads. `migrate_at = 0` migrates before the very
/// first read (abort at entry); `migrate_at = 1` migrates between the read and
/// the commit (abort-no-change mid-sequence).
struct MigrateOnCall {
    expected: CoreId,
    other: CoreId,
    migrate_at: u32,
    count: AtomicU32,
}

impl CoreProvider for MigrateOnCall {
    fn current_core(&self) -> CoreId {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n < self.migrate_at {
            self.expected
        } else {
            self.other
        }
    }
}

#[test]
fn pinned_success_round_trip_lifo() {
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    let core = CoreId(2);
    let sc = SizeClassId::new(3);
    let p = FixedCore(core);

    assert_eq!(cc.fe_pop_pinned(core, A, sc, &p, &m), FeOutcome::Empty);
    assert!(cc.fe_push_pinned(core, A, sc, 0xC0DE, &p, &m).is_success());
    assert!(cc.fe_push_pinned(core, A, sc, 0xF00D, &p, &m).is_success());
    // LIFO.
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &p, &m),
        FeOutcome::Success(0xF00D)
    );
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &p, &m),
        FeOutcome::Success(0xC0DE)
    );
    assert_eq!(cc.fe_pop_pinned(core, A, sc, &p, &m), FeOutcome::Empty);
}

#[test]
fn pinned_pop_abort_leaves_state_unchanged() {
    // The Lean `per_core_cache_abort_no_change` obligation, pop side.
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    let core = CoreId(1);
    let sc = SizeClassId::new(0);
    let pin = FixedCore(core);

    // Seed one token via the success path.
    assert!(cc
        .fe_push_pinned(core, A, sc, 0xABCD, &pin, &m)
        .is_success());
    let len_before = cc.per_cpu(core).unwrap().slot(sc).unwrap().len();
    assert_eq!(len_before, 1);

    // Migrate *between* the read and the commit (call 0 = entry passes, call 1 =
    // pre-commit sees a different core) → Abort, and the token is NOT popped.
    let migrating = MigrateOnCall {
        expected: core,
        other: CoreId(0),
        migrate_at: 1,
        count: AtomicU32::new(0),
    };
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &migrating, &m),
        FeOutcome::Abort
    );
    let len_after = cc.per_cpu(core).unwrap().slot(sc).unwrap().len();
    assert_eq!(len_after, 1, "aborted pop must leave the slot unchanged");

    // Abort at entry (migrate_at = 0): also no change.
    let migrating0 = MigrateOnCall {
        expected: core,
        other: CoreId(0),
        migrate_at: 0,
        count: AtomicU32::new(0),
    };
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &migrating0, &m),
        FeOutcome::Abort
    );
    assert_eq!(cc.per_cpu(core).unwrap().slot(sc).unwrap().len(), 1);

    // The token is still poppable on the pinned core.
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &pin, &m),
        FeOutcome::Success(0xABCD)
    );
}

#[test]
fn pinned_push_abort_leaves_state_unchanged() {
    // Push side of the abort/no-change obligation.
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    let core = CoreId(1);
    let sc = SizeClassId::new(0);
    let pin = FixedCore(core);

    // Initialise the (empty) slot via a success-path pop.
    assert_eq!(cc.fe_pop_pinned(core, A, sc, &pin, &m), FeOutcome::Empty);
    assert_eq!(cc.per_cpu(core).unwrap().slot(sc).unwrap().len(), 0);

    // Migrate between staging and commit → Abort, nothing published.
    let migrating = MigrateOnCall {
        expected: core,
        other: CoreId(0),
        migrate_at: 1,
        count: AtomicU32::new(0),
    };
    assert_eq!(
        cc.fe_push_pinned(core, A, sc, 0xBEEF, &migrating, &m),
        FeOutcome::Abort
    );
    assert_eq!(
        cc.per_cpu(core).unwrap().slot(sc).unwrap().len(),
        0,
        "aborted push must not publish the object"
    );

    // A real push then succeeds and the earlier staged value is overwritten.
    assert!(cc
        .fe_push_pinned(core, A, sc, 0x1234, &pin, &m)
        .is_success());
    assert_eq!(
        cc.fe_pop_pinned(core, A, sc, &pin, &m),
        FeOutcome::Success(0x1234)
    );
}

#[test]
fn pinned_matches_locked_on_success_path() {
    // W7-5 equivalence: with a perfectly-pinned provider, the per-core path makes
    // the same object movements as the locked baseline.
    let m = meta(4 * 1024 * 1024);
    let locked = CpuCache::new();
    let pinned = CpuCache::new();
    let core = CoreId(0);
    let sc = SizeClassId::new(4);
    let p = FixedCore(core);

    let mut rng = 0xFEED_FACE_DEAD_BEEFu64;
    for _ in 0..10_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        if rng & 1 == 0 {
            let v = (rng >> 1) as usize | 1;
            let a = locked.fe_push(core, A, sc, v, &m);
            let b = pinned.fe_push_pinned(core, A, sc, v, &p, &m);
            assert_eq!(a, b);
        } else {
            let a = locked.fe_pop(core, A, sc, &m);
            let b = pinned.fe_pop_pinned(core, A, sc, &p, &m);
            assert_eq!(a, b);
        }
    }
}

#[test]
fn migration_flush_handoff_empties_the_core() {
    // §36.10: a cache MUST be flushed or made unreachable before core ownership
    // changes. Model the hand-off as a drain; the new owner sees an empty slot.
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    let core = CoreId(3);
    let sc = SizeClassId::new(2);
    let p = FixedCore(core);
    let hard = size_class::max_local_capacity(sc) as u32;
    cc.init_slot(core, sc, &m, hard);

    for i in 0..8 {
        assert!(cc
            .fe_push_pinned(core, A, sc, 0xD000 + i, &p, &m)
            .is_success());
    }
    // Hand-off: drain the slot before affinity changes.
    let mut buf = [0usize; 64];
    let mut drained = 0;
    loop {
        let n = cc.pop_batch(core, sc, &mut buf, 64);
        if n == 0 {
            break;
        }
        drained += n;
    }
    assert_eq!(drained, 8, "hand-off drains every cached object");
    // The new owner on this core sees an empty cache.
    assert_eq!(cc.fe_pop_pinned(core, A, sc, &p, &m), FeOutcome::Empty);
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

/// The pinned-core oracle: the OS view of the current core.
fn getcpu_oracle() -> i32 {
    extern "C" {
        fn sched_getcpu() -> i32;
    }
    // SAFETY: no arguments; reads kernel state only.
    unsafe { sched_getcpu() }
}

#[test]
fn pinned_mode_dispatch_routes_fe_pop_push() {
    // W7-5 integration: enable_pinned_core makes the shared fe_pop/fe_push entry
    // points run the pinned sequence (behind the same FeOutcome contract).
    let m = meta(2 * 1024 * 1024);
    let cc = CpuCache::new();
    cc.enable_pinned_core(getcpu_oracle);
    assert!(cc.pinned_mode());
    assert!(!cc.rseq_mode());

    let cpu = getcpu_oracle();
    if cpu < 0 || !pin_to(cpu as usize) {
        return;
    }
    // The `core` argument is a hint in pinned mode (the oracle is authoritative).
    let core = CoreId(getcpu_oracle() as u32);
    let sc = SizeClassId::new(2);

    assert_eq!(cc.fe_pop(core, A, sc, &m), FeOutcome::Empty);
    for i in 0..5 {
        assert!(
            cc.fe_push(core, A, sc, 0x500 + i, &m).is_success(),
            "push {i}"
        );
    }
    for i in (0..5).rev() {
        assert_eq!(cc.fe_pop(core, A, sc, &m), FeOutcome::Success(0x500 + i));
    }
    assert_eq!(cc.fe_pop(core, A, sc, &m), FeOutcome::Empty);
}

#[test]
fn pinned_dispatch_matches_locked() {
    // The dispatched pinned path makes the same moves as the locked baseline.
    let m = meta(4 * 1024 * 1024);
    let locked = CpuCache::new();
    let pinned = CpuCache::new();
    pinned.enable_pinned_core(getcpu_oracle);

    let cpu = getcpu_oracle();
    if cpu < 0 || !pin_to(cpu as usize) {
        return;
    }
    let core = CoreId(getcpu_oracle() as u32);
    let sc = SizeClassId::new(1);

    let mut rng = 0x0BAD_F00D_DEAD_C0DEu64;
    for _ in 0..10_000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        if rng & 1 == 0 {
            let v = (rng >> 1) as usize | 1;
            assert_eq!(
                locked.fe_push(core, A, sc, v, &m),
                pinned.fe_push(core, A, sc, v, &m)
            );
        } else {
            assert_eq!(
                locked.fe_pop(core, A, sc, &m),
                pinned.fe_pop(core, A, sc, &m)
            );
        }
    }
}

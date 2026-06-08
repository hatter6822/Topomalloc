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

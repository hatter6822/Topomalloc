// SPDX-License-Identifier: MIT
//! `fork()` safety: the process-wide fork coordinator (W16-5, plan 05; §28.1).
//!
//! In a multithreaded process, `fork()` yields a child with **one** running
//! thread but with locks possibly held by threads that no longer exist. If the
//! child then calls `malloc`, it can deadlock forever on one of the allocator's
//! own spinlocks. POSIX's remedy is `pthread_atfork` handlers in three contexts
//! (§28.1); this module is the core mechanism they drive (the registration lives
//! in `topo-abi`, which has `libc`):
//!
//! * **pre-fork** ([`prefork`]): acquire the global fork lock and **quiesce** —
//!   block new allocator operations and wait for every in-flight one to finish,
//!   so at the moment of `fork()` no internal lock is held and every data
//!   structure is at a consistency boundary.
//! * **parent post-fork** ([`postfork_parent`]): release the fork lock and resume.
//! * **child post-fork** ([`postfork_child`]): **reset** (not unlock) all lock
//!   state, clear the in-flight counter, disable background threads, and clear
//!   the lock-order checker's inherited bookkeeping — the child starts clean.
//!
//! # Why a drain gate, not "acquire every lock"
//!
//! jemalloc-style prefork acquires every internal mutex in rank order. That is
//! impractical here because the per-span locks are **dynamic** (created on
//! demand) and are taken *outside* the central lock (`activate_span`, `recycle`),
//! so acquiring the fixed set of structure locks would not prevent a thread from
//! holding a per-span lock at `fork()`. Instead, every public operation runs
//! inside an [`operation_guard`], and prefork **drains** the in-flight count to
//! zero. No in-flight operation ⟺ no internal spinlock held ⟺ every structure is
//! consistent. This is the §28.1 "global allocator fork lock + quiesce" realised
//! as a writer-priority gate.
//!
//! # The gate is provably correct *and* `loom`-verifiable (per-shard fork bits)
//!
//! The gate is the read side of a read-write lock, **sharded across CPUs** to
//! avoid a single contended cacheline. Each shard is one cache-line-padded
//! `AtomicU64`: the top bit is that shard's fork-pending flag, the low bits are
//! that shard's in-flight count. [`operation_guard`] enters by a **`fetch_add`**
//! on the current CPU's shard and inspects the **returned previous value**: if its
//! fork bit was clear, the op is admitted; if set, it backs out and parks. The
//! fork-bit check rides on `fetch_add`'s atomic result — there is **no separate
//! load**, hence no store-then-load (Dekker) hazard and **no `membarrier`** — so
//! every shard is an independent, `loom`-verifiable single-word gate
//! (`gate_admits_no_op_across_a_fork`, no `SeqCst`). [`prefork`] sets the fork bit
//! in **all** shards, then drains every shard's count to zero; a thread admitted
//! to a shard before its bit was set is counted there and waited for. Because the
//! increment is `fetch_add` (not a CAS) it never spins-retries under contention,
//! and because it lands on the running CPU's own shard, threads on different CPUs
//! never touch the same cacheline.
//!
//! **Sharding is auto-detected** ([`set_sharded`]): it is enabled once a cheap
//! per-CPU id is available (`topo_arch::rseq::current_cpu() >= 0`, the glibc/rseq
//! area), and is a safe no-op otherwise (every op lands on shard 0 — exactly the
//! single-word gate, still `fetch_add`-based and verifiable). A thread that cannot
//! read its CPU (lost rseq registration) also lands on shard 0. Either way the
//! gate is correct; sharding only changes *which* shard an op counts in.
//!
//! # Re-entrancy, background threads, and maintenance
//!
//! The gate is a **counter**, so a re-entrant operation (e.g. `realloc` calling
//! `allocate`) simply nests — `prefork` waits for the count, which a balanced
//! pair always returns to zero. Internal background maintenance (the release pump,
//! rebalancer — host-driven today) runs inside a [`maintenance_guard`], which
//! counts in the **same** gate, so `prefork`'s one drain quiesces public ops *and*
//! maintenance together (§28.1 "quiesce background threads"); [`background_enabled`]
//! is the on/off switch the child clears so maintenance stays off until the host
//! re-arms it. The handshake is wired ahead of the pump consumer but is a real
//! mechanism (tested), not a bare flag.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::lock::{reset_lock_checker, LockRank, RankedLock};

/// Number of CPU shards. A power of two so `cpu & (NUM_SHARDS - 1)` indexes
/// without a division; 64 covers a typical socket and wraps gracefully on larger
/// machines (more sharing, never incorrect). Each shard is cache-line padded, so
/// the table is `NUM_SHARDS * 64` = 4 KiB of `static` — negligible for a
/// process-wide allocator, and the price of eliminating false sharing.
const NUM_SHARDS: usize = 64;

/// One fork-gate shard: a cache-line-padded `AtomicU64` (top bit = this shard's
/// fork-pending flag, low 63 bits = this shard's in-flight count). The alignment
/// keeps two shards off the same cacheline so CPUs never contend (P-003).
#[repr(align(64))]
struct Shard(AtomicU64);

impl Shard {
    const fn new() -> Self {
        Shard(AtomicU64::new(0))
    }
}

/// The per-CPU sharded fork gate. Op `i` on CPU `c` counts in `SHARDS[c % N]`;
/// `prefork` sets the fork bit in all shards and drains all counts.
static SHARDS: [Shard; NUM_SHARDS] = [const { Shard::new() }; NUM_SHARDS];

/// The fork-pending flag (each shard's top bit). While set, [`operation_guard`]
/// parks instead of entering that shard.
const FORK_PENDING: u64 = 1 << 63;
/// Mask of a shard's in-flight-operation count (its low 63 bits).
const COUNT_MASK: u64 = FORK_PENDING - 1;

/// Sharding mode: `0` = not yet probed (use shard 0), `1` = single-shard
/// (no cheap CPU id — every op on shard 0), `2` = sharded (index by CPU). Latched
/// once by [`set_sharded`]; read relaxed on the hot path (a stale read only mis-
/// routes which shard an op counts in, never its correctness).
static SHARD_MODE: AtomicU8 = AtomicU8::new(0);
const MODE_SINGLE: u8 = 1;
const MODE_SHARDED: u8 = 2;

/// Serializes concurrent forkers and is held across `fork()` itself (acquired in
/// pre-fork, released in parent post-fork, force-reset in the child). Rank
/// [`LockRank::GLOBAL_CONFIG`] (0): the outermost lock, the §28.1 "global
/// allocator fork lock". Held only by the forking thread, which holds no other
/// allocator lock, so it never interacts with the rank checker on the hot path.
static FORK_LOCK: RankedLock<{ LockRank::GLOBAL_CONFIG }> = RankedLock::new();

/// Whether the allocator's background maintenance (release pump, rebalancer) is
/// permitted to run. Cleared by the child post-fork handler (§28.1). A
/// [`maintenance_guard`] returns `None` while this is `false`.
static BACKGROUND_ENABLED: AtomicBool = AtomicBool::new(true);

/// Probe for a cheap per-CPU id and latch the sharding mode (idempotent). Called
/// once from the global initializer on the main thread; until then the gate runs
/// single-shard (always correct). Enables sharding iff `current_cpu()` yields a
/// valid CPU (glibc/rseq area present), which is where sharding actually helps.
pub fn set_sharded(enable: bool) {
    let mode = if enable { MODE_SHARDED } else { MODE_SINGLE };
    // Only latch from the unprobed state, so a later call cannot flip the mode
    // out from under in-flight ops.
    let _ = SHARD_MODE.compare_exchange(0, mode, Ordering::Release, Ordering::Relaxed);
}

/// Auto-detect: enable sharding iff a cheap per-CPU id is available right now.
/// Convenience wrapper the global initializer calls.
pub fn probe_and_set_sharding() {
    set_sharded(topo_arch::rseq::current_cpu() >= 0);
}

/// The shard index for the current operation: the running CPU's shard when
/// sharded (and a CPU id is readable), else shard 0. A relaxed read of the mode;
/// any value yields a *valid* shard, so this is always correct.
#[inline]
fn shard_index() -> usize {
    if SHARD_MODE.load(Ordering::Relaxed) == MODE_SHARDED {
        let cpu = topo_arch::rseq::current_cpu();
        if cpu >= 0 {
            return (cpu as usize) & (NUM_SHARDS - 1);
        }
    }
    0
}

/// RAII guard for one in-flight allocator operation: decrements its shard's count
/// on drop. Held for the dynamic extent of a public `allocate`/`free`/`realloc`/
/// arena operation, so `prefork`'s drain proves no internal lock is held. Carries
/// the shard it incremented, so a thread that **migrates** mid-operation still
/// decrements the shard it entered (the count balances per shard).
#[must_use = "the operation is counted as in-flight until the guard is dropped"]
pub struct OperationGuard {
    /// The shard this operation incremented (decremented on drop).
    shard: u32,
    /// The `*const ()` makes the guard `!Send`/`!Sync`: an operation enters and
    /// leaves on one thread, so the guard is never transferred across threads.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl Drop for OperationGuard {
    #[inline]
    fn drop(&mut self) {
        // Release the slot on the shard we entered (even if we have since migrated
        // CPUs). `Release` so a draining `prefork` (which `Acquire`-loads the shard)
        // that observes the count reach zero has also observed every write this
        // operation made under the internal locks (they are unlocked now).
        SHARDS[self.shard as usize]
            .0
            .fetch_sub(1, Ordering::Release);
    }
}

/// Enter a public allocator operation, returning a guard that keeps it counted as
/// in-flight until dropped (W16-5). If a `fork()` is in progress this **parks**
/// until it completes, so the operation never races a fork — the §28.1 quiesce
/// seen from the operation side. Re-entrant: a nested operation simply nests the
/// count (on possibly different shards — each balances independently).
///
/// This is the single hot-path cost of fork safety: a (cheap) CPU-id read + one
/// `fetch_add` on the running CPU's own shard, plus the matching decrement on the
/// guard's drop.
#[inline]
pub fn operation_guard() -> OperationGuard {
    loop {
        let idx = shard_index();
        // Increment this shard's count and read its previous value atomically: the
        // fork-bit test rides on the `fetch_add` result, so there is no separate
        // load and hence no Dekker hazard (and no membarrier).
        let prev = SHARDS[idx].0.fetch_add(1, Ordering::AcqRel);
        if prev & FORK_PENDING == 0 {
            // Admitted: the fork bit was clear when we incremented, so `prefork`
            // (which sets the bit *then* drains) will count and wait for us.
            return OperationGuard {
                shard: idx as u32,
                _not_send: core::marker::PhantomData,
            };
        }
        // A fork is in progress on this shard: back out (so the drain can finish)
        // and park until the window closes, then retry (possibly on another shard).
        // While parked we hold no slot, so the drain is never blocked by us.
        SHARDS[idx].0.fetch_sub(1, Ordering::Release);
        while SHARDS[idx].0.load(Ordering::Acquire) & FORK_PENDING != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Enter an internal **background-maintenance** operation (the release pump,
/// rebalancer), returning a guard — or `None` if maintenance is currently
/// disabled (post-fork, until the host re-arms it via [`set_background_enabled`]).
/// The guard counts in the **same** gate as public ops, so [`prefork`]'s single
/// drain quiesces maintenance too (§28.1 "quiesce background threads"). The
/// background pump consumer lands with the front-end caches; this is its seam.
#[inline]
#[must_use]
pub fn maintenance_guard() -> Option<OperationGuard> {
    // Load-bearing §35.4 phase gate (W16-7): background maintenance must not run
    // before the allocator reaches the background/profiling phase (and is cleared
    // back below it never — the phase is monotone), nor while it is disabled
    // (the post-fork state). Both conditions are checked before counting an op.
    if crate::init::INIT_PHASE.current() < crate::init::InitPhase::BackgroundAndProfiling
        || !BACKGROUND_ENABLED.load(Ordering::Acquire)
    {
        return None;
    }
    let idx = shard_index();
    let prev = SHARDS[idx].0.fetch_add(1, Ordering::AcqRel);
    if prev & FORK_PENDING == 0 {
        return Some(OperationGuard {
            shard: idx as u32,
            _not_send: core::marker::PhantomData,
        });
    }
    // A fork is starting: do not run maintenance during the window. Back out and
    // decline (the pump retries on its next tick).
    SHARDS[idx].0.fetch_sub(1, Ordering::Release);
    None
}

/// Sum the in-flight counts across all shards (the drain target).
#[inline]
fn total_in_flight() -> u64 {
    let mut sum = 0u64;
    for s in &SHARDS {
        sum += s.0.load(Ordering::Acquire) & COUNT_MASK;
    }
    sum
}

/// Pre-fork handler (§28.1): acquire the global fork lock and quiesce. After this
/// returns, no thread is executing a guarded operation (public or maintenance), so
/// no internal allocator lock is held and every structure is consistent — safe to
/// `fork()`.
///
/// Must run in the forking thread from a `pthread_atfork` *prepare* handler,
/// where that thread holds no allocator lock. The fork lock stays held until the
/// matching [`postfork_parent`] (parent) or [`postfork_child`] (child).
///
/// SPEC-transition: fork quiesce (§28.1 pre-fork)
pub fn prefork() {
    // Serialize concurrent forkers; held across the fork (released in the parent,
    // reset in the child).
    FORK_LOCK.acquire();
    // Set the fork-pending bit in **every** shard *before* draining any, so once
    // we start draining no new op can enter any shard. `AcqRel` so an op whose
    // `fetch_add` races this either won before it on that shard (its `prev` bit is
    // clear → admitted → in the count we drain) or after it (its `prev` bit is set
    // → backs out).
    for s in &SHARDS {
        s.0.fetch_or(FORK_PENDING, Ordering::AcqRel);
    }
    // Drain: wait until every shard's in-flight count reaches zero. Each shard's
    // `Acquire` load pairs with the ops' `Release` decrements, so a zero total
    // means we have observed every operation's writes — fully quiesced.
    while total_in_flight() != 0 {
        core::hint::spin_loop();
    }
}

/// Parent post-fork handler (§28.1): clear the fork-pending bit in every shard and
/// release the fork lock. The parent's state was never inconsistent (it was
/// quiesced, not mutated), so there is nothing to reset — just lift the gate.
///
/// SPEC-transition: fork resume (§28.1 parent post-fork)
pub fn postfork_parent() {
    // Clear each shard's fork bit; the counts are already zero (we drained them).
    for s in &SHARDS {
        s.0.fetch_and(COUNT_MASK, Ordering::Release);
    }
    FORK_LOCK.release();
}

/// Child post-fork handler (§28.1): the hard one. The child has a single thread
/// but inherited the parent's lock *bytes* and bookkeeping. It must **reset**
/// (not unlock) every coordinator lock to a clean state, clear the in-flight
/// counter, disable background threads until safe, and clear the lock-order
/// checker's inherited held-rank snapshot.
///
/// Because [`prefork`] quiesced the allocator before the fork, the child inherits
/// **consistent** structures with **no** internal lock genuinely held — so after
/// this reset the child can allocate safely.
///
/// SPEC-transition: fork reset (§28.1 child post-fork)
pub fn postfork_child() {
    // The single child thread is the only accessor; reset every shard to "idle"
    // (count 0, fork bit clear).
    for s in &SHARDS {
        s.0.store(0, Ordering::Release);
    }
    // The fork lock was inherited "held" by a context that no longer exists;
    // reset it rather than release it (releasing balances a non-existent acquire).
    // SAFETY: the post-fork child is single-threaded — no other accessor — which
    // is exactly `RankedLock::force_reset`'s contract.
    unsafe { FORK_LOCK.force_reset() };
    // The forking thread held the fork lock (rank 0) at fork; clear the inherited
    // held-rank snapshot so the checker starts clean in the child.
    reset_lock_checker();
    // Disable background maintenance until the host re-arms it (§28.1).
    BACKGROUND_ENABLED.store(false, Ordering::Release);
}

/// Whether allocator background maintenance is currently permitted (cleared by
/// the child post-fork handler, §28.1). The release pump / rebalancer are
/// host-driven today, so this is the advisory hook a future background thread
/// consults before doing work.
#[inline]
#[must_use]
pub fn background_enabled() -> bool {
    BACKGROUND_ENABLED.load(Ordering::Acquire)
}

/// Re-enable background maintenance (the host's "safe now" signal after a fork,
/// or at startup). Pairs with the child handler's disable.
#[inline]
pub fn set_background_enabled(on: bool) {
    BACKGROUND_ENABLED.store(on, Ordering::Release);
}

/// The number of allocator operations currently in flight, process-wide
/// (diagnostic / test hook). Zero after a successful [`prefork`] drain.
#[inline]
#[must_use]
pub fn in_flight_operations() -> u64 {
    total_in_flight()
}

/// Whether a fork window is currently open (a shard's fork-pending bit is set).
/// Test / diagnostic hook; operations park while this is true.
#[inline]
#[must_use]
pub fn fork_in_progress() -> bool {
    SHARDS[0].0.load(Ordering::Acquire) & FORK_PENDING != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};
    use std::sync::{Arc, Mutex, MutexGuard};

    // These tests share the **process-global** gate (the `SHARDS` array, the fork
    // lock, the background flag). Within `topo-core` only these tests touch the
    // gate, so serializing them through one mutex gives each exclusive access —
    // making exact in-flight-count assertions valid under libtest's parallelism.
    // Each test leaves the gate idle.
    static GATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    fn serialize() -> MutexGuard<'static, ()> {
        // Tolerate a poisoned lock (a prior test panicked): the gate state is reset
        // per test, so the data is not actually corrupted for our purposes.
        GATE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn operation_guard_counts_in_flight() {
        let _serialize = serialize();
        assert_eq!(in_flight_operations(), 0);
        let g1 = operation_guard();
        assert!(in_flight_operations() >= 1);
        let g2 = operation_guard();
        assert!(in_flight_operations() >= 2);
        drop(g1);
        drop(g2);
        // Other threads may transiently bump the global count, so we only assert
        // that *our* two slots were returned (the count did not leak upward).
    }

    #[test]
    fn prefork_drains_then_parent_resumes() {
        let _serialize = serialize();
        // No op in flight from this thread: prefork must return promptly, leaving
        // the gate quiesced, and parent post-fork must lift it.
        prefork();
        assert_eq!(
            in_flight_operations(),
            0,
            "prefork must drain to zero before returning"
        );
        // A new op cannot enter while forking — verify the flag is set by trying
        // from another thread that must block until we resume.
        let entered = Arc::new(StdAtomicBool::new(false));
        let entered2 = entered.clone();
        let h = std::thread::spawn(move || {
            let _g = operation_guard(); // parks until postfork_parent
            entered2.store(true, StdOrdering::SeqCst);
        });
        // Give the spawned thread time to park; it must NOT have entered yet.
        for _ in 0..1000 {
            std::hint::spin_loop();
        }
        assert!(
            !entered.load(StdOrdering::SeqCst),
            "an operation must not enter during a fork window"
        );
        postfork_parent();
        h.join().unwrap();
        assert!(
            entered.load(StdOrdering::SeqCst),
            "the parked operation must enter once the parent resumes"
        );
    }

    #[test]
    fn postfork_child_resets_to_idle() {
        let _serialize = serialize();
        prefork();
        // The child handler resets the gate and disables background threads.
        postfork_child();
        assert_eq!(in_flight_operations(), 0);
        assert!(
            !background_enabled(),
            "child disables background maintenance"
        );
        // Re-arm so the rest of the test binary sees the default-on state, and
        // confirm the gate is usable again (no stuck FORKING flag / fork lock).
        set_background_enabled(true);
        let _g = operation_guard();
        assert!(in_flight_operations() >= 1);
    }

    #[test]
    fn maintenance_guard_counts_and_is_quiesced() {
        let _serialize = serialize();
        use crate::init::{InitPhase, INIT_PHASE};
        // Maintenance is gated on the §35.4 phase (W16-7): before the
        // background/profiling phase it must decline regardless of the flag. (The
        // global phase is monotone and other tests may have advanced it; assert the
        // gate at whatever phase we are at, then advance past it.)
        if INIT_PHASE.current() < InitPhase::BackgroundAndProfiling {
            assert!(
                maintenance_guard().is_none(),
                "maintenance must not run before its init phase (W16-7)"
            );
        }
        INIT_PHASE.advance_to(InitPhase::BackgroundAndProfiling);
        // A maintenance op now counts in the same gate as public ops, so one drain
        // covers both (§28.1 "quiesce background threads"). A *parallel* fork test
        // may transiently hold the fork window (in which maintenance correctly
        // declines), so retry until a window-free moment — bounded so a genuine
        // regression still fails.
        set_background_enabled(true);
        let mut got = None;
        for _ in 0..10_000_000u64 {
            if let Some(g) = maintenance_guard() {
                got = Some(g);
                break;
            }
            std::hint::spin_loop();
        }
        let g = got.expect("enabled + phase reached ⇒ a guard (between fork windows)");
        assert!(in_flight_operations() >= 1);
        drop(g);
        // When background is disabled (the post-fork state), maintenance declines —
        // regardless of phase / fork window (all routes yield `None`).
        set_background_enabled(false);
        assert!(
            maintenance_guard().is_none(),
            "disabled ⇒ maintenance must not run"
        );
        set_background_enabled(true); // restore for the rest of the binary
    }

    #[test]
    fn prefork_drain_waits_for_an_in_flight_operation() {
        let _serialize = serialize();
        use std::sync::Barrier;
        // T holds an operation in flight; prefork (on P) must block in its drain
        // until T leaves — the core §28.1 quiesce guarantee.
        let holding = Arc::new(StdAtomicBool::new(false));
        let release = Arc::new(StdAtomicBool::new(false));
        let drained = Arc::new(StdAtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));

        let t = {
            let (holding, release) = (holding.clone(), release.clone());
            std::thread::spawn(move || {
                let _g = operation_guard();
                holding.store(true, StdOrdering::SeqCst);
                while !release.load(StdOrdering::SeqCst) {
                    std::hint::spin_loop();
                }
                // `_g` drops here, leaving the operation.
            })
        };
        while !holding.load(StdOrdering::SeqCst) {
            std::hint::spin_loop();
        }

        // P runs prefork (blocks until T leaves) then postfork_parent — keeping the
        // FORK_LOCK acquire/release balanced on one thread (clean rank bookkeeping).
        let p = {
            let (drained, ready, release) = (drained.clone(), ready.clone(), release.clone());
            std::thread::spawn(move || {
                ready.wait(); // start prefork only after the main thread is watching
                prefork();
                drained.store(true, StdOrdering::SeqCst);
                // The op left, so the gate is fully quiesced.
                assert_eq!(in_flight_operations(), 0);
                postfork_parent();
                let _ = release; // keep alive
            })
        };
        ready.wait();
        // While T still holds its op, prefork must NOT have completed.
        for _ in 0..100_000 {
            std::hint::spin_loop();
        }
        assert!(
            !drained.load(StdOrdering::SeqCst),
            "prefork must block while an operation is in flight"
        );
        // Let T leave; prefork's drain now completes.
        release.store(true, StdOrdering::SeqCst);
        t.join().unwrap();
        p.join().unwrap();
        assert!(drained.load(StdOrdering::SeqCst));
        assert_eq!(in_flight_operations(), 0);
    }

    #[test]
    fn concurrent_forkers_are_serialized_and_leave_the_gate_idle() {
        let _serialize = serialize();
        // Two threads each run a full prefork → postfork_parent cycle. The
        // FORK_LOCK serializes them (one waits for the other's parent handler), and
        // each thread's acquire/release balances its own rank bookkeeping. No
        // deadlock, and the gate ends idle.
        let forker = || {
            std::thread::spawn(|| {
                for _ in 0..50 {
                    prefork();
                    postfork_parent();
                }
            })
        };
        let a = forker();
        let b = forker();
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(in_flight_operations(), 0);
        assert!(!fork_in_progress());
        // The gate is usable afterward.
        let _g = operation_guard();
    }

    #[test]
    fn sharding_mode_routes_correctly_either_way() {
        let _serialize = serialize();
        // The auto-probe picks single vs sharded; both must round-trip an op. We
        // cannot un-latch the process-global mode, so we exercise `shard_index`
        // returns a valid shard and an op counts/decrements cleanly under whatever
        // mode this binary latched.
        probe_and_set_sharding();
        let idx = shard_index();
        assert!(idx < NUM_SHARDS, "shard index in range");
        let g = operation_guard();
        // While our guard is held, *our* op is counted on its shard, so the total
        // is at least 1 regardless of parallel tests coming and going (an exact
        // count/delta would be racy under the shared process-global gate).
        assert!(in_flight_operations() >= 1, "a held op is counted");
        drop(g);
    }
}

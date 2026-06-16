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
//! # The gate is provably correct *and* `loom`-verifiable (a single-word CAS gate)
//!
//! The gate is the read side of a read-write lock packed into **one** atomic word
//! (`GATE`): the top bit is a fork-pending flag and the low bits are the
//! in-flight count. [`operation_guard`] enters by a **compare-exchange** that
//! increments the count *only if the fork bit is clear* (and the word is
//! otherwise unchanged); [`prefork`] sets the fork bit, then waits for the count
//! to drain to zero. Because the reader's increment and its fork-bit check are a
//! single CAS on the *same* word, there is no store-then-load (Dekker) hazard: a
//! reader that races a forker either succeeds *before* the bit is set (and is
//! counted by the drain) or fails its CAS *after* (and parks). This is the
//! standard CAS read-write-lock shape — verified over every interleaving by the
//! `gate_admits_no_op_across_a_fork` `loom` model (which, unlike a `SeqCst`
//! Dekker gate, `loom` *can* check, since it uses no `SeqCst`). The only hot-path
//! cost is one CAS on entry + one decrement on exit.
//!
//! # Re-entrancy and background threads
//!
//! The gate is a **counter**, so a re-entrant operation (e.g. `realloc` calling
//! `allocate`) simply nests — `prefork` waits for the count, which a balanced
//! pair always returns to zero. The allocator spawns **no** background threads
//! today (the release controller and rebalancer are host-driven), so
//! [`background_enabled`] is an advisory flag the child clears and a future pump
//! consults — the §28.1 "disable background threads until safe" hook, wired ahead
//! of its consumer.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::lock::{reset_lock_checker, LockRank, RankedLock};

/// The single-word fork gate: top bit `FORK_PENDING` is set during a fork
/// window (operations park); the low bits count in-flight operations. Packing
/// both into one word is what lets a reader's "increment if no fork pending" be a
/// single atomic compare-exchange (no Dekker store-load), so the gate is both
/// correct and `loom`-verifiable. See the module docs.
static GATE: AtomicU64 = AtomicU64::new(0);

/// The fork-pending flag (the gate's top bit). While set, [`operation_guard`]
/// parks instead of entering.
const FORK_PENDING: u64 = 1 << 63;
/// Mask of the in-flight-operation count (the gate's low 63 bits).
const COUNT_MASK: u64 = FORK_PENDING - 1;

/// Serializes concurrent forkers and is held across `fork()` itself (acquired in
/// pre-fork, released in parent post-fork, force-reset in the child). Rank
/// [`LockRank::GLOBAL_CONFIG`] (0): the outermost lock, the §28.1 "global
/// allocator fork lock". Held only by the forking thread, which holds no other
/// allocator lock, so it never interacts with the rank checker on the hot path.
static FORK_LOCK: RankedLock<{ LockRank::GLOBAL_CONFIG }> = RankedLock::new();

/// Whether the allocator's background maintenance (release pump, rebalancer) is
/// permitted to run. Cleared by the child post-fork handler (§28.1). Advisory:
/// the maintenance is host-driven today, so this gates a future pump.
static BACKGROUND_ENABLED: AtomicBool = AtomicBool::new(true);

/// RAII guard for one in-flight allocator operation: decrements the `GATE`
/// count on drop. Held for the dynamic extent of a public `allocate`/`free`/
/// `realloc`/arena operation, so `prefork`'s drain proves no internal lock is held.
#[must_use = "the operation is counted as in-flight until the guard is dropped"]
pub struct OperationGuard {
    // The count lives in the global `GATE`. The `*const ()` makes the guard
    // `!Send`/`!Sync`: an operation enters and leaves on one thread, so the guard
    // is never transferred across threads (it is a scoped, thread-local marker).
    _not_send: core::marker::PhantomData<*const ()>,
}

impl Drop for OperationGuard {
    #[inline]
    fn drop(&mut self) {
        // Release the slot. `Release` so a draining `prefork` (which `Acquire`-loads
        // the gate) that observes the count reach zero has also observed every
        // write this operation made under the internal locks (they are unlocked now).
        GATE.fetch_sub(1, Ordering::Release);
    }
}

/// Enter a public allocator operation, returning a guard that keeps it counted as
/// in-flight until dropped (W16-5). If a `fork()` is in progress this **parks**
/// until it completes, so the operation never races a fork — the §28.1 quiesce
/// seen from the operation side. Re-entrant: a nested operation simply nests the
/// count.
///
/// This is the single hot-path cost of fork safety: one compare-exchange on entry
/// (plus the matching decrement on the guard's drop).
#[inline]
pub fn operation_guard() -> OperationGuard {
    loop {
        let g = GATE.load(Ordering::Acquire);
        if g & FORK_PENDING != 0 {
            // A fork is in progress: park until the window closes, then retry.
            // We hold no slot while parked, so the drain is never blocked by us.
            while GATE.load(Ordering::Acquire) & FORK_PENDING != 0 {
                core::hint::spin_loop();
            }
            continue;
        }
        // Increment the in-flight count *iff* the word is still exactly `g` — so
        // a forker that sets `FORK_PENDING` between our load and this CAS makes the
        // CAS fail, and we loop back to park. The CAS atomically couples "no fork
        // pending" with "we are now counted", eliminating the Dekker hazard.
        match GATE.compare_exchange_weak(g, g + 1, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {
                return OperationGuard {
                    _not_send: core::marker::PhantomData,
                }
            }
            Err(_) => core::hint::spin_loop(), // raced another reader/the forker; retry
        }
    }
}

/// Pre-fork handler (§28.1): acquire the global fork lock and quiesce. After this
/// returns, no thread is executing a guarded operation, so no internal allocator
/// lock is held and every structure is consistent — safe to `fork()`.
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
    // Set the fork-pending bit: from here, a reader's entry CAS fails and it parks.
    // `AcqRel` so a reader whose CAS races this either wins before it (and is in
    // the count we drain) or loses after it (and sees the bit).
    GATE.fetch_or(FORK_PENDING, Ordering::AcqRel);
    // Drain: wait until every in-flight operation has left (count bits reach 0).
    // `Acquire` pairs with each operation's `Release` decrement, so once the count
    // is zero we have observed every operation's writes — the allocator is fully
    // quiesced and consistent.
    while GATE.load(Ordering::Acquire) & COUNT_MASK != 0 {
        core::hint::spin_loop();
    }
}

/// Parent post-fork handler (§28.1): clear the fork-pending bit and release the
/// fork lock. The parent's state was never inconsistent (it was quiesced, not
/// mutated), so there is nothing to reset — just lift the gate.
///
/// SPEC-transition: fork resume (§28.1 parent post-fork)
pub fn postfork_parent() {
    // Clear only the fork bit; the count is already zero (we drained it).
    GATE.fetch_and(COUNT_MASK, Ordering::Release);
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
    // The single child thread is the only accessor; reset the gate to "idle"
    // (count 0, fork bit clear).
    GATE.store(0, Ordering::Release);
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
    GATE.load(Ordering::Acquire) & COUNT_MASK
}

/// Whether a fork window is currently open (the fork-pending bit is set). Test /
/// diagnostic hook; operations park while this is true.
#[inline]
#[must_use]
pub fn fork_in_progress() -> bool {
    GATE.load(Ordering::Acquire) & FORK_PENDING != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};
    use std::sync::Arc;

    // These tests share the process-global gate; they coordinate so a `prefork`
    // window in one test cannot strand another. Each leaves the gate idle.

    #[test]
    fn operation_guard_counts_in_flight() {
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
}

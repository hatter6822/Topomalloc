// SPDX-License-Identifier: MIT
//! The lock hierarchy, ranked locks, and the debug lock-order checker
//! (W16-1, plan 05; §27.2), plus the atomics-ordering map (W16-3, §27.3).
//!
//! # The total order (W16-1a, §27.2)
//!
//! A fixed global acquisition order is the only practical defence against
//! deadlock across the `front → transfer → central → span → backend` layering.
//! Every lock in the allocator carries a **compile-time rank** ([`LockRank`]),
//! and a thread may only acquire a lock whose rank is strictly greater than
//! every rank it already holds. The order below is the §27.2 hierarchy,
//! *refined* into a total order over the concrete locks (the SPEC permits
//! refinement: "this order may be refined, but it MUST remain a total order"):
//!
//! ```text
//! GLOBAL_CONFIG (0) → ARENA_REGISTRY (1) → ARENA (2) → FRONT_END (3) → TRANSFER (4)
//!   → CENTRAL (5) → SPAN_POOL (6) → SPAN (7) → BACKEND (8) → STATS (9)
//! ```
//!
//! | rank | §27.2 bucket | concrete lock(s) |
//! |------|--------------|------------------|
//! | 0 `GLOBAL_CONFIG` | Global config | `fork::FORK_LOCK` (the §28.1 global fork lock); control plane (plan 07) |
//! | 1 `ARENA_REGISTRY` | Arena registry | `arena::ArenaTable::lock` |
//! | 2 `ARENA` | Arena | `allocator::HookRegistry::lock` (per-arena hook mgmt) |
//! | 3 `FRONT_END` | Front-end cache | `cpu_cache::PerCpu::lock` (the locked baseline / RSEQ fallback, W7-4) |
//! | 4 `TRANSFER` | Transfer cache | `transfer_cache::TransferBin` |
//! | 5 `CENTRAL` | NUMA central list | `central::CentralBin::lock` |
//! | 6 `SPAN_POOL` | *(refinement)* | `allocator::Allocator::span_lock` (descriptor pool) |
//! | 7 `SPAN` | Span | `span::SpanDescriptor::lock` (per-span) |
//! | 8 `BACKEND` | Backend extent | `extent::ExtentManager` / `large::LargeAllocator` / `huge::HugePageBackend` |
//! | 9 `STATS` | Stats shard | *(reserved; stats use relaxed atomics today)* |
//!
//! ## Why `SPAN_POOL` (5) is a refinement between `CENTRAL` (4) and `SPAN` (6)
//!
//! The span **descriptor pool** ([`Allocator::span_lock`](crate::allocator)) is
//! held across `SpanDescriptor::recycle`, which takes a **per-span** lock — so
//! the pool is *outer* to the per-span lock and must rank below it. It is never
//! nested with `CENTRAL` (span creation runs *after* the central `NeedSpan`
//! result, the §A.2 "create outside the central lock" rule), so it slots cleanly
//! between 4 and 6. The three `BACKEND` (7) locks (extent manager, large pool,
//! huge backend) are siblings that are *never* held simultaneously — the large
//! path consults the region-cache hook *before* taking the extent lock, and a
//! span's backing extent is allocated *and released* before the descriptor pool
//! lock is taken — so a single shared rank is correct and the strict-`>` checker
//! would (rightly) flag any future attempt to nest two of them.
//!
//! # The checker (W16-1b, the G-conc gate)
//!
//! In debug builds (`debug_assertions`, i.e. the normal `cargo test` build, plus
//! the hardened `debug-checks` profile) every [`RankedLock`] acquisition records
//! its rank on a small **per-thread held-rank set** and asserts the new rank
//! exceeds every rank currently held; release removes it. Any out-of-order
//! acquire trips a `debug_assert!`, failing CI — exactly the deadlock a
//! lock-order cycle would cause, caught deterministically before it can happen.
//! In `performance` builds the checker compiles to nothing (the rank stays a
//! compile-time constant, so W16-1a holds in every build; only the *runtime*
//! check is debug-gated). In pure `no_std` (no thread-local) the checker is a
//! no-op, like the bootstrap re-entrancy guard (S-007).
//!
//! The held-rank set is a **fixed-size, `const`-initialised** thread-local array
//! (never a `Vec`), so the checker is allocation-free and re-entrancy-safe even
//! when `topo-abi` installs the allocator as the process `#[global_allocator]`:
//! recording a rank never calls back into `malloc`.
//!
//! # Atomics-ordering map (W16-3, §27.3)
//!
//! Every atomic in the allocator has a documented memory order. The policy,
//! applied per-site throughout the crate and validated TSan-clean
//! (`cargo xtask test --kind tsan`):
//!
//! | role | order | rationale |
//! |------|-------|-----------|
//! | **publication** of a descriptor / node / buffer | `Release` store | makes the prior initialising writes visible to any consumer that acquires it |
//! | **consumption** of a published descriptor | `Acquire` load | pairs with the release store; the reader sees the fully-initialised object |
//! | **state transition** visible to concurrent free/classify (span state, generation, seqlock, arena state) | `AcqRel` RMW / `Release`+`Acquire` pair | the transition both publishes its result and observes the predecessor |
//! | **lock acquire / release** | `Acquire` on the take, `Release` on the drop | the critical section's reads/writes cannot leak out of the lock |
//! | **approximate counters / stats** | `Relaxed` | only a monotone tally is needed; no datum is *ordered by* the counter (§31.1) |
//! | **lock-free bump cursor** ([`BumpArena`](crate::bootstrap)) | `Relaxed` RMW | disjointness follows from the RMW's atomicity + monotonicity, not from any happens-before on the bytes |
//!
//! The named ordering-protocol invariants (the seqlock, the radix publish/read,
//! the large-free critical section, the W8 span-retirement and W9 quota CAS) are
//! additionally **machine-checked under every interleaving** by the `loom`
//! model in `tests/loom_protocols.rs`, which mirrors these exact orderings.

use core::sync::atomic::{AtomicBool, Ordering};

/// The §27.2 lock ranks, as compile-time `u8` constants (W16-1a). A
/// [`RankedLock`] is parameterised by one of these; the checker enforces that
/// acquisitions are strictly rank-increasing within a thread. See the module
/// docs for the full table and the refinement rationale.
pub struct LockRank;

#[allow(missing_docs)] // each constant is documented by the module-level table.
impl LockRank {
    /// Global configuration lock (control plane, plan 07). Reserved.
    pub const GLOBAL_CONFIG: u8 = 0;
    /// Arena registry lock (`ArenaTable`) — create/destroy/reset/configure.
    pub const ARENA_REGISTRY: u8 = 1;
    /// Per-arena hook-backend registry (`HookRegistry`).
    pub const ARENA: u8 = 2;
    /// Front-end per-CPU / thread cache lock — the locked baseline and RSEQ
    /// fallback (W7-4). The outermost data-path lock (§27.2 "front-end").
    pub const FRONT_END: u8 = 3;
    /// Transfer cache (per-LLC-domain), the innermost middle-end lock.
    pub const TRANSFER: u8 = 4;
    /// NUMA central free-list (per-`(node,sc)` bin).
    pub const CENTRAL: u8 = 5;
    /// Span **descriptor pool** — a refinement between `CENTRAL` and `SPAN`
    /// (outer to the per-span lock; see the module docs).
    pub const SPAN_POOL: u8 = 6;
    /// Per-span lock (the §27.2 "Span lock").
    pub const SPAN: u8 = 7;
    /// Backend extent lock (extent manager / large pool / huge backend).
    pub const BACKEND: u8 = 8;
    /// Stats shard lock. Reserved (stats are relaxed atomics today).
    pub const STATS: u8 = 9;
}

/// A test-and-test-and-set spinlock carrying a **compile-time rank** (W16-1a),
/// routed through the debug lock-order checker (W16-1b). It is the single lock
/// primitive in `topo-core`; every data-structure lock is a `RankedLock` so the
/// checker sees every acquisition.
///
/// The critical sections it guards are short (a handful of atomic edits, at most
/// one provider call at the `BACKEND` rank), there is no fairness requirement,
/// and the type must be `no_std` and `const`-constructible (it lives inside
/// `static`/`UnsafeCell`-backed metadata) — so a spinlock, not a blocking mutex,
/// is the right tool (§27.2). Both a manual [`acquire`](Self::acquire)/
/// [`release`](Self::release) pair (for critical sections with early-return
/// roll-back) and an RAII [`lock`](Self::lock) guard are provided; both feed the
/// checker identically.
pub struct RankedLock<const RANK: u8> {
    locked: AtomicBool,
}

impl<const RANK: u8> RankedLock<RANK> {
    /// A fresh, unlocked ranked lock (usable in `const` context).
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    /// This lock's rank (the const parameter, surfaced for diagnostics/tests).
    #[inline]
    #[must_use]
    pub const fn rank(&self) -> u8 {
        RANK
    }

    /// Acquire the lock (spin), recording the rank with the checker. Pairs with
    /// exactly one [`release`](Self::release). The `Acquire` ordering keeps the
    /// critical section's loads/stores from being reordered before the take
    /// (§27.3 "lock acquire").
    #[inline]
    pub fn acquire(&self) {
        // Record the *intent* order before spinning: the checker's assertion is a
        // property of the acquisition order this thread requests, independent of
        // contention, so recording before the (possibly long) spin keeps the held
        // set accurate even while we wait.
        checker::enter(RANK);
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Spin on a relaxed load (test-and-test-and-set) so a waiter does not
            // hammer the cache line with CAS while another thread holds the lock.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Release a lock taken with [`acquire`](Self::acquire). The `Release`
    /// ordering publishes the critical section's writes to the next acquirer.
    #[inline]
    pub fn release(&self) {
        self.locked.store(false, Ordering::Release);
        checker::leave(RANK);
    }

    /// Acquire the lock and return an RAII [`RankedGuard`] that releases it (and
    /// notifies the checker) on drop — the scoped form of
    /// [`acquire`](Self::acquire)/[`release`](Self::release).
    #[inline]
    pub fn lock(&self) -> RankedGuard<'_, RANK> {
        self.acquire();
        RankedGuard { lock: self }
    }

    /// Force the lock to the unlocked state **without** the rank bookkeeping — the
    /// fork-child reset (W16-5b, §28.1). After `fork()` an internal lock may be
    /// marked held by a thread that no longer exists; the child *resets* (does not
    /// *unlock*) every allocator lock to a clean state. This must run only in the
    /// single-threaded post-fork child, where there is provably no other accessor.
    ///
    /// # Safety
    ///
    /// The caller MUST guarantee no other thread can be touching this lock or the
    /// data it guards — satisfied in the post-fork child (one thread) after a
    /// pre-fork quiesce ([`crate::fork`]). Using it elsewhere can corrupt the
    /// guarded structure by clearing a genuinely-held lock.
    #[inline]
    pub unsafe fn force_reset(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

impl<const RANK: u8> Default for RankedLock<RANK> {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for a [`RankedLock`]: releases the lock (and pops the rank from the
/// checker) when dropped.
#[must_use = "the lock is released as soon as the guard is dropped"]
pub struct RankedGuard<'a, const RANK: u8> {
    lock: &'a RankedLock<RANK>,
}

impl<const RANK: u8> Drop for RankedGuard<'_, RANK> {
    #[inline]
    fn drop(&mut self) {
        self.lock.release();
    }
}

// ---------------------------------------------------------------------------
// The per-thread held-rank checker (W16-1b). Active only in debug builds with a
// thread-local available; a no-op otherwise. Allocation-free and reentrancy-safe
// (a fixed array in a `const`-initialised thread-local), so it never re-enters
// `malloc` even when this crate backs the process `#[global_allocator]`.
// ---------------------------------------------------------------------------

#[cfg(all(debug_assertions, any(test, feature = "std")))]
mod checker {
    use core::cell::Cell;

    /// Max simultaneously-held ranks tracked. The real maximum nesting depth is
    /// ~3 (e.g. `ARENA_REGISTRY → … → BACKEND`); 16 is generous headroom. An
    /// overflow is itself a bug (an unexpected deep nest) and trips an assert.
    const MAX_HELD: usize = 16;

    /// A thread's currently-held ranks (a small stack). `len` entries are live.
    /// `Copy` + fixed-size so the thread-local is `const`-initialised (Local-Exec
    /// TLS) and never allocates — the W16-2 / S-007 re-entrancy rule.
    #[derive(Clone, Copy)]
    struct Held {
        ranks: [u8; MAX_HELD],
        len: usize,
    }

    impl Held {
        const fn new() -> Self {
            Self {
                ranks: [0; MAX_HELD],
                len: 0,
            }
        }
    }

    std::thread_local! {
        static HELD: Cell<Held> = const { Cell::new(Held::new()) };
    }

    /// Record acquiring `rank`: assert it exceeds every currently-held rank (the
    /// strict-increasing, deadlock-free order), then push it.
    #[inline]
    pub(super) fn enter(rank: u8) {
        HELD.with(|cell| {
            let mut held = cell.get();
            // Strict-increasing: a new acquisition must out-rank everything held,
            // so the global order can never form a cycle (W16-1b / DD-3).
            for i in 0..held.len {
                debug_assert!(
                    held.ranks[i] < rank,
                    "lock-order violation (G-conc): acquiring rank {rank} while holding rank {} \
                     — acquisitions MUST be strictly rank-increasing (§27.2)",
                    held.ranks[i]
                );
            }
            debug_assert!(
                held.len < MAX_HELD,
                "lock-order checker: held-rank stack overflow (>{MAX_HELD} nested locks)"
            );
            if held.len < MAX_HELD {
                held.ranks[held.len] = rank;
                held.len += 1;
                cell.set(held);
            }
        });
    }

    /// Record releasing `rank`: remove one matching entry (release need not be
    /// LIFO — the held set is a multiset, so any matching rank is removed).
    #[inline]
    pub(super) fn leave(rank: u8) {
        HELD.with(|cell| {
            let mut held = cell.get();
            // Find the top-most matching rank and swap-remove it. Searching from the
            // top keeps the common LIFO case O(1) and tolerates out-of-order release.
            let mut idx = None;
            for i in (0..held.len).rev() {
                if held.ranks[i] == rank {
                    idx = Some(i);
                    break;
                }
            }
            if let Some(i) = idx {
                held.ranks[i] = held.ranks[held.len - 1];
                held.len -= 1;
                cell.set(held);
            }
            // A `leave` with no matching `enter` means unbalanced acquire/release —
            // but `force_reset` (fork child) deliberately bypasses this path, so a
            // missing entry is not asserted here.
        });
    }

    /// The number of locks the current thread holds (test/diagnostic hook).
    #[inline]
    pub(super) fn held_count() -> usize {
        HELD.with(|cell| cell.get().len)
    }

    /// Clear the current thread's held-rank set (the fork-child reset, W16-5b):
    /// after `fork()` the child's one thread inherits a bookkeeping snapshot from
    /// the moment of fork; reset it so the checker starts clean.
    #[inline]
    pub(super) fn reset_thread() {
        HELD.with(|cell| cell.set(Held::new()));
    }
}

#[cfg(not(all(debug_assertions, any(test, feature = "std"))))]
mod checker {
    /// No-op in `performance` builds and pure `no_std` (no thread-local): the
    /// rank stays a compile-time constant (W16-1a), only the runtime check is
    /// elided. Mirrors the bootstrap re-entrancy guard's `no_std` no-op.
    #[inline(always)]
    pub(super) fn enter(_rank: u8) {}
    #[inline(always)]
    pub(super) fn leave(_rank: u8) {}
    /// Always 0 — nothing is tracked without the checker.
    #[inline(always)]
    pub(super) fn held_count() -> usize {
        0
    }
    /// No-op without the checker.
    #[inline(always)]
    pub(super) fn reset_thread() {}
}

/// The number of allocator locks the current thread holds, per the debug
/// lock-order checker (always `0` outside a debug `std` build). Exposed so the
/// fork handlers and tests can assert a clean lock state (W16-5).
#[inline]
#[must_use]
pub fn held_lock_count() -> usize {
    checker::held_count()
}

/// Reset the current thread's held-rank bookkeeping to empty — the fork-child
/// reset (W16-5b). A no-op outside a debug `std` build.
#[inline]
pub fn reset_lock_checker() {
    checker::reset_thread();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_ranks_are_strictly_increasing() {
        // The documented §27.2 total order, in acquisition order. Asserting the
        // sequence is strictly increasing pins that every rank is distinct and
        // ordered (a runtime check over the slice, so it is not a constant assert).
        let order = [
            LockRank::GLOBAL_CONFIG,
            LockRank::ARENA_REGISTRY,
            LockRank::ARENA,
            LockRank::FRONT_END,
            LockRank::TRANSFER,
            LockRank::CENTRAL,
            LockRank::SPAN_POOL,
            LockRank::SPAN,
            LockRank::BACKEND,
            LockRank::STATS,
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0] < pair[1],
                "lock ranks must be a strict total order: {} !< {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn acquire_release_roundtrips() {
        let l: RankedLock<{ LockRank::CENTRAL }> = RankedLock::new();
        assert_eq!(l.rank(), LockRank::CENTRAL);
        l.acquire();
        assert_eq!(
            held_lock_count(),
            if cfg!(debug_assertions) { 1 } else { 0 }
        );
        l.release();
        assert_eq!(held_lock_count(), 0);
    }

    #[test]
    fn guard_releases_on_drop() {
        let l: RankedLock<{ LockRank::SPAN }> = RankedLock::new();
        {
            let _g = l.lock();
            assert_eq!(
                held_lock_count(),
                if cfg!(debug_assertions) { 1 } else { 0 }
            );
        }
        assert_eq!(held_lock_count(), 0);
        // Re-acquirable after the guard drops.
        let _g = l.lock();
    }

    #[test]
    fn increasing_nesting_is_accepted() {
        // central(4) → span_pool(5) → span(6) → backend(7): the real nesting shape.
        let c: RankedLock<{ LockRank::CENTRAL }> = RankedLock::new();
        let sp: RankedLock<{ LockRank::SPAN_POOL }> = RankedLock::new();
        let s: RankedLock<{ LockRank::SPAN }> = RankedLock::new();
        let b: RankedLock<{ LockRank::BACKEND }> = RankedLock::new();
        let _gc = c.lock();
        let _gsp = sp.lock();
        let _gs = s.lock();
        let _gb = b.lock();
        assert_eq!(
            held_lock_count(),
            if cfg!(debug_assertions) { 4 } else { 0 }
        );
    }

    #[test]
    fn out_of_order_release_is_tolerated() {
        // Release need not be LIFO: the checker tracks a multiset.
        let a: RankedLock<{ LockRank::CENTRAL }> = RankedLock::new();
        let b: RankedLock<{ LockRank::SPAN }> = RankedLock::new();
        a.acquire();
        b.acquire();
        a.release(); // release the *outer* first
        b.release();
        assert_eq!(held_lock_count(), 0);
    }

    #[cfg(all(debug_assertions, any(test, feature = "std")))]
    #[test]
    fn out_of_order_acquire_trips_the_checker() {
        // Acquiring a lower rank while holding a higher one is the deadlock the
        // hierarchy forbids; the debug checker must catch it (G-conc).
        let high: RankedLock<{ LockRank::BACKEND }> = RankedLock::new();
        let low: RankedLock<{ LockRank::CENTRAL }> = RankedLock::new();
        let prev = std::panic::take_hook();
        std::panic::set_hook(std::boxed::Box::new(|_| {}));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            high.acquire();
            low.acquire(); // backend(7) held, acquiring central(4) — violation
        }));
        std::panic::set_hook(prev);
        assert!(
            caught.is_err(),
            "out-of-order acquire must trip the checker"
        );
        // Clean up the thread's bookkeeping for the rest of the test binary: the
        // `high` lock is still recorded as held (we panicked mid-`low.acquire`).
        reset_lock_checker();
    }

    #[test]
    fn force_reset_clears_a_held_lock() {
        let l: RankedLock<{ LockRank::BACKEND }> = RankedLock::new();
        l.acquire();
        // SAFETY: single-threaded test; no other accessor — the fork-child contract.
        unsafe { l.force_reset() };
        // The byte is clear, so it re-acquires without deadlock.
        reset_lock_checker(); // mirror the child handler clearing the checker
        l.acquire();
        l.release();
    }
}

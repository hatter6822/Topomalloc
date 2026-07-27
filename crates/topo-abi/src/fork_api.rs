// SPDX-License-Identifier: MIT
//! `pthread_atfork` registration and the crash-summary C API (W16-5 / W16-6,
//! plan 05; §28.1, §28.4).
//!
//! The fork **mechanism** (the quiesce gate, prefork/postfork) lives in
//! `topo-core` ([`topo_core::fork`]); this module is the OS wiring that
//! `topo-core` cannot do (it is `no_std` and has no `libc`): it installs the
//! three `pthread_atfork` handlers so the process allocator is fork-safe (§28.1),
//! and exposes the lock-free crash summary as a C entry point (§28.4).
//!
//! # The three handlers (§28.1)
//!
//! * **prepare** (parent, before fork): [`topo_core::fork::prefork`] — acquire the
//!   global fork lock and drain in-flight operations, so no internal lock is held
//!   at `fork()`.
//! * **parent** (after fork): [`topo_core::fork::postfork_parent`] — release and
//!   resume.
//! * **child** (after fork): [`topo_core::fork::postfork_child`] — reset the gate,
//!   clear the lock-order checker's inherited bookkeeping, and disable background
//!   maintenance. Because the parent quiesced first, the child inherits consistent,
//!   unlocked structures and can allocate safely.
//!
//! Registration is **idempotent**, **re-entrancy-safe**, and happens **eagerly at
//! library load** via an ELF `.init_array` constructor, with the first-`global()`
//! call as a fallback. Installing the handlers before any allocation means even a
//! `fork()` racing the very first allocation is intercepted, and (because the lazy
//! init is itself fork-gated) `prefork` drains-and-waits for it rather than forking
//! a child that inherits a half-constructed `OnceLock` (W16-5 / #4).

#[cfg(unix)]
use core::sync::atomic::{AtomicU8, Ordering};

/// `pthread_atfork` prepare handler (parent context, pre-fork): quiesce the
/// allocator so `fork()` happens with no internal lock held (§28.1).
#[cfg(unix)]
extern "C" fn atfork_prepare() {
    topo_core::fork::prefork();
}

/// `pthread_atfork` parent handler (post-fork in the parent): resume (§28.1).
#[cfg(unix)]
extern "C" fn atfork_parent() {
    topo_core::fork::postfork_parent();
}

/// `pthread_atfork` child handler (post-fork in the child): reset lock/gate state,
/// revert the front end to its locked baseline, and disable background threads until
/// the host re-arms them (§28.1). The parent's pre-fork quiesce guarantees the child's
/// structures are consistent.
///
/// **The W7 fast path goes off in the child.** Only the forking thread survives, and
/// its RSEQ registration is the kernel's per-*thread* state, not the process's — the
/// registrations of every other thread died with them, so a child that later spawns
/// threads would have them running unregistered while the cache still believed the fast
/// path was live. Reverting to the locked baseline is the §28.1 conservative-mode rule
/// and costs nothing but the spinlock; a host that wants the fast path back in the child
/// re-enables it explicitly, which is also where the child's own threads register.
/// Cached objects are untouched: the child inherited the slots, their contents, and the
/// per-object cached bits as one consistent copy, and the locked path reaches all of it.
///
/// Reached through `global()` only if the engine is already built. If the fork raced the
/// very first allocation, there is no engine to revert — and none of its state to be
/// stale either, since a later `global()` in the child builds it fresh.
#[cfg(unix)]
extern "C" fn atfork_child() {
    topo_core::fork::postfork_child();
    if let Some(eng) = crate::global_if_init() {
        // Not just "disable": the child must also *forget* that RSEQ ever ran, because
        // neither membarrier's registration of intent nor the rseq area survives `fork`,
        // so a child that still thought it owed a non-owner fence could not issue one,
        // and one that still thought RSEQ was enabled would run the fast path over
        // kernel state that no longer exists.
        //
        // SAFETY: this is the `pthread_atfork` **child** handler — the one context that
        // discharges the quiesced-single-threaded obligation by construction. `fork`
        // returns a process with exactly one thread, so no restartable sequence from the
        // parent is in flight to race the mode publish or the latch clear.
        unsafe { eng.reset_front_end_after_fork() };
        refresh_child_entropy(eng, topo_core::deterministic::is_deterministic());
    }
}

/// The child half of §29.5: bank fork-distinct entropy, and apply it to the live samplers
/// unless deterministic mode says to defer.
///
/// `deterministic` is a parameter rather than a query so both branches are testable
/// without switching the process-global mode, which every parallel test would see.
#[cfg(unix)]
fn refresh_child_entropy(eng: &crate::AnyAllocator, deterministic: bool) {
    {
        // §29.5: the child inherited the parent's guard-page coin and quarantine-evictor
        // PRNG state byte for byte, so without a re-seed the two processes make the *same*
        // hardening choices for the same allocation sequence — and one of them is
        // observable to whoever can run it. Fresh, fork-distinct entropy, then re-derive
        // the sampler streams from it.
        //
        // **The two halves are gated differently, and conflating them was a leak.** The
        // *stored process entropy* is refreshed unconditionally: it is not a randomized
        // stream, it is the per-process secret every later re-seed draws from, and nothing
        // reads it while deterministic mode is on (`seed_security_samplers` is the only
        // consumer, and `reseed_live` calls it only on the not-deterministic branch).
        // Leaving it inherited meant a parent and child that both later *disabled*
        // deterministic mode re-derived the guard coin, the quarantine evictor and the heap
        // sampler from the identical value — restoring in that moment exactly the matched,
        // predictable schedules §29.5 exists to prevent, on the very path that advertises
        // unpredictability restored.
        crate::entropy::reseed_after_fork();
        // *Applying* it to the live samplers is what deterministic mode (§30.4) forbids:
        // its whole contract is that every randomized decision derives from the configured
        // global seed so a replay reproduces exactly, and injecting fork-distinct entropy
        // into a running stream would silently break that for any workload that forks. The
        // two goals are genuinely opposed here, and determinism is the one the operator
        // asked for explicitly — so the fresh entropy is banked and takes effect if and
        // when deterministic mode is turned off.
        if !deterministic {
            eng.seed_security_samplers();
        }
    }
}

/// The guard for a **one-shot side effect that can fail transiently**: it records
/// whether the effect *happened*, separately from whether a thread is attempting it.
///
/// Two states cannot express that, and the difference is not academic here. A plain
/// `registered: bool` has to set the flag *before* calling `pthread_atfork` (claiming the
/// attempt is what makes re-entry safe — a blocking `Once` would dead-lock when
/// `pthread_atfork` allocates and re-enters through the global allocator). That same flag
/// is what every other caller reads as "already installed", so during the window between
/// the claim and a failing store a concurrent thread reads a registration that never
/// happened and skips its own attempt. Releasing the flag afterwards does not repair it:
/// the retry has to come from some *later* caller, and once `GLOBAL` is initialized
/// `global()` answers from its fast path, so on a two-state flag no later caller exists.
/// One transient `ENOMEM` — the documented failure when the handler list cannot grow —
/// then costs the process its fork safety permanently, and every subsequent `fork()`
/// leaves the child on inherited allocator state with no quiesce handler.
///
/// Three states fix both halves: an observer's skip means only "someone else is trying",
/// and [`registered`](Self::registered) is a predicate the fast-path retry can act on.
#[cfg(unix)]
struct AtforkGuard(AtomicU8);

#[cfg(unix)]
impl AtforkGuard {
    /// No attempt has been made, or the last one failed and released its claim.
    const IDLE: u8 = 0;
    /// A thread is inside `pthread_atfork` right now. **Not** the same as registered.
    const IN_PROGRESS: u8 = 1;
    /// The handlers are installed. Terminal — `pthread_atfork` cannot be undone.
    const DONE: u8 = 2;

    const fn new() -> AtforkGuard {
        AtforkGuard(AtomicU8::new(Self::IDLE))
    }

    /// Whether the effect has **completed**. One load; `IN_PROGRESS` reads `false`.
    #[inline]
    fn registered(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::DONE
    }

    /// Claim the right to attempt the effect. `false` for both "already done" and
    /// "someone else is mid-attempt" — what the caller does about the latter depends on
    /// *who* it is; see [`register_atfork_handlers`].
    #[inline]
    fn try_claim(&self) -> bool {
        self.0
            .compare_exchange(
                Self::IDLE,
                Self::IN_PROGRESS,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Whether an attempt is in flight right now. One acquire load.
    #[inline]
    fn in_progress(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::IN_PROGRESS
    }

    /// Publish an attempt's outcome: `DONE` on success, back to `IDLE` on failure so
    /// the next caller retries.
    #[inline]
    fn publish(&self, ok: bool) {
        self.0
            .store(if ok { Self::DONE } else { Self::IDLE }, Ordering::Release);
    }
}

#[cfg(unix)]
static ATFORK: AtforkGuard = AtforkGuard::new();

/// Whether the `pthread_atfork` handlers are **installed** — not merely being
/// installed. One load; the fast path of [`crate::global`] uses it to retry a
/// registration that a transient failure lost. Always `true` on non-unix hosts, which
/// have no `fork` to guard.
#[inline]
pub(crate) fn atfork_registered() -> bool {
    #[cfg(unix)]
    {
        ATFORK.registered()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(unix)]
thread_local! {
    /// Set while *this thread* is inside `pthread_atfork`, so a nested call can tell
    /// "I am the registering thread, re-entered through the allocator" from "another
    /// thread is registering". The two need opposite answers and the guard word cannot
    /// distinguish them: it records that *someone* is attempting, not who.
    ///
    /// `const`-initialised `Cell<bool>` with no destructor, so touching it allocates
    /// nothing and cannot re-enter the allocator — the same discipline as the fork
    /// gate's depth TLS.
    static REGISTERING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Install the `pthread_atfork` handlers exactly once (W16-5). Idempotent,
/// **re-entrancy-safe**, and — for a *concurrent* caller — synchronous: it does not
/// return until the handlers are installed or the in-flight attempt has failed.
///
/// The claim is taken *before* `pthread_atfork` is called, so if that call itself
/// allocates and re-enters this function through the global allocator, the nested call
/// finds the claim taken. Two callers can find that, and they must be treated
/// differently:
///
/// * **The registering thread, re-entered.** Returning at once is the only option — it
///   is waiting for itself, so blocking (a `Once`, or a spin) dead-locks. `REGISTERING`
///   identifies it. It should no longer *get* here: [`RegisteringWindow`] serves that
///   thread's allocations from bootstrap storage, so `global()` is not called at all.
///   The branch stays because returning-at-once is the only safe answer for a claimant
///   that reaches it by some route the window does not cover, and because a re-entrant
///   caller that merely returns is not enough on its own — see [`RegisteringWindow`].
/// * **Another thread.** Returning at once leaves it free to walk straight into
///   `GLOBAL.get_or_init` with no handlers installed, which is the exact window this
///   call exists to close: a `fork()` in it strands the child on a half-constructed
///   `OnceLock` with no quiesce handler. It waits instead. The wait is bounded by one
///   `pthread_atfork` call — the holder is running, not blocked on anything this thread
///   holds (the caller has taken no lock and not yet entered the init) — and it is the
///   cold first-allocation path, reached only where the load-time ctor was elided.
///
/// Installed eagerly at load by [`REGISTER_ATFORK_CTOR`], on the first `global()` call as
/// a fallback, and retried from `global()`'s fast path while [`atfork_registered`] is
/// false — see [`AtforkGuard`] for why a two-state flag makes one transient failure
/// permanent. A no-op on non-unix hosts (no `fork`).
pub(crate) fn register_atfork_handlers() {
    #[cfg(unix)]
    {
        run_attempt(&ATFORK, || {
            // SAFETY: the three handlers are `extern "C"` functions with no captured
            // state that only call the allocation-free `topo_core::fork::*` routines;
            // `pthread_atfork` simply records them.
            unsafe {
                libc::pthread_atfork(
                    Some(atfork_prepare),
                    Some(atfork_parent),
                    Some(atfork_child),
                )
            }
        });
    }
}

/// The registering thread's window: it is the one inside `pthread_atfork`, **and** its
/// allocations are served from bootstrap storage rather than the engine.
///
/// The second half is what keeps the first from being a loophole. `pthread_atfork`
/// allocates (glibc grows its handler list once the static pool is exhausted), and when
/// `TopoMallocGlobal` is the process `#[global_allocator]` that allocation comes straight
/// back here. Marking the thread makes the nested `register_atfork_handlers` return
/// instead of waiting for itself — but *returning* was never the end of it: the nested
/// `global()` then walked on into `GLOBAL.get_or_init` and built the whole allocator with
/// no handlers installed. A `fork()` from another thread during that build is exactly the
/// #4 hazard the eager ctor exists to close, reached whenever that ctor was elided — and
/// the child inherits a half-constructed `OnceLock` with no quiesce handler.
///
/// Opening the bootstrap window removes the re-entry rather than tolerating it: the
/// allocation is served by the system allocator, `global()` is never called, and no
/// initialization can happen before `pthread_atfork` returns. The engine's own `free`
/// already routes such a pointer back (it classifies as `Foreign`, the bootstrap-window
/// contract), so nothing downstream needs to know.
///
/// This is the whole re-entry surface: every C symbol this crate exports is
/// `topomalloc_`-prefixed, so it interposes no libc allocator and `pthread_atfork` has no
/// other route back into `global()`.
///
/// Restores both flags on the way out, panic included.
#[cfg(unix)]
struct RegisteringWindow {
    _bootstrap: crate::BootstrapGuard,
}

#[cfg(unix)]
impl RegisteringWindow {
    fn enter() -> Self {
        REGISTERING.with(|r| r.set(true));
        RegisteringWindow {
            _bootstrap: crate::BootstrapGuard::enter(),
        }
    }
}

#[cfg(unix)]
impl Drop for RegisteringWindow {
    fn drop(&mut self) {
        REGISTERING.with(|r| r.set(false));
    }
}

/// One registration attempt against `guard`: claim it, run `install` inside the
/// [`RegisteringWindow`], and publish the outcome. A caller that finds the claim taken
/// waits it out (or returns at once, if it *is* the claimant — see
/// [`register_atfork_handlers`]). Returns whether this call installed the handlers.
///
/// `install` is a parameter, and the guard is too, so both branches are testable without
/// touching the process-global `ATFORK` — driving that from a test would race the
/// registration every other test depends on.
#[cfg(unix)]
fn run_attempt(guard: &AtforkGuard, install: impl FnOnce() -> core::ffi::c_int) -> bool {
    if !guard.try_claim() {
        await_attempt(guard, REGISTERING.with(core::cell::Cell::get));
        return false;
    }
    let rc = {
        let _window = RegisteringWindow::enter();
        install()
    };
    guard.publish(rc == 0);
    rc == 0
}

/// Wait out an in-flight registration attempt — unless this *is* the thread making it,
/// which would be waiting for itself.
///
/// Takes both the guard and the "am I the registering thread" answer as parameters so the
/// two branches are testable against a private guard; driving the process-global one from
/// a test would race the registration every other test depends on.
///
/// The loop ends on either terminal state, not on success: a failed attempt returns the
/// guard to `IDLE`, and the caller then proceeds with `atfork_registered()` false, which
/// `global()`'s fast path retries later. Spinning until `DONE` would instead wait forever
/// on an attempt that keeps failing.
#[cfg(unix)]
#[inline]
fn await_attempt(guard: &AtforkGuard, is_registering_thread: bool) {
    if is_registering_thread {
        return;
    }
    while guard.in_progress() {
        core::hint::spin_loop();
    }
}

/// Install the `pthread_atfork` handlers **at library load** (before `main` and
/// before any allocation), via an ELF `.init_array` constructor, so a `fork()`
/// racing the very first lazy allocation is intercepted (#4). Without it the
/// handlers would first be installed *inside* the initial `global()` init, leaving
/// a window in which a concurrent fork is not quiesced and the child inherits a
/// half-constructed `OnceLock`. Non-Linux/non-ELF targets fall back to the
/// idempotent registration on the first `global()` call.
#[cfg(all(unix, target_os = "linux"))]
#[used]
#[link_section = ".init_array"]
static REGISTER_ATFORK_CTOR: extern "C" fn() = {
    extern "C" fn ctor() {
        register_atfork_handlers();
    }
    ctor
};

/// Write a minimal, **lock-free, allocation-free** allocator summary into `buf`
/// for a crash or signal handler (§28.4, W16-6): init phase, cumulative
/// allocated/freed/live bytes, and whether background maintenance is enabled.
///
/// Returns the number of bytes written (ASCII `key=value` lines), never exceeding
/// `len`. Safe to call from a signal handler: it takes **no** lock, allocates
/// nothing, and never triggers the allocator's lazy initialization (if the
/// allocator is not yet up, only the process-wide init phase / background flag are
/// reported). `buf` must point to at least `len` writable bytes; a null `buf` or
/// zero `len` writes nothing and returns `0`.
///
/// # Safety
///
/// `buf` must be valid for writes of `len` bytes (or null, in which case nothing
/// is written). The function performs no allocation and takes no lock.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_crash_summary(
    buf: *mut core::ffi::c_char,
    len: usize,
) -> usize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    // Read the summary lock-free. If the allocator is not yet initialized, report
    // a zeroed summary carrying just the process init phase / background flag —
    // never force initialization (which would allocate, §28.4).
    let summary = match crate::global_if_init() {
        Some(a) => a.crash_summary(),
        None => topo_core::CrashSummary {
            init_phase: topo_core::INIT_PHASE.current() as u8,
            allocated_bytes: 0,
            freed_bytes: 0,
            live_bytes: 0,
            in_flight_ops: topo_core::in_flight_operations(),
            background_enabled: topo_core::background_enabled(),
        },
    };
    // SAFETY: the caller guarantees `buf` is valid for `len` writes; we form a
    // mutable byte slice over exactly that region and `CrashSummary::write` clamps
    // to it (never overruns).
    let out = unsafe { core::slice::from_raw_parts_mut(buf.cast::<u8>(), len) };
    summary.write(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_atfork_is_idempotent_and_reentrancy_safe() {
        // Calling it repeatedly — including concurrently — must not panic, dead-lock,
        // or double-register. The CAS guard claims registration *before* calling
        // `pthread_atfork`, so a re-entrant call (were `pthread_atfork` itself to
        // allocate through this allocator during the load-time ctor) observes the
        // flag and returns at once, where a blocking `Once` would dead-lock (#4).
        //
        // The claim is provisional, not final: if `pthread_atfork` reports failure
        // (`ENOMEM` — the handler list could not grow) the flag is released again, so
        // the next caller retries rather than inheriting a permanent record of a
        // registration that never happened. That path cannot be forced from a test
        // (the failure is an allocation-pressure condition inside libc), so what is
        // asserted here is the success invariant it preserves: after these calls the
        // flag is set *and* the handlers really are installed.
        register_atfork_handlers();
        register_atfork_handlers();
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(register_atfork_handlers))
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // By now (the test binary has long since allocated, and the `.init_array`
        // ctor ran at load), the handlers are registered.
        assert!(atfork_registered());
    }

    /// A transient `pthread_atfork` failure must stay transient.
    ///
    /// Driven over a **private** [`AtforkGuard`], not the process-global one: the
    /// harness runs tests as parallel threads and every allocating test now consults
    /// the global guard through `global()`'s fast path, so a test that swapped its
    /// state would race real registration attempts. The guard is the whole mechanism,
    /// so exercising an instance of it is exercising the fix.
    ///
    /// Each assertion below fails on a two-state `AtomicBool`, which is what this
    /// replaced: there, claiming the attempt sets the same bit that means "installed",
    /// so an observer during the window reads `true` and skips — and because
    /// `global()`'s fast path skips its retry on that same predicate, and no caller
    /// reaches the slow path once `GLOBAL` is initialized, nothing ever tries again.
    #[cfg(unix)]
    #[test]
    fn a_failed_registration_is_retried_and_never_reads_as_installed() {
        let guard = AtforkGuard::new();
        assert!(!guard.registered(), "a fresh guard has installed nothing");

        // Thread A claims the attempt and is now inside `pthread_atfork`.
        assert!(guard.try_claim());
        // The window a two-state flag gets wrong: an attempt is not an installation.
        assert!(
            !guard.registered(),
            "an in-flight attempt must not advertise itself as installed"
        );
        // Thread B (or a re-entrant call from inside `pthread_atfork` itself) must
        // decline and return — never block, never register a second set of handlers.
        assert!(!guard.try_claim(), "an observer must not double-register");

        // A's call returns ENOMEM. The claim is released...
        guard.publish(false);
        assert!(!guard.registered());
        // ...and the next caller genuinely retries, which is the property that makes
        // the failure transient rather than terminal.
        assert!(guard.try_claim(), "a released claim must be retryable");
        guard.publish(true);
        assert!(guard.registered());

        // Terminal: a later caller neither re-registers nor can disturb the state.
        assert!(!guard.try_claim());
        assert!(guard.registered());
    }

    /// W16-5 (#4): an observer that finds an attempt in flight must **wait**, not
    /// proceed — proceeding walks straight into `GLOBAL.get_or_init` with no handlers
    /// installed, which is the window this registration exists to close. The one
    /// exception is the registering thread itself, re-entered through the allocator by
    /// `pthread_atfork`: it would be waiting for itself.
    ///
    /// Both branches are driven against a **private** guard. The process-global one is
    /// consulted by `global()`'s fast path on every allocating test, so a test that
    /// parked it in `IN_PROGRESS` would stall every sibling that allocates.
    ///
    /// Fails without the fix, which returned immediately in both cases: the observer
    /// thread would finish while the claim was still held.
    #[cfg(unix)]
    #[test]
    fn a_concurrent_observer_waits_out_an_attempt_but_the_registering_thread_does_not() {
        use std::sync::atomic::{AtomicBool, Ordering as O};
        use std::sync::Arc;

        let guard = Arc::new(AtforkGuard::new());
        assert!(
            guard.try_claim(),
            "this thread is now 'inside pthread_atfork'"
        );

        // The registering thread, re-entered: it must return at once. A regression here
        // hangs the test rather than failing it — which is the correct shape, since the
        // defect it guards against is a self-deadlock.
        await_attempt(&guard, true);

        // A different thread must not get past the window.
        let observed = Arc::new(AtomicBool::new(false));
        let handle = {
            let (g, done) = (Arc::clone(&guard), Arc::clone(&observed));
            std::thread::spawn(move || {
                await_attempt(&g, false);
                done.store(true, O::Release);
            })
        };

        // Give the observer room to run and (incorrectly) finish. Not a race: the
        // assertion is that it has *not* completed, so a slow thread cannot make this
        // pass spuriously — only the fix keeps it parked.
        for _ in 0..1000 {
            std::thread::yield_now();
        }
        assert!(
            !observed.load(O::Acquire),
            "an observer proceeded while registration was still in flight"
        );

        // Settling the attempt releases it — on failure as well as success, so an
        // attempt that keeps failing cannot park a caller forever.
        guard.publish(false);
        handle.join().expect("observer thread");
        assert!(observed.load(O::Acquire));
        assert!(!guard.registered(), "a failed attempt installs nothing");
    }

    /// W16-5 (#4): the installer must run with the **bootstrap window open**, so an
    /// allocation `pthread_atfork` makes cannot re-enter `global()` and build the whole
    /// allocator before the handlers are installed. A `fork()` from another thread during
    /// that build strands the child on a half-constructed `OnceLock` with no quiesce
    /// handler — the exact hazard the eager `.init_array` ctor exists to close, reached
    /// whenever that ctor was elided.
    ///
    /// Marking the thread as the claimant (the previous fix) stopped it *waiting for
    /// itself*, but it still returned and walked on into the init. Closing the re-entry
    /// is what this pins.
    ///
    /// Driven over a **private** guard and a stand-in installer, for the reason the
    /// sibling tests give: the process-global `ATFORK` is consulted by `global()`'s fast
    /// path on every allocating test.
    #[cfg(unix)]
    #[test]
    fn a_registration_attempt_runs_its_installer_inside_the_bootstrap_window() {
        let guard = AtforkGuard::new();
        assert!(
            !crate::in_bootstrap_window(),
            "precondition: no window open on this thread"
        );

        let mut window_was_open = false;
        let mut was_registering = false;
        let ok = run_attempt(&guard, || {
            window_was_open = crate::in_bootstrap_window();
            was_registering = REGISTERING.with(core::cell::Cell::get);
            0 // stand in for a successful `pthread_atfork`
        });

        assert!(ok, "a claimed attempt reporting success installs");
        assert!(guard.registered());
        assert!(
            window_was_open,
            "the installer ran outside the bootstrap window: an allocation it makes \
             re-enters global() and initializes GLOBAL with no fork handlers installed"
        );
        assert!(
            was_registering,
            "the installer must also be marked as the registering thread"
        );
        assert!(
            !crate::in_bootstrap_window(),
            "the window must close again, or every later allocation on this thread \
             bypasses the engine"
        );
    }

    /// §29.5/§30.4: a fork child must bank fork-distinct entropy **even while deterministic
    /// mode defers applying it**. Skipping the refresh left parent and child holding the
    /// same `process_entropy`, so a later *disable* in both — the path that advertises
    /// unpredictability restored — re-derived the guard coin, the quarantine evictor and
    /// the heap sampler from one identical value.
    ///
    /// `deterministic` is passed rather than set, so this does not disturb the
    /// process-global mode every parallel test would see. Refreshing the stored entropy is
    /// itself harmless to siblings: a fresh per-process secret is what the value is for,
    /// and nothing asserts a particular one.
    ///
    /// Fails with the refresh back inside the deterministic guard: the entropy is
    /// unchanged.
    #[cfg(unix)]
    #[test]
    fn a_fork_child_banks_fresh_entropy_even_when_deterministic_mode_defers_applying_it() {
        let Some(eng) = crate::global() else {
            return; // no engine on this host: nothing to exercise
        };
        let before = topo_core::harden::process_entropy();
        refresh_child_entropy(eng, /* deterministic */ true);
        let after = topo_core::harden::process_entropy();
        assert_ne!(
            before, after,
            "a deterministic-mode child kept the parent's process entropy, so disabling \
             deterministic mode in both would hand them identical 'unpredictable' streams"
        );
    }

    #[test]
    fn crash_summary_c_api_writes_bounded() {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid 256-byte writable region. `c_char` is `i8` on
        // some targets and `u8` on others (AArch64), so cast the byte pointer to
        // `c_char` rather than assuming the buffer's element type matches.
        let n = unsafe {
            topomalloc_crash_summary(buf.as_mut_ptr().cast::<core::ffi::c_char>(), buf.len())
        };
        assert!(n > 0 && n <= buf.len());
        // SAFETY: `n <= buf.len()`; the written bytes are ASCII.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(buf.as_ptr(), n) };
        let text = core::str::from_utf8(bytes).unwrap();
        assert!(text.contains("init_phase="));
        assert!(text.contains("live_bytes="));
    }

    #[test]
    fn crash_summary_c_api_handles_null_and_zero() {
        // SAFETY: a null buffer is explicitly handled (returns 0, writes nothing).
        let n_null = unsafe { topomalloc_crash_summary(core::ptr::null_mut(), 16) };
        assert_eq!(n_null, 0);
        let mut buf = [0u8; 8];
        // SAFETY: zero length writes nothing, regardless of the (valid) buffer.
        // (`c_char` signedness varies by target — cast the byte pointer to it.)
        let n_zero =
            unsafe { topomalloc_crash_summary(buf.as_mut_ptr().cast::<core::ffi::c_char>(), 0) };
        assert_eq!(n_zero, 0);
    }
}

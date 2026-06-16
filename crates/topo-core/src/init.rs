// SPDX-License-Identifier: MIT
//! Process initialization phases, the re-entrancy guard, and the lock-free crash
//! summary (W16-6 / W16-7, plan 05; §35.4, §28.2–§28.4).
//!
//! # Initialization phases (W16-7, §35.4)
//!
//! The allocator brings itself up through seven ordered phases, each of which
//! MUST be re-entrancy-safe (a reentrant allocation arriving mid-phase must not
//! corrupt the bring-up):
//!
//! ```text
//! Phase 0: static zero state          (nothing initialized; the const default)
//! Phase 1: bootstrap metadata allocator (the §17.4 bump arena — S-007)
//! Phase 2: OS feature discovery        (topology, hugepages, rseq availability)
//! Phase 3: arena registry + default arena
//! Phase 4: per-CPU / RSEQ setup
//! Phase 5: background threads + profiling
//! Phase 6: normal operation
//! ```
//!
//! [`PhaseTracker`] is a single lock-free `AtomicU8`: bring-up advances it
//! **monotonically** ([`advance_to`](PhaseTracker::advance_to) never goes
//! backwards), so the phase is readable from any thread without a lock and a
//! reentrant caller can ask "are we open for business yet?"
//! ([`PhaseTracker::is_operational`]).
//! It is the observable spine of the bootstrap that `topo-abi`'s lazy global
//! initializer already performs under a re-entrancy guard.
//!
//! # Re-entrancy guard (W16-6, §28.3)
//!
//! [`ReentryGuard`] is the general per-thread re-entrancy detector the hook,
//! profiling-callback, and error-reporting paths use: if code inside one of
//! those callbacks re-enters the same guarded domain on the same thread, the
//! guard refuses (returns `None`) rather than recursing — the threading analogue
//! of the bootstrap S-007 backstop. It is allocation-free (a `const`-initialised
//! thread-local flag, Local-Exec TLS), so arming it never re-enters `malloc`
//! even when this crate backs the process `#[global_allocator]`.
//!
//! # Crash summary (W16-6, §28.4)
//!
//! [`CrashSummary`] is a fixed, `Copy` snapshot of the allocator's top-level
//! counters that can be produced **without taking any lock or allocating** — for
//! a signal/crash handler, where the contended structure locks may be held by the
//! faulting thread and `malloc` is not async-signal-safe (§28.2). It carries only
//! values read from process-lifetime atomics; [`CrashSummary::write`] formats it
//! into a caller-provided byte buffer with no allocation.

use core::sync::atomic::{AtomicU8, Ordering};

/// The §35.4 initialization phases (W16-7). Ordered; bring-up advances through
/// them monotonically. `repr(u8)` so a [`PhaseTracker`] stores one atomically.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum InitPhase {
    /// Phase 0: static zero state — nothing initialized (the `const` default).
    StaticZero = 0,
    /// Phase 1: the bootstrap metadata allocator is live (§17.4, S-007).
    BootstrapMetadata = 1,
    /// Phase 2: OS feature discovery (topology, hugepages, rseq availability).
    OsDiscovery = 2,
    /// Phase 3: the arena registry and default arena exist (§22).
    ArenaRegistry = 3,
    /// Phase 4: per-CPU / RSEQ fast-path setup (plan 05 W7).
    PerCpuSetup = 4,
    /// Phase 5: background threads and profiling are armed (§31, W17).
    BackgroundAndProfiling = 5,
    /// Phase 6: normal operation — every subsystem is up.
    Operational = 6,
}

impl InitPhase {
    /// The phase for a raw `u8`, saturating an out-of-range value at
    /// [`Operational`](InitPhase::Operational) (forward-compatible).
    #[inline]
    #[must_use]
    pub const fn from_u8(v: u8) -> InitPhase {
        match v {
            0 => InitPhase::StaticZero,
            1 => InitPhase::BootstrapMetadata,
            2 => InitPhase::OsDiscovery,
            3 => InitPhase::ArenaRegistry,
            4 => InitPhase::PerCpuSetup,
            5 => InitPhase::BackgroundAndProfiling,
            _ => InitPhase::Operational,
        }
    }
}

/// A lock-free, monotone tracker of the current [`InitPhase`] (W16-7). One
/// `AtomicU8`, so the phase is readable from any thread and from a reentrant
/// callback without taking a lock (§35.4 "each phase MUST be reentrancy-safe").
pub struct PhaseTracker {
    phase: AtomicU8,
}

impl PhaseTracker {
    /// A tracker in [`InitPhase::StaticZero`] (usable as a `static`).
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: AtomicU8::new(InitPhase::StaticZero as u8),
        }
    }

    /// The current phase (acquire-loaded, so a reader that observes a phase also
    /// observes the bring-up work that phase published).
    #[inline]
    #[must_use]
    pub fn current(&self) -> InitPhase {
        InitPhase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Advance to `phase` **monotonically**: a later phase is published with a
    /// release store; a request for an earlier-or-equal phase is a no-op (so a
    /// reentrant or racing caller can never wind the phase backwards). Returns
    /// `true` iff this call advanced it.
    ///
    /// SPEC-transition: init phase advance (§35.4)
    pub fn advance_to(&self, phase: InitPhase) -> bool {
        let target = phase as u8;
        let mut cur = self.phase.load(Ordering::Acquire);
        loop {
            if cur >= target {
                return false; // already at or past this phase — no-op (monotone)
            }
            match self.phase.compare_exchange_weak(
                cur,
                target,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Whether the allocator has reached [`InitPhase::Operational`].
    #[inline]
    #[must_use]
    pub fn is_operational(&self) -> bool {
        self.current() == InitPhase::Operational
    }
}

impl Default for PhaseTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide initialization-phase tracker (W16-7). `topo-abi`'s lazy
/// global initializer advances it through the §35.4 phases; the control plane and
/// crash summary read it.
pub static INIT_PHASE: PhaseTracker = PhaseTracker::new();

// ---------------------------------------------------------------------------
// Re-entrancy guard (W16-6, §28.3). Per-thread, allocation-free; a no-op in pure
// `no_std` (no thread-local), like the bootstrap S-007 guard.
// ---------------------------------------------------------------------------

/// A general per-thread re-entrancy guard (W16-6). Construct one with
/// [`enter`](ReentryGuard::enter) around a region — a hook call, a profiling
/// callback, an error report — that must not be re-entered on the same thread; a
/// nested `enter` returns `None`, so the caller fails safe instead of recursing
/// into the allocator (§28.3). The guard clears the flag on drop (including on a
/// panic-unwind).
///
/// Each *domain* gets its own static flag via [`reentry_domain!`]; this type is
/// the RAII handle. It is allocation-free: the flag is a `const`-initialised
/// thread-local `Cell<bool>`, so arming the guard never calls `malloc`.
#[cfg(any(test, feature = "std"))]
pub struct ReentryGuard {
    // The flag to clear on drop. `'static` so the guard can outlive any borrow.
    clear: fn(),
}

#[cfg(any(test, feature = "std"))]
impl Drop for ReentryGuard {
    #[inline]
    fn drop(&mut self) {
        (self.clear)();
    }
}

#[cfg(any(test, feature = "std"))]
impl ReentryGuard {
    /// Construct a guard from a domain's `(check, set, clear)` triple. Returns
    /// `None` if the domain is already active on this thread (a re-entry).
    /// Prefer the [`reentry_domain!`] macro, which wires the thread-local.
    #[inline]
    #[must_use]
    pub fn from_parts(active: fn() -> bool, set: fn(), clear: fn()) -> Option<ReentryGuard> {
        if active() {
            return None;
        }
        set();
        Some(ReentryGuard { clear })
    }
}

/// Define a re-entrancy `domain`: a function `name() -> Option<ReentryGuard>` that
/// yields a guard the first time it is called on a thread and `None` while a
/// guard from the same domain is still alive on that thread (W16-6, §28.3). Pure
/// `no_std` builds (no thread-local) get a no-op that always yields a guard — the
/// guarded code is then responsible for not recursing, as for the bootstrap leaf.
#[macro_export]
macro_rules! reentry_domain {
    ($(#[$meta:meta])* $vis:vis fn $name:ident) => {
        $(#[$meta])*
        #[cfg(any(test, feature = "std"))]
        $vis fn $name() -> ::core::option::Option<$crate::init::ReentryGuard> {
            ::std::thread_local! {
                static ACTIVE: ::core::cell::Cell<bool> = const { ::core::cell::Cell::new(false) };
            }
            fn active() -> bool { ACTIVE.with(::core::cell::Cell::get) }
            fn set() { ACTIVE.with(|f| f.set(true)); }
            fn clear() { ACTIVE.with(|f| f.set(false)); }
            $crate::init::ReentryGuard::from_parts(active, set, clear)
        }

        $(#[$meta])*
        #[cfg(not(any(test, feature = "std")))]
        $vis fn $name() -> ::core::option::Option<$crate::init::ReentryGuard> {
            // No thread-local without `std`: the guard is a no-op marker. The
            // `()` placeholder keeps the signature uniform; the guarded code must
            // not recurse (the leaf contract).
            ::core::option::Option::Some($crate::init::ReentryGuard)
        }
    };
}

/// `no_std` placeholder for [`ReentryGuard`] (a unit marker; the macro yields it).
#[cfg(not(any(test, feature = "std")))]
pub struct ReentryGuard;

// ---------------------------------------------------------------------------
// Crash summary (W16-6, §28.4): lock-free, allocation-free.
// ---------------------------------------------------------------------------

/// A minimal, `Copy` snapshot of the allocator's top-level counters that can be
/// produced **without any lock or allocation** (W16-6, §28.4) — for a crash or
/// signal handler, where `malloc` is unsafe (§28.2) and the structure locks may
/// be held by the faulting thread. Every field is read from a process-lifetime
/// atomic; full stats (which take locks) are unavailable in that context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrashSummary {
    /// The current [`InitPhase`] as a raw `u8` (so the struct stays `Copy`/plain).
    pub init_phase: u8,
    /// Cumulative usable bytes ever handed out (§31.1).
    pub allocated_bytes: u64,
    /// Cumulative usable bytes ever freed.
    pub freed_bytes: u64,
    /// Live bytes (`allocated − freed`); the headline "where is the memory".
    pub live_bytes: u64,
    /// Whether background maintenance is currently enabled (cleared post-fork).
    pub background_enabled: bool,
}

impl CrashSummary {
    /// Format the summary into `buf` as ASCII `key=value` pairs without
    /// allocating, returning the number of bytes written (truncated to fit, never
    /// overflowing `buf`). Async-signal-safe: it only reads `self` and writes
    /// bytes. Returns `0` for an empty buffer.
    pub fn write(&self, buf: &mut [u8]) -> usize {
        let mut w = Cursor::new(buf);
        w.str("topomalloc crash summary\n");
        w.kv_u64("init_phase", u64::from(self.init_phase));
        w.kv_u64("live_bytes", self.live_bytes);
        w.kv_u64("allocated_bytes", self.allocated_bytes);
        w.kv_u64("freed_bytes", self.freed_bytes);
        w.kv_u64("background_enabled", u64::from(self.background_enabled));
        w.len
    }
}

/// A bounded, allocation-free byte-buffer writer for [`CrashSummary::write`].
/// Every method clamps to the buffer, so it can never overflow (§28.4: a crash
/// path must not fault).
struct Cursor<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Append `s`, truncating at the buffer end.
    fn str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if self.len >= self.buf.len() {
                return;
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    /// Append `key=value\n`, decimal, allocation-free.
    fn kv_u64(&mut self, key: &str, value: u64) {
        self.str(key);
        self.str("=");
        // Decimal digits into a small stack buffer (u64::MAX is 20 digits).
        let mut digits = [0u8; 20];
        let mut n = value;
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        for &d in &digits[i..] {
            if self.len >= self.buf.len() {
                return;
            }
            self.buf[self.len] = d;
            self.len += 1;
        }
        self.str("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_ordered() {
        assert!(InitPhase::StaticZero < InitPhase::BootstrapMetadata);
        assert!(InitPhase::BootstrapMetadata < InitPhase::OsDiscovery);
        assert!(InitPhase::OsDiscovery < InitPhase::ArenaRegistry);
        assert!(InitPhase::ArenaRegistry < InitPhase::PerCpuSetup);
        assert!(InitPhase::PerCpuSetup < InitPhase::BackgroundAndProfiling);
        assert!(InitPhase::BackgroundAndProfiling < InitPhase::Operational);
    }

    #[test]
    fn phase_tracker_advances_monotonically() {
        let t = PhaseTracker::new();
        assert_eq!(t.current(), InitPhase::StaticZero);
        assert!(t.advance_to(InitPhase::ArenaRegistry));
        assert_eq!(t.current(), InitPhase::ArenaRegistry);
        // Cannot wind backwards.
        assert!(!t.advance_to(InitPhase::BootstrapMetadata));
        assert_eq!(t.current(), InitPhase::ArenaRegistry);
        // Idempotent at the same phase.
        assert!(!t.advance_to(InitPhase::ArenaRegistry));
        // Advances forward.
        assert!(t.advance_to(InitPhase::Operational));
        assert!(t.is_operational());
    }

    #[test]
    fn from_u8_saturates() {
        assert_eq!(InitPhase::from_u8(0), InitPhase::StaticZero);
        assert_eq!(InitPhase::from_u8(6), InitPhase::Operational);
        assert_eq!(InitPhase::from_u8(200), InitPhase::Operational);
    }

    #[test]
    fn phase_tracker_is_concurrent_monotone() {
        use std::sync::Arc;
        let t = Arc::new(PhaseTracker::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let t = t.clone();
                std::thread::spawn(move || {
                    for p in [
                        InitPhase::BootstrapMetadata,
                        InitPhase::OsDiscovery,
                        InitPhase::ArenaRegistry,
                        InitPhase::Operational,
                    ] {
                        t.advance_to(p);
                        // The observed phase never decreases.
                        assert!(t.current() >= p || t.current() == InitPhase::Operational);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(t.is_operational());
    }

    reentry_domain! {
        /// Test domain for the re-entrancy guard.
        fn test_domain
    }

    #[cfg(any(test, feature = "std"))]
    #[test]
    fn reentry_guard_refuses_nested_entry() {
        let outer = test_domain();
        assert!(outer.is_some(), "first entry yields a guard");
        // A nested entry on the same thread is refused.
        assert!(test_domain().is_none(), "re-entry is refused (fails safe)");
        drop(outer);
        // After the outer guard drops, the domain is enterable again.
        assert!(test_domain().is_some(), "guard released on drop");
    }

    #[test]
    fn crash_summary_writes_without_alloc() {
        let s = CrashSummary {
            init_phase: InitPhase::Operational as u8,
            allocated_bytes: 4096,
            freed_bytes: 1024,
            live_bytes: 3072,
            background_enabled: true,
        };
        let mut buf = [0u8; 256];
        let n = s.write(&mut buf);
        let text = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(text.contains("live_bytes=3072"));
        assert!(text.contains("init_phase=6"));
        assert!(text.contains("allocated_bytes=4096"));
        assert!(text.contains("background_enabled=1"));
    }

    #[test]
    fn crash_summary_write_never_overflows_a_short_buffer() {
        let s = CrashSummary {
            live_bytes: u64::MAX,
            ..Default::default()
        };
        // A buffer far too small must be filled exactly and never overrun.
        let mut buf = [0u8; 8];
        let n = s.write(&mut buf);
        assert!(n <= buf.len());
    }
}

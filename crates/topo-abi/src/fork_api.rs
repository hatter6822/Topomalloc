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
use core::sync::atomic::{AtomicBool, Ordering};

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

/// `pthread_atfork` child handler (post-fork in the child): reset lock/gate state
/// and disable background threads until the host re-arms them (§28.1). The
/// parent's pre-fork quiesce guarantees the child's structures are consistent.
#[cfg(unix)]
extern "C" fn atfork_child() {
    topo_core::fork::postfork_child();
}

#[cfg(unix)]
static ATFORK_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Install the `pthread_atfork` handlers exactly once (W16-5). Idempotent and
/// **re-entrancy-safe**: the guard flag is claimed *before* `pthread_atfork` is
/// called, so if that call itself allocates — re-entering this function through the
/// global allocator during the load-time ctor — the nested call observes the flag
/// and returns without a second registration. (A blocking `Once` would instead
/// dead-lock on such re-entry.) Installed eagerly at load by [`REGISTER_ATFORK_CTOR`]
/// and, as a fallback, on the first `global()` call. A no-op on non-unix hosts (no
/// `fork`).
pub(crate) fn register_atfork_handlers() {
    #[cfg(unix)]
    if ATFORK_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        // SAFETY: the three handlers are `extern "C"` functions with no captured
        // state that only call the allocation-free `topo_core::fork::*` routines;
        // `pthread_atfork` simply records them. A non-zero return (out of memory
        // for the handler list) is ignored — the allocator still works, just
        // without fork hardening, which is the safe degradation.
        unsafe {
            libc::pthread_atfork(
                Some(atfork_prepare),
                Some(atfork_parent),
                Some(atfork_child),
            );
        }
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
        #[cfg(unix)]
        assert!(ATFORK_REGISTERED.load(Ordering::Acquire));
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

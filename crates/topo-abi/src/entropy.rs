// SPDX-License-Identifier: MIT
//! Host entropy for the randomized security samplers (§29.4/§29.5, plan 08 W18).
//!
//! The W18-4 guard-page coin and the W18-3 quarantine sampler/evictor rest on
//! **unpredictability**: an attacker who can compute which allocations receive a guard
//! page simply avoids those slots, and one who can compute the eviction order can force
//! an early reuse. Their state lives in [`topo_core::harden`], which is `no_std` and so
//! cannot read the OS — it ships a compile-time seed, which is the *same* stream in
//! every process of the same binary, i.e. public. This module is the hosted side: it
//! reads real OS entropy once and pushes it into the core through
//! [`topo_core::harden::set_process_entropy`].
//!
//! **Allocation-free.** It runs inside the global initializer (and, when TopoMalloc is
//! the process `#[global_allocator]`, inside its bootstrap window), so it uses raw
//! `libc` only — no `String`, no `Vec`, no `std::env`.
//!
//! **Sources, best-effort and in order.** `getauxval(AT_RANDOM)` (glibc/Linux: 16 bytes
//! the kernel seeds at exec, the same pool the stack canary uses) is preferred; failing
//! that, `getrandom(2)`; failing that, an ASLR/clock mix (the address of a stack local
//! and `CLOCK_MONOTONIC`), which is weak but still process-unique. If every source
//! fails the core simply keeps its build-time seed — the samplers stay correct, only
//! their unpredictability is unimproved.
//!
//! Deterministic mode (§30.4) is unaffected: it re-seeds afterwards from the explicit
//! global seed, and the initializer applies it last.

/// Whether this process is running with **elevated privilege it did not inherit from
/// its caller** — a setuid/setgid exec, or one with capabilities/MAC labels raised by
/// the kernel (`AT_SECURE`).
///
/// The environment belongs to whoever performed the `exec`, so in a secure execution
/// every `TOPOMALLOC_*` tunable is attacker-controlled input to a privileged process.
/// glibc's malloc ignores its `MALLOC_*` tunables under exactly this condition, and for
/// the same reason: `TOPOMALLOC_DETERMINISTIC_SEED` would pin the guard-page sampler and
/// the quarantine evictor to a seed the attacker computes offline (turning a
/// probabilistic defence into a known schedule), `TOPOMALLOC_GUARD_SAMPLE_RATE=0` would
/// switch guard pages off outright, and `TOPOMALLOC_SAMPLE_RATE=1` would force stack
/// unwinding on effectively every allocation.
///
/// `getauxval(AT_SECURE)` is authoritative where available; elsewhere the classic
/// `euid != uid || egid != gid` test is the portable approximation. On a host with
/// neither, the answer is `false` (no elevation to protect against).
#[must_use]
pub(crate) fn is_secure_execution() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const AT_SECURE: libc::c_ulong = 23;
        // SAFETY: a pure query of this process's auxiliary vector; absent keys read `0`.
        if unsafe { libc::getauxval(AT_SECURE) } != 0 {
            return true;
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: all four are argument-less, always-succeeding POSIX id queries.
        unsafe { libc::geteuid() != libc::getuid() || libc::getegid() != libc::getgid() }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Read this process's entropy and install it in the core.
///
/// Idempotent in effect (the last writer wins). Never allocates and never fails
/// observably — a host with no entropy source leaves the build-time seed in place.
pub(crate) fn install_process_entropy() {
    let e = read_entropy();
    topo_core::harden::set_process_entropy(e);
}

/// Best-effort per-process entropy; `0` when nothing could be read (the
/// "not installed" sentinel the core understands).
fn read_entropy() -> u64 {
    if let Some(v) = from_auxv() {
        return v;
    }
    if let Some(v) = from_getrandom() {
        return v;
    }
    from_address_and_clock()
}

/// `getauxval(AT_RANDOM)` — a pointer to 16 kernel-seeded random bytes placed in the
/// auxiliary vector at `execve` (glibc/Linux). `None` off glibc/Linux, or when the
/// auxv entry is absent (`getauxval` answers `0`).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn from_auxv() -> Option<u64> {
    const AT_RANDOM: libc::c_ulong = 25;
    // SAFETY: `getauxval` is a pure query of this process's auxiliary vector; it
    // returns `0` for an absent key rather than failing.
    let p = unsafe { libc::getauxval(AT_RANDOM) } as *const u8;
    if p.is_null() {
        return None;
    }
    let mut buf = [0u8; 8];
    // SAFETY: a non-null `AT_RANDOM` points at 16 readable bytes for the process's
    // lifetime; we read the first 8, which is in bounds and correctly aligned for `u8`.
    unsafe { core::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), 8) };
    let v = u64::from_ne_bytes(buf);
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn from_auxv() -> Option<u64> {
    None
}

/// `getrandom(2)` — the kernel CSPRNG. `None` when the syscall is unavailable
/// (pre-3.17 Linux, a seccomp filter, or a non-Linux host) or returns short.
#[cfg(target_os = "linux")]
fn from_getrandom() -> Option<u64> {
    let mut buf = [0u8; 8];
    // SAFETY: writes at most `buf.len()` bytes into a live local buffer. `GRND_NONBLOCK`
    // keeps this from stalling early boot before the pool is initialized.
    let n = unsafe {
        libc::syscall(
            libc::SYS_getrandom,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
            libc::GRND_NONBLOCK,
        )
    };
    if n != buf.len() as libc::c_long {
        return None;
    }
    let v = u64::from_ne_bytes(buf);
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

#[cfg(not(target_os = "linux"))]
fn from_getrandom() -> Option<u64> {
    None
}

/// Last resort: mix an ASLR-dependent address with a monotonic clock reading. Weak (an
/// attacker who knows the layout learns most of it), but still process-unique — strictly
/// better than a constant shared by every process of the binary.
fn from_address_and_clock() -> u64 {
    let anchor = 0u8;
    let addr = core::ptr::addr_of!(anchor) as u64;
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes the two fields of a live `timespec`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let clock = (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64);
    let mixed = addr ^ clock.rotate_left(32);
    if mixed == 0 {
        1
    } else {
        mixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_is_nonzero_and_installed() {
        // Some source must answer on a normal host, and the core must record it.
        let v = read_entropy();
        assert_ne!(v, 0, "no entropy source produced a value");
        install_process_entropy();
        assert_ne!(topo_core::harden::process_entropy(), 0, "entropy installed");
    }

    #[test]
    fn derived_sampler_seeds_are_distinct_per_domain() {
        install_process_entropy();
        let g = topo_core::harden::entropy_seed(topo_core::deterministic::salt::GUARD);
        let q = topo_core::harden::entropy_seed(topo_core::deterministic::salt::QUARANTINE);
        let (g, q) = (g.expect("guard seed"), q.expect("quarantine seed"));
        assert_ne!(g, 0);
        assert_ne!(q, 0);
        assert_ne!(g, q, "distinct salts must give decorrelated streams");
    }

    #[test]
    fn ordinary_execution_is_not_flagged_secure() {
        // The test binary is not setuid and has no raised capabilities, so environment
        // tunables must remain honoured here — otherwise the gate would silently disable
        // every `TOPOMALLOC_*` knob for ordinary users.
        assert!(
            !is_secure_execution(),
            "an ordinary test process must not be treated as a secure execution"
        );
    }

    #[test]
    fn a_zero_install_is_ignored_as_the_not_installed_sentinel() {
        install_process_entropy();
        let before = topo_core::harden::process_entropy();
        assert_ne!(before, 0);
        topo_core::harden::set_process_entropy(0);
        assert_eq!(
            topo_core::harden::process_entropy(),
            before,
            "0 is the sentinel, not a value"
        );
    }
}

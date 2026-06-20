// SPDX-License-Identifier: MIT
//! W18 hardening integration tests over the **real POSIX provider** (plan 08, §29).
//!
//! These exercise behaviour the in-crate `HostProvider` unit tests cannot: the W18-4
//! guarded allocations' real `mprotect` trap (a forked-child "death test"), and the
//! live quarantine/scrub/junk-fill over the global allocator. Gated on the matching
//! hardening feature so the default suite is unaffected.

/// W18-4 (§29.5): a guarded allocation's trailing/leading **guard page** is `PROT_NONE`,
/// so an overrun or underrun faults with `SIGSEGV` — the protection is real, not
/// advisory, over the POSIX provider. A forked-child death test: the child performs
/// the out-of-bounds write and must be killed by `SIGSEGV`; if it survives (no page
/// protection) it exits with a distinct code the parent rejects.
#[cfg(all(feature = "guard-pages", unix))]
#[test]
fn guarded_overrun_and_underrun_trap_with_sigsegv() {
    use topo_abi::{topo_dallocx, topo_mallocx, topomalloc_malloc_usable_size, TOPO_GUARDED};

    // Allocate a guarded object, fork, and have the child write into a guard page —
    // either just past the object (`overrun`, the trailing guard at `base + usable`)
    // or just before it (`underrun`, the leading guard at `base - 1`). Returns whether
    // the child died from SIGSEGV.
    fn child_faults_at_guard(overrun: bool) -> bool {
        // The child inherits the mapping (and its `PROT_NONE` guards) across `fork`.
        let p = topo_mallocx(100, TOPO_GUARDED);
        assert!(!p.is_null(), "guarded allocation");
        let usable = topomalloc_malloc_usable_size(p);
        let base = p as usize;
        // An address inside a guard page on the chosen side of the object.
        let target = if overrun { base + usable } else { base - 1 };

        // SAFETY: a controlled death test. The child only writes to `target` (a guard
        // page) and `_exit`s — no allocator calls, no locks — so it either faults or
        // exits cleanly; the parent reaps it.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            // Child: the deliberate out-of-bounds write. On a real `mprotect` guard
            // this raises SIGSEGV before the next line.
            // SAFETY: intentionally out of bounds — the guard exists to catch it.
            unsafe { (target as *mut u8).write_volatile(0xFF) };
            // Reached only if the guard did not trap (no page protection).
            // SAFETY: `_exit` is async-signal-safe and ends the child immediately.
            unsafe { libc::_exit(3) };
        }
        // Parent: reap the child and free the (still-guarded, untouched-in-parent)
        // object, then report whether the child died from SIGSEGV.
        let mut status = 0i32;
        // SAFETY: `pid` is our child; `status` is a live `i32`.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(r, pid, "waitpid");
        // SAFETY: `p` is a live guarded allocation of this process (never written here).
        unsafe { topo_dallocx(p, 0) };
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV
    }

    assert!(
        child_faults_at_guard(true),
        "an overrun into the trailing guard page must trap as SIGSEGV"
    );
    assert!(
        child_faults_at_guard(false),
        "an underrun into the leading guard page must trap as SIGSEGV"
    );
}

/// Isolation check: the POSIX provider's `protect` (W18-4 seam) really makes a page
/// fault on access — independent of the allocator wiring.
#[cfg(all(feature = "guard-pages", unix))]
#[test]
fn posix_protect_makes_a_page_inaccessible() {
    use topo_backend_posix::PosixBackingProvider;
    use topo_core::{ArenaId, TopoBackingProvider};

    let prov = PosixBackingProvider::new();
    let region = prov.reserve(ArenaId::DEFAULT, 64 * 1024, 16384).unwrap();
    // Make the second 16 KiB page inaccessible.
    prov.protect(region, 16384, 16384, false).unwrap();
    let guard = region.base as usize + 16384;
    // SAFETY: a controlled death test; the child writes the protected page and exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        // SAFETY: deliberately writing the PROT_NONE page — must fault.
        unsafe { (guard as *mut u8).write_volatile(1) };
        // SAFETY: async-signal-safe exit if it did not fault.
        unsafe { libc::_exit(3) };
    }
    let mut status = 0i32;
    // SAFETY: reaping our child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    prov.release(ArenaId::DEFAULT, region).unwrap();
    assert!(
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGSEGV,
        "protect(PROT_NONE) must make the page fault (status={status:#x})"
    );
}

/// W18-3 (§29.4): the quarantine is opt-in via `$TOPOMALLOC_QUARANTINE` / the C
/// control surface, holds freed objects (so `quarantine.bytes > 0`), and disabling
/// drains it — over the live global allocator.
#[cfg(feature = "quarantine")]
#[test]
fn quarantine_control_surface_holds_and_drains() {
    use topo_abi::{
        topomalloc_free, topomalloc_malloc, topomalloc_quarantine_bytes,
        topomalloc_quarantine_enabled, topomalloc_quarantine_set_enabled,
    };

    topomalloc_quarantine_set_enabled(1);
    assert_eq!(topomalloc_quarantine_enabled(), 1, "quarantine enabled");
    // Free a batch; the quarantine should hold some bytes out of circulation.
    for _ in 0..32 {
        let p = topomalloc_malloc(256);
        assert!(!p.is_null());
        // SAFETY: `p` is a live allocation of ours.
        unsafe { topomalloc_free(p) };
    }
    assert!(
        topomalloc_quarantine_bytes() > 0,
        "freed objects are held in quarantine"
    );
    // Disabling drains everything back.
    topomalloc_quarantine_set_enabled(0);
    assert_eq!(
        topomalloc_quarantine_bytes(),
        0,
        "disable drains the quarantine"
    );
    assert_eq!(topomalloc_quarantine_enabled(), 0);
}

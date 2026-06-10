// SPDX-License-Identifier: MIT
//! The W8-4 zero-size policy matrix (§9.6 / F-004), in its **own integration
//! binary** so it runs in its own process: the policy is process-global, and
//! flipping it here can never race the other test binaries' allocations (the
//! W8 self-audit replaced the in-crate flip test with this isolation).

use std::ffi::c_void;
use std::ptr;

use topo_abi::{
    set_zero_size_policy, topo_mallocx, topo_nallocx, topomalloc_aligned_alloc, topomalloc_calloc,
    topomalloc_free, topomalloc_malloc, topomalloc_memalign, topomalloc_posix_memalign,
    topomalloc_realloc, zero_size_policy, ZeroSizePolicy,
};

/// Test wrapper: pointers freed here are test-owned (the entry point's
/// documented safety contract).
fn free(p: *mut c_void) {
    // SAFETY: see above.
    unsafe { topomalloc_free(p) }
}

/// The thread's current errno (the same cell libc writes); 0 where the host
/// has no C errno (the shim's documented degradation).
fn errno() -> i32 {
    if cfg!(unix) {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    }
}

fn set_errno_zero() {
    // Best-effort errno clearing for the "NULL is not a failure" assertion;
    // platforms without the glibc accessor simply skip the pre-clear (the
    // `errno()` helper reads through std, which is portable).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: writing 0 to the thread's own errno cell.
    unsafe {
        *libc::__errno_location() = 0;
    }
}

/// One test drives the whole matrix sequentially: the policy is process-wide
/// state, so ordering within a single `#[test]` is the synchronization.
#[test]
fn zero_size_policy_matrix() {
    // --- default: unique (§9.6, the dominant-platform expectation) ---------
    assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
    let a = topomalloc_malloc(0);
    let b = topomalloc_malloc(0);
    assert!(!a.is_null() && !b.is_null());
    assert_ne!(a, b, "zero_unique pointers are distinct, freeable objects");
    free(a);
    free(b);
    let c = topomalloc_calloc(0, 64);
    assert!(!c.is_null());
    free(c);
    let m = topo_mallocx(0, 0);
    assert!(!m.is_null());
    free(m);
    assert!(
        topo_nallocx(0, 0) >= 1,
        "nallocx(0) predicts the unique minimum-size object"
    );
    let al = topomalloc_aligned_alloc(64, 0); // 0 is a multiple of 64
    assert!(!al.is_null());
    free(al);

    // --- flipped: null (compat.zero_null) -----------------------------------
    set_zero_size_policy(ZeroSizePolicy::Null);
    set_errno_zero();
    assert!(topomalloc_malloc(0).is_null());
    assert_eq!(errno(), 0, "a zero-size NULL is not a failure (§9.6)");
    assert!(topomalloc_calloc(0, 8).is_null());
    assert!(topomalloc_calloc(8, 0).is_null());
    assert!(topomalloc_aligned_alloc(64, 0).is_null());
    assert!(topomalloc_memalign(64, 0).is_null());
    assert!(topo_mallocx(0, 0).is_null());
    assert_eq!(topo_nallocx(0, 0), 0);
    let mut out: *mut c_void = ptr::null_mut();
    // SAFETY: `out` is a valid writable slot.
    unsafe {
        assert_eq!(topomalloc_posix_memalign(&mut out, 64, 0), 0);
    }
    assert!(out.is_null(), "POSIX: size 0 may yield NULL with success");

    // Fixed regardless of policy: free(NULL) no-op; realloc(p, 0) frees+NULL.
    free(ptr::null_mut());
    let p = topomalloc_malloc(64);
    assert!(!p.is_null(), "non-zero sizes are unaffected by the policy");
    // SAFETY: `p` is live and test-owned; realloc(p, 0) frees it.
    unsafe {
        assert!(topomalloc_realloc(p, 0).is_null());
    }

    // --- back to unique: behavior restores completely -----------------------
    set_zero_size_policy(ZeroSizePolicy::Unique);
    let q = topomalloc_malloc(0);
    assert!(!q.is_null());
    free(q);
}

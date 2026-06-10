// SPDX-License-Identifier: MIT
//! The §10.1 errno protocol (plan 06 W8-1b).
//!
//! POSIX requires: allocation failure sets `errno = ENOMEM`; `free` MUST NOT
//! modify `errno`; `realloc` failure sets `ENOMEM` and leaves the original
//! valid. The C entry points implement this with two combinators:
//!
//! * [`alloc_protocol`] — preserve the caller's `errno` across a *successful*
//!   allocation (the backend may issue syscalls that would otherwise clobber
//!   it), set `ENOMEM` on a null result.
//! * [`preserving_errno`] — save/restore `errno` unconditionally (the free
//!   family and the pure queries).
//!
//! Validation failures (`EINVAL` — e.g. a non-power-of-two alignment to
//! `aligned_alloc`) are set explicitly at the call site via [`set_errno`].
//!
//! On non-unix hosts the shim is a no-op: there is no C `errno` to honor, and
//! every entry point still returns its null/0 failure value.

#[cfg(unix)]
mod imp {
    /// `ENOMEM` for the §10.1 failure protocol.
    pub(crate) const ENOMEM: i32 = libc::ENOMEM;
    /// `EINVAL` for argument-validation failures (`aligned_alloc`,
    /// `posix_memalign`, invalid extended-API flag words).
    pub(crate) const EINVAL: i32 = libc::EINVAL;

    /// The calling thread's errno cell.
    fn errno_ptr() -> *mut i32 {
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "emscripten"))]
        // SAFETY: `__errno_location` returns the thread's errno cell, valid
        // for the thread's lifetime.
        unsafe {
            libc::__errno_location()
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "tvos",
            target_os = "watchos",
            target_os = "freebsd"
        ))]
        // SAFETY: `__error` returns the thread's errno cell.
        unsafe {
            libc::__error()
        }
    }

    pub(crate) fn get_errno() -> i32 {
        // SAFETY: the pointer is the thread's live errno cell.
        unsafe { *errno_ptr() }
    }

    pub(crate) fn set_errno(v: i32) {
        // SAFETY: the pointer is the thread's live errno cell.
        unsafe { *errno_ptr() = v };
    }
}

#[cfg(not(unix))]
mod imp {
    //! No C errno on this host: the protocol degrades to the return values
    //! alone (null / 0 / error codes), which every entry point already carries.
    pub(crate) const ENOMEM: i32 = 12;
    pub(crate) const EINVAL: i32 = 22;

    pub(crate) fn get_errno() -> i32 {
        0
    }

    pub(crate) fn set_errno(_v: i32) {}
}

pub(crate) use imp::{get_errno, set_errno, EINVAL, ENOMEM};

/// Run an allocation attempt under the §10.1 protocol: a null result sets
/// `errno = ENOMEM`; a success restores the errno the caller entered with
/// (backend syscalls on the path must not leak transient values).
pub(crate) fn alloc_protocol(f: impl FnOnce() -> *mut u8) -> *mut u8 {
    let saved = get_errno();
    let p = f();
    if p.is_null() {
        set_errno(ENOMEM);
    } else {
        set_errno(saved);
    }
    p
}

/// Run `f` with `errno` saved and restored unconditionally — the `free`
/// family ("free MUST NOT modify errno", §10.1) and pure queries.
pub(crate) fn preserving_errno<R>(f: impl FnOnce() -> R) -> R {
    let saved = get_errno();
    let r = f();
    set_errno(saved);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_protocol_sets_enomem_on_null_and_restores_on_success() {
        set_errno(7);
        let p = alloc_protocol(core::ptr::null_mut);
        assert!(p.is_null());
        assert_eq!(get_errno(), ENOMEM);

        set_errno(7);
        let mut byte = 0u8;
        let q = alloc_protocol(|| {
            // Simulate a backend syscall clobbering errno mid-allocation.
            set_errno(99);
            &mut byte as *mut u8
        });
        assert!(!q.is_null());
        assert_eq!(get_errno(), 7, "success must restore the caller's errno");
    }

    #[test]
    fn preserving_errno_round_trips() {
        set_errno(33);
        preserving_errno(|| set_errno(ENOMEM));
        assert_eq!(get_errno(), 33);
    }
}

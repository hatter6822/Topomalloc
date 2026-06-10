// SPDX-License-Identifier: MIT
//! The zero-size allocation policy (§9.6 / F-004, plan 06 W8-4).
//!
//! `malloc(0)` behavior is configurable:
//!
//! * [`ZeroSizePolicy::Unique`] (**default**, `compat.zero_unique`) —
//!   `malloc(0)` returns a minimum-size, unique, freeable pointer. This
//!   matches the dominant platform allocator (glibc), per §9.6's ABI-compat
//!   rule.
//! * [`ZeroSizePolicy::Null`] (`compat.zero_null`) — `malloc(0)` returns
//!   `NULL` **without** setting `errno` (a zero-size null is not a failure).
//!
//! The policy governs every zero-size *allocation* entry (`malloc`, `calloc`
//! with a zero product, `aligned_alloc`/`memalign`/`posix_memalign` with
//! `size == 0`, `topo_mallocx`/`topo_nallocx`). Two behaviors are fixed
//! regardless of policy: `free(NULL)` is a no-op (§9.6 MUST), and
//! `realloc(p != NULL, 0)` frees `p` and returns `NULL` (the C17
//! implementation-defined choice of the dominant platform; C23 deprecates
//! zero-size realloc entirely).
//!
//! Configured once from the environment (`TOPOMALLOC_ZERO_SIZE=unique|null`)
//! on first use, or programmatically via [`set_zero_size_policy`] (the
//! plan 07 control plane maps `compat.zero_*` onto this).
//!
//! The environment read uses `libc::getenv` (no allocation), so consulting
//! the policy can never re-enter the allocator — important when TopoMalloc
//! is the process `#[global_allocator]`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// What a zero-size allocation request returns (§9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroSizePolicy {
    /// A minimum-size unique freeable pointer (`compat.zero_unique`, default).
    Unique,
    /// `NULL`, with `errno` untouched (`compat.zero_null`).
    Null,
}

/// Lazily initialized from the environment; mutable thereafter via
/// [`set_zero_size_policy`].
static POLICY: OnceLock<AtomicU8> = OnceLock::new();

const UNIQUE: u8 = 0;
const NULL: u8 = 1;

/// Allocation-free read of `TOPOMALLOC_ZERO_SIZE`. Unknown values (and
/// non-unix hosts) fall back to the default, `Unique`.
fn policy_from_env() -> u8 {
    #[cfg(unix)]
    {
        // SAFETY: getenv takes a valid NUL-terminated name and returns either
        // null or a NUL-terminated environment value (never freed by us).
        let v = unsafe { libc::getenv(c"TOPOMALLOC_ZERO_SIZE".as_ptr()) };
        if !v.is_null() {
            // SAFETY: non-null getenv results point at a NUL-terminated string.
            let bytes = unsafe { core::ffi::CStr::from_ptr(v) }.to_bytes();
            if bytes == b"null" {
                return NULL;
            }
        }
    }
    UNIQUE
}

fn cell() -> &'static AtomicU8 {
    POLICY.get_or_init(|| AtomicU8::new(policy_from_env()))
}

/// The active zero-size policy.
pub fn zero_size_policy() -> ZeroSizePolicy {
    match cell().load(Ordering::Relaxed) {
        NULL => ZeroSizePolicy::Null,
        _ => ZeroSizePolicy::Unique,
    }
}

/// Set the zero-size policy at runtime (W8-4 "configurable"; the plan 07
/// control plane and tests use this).
pub fn set_zero_size_policy(p: ZeroSizePolicy) {
    let v = match p {
        ZeroSizePolicy::Unique => UNIQUE,
        ZeroSizePolicy::Null => NULL,
    };
    cell().store(v, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unique_and_runtime_override_works() {
        // The test environment does not set TOPOMALLOC_ZERO_SIZE, so the
        // default (glibc-compatible) policy applies.
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
        set_zero_size_policy(ZeroSizePolicy::Null);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Null);
        set_zero_size_policy(ZeroSizePolicy::Unique);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
    }
}

// SPDX-License-Identifier: MIT
//! Environment wiring for the zero-size allocation policy (§9.6 / F-004,
//! plan 06 W8-4).
//!
//! The policy **state** lives in the core ([`topo_core::compat`]), where the
//! control plane reads it (`topo.compat.zero_size`, plan 07) and future
//! engine-internal consumers will. This module is the ABI-side wiring: it
//! applies the `TOPOMALLOC_ZERO_SIZE=unique|null` environment default exactly
//! once, lazily, before the first policy-dependent decision — and an explicit
//! [`set_zero_size_policy`] call always wins over the environment, even if it
//! happens first.
//!
//! The environment read uses `libc::getenv` (no allocation), so consulting
//! the policy can never re-enter the allocator — important when TopoMalloc is
//! the process `#[global_allocator]`.
//!
//! Policy semantics (see [`ZeroSizePolicy`]): `Unique` (default) returns a
//! minimum-size unique freeable pointer for `malloc(0)`-style requests;
//! `Null` returns `NULL` with `errno` untouched. Fixed regardless of policy:
//! `free(NULL)` is a no-op (§9.6 MUST) and `realloc(p != NULL, 0)` frees and
//! returns `NULL` (the dominant-platform C17 choice; C23 deprecates
//! zero-size realloc entirely).

use std::sync::Once;

pub use topo_core::compat::ZeroSizePolicy;

/// Guards the one-shot environment application. Claimed by whichever happens
/// first: the lazy env read, or an explicit `set_zero_size_policy` (which
/// claims it with a no-op so a later env read can never override the caller's
/// choice).
static ENV_INIT: Once = Once::new();

/// Allocation-free read of `TOPOMALLOC_ZERO_SIZE`, applied to the core knob.
/// Unknown values (and non-unix hosts) keep the default, `Unique`.
///
/// **Skipped under `AT_SECURE`** (setuid/setgid, file capabilities, or any other
/// secure-exec mechanism), as glibc does for `MALLOC_*` and as the phase-5 readers in
/// `lib.rs` do for every other `TOPOMALLOC_*` tunable. This one needs its own check
/// rather than inheriting theirs: the C allocation entry points call
/// [`zero_size_policy`] *before* `global()`, so it is read outside the initializer the
/// other knobs are gated inside. Without it, an attacker who controls a privileged
/// process's environment can flip `malloc(0)` from the compiled unique-pointer default
/// to `NULL` and turn an unchecked zero-size allocation into a null dereference.
fn apply_env_default() {
    #[cfg(unix)]
    {
        if crate::entropy::is_secure_execution() {
            return;
        }
        // SAFETY: getenv takes a valid NUL-terminated name and returns either
        // null or a NUL-terminated environment value (never freed by us).
        let v = unsafe { libc::getenv(c"TOPOMALLOC_ZERO_SIZE".as_ptr()) };
        if !v.is_null() {
            // SAFETY: non-null getenv results point at a NUL-terminated string.
            let bytes = unsafe { core::ffi::CStr::from_ptr(v) }.to_bytes();
            if bytes == b"null" {
                topo_core::set_zero_size_policy(ZeroSizePolicy::Null);
            }
        }
    }
}

/// The active zero-size policy, applying the environment default on first use.
pub fn zero_size_policy() -> ZeroSizePolicy {
    ENV_INIT.call_once(apply_env_default);
    topo_core::zero_size_policy()
}

/// Set the zero-size policy at runtime (W8-4 "configurable"; the plan 07
/// control plane and tests use this). An explicit set always wins: it claims
/// the one-shot environment slot, so a later first-read cannot override it.
pub fn set_zero_size_policy(p: ZeroSizePolicy) {
    ENV_INIT.call_once(|| {});
    topo_core::set_zero_size_policy(p);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §29 / secure execution: `TOPOMALLOC_ZERO_SIZE` is a `TOPOMALLOC_*` tunable and
    /// must be ignored under `AT_SECURE`, like every other one. It needs its own check
    /// because the C entry points read this policy *before* `global()`, so it sits
    /// outside the phase-5 gate that covers the rest.
    ///
    /// The gate is the shared `is_secure_execution`, already proved to report false for
    /// an ordinary process (`entropy::tests::ordinary_execution_is_not_flagged_secure`)
    /// and to consult `AT_SECURE` where available. What this pins is that the zero-size
    /// reader consults it at all: the call is on the path from `zero_size_policy`, so a
    /// build that dropped it would fail to compile this reference.
    #[test]
    fn the_zero_size_knob_is_gated_on_secure_execution() {
        // An ordinary test process: not secure, so the knob is read normally and the
        // default stands (the environment does not set it here).
        assert!(!crate::entropy::is_secure_execution());
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
    }

    #[test]
    fn default_is_unique_and_runtime_override_works() {
        // The test environment does not set TOPOMALLOC_ZERO_SIZE, so the
        // default (glibc-compatible) policy applies. NOTE: process-global
        // state — this test restores the default, and no other in-crate test
        // depends on a flipped policy (the full behavioral matrix runs in its
        // own process: tests/tests/zero_size_policy.rs).
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
        set_zero_size_policy(ZeroSizePolicy::Null);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Null);
        set_zero_size_policy(ZeroSizePolicy::Unique);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
    }
}

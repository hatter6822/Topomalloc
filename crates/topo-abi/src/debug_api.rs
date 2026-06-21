// SPDX-License-Identifier: MIT
//! C control + introspection for the **Appendix-B invariant checks** (§30.2, plan
//! 08 W19-1, DD-2; §10.5 `topo.debug.*`). These run the runtime invariant
//! checkers — the Appendix-B checklist as first-class code — over the live engine
//! on demand, so a debugging session or a test can assert the whole allocator is
//! well-formed at a chosen point (the operator-triggered home for the O(state)
//! sweeps DD-2 keeps off the per-transition hot path; the cheap per-span B.3 check
//! already runs as a `debug_assert!` at every central transition).
//!
//! All functions are prefixed `topomalloc_debug_*` and are safe from any thread.

use core::ffi::c_int;

use crate::global;

/// `int topomalloc_debug_check_now(void)` (§30.2, §10.5 `topo.debug.check_now`):
/// run the **full Appendix-B invariant check** over the live allocator right now —
/// the shared + hooked back-ends (B.1/B.4), the central free-list layer and its
/// spans (B.1/B.3), and the arena registry (B.5). Returns `1` if every invariant
/// holds (and `1` vacuously when no allocator has been created yet), `0` if a
/// violation is detected. O(state) and lock-acquiring — for diagnostics and tests,
/// not a hot path. The per-CPU/transfer/thread cache layer (B.2) is not owned by
/// the engine at M1, so it is checked by its own `check_invariants` methods rather
/// than here.
#[no_mangle]
pub extern "C" fn topomalloc_debug_check_now() -> c_int {
    global().map_or(1, |a| a.check_invariants() as c_int)
}

/// `int topomalloc_debug_checks_enabled(void)` (§10.5 `topo.debug.checks_enabled`):
/// `1` if the Appendix-B checks are wired into runtime assertions in this build
/// (the `debug-checks` feature, implied by `debug`/`hardened`), else `0`. Lets an
/// operator confirm whether [`topomalloc_debug_check_now`] runs the integrity-tag
/// validations and whether the per-transition `debug_assert!`s are live. Reflects
/// the build of `topo-core` (the engine), not of `topo-abi`.
#[no_mangle]
pub extern "C" fn topomalloc_debug_checks_enabled() -> c_int {
    topo_core::debug_checks_enabled() as c_int
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_now_reports_a_well_formed_live_allocator() {
        // The freshly-constructed (or in-use) global allocator is well-formed, so a
        // check returns OK. This also exercises the full B.1/B.3/B.4/B.5 walk over
        // the live engine through the C boundary.
        assert_eq!(
            topomalloc_debug_check_now(),
            1,
            "the live allocator must be well-formed"
        );
        // Do some allocation traffic, then re-check: still well-formed.
        let p = crate::topomalloc_malloc(64);
        if !p.is_null() {
            // SAFETY: `p` came from `topomalloc_malloc(64)`; freeing it is valid.
            unsafe { crate::topomalloc_free(p) };
        }
        assert_eq!(
            topomalloc_debug_check_now(),
            1,
            "still well-formed after a malloc/free"
        );
    }

    #[test]
    fn checks_enabled_matches_the_engine_build() {
        assert_eq!(
            topomalloc_debug_checks_enabled() == 1,
            topo_core::debug_checks_enabled()
        );
    }
}

// SPDX-License-Identifier: MIT
//! C control + introspection for **deterministic test mode** (§30.4, plan 08
//! W19-3; §10.5 `topo.deterministic.*`). Deterministic mode makes a run
//! reproducible — every randomized decision derives from a single seed, slow
//! paths/purging can be forced, and the §33.7 trace ids are a monotonic counter —
//! so a captured trace replays identically through the Lean executable model (the
//! W21-2 differential runner's prerequisite).
//!
//! It is **off by default** (auto-on only under the `deterministic-test` profile)
//! and the flags are read on cold/slow paths, so the `performance` build pays
//! nothing. All functions are prefixed `topomalloc_deterministic_*` and are safe
//! from any thread. Seed/enable changes reseed the live randomized samplers (the
//! W18-4 guard sampler, the W18-3 quarantine evictor, the W17-3 heap sampler) so
//! the new seed takes effect immediately.

use core::ffi::c_int;

use topo_core::deterministic;

use crate::global;

/// Apply the current seed to every live randomized stream: the global allocator's
/// guard/quarantine samplers and the heap sampler's per-thread seed base. Called
/// whenever the seed or the enabled flag changes, so randomization becomes
/// seed-derived from that point on.
fn reseed_live() {
    reseed_live_with(global());
}

/// [`reseed_live`] against an explicitly supplied engine — used by the startup path,
/// which runs *inside* `GLOBAL.get_or_init` where calling [`global`] would re-enter the
/// still-running `OnceLock` and park the thread on its own initialization.
fn reseed_live_with(engine: Option<&crate::AnyAllocator>) {
    if deterministic::is_deterministic() {
        if let Some(a) = engine {
            a.apply_deterministic_seed();
        }
        // The heap sampler (topo-abi side): rebase its per-thread seed source so
        // sampling is reproducible in a deterministic run.
        crate::sampling::set_base_seed(deterministic::domain_seed(deterministic::salt::SAMPLER));
    } else {
        // Deterministic mode is **off** — including the moment it is switched off, which
        // is the case that used to fall through here doing nothing. `apply_deterministic_seed`
        // is itself a no-op while the mode is off, so a process that enabled deterministic
        // mode and then disabled it kept running the guard-page coin and quarantine evictor
        // on their seed-derived, publicly reproducible streams for the rest of its life,
        // while the API truthfully reported deterministic mode disabled. Re-seeding from
        // process entropy restores the §29.4/§29.5 unpredictability the disable implies.
        if let Some(a) = engine {
            a.seed_security_samplers();
        }
        crate::sampling::set_base_seed(topo_core::harden::process_entropy());
    }
}

/// `void topomalloc_deterministic_set_enabled(int on)` (§10.5
/// `topo.deterministic.enabled`): turn deterministic mode on/off. Enabling reseeds
/// the live randomized samplers from the current seed.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_set_enabled(on: c_int) {
    deterministic::set_deterministic(on != 0);
    reseed_live();
}

/// `int topomalloc_deterministic_enabled(void)` — `1` if deterministic mode is
/// active (the `deterministic-test` profile, or a runtime enable), else `0`.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_enabled() -> c_int {
    deterministic::is_deterministic() as c_int
}

/// `void topomalloc_deterministic_set_seed(uint64_t seed)` (§10.5
/// `topo.deterministic.seed`, §30.4 "disable randomization unless seeded"): set the
/// global seed every randomized decision derives from, and reseed the live samplers
/// so it takes effect at once.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_set_seed(seed: u64) {
    deterministic::set_seed(seed);
    reseed_live();
}

/// `uint64_t topomalloc_deterministic_seed(void)` — the current global seed.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_seed() -> u64 {
    deterministic::seed()
}

/// `void topomalloc_deterministic_set_force_slow_path(int on)` (§10.5
/// `topo.deterministic.force_slow_path`, §30.4): force the front-end/central layer
/// to bypass its fast-path cache reuse, exercising the slow path every time.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_set_force_slow_path(on: c_int) {
    deterministic::set_force_slow_path(on != 0);
}

/// `void topomalloc_deterministic_set_force_purge(int on)` (§10.5
/// `topo.deterministic.force_purge`, §30.4): force the extent backend to release
/// freed backing to the OS eagerly instead of retaining it.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_set_force_purge(on: c_int) {
    deterministic::set_force_purge(on != 0);
}

/// `uint64_t topomalloc_deterministic_next_trace_id(void)` — the next §33.7 trace/
/// request id (a process-global monotonic counter). The canonical id source for
/// trace emission; reproducible in a single-threaded deterministic replay.
#[no_mangle]
pub extern "C" fn topomalloc_deterministic_next_trace_id() -> u64 {
    deterministic::next_trace_id()
}

/// Honour `$TOPOMALLOC_DETERMINISTIC_SEED` at startup (§32.1): a parseable integer
/// enables deterministic mode and sets the seed; an absent/empty value leaves the
/// build default (deterministic only under the `deterministic-test` profile).
/// Called under the bootstrap guard so any one-time setup is served by the system
/// allocator.
///
/// Takes the engine by reference: it runs inside `GLOBAL.get_or_init`, so calling
/// [`global`] here would re-enter the still-running `OnceLock` and hang the process on
/// its first allocation.
pub(crate) fn init_from_env(a: &crate::AnyAllocator) {
    if let Ok(raw) = std::env::var("TOPOMALLOC_DETERMINISTIC_SEED") {
        if let Ok(seed) = raw.trim().parse::<u64>() {
            deterministic::set_deterministic(true);
            deterministic::set_seed(seed);
            reseed_live_with(Some(a));
        }
    } else if deterministic::is_deterministic() {
        // Built under the `deterministic-test` profile with no explicit seed: still
        // apply the default seed to the live samplers so the run is reproducible.
        reseed_live_with(Some(a));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// These tests mutate **process-global** deterministic state (seed, enabled flag,
    /// sampler base), and the harness runs them as parallel threads of one process, so
    /// they must not overlap. Same convention as `tests/tests/deterministic.rs` and
    /// `topo-abi/tests/front_end.rs`.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// Take [`SERIAL`], tolerating a previous test's panic.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn seed_round_trips_through_the_c_surface() {
        let _g = serial();
        let original = topomalloc_deterministic_seed();
        topomalloc_deterministic_set_seed(0x0102_0304_0506_0708);
        assert_eq!(topomalloc_deterministic_seed(), 0x0102_0304_0506_0708);
        // Restore so the process-global seed does not leak to other tests.
        topomalloc_deterministic_set_seed(original);
    }

    #[test]
    fn trace_ids_advance_through_the_c_surface() {
        let a = topomalloc_deterministic_next_trace_id();
        let b = topomalloc_deterministic_next_trace_id();
        assert!(b > a, "trace ids are monotonic");
    }

    /// Disabling deterministic mode must restore *unpredictable* streams.
    ///
    /// `apply_deterministic_seed` is itself a no-op while the mode is off, so the disable
    /// transition used to run `reseed_live` and change nothing: the guard-page coin and
    /// quarantine evictor kept running on their seed-derived, publicly reproducible
    /// streams for the rest of the process, while the API reported deterministic mode
    /// disabled. The sampler base seed is the observable half of that — under
    /// deterministic mode it is the SAMPLER domain seed, and after a disable it must not
    /// still be.
    #[test]
    fn disabling_deterministic_mode_restores_unpredictable_streams() {
        let _g = serial();
        let was_on = deterministic::is_deterministic();
        let original_seed = topomalloc_deterministic_seed();

        topomalloc_deterministic_set_seed(0x5EED_0000_0000_0001);
        topomalloc_deterministic_set_enabled(1);
        let det_base = crate::sampling::base_seed();
        assert_eq!(
            det_base,
            deterministic::domain_seed(deterministic::salt::SAMPLER),
            "while deterministic, the sampler base is the seed-derived one"
        );

        topomalloc_deterministic_set_enabled(0);
        assert_ne!(
            crate::sampling::base_seed(),
            det_base,
            "disabling must move the sampler off the reproducible stream"
        );

        // Restore process-global state for the rest of the suite.
        topomalloc_deterministic_set_enabled(was_on as c_int);
        topomalloc_deterministic_set_seed(original_seed);
    }

    #[test]
    fn enabled_query_is_safe_from_c() {
        let _g = serial();
        // A real read that never panics; matches the module's view.
        let on = topomalloc_deterministic_enabled() == 1;
        assert_eq!(on, deterministic::is_deterministic());
    }
}

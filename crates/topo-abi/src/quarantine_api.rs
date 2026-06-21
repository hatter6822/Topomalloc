// SPDX-License-Identifier: MIT
//! C control + introspection for the W18-3 security **quarantine** (§29.4, §10.5
//! `topo.quarantine.*`). The quarantine holds freed objects out of circulation so
//! a use-after-free is more likely to hit still-quarantined (not yet reused) memory.
//!
//! It is **opt-in**: the machinery is compiled in only with the `quarantine`
//! feature (the `hardened` profile), and even then it is **off until enabled** here
//! or via `$TOPOMALLOC_QUARANTINE` — so the `performance` build pays nothing and a
//! plain `hardened` build does not impose the RSS/latency cost unasked. Without the
//! feature every function below is a safe no-op (`bytes`/`objects` read `0`).
//!
//! All functions are prefixed `topomalloc_quarantine_*` and are safe from any thread.

use core::ffi::c_int;

use crate::global;

/// `void topomalloc_quarantine_set_enabled(int on)` (§10.5 `topo.quarantine.enabled`):
/// turn the quarantine on/off at runtime. Turning it **off drains** the held objects
/// (really frees them). A no-op without the `quarantine` feature.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_set_enabled(on: c_int) {
    if let Some(a) = global() {
        a.set_quarantine_enabled(on != 0);
    }
}

/// `int topomalloc_quarantine_enabled(void)` — `1` if the quarantine is active
/// (feature compiled **and** runtime switch on), else `0`.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_enabled() -> c_int {
    global().is_some_and(|a| a.quarantine_active()) as c_int
}

/// `uint64_t topomalloc_quarantine_bytes(void)` — bytes currently held in the
/// quarantine (§29.4; the `quarantine.bytes` stats class). `0` when off.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_bytes() -> u64 {
    global().map_or(0, |a| a.quarantine_bytes())
}

/// `uint64_t topomalloc_quarantine_objects(void)` — objects currently held. `0`
/// when off.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_objects() -> u64 {
    global().map_or(0, |a| u64::from(a.quarantine_objects()))
}

/// `void topomalloc_quarantine_set_limits(uint64_t max_bytes, uint64_t max_objects)`
/// (§10.5 `topo.quarantine.max_bytes` / `max_objects`): set the eviction budgets.
/// A no-op without the `quarantine` feature (the budgets have nothing to govern).
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_set_limits(max_bytes: u64, max_objects: u64) {
    #[cfg(feature = "quarantine")]
    if let Some(a) = global() {
        let mut p = a.quarantine_policy();
        p.max_bytes = max_bytes;
        p.max_objects = max_objects.min(u64::from(u32::MAX)) as u32;
        a.set_quarantine_policy(p);
    }
    #[cfg(not(feature = "quarantine"))]
    {
        let _ = (max_bytes, max_objects);
    }
}

/// `uint64_t topomalloc_quarantine_max_bytes(void)` — the current byte budget
/// (§10.5 `topo.quarantine.max_bytes`). `0` without the feature.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_max_bytes() -> u64 {
    #[cfg(feature = "quarantine")]
    {
        global().map_or(0, |a| a.quarantine_policy().max_bytes)
    }
    #[cfg(not(feature = "quarantine"))]
    {
        0
    }
}

/// `uint64_t topomalloc_quarantine_max_objects(void)` — the current object budget
/// (§10.5 `topo.quarantine.max_objects`). `0` without the feature.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_max_objects() -> u64 {
    #[cfg(feature = "quarantine")]
    {
        global().map_or(0, |a| u64::from(a.quarantine_policy().max_objects))
    }
    #[cfg(not(feature = "quarantine"))]
    {
        0
    }
}

/// `void topomalloc_quarantine_converge(void)` (§10.5 W18-3 background convergence):
/// really free any held objects beyond the current budget — used after lowering the
/// budget on a quiescent heap (no allocation traffic to converge it incrementally).
/// Idempotent; a no-op when already within budget or without the `quarantine` feature.
#[no_mangle]
pub extern "C" fn topomalloc_quarantine_converge() {
    if let Some(a) = global() {
        a.converge_quarantine();
    }
}

/// `void topomalloc_guard_set_sample_rate(uint64_t rate)` (§10.5 W18-4, §29.5):
/// guard ~1 in `rate` ordinary allocations with inaccessible pages (`0` = explicit
/// `TOPO_GUARDED` only). A no-op without the `guard-pages` feature.
#[no_mangle]
pub extern "C" fn topomalloc_guard_set_sample_rate(rate: u64) {
    if let Some(a) = global() {
        a.set_guard_sample_rate(rate);
    }
}

/// `uint64_t topomalloc_guard_sample_rate(void)` — the current rate (`0` = off).
#[no_mangle]
pub extern "C" fn topomalloc_guard_sample_rate() -> u64 {
    global().map_or(0, |a| a.guard_sample_rate())
}

/// Honour `$TOPOMALLOC_GUARD_SAMPLE_RATE` at startup (§32.1): a positive integer
/// sets the guarded-allocation sampling rate. A no-op without the `guard-pages`
/// feature.
pub(crate) fn guard_init_from_env() {
    if let Ok(raw) = std::env::var("TOPOMALLOC_GUARD_SAMPLE_RATE") {
        if let Ok(rate) = raw.trim().parse::<u64>() {
            if let Some(a) = global() {
                a.set_guard_sample_rate(rate);
            }
        }
    }
}

/// Honour `$TOPOMALLOC_QUARANTINE` at startup (§32.1): a positive integer enables
/// the quarantine and sets its `max_bytes` budget; `1`/`on`/`true` enables it with
/// the default budget; `0`/unset leaves it off. Called under the bootstrap guard so
/// any one-time setup is served by the system allocator. Without the `quarantine`
/// feature, enabling is a no-op (nothing is compiled to hold objects).
pub(crate) fn init_from_env() {
    let Ok(raw) = std::env::var("TOPOMALLOC_QUARANTINE") else {
        return;
    };
    let v = raw.trim();
    let as_num = v.parse::<u64>().ok();
    let enable = matches!(v, "1" | "on" | "true") || as_num.is_some_and(|n| n > 0);
    if !enable {
        return;
    }
    let Some(a) = global() else { return };
    #[cfg(feature = "quarantine")]
    if let Some(max_bytes) = as_num {
        if max_bytes > 1 {
            // A bare `1` is the "just enable" form, not a 1-byte budget.
            let mut p = a.quarantine_policy();
            p.max_bytes = max_bytes;
            a.set_quarantine_policy(p);
        }
    }
    a.set_quarantine_enabled(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_round_trips() {
        // Without the feature, enabling is a no-op (stays inactive); with it, the
        // runtime switch flips. Either way the calls are safe and `bytes` reads back.
        topomalloc_quarantine_set_enabled(1);
        let active = topomalloc_quarantine_enabled() == 1;
        assert_eq!(active, cfg!(feature = "quarantine"));
        assert!(topomalloc_quarantine_bytes() < u64::MAX); // a real read, never panics
        topomalloc_quarantine_set_enabled(0);
        assert_eq!(topomalloc_quarantine_enabled(), 0);
    }

    #[test]
    fn limits_and_converge_are_safe_from_c() {
        // The limit readers and the convergence trigger are safe no-ops without the
        // feature, and consistent with `set_limits` when compiled in. `converge`
        // never panics and leaves held bytes within the budget.
        topomalloc_quarantine_set_limits(4096, 16);
        if cfg!(feature = "quarantine") {
            assert_eq!(topomalloc_quarantine_max_bytes(), 4096);
            assert_eq!(topomalloc_quarantine_max_objects(), 16);
        }
        topomalloc_quarantine_converge(); // never panics; idempotent
        assert!(topomalloc_quarantine_bytes() <= topomalloc_quarantine_max_bytes().max(1));
    }
}

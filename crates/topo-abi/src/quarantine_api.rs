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
}

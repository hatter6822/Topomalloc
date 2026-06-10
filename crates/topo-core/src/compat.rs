// SPDX-License-Identifier: MIT
//! Cross-cutting compatibility knobs (plan 06 W8-4).
//!
//! The zero-size allocation policy (§9.6 / F-004) lives in the core — not the
//! ABI shell — so the control plane (plan 07, Appendix E `topo.compat.*`) and
//! future engine-internal consumers read the same state the public API acts
//! on. The core holds only the **state** (a `no_std` atomic, defaulting to
//! the dominant-platform `Unique` behavior); *wiring* is layered above it:
//! `topo-abi` applies the `TOPOMALLOC_ZERO_SIZE` environment default on first
//! use and enforces the policy at the C entry points, and `topo-control`
//! exposes the active value as `topo.compat.zero_size`.

use core::sync::atomic::{AtomicU8, Ordering};

/// What a zero-size allocation request returns (§9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroSizePolicy {
    /// A minimum-size unique freeable pointer (`compat.zero_unique`,
    /// **default** — the dominant platform allocator expectation, §9.6).
    Unique,
    /// `NULL`, with `errno` untouched (`compat.zero_null`).
    Null,
}

impl ZeroSizePolicy {
    /// The stable string used by the control namespace (`topo.compat.zero_size`)
    /// and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            ZeroSizePolicy::Unique => "unique",
            ZeroSizePolicy::Null => "null",
        }
    }
}

const UNIQUE: u8 = 0;
const NULL: u8 = 1;

/// The process-wide policy. Relaxed ordering suffices: the value is a
/// self-contained byte and every reader tolerates either value at a race
/// boundary (a configuration change mid-allocation simply applies to the next
/// request).
static ZERO_SIZE: AtomicU8 = AtomicU8::new(UNIQUE);

/// The active zero-size policy.
pub fn zero_size_policy() -> ZeroSizePolicy {
    match ZERO_SIZE.load(Ordering::Relaxed) {
        NULL => ZeroSizePolicy::Null,
        _ => ZeroSizePolicy::Unique,
    }
}

/// Set the zero-size policy (W8-4 "configurable"): called by the ABI's
/// environment wiring, the plan 07 control plane, and tests.
pub fn set_zero_size_policy(p: ZeroSizePolicy) {
    let v = match p {
        ZeroSizePolicy::Unique => UNIQUE,
        ZeroSizePolicy::Null => NULL,
    };
    ZERO_SIZE.store(v, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_unique_and_round_trips() {
        // NOTE: process-global state; this test restores the default and is
        // the only in-crate mutator.
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
        assert_eq!(zero_size_policy().as_str(), "unique");
        set_zero_size_policy(ZeroSizePolicy::Null);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Null);
        assert_eq!(zero_size_policy().as_str(), "null");
        set_zero_size_policy(ZeroSizePolicy::Unique);
        assert_eq!(zero_size_policy(), ZeroSizePolicy::Unique);
    }
}

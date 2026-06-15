// SPDX-License-Identifier: MIT
//! C control + introspection surface for heap/lifetime **sampling** (§31.3/§31.4, §10.5
//! `profile.*`, plan 07 W17-3 / W14). These drive and read the live placement profiler
//! ([`crate::sampling`]); the actual sampling is wired into the allocation path, so simply
//! setting a non-zero rate starts attributing the live heap by call site.
//!
//! All functions are prefixed `topomalloc_profile_*` (never exported as bare `malloc`-style
//! symbols, §35.1) and are safe to call from any thread at any time.

use core::ffi::{c_char, c_int};
use core::ptr;

use crate::sampling;

/// `void topomalloc_profile_set_rate(uint64_t mean_bytes)` — start/stop sampled profiling
/// (§10.5 `profile.heap.start` / `stop`). `mean_bytes` is the mean number of allocated
/// bytes between samples (a Poisson rate); `0` disables. Enabling performs the one-time
/// unwinder warm-up off the hot path, so the first sample is allocation-free.
#[no_mangle]
pub extern "C" fn topomalloc_profile_set_rate(mean_bytes: u64) {
    sampling::set_rate(mean_bytes);
}

/// `int topomalloc_profile_enabled(void)` — `1` if sampled profiling is on, else `0`.
#[no_mangle]
pub extern "C" fn topomalloc_profile_enabled() -> c_int {
    sampling::enabled() as c_int
}

/// `uint64_t topomalloc_profile_rate(void)` — the current mean sample interval in bytes
/// (`0` when disabled).
#[no_mangle]
pub extern "C" fn topomalloc_profile_rate() -> u64 {
    sampling::rate_bytes()
}

/// `uint64_t topomalloc_profile_sites(void)` — distinct allocation sites currently
/// profiled.
#[no_mangle]
pub extern "C" fn topomalloc_profile_sites() -> u64 {
    sampling::placement_stats().sites_tracked as u64
}

/// `uint64_t topomalloc_profile_confident_sites(void)` — sites confident enough to steer
/// placement (§24.6).
#[no_mangle]
pub extern "C" fn topomalloc_profile_confident_sites() -> u64 {
    sampling::placement_stats().confident_sites as u64
}

/// `size_t topomalloc_profile_dump_json(char* buf, size_t cap)` (§31.3 `profile.heap.dump`):
/// render the live profile as JSON into `buf` (NUL-terminated, truncated to `cap`), and
/// return the **full** length in bytes (excluding the NUL) so a caller can size a buffer
/// and re-call. Pass `buf == NULL` / `cap == 0` to query the length only.
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_profile_dump_json(buf: *mut c_char, cap: usize) -> usize {
    let json = sampling::dump_json();
    let bytes = json.as_bytes();
    let full = bytes.len();
    if !buf.is_null() && cap > 0 {
        // Leave room for the terminating NUL.
        let n = full.min(cap - 1);
        // SAFETY: `bytes` is a live slice of `full >= n` bytes; the caller guarantees `buf`
        // has `cap > n` writable bytes (disjoint from `json`'s owned heap buffer).
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
            *buf.add(n) = 0;
        }
    }
    full
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_rate_round_trips_enable_state() {
        // Save/restore the process-global rate so this test does not perturb others.
        let prior = topomalloc_profile_rate();
        topomalloc_profile_set_rate(0);
        assert_eq!(topomalloc_profile_enabled(), 0);
        assert_eq!(topomalloc_profile_rate(), 0);
        topomalloc_profile_set_rate(1 << 20);
        assert_eq!(topomalloc_profile_enabled(), 1);
        assert_eq!(topomalloc_profile_rate(), 1 << 20);
        topomalloc_profile_set_rate(prior);
    }

    #[test]
    fn dump_json_length_query_and_truncation() {
        // SAFETY: the length-query form — a null buffer with cap 0 writes nothing.
        let full = unsafe { topomalloc_profile_dump_json(ptr::null_mut(), 0) };
        assert!(full > 0, "even an empty profiler emits a JSON object");
        // A small buffer truncates and NUL-terminates without overrunning.
        let mut small = [0u8; 8];
        // SAFETY: `small` is 8 writable bytes and `cap == 8` matches it.
        let n = unsafe { topomalloc_profile_dump_json(small.as_mut_ptr().cast::<c_char>(), 8) };
        assert_eq!(n, full, "returns the full length regardless of cap");
        assert_eq!(small[7], 0, "NUL terminated within cap");
    }
}

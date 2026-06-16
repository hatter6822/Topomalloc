// SPDX-License-Identifier: MIT
//! Re-entrancy / no-allocation proof for the heap sampler (§31.4, Appendix F, plan 07
//! W17-3) — the SPEC's prescribed "count allocator depth and assert the sampler never
//! increments it" verification.
//!
//! The trap §31.4 / Appendix F call out is a sampled allocation whose stack capture (or
//! bookkeeping) allocates, recursing into the allocator *from inside an allocation*. We pin
//! that the sampled slow path is **allocation-free** by installing a process
//! `#[global_allocator]` that counts every Rust-heap allocation, then driving heavy sampled
//! `topomalloc_malloc`/`free` traffic and asserting the counter does **not** move across the
//! sampled loop: the C engine serves these allocations from its own mmap'd regions (not the
//! Rust heap), and the sampler's fired-sample path touches only fixed buffers, a lock, the
//! monotonic clock, and `libc::backtrace` — so any allocation during the loop would be the
//! sampler re-entering, and the count would rise. It does not.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// A pass-through global allocator that tallies allocations, used to detect any heap
/// allocation made on the sampled path. Counting is armed only around the measured loop so
/// the test harness's own allocations are ignored.
struct CountingAlloc;

static COUNTING_ARMED: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);

// SAFETY: every method forwards verbatim to the system allocator (which upholds the
// `GlobalAlloc` contract); the only added behaviour is a relaxed counter increment when
// armed, which does not affect the returned pointers or their validity.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING_ARMED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `(ptr, layout)` came from this allocator's `alloc`, i.e. from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING_ARMED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING_ARMED.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: `(ptr, layout)` came from `System`; `new_size` forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

use topo_abi::{topomalloc_free, topomalloc_malloc, topomalloc_profile_set_rate};

#[test]
fn sampled_path_makes_no_heap_allocations() {
    // Prime the C engine (its first call lazily reserves regions / leaks metadata — through
    // the Rust heap, which we must not count) and the sampler (its enable allocates the
    // sampled state + warms the unwinder, also off the measured path).
    let warm = topomalloc_malloc(64);
    assert!(!warm.is_null());
    // SAFETY: `warm` is the live allocation just returned by the C engine.
    unsafe { topomalloc_free(warm) };
    topomalloc_profile_set_rate(2048); // small interval ⇒ frequent samples
                                       // Fire a few samples so any one-time lazy initialization (state Box, unwinder dlopen)
                                       // happens *before* we arm the counter.
    for _ in 0..2000 {
        let p = topomalloc_malloc(512);
        assert!(!p.is_null());
        // SAFETY: `p` is the live allocation just returned.
        unsafe { topomalloc_free(p) };
    }

    // Arm: from here, any Rust-heap allocation is either the test harness (there is none in
    // the loop body) or the sampler re-entering — which must not happen.
    COUNTING_ARMED.store(true, Ordering::Relaxed);
    let before = ALLOC_COUNT.load(Ordering::Relaxed);
    let mut fired_guard_ok = true;
    for _ in 0..50_000 {
        let p = topomalloc_malloc(512);
        if p.is_null() {
            fired_guard_ok = false;
            break;
        }
        // The sampler must never leave this thread parked "inside" itself.
        fired_guard_ok &= !topo_abi::sampling::in_sampler();
        // SAFETY: `p` is the live allocation just returned.
        unsafe { topomalloc_free(p) };
    }
    let after = ALLOC_COUNT.load(Ordering::Relaxed);
    COUNTING_ARMED.store(false, Ordering::Relaxed);

    assert!(
        fired_guard_ok,
        "allocation failed or the sampler re-entered"
    );
    assert_eq!(
        after,
        before,
        "the sampled allocation path made {} heap allocation(s) — it must be allocation-free \
         (§31.4 / Appendix F)",
        after - before
    );
    // Confirm sampling actually fired during the measured loop (else the test is vacuous).
    assert!(
        topo_abi::sampling::placement_stats().alloc_samples > 0,
        "sampling did not fire — the no-alloc assertion would be vacuous"
    );

    topomalloc_profile_set_rate(0);
}

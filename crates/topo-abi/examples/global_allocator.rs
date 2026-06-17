// SPDX-License-Identifier: MIT
//! Smoke test: `TopoMallocGlobal` works as the process `#[global_allocator]`.
//!
//! Registering the adapter routes *every* heap allocation in this binary through
//! TopoMalloc. The first one lazily initializes the skeleton heap, and that
//! initializer itself allocates (reading the backend env var, reserving the
//! backing heap, the provider's bookkeeping). Without the per-thread bootstrap
//! guard those init-path allocations would re-enter `global()` and deadlock the
//! `OnceLock` before `main` could run, so reaching the final print is the proof
//! that the guard holds.
//!
//! **W16-2 (TLS, §27.6/DD-4) — proven by re-entrancy depth, not just liveness.**
//! The allocator is wrapped in a [`DepthCounting`] global allocator that tracks
//! per-thread allocation nesting depth and the process-wide maximum. After the
//! one-time init window (whose allocations *do* nest — they bootstrap through the
//! system allocator), the steady-state path is measured: every allocation,
//! **including a fresh thread's very first heap touch** (where that thread's TLS —
//! the lock-order checker, the re-entrancy guards — is established), must run at
//! depth exactly 1, i.e. it makes **no nested allocation**. That is the precise
//! statement of "the first TLS access never re-enters the allocator": the
//! allocator's own thread-local state is `const`-initialised (Local-Exec TLS),
//! allocation-free, so establishing it triggers no recursive `malloc`. A
//! regression would push the observed depth past 1 and fail the assertion (and a
//! true recursion/deadlock would be caught by the `xtask` gate's timeout).
//!
//! The complementary `dlopen` case (general-dynamic TLS) is covered by
//! `tests/tls_dlopen.rs`.
//!
//! Run via `cargo run -p topo-abi --example global_allocator`; `cargo xtask test`
//! and `cargo xtask ci` run it as a gate.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};

use topo_abi::TopoMallocGlobal;

thread_local! {
    /// This thread's current allocation nesting depth. `const`-initialised so it
    /// is Local-Exec TLS (allocation-free on first access) — the very property
    /// W16-2 relies on; if *this* slot allocated on first touch we could not
    /// measure honestly.
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// The maximum allocation nesting depth observed across all threads, *after*
/// [`measuring`](MEASURING) is turned on (i.e. excluding the init window).
static MAX_DEPTH: AtomicU32 = AtomicU32::new(0);
/// Whether to record depths (set once the one-time init has completed).
static MEASURING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A `#[global_allocator]` that wraps [`TopoMallocGlobal`] and records the
/// per-thread allocation nesting depth, so a re-entrant allocation (depth > 1) is
/// observable. Forwards every operation unchanged.
struct DepthCounting;

impl DepthCounting {
    #[inline]
    fn enter() -> u32 {
        DEPTH.with(|c| {
            let d = c.get() + 1;
            c.set(d);
            if MEASURING.load(Ordering::Relaxed) {
                MAX_DEPTH.fetch_max(d, Ordering::Relaxed);
            }
            d
        })
    }
    #[inline]
    fn leave() {
        DEPTH.with(|c| c.set(c.get() - 1));
    }
}

// SAFETY: every method forwards to `TopoMallocGlobal` (a sound `GlobalAlloc`)
// unchanged; the depth bookkeeping only reads/writes a `Cell`/atomic and never
// allocates, so it cannot perturb the wrapped allocator's contract.
unsafe impl GlobalAlloc for DepthCounting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::enter();
        // SAFETY: `layout` forwarded unchanged.
        let p = unsafe { TopoMallocGlobal.alloc(layout) };
        Self::leave();
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::enter();
        // SAFETY: `layout` forwarded unchanged.
        let p = unsafe { TopoMallocGlobal.alloc_zeroed(layout) };
        Self::leave();
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        Self::enter();
        // SAFETY: `(ptr, layout)` forwarded unchanged.
        unsafe { TopoMallocGlobal.dealloc(ptr, layout) };
        Self::leave();
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::enter();
        // SAFETY: forwarded unchanged.
        let p = unsafe { TopoMallocGlobal.realloc(ptr, layout, new_size) };
        Self::leave();
        p
    }
}

#[global_allocator]
static GLOBAL: DepthCounting = DepthCounting;

fn main() {
    // Exercise several allocating paths through the global allocator: a growable
    // Vec, a boxed array, and a formatted String. The first of these triggers the
    // one-time lazy init (whose allocations legitimately nest through the system
    // allocator), so this runs with measurement OFF.
    let v: Vec<u64> = (0..256).collect();
    let boxed = Box::new([7u8; 128]);
    let sum: u64 = v.iter().sum();
    let s = format!("sum={sum}, boxed[0]={}", boxed[0]);
    assert_eq!(v.len(), 256);
    assert_eq!(sum, 256 * 255 / 2);
    assert_eq!(boxed[0], 7);
    assert!(s.contains("sum="));

    // Init is done; from here every allocation is steady-state and MUST run at
    // depth 1 (no nested allocation). Turn measurement on.
    MEASURING.store(true, Ordering::Relaxed);

    // W16-2: the first allocation on each fresh thread establishes that thread's
    // TLS (the lock checker, the re-entrancy guards). With Local-Exec, allocation-
    // free TLS, that first access makes no nested allocation — so the observed
    // depth across all of them stays exactly 1.
    let handles: Vec<_> = (0..16)
        .map(|t| {
            std::thread::spawn(move || {
                // This `Vec::with_capacity` is this thread's very first heap touch.
                let mut buf: Vec<u8> = Vec::with_capacity(64 + t * 8);
                buf.extend((0..buf.capacity()).map(|i| i as u8));
                let local_sum: u64 = buf.iter().map(|&b| u64::from(b)).sum();
                // More steady-state allocations after the TLS exists.
                let big = vec![t as u8; 4096];
                local_sum + big.iter().map(|&b| u64::from(b)).sum::<u64>()
            })
        })
        .collect();
    let mut total = 0u64;
    for h in handles {
        total += h.join().expect("a fresh thread panicked (TLS re-entry?)");
    }
    assert!(total > 0);

    // The precise W16-2 assertion: the steady-state allocate path — including each
    // fresh thread's first TLS-establishing allocation — never re-entered the
    // allocator (depth never exceeded 1).
    let max = MAX_DEPTH.load(Ordering::Relaxed);
    assert!(
        max == 1,
        "W16-2 regression: an allocation re-entered the allocator (max nesting depth = {max}, \
         expected 1) — a thread's first TLS access (or some path) made a nested allocation"
    );

    println!("global-allocator bootstrap OK (16 threads, max alloc depth = {max}): {s}");
}

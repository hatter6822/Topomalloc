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
//! **W16-2 (TLS, §27.6/DD-4).** It additionally drives the *first* allocation on
//! many freshly-spawned threads: a thread's first heap touch is where the
//! allocator's own thread-local state (the lock-order checker, the sampler/
//! bootstrap re-entrancy guards) is established. Because that state is
//! `const`-initialised (Local-Exec TLS) and allocation-free, the first access
//! never re-enters the allocator — reaching the final print across all threads is
//! the proof. A re-entrancy regression would deadlock the spawning thread, which
//! the `xtask` gate runs under a timeout.
//!
//! Run via `cargo run -p topo-abi --example global_allocator`; `cargo xtask test`
//! and `cargo xtask ci` run it as a gate.

use topo_abi::TopoMallocGlobal;

#[global_allocator]
static GLOBAL: TopoMallocGlobal = TopoMallocGlobal;

fn main() {
    // Exercise several allocating paths through the global allocator: a growable
    // Vec, a boxed array, and a formatted String.
    let v: Vec<u64> = (0..256).collect();
    let boxed = Box::new([7u8; 128]);
    let sum: u64 = v.iter().sum();
    let s = format!("sum={sum}, boxed[0]={}", boxed[0]);

    assert_eq!(v.len(), 256);
    assert_eq!(sum, 256 * 255 / 2);
    assert_eq!(boxed[0], 7);
    assert!(s.contains("sum="));

    // W16-2: the first allocation on each fresh thread must not re-enter the
    // allocator while establishing that thread's TLS. Spawn a batch of threads
    // that each allocate *immediately* on entry, then join them all.
    let handles: Vec<_> = (0..16)
        .map(|t| {
            std::thread::spawn(move || {
                // This `Vec::with_capacity` is this thread's very first heap touch.
                let mut buf: Vec<u8> = Vec::with_capacity(64 + t * 8);
                buf.extend((0..buf.capacity()).map(|i| i as u8));
                let local_sum: u64 = buf.iter().map(|&b| u64::from(b)).sum();
                // A second, larger allocation after TLS exists (the steady state).
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

    println!("global-allocator bootstrap OK ({} threads): {s}", 16);
}

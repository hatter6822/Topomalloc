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

    println!("global-allocator bootstrap OK: {s}");
}

// SPDX-License-Identifier: MIT
//! Fuzz target for the W19-1b Appendix-B **cache** invariant checkers (plan 08,
//! §30.2, B.2). Under *any* fuzzer-derived sequence of per-CPU and transfer cache
//! operations, [`CpuCache::check_invariants`] and [`TransferCache::check_invariants`]
//! must hold — capacities are never exceeded and every resident object is distinct
//! and non-null. Each pushed object draws a **fresh** address from a monotonic
//! generator, so the distinctness clause is always satisfiable; a cache operation
//! that duplicated, lost, or mis-bounded an object would trip the checker and the
//! fuzzer would report the input. The central free list and the full engine
//! (B.1/B.3/B.4/B.5) are fuzzed by `malloc_api`; this target covers the standalone
//! no_std front-end cache layer. (The per-thread cache, a `std`-only structure, is
//! covered by its unit + negative tests.)
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::{size_class, ArenaId, CoreId, CpuCache, SizeClassId, TransferCache};

/// A tiny byte-stream cursor over the fuzzer input.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

const NUM_SC: usize = 8;
const NUM_CPU: u32 = 2;

fuzz_target!(|data: &[u8]| {
    // A fresh metadata arena per input (heap, freed when this input returns — so a
    // long campaign does not leak), ample for the lazily-allocated slot buffers.
    let mut meta_buf = vec![0u8; 2 << 20];
    // SAFETY: `meta_buf` outlives both caches below (declared first, dropped last),
    // is never moved or reallocated, and is not aliased elsewhere.
    let meta = unsafe { BumpArena::new(meta_buf.as_mut_ptr(), meta_buf.len()) };

    let cpu = CpuCache::new();
    cpu.set_active_cpus(NUM_CPU);
    let transfer = TransferCache::new();

    // Distinct addresses, so the distinctness clause is always satisfiable: this
    // fuzzes that the cache *operations* preserve B.2 and the checkers never
    // false-fire, not double-free detection (which the negative tests cover).
    let mut next_addr = 0x1_0000usize;
    let mut fresh = || {
        let a = next_addr;
        next_addr = next_addr.wrapping_add(64);
        a
    };

    let mut cur = Cursor::new(data);
    while !cur.done() {
        let op = cur.u8() % 5;
        let sc = SizeClassId::new((cur.u8() as usize) % NUM_SC);
        let core = CoreId(u32::from(cur.u8()) % NUM_CPU);
        match op {
            0 => {
                cpu.init_slot(core, sc, &meta, size_class::batch(sc) as u32);
            }
            1 => {
                cpu.fe_push(core, ArenaId::DEFAULT, sc, fresh(), &meta);
            }
            2 => {
                let _ = cpu.fe_pop(core, ArenaId::DEFAULT, sc, &meta);
            }
            3 => {
                transfer.try_push_batch(ArenaId::DEFAULT, sc, &[fresh()], &meta);
            }
            _ => {
                let mut out = [0usize; 4];
                transfer.try_pop_batch(ArenaId::DEFAULT, sc, &mut out, 4, &meta);
            }
        }
        // The B.2 cache invariants must hold after every operation.
        assert!(
            cpu.check_invariants(),
            "a per-CPU cache operation broke a B.2 invariant"
        );
        assert!(
            transfer.check_invariants(),
            "a transfer cache operation broke a B.2 invariant"
        );
    }
});

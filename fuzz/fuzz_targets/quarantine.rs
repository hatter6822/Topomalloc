// SPDX-License-Identifier: MIT
//! Fuzz target for the W18-3 delayed-reuse quarantine (plan 08, §29.4): under *any*
//! fuzzer-derived sequence of policy changes, offers, and drains, the
//! [`Quarantine`] stays well-formed ([`Quarantine::check_invariants`] — the
//! accounting atomics agree with the ring, the membership filter contains every held
//! entry), never exceeds its byte/object budgets, and **conserves bytes**: every
//! offered allocation is, exactly once, either held (and eventually drained) or
//! immediately freed by the caller (declined / evicted). A lost entry, a leaked or
//! double-counted byte, a budget overshoot, or a torn invariant trips an assert and
//! the fuzzer reports the offending input.
//!
//! The quarantine treats `user_ptr`/`span` as opaque keys it never dereferences, so
//! the fuzzer's synthetic (never-allocated) addresses are safe to drive it with.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::{ArenaId, EvictBatch, Offer, Quarantine, QuarantineEntry, QuarantinePolicy};

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

fuzz_target!(|data: &[u8]| {
    let q = Quarantine::new();
    let mut c = Cursor::new(data);

    // First two bytes seed the policy (kept small so eviction/decline paths fire).
    let max_objects = 1 + (c.u8() as u32 % 32); // 1..=32
    let max_bytes = 64 + (c.u8() as u64 % 32) * 64; // 64..=2048, a multiple of 64
    let random_evict = c.u8() & 1 == 1;
    q.set_policy(QuarantinePolicy {
        max_bytes,
        max_objects,
        per_arena_bytes: 0, // global budget only (per-arena exercised by unit tests)
        random_evict,
        sample_shift: 0,
        ..QuarantinePolicy::DEFAULT
    });

    // Conservation tallies: every offered byte must be accounted for exactly once.
    let mut offered: u64 = 0;
    let mut freed: u64 = 0; // declined + evicted + drained — the caller really frees these
    let mut key: usize = 0x10_0000; // distinct synthetic keys (no spurious double-offer)

    let mut steps = 0u32;
    while !c.done() && steps < 4096 {
        steps += 1;
        let op = c.u8();
        match op % 4 {
            0 | 1 => {
                // Offer a fresh key with a fuzzer-chosen size (a multiple of 16).
                key = key.wrapping_add(0x40);
                let bytes = 16 + (c.u8() as u64 % 16) * 16; // 16..=256
                offered += bytes;
                let entry = QuarantineEntry {
                    user_ptr: key as *mut u8,
                    span: key as *const _, // non-null ⇒ "small"; never dereferenced
                    index: 0,
                    arena: ArenaId::DEFAULT,
                    bytes,
                };
                let mut ev = EvictBatch::new();
                match q.offer(entry, &mut ev) {
                    Offer::Declined => freed += bytes,
                    Offer::Held | Offer::AlreadyQuarantined => {}
                }
                for e in ev.as_slice() {
                    freed += e.bytes;
                }
            }
            2 => {
                // Drain one batch (the caller frees the returned entries).
                for e in q.drain_batch().as_slice() {
                    freed += e.bytes;
                }
            }
            _ => {
                // Toggle the runtime switch — must never desync the accounting.
                q.set_enabled(c.u8() & 1 == 1);
            }
        }
        // Invariants hold after every settled step.
        assert!(q.check_invariants(), "quarantine invariant torn");
        assert!(q.held_bytes() <= max_bytes, "byte budget exceeded");
        assert!(q.held_objects() <= max_objects, "object budget exceeded");
    }

    // Drain the remainder and assert exact byte conservation.
    loop {
        let b = q.drain_batch();
        if b.as_slice().is_empty() {
            break;
        }
        for e in b.as_slice() {
            freed += e.bytes;
        }
    }
    assert_eq!(q.held_bytes(), 0);
    assert_eq!(q.held_objects(), 0);
    assert_eq!(offered, freed, "every offered byte freed exactly once");
});

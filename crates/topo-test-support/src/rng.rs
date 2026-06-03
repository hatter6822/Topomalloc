// SPDX-License-Identifier: MIT
//! A deterministic PRNG and operation generator for deterministic test mode
//! (§30.4) and property/differential harnesses (plan 08). Determinism is the
//! point: the same seed yields the same op stream, so a divergence is
//! reproducible from a single integer.

use alloc::vec::Vec;

/// SplitMix64 — a small, fast, fully deterministic PRNG. Not cryptographic; it
/// exists only to make test inputs reproducible.
#[derive(Clone, Debug)]
pub struct DetRng {
    state: u64,
}

impl DetRng {
    /// Seed the generator. The same seed always yields the same sequence.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (uniform enough for test generation). `n == 0` yields 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

/// An abstract allocator operation for property/differential tests (§34.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `malloc(size)` with the given alignment.
    Malloc {
        /// Requested size in bytes.
        size: usize,
        /// Requested alignment (a power of two).
        align: usize,
    },
    /// `free` of the live allocation at index `which` (modulo the live count).
    Free {
        /// Index into the live set (callers take it modulo the live count).
        which: usize,
    },
    /// `calloc(n, size)`.
    Calloc {
        /// Element count.
        n: usize,
        /// Element size.
        size: usize,
    },
}

/// Generate a deterministic op sequence of length `count` from `rng`.
///
/// The mix favours allocation slightly so the live set grows, exercising the
/// allocator before frees drain it — the shape the property tests want.
pub fn gen_ops(rng: &mut DetRng, count: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        let op = match rng.below(10) {
            0..=5 => {
                let size = (rng.below(256) + 1) as usize;
                let align = 1usize << (rng.below(4)); // 1, 2, 4, or 8
                Op::Malloc { size, align }
            }
            6..=7 => {
                let n = (rng.below(16) + 1) as usize;
                let size = (rng.below(64) + 1) as usize;
                Op::Calloc { n, size }
            }
            _ => Op::Free {
                which: rng.next_u64() as usize,
            },
        };
        ops.push(op);
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = DetRng::new(0xC0FFEE);
        let mut b = DetRng::new(0xC0FFEE);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = DetRng::new(1);
        let mut b = DetRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn gen_ops_is_deterministic_and_sized() {
        let ops1 = gen_ops(&mut DetRng::new(42), 200);
        let ops2 = gen_ops(&mut DetRng::new(42), 200);
        assert_eq!(ops1.len(), 200);
        assert_eq!(ops1, ops2);
        // Generated mallocs use power-of-two alignments.
        for op in &ops1 {
            if let Op::Malloc { align, .. } = op {
                assert!(align.is_power_of_two());
            }
        }
    }
}

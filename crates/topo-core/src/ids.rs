// SPDX-License-Identifier: MIT
//! Core identifier newtypes. These mirror the Lean model's abstract ids
//! (`ArenaId`, `Label`, `SizeClassId` in `SPEC.md` §33.2) so the runtime and the
//! proof talk about the same things.

/// Identifies an arena (policy + authority domain, §22). Arena `0` is the
/// always-present default arena.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ArenaId(pub u32);

impl ArenaId {
    /// The default arena, present from initialization (§35.4 phase 3).
    pub const DEFAULT: ArenaId = ArenaId(0);
}

/// A security/information-flow label (§36.12). On POSIX there is a single
/// `PUBLIC` label; on seLe4n labels partition caches and statistics.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Label(pub u32);

impl Label {
    /// The single label used by the POSIX (single-authority) backend.
    pub const PUBLIC: Label = Label(0);
}

/// Index of a size class into the generated `SIZE_CLASSES` table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SizeClassId(u16);

impl SizeClassId {
    /// Construct from a table index. The caller guarantees `index` is in bounds
    /// of `SIZE_CLASSES`; lookups in `size_class` always satisfy this.
    pub const fn new(index: usize) -> Self {
        debug_assert!(index <= u16::MAX as usize);
        SizeClassId(index as u16)
    }

    /// The table index this id refers to.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identifies a span (the §16.2 descriptor that owns a slab of small objects, a
/// medium allocation, or part of a large allocation). Mirrors the Lean model's
/// `SpanId` (§33.2) so the proof and the runtime name the same descriptor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SpanId(pub u32);

/// Identifies a large-allocation descriptor (§17.2 P-Map-004): the metadata for a
/// single `>= HUGE_THRESHOLD` allocation served by the region/hugepage backend.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LargeId(pub u32);

/// A span generation counter (§16.6 / §27.5). Bumped whenever a span descriptor
/// is recycled for a different size class, arena, or allocation type, so a stale
/// reference captured before the recycle can be detected (ABA / use-after-free
/// protection on the classification path, W3-5).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Generation(pub u32);

impl Generation {
    /// The generation of a freshly created (never-recycled) span.
    pub const FIRST: Generation = Generation(0);

    /// The next generation. **Wraps** rather than saturates: saturating at
    /// `u32::MAX` would pin every recycle past the `2^32`-th at the same value, so a
    /// [`GenGuard`](crate::span::GenGuard) captured *at* the saturated value would
    /// then match every later incarnation of that slot — silently defeating the ABA
    /// guard exactly when the counter is exhausted (§27.5). Wrapping keeps
    /// consecutive incarnations distinct; an ABA collision then needs an exact
    /// `2^32`-recycle alignment of one slot, the irreducible limit of a 32-bit tag.
    /// `0` is skipped on wrap so it stays the sentinel for a never-recycled span
    /// ([`FIRST`](Self::FIRST)).
    #[inline]
    pub const fn next(self) -> Generation {
        match self.0.wrapping_add(1) {
            0 => Generation(1),
            n => Generation(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_next_increments_then_wraps_skipping_zero() {
        // Normal increments (the overwhelmingly common case) are unchanged.
        assert_eq!(Generation::FIRST.next(), Generation(1));
        assert_eq!(Generation(41).next(), Generation(42));
        assert_eq!(Generation(u32::MAX - 1).next(), Generation(u32::MAX));

        // At the boundary it *wraps* rather than saturating, so a slot's incarnations
        // stay distinct — saturating would pin every later recycle at MAX and let a
        // `GenGuard` captured there match forever (the ABA hole this guards against).
        // 0 is skipped so it stays the never-recycled sentinel (`FIRST`).
        assert_eq!(Generation(u32::MAX).next(), Generation(1));
        assert_ne!(Generation(u32::MAX).next(), Generation(u32::MAX));
        assert_ne!(Generation(u32::MAX).next(), Generation::FIRST);
    }
}

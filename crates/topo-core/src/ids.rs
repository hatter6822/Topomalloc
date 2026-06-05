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

    /// The next generation, saturating at `u32::MAX` so a recycle can never wrap a
    /// generation back onto a value a live stale reference might still hold (a
    /// wrap would defeat the ABA guard, §27.5). Saturation is safe: `2^32`
    /// recycles of one descriptor slot without the address space being reused is
    /// not reachable, and pinning at the top only makes the guard *more*
    /// conservative (every further compare reports "stale").
    #[inline]
    pub const fn next(self) -> Generation {
        Generation(self.0.saturating_add(1))
    }
}

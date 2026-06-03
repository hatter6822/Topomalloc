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

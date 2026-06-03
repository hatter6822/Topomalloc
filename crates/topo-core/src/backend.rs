// SPDX-License-Identifier: MIT
//! The backing-provider seam (§3, §36.6). Everything OS/kernel goes through
//! `TopoBackingProvider`; the allocator core never calls `mmap` or `retype`.
//!
//! This is the **M0 subset** of the full §36.6 contract (plan 04 owns the full
//! interface — `reserve_window`/`create_frame`/`map_frame`/…). M0 needs only
//! enough to bump-allocate through the seam and prove that POSIX and the seLe4n
//! simulator are co-equal behind one trait (W0-14a/b). Later work *extends* this
//! trait; it does not replace it.

use crate::error::BackendError;
use crate::ids::ArenaId;

/// Access rights for a backing reservation. On POSIX these map to `mprotect`
/// bits; on seLe4n to `AccessRights`/`PagePerms` (plan 09).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rights(u8);

impl Rights {
    /// Readable.
    pub const READ: Rights = Rights(0b01);
    /// Writable.
    pub const WRITE: Rights = Rights(0b10);
    /// Readable and writable — the default for allocator-owned memory.
    pub const READ_WRITE: Rights = Rights(0b11);

    /// Whether `self` grants at least the rights in `other`.
    #[inline]
    pub fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }
}

/// A committed backing region handed out by a provider. It is a view (address +
/// length); the *provider* owns the memory's lifetime and reclaims it in
/// `release`. On POSIX it is an mmap range; on seLe4n a mapped frame run.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Base address of the region (aligned to at least the requested alignment).
    pub base: *mut u8,
    /// Length of the region in bytes.
    pub len: usize,
}

// SAFETY: `Region` is a plain address+length descriptor and carries no
// ownership — the provider owns and synchronizes the backing store. Sending or
// sharing a `Region` only moves/copies the address, so it is sound across
// threads; concurrent *access* to the bytes is synchronized by the allocator
// that uses it (e.g. `SkeletonAllocator`'s atomic cursor).
unsafe impl Send for Region {}
// SAFETY: see the `Send` impl above — `Region` is a Copy descriptor with no
// interior mutability of its own.
unsafe impl Sync for Region {}

/// The backing-provider seam. POSIX is the degenerate single ambient-authority,
/// single-label case; seLe4n supplies the capability case behind the identical
/// interface (overview §3, D2).
pub trait TopoBackingProvider {
    /// Reserve a region of at least `size` bytes aligned to `align` (a power of
    /// two) for `arena`. The region is not necessarily usable until `commit`.
    fn reserve(&self, arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError>;

    /// Make `[offset, offset+len)` within `region` usable (backed). Committing
    /// twice, or committing a sub-range, must be idempotent and safe.
    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError>;

    /// Return `region` to the backend. After this the region must not be used.
    /// Failure must leave both allocator and backend state well-formed (§36.6).
    fn release(&self, arena: ArenaId, region: Region) -> Result<(), BackendError>;

    /// A short, stable name for diagnostics and trace output (e.g. `"posix"`).
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rights_contains() {
        assert!(Rights::READ_WRITE.contains(Rights::READ));
        assert!(Rights::READ_WRITE.contains(Rights::WRITE));
        assert!(!Rights::READ.contains(Rights::WRITE));
        assert!(Rights::READ_WRITE.contains(Rights::READ_WRITE));
    }
}

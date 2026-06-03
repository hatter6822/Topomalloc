// SPDX-License-Identifier: MIT
//! `PosixBackingProvider` — the degenerate single ambient-authority,
//! single-label case of the §36.6 backing-provider contract (overview §3, D2).
//!
//! **M0 scope.** This is the walking-skeleton provider: it owns its backing
//! store on the host (via the global allocator) so the skeleton can run with no
//! syscalls and no external dependencies. The real POSIX provider — `mmap` for
//! reservation, `madvise(DONTNEED/FREE)` and `mprotect` for commit/decommit and
//! dirty/muzzy/released state — arrives in plan 04 (W4-3) behind this same
//! trait, so nothing above the seam changes.

use std::alloc::{alloc, dealloc, Layout};
use std::sync::Mutex;

use topo_core::{ArenaId, BackendError, Region, TopoBackingProvider};

/// A live backing allocation owned by the provider.
struct Owned {
    base: usize,
    layout: Layout,
}

/// Host-backed POSIX provider (M0). `reserve` hands out a host allocation; the
/// provider owns its lifetime and reclaims it in `release`/`Drop`.
#[derive(Default)]
pub struct PosixBackingProvider {
    owned: Mutex<Vec<Owned>>,
}

impl PosixBackingProvider {
    /// Create an empty provider.
    pub fn new() -> Self {
        Self::default()
    }
}

impl TopoBackingProvider for PosixBackingProvider {
    fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(BackendError::InvalidRequest);
        }
        let layout =
            Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
        // SAFETY: `layout` has nonzero size (checked above) and a valid
        // power-of-two alignment, so calling the global allocator is sound.
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        self.owned
            .lock()
            .expect("provider mutex poisoned")
            .push(Owned {
                base: base as usize,
                layout,
            });
        Ok(Region { base, len: size })
    }

    fn commit(&self, _region: Region, _offset: usize, _len: usize) -> Result<(), BackendError> {
        // Host memory is committed on reservation; nothing to do for M0.
        Ok(())
    }

    fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let mut owned = self.owned.lock().expect("provider mutex poisoned");
        let base = region.base as usize;
        let idx = owned
            .iter()
            .position(|o| o.base == base)
            .ok_or(BackendError::InvalidRequest)?;
        let o = owned.swap_remove(idx);
        // SAFETY: `o.base`/`o.layout` are exactly the pointer and layout from the
        // matching `alloc` in `reserve`, and the region is removed from the set
        // so it cannot be freed twice.
        unsafe { dealloc(o.base as *mut u8, o.layout) };
        Ok(())
    }

    fn name(&self) -> &'static str {
        "posix"
    }
}

impl Drop for PosixBackingProvider {
    fn drop(&mut self) {
        // Reclaim anything not explicitly released so tests/sanitizers see no leak.
        for o in self
            .owned
            .get_mut()
            .expect("provider mutex poisoned")
            .drain(..)
        {
            // SAFETY: same invariant as `release` — each entry pairs a live
            // allocation with its original layout, freed exactly once here.
            unsafe { dealloc(o.base as *mut u8, o.layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_commit_write_release_roundtrip() {
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        assert_eq!(r.len, 4096);
        assert_eq!(r.base as usize % 16, 0);
        // SAFETY: the region is committed for its full length; writing within
        // `[0, len)` is in bounds.
        unsafe {
            for i in 0..r.len {
                r.base.add(i).write(0xab);
            }
            assert_eq!(r.base.add(10).read(), 0xab);
        }
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn rejects_zero_size_and_bad_align() {
        let p = PosixBackingProvider::new();
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 0, 16),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 16, 24),
            Err(BackendError::InvalidRequest)
        ));
    }

    #[test]
    fn name_is_posix() {
        assert_eq!(PosixBackingProvider::new().name(), "posix");
    }
}

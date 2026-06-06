// SPDX-License-Identifier: MIT
//! `PosixBackingProvider` — the degenerate single ambient-authority,
//! single-label case of the §36.6 backing-provider contract (overview §3, D2).
//!
//! **Scope.** This is the host-backed provider: it owns its backing store on the
//! host (via the global allocator) so the allocator runs with no syscalls and no
//! external dependencies — keeping the dual-backend co-equality test (D2)
//! deterministic and the CI/QEMU runs hermetic. The §20.4 physical-state
//! operations (`commit`/`decommit`/`purge_lazy`/`purge_forced`) are implemented to
//! **faithfully model the documented Linux `madvise`/`mprotect` mapping** below;
//! the back-end's [`ExtentManager`](topo_core::ExtentManager) drives them and is
//! the authority on the dirty/muzzy/released byte accounting that reconciles in
//! stats (plan 07). Swapping the host store for real `mmap`/`madvise`/`mprotect`
//! is a localized substitution **behind this same trait** — nothing above the seam
//! changes (the §36.6 collapse documented on [`TopoBackingProvider`]).
//!
//! ## Physical-state mapping (§20.4, W4-3a) — documented per platform
//!
//! TopoMalloc's memory states (§20.1) map to OS primitives as follows. The host
//! provider performs the **observable equivalent** of each (so tests see the right
//! behaviour) and the table is the contract the real-syscall provider implements:
//!
//! | TopoMalloc op (§20.4) | TopoMalloc state | Linux | macOS / BSD | host model here |
//! |---|---|---|---|---|
//! | `commit` | committed | `mprotect(PROT_READ\|WRITE)` / first-touch fault | `mprotect` / fault | no-op (host memory is already backed) |
//! | `decommit` | released | `madvise(MADV_DONTNEED)` | `madvise(MADV_FREE_REUSABLE)` | discard contents now (zero the range) |
//! | `purge_lazy` | muzzy | `madvise(MADV_FREE)` | `madvise(MADV_FREE)` | no-op (contents *may* persist — a valid `MADV_FREE` outcome: "reuse may be cheap or may fault", §20.1) |
//! | `purge_forced` | muzzy (scrubbed) | `madvise(MADV_DONTNEED)` | `madvise(MADV_FREE_REUSABLE)` | discard contents now (zero the range) |
//! | `release` (whole region) | released/recycled | `munmap` | `munmap` | `dealloc` the host allocation |
//! | `revoke_descendants` | — | n/a (single ambient authority) | n/a | no-op |
//!
//! On Linux, `MADV_DONTNEED` drops the pages immediately (RSS falls now; a later
//! read faults a fresh **zero** page), whereas `MADV_FREE` is lazy (the kernel
//! reclaims under pressure and the old contents survive until then) — exactly the
//! §20.1 dirty-vs-muzzy distinction. The host model reproduces the *observable*
//! difference: `decommit`/`purge_forced` zero the range now; `purge_lazy` leaves it
//! intact. Counters ([`commit_calls`](PosixBackingProvider::commit_calls), …) let
//! tests confirm the mapping is exercised; the byte-level dirty/muzzy/released
//! accounting lives in the extent manager (`committed_bytes`/`state_bytes`), which
//! is what reconciles in stats (plan 07).

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use topo_core::{ArenaId, BackendError, Region, TopoBackingProvider};

/// A live backing allocation owned by the provider.
struct Owned {
    base: usize,
    layout: Layout,
}

/// Host-backed POSIX provider. `reserve` hands out a host allocation; the provider
/// owns its lifetime and reclaims it in `release`/`Drop`. The §20.4 physical-state
/// operations model the documented `madvise`/`mprotect` mapping (see the module
/// docs).
#[derive(Default)]
pub struct PosixBackingProvider {
    owned: Mutex<Vec<Owned>>,
    commit_calls: AtomicU64,
    decommit_calls: AtomicU64,
    purge_lazy_calls: AtomicU64,
    purge_forced_calls: AtomicU64,
}

impl PosixBackingProvider {
    /// Create an empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of `commit` calls (observability; lets a test confirm the §20.4
    /// mapping is exercised — W4-3a "states reconcile in stats").
    pub fn commit_calls(&self) -> u64 {
        self.commit_calls.load(Ordering::Relaxed)
    }

    /// Number of `decommit` calls.
    pub fn decommit_calls(&self) -> u64 {
        self.decommit_calls.load(Ordering::Relaxed)
    }

    /// Number of `purge_lazy` calls.
    pub fn purge_lazy_calls(&self) -> u64 {
        self.purge_lazy_calls.load(Ordering::Relaxed)
    }

    /// Number of `purge_forced` calls.
    pub fn purge_forced_calls(&self) -> u64 {
        self.purge_forced_calls.load(Ordering::Relaxed)
    }

    /// Validate that `[offset, offset+len)` lies within `region`, returning the
    /// sub-range's start pointer. A request outside the region is a caller bug
    /// (the extent manager always passes an in-range sub-extent); reject it rather
    /// than computing an out-of-bounds pointer.
    fn checked_subrange(
        region: Region,
        offset: usize,
        len: usize,
    ) -> Result<*mut u8, BackendError> {
        let end = offset
            .checked_add(len)
            .ok_or(BackendError::InvalidRequest)?;
        if end > region.len {
            return Err(BackendError::InvalidRequest);
        }
        // SAFETY: `offset <= region.len` and the region is a single allocation of
        // `region.len` bytes, so `base + offset` is in bounds (one-past-the-end at
        // most when `len == 0`).
        Ok(unsafe { region.base.add(offset) })
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

    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 commit: on Linux this is `mprotect(PROT_READ|WRITE)` / a first-touch
        // fault. Host memory is committed on reservation, so this only validates the
        // range (and is the recommit M-005 requires before a released page is reused).
        Self::checked_subrange(region, offset, len)?;
        self.commit_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 decommit ≈ Linux `madvise(MADV_DONTNEED)`: the contents are dropped
        // now (RSS falls; a later read faults a fresh zero page). Model the
        // observable effect by zeroing the range.
        let p = Self::checked_subrange(region, offset, len)?;
        self.decommit_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `checked_subrange` confirmed `[offset, offset+len)` is in bounds
        // of the committed region `p` points into.
        unsafe { std::ptr::write_bytes(p, 0, len) };
        Ok(())
    }

    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_lazy ≈ Linux `madvise(MADV_FREE)`: lazy discard. The host
        // cannot lazily reclaim, so the contents persist — a valid `MADV_FREE`
        // outcome (§20.1: reuse "may be cheap or may fault"). No mutation.
        Self::checked_subrange(region, offset, len)?;
        self.purge_lazy_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_forced ≈ Linux `madvise(MADV_DONTNEED)`: discard contents
        // promptly. Model by zeroing the range now.
        let p = Self::checked_subrange(region, offset, len)?;
        self.purge_forced_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as in `decommit` — the sub-range is in bounds of the region.
        unsafe { std::ptr::write_bytes(p, 0, len) };
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

    // `revoke_descendants` uses the default no-op (single ambient authority — there
    // are no descendant capabilities to revoke on POSIX, §36.6).

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
    fn decommit_discards_contents_like_madv_dontneed() {
        // §20.4 / W4-3a: decommit models MADV_DONTNEED — a later read sees a fresh
        // zero page. The host model zeroes the range so the effect is observable.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        // SAFETY: committed for its whole length.
        unsafe {
            std::ptr::write_bytes(r.base, 0xcd, r.len);
            assert_eq!(r.base.add(100).read(), 0xcd);
        }
        p.decommit(r, 0, r.len).expect("decommit");
        // After decommit the contents are gone (fresh zeros).
        // SAFETY: still mapped (host memory); reading is in bounds.
        unsafe {
            for i in 0..r.len {
                assert_eq!(r.base.add(i).read(), 0, "decommit must discard contents");
            }
        }
        assert_eq!(p.decommit_calls(), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn purge_lazy_may_retain_purge_forced_discards() {
        // purge_lazy ≈ MADV_FREE: contents MAY persist (host model retains them).
        // purge_forced ≈ MADV_DONTNEED: contents discarded now.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        // SAFETY: committed region.
        unsafe { std::ptr::write_bytes(r.base, 0x77, r.len) };
        p.purge_lazy(r, 0, r.len).expect("purge_lazy");
        // SAFETY: in bounds; lazy purge retained the contents on the host.
        unsafe { assert_eq!(r.base.read(), 0x77, "lazy purge may retain (MADV_FREE)") };
        p.purge_forced(r, 0, r.len).expect("purge_forced");
        // SAFETY: in bounds; forced purge discarded the contents.
        unsafe { assert_eq!(r.base.read(), 0, "forced purge discards (MADV_DONTNEED)") };
        assert_eq!(p.purge_lazy_calls(), 1);
        assert_eq!(p.purge_forced_calls(), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn physical_ops_reject_out_of_range_subranges() {
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        assert!(matches!(
            p.decommit(r, 4096, 1),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            p.commit(r, 0, usize::MAX),
            Err(BackendError::InvalidRequest)
        ));
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn revoke_descendants_is_a_noop_on_posix() {
        // §36.6: POSIX has a single ambient authority — there are no descendant
        // capabilities to revoke, so the default no-op applies.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        assert!(p.revoke_descendants(ArenaId::DEFAULT, r).is_ok());
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn name_is_posix() {
        assert_eq!(PosixBackingProvider::new().name(), "posix");
    }
}

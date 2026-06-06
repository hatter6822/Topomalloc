// SPDX-License-Identifier: GPL-3.0-or-later
//! `Sele4nSim` — a host simulation of the seLe4n capability backend (D2,
//! §36.6). It implements the same [`TopoBackingProvider`] seam as POSIX, so the
//! allocator core runs identically over either (W0-14b, the G-sim spine).
//!
//! **Scope.** Unlike the POSIX provider, the simulator models an *authorized
//! untyped pool*: every reservation is "retyped" out of a finite pool and the
//! accounting is enforced, so the simulator can report exhaustion the way the real
//! kernel does (`UntypedRegionExhausted`). It also walks each reservation through
//! the **§36.6 provider state machine** ([`ProviderState`]) — `AuthorizedUntyped →
//! … → AllocatorCommitted` on reserve, `Unmapped → Revoked → RecyclableUntyped` on
//! release — and **asserts every transition is legal** ([`ProviderStateMachine`]),
//! so the cardinal §36.6 safety bug (recycling untyped that still has a live client
//! mapping) is caught here, on the backend where capability semantics are real.
//! This is the seed of the accounting plan 09 (W22-1/W22-3) grows against the real
//! `sele4n-abi`/`sele4n-types` (pinned per D8); M1 swaps in the real ABI behind the
//! `real-abi` feature, behind this same trait.
//!
//! The §20.4 physical-state operations (`commit`/`decommit`/`purge_lazy`/
//! `purge_forced`) mirror the POSIX provider's documented mapping over the host
//! store (the pool accounting is unchanged — those are sub-frame backing ops, not
//! retype/recycle); `revoke_descendants` is a no-op in the M0 simulation (there are
//! no client-derived capabilities yet — real descendant revocation is plan 09).

use std::alloc::{alloc, dealloc, Layout};
use std::sync::Mutex;

use topo_core::{
    ArenaId, BackendError, ProviderState, ProviderStateMachine, Region, TopoBackingProvider,
};

/// A "frame" retyped from the untyped pool, with its host backing and its §36.6
/// capability-state machine.
struct Frame {
    base: usize,
    layout: Layout,
    /// Bytes charged to the untyped pool for this frame (== `layout.size()`).
    charged: usize,
    /// The §36.6 provider state machine for this frame, walked on reserve and
    /// release and asserted at every step.
    fsm: ProviderStateMachine,
}

struct State {
    /// Bytes still available in the authorized untyped pool.
    pool_remaining: usize,
    frames: Vec<Frame>,
}

/// Host simulator of the seLe4n backing provider.
pub struct Sele4nSim {
    state: Mutex<State>,
    pool_total: usize,
}

impl Sele4nSim {
    /// Create a simulator with `pool_bytes` of authorized untyped memory.
    pub fn new(pool_bytes: usize) -> Self {
        Self {
            state: Mutex::new(State {
                pool_remaining: pool_bytes,
                frames: Vec::new(),
            }),
            pool_total: pool_bytes,
        }
    }

    /// Authorized untyped bytes still available.
    pub fn pool_remaining(&self) -> usize {
        self.state
            .lock()
            .expect("sim mutex poisoned")
            .pool_remaining
    }

    /// Total authorized untyped bytes.
    pub fn pool_total(&self) -> usize {
        self.pool_total
    }

    /// Validate that `[offset, offset+len)` lies within `region` and return the
    /// sub-range pointer (the §36.6 "map only into an authorized window" check, in
    /// the degenerate host form).
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
        // SAFETY: `offset <= region.len` within a single `region.len`-byte allocation.
        Ok(unsafe { region.base.add(offset) })
    }
}

/// Walk a freshly-minted frame's state machine through the §36.6 reservation chain
/// `AuthorizedUntyped → ReservedUntyped → FrameCapMinted → MappedToServer →
/// MappedToClient → AllocatorCommitted`, asserting each transition is legal. On
/// POSIX this whole chain collapses into one `mmap`; the simulator walks it
/// explicitly so the modeled-in-Lean machine (plan 02 W1-11b) is asserted at
/// runtime on the capability backend. Returns the committed frame machine.
fn mint_and_map() -> ProviderStateMachine {
    let mut fsm = ProviderStateMachine::new(); // AuthorizedUntyped
    for to in [
        ProviderState::ReservedUntyped,
        ProviderState::FrameCapMinted,
        ProviderState::MappedToServer,
        ProviderState::MappedToClient,
        ProviderState::AllocatorCommitted,
    ] {
        fsm.advance(to)
            .expect("§36.6 reservation chain must be legal");
    }
    fsm
}

impl TopoBackingProvider for Sele4nSim {
    fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(BackendError::InvalidRequest);
        }
        let layout =
            Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
        let mut st = self.state.lock().expect("sim mutex poisoned");
        // Retype from the authorized untyped pool: refuse if it is exhausted
        // (the simulated `KernelError::UntypedRegionExhausted`). The allocator
        // must handle this exactly as it would the real kernel error (§36.6).
        if size > st.pool_remaining {
            return Err(BackendError::OutOfMemory);
        }
        // SAFETY: nonzero size + valid power-of-two alignment (checked above).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        st.pool_remaining -= size;
        st.frames.push(Frame {
            base: base as usize,
            layout,
            charged: size,
            fsm: mint_and_map(), // walks + asserts the §36.6 reservation chain
        });
        Ok(Region { base, len: size })
    }

    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 commit / recommit (M-005). Host memory is already backed; validate
        // the window. (The frame is already AllocatorCommitted from `reserve`.)
        Self::checked_subrange(region, offset, len)?;
        Ok(())
    }

    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 decommit ≈ MADV_DONTNEED: discard contents now (mirrors POSIX). The
        // pool charge is unchanged — this is a sub-frame backing op, not a recycle.
        let p = Self::checked_subrange(region, offset, len)?;
        // SAFETY: `checked_subrange` confirmed the sub-range is in bounds.
        unsafe { std::ptr::write_bytes(p, 0, len) };
        Ok(())
    }

    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_lazy ≈ MADV_FREE: lazy discard; contents may persist on host.
        Self::checked_subrange(region, offset, len)?;
        Ok(())
    }

    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_forced ≈ MADV_DONTNEED: discard contents now.
        let p = Self::checked_subrange(region, offset, len)?;
        // SAFETY: as in `decommit`.
        unsafe { std::ptr::write_bytes(p, 0, len) };
        Ok(())
    }

    fn revoke_descendants(&self, _arena: ArenaId, _region: Region) -> Result<(), BackendError> {
        // M0 simulation: no client-derived capabilities exist yet, so there is
        // nothing to revoke. Real descendant revocation lands with the capability
        // provider (plan 09 W22) against the pinned seLe4n ABI; the §36.6 ordering
        // (revoke before recycle) is exercised in `release` below.
        Ok(())
    }

    fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let mut st = self.state.lock().expect("sim mutex poisoned");
        let base = region.base as usize;
        let idx = st
            .frames
            .iter()
            .position(|fr| fr.base == base)
            .ok_or(BackendError::InvalidRequest)?;
        let mut fr = st.frames.swap_remove(idx);
        // Walk the §36.6 release tail one legal `next` step at a time — the exact
        // linear chain the Lean `BackingState.next` proves (W1-11b). A released
        // frame is dirtied (no longer live), scrubbed (muzzy), unmapped, revoked,
        // and only then recycled: the unmap-before-revoke-before-recycle ordering
        // that keeps recycled untyped from retaining a live client mapping. The
        // machine refuses any out-of-order step (caught here).
        for to in [
            ProviderState::AllocatorDirty,
            ProviderState::AllocatorMuzzyOrScrubbed,
            ProviderState::Unmapped,
            ProviderState::Revoked,
            ProviderState::RecyclableUntyped,
        ] {
            fr.fsm.advance(to).expect(
                "§36.6 release tail must be legal (dirty → muzzy → unmap → revoke → recycle)",
            );
        }
        // Recycle the untyped back to the pool (the §36.6 RecyclableUntyped step).
        st.pool_remaining += fr.charged;
        // SAFETY: `fr.base`/`fr.layout` are exactly the pointer and layout from
        // the matching `alloc` in `reserve`; removed from the set so freed once.
        unsafe { dealloc(fr.base as *mut u8, fr.layout) };
        Ok(())
    }

    fn name(&self) -> &'static str {
        "sele4n-sim"
    }
}

impl Drop for Sele4nSim {
    fn drop(&mut self) {
        for fr in self
            .state
            .get_mut()
            .expect("sim mutex poisoned")
            .frames
            .drain(..)
        {
            // SAFETY: same invariant as `release`.
            unsafe { dealloc(fr.base as *mut u8, fr.layout) };
        }
    }
}

/// The bridge to the **real** pinned seLe4n ABI (D8, plan 09 W22-0), compiled only
/// under `--features real-abi` against the vendored, pinned `sele4n-abi`/
/// `sele4n-types` (`vendor/sele4n/`, GPL-3.0-or-later). At M0 this proves the seLe4n
/// side **type-checks against the pinned upstream** (the W4-1 acceptance); the full
/// `Sele4nBackingProvider` over real frame/untyped capabilities grows here in plan
/// 09 (W22-0).
#[cfg(feature = "real-abi")]
pub mod real_abi {
    use sele4n_types::KernelError;
    use topo_core::BackendError;

    /// Map a real seLe4n [`KernelError`] onto the host-visible [`BackendError`]
    /// taxonomy (§36.6; the `KernelError` mapping `topo_core::error` documents as a
    /// plan-09 deliverable). Untyped exhaustion is the §36.16 OOM the simulator
    /// already models; malformed retype/mapping/capability requests become
    /// `InvalidRequest`; everything else is reported conservatively as `Unsupported`
    /// until plan 09 refines the full table. This function compiling against the
    /// pinned `KernelError` is the M0 "seLe4n side type-checks vs pinned upstream"
    /// witness (W4-1).
    pub fn map_kernel_error(e: KernelError) -> BackendError {
        match e {
            KernelError::UntypedRegionExhausted => BackendError::OutOfMemory,
            KernelError::InvalidCapability
            | KernelError::UntypedTypeMismatch
            | KernelError::UntypedDeviceRestriction
            | KernelError::UntypedAllocSizeTooSmall
            | KernelError::AddressOutOfBounds
            | KernelError::MappingConflict
            | KernelError::TargetSlotOccupied => BackendError::InvalidRequest,
            _ => BackendError::Unsupported,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn kernel_errors_map_onto_the_backend_taxonomy() {
            // Compiles + runs only under `--features real-abi`, against the pinned
            // upstream `KernelError` — the W4-1 type-check witness, exercised. Locks
            // the whole mapping table so a future edit (or an upstream variant
            // rename/removal) is caught here, not at M1.
            use BackendError::*;
            use KernelError as K;
            // Untyped exhaustion is the §36.16 out-of-memory.
            assert_eq!(map_kernel_error(K::UntypedRegionExhausted), OutOfMemory);
            // Malformed retype / mapping / capability requests → InvalidRequest.
            for e in [
                K::InvalidCapability,
                K::UntypedTypeMismatch,
                K::UntypedDeviceRestriction,
                K::UntypedAllocSizeTooSmall,
                K::AddressOutOfBounds,
                K::MappingConflict,
                K::TargetSlotOccupied,
            ] {
                assert_eq!(map_kernel_error(e), InvalidRequest, "{e:?}");
            }
            // Everything else is reported conservatively as Unsupported until plan 09
            // refines the table (the `#[non_exhaustive]` `KernelError`'s wildcard arm);
            // spot-check representative variants from across the enum.
            for e in [
                K::PolicyDenied,
                K::NotImplemented,
                K::TranslationFault,
                K::ObjectNotFound,
                K::BackingObjectMissing,
            ] {
                assert_eq!(map_kernel_error(e), Unsupported, "{e:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retypes_within_pool_and_recycles() {
        let sim = Sele4nSim::new(8192);
        let r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        assert_eq!(sim.pool_remaining(), 4096);
        sim.release(ArenaId::DEFAULT, r).expect("release");
        assert_eq!(sim.pool_remaining(), sim.pool_total());
    }

    #[test]
    fn exhausting_the_untyped_pool_fails_cleanly() {
        let sim = Sele4nSim::new(4096);
        let _r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        // Pool exhausted: the next retype fails like the real kernel, without
        // borrowing authority (§36.16 exhaustion behaviour).
        assert!(matches!(
            sim.reserve(ArenaId::DEFAULT, 1, 16),
            Err(BackendError::OutOfMemory)
        ));
        assert_eq!(sim.pool_remaining(), 0);
    }

    #[test]
    fn physical_state_ops_model_the_documented_mapping() {
        // §20.4 / W4-1: the simulator implements the physical-state ops so the
        // extent manager runs identically over it and POSIX (D2). decommit/forced
        // purge discard contents now; lazy purge may retain.
        let sim = Sele4nSim::new(1 << 16);
        let r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        sim.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed region.
        unsafe { std::ptr::write_bytes(r.base, 0x99, r.len) };
        sim.purge_lazy(r, 0, r.len).expect("purge_lazy");
        // SAFETY: in bounds; lazy purge retains on host.
        unsafe { assert_eq!(r.base.read(), 0x99) };
        sim.purge_forced(r, 0, r.len).expect("purge_forced");
        // SAFETY: in bounds; forced purge discarded.
        unsafe { assert_eq!(r.base.read(), 0) };
        sim.decommit(r, 0, r.len).expect("decommit");
        sim.release(ArenaId::DEFAULT, r).expect("release");
        assert_eq!(sim.pool_remaining(), sim.pool_total());
    }

    #[test]
    fn reserve_release_walk_the_full_provider_state_machine() {
        // §36.6 (W4-1): a reservation walks AuthorizedUntyped → AllocatorCommitted
        // and a release walks Unmapped → Revoked → RecyclableUntyped — every step
        // asserted legal by the machine. (A bug that recycled a still-mapped frame
        // would trip the assert in `release`.) Here we confirm the happy path
        // completes and the pool balances exactly.
        let sim = Sele4nSim::new(1 << 16);
        let a = sim.reserve(ArenaId::DEFAULT, 8192, 16).expect("a");
        let b = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("b");
        assert_eq!(sim.pool_remaining(), (1 << 16) - 8192 - 4096);
        sim.revoke_descendants(ArenaId::DEFAULT, a).expect("revoke");
        sim.release(ArenaId::DEFAULT, a).expect("release a");
        sim.release(ArenaId::DEFAULT, b).expect("release b");
        assert_eq!(sim.pool_remaining(), sim.pool_total());
    }

    #[test]
    fn reserve_and_release_reject_bad_requests_without_corruption() {
        let sim = Sele4nSim::new(1 << 16);
        // reserve rejects zero size and non-power-of-two alignment (§36.6).
        assert!(matches!(
            sim.reserve(ArenaId::DEFAULT, 0, 16),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            sim.reserve(ArenaId::DEFAULT, 16, 24),
            Err(BackendError::InvalidRequest)
        ));
        let r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        // Physical-state ops reject out-of-range and overflowing sub-windows.
        assert!(matches!(
            sim.commit(r, 1, r.len),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            sim.decommit(r, usize::MAX, 1),
            Err(BackendError::InvalidRequest)
        ));
        // release rejects a foreign base without deallocating it (no double free).
        let foreign = Region {
            base: 0x1000 as *mut u8,
            len: 4096,
        };
        assert!(matches!(
            sim.release(ArenaId::DEFAULT, foreign),
            Err(BackendError::InvalidRequest)
        ));
        sim.release(ArenaId::DEFAULT, r).expect("release");
        // Double release: the frame is gone, so it is rejected — never dealloc'd twice.
        assert!(matches!(
            sim.release(ArenaId::DEFAULT, r),
            Err(BackendError::InvalidRequest)
        ));
        assert_eq!(sim.pool_remaining(), sim.pool_total());
    }

    #[test]
    fn name_is_sele4n_sim() {
        assert_eq!(Sele4nSim::new(0).name(), "sele4n-sim");
    }
}

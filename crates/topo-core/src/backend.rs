// SPDX-License-Identifier: MIT
//! The backing-provider seam (§3, §36.6). Everything OS/kernel goes through
//! `TopoBackingProvider`; the allocator core never calls `mmap` or `retype`.
//!
//! POSIX is the **degenerate single ambient-authority, single-label case** of the
//! §36.6 contract; seLe4n supplies the capability case behind the identical
//! interface (overview §3, D2). On POSIX the §36.6 capability types collapse:
//!
//! | §36.6 concept | POSIX collapse |
//! |---|---|
//! | `VSpaceWindow` / `FrameCap` / `MappedRange` | an address range ([`Region`]) |
//! | `Cap` (a capability to revoke) | the address range; `revoke_descendants` is a no-op |
//! | `reserve_window ∘ create_frame ∘ map_frame` | a single [`reserve`](TopoBackingProvider::reserve) (an `mmap`) |
//! | `AccessRights` / `PagePerms` | [`Rights`] (`mprotect` bits) |
//! | `cache_policy` | [`CachePolicy`] (mapping cacheability) |
//!
//! So `reserve` is the POSIX collapse of the §36.6 `reserve_window`,
//! `create_frame`, and `map_frame` triple (with `READ_WRITE` rights and a
//! [`CachePolicy::Cached`] mapping); the physical-state operations
//! ([`commit`](TopoBackingProvider::commit) / [`decommit`](TopoBackingProvider::decommit)
//! / [`purge_lazy`](TopoBackingProvider::purge_lazy) /
//! [`purge_forced`](TopoBackingProvider::purge_forced)) realize the
//! `AllocatorCommitted → AllocatorDirty → AllocatorMuzzyOrScrubbed` transitions of
//! the [`ProviderState`] machine; and [`release`](TopoBackingProvider::release) /
//! [`revoke_descendants`](TopoBackingProvider::revoke_descendants) realize the
//! `Unmapped → Revoked → RecyclableUntyped` tail (a no-op on POSIX, real
//! capability work on seLe4n). The state machine itself is modeled in Lean (plan
//! 02 W1-11b) and asserted at runtime by [`ProviderStateMachine`].
//!
//! Every seam operation is **fallible and leaves both allocator and backend state
//! well-formed on failure** (§36.6: "Backing-provider failure MUST leave
//! TopoMalloc and seLe4n state well-formed"); the back-end's extent manager
//! ([`crate::extent`]) relies on that to keep its invariants green under
//! failure injection (plan 04 W4-5).

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

/// Cacheability of a backing mapping (§36.8.1 "cache policy"). On POSIX every
/// heap mapping is [`Cached`](CachePolicy::Cached); the other policies exist for
/// device/DMA arenas (plan 09) and are carried so the seam shape matches §36.6
/// without the allocator core ever having to special-case them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CachePolicy {
    /// Normal write-back cached memory — the default for allocator heap.
    #[default]
    Cached,
    /// Uncached (device) memory.
    Uncached,
    /// Write-combining memory (framebuffers / streaming writes).
    WriteCombining,
}

/// A committed backing region handed out by a provider. It is a view (address +
/// length); the *provider* owns the memory's lifetime and reclaims it in
/// `release`. On POSIX it is an mmap range; on seLe4n a mapped frame run — the
/// collapse of a §36.6 `MappedRange`.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    /// Base address of the region (aligned to at least the requested alignment).
    pub base: *mut u8,
    /// Length of the region in bytes.
    pub len: usize,
}

impl Region {
    /// The half-open address range `[base, base + len)` as integers, for the
    /// allocator's own bookkeeping (the extent manager, plan 04). The provider
    /// owns the bytes; this is only an address view.
    #[inline]
    pub fn addr_range(self) -> (usize, usize) {
        let base = self.base as usize;
        (base, base.wrapping_add(self.len))
    }
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

/// The §36.6 backing-provider state machine. Every backing resource walks this
/// chain; the providers assert their internal transitions against
/// [`can_transition`](ProviderState::can_transition) so an illegal jump (e.g.
/// recycling untyped memory that still has a live client mapping, the cardinal
/// §36.6 safety bug) is caught rather than silently performed.
///
/// On POSIX most states collapse — a single `mmap` walks
/// `AuthorizedUntyped → … → AllocatorCommitted` and a single `munmap`/`madvise`
/// walks `Unmapped → Revoked → RecyclableUntyped` — but the *ordering invariants*
/// the machine enforces (unmap before revoke, revoke before recycle, §36.6) hold
/// on both backends, so the same checker guards both.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ProviderState {
    /// Untyped backing authorized to this allocator but not yet reserved (§36.6).
    AuthorizedUntyped = 0,
    /// Untyped reserved for a specific reservation.
    ReservedUntyped = 1,
    /// A frame capability minted from the reserved untyped.
    FrameCapMinted = 2,
    /// The frame mapped into the resource server's VSpace.
    MappedToServer = 3,
    /// The frame mapped into the client's VSpace (reachable by the client).
    MappedToClient = 4,
    /// Backing committed and in use by an allocation (a live object may exist).
    AllocatorCommitted = 5,
    /// Free but physically backed; may hold old data (§20.1 *dirty*).
    AllocatorDirty = 6,
    /// Free and lazily purged / scrubbed (§20.1 *muzzy*).
    AllocatorMuzzyOrScrubbed = 7,
    /// The client mapping has been removed (§36.6: unreachable to the client).
    Unmapped = 8,
    /// Descendant capabilities revoked (§36.6: must precede recycling).
    Revoked = 9,
    /// Untyped returned to the pool, reusable only via a fresh authorized
    /// transition (§36.6 / §36.7 `Released/Recycled`).
    RecyclableUntyped = 10,
}

impl ProviderState {
    /// Whether `self → to` is a legal transition of the §36.6 machine. The chain
    /// is mostly linear (`AuthorizedUntyped → … → AllocatorCommitted`), with the
    /// reuse cycles the §36.7 state mapping requires:
    ///
    /// * `AllocatorCommitted ↔ AllocatorDirty` — alloc dirties, reuse re-commits;
    /// * `AllocatorDirty → AllocatorMuzzyOrScrubbed` — purge;
    /// * `AllocatorMuzzyOrScrubbed → AllocatorCommitted` — **recommit before reuse
    ///   (M-005)**;
    /// * any committed/dirty/muzzy state `→ Unmapped → Revoked →
    ///   RecyclableUntyped` — the **unmap-before-revoke-before-recycle** ordering
    ///   §36.6 mandates so recycled untyped never retains a live client mapping;
    /// * `RecyclableUntyped → ReservedUntyped` — the recycled untyped re-enters
    ///   the pool for a new reservation, closing the loop.
    ///
    /// A self-transition is always legal (an idempotent re-assertion of state).
    pub const fn can_transition(self, to: ProviderState) -> bool {
        use ProviderState::*;
        if self as u8 == to as u8 {
            return true; // idempotent (re-asserting the current state)
        }
        match (self, to) {
            (AuthorizedUntyped, ReservedUntyped)
            | (ReservedUntyped, FrameCapMinted)
            | (FrameCapMinted, MappedToServer)
            | (MappedToServer, MappedToClient)
            | (MappedToClient, AllocatorCommitted)
            | (AllocatorCommitted, AllocatorDirty)
            | (AllocatorDirty, AllocatorCommitted)
            | (AllocatorDirty, AllocatorMuzzyOrScrubbed)
            | (AllocatorMuzzyOrScrubbed, AllocatorCommitted)
            | (Unmapped, Revoked)
            | (Revoked, RecyclableUntyped)
            | (RecyclableUntyped, ReservedUntyped) => true,
            // Any in-use/free state may be unmapped (the client mapping removed).
            (
                MappedToClient | AllocatorCommitted | AllocatorDirty | AllocatorMuzzyOrScrubbed,
                Unmapped,
            ) => true,
            _ => false,
        }
    }
}

/// A runtime tracker for one resource's [`ProviderState`] (§36.6, the "asserted at
/// runtime" half of the modeled-in-Lean machine). [`advance`](Self::advance)
/// refuses an illegal transition with [`BackendError::InvalidRequest`] *and*
/// debug-aborts under `debug-assertions`, so a provider bug that would, say,
/// recycle still-mapped untyped is both caught in test and failed-safe in release.
#[derive(Clone, Copy, Debug)]
pub struct ProviderStateMachine {
    state: ProviderState,
}

impl ProviderStateMachine {
    /// A machine starting at `AuthorizedUntyped` (untyped authorized to us).
    pub const fn new() -> Self {
        Self {
            state: ProviderState::AuthorizedUntyped,
        }
    }

    /// A machine starting in an explicit state (e.g. a resource already mapped).
    pub const fn starting_at(state: ProviderState) -> Self {
        Self { state }
    }

    /// The current state.
    #[inline]
    pub const fn state(&self) -> ProviderState {
        self.state
    }

    /// Advance to `to` if the transition is legal (§36.6). Returns
    /// [`BackendError::InvalidRequest`] and leaves the state unchanged otherwise,
    /// so a refused transition is well-formed (W4-5); under `debug-assertions` an
    /// illegal transition additionally aborts to surface the provider bug.
    ///
    /// SPEC-transition: provider state machine step (§36.6)
    #[inline]
    pub fn advance(&mut self, to: ProviderState) -> Result<(), BackendError> {
        debug_assert!(
            self.state.can_transition(to),
            "illegal provider-state transition (§36.6)"
        );
        if !self.state.can_transition(to) {
            return Err(BackendError::InvalidRequest);
        }
        self.state = to;
        Ok(())
    }
}

impl Default for ProviderStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// The backing-provider seam. POSIX is the degenerate single ambient-authority,
/// single-label case; seLe4n supplies the capability case behind the identical
/// interface (overview §3, D2). See the module docs for the §36.6 mapping.
///
/// Every operation is fallible and **must leave the provider's state well-formed
/// on failure** (§36.6); the physical-state operations are idempotent over a
/// range that is already in the target state, so a retried operation after a
/// partial failure is always safe.
pub trait TopoBackingProvider {
    /// Reserve a region of at least `size` bytes aligned to `align` (a power of
    /// two) for `arena`. The region is not necessarily usable until `commit`. On
    /// POSIX this is the collapse of §36.6 `reserve_window ∘ create_frame ∘
    /// map_frame` with `READ_WRITE` rights and a [`CachePolicy::Cached`] mapping.
    fn reserve(&self, arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError>;

    /// Make `[offset, offset+len)` within `region` usable (backed). Committing
    /// twice, or committing a sub-range, must be idempotent and safe — this is
    /// also the **recommit** §36.6/M-005 requires before a released page is reused.
    ///
    /// SPEC-transition: provider `AllocatorMuzzyOrScrubbed/Reserved -> AllocatorCommitted` (§36.6)
    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError>;

    /// Drop the physical backing of `[offset, offset+len)` but keep the virtual
    /// range reserved (§18.3 `extent_decommit`): reuse requires a `commit`
    /// (M-005). On POSIX this is `madvise(MADV_DONTNEED)` with the mapping kept;
    /// the default is a no-op (a provider that never reclaims, e.g. the M0
    /// host-backed skeleton, leaves the bytes committed).
    ///
    /// SPEC-transition: provider `Allocator* -> Unmapped` (decommit, §18.3/§36.6)
    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Mark `[offset, offset+len)` lazily discardable (§20.4 `purge_lazy`): the OS
    /// may reclaim the pages under pressure, and a later access either reuses the
    /// old contents cheaply or faults a fresh page. On Linux this is
    /// `madvise(MADV_FREE)`. Default no-op (dirty stays dirty).
    ///
    /// SPEC-transition: provider `AllocatorDirty -> AllocatorMuzzyOrScrubbed` (lazy purge, §20.4)
    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Discard the physical contents of `[offset, offset+len)` promptly (§20.4
    /// `purge_forced`): RSS drops now and a later access faults a fresh zero page.
    /// On Linux this is `madvise(MADV_DONTNEED)`. Default no-op.
    ///
    /// SPEC-transition: provider `Allocator* -> AllocatorMuzzyOrScrubbed` (forced purge, §20.4)
    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Return `region` to the backend (§18.3 `extent_release` / §36.6 unmap +
    /// revoke + recycle). After this the region must not be used. Failure must
    /// leave both allocator and backend state well-formed (§36.6).
    ///
    /// SPEC-transition: provider `Unmapped -> Revoked -> RecyclableUntyped` (§36.6)
    fn release(&self, arena: ArenaId, region: Region) -> Result<(), BackendError>;

    /// Revoke every capability derived from `region`'s backing before it can be
    /// returned to a pool that serves another authority domain (§36.6: "revoke
    /// MUST complete before memory is returned to a pool"). **A no-op on POSIX**
    /// (single ambient authority — there are no descendant capabilities to
    /// revoke); real capability revocation on seLe4n (plan 09).
    ///
    /// SPEC-transition: provider `Unmapped -> Revoked` (§36.6)
    fn revoke_descendants(&self, arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let _ = (arena, region);
        Ok(())
    }

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

    #[test]
    fn provider_state_chain_is_legal_and_skips_are_rejected() {
        use ProviderState::*;
        // The forward reservation chain is legal step by step.
        let chain = [
            AuthorizedUntyped,
            ReservedUntyped,
            FrameCapMinted,
            MappedToServer,
            MappedToClient,
            AllocatorCommitted,
        ];
        for w in chain.windows(2) {
            assert!(
                w[0].can_transition(w[1]),
                "{:?} -> {:?} must be legal",
                w[0],
                w[1]
            );
        }
        // Skipping a step is illegal (e.g. reserving straight to a client mapping).
        assert!(!ReservedUntyped.can_transition(MappedToClient));
        // Self-transitions are idempotent and always legal.
        assert!(AllocatorDirty.can_transition(AllocatorDirty));
    }

    #[test]
    fn recycle_requires_unmap_then_revoke_first() {
        use ProviderState::*;
        // §36.6: recycled untyped MUST NOT retain a live client mapping — so a
        // committed/mapped resource can never jump straight to RecyclableUntyped.
        assert!(!AllocatorCommitted.can_transition(RecyclableUntyped));
        assert!(!AllocatorCommitted.can_transition(Revoked));
        assert!(!Unmapped.can_transition(RecyclableUntyped)); // must Revoke first
                                                              // The legal tail: unmap, then revoke, then recycle.
        assert!(AllocatorCommitted.can_transition(Unmapped));
        assert!(Unmapped.can_transition(Revoked));
        assert!(Revoked.can_transition(RecyclableUntyped));
        // The recycled untyped re-enters the pool for a new reservation.
        assert!(RecyclableUntyped.can_transition(ReservedUntyped));
    }

    #[test]
    fn recommit_before_reuse_is_legal_purge_then_recommit() {
        use ProviderState::*;
        // M-005 path: dirty -> muzzy (purge) -> committed (recommit before reuse).
        assert!(AllocatorDirty.can_transition(AllocatorMuzzyOrScrubbed));
        assert!(AllocatorMuzzyOrScrubbed.can_transition(AllocatorCommitted));
        // alloc/free reuse cycle.
        assert!(AllocatorCommitted.can_transition(AllocatorDirty));
        assert!(AllocatorDirty.can_transition(AllocatorCommitted));
    }

    #[test]
    fn state_machine_advance_refuses_illegal_and_stays_well_formed() {
        let mut m = ProviderStateMachine::new();
        assert_eq!(m.state(), ProviderState::AuthorizedUntyped);
        m.advance(ProviderState::ReservedUntyped).expect("legal");
        assert_eq!(m.state(), ProviderState::ReservedUntyped);
        // An illegal jump is refused and the state is unchanged (W4-5 well-formed).
        // (Run only where the debug_assert is compiled out, so the structural
        // backstop — not the abort — is what we exercise.)
        #[cfg(not(debug_assertions))]
        {
            assert_eq!(
                m.advance(ProviderState::RecyclableUntyped),
                Err(BackendError::InvalidRequest)
            );
            assert_eq!(m.state(), ProviderState::ReservedUntyped);
        }
    }

    #[test]
    fn region_addr_range_is_base_and_end() {
        let mut buf = [0u8; 64];
        let r = Region {
            base: buf.as_mut_ptr(),
            len: 64,
        };
        let (lo, hi) = r.addr_range();
        assert_eq!(lo, buf.as_ptr() as usize);
        assert_eq!(hi - lo, 64);
    }
}

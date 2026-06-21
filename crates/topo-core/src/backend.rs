// SPDX-License-Identifier: MIT
//! The backing-provider seam (§3, §36.6). Everything OS/kernel goes through
//! `TopoBackingProvider`; the allocator core never calls `mmap` or `retype`.
//!
//! POSIX is the **degenerate single ambient-authority, single-label case** of the
//! §36.6 contract; seLe4n supplies the capability case behind the identical
//! interface (overview §3, D2). The seam is offered in **two layers**:
//!
//! * the **collapsed POSIX seam** the allocator core actually drives —
//!   [`reserve`](TopoBackingProvider::reserve) / [`commit`](TopoBackingProvider::commit)
//!   / [`decommit`](TopoBackingProvider::decommit) / [`purge_lazy`](TopoBackingProvider::purge_lazy)
//!   / [`purge_forced`](TopoBackingProvider::purge_forced) / [`release`](TopoBackingProvider::release)
//!   / [`revoke_descendants`](TopoBackingProvider::revoke_descendants); and
//! * the **full §36.6 typed surface** plan 09's capability provider overrides —
//!   [`reserve_window`](TopoBackingProvider::reserve_window) /
//!   [`create_frame`](TopoBackingProvider::create_frame) /
//!   [`map_frame`](TopoBackingProvider::map_frame) /
//!   [`unmap_frame`](TopoBackingProvider::unmap_frame) / [`recycle`](TopoBackingProvider::recycle),
//!   over the capability types [`VWindow`]/[`FrameCap`]/[`MappedRange`].
//!
//! On POSIX the capability types collapse to an address range and the granular
//! methods **default-compose the collapsed seam**, so a single `reserve` (an
//! `mmap`) realises `reserve_window ∘ create_frame ∘ map_frame`. Plan 09 overrides
//! the granular methods with real capability operations; the allocator core above
//! the seam never changes.
//!
//! | §36.6 concept | POSIX collapse |
//! |---|---|
//! | `VSpaceWindow` | [`VWindow`] (a reserved address range + rights + cache policy) |
//! | `FrameCap` | [`FrameCap`] (a backing token; on POSIX the window's own pages) |
//! | `MappedRange` | [`MappedRange`] (a mapped [`Region`]) |
//! | `Cap` (to revoke) | the [`Region`] passed to `revoke_descendants` (a no-op) |
//! | `AccessRights` / `PagePerms` | [`Rights`] (`mprotect` bits) |
//! | `cache_policy` | [`CachePolicy`] (mapping cacheability) |
//!
//! The [`ProviderState`] machine models the §36.6 backing lifecycle —
//! `AuthorizedUntyped → … → RecyclableUntyped`. It is the **exact linear chain** of
//! §36.6, mirroring the Lean model `BackingState.next`
//! (`lean/TopoMalloc/SeLe4n/UntypedProvider.lean`, W1-11b): one backing resource
//! traverses it once, cradle to grave. (Allocator-level *reuse* — alloc/free,
//! recommit-before-reuse, M-005 — lives one layer up, in the extent manager's
//! [`ExtentState`](crate::extent::ExtentState); it is **not** a back-edge of this
//! lifecycle.) The runtime [`ProviderStateMachine`] asserts every step is the legal
//! `next`, so the cardinal §36.6 bug — recycling untyped that still has a live
//! client mapping — is caught (it requires the illegal `… → RecyclableUntyped` jump
//! that skips unmap+revoke). [`ProviderState::next`] and the Rust↔Lean agreement are
//! pinned by `provider_next_matches_the_36_6_chain_exactly`.
//!
//! Every seam operation is **fallible and leaves both allocator and backend state
//! well-formed on failure** (§36.6: "Backing-provider failure MUST leave
//! TopoMalloc and seLe4n state well-formed"); the back-end's extent manager
//! ([`crate::extent`]) relies on that to keep its invariants green under
//! failure injection (plan 04 W4-5).

use crate::error::BackendError;
use crate::ids::{ArenaId, Label};

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

/// A reserved VSpace window (§36.8.1): a contiguous virtual range with a rights
/// mask and cache policy, into which provider frames are mapped. On POSIX it is
/// just the reserved address range; on seLe4n a real VSpace window with guard
/// pages, an owner arena, an information-flow label, and a mapping generation
/// (plan 09). It carries the [`Region`] so its backing provenance is preserved.
#[derive(Clone, Copy, Debug)]
pub struct VWindow {
    /// The reserved address range.
    pub region: Region,
    /// The rights the window was reserved with.
    pub rights: Rights,
    /// The window's cache policy.
    pub cache: CachePolicy,
}

// SAFETY: as [`Region`] — a `VWindow` is an address range plus plain rights/cache
// data, with no ownership of its own.
unsafe impl Send for VWindow {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for VWindow {}

/// A frame capability minted from authorized untyped memory (§36.6 `FrameCap`). On
/// POSIX there is no separate frame object — anonymous backing is supplied with the
/// window's own pages — so this is a *token* describing the backing to be mapped
/// (its size and information-flow label); on seLe4n it is a real frame capability
/// (plan 09).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameCap {
    /// The frame's size in bytes (`1 << size_bits`).
    pub size: usize,
    /// The information-flow label the frame belongs to (`PUBLIC` on POSIX).
    pub label: Label,
}

/// A frame mapped into a window (§36.6 `MappedRange`): a usable [`Region`] with the
/// rights it was mapped with. This is what the physical-state operations
/// ([`commit`](TopoBackingProvider::commit) …) act on; [`region`](Self::region) is
/// the [`Region`] to pass them.
#[derive(Clone, Copy, Debug)]
pub struct MappedRange {
    /// The mapped, usable address range.
    pub region: Region,
    /// The rights the range was mapped with.
    pub rights: Rights,
}

// SAFETY: as [`Region`].
unsafe impl Send for MappedRange {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for MappedRange {}

/// The §36.6 backing-provider state machine: the **exact linear lifecycle** a
/// backing resource walks from `AuthorizedUntyped` to `RecyclableUntyped`. The
/// states and their order mirror the Lean model `BackingState`
/// (`lean/TopoMalloc/SeLe4n/UntypedProvider.lean`, W1-11b) one-for-one; the
/// [`next`](Self::next) successor function mirrors Lean's `BackingState.next`, and
/// `provider_next_matches_the_36_6_chain_exactly` pins the two relations
/// equal so they cannot drift.
///
/// The chain is **acyclic and linear**: one resource traverses it once. The
/// allocator-level reuse (alloc/free, recommit-before-reuse M-005) is the extent
/// manager's [`ExtentState`](crate::extent::ExtentState) concern, *not* a back-edge
/// of this lifecycle — keeping the runtime machine identical to the proven model.
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
    /// transition (§36.6 / §36.7 `Released/Recycled`). The lifecycle terminates
    /// here (a new reservation starts a fresh resource at `AuthorizedUntyped`).
    RecyclableUntyped = 10,
}

impl ProviderState {
    /// The §36.6 successor of this state, or `None` at the terminal
    /// `RecyclableUntyped`. **Mirrors the Lean `BackingState.next` exactly** (W1-11b)
    /// — the single, linear §36.6 chain — so the runtime checker and the proof
    /// reason about the identical relation (pinned by
    /// `provider_next_matches_the_36_6_chain_exactly`).
    pub const fn next(self) -> Option<ProviderState> {
        use ProviderState::*;
        Some(match self {
            AuthorizedUntyped => ReservedUntyped,
            ReservedUntyped => FrameCapMinted,
            FrameCapMinted => MappedToServer,
            MappedToServer => MappedToClient,
            MappedToClient => AllocatorCommitted,
            AllocatorCommitted => AllocatorDirty,
            AllocatorDirty => AllocatorMuzzyOrScrubbed,
            AllocatorMuzzyOrScrubbed => Unmapped,
            Unmapped => Revoked,
            Revoked => RecyclableUntyped,
            RecyclableUntyped => return None,
        })
    }

    /// Whether `self → to` is the one legal §36.6 transition — i.e. `to` is exactly
    /// [`next`](Self::next). The chain is linear and acyclic, so an out-of-order
    /// jump (e.g. `AllocatorCommitted → RecyclableUntyped`, skipping unmap+revoke,
    /// the cardinal §36.6 safety bug) is rejected, as is a self-transition.
    pub const fn can_transition(self, to: ProviderState) -> bool {
        match self.next() {
            Some(n) => n as u8 == to as u8,
            None => false,
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

    /// Advance to `to` if it is the legal §36.6 successor (the linear chain).
    /// Returns [`BackendError::InvalidRequest`] and leaves the state unchanged
    /// otherwise, so a refused transition is well-formed (W4-5); under
    /// `debug-assertions` an illegal transition additionally aborts to surface the
    /// provider bug.
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
/// interface (overview §3, D2). See the module docs for the two layers (collapsed
/// POSIX seam vs full §36.6 typed surface) and the capability-type collapse.
///
/// Every operation is fallible and **must leave the provider's state well-formed
/// on failure** (§36.6); the physical-state operations are idempotent over a
/// range that is already in the target state, so a retried operation after a
/// partial failure is always safe.
pub trait TopoBackingProvider {
    // --- the collapsed POSIX seam (the allocator core drives these) ----------

    /// Reserve a region of at least `size` bytes aligned to `align` (a power of
    /// two) for `arena`. The region is not necessarily usable until `commit`. On
    /// POSIX this is the collapse of §36.6 `reserve_window ∘ create_frame ∘
    /// map_frame` with `READ_WRITE` rights and a [`CachePolicy::Cached`] mapping.
    fn reserve(&self, arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError>;

    /// Make `[offset, offset+len)` within `region` usable (backed). Committing
    /// twice, or committing a sub-range, must be idempotent and safe — this is
    /// also the **recommit** §36.6/M-005 requires before a released page is reused.
    ///
    /// SPEC-transition: provider commit / recommit (§18.3/§36.6)
    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError>;

    /// Drop the physical backing of `[offset, offset+len)` but keep the virtual
    /// range reserved (§18.3 `extent_decommit`): reuse requires a `commit`
    /// (M-005). On POSIX this is `madvise(MADV_DONTNEED)` with the mapping kept;
    /// the default is a no-op (a provider that never reclaims leaves the bytes
    /// committed).
    ///
    /// SPEC-transition: provider decommit (§18.3/§20.4)
    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Mark `[offset, offset+len)` lazily discardable (§20.4 `purge_lazy`): the OS
    /// may reclaim the pages under pressure, and a later access either reuses the
    /// old contents cheaply or faults a fresh page. On Linux this is
    /// `madvise(MADV_FREE)`. Default no-op (dirty stays dirty).
    ///
    /// SPEC-transition: provider lazy purge (§20.4)
    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Discard the physical contents of `[offset, offset+len)` promptly (§20.4
    /// `purge_forced`): RSS drops now and a later access faults a fresh zero page.
    /// On Linux this is `madvise(MADV_DONTNEED)`. Default no-op.
    ///
    /// SPEC-transition: provider forced purge (§20.4)
    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let _ = (region, offset, len);
        Ok(())
    }

    /// Make `[offset, offset+len)` **inaccessible** (`accessible == false` — a guard
    /// page that faults on any touch) or restore it read-write (`accessible == true`),
    /// for the W18-4 guarded allocations (§29.5). On Linux this is
    /// `mprotect(PROT_NONE)` / `mprotect(PROT_READ|WRITE)`.
    ///
    /// **Default: a no-op returning `Ok`** — a provider (or the seLe4n simulator)
    /// without page-level protection treats guards as *advisory*, so a guarded
    /// allocation still works (it just does not hardware-trap an overrun). The
    /// allocator never relies on a guard for *correctness* (§2.4); the trap is a
    /// best-effort debugging aid where the platform supports it. `offset`/`len` are
    /// OS-page-aligned sub-ranges of `region`.
    fn protect(
        &self,
        region: Region,
        offset: usize,
        len: usize,
        accessible: bool,
    ) -> Result<(), BackendError> {
        let _ = (region, offset, len, accessible);
        Ok(())
    }

    /// Return `region` to the backend (§18.3 `extent_release` / §36.6 unmap +
    /// revoke + recycle). After this the region must not be used. Failure must
    /// leave both allocator and backend state well-formed (§36.6).
    ///
    /// SPEC-transition: provider release (§36.6)
    fn release(&self, arena: ArenaId, region: Region) -> Result<(), BackendError>;

    /// Revoke every capability derived from `region`'s backing before it can be
    /// returned to a pool that serves another authority domain (§36.6: "revoke
    /// MUST complete before memory is returned to a pool"). **A no-op on POSIX**
    /// (single ambient authority — there are no descendant capabilities to
    /// revoke; `Cap` collapses to the range); real capability revocation on
    /// seLe4n (plan 09).
    ///
    /// SPEC-transition: provider `Unmapped -> Revoked` (§36.6)
    fn revoke_descendants(&self, arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let _ = (arena, region);
        Ok(())
    }

    /// **Bind `region`'s physical backing to NUMA node `os_node`** (§15.5, W13) — the
    /// kernel's node number (from [`Topology::os_node_of`](crate::Topology::os_node_of)),
    /// not a dense id. Best-effort: applied to the *whole reservation* before it is
    /// faulted (e.g. Linux `mbind`/`set_mempolicy`), so future faults land on `os_node`.
    /// **A no-op by default** — POSIX without NUMA, seLe4n, and non-Linux targets return
    /// `Ok(())`, and the live [`NodeRouter`](crate::NodeRouter) treats any `Err` as a
    /// recorded [bind failure](crate::Topology) (counted in stats), never fatal: a missed
    /// bind only loses locality (§2.4 "safety before policy").
    fn bind_node(&self, region: Region, os_node: u32) -> Result<(), BackendError> {
        let _ = (region, os_node);
        Ok(())
    }

    /// A short, stable name for diagnostics and trace output (e.g. `"posix"`).
    fn name(&self) -> &'static str;

    /// Whether memory **freshly committed from an unbacked extent** (Reserved or
    /// Released) is guaranteed to read as all-zero (§26.2 "OS-provided zero pages for
    /// newly committed memory"). When `true`, `calloc` may **skip the redundant
    /// `memset`** for a large/medium allocation served by a just-committed extent —
    /// the W15-5 zeroed-state optimization (§26.3). Defaults to `false` (conservative:
    /// `calloc` always zeroes), so a backend opts in only when it can promise it.
    ///
    /// POSIX promises it: the region is `mmap(MAP_ANONYMOUS)` (zero-filled), and
    /// `decommit` is `MADV_DONTNEED`/`MADV_FREE` so a recommitted page zero-faults.
    /// A custom backing ([`HookProvider`](crate::HookProvider)) does **not** opt in —
    /// the user's allocator hook makes no zeroing promise — so `calloc` keeps zeroing
    /// it. This is the trust boundary the §26.2 guarantee rests on; the allocator
    /// debug-asserts the bytes really are zero whenever it skips the `memset`.
    fn committed_memory_is_zeroed(&self) -> bool {
        false
    }

    // --- the full §36.6 typed surface (plan 09 overrides these) --------------
    //
    // Default implementations express the POSIX collapse by composing the methods
    // above, so a provider that only implements `reserve`/`commit`/`decommit`/
    // `release` gets the whole §36.6 surface for free. A capability provider
    // overrides them with real `reserve_window`/`create_frame`/`map_frame`/… calls.

    /// Reserve a VSpace window of `size` bytes aligned to `align` with `rights`
    /// (§36.6 `reserve_window`). Default: a [`reserve`](Self::reserve) wrapped as a
    /// [`VWindow`] (rights/cache recorded; honoured by the provider's own
    /// `mprotect`/cache attributes).
    ///
    /// SPEC-transition: provider `AuthorizedUntyped -> ReservedUntyped` (§36.6)
    fn reserve_window(
        &self,
        arena: ArenaId,
        size: usize,
        align: usize,
        rights: Rights,
    ) -> Result<VWindow, BackendError> {
        let region = self.reserve(arena, size, align)?;
        Ok(VWindow {
            region,
            rights,
            cache: CachePolicy::Cached,
        })
    }

    /// Mint a frame capability of `1 << size_bits` bytes for `label` (§36.6
    /// `create_frame`). Default (POSIX): a [`FrameCap`] token — anonymous backing is
    /// supplied with the window's own pages, so no separate object is created here.
    /// `size_bits` is the log2 frame size (capped to `usize::BITS - 1` to stay
    /// shift-safe).
    ///
    /// SPEC-transition: provider `ReservedUntyped -> FrameCapMinted` (§36.6)
    fn create_frame(
        &self,
        arena: ArenaId,
        size_bits: u32,
        label: Label,
    ) -> Result<FrameCap, BackendError> {
        let _ = arena;
        let bits = size_bits.min(usize::BITS - 1);
        Ok(FrameCap {
            size: 1usize << bits,
            label,
        })
    }

    /// Map `frame` into `window` with `rights`/`cache`, returning the usable
    /// [`MappedRange`] (§36.6 `map_frame`). Default (POSIX): the window's own pages
    /// back it, so "mapping" commits the window and hands the range back. The frame
    /// must fill the window **exactly** — the collapsed seam maps a window 1:1 to a
    /// frame (a partial map is rejected; see the body). Plan 09 overrides this to map
    /// sub-window frames against real frame capabilities.
    ///
    /// SPEC-transition: provider `FrameCapMinted -> MappedToServer -> MappedToClient -> AllocatorCommitted` (§36.6)
    fn map_frame(
        &self,
        arena: ArenaId,
        frame: FrameCap,
        window: VWindow,
        rights: Rights,
        cache: CachePolicy,
    ) -> Result<MappedRange, BackendError> {
        let _ = (arena, cache);
        // The collapsed POSIX seam backs a window with a single mapping, and
        // `recycle` → `release` reclaims the whole reservation (matched by base). So a
        // frame must fill its window exactly: a partial map would make the returned
        // `MappedRange` describe fewer bytes than `recycle` actually unmaps and leave
        // the remainder untracked. Reject it rather than silently truncating.
        if frame.size != window.region.len {
            return Err(BackendError::InvalidRequest);
        }
        let region = window.region;
        self.commit(region, 0, region.len)?;
        Ok(MappedRange { region, rights })
    }

    /// Make `mapped` unreachable to the client and return its backing as a
    /// [`FrameCap`] (§36.6 `unmap_frame`: "MUST make the range unreachable before
    /// revocation or label transfer"). Default (POSIX): [`decommit`](Self::decommit)
    /// the range; the virtual window stays reserved.
    ///
    /// SPEC-transition: provider `Allocator* -> Unmapped` (§36.6)
    fn unmap_frame(&self, arena: ArenaId, mapped: MappedRange) -> Result<FrameCap, BackendError> {
        let _ = arena;
        self.decommit(mapped.region, 0, mapped.region.len)?;
        Ok(FrameCap {
            size: mapped.region.len,
            label: Label::PUBLIC,
        })
    }

    /// Recycle a `mapped` range's backing to the provider pool (§36.6 `recycle`):
    /// its descendants must already be revoked (`unmap_frame` then
    /// `revoke_descendants`). Default (POSIX): [`release`](Self::release) the range.
    ///
    /// SPEC-transition: provider `Revoked -> RecyclableUntyped` (§36.6)
    fn recycle(&self, arena: ArenaId, mapped: MappedRange) -> Result<(), BackendError> {
        self.release(arena, mapped.region)
    }

    // --- §23.2 extent-hook split/merge notifications (plan 06 W10) -----------
    //
    // These are the two §23.2 hook operations with no §20.4 physical-state
    // analogue: they tell a *custom backing* (one supplied through these very
    // methods, e.g. a `HookProvider`) that the allocator's extent manager has
    // subdivided or coalesced an extent within a reserved region, so the backing
    // can keep its own notion of extent boundaries (§23.1) in step.
    //
    // They are **advisory** (§23.4 "allocator correctness assumes hook
    // correctness"): the extent manager's `ExtentMap` is the source of truth for
    // sub-extent geometry (it splits/merges the region it `reserve`d without
    // otherwise involving the provider, which sees only offset ranges, never
    // extent objects), so a failure here is *reported* (§23.3) but never alters
    // the bookkeeping — the back-end stays well-formed regardless (W10-3). A
    // **no-op default is therefore correct** for POSIX and seLe4n, whose backings
    // do not track sub-extent boundaries (a `reserve` is one indivisible
    // mapping / untyped run).

    /// Notify the backing that the extent occupying `region` (length
    /// `size_a + size_b`) was split into a prefix of `size_a` bytes and a suffix
    /// of `size_b` bytes at offset `size_a` (§23.2 `split`). `committed` is whether
    /// the extent was fully physically backed (so both halves are). Advisory — see
    /// the section note; the default is a no-op.
    ///
    /// SPEC-transition: extent split notification (§23.2 / §18.4)
    fn split(
        &self,
        arena: ArenaId,
        region: Region,
        size_a: usize,
        size_b: usize,
        committed: bool,
    ) -> Result<(), BackendError> {
        let _ = (arena, region, size_a, size_b, committed);
        Ok(())
    }

    /// Notify the backing that the address-adjacent extents `left` and `right`
    /// (with `left.end() == right.base`) were merged into one covering both
    /// (§23.2 `merge`). `committed` is whether both were fully backed (the merge
    /// only coalesces backing-compatible neighbours, §18.4). Advisory — see the
    /// section note; the default is a no-op.
    ///
    /// SPEC-transition: extent merge notification (§23.2 / §18.4)
    fn merge(
        &self,
        arena: ArenaId,
        left: Region,
        right: Region,
        committed: bool,
    ) -> Result<(), BackendError> {
        let _ = (arena, left, right, committed);
        Ok(())
    }
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

    /// Every `ProviderState`, in §36.6 order. Kept here so the differential below
    /// fails loudly if a state is ever added/removed/reordered.
    const CHAIN: [ProviderState; 11] = {
        use ProviderState::*;
        [
            AuthorizedUntyped,
            ReservedUntyped,
            FrameCapMinted,
            MappedToServer,
            MappedToClient,
            AllocatorCommitted,
            AllocatorDirty,
            AllocatorMuzzyOrScrubbed,
            Unmapped,
            Revoked,
            RecyclableUntyped,
        ]
    };

    #[test]
    fn provider_next_matches_the_36_6_chain_exactly() {
        // The Rust↔Lean differential (A1): `ProviderState::next` must equal the
        // single linear §36.6 chain `CHAIN[i] -> CHAIN[i+1]`, terminating at the
        // last state — the *exact* relation `BackingState.next` defines in
        // `lean/TopoMalloc/SeLe4n/UntypedProvider.lean` (W1-11b). Enumerate every
        // ordered pair and assert `can_transition` is true iff it is that one edge,
        // so the runtime checker and the proof cannot drift.
        for (i, &s) in CHAIN.iter().enumerate() {
            let expected_next = CHAIN.get(i + 1).copied();
            assert_eq!(
                s.next(),
                expected_next,
                "{s:?}: next() must follow the §36.6 chain"
            );
            for &t in &CHAIN {
                let legal = s.can_transition(t);
                let should = expected_next == Some(t);
                assert_eq!(legal, should, "transition {s:?} -> {t:?} legality drifted");
            }
        }
        // The chain is acyclic and terminates exactly at RecyclableUntyped.
        assert_eq!(ProviderState::RecyclableUntyped.next(), None);
        // Self-transitions are not steps (Lean `Step s s' := next s = some s'`).
        for &s in &CHAIN {
            assert!(
                !s.can_transition(s),
                "{s:?}: self-transition is not a §36.6 step"
            );
        }
    }

    #[test]
    fn recycle_requires_unmap_then_revoke_first() {
        use ProviderState::*;
        // §36.6: recycled untyped MUST NOT retain a live client mapping — so a
        // committed/mapped resource can never jump toward RecyclableUntyped without
        // first walking dirty -> muzzy -> unmapped -> revoked.
        assert!(!AllocatorCommitted.can_transition(RecyclableUntyped));
        assert!(!AllocatorCommitted.can_transition(Unmapped)); // must dirty/muzzy first
        assert!(!Unmapped.can_transition(RecyclableUntyped)); // must Revoke first
                                                              // The legal tail, one §36.6 step at a time.
        assert!(AllocatorMuzzyOrScrubbed.can_transition(Unmapped));
        assert!(Unmapped.can_transition(Revoked));
        assert!(Revoked.can_transition(RecyclableUntyped));
    }

    #[test]
    fn state_machine_advance_walks_the_chain_and_refuses_skips() {
        // Walking the full §36.6 lifecycle step by step succeeds.
        let mut m = ProviderStateMachine::new();
        for &s in CHAIN.iter().skip(1) {
            m.advance(s).expect("each §36.6 next step is legal");
            assert_eq!(m.state(), s);
        }
        assert_eq!(m.state(), ProviderState::RecyclableUntyped);

        // A skip is refused and leaves the state unchanged (W4-5 well-formed). Run
        // only where the debug_assert is compiled out, so the structural backstop —
        // not the abort — is what we exercise.
        #[cfg(not(debug_assertions))]
        {
            let mut m = ProviderStateMachine::starting_at(ProviderState::ReservedUntyped);
            assert_eq!(
                m.advance(ProviderState::RecyclableUntyped),
                Err(BackendError::InvalidRequest)
            );
            assert_eq!(m.state(), ProviderState::ReservedUntyped);
        }
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "illegal provider-state transition")]
    fn advance_debug_aborts_on_illegal_skip() {
        // Companion to the release-only structural check above. Under
        // `debug-assertions` an illegal §36.6 transition must ABORT (surfacing the
        // provider bug), not silently return `Err`. CI runs tests in debug, so this
        // is the profile that actually exercises `advance`'s rejection — and
        // `starting_at`, which the release-only block above cannot cover here.
        let mut m = ProviderStateMachine::starting_at(ProviderState::ReservedUntyped);
        let _ = m.advance(ProviderState::RecyclableUntyped); // skips the chain → abort
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

    /// A minimal provider that implements only the required methods, to prove the
    /// §23.2 `split`/`merge` notifications default to a well-formed no-op (W10) —
    /// the property POSIX/seLe4n rely on (their backings do not track sub-extent
    /// boundaries, so they never override these).
    struct BareProvider;
    impl TopoBackingProvider for BareProvider {
        fn reserve(&self, _: ArenaId, _: usize, _: usize) -> Result<Region, BackendError> {
            Err(BackendError::Unsupported)
        }
        fn commit(&self, _: Region, _: usize, _: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn release(&self, _: ArenaId, _: Region) -> Result<(), BackendError> {
            Ok(())
        }
        fn name(&self) -> &'static str {
            "bare"
        }
    }

    #[test]
    fn split_merge_notifications_default_to_a_noop() {
        // W10 / §23.2: a backing that does not track sub-extent boundaries (POSIX,
        // seLe4n) inherits a no-op `split`/`merge`, so the allocator can subdivide
        // its reserved region freely without the provider having to care.
        let p = BareProvider;
        let mut buf = [0u8; 64];
        let region = Region {
            base: buf.as_mut_ptr(),
            len: 64,
        };
        let left = Region {
            base: buf.as_mut_ptr(),
            len: 32,
        };
        // SAFETY: `buf` is 64 bytes, so offset 32 is in bounds — the base of the
        // right half (a notification carries only the address, never a deref).
        let right_base = unsafe { buf.as_mut_ptr().add(32) };
        let right = Region {
            base: right_base,
            len: 32,
        };
        assert!(p.split(ArenaId::DEFAULT, region, 32, 32, true).is_ok());
        assert!(p.merge(ArenaId::DEFAULT, left, right, true).is_ok());
    }
}

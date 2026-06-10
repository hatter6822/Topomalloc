// SPDX-License-Identifier: MIT
//! Arena policy & authority domains (plan 06 W9; SPEC §22, §36.4, §36.13, §15.5).
//!
//! Per D2, an arena is *both* a jemalloc-style policy domain (§22) *and* a
//! capability-controlled resource domain (§36.4) — trivial/ambient on POSIX,
//! real on seLe4n, behind the identical types. This module supplies:
//!
//! * [`ArenaState`] — the §22.3/§36.13 lifecycle state machine, **pinned 1:1 to
//!   the Lean model** (`lean/TopoMalloc/ArenaLifecycle.lean`) by
//!   `arena_lifecycle_matches_lean` here and the `arenaLifecycleGate` in
//!   `Check.lean`, exactly as `ProviderState`/`ExtentState` are pinned;
//! * [`CapRights`] — the six §36.4 capability rights, mirroring the Lean
//!   `CapRights` (`SeLe4n/CapBackedArena.lean`) with its attenuation preorder;
//! * [`ArenaQuota`] — exact byte-quota accounting (the runtime form of the Lean
//!   `ArenaQuotaExact`: `used` equals live usable bytes), with delegation
//!   reservations conserved so a delegation tree can never jointly exceed the
//!   root's quota;
//! * [`NumaPolicy`] — the five §15.5 modes, with binding failures surfaced in
//!   stats (W9-7);
//! * [`ArenaSet`] — the registry of per-arena engines: creation (§22.4),
//!   configure, reset (§22.5), destroy/revocation (§22.6/§36.13), and
//!   attenuation-only delegation (§36.4), routing every allocation/free to the
//!   owning arena's engine.
//!
//! # Isolation by construction (§22.7)
//!
//! Each arena owns a complete engine ([`Allocator`]): its own central free
//! lists, span pool, and two backing regions reserved from its own provider
//! instances. No extent, span, or central-list entry is ever shared between
//! arenas — the §22.7 isolation invariant holds structurally, and
//! [`ArenaSet::check_invariants`] additionally verifies the regions of distinct
//! arenas are pairwise disjoint (the executable B.5 oracle). The pagemap and
//! the metadata arena are shared, as the SPEC permits: pagemap entries carry
//! their owning descriptor (which knows its arena), and metadata comes from a
//! *safe metadata arena* (§22.4) precisely so destroying an arena never frees
//! the memory used to track the destruction.
//!
//! # The lifecycle gate (allocations only in `Active`, §22.3)
//!
//! Every operation on a non-default arena enters through a rendezvous gate:
//! increment the slot's `in_flight` count, then check `state == Active` and the
//! slot still holds the requested id — all `SeqCst`. Teardown flips the state
//! (also `SeqCst`) and then waits for `in_flight == 0`. By the standard
//! Dekker/store-buffering argument over the single `SeqCst` total order: an
//! operation that observed `Active` incremented `in_flight` *before* the flip
//! in that order, so the teardown's wait observes the increment and blocks
//! until the operation exits — the engine is never torn down under a live
//! operation. An operation that observes the flip backs out without touching
//! the engine. The **default arena skips the gate entirely**: it is created
//! with the set and is never resettable nor destroyable (§22.5 forbids
//! resetting the default arena; F-006 keeps it always available), so its
//! engine's liveness needs no rendezvous — the plain `malloc` path stays as
//! fast as the engine itself.
//!
//! # Arena ids are never reused
//!
//! Public [`ArenaId`]s are allocated monotonically and retired forever on
//! destroy — the strongest reading of B.5 "destroyed arena IDs are not reused
//! while stale references can exist" (a stale id can *never* alias a new
//! arena). Slots are recycled behind the monotone id plus a bumped slot
//! generation (W9-6e), exactly the span-descriptor recycling discipline
//! (§16.6/§27.5) one level up.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::allocator::{Allocator, AllocatorConfig, AllocatorStats, FreeOutcome};
use crate::backend::TopoBackingProvider;
use crate::bootstrap::MetadataAlloc;
use crate::error::BackendError;
use crate::extent::{BackendLock, ExtentError};
use crate::flags::RequestFlags;
use crate::ids::{ArenaId, Label, NodeId};
use crate::pagemap::PageMap;
use crate::ptr_class::{classify_ptr, MetadataRegion, PointerClass};

// ---------------------------------------------------------------------------
// ArenaState — the §22.3/§36.13 lifecycle, pinned to the Lean model
// ---------------------------------------------------------------------------

/// The §22.3 arena lifecycle states plus §36.13's `ERROR_QUARANTINED`. The
/// states, the legal transitions ([`can_transition`](Self::can_transition)),
/// and the gating predicates mirror the Lean `ArenaState`
/// (`lean/TopoMalloc/ArenaLifecycle.lean`) **exactly**; the
/// `arena_lifecycle_matches_lean` test and the `arenaLifecycleGate` in
/// `Check.lean` pin the two relations equal so they cannot drift (the §22.3
/// analogue of the §36.6/§20.1 differentials).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ArenaState {
    /// Descriptor under construction; the id is not yet published (§22.4).
    Initializing = 0,
    /// Serving allocations — the only state that admits them (§22.3).
    Active = 1,
    /// A destroy/revocation in progress (§36.13): new allocations and
    /// delegations rejected, caches draining, backing being unmapped/revoked.
    Draining = 2,
    /// A reset in progress (§22.5): outstanding allocations being discarded.
    Resetting = 3,
    /// Terminal: reset + metadata removal complete (§22.6); the id is never
    /// reused (B.5).
    Destroyed = 4,
    /// A reset/destroy step failed partway (§36.13): quarantined — never
    /// reported `Destroyed` — until a destroy retry completes.
    ErrorQuarantined = 5,
}

impl ArenaState {
    /// Every state, in the Lean `ArenaState.all` order (for exhaustive tests).
    pub const ALL: [ArenaState; 6] = [
        ArenaState::Initializing,
        ArenaState::Active,
        ArenaState::Draining,
        ArenaState::Resetting,
        ArenaState::Destroyed,
        ArenaState::ErrorQuarantined,
    ];

    /// Decode from the slot's atomic byte. Total: an out-of-range value (which
    /// would take a wild write — the field is written only through this
    /// module) decodes to the safest state, `ErrorQuarantined` (rejects every
    /// operation except a destroy retry).
    #[inline]
    pub(crate) const fn from_u8(v: u8) -> ArenaState {
        match v {
            0 => ArenaState::Initializing,
            1 => ArenaState::Active,
            2 => ArenaState::Draining,
            3 => ArenaState::Resetting,
            4 => ArenaState::Destroyed,
            _ => ArenaState::ErrorQuarantined,
        }
    }

    /// Whether new allocations are admitted (§22.3: only `Active`) — the W9-2
    /// gate, mirroring the Lean `allowsAlloc`.
    #[inline]
    pub const fn allows_alloc(self) -> bool {
        matches!(self, ArenaState::Active)
    }

    /// Whether new delegations are admitted (§36.4/§36.13, W9-6a: a draining
    /// arena rejects new delegations) — mirrors the Lean `allowsDelegation`.
    #[inline]
    pub const fn allows_delegation(self) -> bool {
        matches!(self, ArenaState::Active)
    }

    /// The stable string used in stats/control output (plan 07).
    pub const fn as_str(self) -> &'static str {
        match self {
            ArenaState::Initializing => "initializing",
            ArenaState::Active => "active",
            ArenaState::Draining => "draining",
            ArenaState::Resetting => "resetting",
            ArenaState::Destroyed => "destroyed",
            ArenaState::ErrorQuarantined => "error_quarantined",
        }
    }

    /// Whether `self → to` is a legal lifecycle transition — **the exact edge
    /// set of the Lean `ArenaState.canTransition`** (no self-transitions:
    /// every lifecycle operation moves the state):
    ///
    /// * publish (§22.4): `Initializing → Active`;
    /// * reset (§22.5): `Active → Resetting → Active | ErrorQuarantined`;
    /// * destroy (§36.13/W9-6): `Active → Draining → Destroyed | ErrorQuarantined`;
    /// * destroy retry (W9-6e): `ErrorQuarantined → Draining` — the *only*
    ///   exit from quarantine, so a partial failure can never be reported
    ///   `Destroyed` without completing the full protocol.
    pub const fn can_transition(self, to: ArenaState) -> bool {
        use ArenaState::*;
        matches!(
            (self, to),
            (Initializing, Active)
                | (Active, Resetting)
                | (Resetting, Active)
                | (Resetting, ErrorQuarantined)
                | (Active, Draining)
                | (Draining, Destroyed)
                | (Draining, ErrorQuarantined)
                | (ErrorQuarantined, Draining)
        )
    }
}

// ---------------------------------------------------------------------------
// CapRights — the §36.4 authority bits, pinned to the Lean model
// ---------------------------------------------------------------------------

/// The rights an arena capability grants (§36.4): allocate, free, observe
/// stats, purge/tune, destroy/reset, delegate further. Mirrors the Lean
/// `CapRights` (`SeLe4n/CapBackedArena.lean`) six-for-six — pinned by
/// `cap_rights_match_lean_model`. Quota *enlargement* is deliberately not a
/// right: quotas are fixed at creation/delegation and only attenuate, so the
/// §36.4 "no quota enlargement" rule holds by construction.
///
/// On POSIX the default arena carries [`ALL`](Self::ALL) (ambient authority —
/// the degenerate single-authority case, D2); per-caller capability handles
/// arrive with the seLe4n IPC surface (plan 09). Until then the arena's own
/// rights word *is* the capability this layer enforces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapRights(u8);

impl CapRights {
    /// Allocate from the arena.
    pub const ALLOC: CapRights = CapRights(1 << 0);
    /// Free back into the arena.
    pub const FREE: CapRights = CapRights(1 << 1);
    /// Observe the arena's statistics (§36.4 `TopoStatsCap`).
    pub const STATS: CapRights = CapRights(1 << 2);
    /// Purge/decay/policy tuning (§36.4 `TopoControlCap`-class authority).
    pub const PURGE: CapRights = CapRights(1 << 3);
    /// Reset or destroy the arena's lifecycle (§22.5/§22.6).
    pub const DESTROY: CapRights = CapRights(1 << 4);
    /// Delegate an attenuated child arena (§36.4).
    pub const DELEGATE: CapRights = CapRights(1 << 5);

    /// No rights.
    pub const NONE: CapRights = CapRights(0);
    /// Every right — the ambient authority of the default arena on POSIX.
    pub const ALL: CapRights = CapRights(0b11_1111);

    /// Reconstruct from the slot's atomic byte (reserved bits masked off, so a
    /// value is always within the six defined rights).
    #[inline]
    pub(crate) const fn from_bits(bits: u8) -> CapRights {
        CapRights(bits & Self::ALL.0)
    }

    /// The raw bits (for the slot's atomic).
    #[inline]
    pub(crate) const fn bits(self) -> u8 {
        self.0
    }

    /// The union of two rights sets.
    #[inline]
    #[must_use]
    pub const fn union(self, other: CapRights) -> CapRights {
        CapRights(self.0 | other.0)
    }

    /// Whether `self` grants every right in `needed`.
    #[inline]
    pub const fn contains(self, needed: CapRights) -> bool {
        self.0 & needed.0 == needed.0
    }

    /// The §36.4 attenuation preorder — the Lean `CapRights.le`: every right
    /// `self` grants, `parent` grants too (`self` is a sound attenuation of
    /// `parent`). Reflexive and transitive, as the Lean `le_refl`/`le_trans`
    /// prove; delegation requires `child.is_attenuation_of(parent)`.
    #[inline]
    pub const fn is_attenuation_of(self, parent: CapRights) -> bool {
        self.0 & !parent.0 == 0
    }
}

// ---------------------------------------------------------------------------
// ArenaQuota — exact byte accounting (§36.4 / §36.17 ArenaQuotaExact)
// ---------------------------------------------------------------------------

/// An arena's byte quota (§36.4 `quota_bytes`), with **exact** live accounting
/// and conserved delegation reservations.
///
/// Three counters, one invariant chain:
///
/// * `used` — live usable bytes, charged/uncharged by the engine at the exact
///   alloc/free success points (the coupled `allocStep`/`freeStep`,
///   `SeLe4n/Refinement.lean`). At quiescence `used == live_bytes` — the
///   runtime `ArenaQuotaExact`.
/// * `delegated_out` — quota reserved for live child arenas (§36.4 quota
///   monotonicity against the *remaining* quota). Kept **separate** from
///   `used` so exactness is preserved while the whole delegation tree is still
///   conserved: by induction over the tree, the sum of every descendant's
///   `used` can never exceed the root's `limit`.
/// * `committed` — the CAS-gated admission total. Every charge/reservation
///   raises `committed` first (checked against `limit` in one CAS) and its
///   own counter second; every release lowers its own counter first and
///   `committed` second. `committed ≥ used + delegated_out` therefore holds
///   at **every instant** under any interleaving, and `committed ≤ limit` is
///   CAS-enforced — so `used + delegated_out ≤ limit` always, with no lock on
///   the hot path.
///
/// All counter updates are `Relaxed`: the CAS provides the only invariant that
/// must hold under contention (`committed ≤ limit` on a single location);
/// cross-counter identities are quiescent-state properties, like every other
/// stats identity (§31.1).
#[derive(Debug)]
pub struct ArenaQuota {
    /// The quota limit in bytes (`u64::MAX` = effectively unlimited). Written
    /// only under the registry lock between occupants; immutable while the
    /// arena is published.
    limit: AtomicU64,
    /// The admission gate: `used + delegated_out ≤ committed ≤ limit`.
    committed: AtomicU64,
    /// Live usable bytes (exact, engine-maintained).
    used: AtomicU64,
    /// Bytes reserved for live delegated children.
    delegated_out: AtomicU64,
}

impl ArenaQuota {
    /// A quota cell with `limit` bytes and nothing charged.
    pub const fn new(limit: u64) -> ArenaQuota {
        ArenaQuota {
            limit: AtomicU64::new(limit),
            committed: AtomicU64::new(0),
            used: AtomicU64::new(0),
            delegated_out: AtomicU64::new(0),
        }
    }

    /// The quota limit in bytes.
    #[inline]
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Live usable bytes currently charged (exact at quiescence).
    #[inline]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Bytes reserved for live delegated children.
    #[inline]
    pub fn delegated_out(&self) -> u64 {
        self.delegated_out.load(Ordering::Relaxed)
    }

    /// Raise `committed` by `n` iff the result stays within `limit` (one CAS;
    /// also refuses on `u64` overflow — fail-safe). The admission core shared
    /// by [`try_charge`](Self::try_charge) and
    /// [`try_reserve_delegation`](Self::try_reserve_delegation).
    #[inline]
    fn try_commit(&self, n: u64) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);
        let mut cur = self.committed.load(Ordering::Relaxed);
        loop {
            let next = match cur.checked_add(n) {
                Some(v) if v <= limit => v,
                _ => return false, // over quota (or arithmetic overflow)
            };
            match self.committed.compare_exchange_weak(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Charge `n` live bytes (the coupled allocStep's guard, §36.17
    /// `arena_cap_authorizes_alloc`/`arena_quota_preserved`): `false` — and
    /// nothing changes — if the charge would exceed the limit.
    #[inline]
    pub fn try_charge(&self, n: u64) -> bool {
        if !self.try_commit(n) {
            return false;
        }
        self.used.fetch_add(n, Ordering::Relaxed);
        true
    }

    /// Return `n` live bytes (the coupled freeStep). Exactly pairs a previous
    /// [`try_charge`](Self::try_charge) — the engine charges and uncharges the
    /// identical usable size, so underflow is impossible in a correct build;
    /// the debug asserts make a pairing bug loud, and in release a wrapped
    /// `committed` only ever *refuses* future charges (fail-safe direction).
    #[inline]
    pub fn uncharge(&self, n: u64) {
        // `used` first, `committed` second: `committed ≥ used + delegated_out`
        // holds at every instant (see the type docs).
        let prev_used = self.used.fetch_sub(n, Ordering::Relaxed);
        debug_assert!(prev_used >= n, "quota uncharge exceeds charge");
        let prev_committed = self.committed.fetch_sub(n, Ordering::Relaxed);
        debug_assert!(prev_committed >= n, "quota committed underflow");
    }

    /// Reserve `q` bytes for a delegated child (§36.4 quota monotonicity: the
    /// child's whole limit is reserved against this arena's *remaining*
    /// quota). `false` — and nothing changes — if the reservation would exceed
    /// the limit.
    #[inline]
    pub fn try_reserve_delegation(&self, q: u64) -> bool {
        if !self.try_commit(q) {
            return false;
        }
        self.delegated_out.fetch_add(q, Ordering::Relaxed);
        true
    }

    /// Return a destroyed child's reservation of `q` bytes.
    #[inline]
    pub fn release_delegation(&self, q: u64) {
        let prev = self.delegated_out.fetch_sub(q, Ordering::Relaxed);
        debug_assert!(prev >= q, "delegation release exceeds reservation");
        let prev_committed = self.committed.fetch_sub(q, Ordering::Relaxed);
        debug_assert!(prev_committed >= q, "quota committed underflow");
    }

    /// Zero `used` wholesale — the arena-reset postcondition (`arenaReset`
    /// relabels every slot non-live ⇒ `arenaLiveBytes = 0` ⇒ exactness demands
    /// `used = 0`). Returns the bytes that were charged. Delegation
    /// reservations are untouched (children are unaffected by a parent reset).
    pub fn reset_used(&self) -> u64 {
        let old = self.used.swap(0, Ordering::Relaxed);
        let prev_committed = self.committed.fetch_sub(old, Ordering::Relaxed);
        debug_assert!(prev_committed >= old, "quota committed underflow on reset");
        old
    }

    /// Re-arm the cell for a new occupant (registry lock held; no operations
    /// can race — the slot is unpublished).
    fn reconfigure(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
        self.committed.store(0, Ordering::Relaxed);
        self.used.store(0, Ordering::Relaxed);
        self.delegated_out.store(0, Ordering::Relaxed);
    }

    /// The quiescent-state invariant chain (the B.5 quota oracle):
    /// `used + delegated_out == committed ≤ limit`.
    pub fn invariants_hold(&self) -> bool {
        let used = self.used.load(Ordering::Relaxed);
        let delegated = self.delegated_out.load(Ordering::Relaxed);
        let committed = self.committed.load(Ordering::Relaxed);
        let limit = self.limit.load(Ordering::Relaxed);
        used.checked_add(delegated) == Some(committed) && committed <= limit
    }
}

// ---------------------------------------------------------------------------
// NumaPolicy — §15.5 (W9-7)
// ---------------------------------------------------------------------------

/// The §15.5 NUMA placement modes. All five are representable from M1 (the
/// type is the contract); placement *effect* is wired by the topology layer
/// (plan 04 W13) — until then the process is modeled as one node and binding
/// failures are surfaced through the arena's stats (W9-7: "NUMA binding
/// failures MUST be visible in stats").
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NumaPolicy {
    /// Do not override OS placement — the §32.3-safe default.
    #[default]
    OsDefault,
    /// Prefer the current NUMA node.
    Local,
    /// Distribute across nodes.
    Interleave,
    /// Prefer/require a specific node.
    Bind(NodeId),
    /// Use the arena's own placement policy (meaningful at request level; at
    /// arena level it defers to the default arena's policy).
    ArenaPolicy,
}

/// NUMA nodes the topology layer currently knows. Plan 04 W13 replaces this
/// with real discovery; until then the conservative single-domain model of
/// §15.2 applies.
pub const fn known_numa_nodes() -> u32 {
    1
}

impl NumaPolicy {
    /// The stable mode string used in stats/control output (§15.5 names).
    pub const fn as_str(self) -> &'static str {
        match self {
            NumaPolicy::OsDefault => "os_default",
            NumaPolicy::Local => "local",
            NumaPolicy::Interleave => "interleave",
            NumaPolicy::Bind(_) => "bind",
            NumaPolicy::ArenaPolicy => "arena_policy",
        }
    }

    /// §22.4 "validate policy": a `Bind` target must be a known node.
    pub const fn is_valid(self) -> bool {
        match self {
            NumaPolicy::Bind(node) => node.0 < known_numa_nodes(),
            _ => true,
        }
    }

    /// Pack for the slot's atomics: `(kind, node)`.
    const fn pack(self) -> (u8, u32) {
        match self {
            NumaPolicy::OsDefault => (0, 0),
            NumaPolicy::Local => (1, 0),
            NumaPolicy::Interleave => (2, 0),
            NumaPolicy::Bind(node) => (3, node.0),
            NumaPolicy::ArenaPolicy => (4, 0),
        }
    }

    /// Unpack from the slot's atomics (total: unknown kinds decode to the
    /// safe default).
    const fn unpack(kind: u8, node: u32) -> NumaPolicy {
        match kind {
            1 => NumaPolicy::Local,
            2 => NumaPolicy::Interleave,
            3 => NumaPolicy::Bind(NodeId(node)),
            4 => NumaPolicy::ArenaPolicy,
            _ => NumaPolicy::OsDefault,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors, names, configuration
// ---------------------------------------------------------------------------

/// Why an arena operation failed. Mirrors the §36.14 error classes where they
/// apply (`TOPO_ERR_INVALID_CAP` ⇒ [`NoSuchArena`](Self::NoSuchArena),
/// `TOPO_ERR_AUTHORITY_DENIED` ⇒ [`AuthorityDenied`](Self::AuthorityDenied),
/// `TOPO_ERR_QUOTA_EXCEEDED` ⇒ [`QuotaExceeded`](Self::QuotaExceeded),
/// `TOPO_ERR_LABEL_VIOLATION` ⇒ [`LabelViolation`](Self::LabelViolation),
/// `TOPO_ERR_ARENA_DRAINING`/illegal state ⇒ [`InvalidState`](Self::InvalidState),
/// `TOPO_ERR_WOULD_BLOCK` ⇒ [`WouldBlock`](Self::WouldBlock)).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArenaError {
    /// No live arena has this id (never created, or destroyed — ids are never
    /// reused, so the two are indistinguishable by design; one deterministic
    /// error, the W8 audit rule).
    NoSuchArena,
    /// The arena's lifecycle state forbids this operation (§22.3).
    InvalidState,
    /// The default arena cannot be reset or destroyed (§22.5, F-006).
    DefaultArenaProtected,
    /// The arena's capability lacks a required right (§36.4), or a delegation
    /// requested rights the parent does not hold.
    AuthorityDenied,
    /// A delegation's quota does not fit the parent's remaining quota (§36.4
    /// quota monotonicity).
    QuotaExceeded,
    /// A delegation tried to change the information-flow label (§36.4 label
    /// monotonicity — the Lean `DelegatesFrom.label` is equality).
    LabelViolation,
    /// No vacant arena slot, or the monotone id space is exhausted.
    CapacityExhausted,
    /// The arena could not be quiesced within the spin budget (§22.6 "fail or
    /// block"): an operation is still in flight. Nothing was torn down; retry.
    WouldBlock,
    /// The configuration failed §22.4 validation.
    ConfigInvalid,
    /// Engine construction or a teardown step failed in the backend; for
    /// teardown the arena is left `ErrorQuarantined` (W9-6e), never
    /// `Destroyed`.
    Backend(BackendError),
    /// Metadata or descriptor-pool exhaustion while building the engine.
    Exhausted,
}

impl From<ExtentError> for ArenaError {
    fn from(e: ExtentError) -> ArenaError {
        match e {
            ExtentError::Backend(b) => ArenaError::Backend(b),
            ExtentError::InvalidRequest | ExtentError::Overflow => ArenaError::ConfigInvalid,
            ExtentError::Exhausted | ExtentError::Stale | ExtentError::NotFree => {
                ArenaError::Exhausted
            }
        }
    }
}

/// A fixed 32-byte arena name (§22.2 `char name[32]`), zero-padded.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArenaName([u8; 32]);

impl ArenaName {
    /// Build from a string, rejecting names longer than 32 bytes
    /// ([`ArenaError::ConfigInvalid`] at creation).
    pub fn new(name: &str) -> Option<ArenaName> {
        let bytes = name.as_bytes();
        if bytes.len() > 32 {
            return None;
        }
        let mut buf = [0u8; 32];
        buf[..bytes.len()].copy_from_slice(bytes);
        Some(ArenaName(buf))
    }

    /// The raw, zero-padded bytes.
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The name as a string slice (up to the first NUL).
    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(32);
        core::str::from_utf8(&self.0[..len]).unwrap_or("")
    }
}

/// Configuration for [`ArenaSet::create`] (§22.4, F-005). The default is a
/// modest, fully-capable arena: unlimited quota, all rights, the single
/// `PUBLIC` label, OS-default NUMA placement, and the small engine geometry
/// (explicit arenas reserve their own regions, so the default keeps them far
/// smaller than the process-wide default arena's).
#[derive(Clone, Copy, Debug)]
pub struct ArenaConfig {
    /// Diagnostic name (§22.2).
    pub name: ArenaName,
    /// Information-flow label (§36.12); `PUBLIC` on POSIX.
    pub label: Label,
    /// Quota limit in bytes; `u64::MAX` = effectively unlimited.
    pub quota_limit: u64,
    /// The capability rights the arena is minted with (§36.4). **Authority not
    /// granted here cannot be conjured later** — in particular, an arena minted
    /// without [`CapRights::DESTROY`] can never be reset or destroyed and holds
    /// its quota until process exit.
    pub rights: CapRights,
    /// NUMA placement mode (§15.5).
    pub numa: NumaPolicy,
    /// Engine geometry (regions + descriptor pools).
    pub allocator: AllocatorConfig,
}

impl Default for ArenaConfig {
    fn default() -> ArenaConfig {
        ArenaConfig {
            name: ArenaName::default(),
            label: Label::PUBLIC,
            quota_limit: u64::MAX,
            rights: CapRights::ALL,
            numa: NumaPolicy::OsDefault,
            allocator: AllocatorConfig::small(),
        }
    }
}

impl ArenaConfig {
    /// §22.4 "validate policy".
    fn is_valid(&self) -> bool {
        self.numa.is_valid()
    }
}

/// An attenuation request for [`ArenaSet::delegate`] (§36.4): the child's
/// rights must be a subset of the parent's, its quota must fit the parent's
/// remaining quota (and is reserved from it), and its label must equal the
/// parent's — the Lean `DelegatesFrom` invariants, *checked*, not assumed.
#[derive(Clone, Copy, Debug)]
pub struct DelegationSpec {
    /// Diagnostic name for the child.
    pub name: ArenaName,
    /// The child's rights; must satisfy
    /// `rights.is_attenuation_of(parent.rights)`.
    pub rights: CapRights,
    /// The child's quota limit; reserved against the parent's remaining quota
    /// for the child's whole lifetime.
    pub quota_limit: u64,
    /// The child's label; must equal the parent's (§36.4 label monotonicity).
    pub label: Label,
    /// Engine geometry for the child.
    pub allocator: AllocatorConfig,
}

// ---------------------------------------------------------------------------
// Provider factory and cache-drain seams
// ---------------------------------------------------------------------------

/// Makes a fresh [`TopoBackingProvider`] instance per arena region (each arena
/// owns two: spans and medium/large). On POSIX every instance is an
/// independent ambient-authority mapper; on seLe4n the factory carves
/// per-arena authority from the resource server's inventory (plan 09).
pub trait ProviderFactory: Sync {
    /// The provider type every arena engine in the set uses.
    type Provider: TopoBackingProvider + Send + Sync;

    /// Make a provider for one of `arena`'s regions.
    fn make_provider(&self, arena: ArenaId) -> Result<Self::Provider, BackendError>;
}

/// The cache-drain seam (W9-4b/W9-6b): invoked during reset/destroy — after
/// the lifecycle gate has flipped (no new operations) and before the engine
/// teardown — so every front-end structure removes the arena's objects.
///
/// `classify` resolves an address to its owning arena through the still-live
/// pagemap; implementations **discard** matching entries (they must not free
/// them back — the teardown reclaims the backing wholesale, and the gate would
/// reject the frees). This is §22.5's "flushed or *invalidated safely*"
/// realized as invalidate, and B.5's "no cache holds an arena object
/// post-drain" is the acceptance test. The M1/M2 cache layer registers its
/// fabric here when W16-4 wires it under the public API; tests drive real
/// `ThreadCache`/`TransferCache`/`CpuCache` instances through it today.
pub trait ArenaCacheDrain: Sync {
    /// Remove every cached address belonging to `arena` from all caches.
    fn drain(&self, arena: ArenaId, classify: &dyn Fn(usize) -> Option<ArenaId>);
}

// ---------------------------------------------------------------------------
// Per-arena counters and snapshots (stats, plan 07 wiring)
// ---------------------------------------------------------------------------

/// Lifecycle/policy counters a slot keeps across its occupant's life (atomics;
/// relaxed — stats counters, not synchronization).
#[derive(Debug, Default)]
struct ArenaCounters {
    /// Completed resets (§22.5 "stats record reset generation").
    resets: AtomicU32,
    /// The reset generation: bumped per completed reset (W9-4c).
    reset_generation: AtomicU32,
    /// NUMA binding failures (§15.5: MUST be visible in stats, W9-7).
    numa_binding_failures: AtomicU64,
    /// Destroy-protocol scrubs performed (W9-6c).
    scrubs: AtomicU32,
    /// Destroy-protocol scrubs skipped because the reuse label equals the
    /// arena label (the W9-6c skip rule, recorded as required).
    scrubs_skipped_label: AtomicU32,
}

impl ArenaCounters {
    fn reset_for_new_occupant(&self) {
        self.resets.store(0, Ordering::Relaxed);
        self.reset_generation.store(0, Ordering::Relaxed);
        self.numa_binding_failures.store(0, Ordering::Relaxed);
        self.scrubs.store(0, Ordering::Relaxed);
        self.scrubs_skipped_label.store(0, Ordering::Relaxed);
    }
}

/// A point-in-time view of one arena (the per-arena stats surface, §31.1 /
/// plan 07). Engine counters come from [`AllocatorStats`]; the lifecycle and
/// authority fields from the descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaSnapshot {
    /// The arena's id.
    pub id: ArenaId,
    /// Diagnostic name.
    pub name: ArenaName,
    /// Lifecycle state at snapshot time.
    pub state: ArenaState,
    /// Information-flow label.
    pub label: Label,
    /// The rights the arena's capability grants.
    pub rights: CapRights,
    /// The delegating parent, if any.
    pub parent: Option<ArenaId>,
    /// Quota limit in bytes (`u64::MAX` = unlimited).
    pub quota_limit: u64,
    /// Live usable bytes charged (exact at quiescence).
    pub quota_used: u64,
    /// Bytes reserved for live delegated children.
    pub quota_delegated: u64,
    /// NUMA placement mode.
    pub numa: NumaPolicy,
    /// Completed resets.
    pub resets: u32,
    /// Current reset generation.
    pub reset_generation: u32,
    /// NUMA binding failures (W9-7).
    pub numa_binding_failures: u64,
    /// Destroy-protocol scrubs performed / label-skipped (W9-6c).
    pub scrubs: u32,
    /// Scrubs skipped because cross-label reuse was impossible.
    pub scrubs_skipped_label: u32,
    /// The engine's statistics snapshot.
    pub engine: AllocatorStats,
}

// ---------------------------------------------------------------------------
// ArenaSlot — one registry slot (recycled behind monotone ids, §27.5 style)
// ---------------------------------------------------------------------------

/// "This slot is vacant" sentinel for `ArenaSlot::id`. Ids are monotone and
/// creation fails before reaching it, so no live arena can collide with it.
const VACANT: u32 = u32::MAX;
/// "No parent" sentinel for `ArenaSlot::parent` (root arenas).
const NO_PARENT: u32 = u32::MAX;

/// Spin budget for the teardown rendezvous (in-flight operations are bounded
/// by one malloc/free under the engine's own locks — microseconds). Reaching
/// this budget means an operation guard leaked (a bug); the lifecycle call
/// then fails with [`ArenaError::WouldBlock`] instead of hanging (§22.6 "MUST
/// fail or block").
const QUIESCE_SPIN_LIMIT: u64 = 100_000_000;

/// One registry slot. All descriptor fields are atomics (or published-before-
/// `Active` plain cells), so the hot path reads them without the registry
/// lock; the engine itself lives in place and is constructed/dropped only
/// under the registry lock with the rendezvous drained.
struct ArenaSlot<'a, P: TopoBackingProvider> {
    /// Occupant's public id, or [`VACANT`]. The hot-path lookup key.
    id: AtomicU32,
    /// [`ArenaState`] byte; the lifecycle gate.
    state: AtomicU8,
    /// The rendezvous count of operations inside the engine.
    in_flight: AtomicU32,
    /// Slot generation: bumped when an occupant is destroyed (W9-6e), the
    /// span-recycle discipline one level up.
    generation: AtomicU32,
    /// Occupant's [`CapRights`] bits (immutable while published).
    rights: AtomicU8,
    /// Occupant's [`Label`] (immutable while published).
    label: AtomicU32,
    /// Parent arena id, or [`NO_PARENT`] (immutable while published).
    parent: AtomicU32,
    /// Packed [`NumaPolicy`] (mutable via `configure`, registry-locked).
    numa_kind: AtomicU8,
    numa_node: AtomicU32,
    /// The occupant's quota cell. The engine holds a `&'a` into this — sound
    /// because slots live in never-freed metadata (§27.5).
    quota: ArenaQuota,
    /// Lifecycle/policy counters.
    counters: ArenaCounters,
    /// Diagnostic name; written only before publication (then immutable), so
    /// post-publication reads need no lock (release/acquire via `state`).
    name: UnsafeCell<[u8; 32]>,
    /// The engine, constructed in place at creation and dropped in place at
    /// destroy step W9-6d ("recycle": the providers' `Drop` releases the
    /// reservations). Initialized exactly when the slot is occupied and the
    /// state has left `Initializing`.
    engine: UnsafeCell<MaybeUninit<Allocator<'a, P>>>,
}

impl<'a, P: TopoBackingProvider> ArenaSlot<'a, P> {
    /// Re-arm a zeroed or vacated slot (registry lock held, slot unpublished).
    fn arm(
        &self,
        cfg_name: &ArenaName,
        label: Label,
        rights: CapRights,
        parent: u32,
        numa: NumaPolicy,
        quota_limit: u64,
    ) {
        self.state
            .store(ArenaState::Initializing as u8, Ordering::Relaxed);
        self.rights.store(rights.bits(), Ordering::Relaxed);
        self.label.store(label.0, Ordering::Relaxed);
        self.parent.store(parent, Ordering::Relaxed);
        let (kind, node) = numa.pack();
        self.numa_kind.store(kind, Ordering::Relaxed);
        self.numa_node.store(node, Ordering::Relaxed);
        self.quota.reconfigure(quota_limit);
        self.counters.reset_for_new_occupant();
        // SAFETY: registry lock held and the slot is unpublished (`id` is
        // VACANT), so no reader can observe the name mid-write; the later
        // `state`/`id` release-stores publish it.
        unsafe { *self.name.get() = *cfg_name.bytes() };
    }

    /// The slot's current state byte, decoded.
    #[inline]
    fn state(&self, order: Ordering) -> ArenaState {
        ArenaState::from_u8(self.state.load(order))
    }

    /// One legal lifecycle step (registry-locked paths): asserts the edge is
    /// in the Lean-pinned relation, then stores. The runtime counterpart of
    /// walking `ArenaState.canTransition`.
    fn lifecycle_step(&self, to: ArenaState) {
        let cur = self.state(Ordering::SeqCst);
        debug_assert!(
            cur.can_transition(to),
            "illegal arena lifecycle transition {cur:?} -> {to:?} (§22.3/§36.13)"
        );
        self.state.store(to as u8, Ordering::SeqCst);
    }

    /// The occupant's name (published slots only).
    fn name(&self) -> ArenaName {
        // SAFETY: written only before publication (see `arm`); the acquiring
        // state/id reads that precede every call established the publication
        // edge, after which the bytes are immutable.
        ArenaName(unsafe { *self.name.get() })
    }

    /// The occupant's parent id, if any.
    fn parent_id(&self) -> Option<ArenaId> {
        match self.parent.load(Ordering::Relaxed) {
            NO_PARENT => None,
            p => Some(ArenaId(p)),
        }
    }
}

// ---------------------------------------------------------------------------
// The operation guard (the rendezvous gate)
// ---------------------------------------------------------------------------

/// An RAII hold of one slot's in-flight gate: while alive, the slot's engine
/// cannot be torn down (the teardown waits for `in_flight == 0` after leaving
/// `Active`). Dropping it — including on unwind — releases the gate.
struct OpGuard<'s, 'a, P: TopoBackingProvider> {
    slot: &'s ArenaSlot<'a, P>,
}

impl<'a, P: TopoBackingProvider> OpGuard<'_, 'a, P> {
    /// The gated engine.
    #[inline]
    fn engine(&self) -> &Allocator<'a, P> {
        // SAFETY: the guard was admitted by `enter` — `in_flight` was raised
        // *before* `state` was observed `Active` with the matching id, so by
        // the SeqCst rendezvous (module docs) the teardown cannot have dropped
        // (nor can it drop) the engine before this guard releases. An `Active`
        // published slot always holds an initialized engine.
        unsafe { (*self.slot.engine.get()).assume_init_ref() }
    }
}

impl<P: TopoBackingProvider> Drop for OpGuard<'_, '_, P> {
    #[inline]
    fn drop(&mut self) {
        self.slot.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// ArenaSet — the registry
// ---------------------------------------------------------------------------

/// The arena registry (plan 06 W9): a fixed-capacity set of per-arena engines
/// over one shared pagemap and one safe metadata arena (§22.4), with the
/// default arena permanently in slot 0. See the module docs for the gate, the
/// id discipline, and the isolation argument.
pub struct ArenaSet<'a, F: ProviderFactory> {
    factory: F,
    meta: &'a dyn MetadataAlloc,
    meta_region: &'a (dyn MetadataRegion + Sync),
    pagemap: &'a PageMap,
    /// Slot storage in never-freed metadata (the SpanPool discipline).
    slots: NonNull<ArenaSlot<'a, F::Provider>>,
    capacity: u32,
    /// Serializes every lifecycle operation (create/configure/delegate/reset/
    /// destroy) and drain-hook installation. Hot-path operations never take
    /// it, so lifecycle waits cannot deadlock against allocation traffic
    /// (§27.2: this lock is *above* every engine-internal lock).
    registry_lock: BackendLock,
    /// Monotone id source for explicit arenas (0 is the default arena).
    next_id: AtomicU32,
    /// The W9-4b/6b cache-drain hook; written under the registry lock, read
    /// during registry-locked teardowns.
    drain_hook: UnsafeCell<Option<&'a (dyn ArenaCacheDrain + 'a)>>,
}

// SAFETY: the slot array lives in never-freed metadata; every mutable slot
// path is serialized by `registry_lock` + the in-flight rendezvous (engines
// are constructed/dropped only with the gate drained), hot-path descriptor
// reads are atomics, `drain_hook` is written/read only under the registry
// lock, and the engines themselves are `Sync` (`Allocator`'s own argument).
unsafe impl<F: ProviderFactory + Sync> Sync for ArenaSet<'_, F> where F::Provider: Send + Sync {}
// SAFETY: moving the set moves ownership of the slot storage and the factory;
// the borrowed meta/pagemap references are `Sync`, and no thread-affine state
// exists (locks are word-based spinlocks).
unsafe impl<F: ProviderFactory + Send> Send for ArenaSet<'_, F> where F::Provider: Send + Sync {}

/// RAII hold of the registry lock.
struct RegistryGuard<'l> {
    lock: &'l BackendLock,
}

impl Drop for RegistryGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.release();
    }
}

impl<'a, F: ProviderFactory> ArenaSet<'a, F> {
    /// Build a set with `capacity` slots (≥ 1) and create the **default arena**
    /// (id 0, slot 0) from `default_cfg` — ambient on POSIX: full rights and,
    /// by default, unlimited quota. Fails cleanly (nothing leaked) if the
    /// metadata, the providers, or the default engine cannot be built.
    pub fn new(
        factory: F,
        meta: &'a dyn MetadataAlloc,
        meta_region: &'a (dyn MetadataRegion + Sync),
        pagemap: &'a PageMap,
        default_cfg: &ArenaConfig,
        capacity: usize,
    ) -> Result<Self, ArenaError> {
        if capacity == 0 || capacity >= VACANT as usize || !default_cfg.is_valid() {
            return Err(ArenaError::ConfigInvalid);
        }
        let bytes = capacity
            .checked_mul(core::mem::size_of::<ArenaSlot<'a, F::Provider>>())
            .ok_or(ArenaError::ConfigInvalid)?;
        let mem = meta
            .alloc(bytes, core::mem::align_of::<ArenaSlot<'a, F::Provider>>())
            .ok_or(ArenaError::Exhausted)?;
        // SAFETY: `mem` is a fresh, exclusively-owned, aligned region of
        // exactly `bytes` bytes. Zeroing yields `capacity` valid slots: every
        // field is an atomic/integer cell whose all-zero pattern is legal, and
        // `engine` is `MaybeUninit` (no validity requirement).
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        let slots = mem.cast::<ArenaSlot<'a, F::Provider>>();

        let set = ArenaSet {
            factory,
            meta,
            meta_region,
            pagemap,
            slots,
            capacity: capacity as u32,
            registry_lock: BackendLock::new(),
            next_id: AtomicU32::new(1),
            drain_hook: UnsafeCell::new(None),
        };
        // Mark every slot vacant (id = VACANT, state = Destroyed): a zeroed id
        // would alias the default arena's id 0.
        for i in 0..set.capacity {
            let s = set.slot(i);
            s.id.store(VACANT, Ordering::Relaxed);
            s.state
                .store(ArenaState::Destroyed as u8, Ordering::Relaxed);
        }

        // Create the default arena in slot 0 (§35.4 phase 3; F-006). Same
        // §22.4 order as `create`: validate → metadata → engine (hooks would
        // install here, before any extent) → publish the id last.
        let slot0 = set.slot(0);
        slot0.arm(
            &default_cfg.name,
            default_cfg.label,
            default_cfg.rights,
            NO_PARENT,
            default_cfg.numa,
            default_cfg.quota_limit,
        );
        let engine = set.build_engine(ArenaId::DEFAULT, &default_cfg.allocator, &slot0.quota)?;
        // SAFETY: slot 0 is unpublished (no other thread has the set yet) and
        // its `engine` cell is `MaybeUninit`; writing initializes it.
        unsafe { (*slot0.engine.get()).write(engine) };
        slot0.lifecycle_step(ArenaState::Active);
        slot0.id.store(ArenaId::DEFAULT.0, Ordering::SeqCst);
        Ok(set)
    }

    /// Install the cache-drain hook (W9-4b/W9-6b). The cache fabric registers
    /// itself once at wiring time (W16-4); until then resets/destroys drain
    /// nothing — correct for the M1 central path, which feeds no caches.
    pub fn set_cache_drain(&self, hook: &'a dyn ArenaCacheDrain) {
        let _g = self.lock_registry();
        // SAFETY: registry lock held; teardowns read the hook under the same
        // lock, so no read can race this write.
        unsafe { *self.drain_hook.get() = Some(hook) };
    }

    /// The registry capacity (concurrently live arenas, default included).
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// Number of live (published, non-destroyed) arenas, default included.
    pub fn live_arenas(&self) -> usize {
        let mut n = 0;
        for i in 0..self.capacity {
            let s = self.slot(i);
            if s.id.load(Ordering::SeqCst) != VACANT {
                n += 1;
            }
        }
        n
    }

    // -- internal plumbing ----------------------------------------------------

    #[inline]
    fn lock_registry(&self) -> RegistryGuard<'_> {
        self.registry_lock.acquire();
        RegistryGuard {
            lock: &self.registry_lock,
        }
    }

    /// Slot `i` (`i < capacity`). Slot storage is never freed, so the
    /// reference lives as long as `self`.
    #[inline]
    fn slot(&self, i: u32) -> &ArenaSlot<'a, F::Provider> {
        debug_assert!(i < self.capacity);
        // SAFETY: `i < capacity` keeps the offset inside the slot allocation,
        // which is metadata-backed and valid for the set's whole lifetime.
        unsafe { &*self.slots.as_ptr().add(i as usize) }
    }

    /// Find the slot currently publishing `id`. Ids are never reused, so a
    /// hit is unambiguous; a miss means the arena never existed or was
    /// destroyed (one deterministic error, per the W8 audit rule).
    fn find_slot(&self, id: ArenaId) -> Option<&ArenaSlot<'a, F::Provider>> {
        for i in 0..self.capacity {
            let s = self.slot(i);
            if s.id.load(Ordering::SeqCst) == id.0 {
                return Some(s);
            }
        }
        None
    }

    /// Enter the rendezvous gate for `id`'s slot, requiring `needed` rights:
    /// raise `in_flight`, then (all `SeqCst` — see the module docs) confirm
    /// the slot still publishes `id` in state `Active` and grants the rights.
    /// On any failure the gate is released and nothing was touched.
    fn enter<'s>(
        &self,
        slot: &'s ArenaSlot<'a, F::Provider>,
        id: ArenaId,
        needed: CapRights,
    ) -> Result<OpGuard<'s, 'a, F::Provider>, ArenaError> {
        slot.in_flight.fetch_add(1, Ordering::SeqCst);
        let state_ok = slot.state(Ordering::SeqCst).allows_alloc();
        let id_ok = slot.id.load(Ordering::SeqCst) == id.0;
        if state_ok && id_ok {
            let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
            if rights.contains(needed) {
                return Ok(OpGuard { slot });
            }
            slot.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(ArenaError::AuthorityDenied);
        }
        slot.in_flight.fetch_sub(1, Ordering::SeqCst);
        if id_ok {
            Err(ArenaError::InvalidState) // §22.3: not Active
        } else {
            Err(ArenaError::NoSuchArena) // recycled/vacated since the lookup
        }
    }

    /// The default arena's engine. Slot 0 publishes id 0 in `Active` for the
    /// set's whole life (reset/destroy refuse the default arena), so no gate
    /// is needed — the plain `malloc` path pays nothing for W9.
    #[inline]
    fn default_engine(&self) -> &Allocator<'a, F::Provider> {
        // SAFETY: slot 0's engine is initialized by `new` (construction fails
        // otherwise) and is never torn down: `reset`/`destroy` reject
        // `ArenaId::DEFAULT` before any state change.
        unsafe { (*self.slot(0).engine.get()).assume_init_ref() }
    }

    /// Build an engine for `arena`, quota-coupled to `quota`.
    fn build_engine(
        &self,
        arena: ArenaId,
        cfg: &AllocatorConfig,
        quota: &ArenaQuota,
    ) -> Result<Allocator<'a, F::Provider>, ArenaError> {
        let span_provider = self
            .factory
            .make_provider(arena)
            .map_err(ArenaError::Backend)?;
        let large_provider = self
            .factory
            .make_provider(arena)
            .map_err(ArenaError::Backend)?;
        let mut engine = Allocator::new(
            span_provider,
            large_provider,
            self.meta,
            self.meta_region,
            self.pagemap,
            arena,
            *cfg,
        )?;
        // SAFETY: `quota` lives inside a slot carved from `self.meta` —
        // never-freed metadata (§27.5) valid for at least `'a` — so extending
        // the borrow to `'a` for the engine is sound. The cell is re-armed
        // only between occupants (registry lock held, gate drained), never
        // while an engine referencing it can run.
        let quota_ref: &'a ArenaQuota = unsafe { &*(quota as *const ArenaQuota) };
        engine.set_quota(quota_ref);
        Ok(engine)
    }

    /// Resolve an address to its owning arena through the shared pagemap (the
    /// routing primitive for `free`/`realloc`/`usable_size`, and the classify
    /// closure handed to the cache-drain hook).
    fn owner_of(&self, addr: usize) -> Option<ArenaId> {
        match classify_ptr(self.pagemap, self.meta_region, addr) {
            PointerClass::Small { arena, .. } => Some(arena),
            // SAFETY: descriptors live in never-freed metadata (§27.5).
            PointerClass::Large { desc } => Some(unsafe { &*desc }.arena()),
            _ => None,
        }
    }

    /// Spin until `slot`'s in-flight count drains (the teardown half of the
    /// rendezvous; registry lock held so no new lifecycle op interferes, and
    /// the state has already left `Active` so no new operation can enter).
    fn wait_quiescent(&self, slot: &ArenaSlot<'a, F::Provider>) -> Result<(), ArenaError> {
        let mut spins: u64 = 0;
        while slot.in_flight.load(Ordering::SeqCst) != 0 {
            core::hint::spin_loop();
            spins += 1;
            if spins >= QUIESCE_SPIN_LIMIT {
                return Err(ArenaError::WouldBlock);
            }
        }
        Ok(())
    }

    /// The label backing recycles to in this set's provider pool. `PUBLIC` for
    /// the single-authority backends (POSIX and the host simulator); plan 09
    /// threads the resource server's real pool label here. The W9-6c scrub
    /// decision compares the arena's label against this.
    fn recycle_label(&self) -> Label {
        Label::PUBLIC
    }

    // -- creation & configuration (W9-3, §22.4) --------------------------------

    /// Create an explicit arena (F-005), enforcing the §22.4 order: **validate
    /// policy → metadata from the safe metadata arena → engine construction
    /// (extent hooks, when W10 lands them, install here — before any extent
    /// can exist) → publish the id only after complete initialization.** A
    /// failure at any step unwinds fully: the slot returns to vacant and the
    /// id is not consumed, so the arena was never observable (the Lean model's
    /// reading: a failed construction is outside the lifecycle).
    ///
    /// SPEC-transition: arena create/publish, `Initializing -> Active` (§22.4)
    pub fn create(&self, cfg: &ArenaConfig) -> Result<ArenaId, ArenaError> {
        let _g = self.lock_registry();
        self.create_locked(cfg, NO_PARENT)
    }

    /// `create`'s body, with the registry lock already held (so `delegate` can
    /// validate, reserve, and create in **one** critical section — no window
    /// in which the parent could drain between the attenuation checks and the
    /// child's birth, which would break `destroy_revokes_descendants`).
    fn create_locked(&self, cfg: &ArenaConfig, parent: u32) -> Result<ArenaId, ArenaError> {
        if !cfg.is_valid() {
            return Err(ArenaError::ConfigInvalid);
        }
        // Find a vacant slot (never slot 0 — the default arena's home).
        let mut vacant = None;
        for i in 0..self.capacity {
            if self.slot(i).id.load(Ordering::SeqCst) == VACANT {
                vacant = Some(i);
                break;
            }
        }
        let Some(idx) = vacant else {
            return Err(ArenaError::CapacityExhausted);
        };
        let id = self.next_id.load(Ordering::Relaxed);
        if id == VACANT {
            return Err(ArenaError::CapacityExhausted); // monotone id space spent
        }
        let slot = self.slot(idx);
        slot.arm(
            &cfg.name,
            cfg.label,
            cfg.rights,
            parent,
            cfg.numa,
            cfg.quota_limit,
        );
        let engine = match self.build_engine(ArenaId(id), &cfg.allocator, &slot.quota) {
            Ok(e) => e,
            Err(e) => {
                // Unwind: the slot was never published (id still VACANT); mark
                // it vacant-stateled again and consume nothing.
                slot.state
                    .store(ArenaState::Destroyed as u8, Ordering::Relaxed);
                return Err(e);
            }
        };
        // SAFETY: registry lock held; the slot is unpublished (`id` VACANT) so
        // no gate can admit an operation; the engine cell of a vacant slot is
        // logically uninitialized (fresh slots are `MaybeUninit`; vacated
        // slots had their engine dropped in place at destroy step W9-6d).
        unsafe { (*slot.engine.get()).write(engine) };
        // Publish: state first (Initializing → Active is the legal §22.4
        // edge), then the id — the id store is what makes the arena findable,
        // honouring "publish arena ID only after complete initialization".
        slot.lifecycle_step(ArenaState::Active);
        slot.id.store(id, Ordering::SeqCst);
        self.next_id.store(id + 1, Ordering::Relaxed);
        Ok(ArenaId(id))
    }

    /// Delegate an attenuated child arena from `parent` (§36.4, W9-5). The
    /// child is a full arena (own engine, own regions) whose authority is a
    /// **checked attenuation** of the parent's — the runtime form of the Lean
    /// `DelegatesFrom`:
    ///
    /// * **authority monotonicity** — `spec.rights` must be an attenuation of
    ///   the parent's rights (and the parent must itself hold
    ///   [`CapRights::DELEGATE`]);
    /// * **quota monotonicity** — `spec.quota_limit` is reserved against the
    ///   parent's *remaining* quota (`used + delegated_out + limit ≤ quota`),
    ///   so by induction the whole delegation tree can never jointly exceed
    ///   the root's quota;
    /// * **label monotonicity** — `spec.label` must equal the parent's (no
    ///   silent downgrade; the Lean `DelegatesFrom.label` is equality).
    ///
    /// Delegation requires the parent `Active` (W9-6a: a draining arena
    /// rejects new delegations).
    ///
    /// SPEC-transition: arena delegation / capability attenuation (§36.4)
    pub fn delegate(&self, parent: ArenaId, spec: &DelegationSpec) -> Result<ArenaId, ArenaError> {
        // One critical section for validate + reserve + create: the parent
        // cannot drain (nor be destroyed) between the attenuation checks and
        // the child's publication.
        let _g = self.lock_registry();
        let pslot = self.find_slot(parent).ok_or(ArenaError::NoSuchArena)?;
        if !pslot.state(Ordering::SeqCst).allows_delegation() {
            return Err(ArenaError::InvalidState); // W9-6a: draining rejects delegation
        }
        let prights = CapRights::from_bits(pslot.rights.load(Ordering::Relaxed));
        if !prights.contains(CapRights::DELEGATE) {
            return Err(ArenaError::AuthorityDenied);
        }
        if !spec.rights.is_attenuation_of(prights) {
            return Err(ArenaError::AuthorityDenied); // §36.4 authority monotonicity
        }
        if Label(pslot.label.load(Ordering::Relaxed)) != spec.label {
            return Err(ArenaError::LabelViolation); // §36.4 label monotonicity
        }
        // §36.4 quota monotonicity: reserve the child's whole limit from the
        // parent's remaining quota for the child's lifetime.
        if !pslot.quota.try_reserve_delegation(spec.quota_limit) {
            return Err(ArenaError::QuotaExceeded);
        }
        let cfg = ArenaConfig {
            name: spec.name,
            label: spec.label,
            quota_limit: spec.quota_limit,
            rights: spec.rights,
            numa: NumaPolicy::OsDefault,
            allocator: spec.allocator,
        };
        match self.create_locked(&cfg, parent.0) {
            Ok(child) => Ok(child),
            Err(e) => {
                // Creation failed after the reservation: hand it back (same
                // critical section, so the parent is still live).
                pslot.quota.release_delegation(spec.quota_limit);
                Err(e)
            }
        }
    }

    /// Reconfigure the mutable policy subset — the NUMA mode (§15.5) — of an
    /// `Active` arena (F-005 "configure"; requires [`CapRights::PURGE`], the
    /// §36.4 tuning authority). Identity and authority fields (label, quota,
    /// rights, name) are immutable after creation: capabilities only
    /// attenuate, and they attenuate through [`delegate`](Self::delegate).
    pub fn configure(&self, id: ArenaId, numa: NumaPolicy) -> Result<(), ArenaError> {
        if !numa.is_valid() {
            return Err(ArenaError::ConfigInvalid);
        }
        let _g = self.lock_registry();
        let slot = self.find_slot(id).ok_or(ArenaError::NoSuchArena)?;
        if slot.state(Ordering::SeqCst) != ArenaState::Active {
            return Err(ArenaError::InvalidState);
        }
        let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
        if !rights.contains(CapRights::PURGE) {
            return Err(ArenaError::AuthorityDenied);
        }
        let (kind, node) = numa.pack();
        slot.numa_kind.store(kind, Ordering::Relaxed);
        slot.numa_node.store(node, Ordering::Relaxed);
        Ok(())
    }

    /// Record a NUMA binding failure against `id` (§15.5: failures MUST be
    /// visible in stats, W9-7). Called by the placement path when a `Bind`
    /// target cannot be honoured.
    pub fn record_numa_binding_failure(&self, id: ArenaId) {
        if let Some(slot) = self.find_slot(id) {
            slot.counters
                .numa_binding_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // -- reset (W9-4, §22.5) ----------------------------------------------------

    /// Reset an explicit arena (§22.5): discard **every** outstanding
    /// allocation, return/retain its extents per the engine's retain policy,
    /// drain the registered caches, bump the reset generation, and leave the
    /// arena `Active`. Preconditions enforced (W9-4a): the arena must be
    /// `Active`, must not be the default arena (§22.5; F-006 keeps it
    /// permanently available), and its capability must grant
    /// [`CapRights::DESTROY`] (reset is destroy-class authority — it
    /// invalidates outstanding pointers).
    ///
    /// On a partial backing failure the arena lands in `ErrorQuarantined`
    /// (never silently `Active` with unknown backing, the §36.13 principle);
    /// the only way out is a destroy retry.
    ///
    /// SPEC-transition: arena reset, `Active -> Resetting -> Active` (§22.5)
    pub fn reset(&self, id: ArenaId) -> Result<(), ArenaError> {
        let _g = self.lock_registry();
        if id == ArenaId::DEFAULT {
            return Err(ArenaError::DefaultArenaProtected);
        }
        let slot = self.find_slot(id).ok_or(ArenaError::NoSuchArena)?;
        // W9-4a: Active → Resetting, atomically (a racing lifecycle op cannot
        // exist — registry lock — but the gate-reading hot paths require the
        // SeqCst store).
        if slot.state(Ordering::SeqCst) != ArenaState::Active {
            return Err(ArenaError::InvalidState);
        }
        let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
        if !rights.contains(CapRights::DESTROY) {
            return Err(ArenaError::AuthorityDenied);
        }
        slot.lifecycle_step(ArenaState::Resetting);
        // W9-4a "no active allocators": drain the rendezvous. Nothing has been
        // torn down yet, so a timeout legally returns to `Active`
        // (`Resetting -> Active` is the reset-complete edge — completing an
        // empty reset) and reports `WouldBlock`.
        if self.wait_quiescent(slot).is_err() {
            slot.lifecycle_step(ArenaState::Active);
            return Err(ArenaError::WouldBlock);
        }
        // W9-4b: every cache holding this arena's objects drains (invalidate
        // semantics — see `ArenaCacheDrain`). The pagemap is still fully
        // populated here, so the classify closure resolves every address.
        self.run_cache_drain(id);
        // W9-4c: discard objects, return extents per policy, reset accounting.
        // SAFETY: the gate is drained and the registry lock serializes
        // lifecycle ops — the engine is quiescent, which is `reset_all`'s
        // contract; §22.5's pointer-invalidation contract is this method's own.
        let report = unsafe { self.engine_of(slot).reset_all() };
        if report.backing_failures > 0 {
            slot.lifecycle_step(ArenaState::ErrorQuarantined); // W9-6e principle
            return Err(ArenaError::Backend(BackendError::Unsupported));
        }
        // W9-4c: bump the reset generation; record the reset (§22.5 "stats
        // record reset generation").
        slot.counters.resets.fetch_add(1, Ordering::Relaxed);
        slot.counters
            .reset_generation
            .fetch_add(1, Ordering::Relaxed);
        slot.lifecycle_step(ArenaState::Active); // §22.5: remains Active
        Ok(())
    }

    /// The engine of an occupied slot (registry-locked teardown paths).
    #[inline]
    fn engine_of<'s>(
        &self,
        slot: &'s ArenaSlot<'a, F::Provider>,
    ) -> &'s Allocator<'a, F::Provider> {
        // SAFETY: callers hold the registry lock on a slot they have verified
        // occupied (id != VACANT) in a non-`Initializing` state — such a
        // slot's engine cell was initialized at publication and is dropped
        // only by the destroy path, which also holds the registry lock.
        unsafe { (*slot.engine.get()).assume_init_ref() }
    }

    /// Invoke the registered cache-drain hook for `arena` (registry lock held).
    fn run_cache_drain(&self, arena: ArenaId) {
        // SAFETY: registry lock held — the only writer (`set_cache_drain`)
        // holds the same lock.
        let hook = unsafe { *self.drain_hook.get() };
        if let Some(h) = hook {
            h.drain(arena, &|addr| self.owner_of(addr));
        }
    }

    // -- destroy (W9-4d/W9-6, §22.6/§36.13) -------------------------------------

    /// Destroy an explicit arena (§22.6): the full §36.13 revocation protocol,
    /// strictly ordered and step-isolated (plan 06 DD-3):
    ///
    /// ```text
    /// 1  DRAINING: reject new allocations + delegations          (W9-6a)
    /// 2  drain caches; discard objects + central lists;
    ///    stale frees now classify foreign and are rejected       (W9-6b)
    /// 3  unmap (decommit + seal both regions)                    (W9-6c)
    /// 4  scrub iff cross-label reuse is possible; recorded       (W9-6c)
    /// 5  revoke derived capabilities                             (W9-6d)
    /// 6  recycle (drop the engine; providers release backing)    (W9-6d)
    /// 7  DESTROYED + generation++; the id is retired forever     (W9-6e)
    /// ```
    ///
    /// Live delegated children are destroyed first (leaves inward): destroying
    /// a parent revokes all derived authority, mirroring the Lean
    /// `destroy_revokes_descendants` at the arena level. **A partial failure
    /// at any step leaves the arena (and any still-live descendants)
    /// `Draining`/`ErrorQuarantined` — never `Destroyed`** (the only edge into
    /// `Destroyed` leaves `Draining`, pinned by the Lean
    /// `only_draining_reaches_destroyed`); calling `destroy` again retries the
    /// protocol (every step is idempotent).
    ///
    /// SPEC-transition: arena destroy/revocation, `Active|ErrorQuarantined -> Draining -> Destroyed` (§22.6/§36.13)
    pub fn destroy(&self, id: ArenaId) -> Result<(), ArenaError> {
        let _g = self.lock_registry();
        if id == ArenaId::DEFAULT {
            return Err(ArenaError::DefaultArenaProtected);
        }
        let slot = self.find_slot(id).ok_or(ArenaError::NoSuchArena)?;
        let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
        if !rights.contains(CapRights::DESTROY) {
            return Err(ArenaError::AuthorityDenied);
        }
        // Pre-pass: flip the whole subtree to Draining (W9-6a — new
        // allocations and delegations rejected everywhere before any teardown
        // begins), depth irrelevant. Entry states: Active (fresh destroy),
        // ErrorQuarantined (retry), Draining (resume after WouldBlock).
        self.flip_subtree_draining(id)?;
        // Teardown leaves-first: repeatedly destroy a doomed arena with no
        // live children, until the subtree is gone. Terminates: every
        // iteration vacates exactly one slot, and the subtree is finite.
        loop {
            let Some(leaf) = self.find_doomed_leaf(id) else {
                // The root itself is the last leaf; when even it is gone, the
                // subtree is fully destroyed.
                return Ok(());
            };
            self.destroy_one(leaf)?;
        }
    }

    /// Flip `root` and every live descendant to `Draining` (the W9-6a
    /// pre-pass). Legal entry states per node: `Active` (fresh),
    /// `ErrorQuarantined` (retry), `Draining` (resume); anything else is a
    /// lifecycle violation.
    fn flip_subtree_draining(&self, root: ArenaId) -> Result<(), ArenaError> {
        for i in 0..self.capacity {
            let s = self.slot(i);
            let sid = s.id.load(Ordering::SeqCst);
            if sid == VACANT {
                continue;
            }
            if sid != root.0 && !self.is_descendant_of(ArenaId(sid), root) {
                continue;
            }
            match s.state(Ordering::SeqCst) {
                ArenaState::Active | ArenaState::ErrorQuarantined => {
                    s.lifecycle_step(ArenaState::Draining);
                }
                ArenaState::Draining => {} // resuming an interrupted destroy
                _ => return Err(ArenaError::InvalidState),
            }
        }
        Ok(())
    }

    /// Whether `id` descends from `root` through parent links. Bounded by
    /// `capacity` steps (defence against a corrupted parent cycle).
    fn is_descendant_of(&self, id: ArenaId, root: ArenaId) -> bool {
        let mut cur = id;
        for _ in 0..self.capacity {
            let Some(slot) = self.find_slot(cur) else {
                return false;
            };
            match slot.parent_id() {
                Some(p) if p == root => return true,
                Some(p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Find a doomed (Draining) arena in `root`'s subtree — `root` included —
    /// that has no live children (a teardown leaf).
    fn find_doomed_leaf(&self, root: ArenaId) -> Option<ArenaId> {
        for i in 0..self.capacity {
            let s = self.slot(i);
            let sid = s.id.load(Ordering::SeqCst);
            if sid == VACANT {
                continue;
            }
            let aid = ArenaId(sid);
            if aid != root && !self.is_descendant_of(aid, root) {
                continue;
            }
            if self.has_live_children(aid) {
                continue;
            }
            return Some(aid);
        }
        None
    }

    /// Whether any live arena names `id` as its parent.
    fn has_live_children(&self, id: ArenaId) -> bool {
        for i in 0..self.capacity {
            let s = self.slot(i);
            if s.id.load(Ordering::SeqCst) == VACANT {
                continue;
            }
            if s.parent_id() == Some(id) {
                return true;
            }
        }
        false
    }

    /// Destroy exactly one childless, `Draining` arena: protocol steps 2–7.
    /// On a step failure the arena moves to `ErrorQuarantined` and the error
    /// is returned (never `Destroyed`, W9-6e).
    fn destroy_one(&self, id: ArenaId) -> Result<(), ArenaError> {
        let slot = self.find_slot(id).ok_or(ArenaError::NoSuchArena)?;
        debug_assert_eq!(slot.state(Ordering::SeqCst), ArenaState::Draining);
        debug_assert!(!self.has_live_children(id));

        // Quiesce: after Draining no new operation enters; wait out the rest.
        // A timeout leaves the arena Draining (resume by calling destroy
        // again) — nothing has been torn down.
        self.wait_quiescent(slot)?;

        // Step 2 (W9-6b): drain caches, then discard every object — central
        // lists empty, pagemap entries retired (stale frees now classify
        // foreign/released and are rejected: the "reject stale frees" choice).
        self.run_cache_drain(id);
        let engine = self.engine_of(slot);
        // SAFETY: gate drained + registry lock held ⇒ quiescent (reset_all's
        // contract); destroy invalidates outstanding pointers by definition.
        let report = unsafe { engine.reset_all() };
        if report.backing_failures > 0 {
            slot.lifecycle_step(ArenaState::ErrorQuarantined);
            return Err(ArenaError::Backend(BackendError::Unsupported));
        }

        // Step 3 (W9-6c): unmap — client-unreachable before revocation
        // (§36.6), and the managers seal against any further allocation.
        if let Err(e) = engine.unmap_backing() {
            slot.lifecycle_step(ArenaState::ErrorQuarantined);
            return Err(e.into());
        }

        // Step 4 (W9-6c): scrub iff backing can be reused at a different
        // label (§36.12; skippable only when the reuse label equals the
        // arena's — on the single-label backends it always does). Recorded
        // either way, as the acceptance requires.
        let arena_label = Label(slot.label.load(Ordering::Relaxed));
        if arena_label != self.recycle_label() {
            if let Err(e) = engine.scrub_backing() {
                slot.lifecycle_step(ArenaState::ErrorQuarantined);
                return Err(e.into());
            }
            slot.counters.scrubs.fetch_add(1, Ordering::Relaxed);
        } else {
            slot.counters
                .scrubs_skipped_label
                .fetch_add(1, Ordering::Relaxed);
        }

        // Step 5 (W9-6d): revoke derived capabilities — before the backing
        // can return to a pool serving another authority domain (§36.6).
        if let Err(e) = engine.revoke_backing() {
            slot.lifecycle_step(ArenaState::ErrorQuarantined);
            return Err(e.into());
        }

        // Step 6 (W9-6d): recycle — drop the engine in place; the providers'
        // `Drop` releases both reservations to the OS/pool (§36.6
        // `Revoked -> RecyclableUntyped`).
        // SAFETY: registry lock held, gate drained, state Draining (no new
        // entry can be admitted), and the slot is occupied — the engine cell
        // is initialized and uniquely ours to drop. After this the cell is
        // logically uninitialized; the slot is vacated below before the lock
        // is released, so no path can observe an occupied slot without an
        // engine.
        unsafe { ptr::drop_in_place((*slot.engine.get()).as_mut_ptr()) };

        // Step 7 (W9-6e): return the child's quota reservation to its parent,
        // finalize DESTROYED, bump the slot generation, and retire the id
        // forever (vacate the slot — a stale id can never resolve again).
        if let Some(parent) = slot.parent_id() {
            if let Some(pslot) = self.find_slot(parent) {
                pslot
                    .quota
                    .release_delegation(slot.quota.limit.load(Ordering::Relaxed));
            }
        }
        slot.lifecycle_step(ArenaState::Destroyed);
        slot.generation.fetch_add(1, Ordering::Relaxed);
        slot.id.store(VACANT, Ordering::SeqCst);
        Ok(())
    }

    // -- routed allocation paths (the W9 consumer surface) ----------------------

    /// Allocate, routing by the flag word's arena field (absent ⇒ the default
    /// arena). Null on any failure — invalid flags, unknown/inactive arena,
    /// missing [`CapRights::ALLOC`], quota exceeded, or OOM.
    pub fn allocate(&self, size: usize, align: usize, flags: RequestFlags) -> *mut u8 {
        match flags.explicit_arena() {
            None | Some(ArenaId::DEFAULT) => self.default_engine().allocate(size, align, flags),
            Some(arena) => self.allocate_in(arena, size, align, flags),
        }
    }

    /// Allocate from an explicit arena (`topo_mallocx_arena`, §36.14 — the
    /// path for ids beyond the flag word's field). If `flags` *also* names an
    /// arena it must be the same one (a conflicting routing request is a
    /// deterministic failure, §10.4).
    pub fn allocate_in(
        &self,
        arena: ArenaId,
        size: usize,
        align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        if let Some(explicit) = flags.explicit_arena() {
            if explicit != arena {
                return ptr::null_mut();
            }
        }
        if arena == ArenaId::DEFAULT {
            return self.default_engine().allocate(size, align, flags);
        }
        let Some(slot) = self.find_slot(arena) else {
            return ptr::null_mut();
        };
        let Ok(guard) = self.enter(slot, arena, CapRights::ALLOC) else {
            return ptr::null_mut();
        };
        guard.engine().allocate(size, align, flags)
    }

    /// Free `ptr`, routed to its owning arena through the shared pagemap. The
    /// arena layer adds two outcomes the engine never produces: an inactive
    /// owner rejects the free ([`FreeOutcome::ArenaInactive`] — §36.13 "reject
    /// stale frees"), and an owner without [`CapRights::FREE`] denies it
    /// ([`FreeOutcome::Denied`]). The owning engine re-validates everything
    /// else (§35.2: nothing is ever freed on a rejected outcome).
    ///
    /// # Safety
    ///
    /// As [`Allocator::free`]: `ptr` must be null, a live allocation of this
    /// set the caller still owns, or memory never returned by it (rejected
    /// harmlessly). A stale pointer aliasing a recycled live allocation would
    /// free another owner's object.
    pub unsafe fn free(&self, ptr: *mut u8) -> FreeOutcome {
        if ptr.is_null() {
            return FreeOutcome::Null;
        }
        match self.owner_of(ptr as usize) {
            None | Some(ArenaId::DEFAULT) => {
                // Not arena-owned memory (let the default engine produce the
                // precise §17.5 invalid-free taxonomy) or default-owned.
                // SAFETY: the caller's contract, forwarded unchanged.
                unsafe { self.default_engine().free(ptr) }
            }
            Some(owner) => {
                let Some(slot) = self.find_slot(owner) else {
                    // The owner died between classification and routing; its
                    // teardown retires the pagemap entries, so re-classify
                    // via the default engine for the precise outcome.
                    // SAFETY: the caller's contract, forwarded unchanged.
                    return unsafe { self.default_engine().free(ptr) };
                };
                match self.enter(slot, owner, CapRights::FREE) {
                    // SAFETY: the caller's contract, forwarded unchanged.
                    Ok(guard) => unsafe { guard.engine().free(ptr) },
                    Err(ArenaError::AuthorityDenied) => FreeOutcome::Denied,
                    Err(_) => FreeOutcome::ArenaInactive,
                }
            }
        }
    }

    /// Reallocate `ptr`, preserving its owning arena (§25.4: the move path
    /// allocates from the same arena). A flag word that explicitly names a
    /// *different* arena is a deterministic failure (cross-arena migration is
    /// not a `realloc`); naming the same arena is allowed. Requires both
    /// [`CapRights::ALLOC`] and [`CapRights::FREE`] on the owner.
    ///
    /// # Safety
    ///
    /// As [`Allocator::realloc`].
    pub unsafe fn realloc(
        &self,
        ptr: *mut u8,
        new_size: usize,
        min_align: usize,
        flags: RequestFlags,
    ) -> *mut u8 {
        if ptr.is_null() {
            return self.allocate(new_size, min_align, flags);
        }
        let owner = self.owner_of(ptr as usize);
        if let Some(explicit) = flags.explicit_arena() {
            if owner != Some(explicit) {
                return ptr::null_mut(); // §25.4: arena preserved, never migrated
            }
        }
        match owner {
            None | Some(ArenaId::DEFAULT) => {
                // SAFETY: the caller's contract, forwarded unchanged.
                unsafe {
                    self.default_engine()
                        .realloc(ptr, new_size, min_align, flags)
                }
            }
            Some(owner) => {
                let Some(slot) = self.find_slot(owner) else {
                    return ptr::null_mut();
                };
                let Ok(guard) = self.enter(slot, owner, CapRights::ALLOC.union(CapRights::FREE))
                else {
                    return ptr::null_mut();
                };
                // SAFETY: the caller's contract, forwarded unchanged.
                unsafe { guard.engine().realloc(ptr, new_size, min_align, flags) }
            }
        }
    }

    /// The usable size of the live allocation at `ptr` (§10.1), routed to its
    /// owner. `None` for everything `Allocator::usable_size` rejects, and for
    /// owners that are not `Active` (mid-teardown geometry is meaningless).
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        if ptr.is_null() {
            return None;
        }
        match self.owner_of(ptr as usize)? {
            ArenaId::DEFAULT => self.default_engine().usable_size(ptr),
            owner => {
                let slot = self.find_slot(owner)?;
                let guard = self.enter(slot, owner, CapRights::NONE).ok()?;
                guard.engine().usable_size(ptr)
            }
        }
    }

    /// Whether `ptr` is a live allocation of any `Active` arena in this set.
    pub fn owns(&self, ptr: *mut u8) -> bool {
        if ptr.is_null() {
            return false;
        }
        match self.owner_of(ptr as usize) {
            None => false,
            Some(ArenaId::DEFAULT) => self.default_engine().owns(ptr),
            Some(owner) => {
                let Some(slot) = self.find_slot(owner) else {
                    return false;
                };
                match self.enter(slot, owner, CapRights::NONE) {
                    Ok(guard) => guard.engine().owns(ptr),
                    Err(_) => false,
                }
            }
        }
    }

    /// Whether `addr` lies in memory this set manages **at all** (any arena's
    /// pages, live or not, or shared metadata) — the §35.2 mixed-allocator
    /// routing predicate, engine-independent: one pagemap covers every arena.
    pub fn recognizes(&self, ptr: *mut u8) -> bool {
        !ptr.is_null()
            && (!self.pagemap.lookup(ptr as usize).is_empty()
                || self.meta_region.contains(ptr as usize))
    }

    // -- stats & introspection (plan 07 wiring; B.5) -----------------------------

    /// The lifecycle state of arena `id`, if it is live. States gate
    /// operations and are not secret; no rights check.
    pub fn arena_state(&self, id: ArenaId) -> Option<ArenaState> {
        let slot = self.find_slot(id)?;
        let state = slot.state(Ordering::SeqCst);
        // Re-check the id: the slot may have been vacated/recycled between the
        // two loads (ids are never reused, so a match pins the occupant).
        (slot.id.load(Ordering::SeqCst) == id.0).then_some(state)
    }

    /// The rights arena `id`'s capability grants, if it is live. Not gated:
    /// in the collapsed (single-authority) model the id *is* the token, and
    /// rights only describe which operations that token admits — they reveal
    /// nothing an attempted operation would not.
    pub fn rights_of(&self, id: ArenaId) -> Option<CapRights> {
        let slot = self.find_slot(id)?;
        let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
        (slot.id.load(Ordering::SeqCst) == id.0).then_some(rights)
    }

    /// A point-in-time snapshot of arena `id` (§31.1). Requires the arena's
    /// capability to grant [`CapRights::STATS`] (§36.4 `TopoStatsCap`).
    pub fn snapshot(&self, id: ArenaId) -> Result<ArenaSnapshot, ArenaError> {
        let slot = self.find_slot(id).ok_or(ArenaError::NoSuchArena)?;
        let rights = CapRights::from_bits(slot.rights.load(Ordering::Relaxed));
        if !rights.contains(CapRights::STATS) {
            return Err(ArenaError::AuthorityDenied);
        }
        if id == ArenaId::DEFAULT {
            return Ok(self.snapshot_slot(slot, self.default_engine().stats()));
        }
        let guard = self.enter(slot, id, CapRights::STATS)?;
        let engine = guard.engine().stats();
        Ok(self.snapshot_slot(slot, engine))
    }

    /// Snapshot **every** live arena into `visit` — the operator/aggregation
    /// surface behind the process-wide stats JSON (plan 07). This is ambient
    /// operator authority by design (the per-arena [`CapRights::STATS`] gate
    /// applies to [`snapshot`](Self::snapshot), the per-capability path);
    /// label-scoped redaction for lower domains arrives with W17/plan 09
    /// (§36.12).
    pub fn snapshot_all(&self, visit: &mut dyn FnMut(ArenaSnapshot)) {
        for i in 0..self.capacity {
            let slot = self.slot(i);
            let sid = slot.id.load(Ordering::SeqCst);
            if sid == VACANT {
                continue;
            }
            let id = ArenaId(sid);
            let engine = if id == ArenaId::DEFAULT {
                self.default_engine().stats()
            } else {
                match self.enter(slot, id, CapRights::NONE) {
                    Ok(guard) => guard.engine().stats(),
                    Err(_) => continue, // mid-teardown; skip this cycle
                }
            };
            visit(self.snapshot_slot(slot, engine));
        }
    }

    /// Assemble a snapshot from a slot's descriptor + an engine snapshot.
    fn snapshot_slot(
        &self,
        slot: &ArenaSlot<'a, F::Provider>,
        engine: AllocatorStats,
    ) -> ArenaSnapshot {
        ArenaSnapshot {
            id: ArenaId(slot.id.load(Ordering::SeqCst)),
            name: slot.name(),
            state: slot.state(Ordering::SeqCst),
            label: Label(slot.label.load(Ordering::Relaxed)),
            rights: CapRights::from_bits(slot.rights.load(Ordering::Relaxed)),
            parent: slot.parent_id(),
            quota_limit: slot.quota.limit(),
            quota_used: slot.quota.used(),
            quota_delegated: slot.quota.delegated_out(),
            numa: NumaPolicy::unpack(
                slot.numa_kind.load(Ordering::Relaxed),
                slot.numa_node.load(Ordering::Relaxed),
            ),
            resets: slot.counters.resets.load(Ordering::Relaxed),
            reset_generation: slot.counters.reset_generation.load(Ordering::Relaxed),
            numa_binding_failures: slot.counters.numa_binding_failures.load(Ordering::Relaxed),
            scrubs: slot.counters.scrubs.load(Ordering::Relaxed),
            scrubs_skipped_label: slot.counters.scrubs_skipped_label.load(Ordering::Relaxed),
            engine,
        }
    }

    /// The executable B.5 arena oracle (call quiescent, like every Appendix-B
    /// checker):
    ///
    /// * every live arena's engine is well-formed (W4-5);
    /// * every quota chain holds (`used + delegated_out == committed ≤ limit`)
    ///   and `used` equals the engine's live bytes exactly (the runtime
    ///   `ArenaQuotaExact`);
    /// * every live parent link resolves to a live arena (no dangling
    ///   delegation — `destroy_revokes_descendants` at the arena level);
    /// * the backing regions of distinct live arenas are pairwise disjoint
    ///   (§22.7 isolation, executable form).
    pub fn check_invariants(&self) -> bool {
        let _g = self.lock_registry();
        for i in 0..self.capacity {
            let slot = self.slot(i);
            if slot.id.load(Ordering::SeqCst) == VACANT {
                continue;
            }
            if slot.state(Ordering::SeqCst) == ArenaState::Initializing {
                continue; // unpublished (unreachable: ids publish post-init)
            }
            let engine = self.engine_of(slot);
            if !engine.check_invariants() {
                return false;
            }
            if !slot.quota.invariants_hold() {
                return false;
            }
            if slot.quota.used() != engine.stats().live_bytes {
                return false; // ArenaQuotaExact violated
            }
            if let Some(parent) = slot.parent_id() {
                if self.find_slot(parent).is_none() {
                    return false; // dangling delegation
                }
            }
            // §22.7: pairwise-disjoint regions against every *later* live slot
            // (each unordered pair checked once).
            let regions = engine.backing_regions();
            for j in (i + 1)..self.capacity {
                let other = self.slot(j);
                if other.id.load(Ordering::SeqCst) == VACANT {
                    continue;
                }
                let other_regions = self.engine_of(other).backing_regions();
                for (a0, a1) in regions.iter().copied() {
                    for (b0, b1) in other_regions.iter().copied() {
                        if a0 < b1 && b0 < a1 {
                            return false; // overlapping arena regions (§22.7)
                        }
                    }
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::generated::tables::PAGE_SIZE;
    use crate::ptr_class::InvalidFree;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    // -- the Lean differentials (V-004 lockstep pins) --------------------------

    /// The canonical edge list of `lean/TopoMalloc/ArenaLifecycle.lean`
    /// (`ArenaState.arenaEdges`), kept literal here so drift on either side
    /// fails this test or the `arenaLifecycleGate` in `Check.lean`.
    const LEAN_ARENA_EDGES: [(ArenaState, ArenaState); 8] = {
        use ArenaState::*;
        [
            (Initializing, Active),
            (Active, Resetting),
            (Resetting, Active),
            (Resetting, ErrorQuarantined),
            (Active, Draining),
            (Draining, Destroyed),
            (Draining, ErrorQuarantined),
            (ErrorQuarantined, Draining),
        ]
    };

    #[test]
    fn arena_lifecycle_matches_lean() {
        // All 36 ordered pairs: `can_transition` is exactly the Lean edge set
        // (no reflexivity — every lifecycle operation moves the state), and
        // the gating predicates admit exactly `Active` — the same relation
        // `arenaLifecycleGate` evaluates on the Lean side (W9-2).
        for s in ArenaState::ALL {
            for t in ArenaState::ALL {
                let legal = s.can_transition(t);
                let in_edges = LEAN_ARENA_EDGES.contains(&(s, t));
                assert_eq!(
                    legal, in_edges,
                    "transition {s:?} -> {t:?} drifted from Lean"
                );
            }
            assert_eq!(s.allows_alloc(), s == ArenaState::Active);
            assert_eq!(s.allows_delegation(), s == ArenaState::Active);
        }
        // The named Lean theorems, re-checked over the runtime relation:
        // destroyed_is_terminal; only_draining_reaches_destroyed;
        // quarantine_only_retries_draining; nothing_reenters_initializing.
        for t in ArenaState::ALL {
            assert!(!ArenaState::Destroyed.can_transition(t));
            assert!(!t.can_transition(ArenaState::Initializing));
            if t.can_transition(ArenaState::Destroyed) {
                assert_eq!(t, ArenaState::Draining);
            }
            if ArenaState::ErrorQuarantined.can_transition(t) {
                assert_eq!(t, ArenaState::Draining);
            }
        }
    }

    #[test]
    fn cap_rights_match_lean_model() {
        // Six rights, exactly the Lean `CapRights` fields (alloc, free, stats,
        // purge, destroy, delegate) — pinned by bit count and by the preorder.
        let singles = [
            CapRights::ALLOC,
            CapRights::FREE,
            CapRights::STATS,
            CapRights::PURGE,
            CapRights::DESTROY,
            CapRights::DELEGATE,
        ];
        let mut all = CapRights::NONE;
        for (i, r) in singles.iter().enumerate() {
            assert_eq!(r.bits().count_ones(), 1, "right {i} is one bit");
            all = all.union(*r);
        }
        assert_eq!(all, CapRights::ALL, "the six rights partition ALL");
        // The attenuation preorder is the Lean `le`: reflexive, transitive,
        // and subset-shaped (every right a grants, b grants).
        for a in 0..=CapRights::ALL.bits() {
            let ra = CapRights::from_bits(a);
            assert!(ra.is_attenuation_of(ra), "le_refl");
            for b in 0..=CapRights::ALL.bits() {
                let rb = CapRights::from_bits(b);
                let le = ra.is_attenuation_of(rb);
                let subset = singles.iter().all(|s| !ra.contains(*s) || rb.contains(*s));
                assert_eq!(le, subset, "le({a:#b},{b:#b}) is the subset relation");
                for c in 0..=CapRights::ALL.bits() {
                    let rc = CapRights::from_bits(c);
                    if ra.is_attenuation_of(rb) && rb.is_attenuation_of(rc) {
                        assert!(ra.is_attenuation_of(rc), "le_trans");
                    }
                }
            }
        }
    }

    // -- quota -----------------------------------------------------------------

    #[test]
    fn quota_charges_exactly_and_refuses_over_limit() {
        let q = ArenaQuota::new(100);
        assert!(q.try_charge(60));
        assert!(q.try_charge(40));
        assert_eq!(q.used(), 100);
        assert!(!q.try_charge(1), "over limit must refuse");
        assert_eq!(q.used(), 100, "a refused charge changes nothing");
        q.uncharge(40);
        assert!(q.try_charge(40));
        assert!(q.invariants_hold());
    }

    #[test]
    fn quota_delegation_reserves_from_remaining() {
        let q = ArenaQuota::new(100);
        assert!(q.try_charge(30));
        // Remaining is 70: a 71-byte child must refuse, a 70-byte child fits.
        assert!(!q.try_reserve_delegation(71));
        assert!(q.try_reserve_delegation(70));
        assert_eq!(q.delegated_out(), 70);
        // Now nothing remains for own use.
        assert!(!q.try_charge(1));
        q.release_delegation(70);
        assert!(q.try_charge(70));
        assert!(q.invariants_hold());
    }

    #[test]
    fn quota_reset_used_zeroes_exactly_and_keeps_delegations() {
        let q = ArenaQuota::new(100);
        assert!(q.try_charge(25));
        assert!(q.try_reserve_delegation(50));
        assert_eq!(q.reset_used(), 25);
        assert_eq!(q.used(), 0);
        assert_eq!(q.delegated_out(), 50, "children unaffected by a reset");
        assert!(q.invariants_hold());
        assert!(q.try_charge(50), "the freed headroom is reusable");
        assert!(!q.try_charge(1));
    }

    #[test]
    fn quota_charge_is_overflow_safe() {
        let q = ArenaQuota::new(u64::MAX);
        assert!(q.try_charge(u64::MAX - 1));
        // The next charge would overflow `committed`: refuse, never wrap.
        assert!(!q.try_charge(2));
        assert!(q.try_charge(1));
        assert!(q.invariants_hold());
    }

    #[test]
    fn quota_concurrent_charges_never_exceed_limit() {
        use std::sync::Arc;
        // N threads × M charge/uncharge rounds against a tight limit: the CAS
        // gate must never admit more than `limit` at once, under any
        // interleaving (the runtime form of `arena_quota_preserved`).
        let q = Arc::new(ArenaQuota::new(64));
        let peak_violation = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let q = Arc::clone(&q);
            let bad = Arc::clone(&peak_violation);
            handles.push(std::thread::spawn(move || {
                for _ in 0..20_000 {
                    if q.try_charge(8) {
                        // `committed` is CAS-bounded; observe it directly.
                        if q.committed.load(Ordering::Relaxed) > 64 {
                            bad.store(true, Ordering::Relaxed);
                        }
                        q.uncharge(8);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(!peak_violation.load(Ordering::Relaxed), "limit breached");
        assert_eq!(q.used(), 0);
        assert!(q.invariants_hold());
    }

    // -- numa & names ------------------------------------------------------------

    #[test]
    fn numa_policy_validates_and_round_trips() {
        // §15.5: all five modes representable; Bind validated against known
        // topology (one node until plan 04 W13).
        let modes = [
            NumaPolicy::OsDefault,
            NumaPolicy::Local,
            NumaPolicy::Interleave,
            NumaPolicy::Bind(NodeId(0)),
            NumaPolicy::ArenaPolicy,
        ];
        for m in modes {
            assert!(m.is_valid());
            let (k, n) = m.pack();
            assert_eq!(NumaPolicy::unpack(k, n), m, "pack/unpack round-trip");
        }
        assert!(!NumaPolicy::Bind(NodeId(known_numa_nodes())).is_valid());
    }

    #[test]
    fn arena_name_bounds_and_round_trips() {
        assert_eq!(ArenaName::new("requests").unwrap().as_str(), "requests");
        assert_eq!(ArenaName::new("").unwrap().as_str(), "");
        let max = "x".repeat(32);
        assert_eq!(ArenaName::new(&max).unwrap().as_str(), max);
        assert!(ArenaName::new(&"x".repeat(33)).is_none());
    }

    // -- the ArenaSet end-to-end suite ------------------------------------------

    /// A host-allocation provider (the allocator.rs test pattern): committed
    /// RW memory, page-aligned reservations, real release.
    struct HostProvider {
        owned: Mutex<Vec<(usize, std::alloc::Layout)>>,
        /// Failure injection for the W9-6e partial-failure tests.
        fail_decommit: std::sync::Arc<AtomicBool>,
    }

    impl TopoBackingProvider for HostProvider {
        fn reserve(
            &self,
            _arena: ArenaId,
            size: usize,
            align: usize,
        ) -> Result<crate::backend::Region, BackendError> {
            if size == 0 || !align.is_power_of_two() {
                return Err(BackendError::InvalidRequest);
            }
            let layout = std::alloc::Layout::from_size_align(size, align.max(PAGE_SIZE))
                .map_err(|_| BackendError::InvalidRequest)?;
            // SAFETY: nonzero size.
            let p = unsafe { std::alloc::alloc(layout) };
            if p.is_null() {
                return Err(BackendError::OutOfMemory);
            }
            self.owned.lock().unwrap().push((p as usize, layout));
            Ok(crate::backend::Region { base: p, len: size })
        }
        fn commit(
            &self,
            _r: crate::backend::Region,
            _o: usize,
            _l: usize,
        ) -> Result<(), BackendError> {
            Ok(())
        }
        fn decommit(
            &self,
            _r: crate::backend::Region,
            _o: usize,
            _l: usize,
        ) -> Result<(), BackendError> {
            if self.fail_decommit.load(Ordering::Relaxed) {
                return Err(BackendError::Unsupported);
            }
            Ok(())
        }
        fn release(
            &self,
            _arena: ArenaId,
            region: crate::backend::Region,
        ) -> Result<(), BackendError> {
            let mut owned = self.owned.lock().unwrap();
            if let Some(i) = owned.iter().position(|(b, _)| *b == region.base as usize) {
                let (base, layout) = owned.swap_remove(i);
                // SAFETY: (base, layout) came from `alloc` in `reserve`.
                unsafe { std::alloc::dealloc(base as *mut u8, layout) };
            }
            Ok(())
        }
        fn name(&self) -> &'static str {
            "host-test"
        }
    }

    impl Drop for HostProvider {
        fn drop(&mut self) {
            for (b, l) in self.owned.get_mut().unwrap().drain(..) {
                // SAFETY: every entry came from `alloc` in `reserve`.
                unsafe { std::alloc::dealloc(b as *mut u8, l) };
            }
        }
    }

    struct HostFactory {
        fail_decommit: std::sync::Arc<AtomicBool>,
    }

    impl HostFactory {
        fn new() -> HostFactory {
            HostFactory {
                fail_decommit: std::sync::Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl ProviderFactory for HostFactory {
        type Provider = HostProvider;
        fn make_provider(&self, _arena: ArenaId) -> Result<HostProvider, BackendError> {
            Ok(HostProvider {
                owned: Mutex::new(Vec::new()),
                fail_decommit: std::sync::Arc::clone(&self.fail_decommit),
            })
        }
    }

    /// A small engine geometry so multi-arena tests stay cheap (the host
    /// provider commits real memory).
    fn tiny_engine() -> AllocatorConfig {
        AllocatorConfig {
            span_region_bytes: 2 * 1024 * 1024,
            span_extent_slots: 64,
            span_slots: 64,
            large_region_bytes: 8 * 1024 * 1024,
            large_extent_slots: 64,
            large_slots: 64,
        }
    }

    fn tiny_cfg() -> ArenaConfig {
        ArenaConfig {
            allocator: tiny_engine(),
            ..ArenaConfig::default()
        }
    }

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: a valid, owned allocation of `len` bytes, leaked for the
        // test's lifetime.
        unsafe { BumpArena::new(ptr, len) }
    }

    fn set<'a>(m: &'a BumpArena, pm: &'a PageMap, capacity: usize) -> ArenaSet<'a, HostFactory> {
        ArenaSet::new(HostFactory::new(), m, m, pm, &tiny_cfg(), capacity).expect("arena set")
    }

    /// Test wrapper for `ArenaSet::free`: every pointer freed in these tests
    /// is one the test just received and still owns, null, or memory never
    /// returned by the set — exactly the safety contract.
    fn tfree(s: &ArenaSet<'_, HostFactory>, p: *mut u8) -> FreeOutcome {
        // SAFETY: the caller-ownership contract above.
        unsafe { s.free(p) }
    }

    #[test]
    fn default_arena_works_ambient() {
        // W9-1 acceptance: the POSIX default arena works; the capability
        // fields are present with trivial ambient values.
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        let p = s.allocate(64, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        // SAFETY: 64 writable bytes.
        unsafe { ptr::write_bytes(p, 0xab, 64) };
        assert!(s.owns(p));
        assert_eq!(s.usable_size(p), Some(64));
        assert_eq!(tfree(&s, p), FreeOutcome::Freed);
        let snap = s.snapshot(ArenaId::DEFAULT).unwrap();
        assert_eq!(snap.id, ArenaId::DEFAULT);
        assert_eq!(snap.state, ArenaState::Active);
        assert_eq!(snap.label, Label::PUBLIC);
        assert_eq!(snap.rights, CapRights::ALL);
        assert_eq!(snap.quota_limit, u64::MAX);
        assert_eq!(snap.parent, None);
        assert!(s.check_invariants());
    }

    #[test]
    fn create_allocate_route_and_isolate() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        let a = s.create(&tiny_cfg()).unwrap();
        let b = s.create(&tiny_cfg()).unwrap();
        assert_ne!(a, b);

        // Route by flag word and by explicit id; both land in the right arena.
        let fa = RequestFlags::NONE.with_arena(a.0).unwrap();
        let pa = s.allocate(128, 16, fa);
        let pb = s.allocate_in(b, 128, 16, RequestFlags::NONE);
        let pd = s.allocate(128, 16, RequestFlags::NONE);
        assert!(!pa.is_null() && !pb.is_null() && !pd.is_null());
        // Conflicting routing (handle says b, flags say a) fails (§10.4).
        assert!(s.allocate_in(b, 64, 16, fa).is_null());

        // §22.7 isolation: each pointer resolves to its own arena; the
        // regions are disjoint (check_invariants proves pairwise).
        assert!(s.check_invariants());

        // Reset A: B and the default arena are untouched and functional
        // (the runtime `arena_reset_invalidates_only_target_arena`).
        // SAFETY: pb/pd stay live; we only write our own allocations.
        unsafe {
            ptr::write_bytes(pb, 0x5b, 128);
            ptr::write_bytes(pd, 0x5d, 128);
        }
        s.reset(a).unwrap();
        // SAFETY: reading back the bytes we wrote to live allocations.
        unsafe {
            assert_eq!(*pb, 0x5b);
            assert_eq!(*pd, 0x5d);
        }
        assert_eq!(tfree(&s, pb), FreeOutcome::Freed);
        assert_eq!(tfree(&s, pd), FreeOutcome::Freed);
        // The reset arena is Active again and serves fresh allocations.
        assert_eq!(s.arena_state(a), Some(ArenaState::Active));
        let pa2 = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        assert!(!pa2.is_null());
        assert_eq!(tfree(&s, pa2), FreeOutcome::Freed);
        assert!(s.check_invariants());
    }

    #[test]
    fn reset_discards_and_invalidates_stale_pointers() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        let a = s.create(&tiny_cfg()).unwrap();

        // A small object, a medium one, and one freed before the reset.
        let p_small = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        let p_med = s.allocate_in(a, 64 * 1024, 16, RequestFlags::NONE);
        let p_freed = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        assert!(!p_small.is_null() && !p_med.is_null() && !p_freed.is_null());
        assert_eq!(tfree(&s, p_freed), FreeOutcome::Freed);

        let before = s.snapshot(a).unwrap();
        assert!(before.engine.live_bytes > 0);
        assert_eq!(before.quota_used, before.engine.live_bytes);

        s.reset(a).unwrap();

        // §22.5 postconditions: no live objects in accounting; stats record
        // the reset generation; the arena remains Active.
        let after = s.snapshot(a).unwrap();
        assert_eq!(after.engine.live_bytes, 0);
        assert_eq!(after.quota_used, 0);
        assert_eq!(after.resets, 1);
        assert_eq!(after.reset_generation, 1);
        assert_eq!(after.state, ArenaState::Active);

        // Outstanding pointers are invalid and *rejected* — never acted on
        // (their pagemap entries are retired, so they classify foreign).
        assert!(!s.owns(p_small));
        assert_eq!(s.usable_size(p_small), None);
        assert!(matches!(
            tfree(&s, p_small),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        assert!(matches!(
            tfree(&s, p_med),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        assert!(s.check_invariants());
    }

    #[test]
    fn default_arena_is_protected_and_unknown_ids_are_deterministic() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 2);
        assert_eq!(
            s.reset(ArenaId::DEFAULT),
            Err(ArenaError::DefaultArenaProtected)
        );
        assert_eq!(
            s.destroy(ArenaId::DEFAULT),
            Err(ArenaError::DefaultArenaProtected)
        );
        // "No such arena" is one deterministic error (the W8 audit rule).
        assert_eq!(s.reset(ArenaId(99)), Err(ArenaError::NoSuchArena));
        assert_eq!(s.destroy(ArenaId(99)), Err(ArenaError::NoSuchArena));
        assert!(s
            .allocate_in(ArenaId(99), 64, 16, RequestFlags::NONE)
            .is_null());
        assert_eq!(s.arena_state(ArenaId(99)), None);
    }

    #[test]
    fn destroy_retires_id_forever_and_rejects_stale_frees() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        let a = s.create(&tiny_cfg()).unwrap();
        let p = s.allocate_in(a, 4096, 16, RequestFlags::NONE);
        assert!(!p.is_null());

        s.destroy(a).unwrap();

        // §22.6/§36.13: the arena is gone; its id is never reused (B.5); a
        // stale pointer is invalid and rejected; a stale free never becomes
        // valid for a new arena.
        assert_eq!(s.arena_state(a), None);
        assert_eq!(s.destroy(a), Err(ArenaError::NoSuchArena));
        assert!(matches!(
            tfree(&s, p),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        let b = s.create(&tiny_cfg()).unwrap();
        assert_ne!(b, a, "destroyed ids are never reused");
        assert!(matches!(
            tfree(&s, p),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        assert!(s.check_invariants());
    }

    #[test]
    fn rights_are_enforced_at_every_entry_point() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);

        // An alloc/free-only arena (the §36.4 example delegation shape).
        let cfg = ArenaConfig {
            rights: CapRights::ALLOC.union(CapRights::FREE),
            ..tiny_cfg()
        };
        let a = s.create(&cfg).unwrap();
        let p = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        assert!(!p.is_null(), "ALLOC right admits allocation");
        assert_eq!(tfree(&s, p), FreeOutcome::Freed, "FREE right admits free");
        assert_eq!(s.snapshot(a), Err(ArenaError::AuthorityDenied), "no STATS");
        assert_eq!(
            s.configure(a, NumaPolicy::Local),
            Err(ArenaError::AuthorityDenied),
            "no PURGE"
        );
        assert_eq!(s.reset(a), Err(ArenaError::AuthorityDenied), "no DESTROY");
        assert_eq!(s.destroy(a), Err(ArenaError::AuthorityDenied), "no DESTROY");
        let spec = DelegationSpec {
            name: ArenaName::default(),
            rights: CapRights::ALLOC,
            quota_limit: 1024,
            label: Label::PUBLIC,
            allocator: tiny_engine(),
        };
        assert_eq!(
            s.delegate(a, &spec),
            Err(ArenaError::AuthorityDenied),
            "no DELEGATE"
        );

        // A no-ALLOC arena refuses allocation (`arena_cap_authorizes_alloc`:
        // a successful allocation implies the capability granted it).
        let cfg = ArenaConfig {
            rights: CapRights::FREE.union(CapRights::DESTROY),
            ..tiny_cfg()
        };
        let b = s.create(&cfg).unwrap();
        assert!(s.allocate_in(b, 64, 16, RequestFlags::NONE).is_null());

        // A no-FREE arena denies frees without touching state.
        let cfg = ArenaConfig {
            rights: CapRights::ALLOC.union(CapRights::DESTROY),
            ..tiny_cfg()
        };
        let c = s.create(&cfg).unwrap();
        let p = s.allocate_in(c, 64, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        assert_eq!(tfree(&s, p), FreeOutcome::Denied);
        assert!(s.usable_size(p).is_some(), "the object is still live");
        s.destroy(c).unwrap();
    }

    #[test]
    fn quota_gates_allocation_exactly() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        // Quota for exactly two 4 KiB pages of medium allocations.
        let cfg = ArenaConfig {
            quota_limit: 2 * PAGE_SIZE as u64,
            ..tiny_cfg()
        };
        let a = s.create(&cfg).unwrap();
        let p1 = s.allocate_in(a, PAGE_SIZE, 16, RequestFlags::NONE);
        let p2 = s.allocate_in(a, PAGE_SIZE, 16, RequestFlags::NONE);
        assert!(!p1.is_null() && !p2.is_null());
        // The third page exceeds the quota: refused before any backend work.
        let p3 = s.allocate_in(a, PAGE_SIZE, 16, RequestFlags::NONE);
        assert!(p3.is_null(), "quota must gate the third page");
        // Freeing one restores headroom exactly (the coupled freeStep).
        assert_eq!(tfree(&s, p1), FreeOutcome::Freed);
        let p4 = s.allocate_in(a, PAGE_SIZE, 16, RequestFlags::NONE);
        assert!(!p4.is_null());
        let snap = s.snapshot(a).unwrap();
        assert_eq!(snap.quota_used, snap.engine.live_bytes, "ArenaQuotaExact");
        assert!(s.check_invariants());
    }

    #[test]
    fn delegation_enforces_the_lean_attenuation_invariants() {
        let m = meta(32 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 8);
        let parent = s
            .create(&ArenaConfig {
                quota_limit: 1024 * 1024,
                ..tiny_cfg()
            })
            .unwrap();

        // Authority monotonicity: requesting a right the parent lacks fails.
        let p_rights = CapRights::ALLOC
            .union(CapRights::FREE)
            .union(CapRights::DELEGATE)
            .union(CapRights::DESTROY);
        let limited = s
            .create(&ArenaConfig {
                rights: p_rights,
                quota_limit: 1024 * 1024,
                ..tiny_cfg()
            })
            .unwrap();
        let widened = DelegationSpec {
            name: ArenaName::default(),
            rights: p_rights.union(CapRights::STATS), // wider than the parent
            quota_limit: 1024,
            label: Label::PUBLIC,
            allocator: tiny_engine(),
        };
        assert_eq!(
            s.delegate(limited, &widened),
            Err(ArenaError::AuthorityDenied)
        );

        // Label monotonicity: a different label is a violation (Lean
        // `DelegatesFrom.label` is equality).
        let relabeled = DelegationSpec {
            rights: CapRights::ALLOC,
            label: Label(7),
            ..widened
        };
        assert_eq!(
            s.delegate(parent, &relabeled),
            Err(ArenaError::LabelViolation)
        );

        // Quota monotonicity: the child's limit must fit the parent's
        // remaining quota — and is reserved from it.
        let oversized = DelegationSpec {
            rights: CapRights::ALLOC
                .union(CapRights::FREE)
                .union(CapRights::DESTROY),
            label: Label::PUBLIC,
            quota_limit: 2 * 1024 * 1024, // > parent's 1 MiB
            ..widened
        };
        assert_eq!(
            s.delegate(parent, &oversized),
            Err(ArenaError::QuotaExceeded)
        );

        let child_spec = DelegationSpec {
            quota_limit: 512 * 1024,
            ..oversized
        };
        let child = s.delegate(parent, &child_spec).unwrap();
        let psnap = s.snapshot(parent).unwrap();
        assert_eq!(psnap.quota_delegated, 512 * 1024, "reservation recorded");
        let csnap = s.snapshot(child).unwrap_or_else(|_| {
            // The child has no STATS right; read through the operator surface.
            let mut found = None;
            s.snapshot_all(&mut |snap| {
                if snap.id == child {
                    found = Some(snap);
                }
            });
            found.expect("child visible to the operator surface")
        });
        assert_eq!(csnap.parent, Some(parent));
        assert_eq!(csnap.quota_limit, 512 * 1024);

        // A second delegation must fit what remains (1 MiB − 512 KiB).
        let too_big_now = DelegationSpec {
            quota_limit: 768 * 1024,
            ..child_spec
        };
        assert_eq!(
            s.delegate(parent, &too_big_now),
            Err(ArenaError::QuotaExceeded)
        );

        // Destroying the child returns its reservation.
        s.destroy(child).unwrap();
        let psnap = s.snapshot(parent).unwrap();
        assert_eq!(psnap.quota_delegated, 0);
        let now_fits = s.delegate(parent, &too_big_now).unwrap();
        s.destroy(now_fits).unwrap();
        assert!(s.check_invariants());
    }

    #[test]
    fn destroying_a_parent_revokes_the_whole_subtree() {
        let m = meta(32 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 8);
        let parent = s.create(&tiny_cfg()).unwrap();
        let spec = DelegationSpec {
            name: ArenaName::default(),
            rights: CapRights::ALL,
            quota_limit: 1024 * 1024,
            label: Label::PUBLIC,
            allocator: tiny_engine(),
        };
        let child = s.delegate(parent, &spec).unwrap();
        let grandchild = s
            .delegate(
                child,
                &DelegationSpec {
                    quota_limit: 256 * 1024,
                    ..spec
                },
            )
            .unwrap();
        let pc = s.allocate_in(child, 256, 16, RequestFlags::NONE);
        let pg = s.allocate_in(grandchild, 256, 16, RequestFlags::NONE);
        assert!(!pc.is_null() && !pg.is_null());

        // Destroy the root: the runtime `destroy_revokes_descendants` — no
        // live descendant arena, mapping, or pointer survives.
        s.destroy(parent).unwrap();
        assert_eq!(s.arena_state(parent), None);
        assert_eq!(s.arena_state(child), None);
        assert_eq!(s.arena_state(grandchild), None);
        assert!(s.allocate_in(child, 64, 16, RequestFlags::NONE).is_null());
        assert!(matches!(
            tfree(&s, pc),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        assert!(matches!(
            tfree(&s, pg),
            FreeOutcome::Invalid(InvalidFree::Foreign)
        ));
        assert!(s.check_invariants());
    }

    #[test]
    fn partial_destroy_failure_quarantines_never_destroys() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let factory = HostFactory::new();
        let fail = std::sync::Arc::clone(&factory.fail_decommit);
        let s = ArenaSet::new(factory, &m, &m, &pm, &tiny_cfg(), 4).expect("arena set");
        let a = s.create(&tiny_cfg()).unwrap();
        let p = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        assert!(!p.is_null());

        // Make the unmap step (whole-region decommit) fail: the protocol must
        // stop in ErrorQuarantined — never Destroyed (W9-6e) — and the arena
        // must reject all work in quarantine.
        fail.store(true, Ordering::Relaxed);
        match s.destroy(a) {
            Err(ArenaError::Backend(_)) => {}
            other => panic!("expected a backend failure, got {other:?}"),
        }
        assert_eq!(s.arena_state(a), Some(ArenaState::ErrorQuarantined));
        assert!(s.allocate_in(a, 64, 16, RequestFlags::NONE).is_null());
        assert_eq!(s.reset(a), Err(ArenaError::InvalidState));
        let spec = DelegationSpec {
            name: ArenaName::default(),
            rights: CapRights::ALLOC,
            quota_limit: 1024,
            label: Label::PUBLIC,
            allocator: tiny_engine(),
        };
        assert_eq!(s.delegate(a, &spec), Err(ArenaError::InvalidState));

        // Retry once the backend recovers: quarantine re-enters Draining and
        // the protocol completes (idempotent steps).
        fail.store(false, Ordering::Relaxed);
        s.destroy(a).unwrap();
        assert_eq!(s.arena_state(a), None);
        assert!(s.check_invariants());
    }

    /// A drain hook that records each invocation and classifies two probe
    /// addresses (set via interior mutability before the lifecycle call).
    struct RecordingDrain {
        calls: Mutex<Vec<(u32, bool, bool)>>, // (arena, probe_a matches, probe_b matches)
        probe_a: core::sync::atomic::AtomicUsize,
        probe_b: core::sync::atomic::AtomicUsize,
    }

    impl RecordingDrain {
        fn new() -> RecordingDrain {
            RecordingDrain {
                calls: Mutex::new(Vec::new()),
                probe_a: core::sync::atomic::AtomicUsize::new(0),
                probe_b: core::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl ArenaCacheDrain for RecordingDrain {
        fn drain(&self, arena: ArenaId, classify: &dyn Fn(usize) -> Option<ArenaId>) {
            let a = classify(self.probe_a.load(Ordering::Relaxed));
            let b = classify(self.probe_b.load(Ordering::Relaxed));
            self.calls
                .lock()
                .unwrap()
                .push((arena.0, a == Some(arena), b == Some(arena)));
        }
    }

    #[test]
    fn cache_drain_hook_runs_with_a_live_classifier() {
        // W9-4b: the drain hook fires during reset *before* the teardown, with
        // a classifier that still resolves the target arena's addresses — and
        // only that arena's (the drain is arena-exact, B.5). Declared before
        // the set so the `&'a` hook borrow outlives it.
        let drain = RecordingDrain::new();
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        s.set_cache_drain(&drain);
        let a = s.create(&tiny_cfg()).unwrap();
        let b = s.create(&tiny_cfg()).unwrap();
        let pa = s.allocate_in(a, 64, 16, RequestFlags::NONE);
        let pb = s.allocate_in(b, 64, 16, RequestFlags::NONE);
        assert!(!pa.is_null() && !pb.is_null());
        drain.probe_a.store(pa as usize, Ordering::Relaxed);
        drain.probe_b.store(pb as usize, Ordering::Relaxed);

        s.reset(a).unwrap();
        {
            let calls = drain.calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "one drain per reset");
            let (arena, a_matched, b_matched) = calls[0];
            assert_eq!(arena, a.0);
            assert!(
                a_matched,
                "the classifier resolves the target arena's address"
            );
            assert!(
                !b_matched,
                "another arena's address never matches the target"
            );
        }

        // The drain also runs during destroy (W9-6b), before the pagemap
        // teardown retires the entries.
        s.destroy(b).unwrap();
        {
            let calls = drain.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            let (arena, a_matched, b_matched) = calls[1];
            assert_eq!(arena, b.0);
            assert!(!a_matched, "arena A's stale probe never matches B");
            assert!(b_matched, "B's probe resolves during B's drain");
        }
        assert!(s.check_invariants());
    }

    #[test]
    fn capacity_and_id_space_are_bounded_cleanly() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 2); // default + one explicit
        let a = s.create(&tiny_cfg()).unwrap();
        assert_eq!(s.create(&tiny_cfg()), Err(ArenaError::CapacityExhausted));
        // Destroy frees the slot; the next create succeeds with a fresh id.
        s.destroy(a).unwrap();
        let b = s.create(&tiny_cfg()).unwrap();
        assert!(b.0 > a.0, "ids are monotone, never reused");
        assert_eq!(s.live_arenas(), 2);
        assert_eq!(s.capacity(), 2);
    }

    #[test]
    fn configure_updates_numa_and_validates() {
        let m = meta(8 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 2);
        let a = s.create(&tiny_cfg()).unwrap();
        s.configure(a, NumaPolicy::Local).unwrap();
        let snap = s.snapshot(a).unwrap();
        assert_eq!(snap.numa, NumaPolicy::Local);
        assert_eq!(
            s.configure(a, NumaPolicy::Bind(NodeId(known_numa_nodes()))),
            Err(ArenaError::ConfigInvalid)
        );
        // W9-7: binding failures are visible in stats.
        s.record_numa_binding_failure(a);
        s.record_numa_binding_failure(a);
        assert_eq!(s.snapshot(a).unwrap().numa_binding_failures, 2);
    }

    #[test]
    fn zero_size_and_realloc_route_by_owner() {
        let m = meta(16 * 1024 * 1024);
        let pm = PageMap::new();
        let s = set(&m, &pm, 4);
        let a = s.create(&tiny_cfg()).unwrap();
        let p = s.allocate_in(a, 100, 16, RequestFlags::NONE);
        assert!(!p.is_null());
        // SAFETY: 100 writable bytes.
        unsafe {
            for i in 0..100u8 {
                p.add(i as usize).write(i);
            }
        }
        // Realloc grows within the owning arena (§25.4: arena preserved);
        // content survives.
        // SAFETY: `p` is live and owned by this test; reallocated exactly once.
        let q = unsafe { s.realloc(p, 100_000, 16, RequestFlags::NONE) };
        assert!(!q.is_null());
        // SAFETY: the first 100 bytes of `q` hold the preserved prefix.
        unsafe {
            for i in 0..100u8 {
                assert_eq!(q.add(i as usize).read(), i);
            }
        }
        // The result still belongs to arena `a` (quota exactness proves the
        // charge moved within one arena).
        let snap = s.snapshot(a).unwrap();
        assert_eq!(snap.quota_used, snap.engine.live_bytes);
        assert!(snap.engine.live_bytes >= 100_000u64);
        // An explicit *different* arena on realloc is a deterministic failure.
        let fd = RequestFlags::NONE.with_arena(0).unwrap();
        // SAFETY: `q` is live; a failed realloc leaves it untouched (§25.1).
        assert!(unsafe { s.realloc(q, 64, 16, fd) }.is_null());
        assert_eq!(tfree(&s, q), FreeOutcome::Freed);
        s.destroy(a).unwrap();
    }

    #[test]
    fn lifecycle_gate_under_concurrent_allocation_stress() {
        // Hammer allocations on an arena while it is reset and destroyed: no
        // crash, no invariant violation, and every thread's view stays
        // coherent (allocations either succeed in Active or fail cleanly).
        use std::sync::Arc;
        let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
        let m_ref: &'static BumpArena = Box::leak(Box::new(meta(32 * 1024 * 1024)));
        let s: &'static ArenaSet<'static, HostFactory> = Box::leak(Box::new(
            ArenaSet::new(HostFactory::new(), m_ref, m_ref, pm, &tiny_cfg(), 4).unwrap(),
        ));
        let a = s.create(&tiny_cfg()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let stop = Arc::clone(&stop);
            handles.push(std::thread::spawn(move || {
                let mut survived = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let p = s.allocate_in(a, 64, 16, RequestFlags::NONE);
                    if !p.is_null() {
                        // SAFETY: just allocated, owned here, freed once.
                        let out = unsafe { s.free(p) };
                        // The free may race a reset's gate flip: Freed when it
                        // won the gate, ArenaInactive when the reset got there
                        // first, Invalid(Foreign) when the teardown already
                        // retired the pages. Never anything else.
                        assert!(
                            matches!(
                                out,
                                FreeOutcome::Freed
                                    | FreeOutcome::ArenaInactive
                                    | FreeOutcome::Invalid(InvalidFree::Foreign)
                            ),
                            "unexpected outcome under lifecycle race: {out:?}"
                        );
                        survived += 1;
                    }
                }
                survived
            }));
        }
        for _ in 0..20 {
            let _ = s.reset(a);
            std::thread::yield_now();
        }
        // Under load a preempted in-flight operation can legitimately make the
        // quiesce wait give up (§22.6 "fail or block" — we fail with
        // `WouldBlock`, never hang, leaving the arena `Draining`). Retrying is
        // the documented protocol resumption; a bounded loop keeps a genuine
        // leak from spinning forever.
        let mut destroyed = false;
        for _ in 0..1000 {
            match s.destroy(a) {
                Ok(()) => {
                    destroyed = true;
                    break;
                }
                Err(ArenaError::WouldBlock) => std::thread::yield_now(),
                Err(e) => panic!("unexpected destroy failure under stress: {e:?}"),
            }
        }
        assert!(destroyed, "destroy never quiesced: an op guard leaked");
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.arena_state(a), None);
        assert!(s.check_invariants());
    }
}

// SPDX-License-Identifier: MIT
//! Arena policy & authority domains — capability-backed (§22, §36.4, §36.13,
//! plan 06 W9).
//!
//! An **arena** is two things at once (D2): a jemalloc-style *policy domain*
//! (§22 — it owns extents, spans, statistics, decay, cache budgets, hugepage
//! and NUMA policy) **and** a seLe4n-style *capability-controlled resource
//! domain* (§36.4 — an authority, an information-flow label, and a quota). On
//! POSIX the capability fields are **ambient/trivial** (the default arena grants
//! every right, carries the single `PUBLIC` label, and has an unlimited quota);
//! on seLe4n they are real. The *types* exist in the core from M1 so the seLe4n
//! backend drops in without an architecture change.
//!
//! This module is the **authority + lifecycle** half of W9: the arena
//! descriptor ([`ArenaStats`] view), the §22.3 lifecycle state machine
//! ([`ArenaState`]), capability-monotonic delegation ([`CapRights`] +
//! [`ArenaTable::delegate`], §36.4/§36.16), quota accounting that cannot wrap,
//! NUMA policy modes ([`NumaPolicy`], §15.5), and the registry
//! ([`ArenaTable`]). The *data-path* half — routing an allocation to its
//! arena's storage and reclaiming an arena's spans/large allocations on
//! reset/destroy/revoke — lives in [`crate::allocator`], which drives this
//! registry.
//!
//! # Lockstep with the Lean model
//!
//! The capability lattice and delegation invariants mirror
//! `lean/TopoMalloc/SeLe4n/CapBackedArena.lean` one-for-one: [`CapRights`] is
//! `CapRights`, [`CapRights::attenuates`] is `CapRights.le`, and the three
//! [`Delegation`] checks are the `DelegatesFrom` fields (authority/quota/label
//! monotonicity, §36.4). The §22.3/§36.13 lifecycle state machine
//! ([`ArenaState::can_transition`]) mirrors `lean/TopoMalloc/ArenaLifecycle.lean`
//! (`ArenaPhase.step`); the Rust↔Lean agreement is pinned by the differential
//! test `arena_state_machine_matches_the_lean_lifecycle`.
//!
//! # The §36.13 revocation order (DD-3)
//!
//! Destroying an arena is far more serious in the seLe4n profile than a POSIX
//! `free`: backing is revocable capabilities and mapped frames, and recycling
//! backing while a client mapping or derived capability still exists would hand
//! **live authority to another security domain**. The order is the whole game —
//! **unmap → revoke → recycle** — and a partial failure must stop cleanly in
//! [`ArenaState::Draining`]/[`ArenaState::ErrorQuarantined`], **never**
//! [`ArenaState::Destroyed`]. [`RevocationPhase`] models that ordered, step-
//! isolated protocol; the allocator executes it and the table records the
//! terminal state.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::extent::BackendLock;
use crate::flags::HugepagePolicy;
use crate::ids::{ArenaId, Generation, Label, NodeId};
use crate::release::LatencyClass;

/// Maximum number of arenas a single [`ArenaTable`] tracks (ids `0..MAX_ARENAS`).
///
/// The bound is **exactly** the flag-encodable range: the internal `TOPO_ARENA`
/// flag field carries an arena id in a fixed width (see
/// [`crate::flags::RequestFlags::MAX_ARENA_ID`]), so *every* id the table can
/// vend — the largest being `MAX_ARENAS - 1` — must satisfy `id <= MAX_ARENA_ID`,
/// i.e. `MAX_ARENAS == MAX_ARENA_ID + 1`. Were the bound any larger, `create` /
/// `delegate` could hand back an id that `TOPO_ARENA(id)` then rejects as
/// unroutable — an arena usable through the advertised allocation path in name
/// only (a PR #13 review finding). Larger arena populations are the
/// "explicit-arena handle" surface (§36.14) — out of scope for the M4 vertical
/// slice, and would require widening the *internal* field, not merely this bound.
/// The compile-time assertions in `flags.rs` pin the two together.
pub const MAX_ARENAS: usize = 255;

/// A quota of [`QUOTA_UNLIMITED`] imposes no ceiling — the ambient POSIX case.
pub const QUOTA_UNLIMITED: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// Capability rights (§36.4) — mirrors Lean `CapRights`
// ---------------------------------------------------------------------------

/// The rights an arena capability grants (§36.4): allocate, free, observe
/// statistics, destroy/reset. Capabilities are **attenuable** — a delegate may
/// carry any subset of the delegator's rights, never a superset
/// ([`attenuates`](Self::attenuates)). Mirrors the Lean `CapRights` structure
/// (`SeLe4n/CapBackedArena.lean`); the four bits are the four Lean fields.
///
/// **Enforcement on POSIX (D2 collapse).** All four rights are carried (so
/// delegation can attenuate them and the seLe4n profile can enforce them). The
/// POSIX *engine* enforces only [`ALLOC`](Self::ALLOC) — [`ArenaTable::try_charge`]
/// rejects allocation from an arena whose cap lacks it (§36.16's "clients cannot
/// allocate from arenas without rights"), the property that is intrinsic to the
/// arena. `free`/`stats`/`destroy` are **ambient** on POSIX: a single process
/// holds full authority, and gating them on a *delegated child's* rights would
/// wrongly bar the *parent*-authority holder from freeing/destroying what it
/// delegated. Those rights are enforced at the seLe4n resource-server IPC
/// boundary against the **caller's** cap (plan 09), not here — the API takes an
/// arena id, not a cap, so the engine cannot know the caller's authority.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CapRights(u8);

impl CapRights {
    /// Authority to allocate from the arena.
    pub const ALLOC: CapRights = CapRights(0b0001);
    /// Authority to free the arena's allocations.
    pub const FREE: CapRights = CapRights(0b0010);
    /// Authority to observe the arena's statistics (§36.12 info-flow gated).
    pub const STATS: CapRights = CapRights(0b0100);
    /// Authority to reset/destroy the arena (the most privileged right).
    pub const DESTROY: CapRights = CapRights(0b1000);

    const ALL_BITS: u8 = 0b1111;

    /// Every right — the ambient POSIX default-arena authority.
    pub const ALL: CapRights = CapRights(Self::ALL_BITS);
    /// No rights.
    pub const NONE: CapRights = CapRights(0);

    /// Construct from a raw bit pattern, rejecting unknown bits so a forged
    /// rights word can never smuggle authority the lattice does not define.
    #[inline]
    pub const fn from_bits(bits: u8) -> Option<CapRights> {
        if bits & !Self::ALL_BITS != 0 {
            None
        } else {
            Some(CapRights(bits))
        }
    }

    /// The raw bits (for the atomic registry slot / round-tripping).
    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The union of `self` and `other` (used to build a rights set).
    #[inline]
    pub const fn union(self, other: CapRights) -> CapRights {
        CapRights(self.0 | other.0)
    }

    /// Whether `self` grants **every** right in `other`.
    #[inline]
    pub const fn contains(self, other: CapRights) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether `self` is a sound **attenuation** of `parent`: every right `self`
    /// grants, `parent` grants too (`self ⊆ parent`). This is the §36.4
    /// *authority monotonicity* relation and the Lean `CapRights.le self parent`
    /// — delegation is allowed exactly when it holds.
    #[inline]
    pub const fn attenuates(self, parent: CapRights) -> bool {
        parent.contains(self)
    }
}

// ---------------------------------------------------------------------------
// Decay policy (§20.5/§22.2) — a carried descriptor field
// ---------------------------------------------------------------------------

/// Per-arena decay & background-purge configuration (§20.2/§22.2 `DecayConfig`,
/// Appendix C, plan 04 W12-1a): how long freed backing lingers *dirty* (physically
/// backed, reusable cheaply) before it is lazily purged to *muzzy*, how long *muzzy*
/// lingers before being returned to the OS, the rate cap on release, and whether the
/// background-purge pump does routine work for this arena. Carried on the arena
/// descriptor from M1 (D2) and **consumed by the release controller**
/// ([`ReleaseController`](crate::release::ReleaseController), W12): the controller
/// reads these to gate the §21.3 ladder and bound its per-tick release rate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecayConfig {
    /// Milliseconds a freed extent stays *dirty* before lazy purge (§20.1/§20.2).
    pub dirty_decay_ms: u64,
    /// Milliseconds a *muzzy* extent stays before being returned to the OS (§20.2).
    pub muzzy_decay_ms: u64,
    /// Upper bound on bytes returned to the OS per second (§20.2); `0` ⇒ unlimited.
    /// The cap is what keeps a large idle drop from stalling on a burst of
    /// `madvise`/`decommit`; the unmet remainder becomes release-controller backlog.
    pub release_rate_bytes_per_sec: u64,
    /// Whether the background-purge pump does any non-emergency work for this arena
    /// (§20.2/§20.3). `false` suppresses routine decay (Emergency release still acts).
    pub background_purge_enabled: bool,
}

impl DecayConfig {
    /// The Appendix-C / §32.3 server defaults (10 s dirty, 10 s muzzy, unlimited rate,
    /// background purging on).
    pub const DEFAULT: DecayConfig = DecayConfig {
        dirty_decay_ms: 10_000,
        muzzy_decay_ms: 10_000,
        release_rate_bytes_per_sec: 0,
        background_purge_enabled: true,
    };

    /// The latency-first server preset (the [`DEFAULT`](Self::DEFAULT)).
    pub const fn server() -> Self {
        Self::DEFAULT
    }

    /// The RSS-minimizing preset (the `low_rss` profile, §30.1): zero decay so dirty
    /// and muzzy memory are purged/released promptly, trading reuse cost for footprint.
    pub const fn low_rss() -> Self {
        DecayConfig {
            dirty_decay_ms: 0,
            muzzy_decay_ms: 0,
            release_rate_bytes_per_sec: 0,
            background_purge_enabled: true,
        }
    }

    /// The debug preset: prompt release (returns memory quickly so use-after-free is
    /// caught sooner, §20.5) — the same aggressive decay as [`low_rss`](Self::low_rss).
    pub const fn debug() -> Self {
        Self::low_rss()
    }
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// ---------------------------------------------------------------------------
// NUMA policy (§15.5)
// ---------------------------------------------------------------------------

/// Per-arena NUMA placement policy (§15.5). At M4 the policy is *recorded* and
/// its binding-failure visibility is wired into stats; topology-aware placement
/// that consumes it is plan 04 (W13). A binding failure is never fatal — it is
/// surfaced in [`ArenaStats::numa_bind_failures`] (§15.5 "NUMA binding failures
/// MUST be visible in stats").
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum NumaPolicy {
    /// Prefer the current NUMA node (`local`).
    Local,
    /// Distribute across nodes (`interleave`).
    Interleave,
    /// Prefer or require a specific node (`bind(node)`).
    Bind(NodeId),
    /// Use arena-specific placement (`arena_policy`).
    ArenaPolicy,
    /// Do not override OS placement (`OS_default`) — the default.
    #[default]
    OsDefault,
}

impl NumaPolicy {
    /// The stable string used in stats/control output (§15.5 mode names).
    pub const fn as_str(self) -> &'static str {
        match self {
            NumaPolicy::Local => "local",
            NumaPolicy::Interleave => "interleave",
            NumaPolicy::Bind(_) => "bind",
            NumaPolicy::ArenaPolicy => "arena_policy",
            NumaPolicy::OsDefault => "os_default",
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle state machine (§22.3 + §36.13)
// ---------------------------------------------------------------------------

/// The arena lifecycle state (§22.3, extended by §36.13's `ERROR_QUARANTINED`).
///
/// **Allocations are permitted only in [`Active`](Self::Active)** (§22.3). The
/// state machine is the safety spine of reset/destroy/revocation: a partial
/// failure during destruction lands in [`Draining`](Self::Draining) or
/// [`ErrorQuarantined`](Self::ErrorQuarantined), **never**
/// [`Destroyed`](Self::Destroyed) (§36.13), so a half-revoked CSpace is never
/// reported as a clean teardown. [`can_transition`](Self::can_transition)
/// mirrors the Lean `ArenaPhase.step` exactly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ArenaState {
    /// Created, metadata being initialized; not yet allocatable (§22.4: the id
    /// is published only after initialization completes).
    Initializing = 0,
    /// Live and serving allocations (§22.3 — the only allocating state).
    Active = 1,
    /// Reset in progress (§22.5): new allocations rejected; on completion the
    /// arena returns to [`Active`](Self::Active).
    Resetting = 2,
    /// Destroy/revocation in progress (§36.13): new allocations and delegations
    /// rejected; on completion the arena becomes [`Destroyed`](Self::Destroyed).
    Draining = 3,
    /// Fully destroyed (§22.6): metadata removed, backing reclaimed, id retired.
    /// Terminal; the slot is available for a future create behind a generation
    /// bump (§B.5 id non-reuse-while-stale).
    Destroyed = 4,
    /// A reset/destroy hit a partial failure and stopped safely (§36.13): the
    /// arena is quarantined, never reported `Destroyed`. Terminal pending
    /// operator recovery.
    ErrorQuarantined = 5,
}

impl ArenaState {
    /// The stable string used in stats/control output.
    pub const fn as_str(self) -> &'static str {
        match self {
            ArenaState::Initializing => "initializing",
            ArenaState::Active => "active",
            ArenaState::Resetting => "resetting",
            ArenaState::Draining => "draining",
            ArenaState::Destroyed => "destroyed",
            ArenaState::ErrorQuarantined => "error_quarantined",
        }
    }

    /// Decode from the atomic byte (total: any unknown value is read as
    /// [`Destroyed`](Self::Destroyed), the safe "not allocatable" reading, so a
    /// torn/forged byte can never be mistaken for [`Active`](Self::Active)).
    #[inline]
    const fn from_u8(v: u8) -> ArenaState {
        match v {
            0 => ArenaState::Initializing,
            1 => ArenaState::Active,
            2 => ArenaState::Resetting,
            3 => ArenaState::Draining,
            5 => ArenaState::ErrorQuarantined,
            _ => ArenaState::Destroyed,
        }
    }

    /// Whether the arena is currently allocatable (§22.3: only `Active`).
    #[inline]
    pub const fn is_active(self) -> bool {
        matches!(self, ArenaState::Active)
    }

    /// Whether this is a terminal state (no further transitions; the slot is
    /// either gone or quarantined).
    #[inline]
    pub const fn is_terminal(self) -> bool {
        matches!(self, ArenaState::Destroyed | ArenaState::ErrorQuarantined)
    }

    /// Whether `self → to` is a legal lifecycle transition (§22.3/§36.13). The
    /// graph is acyclic except for `Resetting → Active` (a completed reset
    /// returns the arena to service, §22.5). Mirrors the Lean `ArenaPhase.step`
    /// (`lean/TopoMalloc/ArenaLifecycle.lean`), pinned equal by
    /// `arena_state_machine_matches_the_lean_lifecycle`.
    ///
    /// SPEC-transition: arena lifecycle step (§22.3/§36.13)
    pub const fn can_transition(self, to: ArenaState) -> bool {
        use ArenaState::*;
        matches!(
            (self, to),
            // Creation completes (§22.4: publish id only after init).
            (Initializing, Active)
            // Reset begins / completes (§22.5: arena remains Active on success).
            | (Active, Resetting)
            | (Resetting, Active)
            // Destroy begins / completes (§36.13: DRAINING then DESTROYED).
            | (Active, Draining)
            | (Draining, Destroyed)
            // Partial-failure landing zones (§36.13: never DESTROYED on failure).
            | (Resetting, ErrorQuarantined)
            | (Draining, ErrorQuarantined)
        )
    }
}

// ---------------------------------------------------------------------------
// §36.13 revocation protocol phases (DD-3)
// ---------------------------------------------------------------------------

/// The ordered phases of arena destruction (§36.13 / plan 06 DD-3). The order is
/// load-bearing: **unmap before revoke before recycle**, because recycling
/// untyped backing while a client mapping or derived capability still exists
/// would hand live authority to another security domain. Each phase is its own
/// step so a partial failure stops cleanly — the arena lands in
/// [`ArenaState::Draining`]/[`ArenaState::ErrorQuarantined`], never
/// [`ArenaState::Destroyed`].
///
/// On POSIX the unmap/revoke/recycle phases collapse to "free the extent" (a
/// no-op revoke — single ambient authority), so the *structure* is identical and
/// the seLe4n capability provider (plan 09) is a drop-in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum RevocationPhase {
    /// Enter DRAINING: reject new allocations and delegations; notify clients
    /// (§36.13, W9-6a).
    Draining = 0,
    /// Drain local/transfer caches and central lists; quarantine or reject stale
    /// frees (§36.13, W9-6b — shares the W9-4b cache search).
    DrainCaches = 1,
    /// Unmap client VSpace windows (§36.13, W9-6c) and scrub dirty pages if
    /// cross-label reuse is possible (→ plan 08 W18-6).
    Unmap = 2,
    /// Revoke derived frame/mapping capabilities; delete obsolete CSlots
    /// (§36.13, W9-6d).
    Revoke = 3,
    /// Recycle untyped backing to the resource-server free pools (§36.13, W9-6d
    /// `provider.recycle`).
    Recycle = 4,
    /// Finalize: mark DESTROYED with a generation increment (§36.13, W9-6e).
    Finalize = 5,
}

impl RevocationPhase {
    /// The first phase.
    pub const FIRST: RevocationPhase = RevocationPhase::Draining;

    /// The next phase in the ordered protocol, or `None` at
    /// [`Finalize`](Self::Finalize) (the protocol is a single linear chain — the
    /// recycle-after-revoke-after-unmap ordering is exactly its acyclicity).
    pub const fn next(self) -> Option<RevocationPhase> {
        use RevocationPhase::*;
        Some(match self {
            Draining => DrainCaches,
            DrainCaches => Unmap,
            Unmap => Revoke,
            Revoke => Recycle,
            Recycle => Finalize,
            Finalize => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A failure from an arena lifecycle or authority operation. The variants line
/// up with the §36.14 error classes the capability ABI must distinguish.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArenaError {
    /// No arena with the given id is registered.
    NotFound,
    /// The arena exists but is not [`Active`](ArenaState::Active) (e.g. draining,
    /// resetting, destroyed) — `TOPO_ERR_ARENA_DRAINING`.
    NotActive,
    /// The caller's capability lacks the required right
    /// (`TOPO_ERR_AUTHORITY_DENIED`).
    AuthorityDenied,
    /// The request would push the arena's used bytes past its quota
    /// (`TOPO_ERR_QUOTA_EXCEEDED`).
    QuotaExceeded,
    /// A delegation would widen rights/quota or downgrade the label
    /// (`TOPO_ERR_LABEL_VIOLATION` / authority/quota monotonicity, §36.4).
    Attenuation,
    /// The arena table is full (every slot live) — no id to assign.
    Exhausted,
    /// The requested policy is malformed (e.g. an empty/over-long name, or a
    /// quota that is zero).
    InvalidPolicy,
    /// The requested lifecycle transition is illegal from the current state
    /// (§22.3 — e.g. resetting a draining arena).
    IllegalTransition,
    /// The operation is not permitted on the default arena (§22.5: reset/destroy
    /// is for explicit arenas, not the always-present default).
    IsDefault,
}

// ---------------------------------------------------------------------------
// Policy & delegation
// ---------------------------------------------------------------------------

/// The maximum arena-name length (§22.2 `char name[32]`, NUL-terminated room).
pub const ARENA_NAME_LEN: usize = 32;

/// An arena creation/configuration policy (§22.4): the authority it confers, its
/// information-flow label, its quota ceiling, its NUMA placement mode, and a
/// short diagnostic name. Validated by [`ArenaPolicy::validate`] before any
/// metadata is touched (§22.4 "validate policy" is the *first* creation step).
#[derive(Clone, Copy, Debug)]
pub struct ArenaPolicy {
    /// The authority an arena capability over this arena grants (§36.4).
    pub rights: CapRights,
    /// The arena's information-flow label (§36.12). `PUBLIC` on POSIX.
    pub label: Label,
    /// The arena's quota ceiling in bytes ([`QUOTA_UNLIMITED`] for no ceiling).
    pub quota_limit: u64,
    /// The NUMA placement policy (§15.5).
    pub numa: NumaPolicy,
    /// The decay timing for this arena's freed backing (§20.5/§22.2). Carried
    /// from M1; consumed by the release controller at plan 04 W12 (M5).
    pub decay: DecayConfig,
    /// The hugepage placement policy for this arena (§22.2 `HugePolicy`).
    /// Carried from M1; consumed by the hugepage backend at plan 04 W11 (M5).
    pub huge: HugepagePolicy,
    /// A soft per-arena front-end cache budget in bytes (§22.2 `CacheBudget`);
    /// `0` ⇒ the global default. Carried from M1; consumed by the cache layer at
    /// plan 05 W6 (M2).
    pub cache_budget_bytes: u64,
    /// The slowest release/purge latency class this arena tolerates (§36.11, plan 04
    /// W12-4). Subsumes the three SPEC flags: [`FastOnly`](LatencyClass::FastOnly) is
    /// `no_ipc_fast_only` (a real-time arena — the release controller skips every
    /// blocking ladder rung for it), [`BoundedSlow`](LatencyClass::BoundedSlow) is
    /// `bounded_slow_path`, and [`MayBlock`](LatencyClass::MayBlock) (the default) is
    /// `may_block`. Feeds [`ReleaseController`](crate::release::ReleaseController)'s
    /// `max_latency`.
    pub latency: LatencyClass,
    /// A short diagnostic name (truncated to [`ARENA_NAME_LEN`]).
    pub name: [u8; ARENA_NAME_LEN],
}

impl Default for ArenaPolicy {
    fn default() -> Self {
        Self::explicit()
    }
}

impl ArenaPolicy {
    /// The ambient policy for the always-present default arena (§35.4 phase 3):
    /// every right, the `PUBLIC` label, an unlimited quota, OS-default NUMA.
    pub const fn ambient_default() -> ArenaPolicy {
        ArenaPolicy {
            rights: CapRights::ALL,
            label: Label::PUBLIC,
            quota_limit: QUOTA_UNLIMITED,
            numa: NumaPolicy::OsDefault,
            decay: DecayConfig::DEFAULT,
            huge: HugepagePolicy::Default,
            cache_budget_bytes: 0,
            latency: LatencyClass::MayBlock,
            name: *b"default\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        }
    }

    /// A reasonable default policy for an explicit arena: full local authority,
    /// `PUBLIC` label, unlimited quota, OS-default NUMA, empty name. Callers
    /// override fields as needed.
    pub const fn explicit() -> ArenaPolicy {
        ArenaPolicy {
            rights: CapRights::ALL,
            label: Label::PUBLIC,
            quota_limit: QUOTA_UNLIMITED,
            numa: NumaPolicy::OsDefault,
            decay: DecayConfig::DEFAULT,
            huge: HugepagePolicy::Default,
            cache_budget_bytes: 0,
            latency: LatencyClass::MayBlock,
            name: [0u8; ARENA_NAME_LEN],
        }
    }

    /// Set the human-readable name (truncated to [`ARENA_NAME_LEN`]).
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = [0u8; ARENA_NAME_LEN];
        let bytes = name.as_bytes();
        let n = if bytes.len() < ARENA_NAME_LEN {
            bytes.len()
        } else {
            ARENA_NAME_LEN
        };
        self.name[..n].copy_from_slice(&bytes[..n]);
        self
    }

    /// Set the quota ceiling.
    pub const fn with_quota(mut self, limit: u64) -> Self {
        self.quota_limit = limit;
        self
    }

    /// Set the slowest release/purge latency class this arena tolerates (§36.11,
    /// W12-4). A `FastOnly` (real-time, `no_ipc_fast_only`) arena makes the release
    /// controller skip every blocking ladder rung.
    pub const fn with_latency(mut self, latency: LatencyClass) -> Self {
        self.latency = latency;
        self
    }

    /// Set the conferred rights.
    pub const fn with_rights(mut self, rights: CapRights) -> Self {
        self.rights = rights;
        self
    }

    /// Set the information-flow label.
    pub const fn with_label(mut self, label: Label) -> Self {
        self.label = label;
        self
    }

    /// Set the NUMA placement policy.
    pub const fn with_numa(mut self, numa: NumaPolicy) -> Self {
        self.numa = numa;
        self
    }

    /// Set the decay timing (§20.5/§22.2).
    pub const fn with_decay(mut self, decay: DecayConfig) -> Self {
        self.decay = decay;
        self
    }

    /// Set the hugepage placement policy (§22.2).
    pub const fn with_huge(mut self, huge: HugepagePolicy) -> Self {
        self.huge = huge;
        self
    }

    /// Set the soft per-arena cache budget in bytes (`0` ⇒ global default).
    pub const fn with_cache_budget(mut self, bytes: u64) -> Self {
        self.cache_budget_bytes = bytes;
        self
    }

    /// Validate the policy (§22.4 step 1). A zero quota is rejected (an arena
    /// that can never allocate is a configuration error, not a useful domain);
    /// everything else is well-formed by construction (the rights bits and NUMA
    /// policy are closed enums, and the name is a fixed-width buffer).
    pub const fn validate(&self) -> Result<(), ArenaError> {
        if self.quota_limit == 0 {
            return Err(ArenaError::InvalidPolicy);
        }
        Ok(())
    }
}

/// A delegation request (§36.4 / §36.14 `delegate_arena`): the attenuated
/// authority a parent wishes to confer on a child arena. Validated against the
/// parent by [`ArenaTable::delegate`]; the three checks are the Lean
/// `DelegatesFrom` fields.
#[derive(Clone, Copy, Debug)]
pub struct Delegation {
    /// The rights the child should carry (MUST attenuate the parent's).
    pub rights: CapRights,
    /// The child's quota ceiling (MUST be ≤ the parent's *remaining* quota).
    pub quota_limit: u64,
    /// The child's label (MUST equal the parent's — no silent downgrade).
    pub label: Label,
    /// The child's NUMA policy (inherited or overridden; not a security field).
    pub numa: NumaPolicy,
    /// The child's diagnostic name.
    pub name: [u8; ARENA_NAME_LEN],
}

impl Delegation {
    /// A delegation that simply inherits the parent's authority/label, naming the
    /// child and capping its quota. Callers attenuate further with the builders.
    pub fn inheriting(parent: &ArenaStats, quota_limit: u64, name: &str) -> Delegation {
        let mut nm = [0u8; ARENA_NAME_LEN];
        let bytes = name.as_bytes();
        let n = bytes.len().min(ARENA_NAME_LEN);
        nm[..n].copy_from_slice(&bytes[..n]);
        Delegation {
            rights: parent.rights,
            quota_limit,
            label: parent.label,
            numa: parent.numa,
            name: nm,
        }
    }

    /// Restrict the delegated rights (attenuation; must stay a subset of parent).
    pub const fn with_rights(mut self, rights: CapRights) -> Self {
        self.rights = rights;
        self
    }
}

/// The reconfigurable (non-authority) policy of an arena (§22.4 *configure*,
/// F-005). Reconfiguration deliberately **cannot** change the authority
/// (rights/label) or the quota ceiling — those are fixed at create/delegate
/// time so a `configure` can never widen authority (§36.4). It updates the
/// placement/lifetime knobs an arena owns (§22.1): decay, hugepage policy, NUMA
/// preference, the soft cache budget, and the diagnostic name.
#[derive(Clone, Copy, Debug)]
pub struct ArenaConfig {
    /// New decay timing (§20.5/§22.2).
    pub decay: DecayConfig,
    /// New hugepage placement policy (§22.2).
    pub huge: HugepagePolicy,
    /// New NUMA placement policy (§15.5).
    pub numa: NumaPolicy,
    /// New soft cache budget in bytes (`0` ⇒ global default).
    pub cache_budget_bytes: u64,
    /// New diagnostic name (truncated to [`ARENA_NAME_LEN`]).
    pub name: [u8; ARENA_NAME_LEN],
}

impl ArenaConfig {
    /// Build a config snapshot from an arena's current [`ArenaStats`], so a
    /// caller can change one knob and keep the rest (`ArenaConfig::from_stats(s)
    /// .with_decay(d)`).
    pub fn from_stats(s: &ArenaStats) -> ArenaConfig {
        ArenaConfig {
            decay: s.decay,
            huge: s.huge,
            numa: s.numa,
            cache_budget_bytes: s.cache_budget_bytes,
            name: s.name,
        }
    }

    /// Set the decay timing.
    pub const fn with_decay(mut self, decay: DecayConfig) -> Self {
        self.decay = decay;
        self
    }

    /// Set the NUMA placement policy.
    pub const fn with_numa(mut self, numa: NumaPolicy) -> Self {
        self.numa = numa;
        self
    }

    /// Set the hugepage placement policy.
    pub const fn with_huge(mut self, huge: HugepagePolicy) -> Self {
        self.huge = huge;
        self
    }
}

// ---------------------------------------------------------------------------
// Stats view
// ---------------------------------------------------------------------------

/// Per-arena custom-backing (extent-hook) failure counts (§23, plan 06 W10),
/// aggregated over the arena's **two** hooked providers (its span-extent backing and
/// its large-allocation backing). Present only for an arena created with
/// [`arena_create_hooked`](crate::Allocator::arena_create_hooked); the shared
/// (POSIX/seLe4n) backend has no hooks to fail. All zero means the custom backing has
/// served the arena without faulting.
/// Only **swallowed** failures are counted (the allocator handles them internally,
/// so they are otherwise invisible). A `reserve` failure is deliberately absent: it
/// is *returned* to the caller (the alloc / arena-create fails visibly) and drops the
/// backing, so it can never be non-zero for a live or retired hooked arena.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookFailureStats {
    /// `commit` / `decommit` / `purge_*` hook failures (recovered while well-formed).
    pub commit: u64,
    /// Whole-region `release` (`dealloc`) failures — a backing refusing to take a
    /// region back (a leak in the backing; never an allocator fault).
    pub release: u64,
    /// Advisory `split`-hook failures (recorded, never acted on — §23.4).
    pub split: u64,
    /// Advisory `merge`-hook failures (recorded, never acted on — §23.4).
    pub merge: u64,
}

/// An instantaneous, copyable view of one arena's authority + accounting (§22.2
/// `ArenaStats`, §36.4). Produced by [`ArenaTable::stats`]; the values are
/// per-field atomic reads (relaxed), so a snapshot taken during concurrent
/// operations is per-field accurate but not globally instantaneous.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArenaStats {
    /// The arena id.
    pub id: ArenaId,
    /// The lifecycle state (§22.3).
    pub state: ArenaState,
    /// The authority the arena confers (§36.4).
    pub rights: CapRights,
    /// The information-flow label (§36.12).
    pub label: Label,
    /// The quota ceiling in bytes ([`QUOTA_UNLIMITED`] for none).
    pub quota_limit: u64,
    /// Bytes currently charged to the arena (own live usable bytes — the §36.17
    /// `used` counter). Excludes quota reserved for delegated children (see
    /// [`reserved`](Self::reserved)), so `Σ arena.used == live_bytes` reconciles
    /// (§8.6/§36.17).
    pub used: u64,
    /// Quota reserved for this arena's live delegated children — the sum of each
    /// child's `quota_limit` (§36.4 budget partition). `used + reserved` is the
    /// arena's committed total against `quota_limit`; `remaining_quota` is what is
    /// left for further own allocations *or* delegations. Zero for an arena with
    /// no live children, so reservation is invisible to the common case.
    pub reserved: u64,
    /// The **incarnation** generation (§36.13): bumped on create and destroy, so
    /// a handle/reference to a *destroyed-then-recreated* id is detectably stale.
    /// Survives a reset (the arena persists), which is what lets an arena handle
    /// remain valid across resets of its own arena.
    pub generation: Generation,
    /// The **reset** generation (§22.5 "stats record reset generation"): bumped
    /// each time the arena is reset, so a stale pointer into a reset arena is
    /// detectable independently of the incarnation.
    pub reset_generation: Generation,
    /// The NUMA placement policy (§15.5).
    pub numa: NumaPolicy,
    /// The decay timing (§20.5/§22.2).
    pub decay: DecayConfig,
    /// The hugepage placement policy (§22.2).
    pub huge: HugepagePolicy,
    /// The soft per-arena cache budget in bytes (§22.2; `0` ⇒ global default).
    pub cache_budget_bytes: u64,
    /// Count of NUMA binding failures surfaced for this arena (§15.5).
    pub numa_bind_failures: u64,
    /// Custom-backing (extent-hook) failure counts (§23, W10), or `None` for an
    /// arena with no hooked backing. Aggregated over the arena's span + large hooked
    /// providers by [`Allocator::arena_stats`](crate::Allocator::arena_stats).
    pub hooks: Option<HookFailureStats>,
    /// The delegating parent arena, if this arena was delegated (§36.4).
    pub parent: Option<ArenaId>,
    /// The diagnostic name (§22.2).
    pub name: [u8; ARENA_NAME_LEN],
}

impl ArenaStats {
    /// Remaining quota: `quota_limit − used − reserved`, saturating.
    /// [`QUOTA_UNLIMITED`] stays unlimited. This is the budget left for either a
    /// further own allocation **or** a new delegation — it excludes quota already
    /// reserved for live children, so a parent can never delegate (or allocate)
    /// more than it holds (§36.4 quota monotonicity / budget partition). A
    /// delegation reserving `r` reduces `remaining_quota` by `r` until the child
    /// is destroyed.
    pub const fn remaining_quota(&self) -> u64 {
        if self.quota_limit == QUOTA_UNLIMITED {
            QUOTA_UNLIMITED
        } else {
            self.quota_limit
                .saturating_sub(self.used)
                .saturating_sub(self.reserved)
        }
    }
}

// ---------------------------------------------------------------------------
// Registry slots
// ---------------------------------------------------------------------------

/// The lock-free, hot-path-readable half of an arena slot. Reads of [`state`]
/// (the allocation gate) and the quota CAS happen here without the table lock,
/// so the per-arena fast path is a couple of atomics.
struct ArenaAtomics {
    /// [`ArenaState`] as a byte.
    state: AtomicU8,
    /// [`CapRights`] bits.
    rights: AtomicU8,
    /// Incarnation generation (§36.13): bumped on create + destroy.
    generation: AtomicU32,
    /// Reset generation (§22.5): bumped on reset, independent of incarnation.
    reset_gen: AtomicU32,
    /// Bytes **committed** against the quota: own live bytes **plus** the quota
    /// reserved for live delegated children (§36.4 budget partition). This is the
    /// single counter the allocation gate compare-exchanges against
    /// [`quota_limit`](Self::quota_limit), so reserving a child's quota and
    /// charging an own allocation contend on the *same* atomic — the only way two
    /// lock-free writers can keep `committed ≤ quota_limit` race-free. The §36.17
    /// own-live `used` a caller sees ([`ArenaStats::used`]) is `committed −
    /// reserved`; reservations are budget holds, not live bytes.
    committed: AtomicU64,
    /// Quota currently **reserved** for live delegated children — the sum of each
    /// live child's `quota_limit` (§36.4). A subset of [`committed`](Self::committed);
    /// mutated only under the table lock (delegate adds, a child's destroy returns
    /// it to this parent). Tracked so `used = committed − reserved` reconciles
    /// (§8.6) and so `remaining_quota` excludes already-delegated budget.
    reserved: AtomicU64,
    /// Quota ceiling ([`QUOTA_UNLIMITED`] for ambient).
    quota_limit: AtomicU64,
    /// NUMA binding-failure counter (§15.5 stats visibility).
    numa_bind_failures: AtomicU64,
    /// The arena's hooked-backing **registry slot, plus one** (`0` ⇒ no hooked
    /// backing — the shared backend serves it; `k` ⇒ registry slot `k − 1`, plan
    /// 06 W10). Read lock-free on the routing fast path so a hooked arena resolves
    /// its own [`HookProvider`]-backed span/large backing in O(1) instead of a
    /// registry scan. Set when the backing is registered, cleared on teardown;
    /// **survives a reset** (the region is kept). A `u8` bounds `MAX_HOOK_BACKENDS`.
    hook_slot: AtomicU8,
}

impl ArenaAtomics {
    /// An unused slot: `Destroyed` (not allocatable), generation `0`
    /// (never-used sentinel), no rights, nothing committed or reserved, unlimited
    /// ceiling.
    const fn empty() -> ArenaAtomics {
        ArenaAtomics {
            state: AtomicU8::new(ArenaState::Destroyed as u8),
            rights: AtomicU8::new(0),
            generation: AtomicU32::new(0),
            reset_gen: AtomicU32::new(0),
            committed: AtomicU64::new(0),
            reserved: AtomicU64::new(0),
            quota_limit: AtomicU64::new(QUOTA_UNLIMITED),
            numa_bind_failures: AtomicU64::new(0),
            hook_slot: AtomicU8::new(0),
        }
    }
}

/// The slow-path (table-lock-guarded) descriptive half of an arena slot.
#[derive(Clone, Copy)]
struct ArenaMeta {
    label: Label,
    numa: NumaPolicy,
    decay: DecayConfig,
    huge: HugepagePolicy,
    cache_budget_bytes: u64,
    /// Delegating parent as `id + 1` (`0` ⇒ no parent / root arena).
    parent_plus1: u32,
    /// The parent's **incarnation generation** at delegation time (§36.13). On
    /// this arena's destroy, its reserved quota is returned to the parent only if
    /// the parent slot still holds that same incarnation — so a parent that was
    /// destroyed (and perhaps recreated at the same id) is never miscredited the
    /// budget of a child it never delegated. Meaningless when `parent_plus1 == 0`.
    parent_generation: u32,
    name: [u8; ARENA_NAME_LEN],
}

impl ArenaMeta {
    const fn empty() -> ArenaMeta {
        ArenaMeta {
            label: Label::PUBLIC,
            numa: NumaPolicy::OsDefault,
            decay: DecayConfig::DEFAULT,
            huge: HugepagePolicy::Default,
            cache_budget_bytes: 0,
            parent_plus1: 0,
            parent_generation: 0,
            name: [0u8; ARENA_NAME_LEN],
        }
    }
}

/// The shared head-state guarded by the table lock (the high-water id allocator).
struct TableInner {
    /// Next never-used slot id; ids `0..high_water` have been assigned at least
    /// once. Id `0` is the default arena, assigned at construction.
    high_water: u32,
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// The arena registry (§22, §36.4): a fixed-capacity table of arena authority +
/// lifecycle records. The default arena (id `0`) is present and `Active` from
/// construction (§35.4 phase 3, the ambient POSIX domain). Explicit arenas are
/// created with [`create`](Self::create) / [`delegate`](Self::delegate), driven
/// through their lifecycle with [`begin_reset`](Self::begin_reset) /
/// [`begin_destroy`](Self::begin_destroy), and accounted with the hot-path
/// [`try_charge`](Self::try_charge) / [`credit`](Self::credit).
///
/// **Concurrency.** Hot-path reads (the allocation gate
/// [`try_charge`](Self::try_charge) and [`credit`](Self::credit)) use only
/// per-slot atomics — no lock. Slow-path mutations (create,
/// delegate, configure, the lifecycle transitions) take the single table lock,
/// which also serializes id assignment and the descriptive-field writes. The
/// table is `Sync` because every field is either an atomic or guarded by the
/// lock.
pub struct ArenaTable {
    /// Lock serializing slow-path mutation (create/destroy/reset/configure) and
    /// guarding the descriptive `meta` fields and `inner`.
    lock: BackendLock,
    /// Per-slot hot-path atomics (lock-free reads).
    atomics: [ArenaAtomics; MAX_ARENAS],
    /// Per-slot descriptive fields. Guarded by `lock`.
    meta: core::cell::UnsafeCell<[ArenaMeta; MAX_ARENAS]>,
    /// Lock-guarded head-state.
    inner: core::cell::UnsafeCell<TableInner>,
}

// SAFETY: every hot-path field is an atomic; the `meta`/`inner` `UnsafeCell`s are
// only ever accessed while `lock` is held (the slow paths), so there is no data
// race. `ArenaMeta`/`TableInner` contain only plain Copy data.
unsafe impl Sync for ArenaTable {}
// SAFETY: the table owns its slots; there is no thread-affine state.
unsafe impl Send for ArenaTable {}

impl Default for ArenaTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ArenaTable {
    /// A fresh table with only the always-present default arena (id `0`),
    /// `Active` with ambient authority (§35.4 phase 3).
    pub fn new() -> ArenaTable {
        let table = ArenaTable {
            lock: BackendLock::new(),
            atomics: [const { ArenaAtomics::empty() }; MAX_ARENAS],
            meta: core::cell::UnsafeCell::new([const { ArenaMeta::empty() }; MAX_ARENAS]),
            inner: core::cell::UnsafeCell::new(TableInner { high_water: 1 }),
        };
        // Install the default arena (id 0) at generation 1, Active, ambient.
        let p = ArenaPolicy::ambient_default();
        let a = &table.atomics[ArenaId::DEFAULT.0 as usize];
        a.state.store(ArenaState::Active as u8, Ordering::Relaxed);
        a.rights.store(p.rights.bits(), Ordering::Relaxed);
        a.generation
            .store(Generation::FIRST.next().0, Ordering::Relaxed);
        a.quota_limit.store(p.quota_limit, Ordering::Relaxed);
        // SAFETY: no other thread can observe `table` before `new` returns, so the
        // single-threaded initialization of `meta[0]` needs no lock.
        unsafe {
            (*table.meta.get())[ArenaId::DEFAULT.0 as usize] = ArenaMeta {
                label: p.label,
                numa: p.numa,
                decay: p.decay,
                huge: p.huge,
                cache_budget_bytes: p.cache_budget_bytes,
                parent_plus1: 0,
                parent_generation: 0,
                name: p.name,
            };
        }
        table
    }

    /// The atomics slot for `arena`, or `None` if the id is out of range.
    #[inline]
    fn slot(&self, arena: ArenaId) -> Option<&ArenaAtomics> {
        self.atomics.get(arena.0 as usize)
    }

    /// Whether `arena` is registered (created and not in the unused/Destroyed
    /// state). The default arena is always registered.
    pub fn is_registered(&self, arena: ArenaId) -> bool {
        match self.slot(arena) {
            Some(a) => {
                ArenaState::from_u8(a.state.load(Ordering::Acquire)) != ArenaState::Destroyed
            }
            None => false,
        }
    }

    /// The current lifecycle state of `arena`, or `None` if out of range.
    pub fn state(&self, arena: ArenaId) -> Option<ArenaState> {
        self.slot(arena)
            .map(|a| ArenaState::from_u8(a.state.load(Ordering::Acquire)))
    }

    /// Whether `arena` is currently allocatable (registered **and** `Active`).
    /// The hot-path gate the allocator consults before serving a request.
    #[inline]
    pub fn is_active(&self, arena: ArenaId) -> bool {
        match self.slot(arena) {
            Some(a) => ArenaState::from_u8(a.state.load(Ordering::Acquire)).is_active(),
            None => false,
        }
    }

    // -- creation & delegation (§22.4 / §36.4) --------------------------------

    /// Create a new explicit arena from `policy` (§22.4). Validates the policy
    /// **first**, then claims a slot, writes the descriptive fields, and only
    /// **then** publishes the id as `Active` — so a half-initialized arena is
    /// never observable (§22.4 "publish arena ID only after complete
    /// initialization"). The new arena starts at a fresh generation (`> 0`).
    ///
    /// Returns [`ArenaError::Exhausted`] if every slot is live, or
    /// [`ArenaError::InvalidPolicy`] if the policy is malformed.
    ///
    /// SPEC-transition: arena create `Initializing → Active` (§22.4)
    pub fn create(&self, policy: &ArenaPolicy) -> Result<ArenaId, ArenaError> {
        policy.validate()?;
        self.lock.acquire();
        let result = self.create_locked(policy, 0, 0);
        self.lock.release();
        result
    }

    /// Delegate an attenuated child arena from `parent` (§36.4 / §36.14). The
    /// three §36.4 monotonicity invariants are enforced — and they are the Lean
    /// `DelegatesFrom` fields:
    ///
    /// * **authority monotonicity:** `child.rights ⊆ parent.rights`;
    /// * **quota monotonicity:** `child.quota ≤ parent.remaining_quota`;
    /// * **label monotonicity:** `child.label == parent.label` (no downgrade).
    ///
    /// A violation of any is [`ArenaError::Attenuation`] (§36.16 "delegation
    /// cannot widen rights/quota or downgrade label"). The parent must be a
    /// registered, non-draining arena.
    ///
    /// SPEC-transition: arena delegate (§36.4, attenuation-only)
    pub fn delegate(&self, parent: ArenaId, del: &Delegation) -> Result<ArenaId, ArenaError> {
        self.lock.acquire();
        let result = self.delegate_locked(parent, del);
        self.lock.release();
        result
    }

    /// `create`, with the table lock held. `parent_plus1 == 0` ⇒ a root arena;
    /// otherwise `parent_generation` is the parent's incarnation at delegation,
    /// recorded so the child's destroy returns its reserved quota only to that
    /// exact incarnation (§36.13).
    fn create_locked(
        &self,
        policy: &ArenaPolicy,
        parent_plus1: u32,
        parent_generation: u32,
    ) -> Result<ArenaId, ArenaError> {
        self.create_locked_inner(policy, parent_plus1, parent_generation, true)
    }

    /// `create_locked`, but with explicit publish control: `publish == false`
    /// leaves the arena `Initializing` (not yet allocatable), so a caller that must
    /// install per-arena state **before** the id is allocatable (§22.4 "install
    /// hooks before first extent allocation", plan 06 W10) can do so and then call
    /// [`publish`](Self::publish). The arena id is private to the caller until
    /// published, so the unpublished window is race-free.
    fn create_locked_inner(
        &self,
        policy: &ArenaPolicy,
        parent_plus1: u32,
        parent_generation: u32,
        publish: bool,
    ) -> Result<ArenaId, ArenaError> {
        let id = self.claim_slot_locked()?;
        let a = &self.atomics[id as usize];
        // Bump the *incarnation* generation off whatever the (possibly recycled)
        // slot carried, so a handle to a prior incarnation is detectably stale
        // (§B.5/§36.13). A fresh incarnation starts its reset generation at FIRST.
        let gen = Generation(a.generation.load(Ordering::Relaxed)).next();
        a.generation.store(gen.0, Ordering::Relaxed);
        a.reset_gen
            .store(Generation::FIRST.next().0, Ordering::Relaxed);
        a.rights.store(policy.rights.bits(), Ordering::Relaxed);
        // A fresh incarnation commits and reserves nothing.
        a.committed.store(0, Ordering::Relaxed);
        a.reserved.store(0, Ordering::Relaxed);
        a.quota_limit.store(policy.quota_limit, Ordering::Relaxed);
        a.numa_bind_failures.store(0, Ordering::Relaxed);
        // A fresh incarnation has no hooked backing until one is registered (W10).
        a.hook_slot.store(0, Ordering::Relaxed);
        // Initializing: descriptive fields written before the id is published.
        a.state
            .store(ArenaState::Initializing as u8, Ordering::Relaxed);
        // SAFETY: the table lock is held, so this is the exclusive writer of
        // `meta[id]`.
        unsafe {
            (*self.meta.get())[id as usize] = ArenaMeta {
                label: policy.label,
                numa: policy.numa,
                decay: policy.decay,
                huge: policy.huge,
                cache_budget_bytes: policy.cache_budget_bytes,
                parent_plus1,
                parent_generation,
                name: policy.name,
            };
        }
        // Publish: the release store pairs with the acquire loads in `is_active`
        // / `try_charge`, so a thread that sees `Active` also sees the fields. When
        // `publish` is false the arena stays `Initializing` until [`publish`].
        if publish {
            a.state.store(ArenaState::Active as u8, Ordering::Release);
        }
        Ok(ArenaId(id))
    }

    /// Create an arena left in `Initializing` (not yet allocatable), returning its
    /// id (private to the caller). The caller installs any per-arena state, then
    /// calls [`publish`](Self::publish) to make it `Active`, or
    /// [`abandon_pending`](Self::abandon_pending) to discard it (plan 06 W10).
    ///
    /// SPEC-transition: arena create (pending half, §22.4)
    pub fn create_pending(&self, policy: &ArenaPolicy) -> Result<ArenaId, ArenaError> {
        policy.validate()?;
        self.lock.acquire();
        let r = self.create_locked_inner(policy, 0, 0, false);
        self.lock.release();
        r
    }

    /// Publish a [`create_pending`](Self::create_pending) arena: `Initializing →
    /// Active`. Rejects an arena not in `Initializing`.
    ///
    /// SPEC-transition: arena create (publish half, §22.4)
    pub fn publish(&self, arena: ArenaId) -> Result<(), ArenaError> {
        let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
        if ArenaState::from_u8(a.state.load(Ordering::Acquire)) != ArenaState::Initializing {
            return Err(ArenaError::IllegalTransition);
        }
        a.state.store(ArenaState::Active as u8, Ordering::Release);
        Ok(())
    }

    /// Discard a [`create_pending`](Self::create_pending) arena whose initialization
    /// failed: `Initializing → Destroyed` with a generation bump, releasing the slot
    /// (§B.5). Rejects an arena not in `Initializing`.
    pub fn abandon_pending(&self, arena: ArenaId) -> Result<(), ArenaError> {
        self.lock.acquire();
        let r = (|| {
            let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
            if ArenaState::from_u8(a.state.load(Ordering::Acquire)) != ArenaState::Initializing {
                return Err(ArenaError::IllegalTransition);
            }
            let gen = Generation(a.generation.load(Ordering::Relaxed)).next();
            a.generation.store(gen.0, Ordering::Relaxed);
            a.rights.store(0, Ordering::Relaxed);
            a.state
                .store(ArenaState::Destroyed as u8, Ordering::Release);
            Ok(())
        })();
        self.lock.release();
        r
    }

    /// `delegate`, with the table lock held.
    fn delegate_locked(&self, parent: ArenaId, del: &Delegation) -> Result<ArenaId, ArenaError> {
        let pstats = self.stats_locked(parent).ok_or(ArenaError::NotFound)?;
        // A draining/destroyed parent cannot delegate (§36.13 "reject new
        // delegations" while draining).
        if !pstats.state.is_active() {
            return Err(ArenaError::NotActive);
        }
        // §36.4 authority monotonicity.
        if !del.rights.attenuates(pstats.rights) {
            return Err(ArenaError::Attenuation);
        }
        // §36.4 quota monotonicity (child ≤ parent's *remaining* budget).
        if del.quota_limit > pstats.remaining_quota() {
            return Err(ArenaError::Attenuation);
        }
        // §36.4 label monotonicity (no silent downgrade).
        if del.label != pstats.label {
            return Err(ArenaError::Attenuation);
        }
        // The child inherits the delegated authority/label/quota/NUMA; the
        // non-authority placement knobs (decay/hugepage/cache budget) start at
        // their defaults and may be tuned later with `configure`.
        let policy = ArenaPolicy {
            rights: del.rights,
            label: del.label,
            quota_limit: del.quota_limit,
            numa: del.numa,
            decay: DecayConfig::DEFAULT,
            huge: HugepagePolicy::Default,
            cache_budget_bytes: 0,
            latency: LatencyClass::MayBlock,
            name: del.name,
        };
        policy.validate()?;
        // §36.4 budget partition: reserve the child's quota on the parent, so the
        // parent's own allocations *and* any further delegations cannot, together
        // with this child, exceed the parent's quota — the runtime image of the
        // tree-wide bound proved in the Lean bridge (`subtree_used_le_quota`).
        // Reservation bites only a *finite* parent: an unlimited parent has no
        // budget to partition, and a finite parent cannot delegate an unlimited
        // child (the remaining-quota check above rejects that), so the reserved
        // amount is always finite here. The reserve runs through the same gated
        // CAS the hot path uses, so a concurrent own allocation that consumed the
        // budget after the snapshot check is caught — never silently overrun.
        let parent_finite = pstats.quota_limit != QUOTA_UNLIMITED;
        if parent_finite {
            self.reserve_child_quota_locked(parent, del.quota_limit)?;
        }
        match self.create_locked(&policy, parent.0 + 1, pstats.generation.0) {
            Ok(child) => Ok(child),
            Err(e) => {
                // A failed create must leak no reservation: give the budget back.
                if parent_finite {
                    self.release_child_quota_locked(parent, del.quota_limit);
                }
                Err(e)
            }
        }
    }

    /// Claim a free slot id (lock held). Prefers a never-used high-water slot;
    /// falls back to recycling a [`Destroyed`](ArenaState::Destroyed) slot under
    /// capacity pressure (its generation is bumped on reuse, §B.5). Never
    /// reuses the default arena's slot.
    fn claim_slot_locked(&self) -> Result<u32, ArenaError> {
        // SAFETY: lock held ⇒ exclusive access to `inner`.
        let inner = unsafe { &mut *self.inner.get() };
        if (inner.high_water as usize) < MAX_ARENAS {
            let id = inner.high_water;
            inner.high_water += 1;
            return Ok(id);
        }
        // High-water exhausted: recycle a Destroyed (and thus stale-only) slot.
        for id in 1..MAX_ARENAS as u32 {
            let st = ArenaState::from_u8(self.atomics[id as usize].state.load(Ordering::Acquire));
            if st == ArenaState::Destroyed {
                return Ok(id);
            }
        }
        Err(ArenaError::Exhausted)
    }

    // -- lifecycle transitions (§22.5 / §36.13) -------------------------------

    /// Begin a reset (`Active → Resetting`, §22.5). Rejects the default arena
    /// ([`ArenaError::IsDefault`] — §22.5 "not the default automatic arena") and
    /// any arena not currently `Active` ([`ArenaError::IllegalTransition`]). New
    /// allocations are refused while `Resetting`. The caller then drains the
    /// arena's caches/extents and calls [`finish_reset`](Self::finish_reset).
    ///
    /// SPEC-transition: arena `Active → Resetting` (§22.5)
    pub fn begin_reset(&self, arena: ArenaId) -> Result<(), ArenaError> {
        if arena == ArenaId::DEFAULT {
            return Err(ArenaError::IsDefault);
        }
        self.transition(arena, ArenaState::Resetting)
    }

    /// Complete a reset (`Resetting → Active`, §22.5): zero the used counter,
    /// bump the **reset** generation (so a stale pointer into the reset arena is
    /// detectable), and return the arena to service. The **incarnation**
    /// generation is left unchanged — the arena persists, so a handle to it
    /// survives the reset. The caller MUST have drained the arena's objects first
    /// (B.5). Returns the new reset generation.
    ///
    /// SPEC-transition: arena `Resetting → Active` (§22.5)
    pub fn finish_reset(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        self.lock.acquire();
        let r = self.finish_reset_locked(arena);
        self.lock.release();
        r
    }

    fn finish_reset_locked(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
        let cur = ArenaState::from_u8(a.state.load(Ordering::Acquire));
        if !cur.can_transition(ArenaState::Active) {
            return Err(ArenaError::IllegalTransition);
        }
        // Reset discards this arena's *own* objects (own-live → 0) but the arena
        // and any delegated children persist, so its reserved child-quota stays:
        // committed drops to exactly `reserved`. Quiescence + the `Resetting` state
        // block `try_charge`, so committed is stable under this store.
        a.committed
            .store(a.reserved.load(Ordering::Relaxed), Ordering::Relaxed);
        let reset_gen = Generation(a.reset_gen.load(Ordering::Relaxed)).next();
        a.reset_gen.store(reset_gen.0, Ordering::Relaxed);
        a.state.store(ArenaState::Active as u8, Ordering::Release);
        Ok(reset_gen)
    }

    /// Begin a destroy (`Active → Draining`, §36.13 step 1): reject new
    /// allocations and delegations. Rejects the default arena and a non-`Active`
    /// arena. The caller then runs the [`RevocationPhase`] protocol and calls
    /// [`finish_destroy`](Self::finish_destroy) (success) or
    /// [`quarantine`](Self::quarantine) (partial failure).
    ///
    /// SPEC-transition: arena `Active → Draining` (§36.13)
    pub fn begin_destroy(&self, arena: ArenaId) -> Result<(), ArenaError> {
        if arena == ArenaId::DEFAULT {
            return Err(ArenaError::IsDefault);
        }
        self.transition(arena, ArenaState::Draining)
    }

    /// Complete a destroy (`Draining → Destroyed`, §36.13 step 7): bump the
    /// generation and retire the id. The slot becomes available for a future
    /// create behind that generation bump (§B.5 id non-reuse-while-stale). The
    /// caller MUST have completed every revocation phase first.
    ///
    /// SPEC-transition: arena `Draining → Destroyed` (§36.13)
    pub fn finish_destroy(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        self.lock.acquire();
        let r = self.finish_destroy_locked(arena);
        self.lock.release();
        r
    }

    fn finish_destroy_locked(&self, arena: ArenaId) -> Result<Generation, ArenaError> {
        let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
        let cur = ArenaState::from_u8(a.state.load(Ordering::Acquire));
        if !cur.can_transition(ArenaState::Destroyed) {
            return Err(ArenaError::IllegalTransition);
        }
        // Return this arena's reserved quota to its delegating parent before its
        // own accounting is cleared (§36.4), so destroying a child frees the
        // parent's budget for reuse. Generation-checked, so a parent that was
        // itself destroyed/recycled is never miscredited.
        self.return_reservation_to_parent_locked(arena);
        a.committed.store(0, Ordering::Relaxed);
        a.reserved.store(0, Ordering::Relaxed);
        a.rights.store(0, Ordering::Relaxed);
        let gen = Generation(a.generation.load(Ordering::Relaxed)).next();
        a.generation.store(gen.0, Ordering::Relaxed);
        a.state
            .store(ArenaState::Destroyed as u8, Ordering::Release);
        Ok(gen)
    }

    /// Quarantine an arena after a **partial failure** during reset or destroy
    /// (`Resetting|Draining → ErrorQuarantined`, §36.13): the arena stops safely,
    /// never reaching `Destroyed`. Allocations stay refused. This is the §36.13
    /// "partial failure MUST leave DRAINING or ERROR_QUARANTINED, not DESTROYED".
    ///
    /// The used counter is zeroed: a quarantine is reached only *after* the drain
    /// has deactivated/retired every object (the partial failure is that some
    /// *backing* could not be recycled, not that objects survived), so no live
    /// bytes remain charged — only leaked backing pending operator recovery.
    ///
    /// SPEC-transition: arena `* → ErrorQuarantined` (§36.13)
    pub fn quarantine(&self, arena: ArenaId) -> Result<(), ArenaError> {
        self.lock.acquire();
        let r = (|| {
            let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
            let cur = ArenaState::from_u8(a.state.load(Ordering::Acquire));
            if !cur.can_transition(ArenaState::ErrorQuarantined) {
                return Err(ArenaError::IllegalTransition);
            }
            // The drain retired every object (own-live → 0), but a quarantine is
            // not a destroy: the arena persists pending operator recovery, so its
            // reserved child-quota is kept and committed drops to exactly
            // `reserved` (own-live zero — the terminal-state invariant in
            // `check_invariants`).
            a.committed
                .store(a.reserved.load(Ordering::Relaxed), Ordering::Relaxed);
            a.state
                .store(ArenaState::ErrorQuarantined as u8, Ordering::Release);
            Ok(())
        })();
        self.lock.release();
        r
    }

    /// Apply a single guarded lifecycle transition under the lock.
    fn transition(&self, arena: ArenaId, to: ArenaState) -> Result<(), ArenaError> {
        self.lock.acquire();
        let r = (|| {
            let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
            let cur = ArenaState::from_u8(a.state.load(Ordering::Acquire));
            if !cur.can_transition(to) {
                return Err(ArenaError::IllegalTransition);
            }
            a.state.store(to as u8, Ordering::Release);
            Ok(())
        })();
        self.lock.release();
        r
    }

    // -- accounting (hot path, §36.17) ----------------------------------------

    /// Charge `size` bytes against `arena`'s quota (§36.4 / §36.17), the
    /// allocation gate. Rejects when the arena is not `Active`
    /// ([`ArenaError::NotActive`]), lacks the [`CapRights::ALLOC`] right
    /// ([`ArenaError::AuthorityDenied`]), or the charge would push the
    /// **committed** total (own bytes + reserved child quota) past the quota or
    /// overflow ([`ArenaError::QuotaExceeded`]). Lock-free: a state/rights load
    /// plus a compare-exchange on `committed` — the same atomic a delegation's
    /// reservation contends on, so own bytes and reserved budget can never jointly
    /// exceed the ceiling even under concurrency.
    ///
    /// SPEC-transition: arena quota charge (§36.4/§36.17)
    pub fn try_charge(&self, arena: ArenaId, size: u64) -> Result<(), ArenaError> {
        let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
        if !ArenaState::from_u8(a.state.load(Ordering::Acquire)).is_active() {
            return Err(ArenaError::NotActive);
        }
        let rights = CapRights(a.rights.load(Ordering::Relaxed));
        if !rights.contains(CapRights::ALLOC) {
            return Err(ArenaError::AuthorityDenied);
        }
        let limit = a.quota_limit.load(Ordering::Relaxed);
        let mut cur = a.committed.load(Ordering::Acquire);
        loop {
            let next = cur.checked_add(size).ok_or(ArenaError::QuotaExceeded)?;
            if next > limit {
                return Err(ArenaError::QuotaExceeded);
            }
            match a
                .committed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(observed) => cur = observed,
            }
        }
    }

    /// Credit `size` bytes of **own** live storage back to `arena` on free
    /// (§36.17): lowers `committed` (own bytes + reserved), saturating at zero so
    /// a stale/double free can never underflow the counter, and never touching the
    /// reserved portion (a child's quota is returned only by the child's destroy,
    /// not by freeing the parent's own objects). A no-op for an unregistered id
    /// (the free path tolerates a torn arena read). Does **not** require the arena
    /// be `Active`: bytes are credited as objects are reclaimed during draining too.
    ///
    /// SPEC-transition: arena quota credit (§36.17)
    pub fn credit(&self, arena: ArenaId, size: u64) {
        let Some(a) = self.slot(arena) else {
            return;
        };
        let mut cur = a.committed.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_sub(size);
            match a
                .committed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Reserve `amount` of `parent`'s quota for a child being delegated (§36.4
    /// budget partition). Called only under the table lock, but contends on
    /// `committed` through the **same** gated compare-exchange the lock-free
    /// [`try_charge`](Self::try_charge) uses, so the reservation and any racing own
    /// allocation jointly respect `committed ≤ quota_limit`. Returns
    /// [`ArenaError::Attenuation`] if the parent's budget cannot absorb the
    /// reservation (a concurrent allocation consumed it after the caller's
    /// snapshot check) — the same monotonicity failure the snapshot check reports.
    fn reserve_child_quota_locked(&self, parent: ArenaId, amount: u64) -> Result<(), ArenaError> {
        let a = self.slot(parent).ok_or(ArenaError::NotFound)?;
        let limit = a.quota_limit.load(Ordering::Relaxed);
        let mut cur = a.committed.load(Ordering::Acquire);
        loop {
            let next = cur.checked_add(amount).ok_or(ArenaError::Attenuation)?;
            if next > limit {
                return Err(ArenaError::Attenuation);
            }
            match a
                .committed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    // `reserved` is only ever mutated under the table lock (here
                    // and in the release below), so a plain add is race-free.
                    a.reserved.fetch_add(amount, Ordering::Relaxed);
                    return Ok(());
                }
                Err(observed) => cur = observed,
            }
        }
    }

    /// Return `amount` of reserved quota to `parent` (§36.4): a child's destroy
    /// gives its reservation back, or a failed delegation rolls its reservation
    /// back. Lowers both `committed` and `reserved`, saturating at zero so a torn
    /// read can never underflow. `committed` uses the gated compare-exchange
    /// (`try_charge` may be racing on the parent); `reserved` is lock-held.
    fn release_child_quota_locked(&self, parent: ArenaId, amount: u64) {
        let Some(a) = self.slot(parent) else {
            return;
        };
        let mut cur = a.committed.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_sub(amount);
            match a
                .committed
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
        let mut r = a.reserved.load(Ordering::Relaxed);
        loop {
            let next = r.saturating_sub(amount);
            match a
                .reserved
                .compare_exchange_weak(r, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => r = observed,
            }
        }
    }

    /// Give a destroyed child's reserved quota back to its parent (§36.4). The
    /// amount is the child's own `quota_limit` — exactly what [`delegate`] reserved
    /// on the parent. A no-op for a root arena (no parent), for an unlimited parent
    /// (which reserved nothing), or for a parent that has since been
    /// destroyed/recycled: the generation check ensures only the *exact*
    /// incarnation that granted the delegation is credited, so a recycled slot's
    /// new occupant is never handed budget it never lent. Called under the table
    /// lock from [`finish_destroy_locked`](Self::finish_destroy_locked).
    fn return_reservation_to_parent_locked(&self, child: ArenaId) {
        // SAFETY: the table lock is held by every caller (finish_destroy).
        let m = unsafe { (*self.meta.get())[child.0 as usize] };
        if m.parent_plus1 == 0 {
            return; // root arena: nothing was reserved on a parent
        }
        let parent = ArenaId(m.parent_plus1 - 1);
        let Some(pa) = self.slot(parent) else {
            return;
        };
        // An unlimited parent partitions no budget, so it reserved nothing.
        if pa.quota_limit.load(Ordering::Relaxed) == QUOTA_UNLIMITED {
            return;
        }
        // Credit only the exact incarnation that delegated this child; a
        // destroyed/recreated parent (generation bumped) is already gone.
        if pa.generation.load(Ordering::Relaxed) != m.parent_generation {
            return;
        }
        let Some(ca) = self.slot(child) else {
            return;
        };
        let amount = ca.quota_limit.load(Ordering::Relaxed);
        // A finite parent never had an unlimited child reserved (delegation
        // rejects that); guard anyway so an impossible torn read can't mis-return.
        if amount == QUOTA_UNLIMITED {
            return;
        }
        self.release_child_quota_locked(parent, amount);
    }

    /// Record a NUMA binding failure for `arena` (§15.5 "NUMA binding failures
    /// MUST be visible in stats"). Surfaced in [`ArenaStats::numa_bind_failures`].
    pub fn record_numa_bind_failure(&self, arena: ArenaId) {
        if let Some(a) = self.slot(arena) {
            a.numa_bind_failures.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `arena`'s hooked-backing registry slot **plus one** (`0` ⇒ none), read
    /// lock-free for the W10 routing fast path. Returns `0` for an out-of-range id.
    pub fn hook_slot_of(&self, arena: ArenaId) -> u8 {
        self.slot(arena)
            .map_or(0, |a| a.hook_slot.load(Ordering::Acquire))
    }

    /// Record that `arena`'s hooked backing lives in registry slot `slot_plus1 − 1`
    /// (`0` clears it). A single lock-free store; the caller orders it before the
    /// registry `count` release (create) or after taking the backing out (teardown),
    /// so a reader that has observed `count` also observes this. A no-op for an
    /// out-of-range id.
    pub fn set_hook_slot(&self, arena: ArenaId, slot_plus1: u8) {
        if let Some(a) = self.slot(arena) {
            a.hook_slot.store(slot_plus1, Ordering::Release);
        }
    }

    // -- introspection --------------------------------------------------------

    /// A snapshot of `arena`'s authority + accounting, or `None` if the id is out
    /// of range. The default and any registered arena resolve; an unused slot
    /// reads as `Destroyed`.
    pub fn stats(&self, arena: ArenaId) -> Option<ArenaStats> {
        self.lock.acquire();
        let s = self.stats_locked(arena);
        self.lock.release();
        s
    }

    fn stats_locked(&self, arena: ArenaId) -> Option<ArenaStats> {
        let a = self.slot(arena)?;
        // SAFETY: the table lock is held by every caller of `stats_locked`.
        let m = unsafe { (*self.meta.get())[arena.0 as usize] };
        Some(ArenaStats {
            id: arena,
            state: ArenaState::from_u8(a.state.load(Ordering::Acquire)),
            rights: CapRights(a.rights.load(Ordering::Relaxed)),
            label: m.label,
            quota_limit: a.quota_limit.load(Ordering::Relaxed),
            // The §36.17 own-live counter is committed minus reserved
            // child-quota; reservations are budget holds, not live bytes, so
            // `Σ used == live_bytes` still reconciles (§8.6).
            used: a
                .committed
                .load(Ordering::Relaxed)
                .saturating_sub(a.reserved.load(Ordering::Relaxed)),
            reserved: a.reserved.load(Ordering::Relaxed),
            generation: Generation(a.generation.load(Ordering::Relaxed)),
            reset_generation: Generation(a.reset_gen.load(Ordering::Relaxed)),
            numa: m.numa,
            decay: m.decay,
            huge: m.huge,
            cache_budget_bytes: m.cache_budget_bytes,
            numa_bind_failures: a.numa_bind_failures.load(Ordering::Relaxed),
            // Filled in by `Allocator::arena_stats` for a hooked arena (the table has
            // no access to the providers); `None` here means "no hooked backing".
            hooks: None,
            parent: if m.parent_plus1 == 0 {
                None
            } else {
                Some(ArenaId(m.parent_plus1 - 1))
            },
            name: m.name,
        })
    }

    /// Reconfigure `arena`'s non-authority policy (§22.4 *configure*, F-005): its
    /// decay timing, hugepage policy, NUMA preference, soft cache budget, and
    /// name. The authority (rights/label) and the quota ceiling are **not**
    /// changeable here — those are fixed at create/delegate time so a configure
    /// can never widen authority (§36.4). The arena must be registered (any
    /// non-`Destroyed` state); reconfiguring a draining/resetting arena is
    /// permitted (it only adjusts inert policy knobs).
    ///
    /// SPEC-transition: arena configure (§22.4)
    pub fn configure(&self, arena: ArenaId, cfg: &ArenaConfig) -> Result<(), ArenaError> {
        self.lock.acquire();
        let r = (|| {
            let a = self.slot(arena).ok_or(ArenaError::NotFound)?;
            if ArenaState::from_u8(a.state.load(Ordering::Acquire)) == ArenaState::Destroyed {
                return Err(ArenaError::NotFound);
            }
            // SAFETY: the table lock is held, so this is the exclusive writer of
            // `meta[arena]` (the authority atomics are untouched).
            unsafe {
                let m = &mut (*self.meta.get())[arena.0 as usize];
                m.decay = cfg.decay;
                m.huge = cfg.huge;
                m.numa = cfg.numa;
                m.cache_budget_bytes = cfg.cache_budget_bytes;
                m.name = cfg.name;
            }
            Ok(())
        })();
        self.lock.release();
        r
    }

    // -- handles (§36.13/§36.14: generation-checked arena references) ----------

    /// A generation-checked **handle** to `arena`'s current incarnation, or
    /// `None` if the id is unregistered. The handle packs `(incarnation
    /// generation << 32) | id`; [`resolve_handle`](Self::resolve_handle) rejects
    /// it once the id is destroyed-and-recreated (a new incarnation, §36.13). `0`
    /// is never a valid handle (the default arena's generation is `≥ 1`).
    pub fn handle(&self, arena: ArenaId) -> Option<u64> {
        let s = self.stats(arena)?;
        if s.state == ArenaState::Destroyed {
            return None;
        }
        Some(((s.generation.0 as u64) << 32) | arena.0 as u64)
    }

    /// Resolve a [`handle`](Self::handle) to its [`ArenaId`], or `None` if the
    /// handle is **stale** — its id was destroyed (and possibly recreated as a
    /// new incarnation), so the packed generation no longer matches the live one
    /// (§36.13 "a stale reference MUST NOT become valid for a new arena"). A
    /// handle survives resets of its own arena (the incarnation is unchanged).
    pub fn resolve_handle(&self, handle: u64) -> Option<ArenaId> {
        let id = ArenaId((handle & 0xffff_ffff) as u32);
        let gen = (handle >> 32) as u32;
        let s = self.stats(id)?;
        if s.state != ArenaState::Destroyed && s.generation.0 == gen {
            Some(id)
        } else {
            None
        }
    }

    /// Total NUMA binding failures across every arena (§15.5 stats visibility,
    /// W9-7) — the aggregate surfaced in the engine's stats snapshot.
    pub fn total_numa_bind_failures(&self) -> u64 {
        self.lock.acquire();
        // SAFETY: lock held.
        let hw = unsafe { (*self.inner.get()).high_water } as usize;
        self.lock.release();
        (0..hw)
            .map(|i| self.atomics[i].numa_bind_failures.load(Ordering::Relaxed))
            .sum()
    }

    /// The number of currently-registered arenas (including the default),
    /// counting every slot not in the unused/`Destroyed` state.
    pub fn live_count(&self) -> usize {
        self.lock.acquire();
        // SAFETY: lock held.
        let hw = unsafe { (*self.inner.get()).high_water } as usize;
        self.lock.release();
        (0..hw)
            .filter(|&i| {
                ArenaState::from_u8(self.atomics[i].state.load(Ordering::Acquire))
                    != ArenaState::Destroyed
            })
            .count()
    }

    /// Whether the table is well-formed (Appendix B.5 — the debug/test oracle):
    ///
    /// * the default arena is always registered and never `Destroyed` (it is the
    ///   ambient domain that must always be allocatable);
    /// * every registered arena's `used` never exceeds its quota (§36.4 quota
    ///   accounting);
    /// * a **terminal** arena (`Destroyed` or `ErrorQuarantined`) holds zero used
    ///   bytes — B.5 "no live objects remain in arena accounting" after teardown.
    ///   A quarantine is reached only after the drain has retired every object
    ///   (the partial failure is leaked *backing*, not surviving objects), so its
    ///   live-byte accounting is exact too, and `Σ arena.used == live_bytes` keeps
    ///   reconciling even across a quarantine (§8.6/§36.17).
    pub fn check_invariants(&self) -> bool {
        self.lock.acquire();
        // SAFETY: lock held.
        let hw = unsafe { (*self.inner.get()).high_water } as usize;
        self.lock.release();
        // The default arena must be present and active-or-resetting (never gone).
        if !self.is_registered(ArenaId::DEFAULT) {
            return false;
        }
        for i in 0..hw {
            let a = &self.atomics[i];
            let st = ArenaState::from_u8(a.state.load(Ordering::Acquire));
            let committed = a.committed.load(Ordering::Relaxed);
            let reserved = a.reserved.load(Ordering::Relaxed);
            let limit = a.quota_limit.load(Ordering::Relaxed);
            // The gate invariant: committed (own bytes + reserved child quota)
            // never exceeds the quota (§36.4). Every committing op checks this
            // before its compare-exchange, so it holds even mid-update.
            if committed > limit {
                return false;
            }
            // A terminal arena holds zero *own* live bytes (own = committed −
            // reserved): the drain retired every object before reaching
            // Destroyed/ErrorQuarantined. A quarantined arena's reservations
            // persist (it is not destroyed), so we check own-live, not committed.
            if st.is_terminal() && committed.saturating_sub(reserved) != 0 {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caprights_lattice_attenuation() {
        // ALL contains everything; NONE attenuates everything.
        assert!(CapRights::ALL.contains(CapRights::ALLOC));
        assert!(CapRights::ALL.contains(CapRights::DESTROY));
        assert!(CapRights::NONE.attenuates(CapRights::ALL));
        assert!(CapRights::ALLOC.attenuates(CapRights::ALL));
        // ALLOC|FREE attenuates ALL, but ALL does not attenuate ALLOC.
        let af = CapRights::ALLOC.union(CapRights::FREE);
        assert!(af.attenuates(CapRights::ALL));
        assert!(!CapRights::ALL.attenuates(af));
        // attenuation is reflexive.
        assert!(af.attenuates(af));
        // Unknown bits are rejected.
        assert!(CapRights::from_bits(0b1_0000).is_none());
        assert_eq!(CapRights::from_bits(0b1111), Some(CapRights::ALL));
    }

    #[test]
    fn state_machine_is_exactly_the_spec_graph() {
        use ArenaState::*;
        // The complete legal edge set (§22.3 + §36.13).
        let legal = [
            (Initializing, Active),
            (Active, Resetting),
            (Resetting, Active),
            (Active, Draining),
            (Draining, Destroyed),
            (Resetting, ErrorQuarantined),
            (Draining, ErrorQuarantined),
        ];
        let all = [
            Initializing,
            Active,
            Resetting,
            Draining,
            Destroyed,
            ErrorQuarantined,
        ];
        for &from in &all {
            for &to in &all {
                let should = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition(to),
                    should,
                    "transition {from:?} -> {to:?}"
                );
            }
        }
        // Allocations only in Active; terminals are terminal.
        assert!(Active.is_active());
        for s in [
            Initializing,
            Resetting,
            Draining,
            Destroyed,
            ErrorQuarantined,
        ] {
            assert!(!s.is_active());
        }
        assert!(Destroyed.is_terminal() && ErrorQuarantined.is_terminal());
        // §36.13: a partial failure can never jump straight to Destroyed.
        assert!(!ErrorQuarantined.can_transition(Destroyed));
        assert!(!Resetting.can_transition(Destroyed));
    }

    #[test]
    fn revocation_phases_are_a_linear_chain_unmap_before_revoke_before_recycle() {
        use RevocationPhase::*;
        // The single linear protocol; the ordering IS its acyclicity (DD-3).
        let chain = [Draining, DrainCaches, Unmap, Revoke, Recycle, Finalize];
        for w in chain.windows(2) {
            assert_eq!(w[0].next(), Some(w[1]));
        }
        assert_eq!(Finalize.next(), None);
        // Unmap precedes Revoke precedes Recycle (the cardinal §36.13 order).
        let pos = |p: RevocationPhase| chain.iter().position(|&q| q == p).unwrap();
        assert!(pos(Unmap) < pos(Revoke));
        assert!(pos(Revoke) < pos(Recycle));
    }

    #[test]
    fn default_arena_is_present_active_and_ambient() {
        let t = ArenaTable::new();
        assert!(t.is_registered(ArenaId::DEFAULT));
        assert!(t.is_active(ArenaId::DEFAULT));
        let s = t.stats(ArenaId::DEFAULT).unwrap();
        assert_eq!(s.rights, CapRights::ALL);
        assert_eq!(s.label, Label::PUBLIC);
        assert_eq!(s.quota_limit, QUOTA_UNLIMITED);
        assert_eq!(s.used, 0);
        assert_eq!(t.live_count(), 1);
        assert!(t.check_invariants());
    }

    #[test]
    fn create_validates_and_publishes_only_after_init() {
        let t = ArenaTable::new();
        // A zero quota is rejected (§22.4 validate-first).
        assert_eq!(
            t.create(&ArenaPolicy::explicit().with_quota(0)),
            Err(ArenaError::InvalidPolicy)
        );
        let id = t
            .create(
                &ArenaPolicy::explicit()
                    .with_quota(4096)
                    .with_name("scratch"),
            )
            .unwrap();
        assert_ne!(id, ArenaId::DEFAULT);
        assert!(t.is_active(id));
        let s = t.stats(id).unwrap();
        assert_eq!(s.quota_limit, 4096);
        assert_eq!(&s.name[..7], b"scratch");
        assert!(s.generation.0 > 0, "a fresh arena has a nonzero generation");
        assert_eq!(t.live_count(), 2);
    }

    #[test]
    fn quota_charge_and_credit_cannot_wrap() {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit().with_quota(1000)).unwrap();
        t.try_charge(id, 600).unwrap();
        assert_eq!(t.stats(id).unwrap().used, 600);
        // Over the ceiling: rejected, used unchanged.
        assert_eq!(t.try_charge(id, 500), Err(ArenaError::QuotaExceeded));
        assert_eq!(t.stats(id).unwrap().used, 600);
        // Exactly fits.
        t.try_charge(id, 400).unwrap();
        assert_eq!(t.stats(id).unwrap().used, 1000);
        assert_eq!(t.stats(id).unwrap().remaining_quota(), 0);
        // Credit saturates: crediting more than is charged floors at zero.
        t.credit(id, 5000);
        assert_eq!(t.stats(id).unwrap().used, 0);
        // Overflow charge is QuotaExceeded, never a wrap.
        let big = t.create(&ArenaPolicy::explicit()).unwrap(); // unlimited
        t.try_charge(big, u64::MAX - 10).unwrap();
        assert_eq!(t.try_charge(big, 100), Err(ArenaError::QuotaExceeded));
        assert!(t.check_invariants());
    }

    #[test]
    fn allocation_gate_rejects_non_active_and_no_alloc_right() {
        let t = ArenaTable::new();
        // No ALLOC right ⇒ AuthorityDenied (§36.4 "clients cannot allocate
        // without rights").
        let ro = t
            .create(&ArenaPolicy::explicit().with_rights(CapRights::STATS))
            .unwrap();
        assert_eq!(t.try_charge(ro, 16), Err(ArenaError::AuthorityDenied));
        // A draining arena rejects new allocations (§36.13).
        let id = t.create(&ArenaPolicy::explicit()).unwrap();
        t.begin_destroy(id).unwrap();
        assert_eq!(t.try_charge(id, 16), Err(ArenaError::NotActive));
        assert!(!t.is_active(id));
        // Out-of-range id ⇒ NotFound.
        assert_eq!(t.try_charge(ArenaId(9999), 16), Err(ArenaError::NotFound));
    }

    #[test]
    fn delegation_is_attenuation_only() {
        let t = ArenaTable::new();
        let parent = t
            .create(
                &ArenaPolicy::explicit()
                    .with_quota(1000)
                    .with_rights(CapRights::ALLOC.union(CapRights::FREE))
                    .with_label(Label(7)),
            )
            .unwrap();
        let pstats = t.stats(parent).unwrap();

        // A faithful attenuation succeeds.
        let child = t
            .delegate(
                parent,
                &Delegation::inheriting(&pstats, 400, "child").with_rights(CapRights::ALLOC),
            )
            .unwrap();
        let cs = t.stats(child).unwrap();
        assert_eq!(cs.rights, CapRights::ALLOC);
        assert_eq!(cs.quota_limit, 400);
        assert_eq!(cs.label, Label(7));
        assert_eq!(cs.parent, Some(parent));

        // Widening rights is rejected (authority monotonicity).
        assert_eq!(
            t.delegate(
                parent,
                &Delegation::inheriting(&pstats, 100, "x").with_rights(CapRights::DESTROY)
            ),
            Err(ArenaError::Attenuation)
        );
        // Quota beyond the parent's remaining budget is rejected.
        assert_eq!(
            t.delegate(parent, &Delegation::inheriting(&pstats, 1001, "x")),
            Err(ArenaError::Attenuation)
        );
        // Downgrading the label is rejected (label monotonicity).
        let mut bad = Delegation::inheriting(&pstats, 100, "x");
        bad.label = Label(0);
        assert_eq!(t.delegate(parent, &bad), Err(ArenaError::Attenuation));
    }

    #[test]
    fn quota_monotonicity_uses_remaining_not_total() {
        let t = ArenaTable::new();
        let parent = t.create(&ArenaPolicy::explicit().with_quota(1000)).unwrap();
        t.try_charge(parent, 700).unwrap(); // 300 remaining
        let pstats = t.stats(parent).unwrap();
        assert_eq!(pstats.remaining_quota(), 300);
        // A child up to the remaining budget is fine; beyond it is rejected.
        assert!(t
            .delegate(parent, &Delegation::inheriting(&pstats, 300, "ok"))
            .is_ok());
        assert_eq!(
            t.delegate(parent, &Delegation::inheriting(&pstats, 301, "no")),
            Err(ArenaError::Attenuation)
        );
    }

    #[test]
    fn delegation_reserves_parent_quota_so_the_subtree_cannot_over_commit() {
        // The PR #13 P1: without reservation a finite parent and its child could
        // *each* allocate the full quota (Σ descendants > parent). Delegation now
        // reserves the child's quota on the parent (§36.4 budget partition), so
        // the parent's own budget shrinks and the whole subtree's live bytes stay
        // within the parent's quota — the runtime image of the Lean
        // `subtree_used_le_quota` theorem.
        let t = ArenaTable::new();
        let parent = t.create(&ArenaPolicy::explicit().with_quota(100)).unwrap();

        let pstats = t.stats(parent).unwrap();
        let child = t
            .delegate(parent, &Delegation::inheriting(&pstats, 100, "c"))
            .unwrap();
        let ps = t.stats(parent).unwrap();
        assert_eq!(
            ps.reserved, 100,
            "the child's quota is reserved on the parent"
        );
        assert_eq!(ps.used, 0, "a reservation is not own-live bytes (§8.6)");
        assert_eq!(
            ps.remaining_quota(),
            0,
            "no budget left to allocate or delegate"
        );

        // The parent can no longer allocate a single byte: its budget is fully
        // delegated. (Before the fix it could still allocate the whole 100, so the
        // subtree would hold 200 against a 100 quota.)
        assert_eq!(t.try_charge(parent, 1), Err(ArenaError::QuotaExceeded));
        // The child may allocate up to *its* quota, and no further.
        t.try_charge(child, 100).unwrap();
        assert_eq!(t.try_charge(child, 1), Err(ArenaError::QuotaExceeded));
        // Σ subtree own-live (parent 0 + child 100) == 100 == parent quota.
        assert_eq!(
            t.stats(parent).unwrap().used + t.stats(child).unwrap().used,
            100,
            "the tree-wide bound holds: Σ own-live ≤ root quota"
        );
        assert!(t.check_invariants());

        // Destroying the child returns its reservation; the parent can allocate
        // its full quota again.
        t.begin_destroy(child).unwrap();
        t.finish_destroy(child).unwrap();
        let ps = t.stats(parent).unwrap();
        assert_eq!(ps.reserved, 0, "destroy returns the child's reservation");
        assert_eq!(ps.remaining_quota(), 100, "the parent's budget is restored");
        t.try_charge(parent, 100).unwrap();
        assert!(t.check_invariants());
    }

    #[test]
    fn delegation_reservation_partitions_budget_across_siblings() {
        // Sibling children cannot together reserve more than the parent's quota —
        // the partition is shared, not per-child (§36.4).
        let t = ArenaTable::new();
        let parent = t.create(&ArenaPolicy::explicit().with_quota(100)).unwrap();
        let p0 = t.stats(parent).unwrap();
        let a = t
            .delegate(parent, &Delegation::inheriting(&p0, 60, "a"))
            .unwrap();
        let p1 = t.stats(parent).unwrap();
        assert_eq!(p1.remaining_quota(), 40, "60 reserved, 40 remains");
        // A 50-byte sibling no longer fits the remaining 40...
        assert_eq!(
            t.delegate(parent, &Delegation::inheriting(&p1, 50, "b50")),
            Err(ArenaError::Attenuation)
        );
        // ...but a 40-byte sibling exactly fills it.
        let b = t
            .delegate(parent, &Delegation::inheriting(&p1, 40, "b"))
            .unwrap();
        assert_eq!(t.stats(parent).unwrap().remaining_quota(), 0);
        // Each child uses its own quota; the parent's own budget is exhausted.
        t.try_charge(a, 60).unwrap();
        t.try_charge(b, 40).unwrap();
        assert_eq!(t.try_charge(parent, 1), Err(ArenaError::QuotaExceeded));
        let sum =
            t.stats(parent).unwrap().used + t.stats(a).unwrap().used + t.stats(b).unwrap().used;
        assert_eq!(sum, 100, "Σ subtree own-live == parent quota");
        assert!(t.check_invariants());
    }

    #[test]
    fn destroying_a_parent_before_its_child_keeps_accounting_sound() {
        // The reservation is returned only to the parent incarnation that granted
        // it (generation-checked, §36.13). Destroying the parent first bumps its
        // generation, so the orphaned child's later destroy finds a mismatch and
        // safely *skips* the return — no underflow, the table stays well-formed.
        // (The orphaned child is a lifecycle the caller owns; this asserts only
        // that the quota accounting cannot be corrupted by it.)
        let t = ArenaTable::new();
        let parent = t.create(&ArenaPolicy::explicit().with_quota(100)).unwrap();
        let p0 = t.stats(parent).unwrap();
        let child = t
            .delegate(parent, &Delegation::inheriting(&p0, 40, "c"))
            .unwrap();
        assert_eq!(t.stats(parent).unwrap().reserved, 40);

        t.begin_destroy(parent).unwrap();
        t.finish_destroy(parent).unwrap();
        // The parent incarnation is gone (generation bumped); destroying the child
        // must not panic, underflow, or break the invariants.
        t.begin_destroy(child).unwrap();
        t.finish_destroy(child).unwrap();
        assert!(t.check_invariants());
    }

    #[test]
    fn reset_lifecycle_bumps_reset_gen_keeps_incarnation_and_stays_active() {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit().with_quota(4096)).unwrap();
        let inc0 = t.stats(id).unwrap().generation; // incarnation
        let rg0 = t.stats(id).unwrap().reset_generation;
        let handle = t.handle(id).unwrap();
        t.try_charge(id, 1024).unwrap();

        // The default arena cannot be reset (§22.5).
        assert_eq!(t.begin_reset(ArenaId::DEFAULT), Err(ArenaError::IsDefault));

        t.begin_reset(id).unwrap();
        assert_eq!(t.state(id), Some(ArenaState::Resetting));
        // New allocations are refused mid-reset.
        assert_eq!(t.try_charge(id, 16), Err(ArenaError::NotActive));
        let rg1 = t.finish_reset(id).unwrap();
        assert!(t.is_active(id), "reset returns the arena to Active (§22.5)");
        assert_eq!(
            t.stats(id).unwrap().used,
            0,
            "B.5: no live bytes after reset"
        );
        assert_ne!(rg1, rg0, "reset bumps the reset generation (§22.5)");
        // The incarnation is unchanged, so a handle survives the reset.
        assert_eq!(
            t.stats(id).unwrap().generation,
            inc0,
            "reset must not change the incarnation generation"
        );
        assert_eq!(
            t.resolve_handle(handle),
            Some(id),
            "a handle stays valid across a reset of its own arena"
        );
        assert!(t.check_invariants());
    }

    #[test]
    fn destroy_lifecycle_and_partial_failure_never_destroyed() {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit()).unwrap();
        let g0 = t.stats(id).unwrap().generation;

        // Default arena cannot be destroyed.
        assert_eq!(
            t.begin_destroy(ArenaId::DEFAULT),
            Err(ArenaError::IsDefault)
        );

        // Happy path: Active → Draining → Destroyed, incarnation generation++.
        let handle = t.handle(id).unwrap();
        t.begin_destroy(id).unwrap();
        assert_eq!(t.state(id), Some(ArenaState::Draining));
        let g1 = t.finish_destroy(id).unwrap();
        assert_eq!(t.state(id), Some(ArenaState::Destroyed));
        assert_ne!(g1, g0, "destroy bumps the incarnation generation (§36.13)");
        assert!(
            !t.is_registered(id),
            "a destroyed slot is available for reuse"
        );
        // The pre-destroy handle is now stale (the incarnation is gone).
        assert_eq!(
            t.resolve_handle(handle),
            None,
            "a handle to a destroyed arena is stale (§36.13)"
        );

        // Partial-failure path: Draining → ErrorQuarantined, never Destroyed.
        let id2 = t.create(&ArenaPolicy::explicit()).unwrap();
        t.begin_destroy(id2).unwrap();
        t.quarantine(id2).unwrap();
        assert_eq!(t.state(id2), Some(ArenaState::ErrorQuarantined));
        // A quarantined arena can never be finalized as Destroyed (§36.13).
        assert_eq!(t.finish_destroy(id2), Err(ArenaError::IllegalTransition));
        assert!(t.check_invariants());
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let t = ArenaTable::new();
        let id = t.create(&ArenaPolicy::explicit()).unwrap();
        // Cannot finish a reset that never began.
        assert_eq!(t.finish_reset(id), Err(ArenaError::IllegalTransition));
        // Cannot destroy mid-reset (must finish the reset first).
        t.begin_reset(id).unwrap();
        assert_eq!(t.begin_destroy(id), Err(ArenaError::IllegalTransition));
    }

    #[test]
    fn numa_bind_failures_are_recorded() {
        let t = ArenaTable::new();
        let id = t
            .create(&ArenaPolicy::explicit().with_numa(NumaPolicy::Bind(NodeId(1))))
            .unwrap();
        assert_eq!(t.stats(id).unwrap().numa, NumaPolicy::Bind(NodeId(1)));
        t.record_numa_bind_failure(id);
        t.record_numa_bind_failure(id);
        assert_eq!(t.stats(id).unwrap().numa_bind_failures, 2);
    }

    #[test]
    fn destroyed_ids_are_reused_only_under_pressure_with_a_generation_bump() {
        let t = ArenaTable::new();
        // Fill the table to capacity.
        let mut ids = Vec::new();
        loop {
            match t.create(&ArenaPolicy::explicit()) {
                Ok(id) => ids.push(id),
                Err(ArenaError::Exhausted) => break,
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert_eq!(t.live_count(), MAX_ARENAS);
        // Destroy one; its slot becomes reusable, the next create recycles it
        // with a bumped generation (so stale handles are detectable, §B.5).
        let victim = ids[10];
        let gen_before = t.stats(victim).unwrap().generation;
        t.begin_destroy(victim).unwrap();
        t.finish_destroy(victim).unwrap();
        let reused = t.create(&ArenaPolicy::explicit()).unwrap();
        assert_eq!(
            reused, victim,
            "the freed slot id is recycled under pressure"
        );
        assert_ne!(
            t.stats(reused).unwrap().generation,
            gen_before,
            "a recycled id carries a fresh generation"
        );
    }

    #[test]
    fn configure_updates_policy_but_never_authority() {
        let t = ArenaTable::new();
        let id = t
            .create(
                &ArenaPolicy::explicit()
                    .with_quota(4096)
                    .with_rights(CapRights::ALLOC)
                    .with_label(Label(3)),
            )
            .unwrap();
        let before = t.stats(id).unwrap();
        // Reconfigure the placement/lifetime knobs.
        let cfg = ArenaConfig::from_stats(&before)
            .with_decay(DecayConfig {
                dirty_decay_ms: 1,
                muzzy_decay_ms: 2,
                ..DecayConfig::DEFAULT
            })
            .with_numa(NumaPolicy::Interleave)
            .with_huge(HugepagePolicy::Prefer);
        t.configure(id, &cfg).unwrap();
        let after = t.stats(id).unwrap();
        assert_eq!(after.decay.dirty_decay_ms, 1);
        assert_eq!(after.decay.muzzy_decay_ms, 2);
        assert_eq!(after.numa, NumaPolicy::Interleave);
        assert_eq!(after.huge, HugepagePolicy::Prefer);
        // Authority and quota are untouched — configure cannot widen authority.
        assert_eq!(after.rights, CapRights::ALLOC);
        assert_eq!(after.label, Label(3));
        assert_eq!(after.quota_limit, 4096);
        // Configuring an unregistered/destroyed id fails.
        assert_eq!(t.configure(ArenaId(9999), &cfg), Err(ArenaError::NotFound));
    }

    #[test]
    fn lifecycle_is_ambient_not_gated_on_the_arenas_own_rights() {
        // POSIX collapse (D2): only ALLOC is gated on the arena's cap rights
        // (§36.16). reset/destroy are ambient — gating them on a *delegated
        // child's* rights would wrongly bar the *parent*-authority holder. A
        // no-DESTROY, no-FREE arena is therefore still resettable and
        // destroyable here; the caller-cap check is the seLe4n IPC boundary's job.
        let t = ArenaTable::new();
        let id = t
            .create(&ArenaPolicy::explicit().with_rights(CapRights::ALLOC))
            .unwrap();
        let r = t.stats(id).unwrap().rights;
        assert!(r.contains(CapRights::ALLOC));
        assert!(!r.contains(CapRights::DESTROY) && !r.contains(CapRights::FREE));
        // Allocation IS gated (has ALLOC → admitted).
        t.try_charge(id, 64).unwrap();
        t.credit(id, 64);
        // reset + destroy succeed despite no DESTROY right (ambient on POSIX).
        t.begin_reset(id).unwrap();
        t.finish_reset(id).unwrap();
        t.begin_destroy(id).unwrap();
        t.finish_destroy(id).unwrap();
        assert_eq!(t.stats(id).unwrap().state, ArenaState::Destroyed);
    }

    #[test]
    fn handle_detects_a_reused_id_as_stale() {
        // §36.13: a handle to a destroyed-then-recreated id is stale even though
        // the raw id is reused. Fill the table so the destroyed id is recycled.
        let t = ArenaTable::new();
        let mut ids = Vec::new();
        while let Ok(id) = t.create(&ArenaPolicy::explicit()) {
            ids.push(id);
        }
        let victim = ids[3];
        let handle = t.handle(victim).unwrap();
        assert_eq!(t.resolve_handle(handle), Some(victim));
        t.begin_destroy(victim).unwrap();
        t.finish_destroy(victim).unwrap();
        // The old handle is stale immediately after destroy.
        assert_eq!(t.resolve_handle(handle), None);
        // Recreate: the id is recycled with a new incarnation; the old handle
        // stays stale (it must not become valid for the new arena, §36.13).
        let reused = t.create(&ArenaPolicy::explicit()).unwrap();
        assert_eq!(reused, victim, "id recycled under pressure");
        assert_eq!(
            t.resolve_handle(handle),
            None,
            "a stale handle must not resolve to the reused id's new incarnation"
        );
        // A fresh handle to the new incarnation works.
        let fresh = t.handle(reused).unwrap();
        assert_eq!(t.resolve_handle(fresh), Some(reused));
        assert_ne!(handle, fresh, "the new incarnation has a distinct handle");
    }
}

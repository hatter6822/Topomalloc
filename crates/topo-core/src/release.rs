// SPDX-License-Identifier: MIT
//! The memory release controller & background-purge pump (§20–§21, plan 04 W12).
//!
//! This is the **policy brain** that decides *when* and *how much* unused memory
//! returns to the OS, balancing RSS against page faults, hugepage coverage, latency,
//! and memory pressure (§21.1). It is a **pure, `no_std`, host-driven** object: it
//! makes no provider calls and reads no clock of its own. The host samples the cheap
//! §21.2 observation vector ([`ReleaseInputs`]), calls [`ReleaseController::tick`]
//! with the current time, and *executes* the returned [`ReleasePlan`] by driving the
//! already-existing release **mechanisms** — cache drain, the hugepage backend's
//! [`release_empty_excess`](crate::HugePageBackend::release_empty_excess) /
//! [`subrelease`](crate::HugePageBackend::subrelease) /
//! [`mark_cold`](crate::HugePageBackend::mark_cold), and the extent manager's
//! `purge`/`release` ops.
//!
//! Because the controller only *sequences* mechanisms that are each already certified
//! by the §21.6 release-safety theorem (`release_to_os_preserves_live_objects`,
//! `lean/TopoMalloc/Theorems/Release.lean`), it adds **no new abstract state-machine
//! transition** — so there is no Lean obligation for the policy itself, only for the
//! mechanisms it drives (which remain proved). The single load-bearing *correctness*
//! property the controller owns is anti-oscillation: it must not release memory the
//! application is about to fault straight back (§21.1 R2), which the
//! [`demand_reserve`] brake guarantees and the `oscillation` tests pin.
//!
//! ## The §21.3 release priority ladder
//!
//! Each tick produces a budgeted plan over the six §21.3 rungs, in priority order,
//! each rung gated by the current §21.5 [`PressureMode`]:
//!
//! 1. drain idle CPU/thread caches,
//! 2. release completely-empty hugepages **beyond the demand reserve**,
//! 3. purge dirty spans **not on hot hugepages** (after `dirty_decay_ms`),
//! 4. convert aged dirty → muzzy where cheap (also gated by `dirty_decay_ms`),
//! 5. subrelease cold-sparse partial hugepages (H-005-guarded by the mechanism),
//!    5b. release muzzy back to the OS once it ages past `muzzy_decay_ms`,
//! 6. emergency shrink (Emergency only).
//!
//! The total bytes planned per tick are capped by the arena's
//! `release_rate_bytes_per_sec` (§20.2); desired-but-ungranted work accumulates as
//! [`ReleaseController::backlog_bytes`] and is surfaced in stats (§20.3).

// The §20.2/§22.2 decay & background-purge knobs the controller reads live on the
// arena descriptor (`arena::DecayConfig`, W12-1a), the single source of truth wired
// into `ArenaPolicy`; the controller consumes them rather than defining a parallel
// type.
use crate::arena::DecayConfig;
use crate::ids::ArenaId;

/// The §21.5 memory-pressure watermarks, as basis points (1/10000) of the cgroup
/// memory limit. Crossing `soft_bp`/`hard_bp` escalates the [`PressureMode`];
/// `hysteresis_bp` is the margin a reading must fall **below** a threshold by before
/// the mode steps back down, preventing flapping at the boundary.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PressureThresholds {
    /// Utilization (bp of the limit) at/above which [`PressureMode::Soft`] engages.
    pub soft_bp: u32,
    /// Utilization (bp of the limit) at/above which [`PressureMode::Hard`] engages.
    pub hard_bp: u32,
    /// De-escalation margin (bp): a mode steps down only once utilization drops below
    /// its entry threshold minus this margin (§21.5 anti-flap).
    pub hysteresis_bp: u32,
}

impl Default for PressureThresholds {
    /// Soft at 75%, Hard at 90%, with a 5% de-escalation margin (§21.5 defaults).
    fn default() -> Self {
        Self {
            soft_bp: 7_500,
            hard_bp: 9_000,
            hysteresis_bp: 500,
        }
    }
}

/// The §21.5 memory-pressure mode. Severity increases Normal → Soft → Hard →
/// Emergency; each rung of the §21.3 ladder is gated by the active mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PressureMode {
    /// No pressure: preserve hugepages, decay on the normal timers (§21.5).
    #[default]
    Normal,
    /// Approaching the budget: shrink idle caches, release empty hugepages (§21.5).
    Soft,
    /// Near the limit: accelerate purge, shrink all caches, subrelease cold-sparse.
    Hard,
    /// Allocation failure or cgroup-critical (O-007): bypass optional caches, release
    /// aggressively, disable the HugeCache reserve (§21.5/§36.5).
    Emergency,
}

impl PressureMode {
    /// A 0–3 severity rank (Normal=0 … Emergency=3), so escalation/de-escalation and
    /// rung gating are simple integer comparisons.
    pub const fn severity(self) -> u8 {
        match self {
            PressureMode::Normal => 0,
            PressureMode::Soft => 1,
            PressureMode::Hard => 2,
            PressureMode::Emergency => 3,
        }
    }

    /// The stable string used in stats/diagnostics (Appendix D/E).
    pub const fn as_str(self) -> &'static str {
        match self {
            PressureMode::Normal => "normal",
            PressureMode::Soft => "soft",
            PressureMode::Hard => "hard",
            PressureMode::Emergency => "emergency",
        }
    }
}

/// The latency class of a slow path (§36.11, W12-4). Real-time arenas can forbid the
/// blocking classes; the controller skips ladder rungs whose class exceeds the
/// arena's tolerance ([`ReleaseController::max_latency`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LatencyClass {
    /// Bounded, lock-free-ish work safe on a real-time path (e.g. cache-pointer
    /// bookkeeping). The only class a `no_ipc_fast_only` arena permits.
    #[default]
    FastOnly,
    /// Bounded slow path: a known-bounded amount of work that may take a lock but does
    /// not block indefinitely (e.g. a partial subrelease's accounting).
    BoundedSlow,
    /// May block on the OS/kernel (e.g. `madvise`/`decommit`/IPC) — forbidden on
    /// `no_ipc_fast_only` arenas (§36.11).
    MayBlock,
}

impl LatencyClass {
    /// A 0–2 rank so "class ≤ tolerance" is an integer comparison.
    const fn rank(self) -> u8 {
        match self {
            LatencyClass::FastOnly => 0,
            LatencyClass::BoundedSlow => 1,
            LatencyClass::MayBlock => 2,
        }
    }

    /// Whether a step of this class is permitted under a `tolerance` ceiling.
    const fn permitted_under(self, tolerance: LatencyClass) -> bool {
        self.rank() <= tolerance.rank()
    }
}

/// The §21.2 observation vector — everything the controller reads, sampled cheaply by
/// the host once per tick (W12-2a). Byte counts are absolute; the two `*_total`
/// fields are **cumulative** so the controller derives alloc/free *rates* from their
/// deltas across ticks (no rate bookkeeping on the hot path).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReleaseInputs {
    /// Bytes live in the application (§21.2 `live_bytes`).
    pub live_bytes: u64,
    /// Resident set size, if the host can sample it cheaply (§21.2 `rss_bytes`); `0`
    /// if unknown.
    pub rss_bytes: u64,
    /// Free, physically-backed bytes that may hold old data (§20.1 *dirty*).
    pub dirty_bytes: u64,
    /// Of `dirty_bytes`, the portion residing on **hot** hugepages, which the §21.3
    /// ladder purges *last* (rung 3 is "dirty **not** on hot hugepages").
    pub hot_dirty_bytes: u64,
    /// Free, lazily-purged bytes (§20.1 *muzzy*).
    pub muzzy_bytes: u64,
    /// Free bytes already returned to the OS (§20.1 *released*).
    pub released_bytes: u64,
    /// Drainable idle cache bytes — per-CPU + thread + transfer + idle central
    /// (§21.3 rung 1).
    pub idle_cache_bytes: u64,
    /// Completely-empty backed hugepage bytes the HugeCache holds for reuse, the
    /// release candidates of §21.3 rung 2 (the `release_empty_excess` supply).
    pub empty_backed_hugepage_bytes: u64,
    /// Cold-sparse partial-hugepage bytes eligible for H-005-guarded subrelease
    /// (§21.3 rung 5; the `subrelease` supply).
    pub cold_sparse_bytes: u64,
    /// Hugepage coverage ratio in basis points (§19.7); the controller preserves
    /// coverage under low pressure (§20.3).
    pub hugepage_coverage_ratio_bp: u32,
    /// Cumulative bytes ever allocated (§21.2 `allocation_rate` source).
    pub allocated_bytes_total: u64,
    /// Cumulative bytes ever freed (§21.2 `free_rate` source).
    pub freed_bytes_total: u64,
    /// Refill miss rate as misses per 1000 refills (§21.2 `refill_miss_rate`, a refill
    /// *latency* proxy): a higher value enlarges the demand reserve.
    pub refill_miss_rate_ppk: u32,
    /// Current cgroup/container memory charge (§21.2 `cgroup_memory_current`); `0` if
    /// unknown.
    pub cgroup_current: u64,
    /// cgroup/container memory limit (§21.2 `cgroup_memory_max`); `0` ⇒ no limit known
    /// (pressure is then driven only by `alloc_failed`/`pressure_notifications`).
    pub cgroup_max: u64,
    /// Monotonic count of OS memory-pressure notifications (§21.2, PSI-style); a rise
    /// since the last tick forces at least Soft pressure.
    pub pressure_notifications: u64,
    /// An allocation has just failed (§21.5 Emergency trigger, O-007).
    pub alloc_failed: bool,
    /// The system is under CPU pressure (§20.3 "yield under CPU pressure"): the pump
    /// does only emergency work this tick.
    pub cpu_pressure: bool,
}

impl ReleaseInputs {
    /// Dirty bytes eligible for purging — those **not** on hot hugepages (§21.3 rung
    /// 3). Saturating, so a stale `hot_dirty_bytes > dirty_bytes` reads as zero.
    fn purgeable_dirty(&self) -> u64 {
        self.dirty_bytes.saturating_sub(self.hot_dirty_bytes)
    }

    /// cgroup utilization in basis points of the limit, or `0` when no limit is known.
    fn utilization_bp(&self) -> u32 {
        if self.cgroup_max == 0 {
            return 0;
        }
        let bp = (self.cgroup_current as u128 * 10_000) / self.cgroup_max as u128;
        bp.min(u32::MAX as u128) as u32
    }

    /// cgroup-critical: charged at or above the limit (§21.5 Emergency trigger).
    fn cgroup_critical(&self) -> bool {
        self.cgroup_max != 0 && self.cgroup_current >= self.cgroup_max
    }
}

/// The byte budget the controller plans for one tick, one entry per §21.3 ladder rung,
/// plus the decisions that shaped it (W12-2b). The host **executes** it by driving the
/// corresponding mechanism with each rung's byte budget; an all-zero plan is a no-op.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReleasePlan {
    /// The §21.5 mode this plan was computed under.
    pub mode: PressureMode,
    /// The §21.4 demand reserve withheld from release this tick (the anti-oscillation
    /// brake): empty-hugepage release (rung 2) only acts on the supply *beyond* this.
    pub demand_reserve_bytes: u64,
    /// Rung 1 — bytes of idle cache to drain.
    pub drain_caches_bytes: u64,
    /// Rung 2 — empty-backed hugepage bytes to release to the OS (beyond the reserve).
    pub release_empty_hugepages_bytes: u64,
    /// Rung 3 — dirty (non-hot-hugepage) bytes to purge.
    pub purge_dirty_bytes: u64,
    /// Rung 4 — dirty bytes to convert to muzzy (the cheaper lazy purge).
    pub dirty_to_muzzy_bytes: u64,
    /// Rung 5 — cold-sparse partial-hugepage bytes to subrelease (H-005-guarded).
    pub subrelease_cold_sparse_bytes: u64,
    /// Rung 5b — *muzzy* bytes to release to the OS (`MADV_DONTNEED`) once they have
    /// aged past `muzzy_decay_ms`, or under Hard pressure (§20.2 muzzy decay). Lets
    /// muzzy memory actually return to the OS outside Emergency — without it,
    /// `muzzy_decay_ms` would never reclaim. Emergency releases muzzy via
    /// [`emergency_shrink_bytes`](Self::emergency_shrink_bytes) instead (no overlap).
    pub release_muzzy_bytes: u64,
    /// Rung 6 — emergency shrink bytes (Emergency only): forced muzzy/global shrink.
    pub emergency_shrink_bytes: u64,
    /// Whether the HugeCache empty-backed reserve must be disabled for this tick
    /// (§21.5/§36.5 Emergency): the backend should release *all* empty hugepages.
    pub disable_hugecache_reserve: bool,
}

impl ReleasePlan {
    /// Total bytes this plan asks the host to return/recycle across all rungs — what
    /// the rate cap (§20.2) bounds and what the backlog (§20.3) accounts.
    pub fn total_bytes(&self) -> u64 {
        self.drain_caches_bytes
            .saturating_add(self.release_empty_hugepages_bytes)
            .saturating_add(self.purge_dirty_bytes)
            .saturating_add(self.dirty_to_muzzy_bytes)
            .saturating_add(self.subrelease_cold_sparse_bytes)
            .saturating_add(self.release_muzzy_bytes)
            .saturating_add(self.emergency_shrink_bytes)
    }

    /// Whether the plan asks for no work at all.
    pub fn is_empty(&self) -> bool {
        self.total_bytes() == 0 && !self.disable_hugecache_reserve
    }
}

/// A snapshot of the controller's running counters (§20.3), surfaced in stats (plan
/// 07). All cumulative except `mode`/`backlog_bytes`, which are instantaneous.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReleaseStats {
    /// The current §21.5 pressure mode.
    pub mode: PressureMode,
    /// Bytes desired-but-not-yet-granted by the rate cap (§20.3 backlog).
    pub backlog_bytes: u64,
    /// Cumulative bytes ever planned for release across all rungs.
    pub planned_bytes_total: u64,
    /// Cumulative ticks processed (a liveness signal for the pump).
    pub ticks: u64,
    /// The §21.4 demand reserve from the most recent tick.
    pub demand_reserve_bytes: u64,
    /// Cumulative number of ticks that planned any work (background activity rate).
    pub active_ticks: u64,
    /// The most recent observed allocation rate (§21.2 `allocation_rate`), bytes/sec,
    /// derived from the cumulative-counter delta over the last interval.
    pub alloc_rate_bps: u64,
    /// The most recent observed free rate (§21.2 `free_rate`), bytes/sec.
    pub free_rate_bps: u64,
}

/// How far ahead the demand reserve looks: the reserve covers roughly the bytes the
/// application would allocate in this window at the recent rate (§21.4). One refill
/// horizon — long enough to bridge a churn burst, short enough not to hoard.
const RESERVE_WINDOW_MS: u64 = 1_000;

/// The horizon over which the §21.4 `recent_peak` of releasable-free memory relaxes
/// back toward the current free when no new peak occurs (a leaky peak-hold). Long
/// enough to remember a churn burst's peak across several reserve windows, short
/// enough that a one-off transient free spike is forgotten rather than pinning the
/// demand-reserve cap high indefinitely — which would over-retain RSS long after the
/// spike drained. Eight reserve windows.
const PEAK_DECAY_MS: u64 = 8 * RESERVE_WINDOW_MS;

/// The §21.4 demand reserve (W12-2c) — the anti-oscillation brake. Withholds from
/// release roughly the bytes the application will allocate during one
/// `RESERVE_WINDOW_MS` at `alloc_rate_bps`, enlarged when refills are expensive
/// (`refill_miss_rate_ppk`) and capped at `recent_peak_free` (never reserve more free
/// memory than has recently existed), then scaled **down** by pressure severity so a
/// stressed system reclaims more (Emergency reserves nothing, §21.5/§36.5).
///
/// The load-bearing property (§21.1 R2): when the allocation rate is high the reserve
/// is large, so the ladder releases little and the application does not refault what
/// it just freed; when the rate is ~0 (truly idle) the reserve is ~0 and memory is
/// freed. Pinned by the `oscillation` tests.
pub fn demand_reserve(
    alloc_rate_bps: u64,
    recent_peak_free: u64,
    refill_miss_rate_ppk: u32,
    mode: PressureMode,
) -> u64 {
    // Base: bytes allocated in one reserve window at the recent rate.
    let base = (alloc_rate_bps as u128 * RESERVE_WINDOW_MS as u128) / 1_000;
    // Enlarge when refills miss often (expensive to refault): ×(1 + miss/1000).
    let scaled = base + (base * refill_miss_rate_ppk as u128) / 1_000;
    // Never hold back more free memory than recently existed.
    let capped = scaled.min(recent_peak_free as u128);
    // Pressure attenuation: Normal ×1, Soft ×1/2, Hard ×1/4, Emergency ×0.
    let reserve = match mode {
        PressureMode::Normal => capped,
        PressureMode::Soft => capped / 2,
        PressureMode::Hard => capped / 4,
        PressureMode::Emergency => 0,
    };
    reserve.min(u64::MAX as u128) as u64
}

/// The stateful release controller / background-purge pump (§20.3/§21, W12). One per
/// process (or per policy domain). Pure and `no_std`: [`tick`](Self::tick) takes the
/// host's clock and observation vector and returns a [`ReleasePlan`] for the host to
/// execute — the controller never touches the provider itself.
pub struct ReleaseController {
    config: DecayConfig,
    thresholds: PressureThresholds,
    max_latency: LatencyClass,
    /// The bounded, **heap-independent** emergency reserve (§36.5, W12-3b): set once at
    /// construction and never funded from the normal heap, so emergency release does
    /// not depend on the memory it is trying to reclaim.
    emergency_reserve_bytes: u64,
    // --- running state ---
    mode: PressureMode,
    last_tick_ms: u64,
    started: bool,
    /// Timestamp dirty memory was last observed to (re)appear, for the dirty-decay
    /// gate (§20.2); meaningful only while `dirty_present`.
    dirty_since_ms: u64,
    /// Whether dirty memory was present on the previous tick (so the decay clock
    /// measures *continuous* dirtiness and a `now_ms == 0` start is not ambiguous).
    dirty_present: bool,
    /// Timestamp muzzy memory was last observed to (re)appear, for the muzzy-decay
    /// gate (§20.2); meaningful only while `muzzy_present`. The analogue of
    /// `dirty_since_ms` for the second decay stage (muzzy → released).
    muzzy_since_ms: u64,
    /// Whether muzzy memory was present on the previous tick (continuous-muzzy clock).
    muzzy_present: bool,
    last_allocated_total: u64,
    last_freed_total: u64,
    last_pressure_notifications: u64,
    /// The **anchor** of the §21.4 `recent_peak` cap: the last new high of releasable
    /// free, or the current free once the prior anchor fully aged out. The *effective*
    /// cap each tick is this anchor relaxed toward current free by its age (a leaky
    /// peak-hold, computed in [`tick`](Self::tick)), so the reserve follows what has
    /// *recently* existed rather than an all-time high-water mark. Written only at a
    /// re-anchor, so the decay stays a pure function of elapsed time (tick-cadence
    /// independent).
    recent_peak_free: u64,
    /// The instant [`recent_peak_free`](Self::recent_peak_free) was last (re-)anchored,
    /// from which the leaky peak-hold's age-based decay is measured.
    peak_anchor_ms: u64,
    /// The §21.2 alloc/free rates observed on the most recent interval (bytes/sec).
    alloc_rate_bps: u64,
    free_rate_bps: u64,
    backlog_bytes: u64,
    planned_bytes_total: u64,
    demand_reserve_bytes: u64,
    ticks: u64,
    active_ticks: u64,
    /// Round-robin cursor for fair multi-arena purging (§20.3, W12-1b).
    next_arena: u32,
}

impl ReleaseController {
    /// A controller with the given decay config and the §21.5 default thresholds, no
    /// latency restriction, and no emergency reserve. Use [`with`](Self::with) to set
    /// thresholds / latency ceiling / emergency reserve.
    pub fn new(config: DecayConfig) -> Self {
        Self::with(
            config,
            PressureThresholds::default(),
            LatencyClass::MayBlock,
            0,
        )
    }

    /// A fully-specified controller: `thresholds` (§21.5 watermarks), `max_latency`
    /// (the slowest ladder rung this domain tolerates, §36.11), and
    /// `emergency_reserve_bytes` (the §36.5 heap-independent reserve).
    pub fn with(
        config: DecayConfig,
        thresholds: PressureThresholds,
        max_latency: LatencyClass,
        emergency_reserve_bytes: u64,
    ) -> Self {
        Self {
            config,
            thresholds,
            max_latency,
            emergency_reserve_bytes,
            mode: PressureMode::Normal,
            last_tick_ms: 0,
            started: false,
            dirty_since_ms: 0,
            dirty_present: false,
            muzzy_since_ms: 0,
            muzzy_present: false,
            last_allocated_total: 0,
            last_freed_total: 0,
            last_pressure_notifications: 0,
            recent_peak_free: 0,
            peak_anchor_ms: 0,
            alloc_rate_bps: 0,
            free_rate_bps: 0,
            backlog_bytes: 0,
            planned_bytes_total: 0,
            demand_reserve_bytes: 0,
            ticks: 0,
            active_ticks: 0,
            next_arena: 0,
        }
    }

    /// Build a controller **for an arena** (W12-4 wiring): its §20.2 decay config and
    /// its §36.11 [`latency`](crate::arena::ArenaPolicy::latency) tolerance are taken
    /// from the policy, with the default §21.5 thresholds and no emergency reserve. A
    /// `FastOnly` arena thus drives a controller that skips every blocking ladder rung.
    pub fn for_arena(policy: &crate::arena::ArenaPolicy) -> Self {
        Self::with(
            policy.decay,
            PressureThresholds::default(),
            policy.latency,
            0,
        )
    }

    /// The active decay configuration.
    pub fn config(&self) -> DecayConfig {
        self.config
    }

    /// The current §21.5 pressure mode.
    pub fn mode(&self) -> PressureMode {
        self.mode
    }

    /// Bytes desired-but-not-yet-granted by the rate cap (§20.3 backlog).
    pub fn backlog_bytes(&self) -> u64 {
        self.backlog_bytes
    }

    /// The slowest ladder rung this controller will plan (§36.11).
    pub fn max_latency(&self) -> LatencyClass {
        self.max_latency
    }

    /// The §36.5 heap-independent emergency reserve (constant after construction).
    pub fn emergency_reserve_bytes(&self) -> u64 {
        self.emergency_reserve_bytes
    }

    /// A snapshot of the running counters (§20.3) for stats.
    pub fn stats(&self) -> ReleaseStats {
        ReleaseStats {
            mode: self.mode,
            backlog_bytes: self.backlog_bytes,
            planned_bytes_total: self.planned_bytes_total,
            ticks: self.ticks,
            demand_reserve_bytes: self.demand_reserve_bytes,
            active_ticks: self.active_ticks,
            alloc_rate_bps: self.alloc_rate_bps,
            free_rate_bps: self.free_rate_bps,
        }
    }

    /// The next arena index to service under round-robin fairness, advancing the
    /// cursor modulo `arena_count` (§20.3 "process arenas fairly", W12-1b). The host
    /// drives one arena's decay per call so no arena is starved under the rate cap.
    /// `arena_count == 0` yields the default arena and does not advance.
    pub fn next_fair_arena(&mut self, arena_count: u32) -> ArenaId {
        if arena_count == 0 {
            return ArenaId::DEFAULT;
        }
        let idx = self.next_arena % arena_count;
        self.next_arena = (self.next_arena + 1) % arena_count;
        ArenaId(idx)
    }

    /// **The background-purge pump step (§20.3, W12-1b).** Given the current time
    /// `now_ms` and the freshly-sampled `inputs`, update the pressure mode (§21.5,
    /// with hysteresis), compute the §21.4 demand reserve, and plan the §21.3 ladder —
    /// rate-capped (§20.2) with the unmet remainder added to the backlog (§20.3).
    ///
    /// The host calls this off the application fast path (§20.3) and then executes the
    /// returned [`ReleasePlan`] by driving the release mechanisms. Idempotent in the
    /// sense that re-ticking with the same time grants no new rate budget.
    pub fn tick(&mut self, now_ms: u64, inputs: ReleaseInputs) -> ReleasePlan {
        let elapsed_ms = if self.started {
            now_ms.saturating_sub(self.last_tick_ms)
        } else {
            0
        };

        // Track a *recent* peak of releasable-free memory for the §21.4 reserve cap
        // (`recent_peak`). The cap is the peak **anchor** relaxed linearly toward the
        // current free over `PEAK_DECAY_MS`, measured from the instant the anchor was set
        // (a leaky peak-hold) — so it follows *recent* free rather than an all-time
        // high-water mark that would keep the reserve stale-high (over-retaining RSS)
        // long after a transient free burst had drained.
        //
        // Crucially the decay is computed from the anchor's *age*, not by subtracting a
        // fraction of the remaining excess each tick: a per-tick fraction would compound
        // (exponential) and make the cap depend on how *often* the pump ticks — N small
        // ticks would leave ~1/e of a long-dead spike after a full horizon while one big
        // tick fully decayed it. Anchoring makes the cap a pure function of elapsed time,
        // so any tick cadence over the same span yields the same cap.
        let free_now = inputs
            .idle_cache_bytes
            .saturating_add(inputs.dirty_bytes)
            .saturating_add(inputs.muzzy_bytes)
            .saturating_add(inputs.empty_backed_hugepage_bytes);
        // Re-anchor on a new high of releasable free, or once the old anchor has fully
        // aged out (≥ PEAK_DECAY_MS): record both the value and the instant it was set.
        if free_now >= self.recent_peak_free
            || now_ms.saturating_sub(self.peak_anchor_ms) >= PEAK_DECAY_MS
        {
            self.recent_peak_free = free_now;
            self.peak_anchor_ms = now_ms;
        }
        // The effective cap: anchor (≥ `free_now`) relaxed toward `free_now` by the
        // anchor's age. Always ≥ `free_now`, so the cap can still cover the live free
        // supply; just after a re-anchor (age 0) it equals the anchor itself.
        let recent_peak = {
            let age = now_ms
                .saturating_sub(self.peak_anchor_ms)
                .min(PEAK_DECAY_MS);
            let excess = (self.recent_peak_free - free_now) as u128; // anchor ≥ free_now
            let remaining = excess * (PEAK_DECAY_MS - age) as u128 / PEAK_DECAY_MS as u128;
            free_now.saturating_add(remaining as u64)
        };

        // Alloc/free rates (bytes/sec) from the cumulative counter deltas over the
        // interval (§21.2). The reserve keys on the *allocation* rate (the conservative
        // anti-oscillation choice: reserve for what will be allocated, regardless of
        // frees); both are surfaced in stats so the host can observe the working set.
        let alloc_rate_bps = rate_per_sec(
            inputs
                .allocated_bytes_total
                .saturating_sub(self.last_allocated_total),
            elapsed_ms,
        );
        let free_rate_bps = rate_per_sec(
            inputs
                .freed_bytes_total
                .saturating_sub(self.last_freed_total),
            elapsed_ms,
        );
        self.alloc_rate_bps = alloc_rate_bps;
        self.free_rate_bps = free_rate_bps;

        // Track the dirty-decay clock: stamp when dirty (re)appears, clear when it
        // drains to zero, so the gate measures *continuous* dirtiness (§20.2). The
        // `dirty_present` flag (not a `dirty_since_ms == 0` sentinel) keeps a tick at
        // `now_ms == 0` unambiguous.
        if inputs.dirty_bytes == 0 {
            self.dirty_present = false;
            self.dirty_since_ms = now_ms;
        } else if !self.dirty_present {
            self.dirty_present = true;
            self.dirty_since_ms = now_ms;
        }
        // The muzzy-decay clock (the second decay stage), tracked exactly like dirty so
        // the gate measures *continuous* muzzy residency (§20.2).
        if inputs.muzzy_bytes == 0 {
            self.muzzy_present = false;
            self.muzzy_since_ms = now_ms;
        } else if !self.muzzy_present {
            self.muzzy_present = true;
            self.muzzy_since_ms = now_ms;
        }

        // §21.5 pressure mode with hysteresis (escalate immediately, de-escalate only
        // past the margin). A rise in pressure notifications forces at least Soft.
        let pressure_rose = inputs.pressure_notifications > self.last_pressure_notifications;
        self.mode = self.classify_mode(&inputs, pressure_rose);

        // §21.4 demand reserve (the anti-oscillation brake).
        let reserve = demand_reserve(
            alloc_rate_bps,
            recent_peak,
            inputs.refill_miss_rate_ppk,
            self.mode,
        );
        self.demand_reserve_bytes = reserve;

        // The dirty-decay gate (§20.2): in Normal/Soft, only purge dirty that has been
        // continuously resident longer than `dirty_decay_ms`; Hard/Emergency ignore
        // the timer (accelerated purge, §21.5).
        let dirty_aged = self.mode.severity() >= PressureMode::Hard.severity()
            || now_ms.saturating_sub(self.dirty_since_ms) >= self.config.dirty_decay_ms;
        // The muzzy-decay gate (§20.2, the second stage): release muzzy once it has
        // been continuously resident past `muzzy_decay_ms`; Hard/Emergency accelerate.
        let muzzy_aged = self.mode.severity() >= PressureMode::Hard.severity()
            || now_ms.saturating_sub(self.muzzy_since_ms) >= self.config.muzzy_decay_ms;

        let mut plan = self.plan_ladder(&inputs, reserve, dirty_aged, muzzy_aged);

        // Per-tick rate budget (§20.2). The plan is recomputed from the *absolute*
        // current supply every tick, so the carried backlog and this tick's plan
        // describe the **same** persistent releasable memory — not two separate debts.
        // `desired` is therefore the *max* of the two (the larger of "what we still owe"
        // and "what we now see"), never their sum: summing would double-count a
        // rate-capped persistent supply and let the backlog diverge far past the memory
        // that actually exists (e.g. ~16 MiB of dirty held under a 1 MiB/s cap would
        // accrue ~15 MiB of "owed" release *every* tick forever). `0` rate ⇒ unlimited.
        let supply = plan.total_bytes();
        let desired = supply.max(self.backlog_bytes);
        if self.config.release_rate_bytes_per_sec != 0 && self.mode != PressureMode::Emergency {
            // The budget is just this interval's allowance; the unmet remainder of
            // `desired` becomes the new backlog.
            let budget = rate_budget(
                self.config.release_rate_bytes_per_sec,
                elapsed_ms,
                !self.started,
            );
            let granted = budget.min(desired);
            scale_plan_to_budget(&mut plan, granted);
            // Credit only what was ACTUALLY planned this tick, not the full `granted`:
            // `scale_plan_to_budget` fills the plan only up to the work that currently
            // exists, so when `granted` exceeds the available work the plan totals less
            // than `granted`; crediting the full `granted` would silently forgive the
            // unplanned remainder (RSS stays high while the controller reports
            // progress). The remainder stays owed as backlog — and is additionally capped
            // by **this tick's observed supply** (`supply`, the uncapped ladder total),
            // because a backlog is release work that still *exists*. Without that cap it
            // latched: once the supply drained, `desired` fell back to the old backlog and
            // the plan totalled `0`, so `desired − 0` reproduced the same number every
            // tick — a fully-drained, idle allocator reporting megabytes of "owed" release
            // forever, and the §21.4 anti-oscillation brake reading a debt that no memory
            // backs (§20.3).
            self.backlog_bytes = desired.saturating_sub(plan.total_bytes()).min(supply);
        } else {
            // Unlimited (or Emergency, which bypasses the cap): clear the backlog.
            self.backlog_bytes = 0;
        }

        // Commit running state.
        self.last_tick_ms = now_ms;
        self.last_allocated_total = inputs.allocated_bytes_total;
        self.last_freed_total = inputs.freed_bytes_total;
        self.last_pressure_notifications = inputs.pressure_notifications;
        self.started = true;
        self.ticks += 1;
        let planned = plan.total_bytes();
        self.planned_bytes_total = self.planned_bytes_total.saturating_add(planned);
        if !plan.is_empty() {
            self.active_ticks += 1;
        }
        plan
    }

    /// §21.5 mode classification with hysteresis over the current mode.
    fn classify_mode(&self, inputs: &ReleaseInputs, pressure_rose: bool) -> PressureMode {
        // Emergency is the highest priority trigger (O-007): an allocation failure or a
        // cgroup-critical charge, regardless of watermark.
        if inputs.alloc_failed || inputs.cgroup_critical() {
            return PressureMode::Emergency;
        }
        let u = inputs.utilization_bp();
        // The raw watermark mode from utilization alone.
        let mut raw = if u >= self.thresholds.hard_bp {
            PressureMode::Hard
        } else if u >= self.thresholds.soft_bp {
            PressureMode::Soft
        } else {
            PressureMode::Normal
        };
        // A fresh memory-pressure notification forces at least Soft (§21.2/§21.5).
        if pressure_rose && raw.severity() < PressureMode::Soft.severity() {
            raw = PressureMode::Soft;
        }
        // Escalate immediately; de-escalate only once below the entry threshold by the
        // hysteresis margin (anti-flap, §21.5).
        if raw.severity() >= self.mode.severity() {
            return raw;
        }
        let stay = match self.mode {
            PressureMode::Emergency => {
                // Leave Emergency once neither trigger holds (checked above) — fall to
                // the raw watermark mode, no margin (the triggers are already clear).
                false
            }
            PressureMode::Hard => {
                u >= self
                    .thresholds
                    .hard_bp
                    .saturating_sub(self.thresholds.hysteresis_bp)
            }
            PressureMode::Soft => {
                u >= self
                    .thresholds
                    .soft_bp
                    .saturating_sub(self.thresholds.hysteresis_bp)
            }
            PressureMode::Normal => false,
        };
        if stay {
            self.mode
        } else {
            raw
        }
    }

    /// Build the §21.3 ladder plan (pre-rate-cap), each rung gated by the mode and the
    /// latency ceiling (§36.11). `dirty_aged` is the §20.2 decay-gate result.
    fn plan_ladder(
        &self,
        inputs: &ReleaseInputs,
        reserve: u64,
        dirty_aged: bool,
        muzzy_aged: bool,
    ) -> ReleasePlan {
        let mut plan = ReleasePlan {
            mode: self.mode,
            demand_reserve_bytes: reserve,
            ..ReleasePlan::default()
        };
        let sev = self.mode.severity();
        let emergency = self.mode == PressureMode::Emergency;

        // Background purging suppressed (§20.2) ⇒ only Emergency does anything.
        if !self.config.background_purge_enabled && !emergency {
            return plan;
        }
        // §20.3 "yield under CPU pressure": do only emergency work when CPU-pressured.
        if inputs.cpu_pressure && !emergency {
            return plan;
        }

        // Rung 1 — drain idle caches (FastOnly). Normal trims a quarter (keep caches
        // warm); Soft+ drains progressively more; Hard/Emergency drains all.
        if LatencyClass::FastOnly.permitted_under(self.max_latency) {
            plan.drain_caches_bytes = match sev {
                0 => inputs.idle_cache_bytes / 4,
                1 => inputs.idle_cache_bytes / 2,
                _ => inputs.idle_cache_bytes,
            };
        }

        // Rung 2 — release empty hugepages beyond the demand reserve (MayBlock). Normal
        // preserves coverage (§20.3) and releases nothing here; Soft+ releases the
        // supply above the reserve. Emergency disables the reserve entirely (§36.5).
        if sev >= PressureMode::Soft.severity()
            && LatencyClass::MayBlock.permitted_under(self.max_latency)
        {
            let reserve_for_hp = if emergency { 0 } else { reserve };
            plan.release_empty_hugepages_bytes = inputs
                .empty_backed_hugepage_bytes
                .saturating_sub(reserve_for_hp);
        }
        if emergency {
            plan.disable_hugecache_reserve = true;
        }

        // Rung 3 — purge dirty not on hot hugepages (MayBlock), gated by the decay
        // timer in Normal/Soft (`dirty_aged`), accelerated in Hard/Emergency.
        if dirty_aged && LatencyClass::MayBlock.permitted_under(self.max_latency) {
            plan.purge_dirty_bytes = inputs.purgeable_dirty();
        }

        // Rung 4 — convert dirty→muzzy where cheap (BoundedSlow lazy purge): the (aged)
        // dirty we did *not* fully purge this tick is marked discardable. Gated by the
        // dirty-decay timer (`dirty_aged`) like rung 3, so dirty is RETAINED for reuse
        // until `dirty_decay_ms` elapses (not lazily purged on the first background
        // tick); bytes already counted under rung 3 are excluded (no double-count).
        if dirty_aged && LatencyClass::BoundedSlow.permitted_under(self.max_latency) {
            plan.dirty_to_muzzy_bytes = inputs
                .purgeable_dirty()
                .saturating_sub(plan.purge_dirty_bytes);
        }

        // Rung 5 — subrelease cold-sparse partial hugepages (BoundedSlow; the mechanism
        // enforces H-005). Only Hard/Emergency, or never below — preserving coverage
        // under light pressure (§19.6/§20.3).
        if sev >= PressureMode::Hard.severity()
            && LatencyClass::BoundedSlow.permitted_under(self.max_latency)
        {
            plan.subrelease_cold_sparse_bytes = inputs.cold_sparse_bytes;
        }

        // Rung 5b — release aged muzzy to the OS (MayBlock): muzzy continuously resident
        // past `muzzy_decay_ms`, or under Hard pressure, is returned (`MADV_DONTNEED`),
        // so the second decay stage actually reclaims RSS *outside* Emergency (without
        // this, `muzzy_decay_ms` never reclaims — the muzzy would linger until an alloc
        // failure or cgroup-critical event). Excludes Emergency, which shrinks muzzy via
        // rung 6 instead, so the two never double-count the same bytes.
        if muzzy_aged && !emergency && LatencyClass::MayBlock.permitted_under(self.max_latency) {
            plan.release_muzzy_bytes = inputs.muzzy_bytes;
        }

        // Rung 6 — emergency shrink (Emergency only, MayBlock): force the global caches
        // and any remaining muzzy back, bypassing optional caches (§21.5/O-007).
        if emergency && LatencyClass::MayBlock.permitted_under(self.max_latency) {
            plan.emergency_shrink_bytes = inputs.muzzy_bytes;
        }

        plan
    }
}

/// Bytes/sec from a delta over an interval; `0` for a zero-length interval.
fn rate_per_sec(delta_bytes: u64, elapsed_ms: u64) -> u64 {
    if elapsed_ms == 0 {
        return 0;
    }
    ((delta_bytes as u128 * 1_000) / elapsed_ms as u128).min(u64::MAX as u128) as u64
}

/// Bytes permitted by a per-second rate over an interval.
///
/// `first` marks the very first tick, which has no interval to measure and is granted a
/// one-window floor so the controller can act immediately. Every later zero-length
/// interval grants **nothing**: two ticks in the same millisecond (or a backwards clock
/// step, which `saturating_sub` also reports as `0`) must not each receive a full second
/// of allowance — a host draining the backlog in a loop at, say, 10 kHz would otherwise
/// be granted 10 000 × the configured §20.2 rate, and `tick`'s own contract
/// ("re-ticking with the same time grants no new rate budget") would be false.
fn rate_budget(rate_bps: u64, elapsed_ms: u64, first: bool) -> u64 {
    let ms = match (elapsed_ms, first) {
        (0, true) => RESERVE_WINDOW_MS,
        (0, false) => return 0,
        (ms, _) => ms,
    };
    ((rate_bps as u128 * ms as u128) / 1_000).min(u64::MAX as u128) as u64
}

/// Trim a plan to `budget` total bytes, granting from the **top** of the §21.3 ladder
/// down (rung 1 → rung 6) so a tight cap funds the highest-priority work first and
/// defers the rest (which the caller records as backlog, §20.3).
fn scale_plan_to_budget(plan: &mut ReleasePlan, budget: u64) {
    let mut remaining = budget;
    for slot in [
        &mut plan.drain_caches_bytes,
        &mut plan.release_empty_hugepages_bytes,
        &mut plan.purge_dirty_bytes,
        &mut plan.dirty_to_muzzy_bytes,
        &mut plan.subrelease_cold_sparse_bytes,
        &mut plan.release_muzzy_bytes,
        &mut plan.emergency_shrink_bytes,
    ] {
        let take = (*slot).min(remaining);
        *slot = take;
        remaining -= take;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_inputs() -> ReleaseInputs {
        ReleaseInputs {
            idle_cache_bytes: 1_000_000,
            dirty_bytes: 2_000_000,
            muzzy_bytes: 500_000,
            empty_backed_hugepage_bytes: 8 * 1024 * 1024,
            cold_sparse_bytes: 4 * 1024 * 1024,
            ..ReleaseInputs::default()
        }
    }

    #[test]
    fn a_repeated_tick_at_the_same_time_grants_no_new_rate_budget() {
        // §20.2 rate cap + `tick`'s own contract ("re-ticking with the same time grants no
        // new rate budget"). `elapsed_ms` is `0` for a second tick in the same millisecond
        // *and* for a backwards clock step, and granting a full window each time let a
        // host draining the backlog in a loop release orders of magnitude past the cap.
        // Only the very first tick gets the bootstrap floor.
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1024 * 1024, // 1 MiB/s
            ..DecayConfig::low_rss()
        };
        let mut c = ReleaseController::new(cfg);
        let inputs = ReleaseInputs {
            cgroup_current: 95,
            cgroup_max: 100,
            ..base_inputs()
        };

        // First tick: the one-window floor lets the controller act immediately.
        let first = c.tick(1_000, inputs);
        assert!(
            first.total_bytes() > 0,
            "the first tick must be able to act"
        );

        // Same millisecond again: nothing new is granted.
        let same = c.tick(1_000, inputs);
        assert_eq!(
            same.total_bytes(),
            0,
            "a same-millisecond re-tick granted {} bytes of fresh budget",
            same.total_bytes()
        );
        // A backwards clock reads as a zero interval too — also no grant, no panic.
        let back = c.tick(900, inputs);
        assert_eq!(back.total_bytes(), 0, "a backwards clock granted budget");

        // A real interval grants exactly its share (1 MiB/s over 500 ms = 512 KiB).
        let later = c.tick(1_400, inputs);
        assert!(
            later.total_bytes() > 0 && later.total_bytes() <= 512 * 1024,
            "500 ms at 1 MiB/s granted {} bytes",
            later.total_bytes()
        );
    }

    #[test]
    fn default_config_is_the_server_preset() {
        let c = DecayConfig::default();
        assert_eq!(c.dirty_decay_ms, 10_000);
        assert_eq!(c.muzzy_decay_ms, 10_000);
        assert!(c.background_purge_enabled);
        assert_eq!(DecayConfig::server(), c);
        assert_eq!(DecayConfig::low_rss().dirty_decay_ms, 0);
    }

    #[test]
    fn pressure_mode_escalates_with_cgroup_utilization() {
        let mut c = ReleaseController::new(DecayConfig::default());
        // 50% → Normal.
        let i = ReleaseInputs {
            cgroup_current: 50,
            cgroup_max: 100,
            ..base_inputs()
        };
        c.tick(1_000, i);
        assert_eq!(c.mode(), PressureMode::Normal);
        // 80% → Soft.
        let i = ReleaseInputs {
            cgroup_current: 80,
            cgroup_max: 100,
            ..base_inputs()
        };
        c.tick(2_000, i);
        assert_eq!(c.mode(), PressureMode::Soft);
        // 95% → Hard.
        let i = ReleaseInputs {
            cgroup_current: 95,
            cgroup_max: 100,
            ..base_inputs()
        };
        c.tick(3_000, i);
        assert_eq!(c.mode(), PressureMode::Hard);
    }

    #[test]
    fn alloc_failure_forces_emergency_and_disables_hugecache_reserve() {
        let mut c = ReleaseController::new(DecayConfig::default());
        let i = ReleaseInputs {
            alloc_failed: true,
            ..base_inputs()
        };
        let plan = c.tick(1_000, i);
        assert_eq!(c.mode(), PressureMode::Emergency);
        assert!(
            plan.disable_hugecache_reserve,
            "Emergency disables the HugeCache reserve (§36.5)"
        );
        assert_eq!(plan.demand_reserve_bytes, 0, "Emergency reserves nothing");
        // Emergency releases all empty hugepages (no reserve withheld).
        assert_eq!(plan.release_empty_hugepages_bytes, 8 * 1024 * 1024);
    }

    #[test]
    fn pressure_mode_has_hysteresis_on_the_way_down() {
        let mut c = ReleaseController::new(DecayConfig::default());
        // Escalate to Hard at 92%.
        c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 92,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(c.mode(), PressureMode::Hard);
        // Drop to 88% (below hard_bp=9000 but within the 500bp margin: 9000-500=8500,
        // 8800 ≥ 8500) ⇒ stays Hard (anti-flap).
        c.tick(
            2_000,
            ReleaseInputs {
                cgroup_current: 88,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(c.mode(), PressureMode::Hard);
        // Drop to 80% (below 85%) ⇒ steps down to Soft.
        c.tick(
            3_000,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(c.mode(), PressureMode::Soft);
    }

    #[test]
    fn a_fresh_pressure_notification_forces_at_least_soft() {
        // §21.2/§21.5: a rise in the OS memory-pressure notification count (PSI-style)
        // forces at least Soft even with no cgroup limit known, so the controller reacts
        // to kernel pressure it cannot see as a utilization watermark. Once the
        // notifications stop rising it de-escalates.
        let mut c = ReleaseController::new(DecayConfig::default());
        // Baseline establishes the notification count (0); no pressure ⇒ Normal.
        c.tick(0, base_inputs());
        assert_eq!(
            c.mode(),
            PressureMode::Normal,
            "no notification yet ⇒ Normal"
        );
        // A fresh notification ⇒ at least Soft (no cgroup signal at all).
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                pressure_notifications: 1,
                ..base_inputs()
            },
        );
        assert_eq!(
            plan.mode,
            PressureMode::Soft,
            "a fresh pressure notification forces Soft"
        );
        // Soft releases the empty-hugepage supply beyond the (idle ⇒ zero) reserve.
        assert_eq!(plan.release_empty_hugepages_bytes, 8 * 1024 * 1024);
        // No further notification and no cgroup pressure ⇒ de-escalate to Normal.
        let plan = c.tick(
            2_000,
            ReleaseInputs {
                pressure_notifications: 1,
                ..base_inputs()
            },
        );
        assert_eq!(
            plan.mode,
            PressureMode::Normal,
            "no fresh notification (count unchanged) ⇒ back to Normal"
        );
    }

    #[test]
    fn emergency_de_escalates_to_the_live_watermark_once_the_trigger_clears() {
        // §21.5: Emergency is a transient trigger (alloc failure / cgroup-critical).
        // When it clears, the mode falls back to whatever the cgroup watermark warrants
        // — it must not latch in Emergency.
        let mut c = ReleaseController::new(DecayConfig::default());
        // Alloc failure at 80% utilization ⇒ Emergency (the trigger overrides the
        // watermark).
        c.tick(
            0,
            ReleaseInputs {
                alloc_failed: true,
                cgroup_current: 80,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(c.mode(), PressureMode::Emergency);
        // Trigger clears but utilization is still 80% ⇒ falls back to Soft, not Normal.
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(
            plan.mode,
            PressureMode::Soft,
            "Emergency falls back to the live watermark mode, not all the way to Normal"
        );
        // Utilization then drops below the soft watermark ⇒ Normal.
        let plan = c.tick(
            2_000,
            ReleaseInputs {
                cgroup_current: 50,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(plan.mode, PressureMode::Normal);
    }

    #[test]
    fn normal_mode_preserves_hugepages_and_respects_the_decay_timer() {
        let mut c = ReleaseController::new(DecayConfig::default());
        // First tick at t=0 establishes the dirty clock; no pressure.
        let plan = c.tick(0, base_inputs());
        assert_eq!(plan.mode, PressureMode::Normal);
        // Normal does NOT release empty hugepages (preserve coverage, §20.3).
        assert_eq!(plan.release_empty_hugepages_bytes, 0);
        // Dirty is not yet aged (decay 10s, only 5s elapsed) ⇒ no purge.
        let plan = c.tick(5_000, base_inputs());
        assert_eq!(
            plan.purge_dirty_bytes, 0,
            "dirty not yet aged past dirty_decay_ms"
        );
        // After 11s of continuous dirtiness ⇒ purge eligible.
        let plan = c.tick(11_000, base_inputs());
        assert_eq!(plan.purge_dirty_bytes, 2_000_000, "aged dirty is purged");
    }

    #[test]
    fn purge_skips_dirty_on_hot_hugepages() {
        let mut c = ReleaseController::new(DecayConfig::low_rss()); // zero decay
        let i = ReleaseInputs {
            dirty_bytes: 3_000_000,
            hot_dirty_bytes: 1_000_000,
            ..base_inputs()
        };
        let plan = c.tick(0, i);
        // Only the non-hot dirty (3M - 1M) is purged (§21.3 rung 3).
        assert_eq!(plan.purge_dirty_bytes, 2_000_000);
    }

    #[test]
    fn cold_sparse_subrelease_only_under_hard_or_worse() {
        let mut c = ReleaseController::new(DecayConfig::default());
        // Soft (80%): no subrelease (preserve coverage).
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(plan.subrelease_cold_sparse_bytes, 0);
        // Hard (95%): subrelease the cold-sparse supply.
        let plan = c.tick(
            2_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert_eq!(plan.subrelease_cold_sparse_bytes, 4 * 1024 * 1024);
    }

    // --- the §21.1 R2 anti-oscillation property (W12-2c) --------------------

    #[test]
    fn demand_reserve_grows_with_rate_shrinks_with_pressure_and_caps_at_peak() {
        let peak = 100 * 1024 * 1024;
        // Idle ⇒ no reserve.
        assert_eq!(demand_reserve(0, peak, 0, PressureMode::Normal), 0);
        // One window (1s) of allocation at 16 MiB/s ⇒ ~16 MiB reserve.
        let busy = demand_reserve(16 * 1024 * 1024, peak, 0, PressureMode::Normal);
        assert_eq!(busy, 16 * 1024 * 1024);
        // Higher rate ⇒ strictly larger reserve (monotonic).
        assert!(demand_reserve(32 * 1024 * 1024, peak, 0, PressureMode::Normal) > busy);
        // Expensive refills (misses) enlarge the reserve.
        assert!(demand_reserve(16 * 1024 * 1024, peak, 500, PressureMode::Normal) > busy);
        // Pressure attenuates it; Emergency reserves nothing (§36.5).
        assert!(demand_reserve(16 * 1024 * 1024, peak, 0, PressureMode::Hard) < busy);
        assert_eq!(
            demand_reserve(16 * 1024 * 1024, peak, 0, PressureMode::Emergency),
            0
        );
        // Never reserve more free memory than recently existed (the peak cap).
        assert_eq!(
            demand_reserve(1024 * 1024 * 1024, 8 * 1024 * 1024, 0, PressureMode::Normal),
            8 * 1024 * 1024
        );
    }

    /// Release a tick under Soft pressure with `alloc_at_1s` cumulative bytes allocated
    /// by the second tick — the §21.1 oscillation harness (same pressure, varying
    /// churn).
    fn release_under_churn(alloc_at_1s: u64) -> ReleasePlan {
        let mut c = ReleaseController::new(DecayConfig::default());
        c.tick(
            0,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                allocated_bytes_total: 0,
                ..base_inputs()
            },
        );
        c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                allocated_bytes_total: alloc_at_1s,
                ..base_inputs()
            },
        )
    }

    #[test]
    fn oscillation_churn_releases_strictly_less_than_idle() {
        // The load-bearing §21.1 R2 property: at the *same* pressure, a churning
        // workload holds a reserve and releases strictly fewer empty hugepages than an
        // idle one — so it does not release memory it is about to refault.
        let idle = release_under_churn(0);
        let churn = release_under_churn(64 * 1024 * 1024); // 64 MiB/s
        assert_eq!(idle.demand_reserve_bytes, 0, "idle ⇒ no reserve");
        assert_eq!(
            idle.release_empty_hugepages_bytes,
            8 * 1024 * 1024,
            "idle releases the whole empty-hugepage supply"
        );
        assert!(churn.demand_reserve_bytes > 0, "churn ⇒ a reserve is held");
        assert!(
            churn.release_empty_hugepages_bytes < idle.release_empty_hugepages_bytes,
            "churn releases strictly less (held back against refault, §21.1 R2)"
        );
    }

    #[test]
    fn recent_peak_free_decays_so_a_stale_spike_does_not_pin_the_reserve() {
        // §21.4 keys the reserve cap on the *recent* peak free, not an all-time
        // high-water mark. A one-off free spike must not hold the demand-reserve cap
        // high forever: once free has been small for a full PEAK_DECAY_MS, a churn
        // tick's reserve is bounded by the (small) recent free, not the stale spike —
        // so the controller stops over-retaining RSS after the spike drains.
        let mut c = ReleaseController::new(DecayConfig::default());
        // t=0: a transient 100 MiB free spike establishes a high peak.
        c.tick(
            0,
            ReleaseInputs {
                idle_cache_bytes: 100 * 1024 * 1024,
                ..ReleaseInputs::default()
            },
        );
        // t=PEAK_DECAY_MS: free has collapsed to ~2 MiB and a 80 MiB/window allocation
        // burst would, with a stale 100 MiB peak, reserve the whole alloc-rate base
        // (10 MiB). With the leaky peak-hold the peak has relaxed to the recent ~2 MiB,
        // so the reserve is capped there instead.
        let plan = c.tick(
            PEAK_DECAY_MS,
            ReleaseInputs {
                idle_cache_bytes: 1024 * 1024,
                empty_backed_hugepage_bytes: 1024 * 1024,
                allocated_bytes_total: 80 * 1024 * 1024,
                ..ReleaseInputs::default()
            },
        );
        assert_eq!(
            plan.demand_reserve_bytes,
            2 * 1024 * 1024,
            "reserve capped by the decayed recent peak (~2 MiB), not the stale 100 MiB \
             spike (which would have allowed the full 10 MiB alloc-rate reserve)"
        );
        // Decay never strands the cap low: when free regrows, the peak catches up
        // immediately (a new high), so the reserve can again cover a large supply.
        let regrown = c.tick(
            PEAK_DECAY_MS + 1_000,
            ReleaseInputs {
                idle_cache_bytes: 50 * 1024 * 1024,
                // +40 MiB allocated over the 1 s since the prior tick ⇒ 40 MiB/s.
                allocated_bytes_total: (80 + 40) * 1024 * 1024,
                ..ReleaseInputs::default()
            },
        );
        assert!(
            regrown.demand_reserve_bytes > 2 * 1024 * 1024,
            "a regrown free pool lifts the recent peak again (new high adopted at once)"
        );
    }

    #[test]
    fn recent_peak_decay_is_independent_of_tick_frequency() {
        // §21.4 review finding: the recent-peak cap must decay by *elapsed time*, not
        // per-tick. A per-tick fractional decay compounds (exponential) and leaves a
        // long-dead spike capping the reserve high when the pump ticks often — e.g. ~1/e
        // of the excess still present after a full horizon of 1 ms ticks. The anchored
        // decay makes the cap a pure function of elapsed time, so fine and coarse tick
        // cadences over the same span yield the *same* reserve.
        //
        // Drive a 100 MiB spike then hold ~1 MiB free across a half-horizon, ticking at
        // `step_ms`, with a steady 128 MiB/s allocation so the (cap, not the rate) bounds
        // the reserve. Returns the resulting demand reserve.
        fn reserve_after(step_ms: u64) -> u64 {
            const SPIKE: u64 = 100 * 1024 * 1024;
            const LOW: u64 = 1024 * 1024;
            const RATE: u64 = 128 * 1024 * 1024; // base ≫ the half-decayed cap
            let mut c = ReleaseController::new(DecayConfig::default());
            c.tick(
                0,
                ReleaseInputs {
                    idle_cache_bytes: SPIKE,
                    ..ReleaseInputs::default()
                },
            );
            let span = PEAK_DECAY_MS / 2;
            let mut now = 0u64;
            let mut allocated = 0u64;
            while now < span {
                now += step_ms;
                allocated += RATE * step_ms / 1_000;
                c.tick(
                    now,
                    ReleaseInputs {
                        idle_cache_bytes: LOW,
                        allocated_bytes_total: allocated,
                        ..ReleaseInputs::default()
                    },
                );
            }
            c.stats().demand_reserve_bytes
        }
        // 1 ms ticks vs one half-horizon tick: identical cadence-independent result.
        let fine = reserve_after(1);
        let coarse = reserve_after(PEAK_DECAY_MS / 2);
        assert_eq!(
            fine, coarse,
            "recent-peak decay must not depend on tick cadence (frequency independence)"
        );
        // At half-life the cap is roughly halfway between the 1 MiB floor and the 100 MiB
        // spike (~50 MiB), i.e. the spike is half-remembered — emphatically not the ~37%
        // (1/e) the old per-tick compounding bug left after a *full* horizon.
        assert!(
            fine > 40 * 1024 * 1024 && fine < 60 * 1024 * 1024,
            "half-decayed cap ~50 MiB (linear age-based decay), got {fine}"
        );
    }

    // --- rate limiting & backlog (§20.2/§20.3) ------------------------------

    #[test]
    fn rate_cap_limits_release_and_accrues_backlog() {
        // A 1 MiB/s rate cap with a big idle drop: only ~1 MiB is granted per second,
        // the rest becomes backlog.
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1024 * 1024,
            ..DecayConfig::low_rss()
        };
        let mut c = ReleaseController::new(cfg);
        c.tick(
            0,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        // 1s elapsed ⇒ ~1 MiB budget; desired is far larger ⇒ backlog grows.
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert!(
            plan.total_bytes() <= 1024 * 1024,
            "granted within the rate cap"
        );
        assert!(
            c.backlog_bytes() > 0,
            "unmet desire accrues as backlog (§20.3)"
        );
    }

    #[test]
    fn emergency_bypasses_the_rate_cap() {
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1,
            ..DecayConfig::default()
        };
        let mut c = ReleaseController::new(cfg);
        c.tick(0, base_inputs());
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                alloc_failed: true,
                ..base_inputs()
            },
        );
        assert_eq!(plan.mode, PressureMode::Emergency);
        // Despite a 1 B/s cap, Emergency releases the whole empty-hugepage supply.
        assert_eq!(plan.release_empty_hugepages_bytes, 8 * 1024 * 1024);
        assert_eq!(c.backlog_bytes(), 0, "Emergency clears the backlog");
    }

    #[test]
    fn background_purge_disabled_suppresses_routine_work_but_not_emergency() {
        let cfg = DecayConfig {
            background_purge_enabled: false,
            ..DecayConfig::low_rss()
        };
        let mut c = ReleaseController::new(cfg);
        // High utilization but background purge off ⇒ no routine release.
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert!(
            plan.is_empty(),
            "routine background purge suppressed by the knob"
        );
        // Emergency still acts.
        let plan = c.tick(
            2_000,
            ReleaseInputs {
                alloc_failed: true,
                ..base_inputs()
            },
        );
        assert!(!plan.is_empty(), "emergency release is never suppressed");
    }

    #[test]
    fn cpu_pressure_yields_routine_work() {
        let mut c = ReleaseController::new(DecayConfig::low_rss());
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 80,
                cgroup_max: 100,
                cpu_pressure: true,
                ..base_inputs()
            },
        );
        assert!(
            plan.is_empty(),
            "yield routine work under CPU pressure (§20.3)"
        );
    }

    // --- latency classes (§36.11, W12-4) ------------------------------------

    #[test]
    fn fast_only_arena_skips_blocking_rungs() {
        // A real-time arena (FastOnly ceiling) may drain caches but must not plan any
        // MayBlock release (madvise/decommit) or BoundedSlow subrelease.
        let mut c = ReleaseController::with(
            DecayConfig::low_rss(),
            PressureThresholds::default(),
            LatencyClass::FastOnly,
            0,
        );
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        assert!(
            plan.drain_caches_bytes > 0,
            "FastOnly cache drain is permitted"
        );
        assert_eq!(
            plan.release_empty_hugepages_bytes, 0,
            "no MayBlock release on a fast-only arena"
        );
        assert_eq!(plan.purge_dirty_bytes, 0, "no MayBlock purge");
        assert_eq!(
            plan.subrelease_cold_sparse_bytes, 0,
            "no BoundedSlow subrelease"
        );
    }

    #[test]
    fn bounded_slow_arena_converts_dirty_to_muzzy_instead_of_blocking_purge() {
        // §21.3/§36.11: a `bounded_slow_path` arena cannot take the MayBlock direct
        // purge (rung 3, MADV_DONTNEED) but CAN take the cheaper BoundedSlow lazy
        // conversion (rung 4, MADV_FREE → muzzy). So aged dirty is converted to muzzy
        // rather than force-purged, and the MayBlock empty-hugepage release is skipped
        // even though Soft pressure would otherwise fire it — the rung is gated by the
        // arena's latency ceiling, not just the mode.
        let mut c = ReleaseController::with(
            DecayConfig::low_rss(), // zero decay ⇒ dirty immediately aged
            PressureThresholds::default(),
            LatencyClass::BoundedSlow,
            0,
        );
        let plan = c.tick(
            0,
            ReleaseInputs {
                cgroup_current: 80, // Soft: rung 2 would fire but for the latency gate
                cgroup_max: 100,
                dirty_bytes: 3_000_000,
                hot_dirty_bytes: 1_000_000,
                ..base_inputs()
            },
        );
        assert_eq!(plan.mode, PressureMode::Soft);
        // FastOnly cache drain is permitted.
        assert!(plan.drain_caches_bytes > 0, "FastOnly drain permitted");
        // No MayBlock work: neither the direct dirty purge nor the empty-hugepage
        // release, despite Soft pressure.
        assert_eq!(plan.purge_dirty_bytes, 0, "no MayBlock direct purge");
        assert_eq!(
            plan.release_empty_hugepages_bytes, 0,
            "no MayBlock hugepage release on a bounded-slow arena"
        );
        // The non-hot aged dirty (3M − 1M) is lazily converted to muzzy (rung 4).
        assert_eq!(
            plan.dirty_to_muzzy_bytes, 2_000_000,
            "aged non-hot dirty is converted to muzzy (the BoundedSlow lazy purge)"
        );
    }

    // --- emergency reserve independence (§36.5, W12-3b) ---------------------

    #[test]
    fn for_arena_inherits_decay_and_latency_from_the_policy() {
        use crate::arena::ArenaPolicy;
        // A real-time (fast-only) arena with the low-RSS decay drives a controller that
        // adopts both knobs: it skips the blocking rungs even under Hard pressure.
        let policy = ArenaPolicy::explicit()
            .with_latency(LatencyClass::FastOnly)
            .with_decay(DecayConfig::low_rss());
        let mut c = ReleaseController::for_arena(&policy);
        assert_eq!(c.max_latency(), LatencyClass::FastOnly);
        assert_eq!(c.config(), DecayConfig::low_rss());
        let plan = c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        // FastOnly ⇒ cache drain only, no MayBlock release/purge or BoundedSlow subrelease.
        assert!(plan.drain_caches_bytes > 0);
        assert_eq!(plan.release_empty_hugepages_bytes, 0);
        assert_eq!(plan.purge_dirty_bytes, 0);
        assert_eq!(plan.subrelease_cold_sparse_bytes, 0);
    }

    #[test]
    fn emergency_reserve_is_constant_and_heap_independent() {
        let c = ReleaseController::with(
            DecayConfig::default(),
            PressureThresholds::default(),
            LatencyClass::MayBlock,
            64 * 1024,
        );
        // The reserve is fixed at construction and never funded from the heap it
        // protects (§36.5): no tick can change it.
        assert_eq!(c.emergency_reserve_bytes(), 64 * 1024);
    }

    // --- arena fairness (§20.3, W12-1b) -------------------------------------

    #[test]
    fn next_fair_arena_round_robins() {
        let mut c = ReleaseController::new(DecayConfig::default());
        let mut seq = [0u32; 7];
        for s in &mut seq {
            *s = c.next_fair_arena(3).0;
        }
        assert_eq!(
            seq,
            [0, 1, 2, 0, 1, 2, 0],
            "fair round-robin across 3 arenas"
        );
        // Zero arenas ⇒ always the default, cursor unmoved.
        assert_eq!(c.next_fair_arena(0), ArenaId::DEFAULT);
    }

    #[test]
    fn stats_surface_observed_alloc_and_free_rates() {
        // The §21.2 alloc/free rates are derived from the cumulative-counter deltas and
        // surfaced (so both `*_total` inputs are genuinely consumed, not sampled-unused).
        let mut c = ReleaseController::new(DecayConfig::default());
        c.tick(
            0,
            ReleaseInputs {
                allocated_bytes_total: 0,
                freed_bytes_total: 0,
                ..base_inputs()
            },
        );
        // 1 s later: +16 MiB allocated, +4 MiB freed ⇒ 16 MiB/s alloc, 4 MiB/s free.
        c.tick(
            1_000,
            ReleaseInputs {
                allocated_bytes_total: 16 * 1024 * 1024,
                freed_bytes_total: 4 * 1024 * 1024,
                ..base_inputs()
            },
        );
        let s = c.stats();
        assert_eq!(s.alloc_rate_bps, 16 * 1024 * 1024);
        assert_eq!(s.free_rate_bps, 4 * 1024 * 1024);
    }

    #[test]
    fn stats_track_ticks_and_planned_bytes() {
        let mut c = ReleaseController::new(DecayConfig::low_rss());
        c.tick(0, base_inputs());
        c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        let s = c.stats();
        assert_eq!(s.ticks, 2);
        assert!(s.active_ticks >= 1);
        assert!(s.planned_bytes_total > 0);
        assert_eq!(s.mode, c.mode());
    }

    #[test]
    fn dirty_is_retained_not_lazily_purged_before_decay() {
        // #5: in Normal/Soft, dirty must be RETAINED (neither purged nor lazily
        // converted to muzzy) until `dirty_decay_ms` elapses. Before the fix, rung 4
        // converted all purgeable dirty to muzzy on the very first background tick,
        // ignoring the decay policy and raising refault risk.
        let mut c = ReleaseController::new(DecayConfig::default()); // 10s dirty decay
        let plan = c.tick(0, base_inputs());
        assert_eq!(plan.mode, PressureMode::Normal);
        assert_eq!(plan.purge_dirty_bytes, 0, "no forced purge before decay");
        assert_eq!(
            plan.dirty_to_muzzy_bytes, 0,
            "dirty retained, not lazily converted before dirty_decay_ms"
        );
        let plan = c.tick(5_000, base_inputs());
        assert_eq!(plan.dirty_to_muzzy_bytes, 0, "still retained at 5s");
    }

    #[test]
    fn muzzy_is_released_after_its_decay_interval() {
        // #10: muzzy must return to the OS once it has aged past `muzzy_decay_ms`, not
        // only under Emergency — otherwise the muzzy-decay knob never reclaims RSS.
        let mut c = ReleaseController::new(DecayConfig::default()); // 10s muzzy decay
        let plan = c.tick(0, base_inputs()); // establishes the muzzy clock
        assert_eq!(plan.mode, PressureMode::Normal);
        assert_eq!(plan.release_muzzy_bytes, 0, "muzzy retained before decay");
        let plan = c.tick(5_000, base_inputs());
        assert_eq!(plan.release_muzzy_bytes, 0, "still retained at 5s");
        let plan = c.tick(11_000, base_inputs());
        assert_eq!(
            plan.release_muzzy_bytes, 500_000,
            "aged muzzy is released to the OS, with no pressure at all"
        );
        assert_eq!(plan.mode, PressureMode::Normal);
    }

    #[test]
    fn backlog_survives_a_tick_that_plans_less_than_the_budget() {
        // #11: a deferred backlog must be reduced only by what is ACTUALLY planned, not
        // by the full granted budget. Before the fix, a later tick whose current work
        // was below the budget silently forgave real pending release work (RSS stayed
        // high while the controller reported progress).
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1024 * 1024,
            ..DecayConfig::low_rss()
        };
        let mut c = ReleaseController::new(cfg);
        // Accrue a backlog: large work under Hard, rate-capped to ~1 MiB/s.
        c.tick(
            0,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        c.tick(
            1_000,
            ReleaseInputs {
                cgroup_current: 95,
                cgroup_max: 100,
                ..base_inputs()
            },
        );
        let backlog = c.backlog_bytes();
        assert!(backlog > 0, "backlog accrued under the rate cap");
        // A later tick whose *current* work is below the generous budget must not forgive
        // the pending remainder: the credit is what was actually planned, not the grant.
        let small = ReleaseInputs {
            dirty_bytes: 4096,
            cgroup_current: 95,
            cgroup_max: 100,
            ..ReleaseInputs::default()
        };
        let plan = c.tick(100_000, small);
        assert!(
            plan.total_bytes() > 0 && plan.total_bytes() < backlog,
            "this tick plans some, but far less than the carried backlog"
        );
        assert!(
            c.backlog_bytes() > 0,
            "a well-budgeted tick with real remaining work must not forgive the backlog"
        );

        // But once the supply is genuinely gone, so is the debt: a backlog is release
        // work that still *exists*. Leaving it latched made a fully-drained, idle
        // allocator report megabytes of owed release forever (and fed the §21.4
        // anti-oscillation brake a debt no memory backs).
        let plan = c.tick(200_000, ReleaseInputs::default());
        assert_eq!(plan.total_bytes(), 0, "no work exists any more");
        assert_eq!(
            c.backlog_bytes(),
            0,
            "the backlog must drain with the supply, not latch at its high-water mark"
        );
    }

    #[test]
    fn backlog_stays_bounded_by_supply_under_a_persistent_rate_capped_drop() {
        // §20.3: the backlog is "release work owed," so it must track the *remaining*
        // pending supply and never balloon past it. The plan is recomputed from the
        // absolute current supply each tick, so summing this tick's desire onto the
        // carried backlog would double-count the *same* unreleased bytes and diverge
        // (~15 MiB accrued every tick under the cap below). Taking the max keeps it
        // bounded by the actual supply.
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1024 * 1024, // 1 MiB/s — far below the supply
            ..DecayConfig::low_rss()
        };
        // The absolute releasable desire of one Hard-pressure tick, measured with no
        // rate cap, is the bound the backlog must never exceed.
        let supply = ReleaseInputs {
            cgroup_current: 95,
            cgroup_max: 100,
            ..base_inputs()
        };
        let total_supply = ReleaseController::new(DecayConfig::low_rss())
            .tick(0, supply)
            .total_bytes();
        assert!(
            total_supply > 4 * 1024 * 1024,
            "a meaningful supply to defer"
        );

        // Drive 200 rate-capped ticks against the *same* persistent supply (the host has
        // not drained it yet, so every tick re-sees the full ~16 MiB).
        let mut c = ReleaseController::new(cfg);
        for t in 0..=200u64 {
            c.tick(t * 1_000, supply);
        }
        // A summing accumulator would report ~200× the real supply here; the max-based
        // accounting stays within the actual pending memory.
        assert!(
            c.backlog_bytes() <= total_supply,
            "backlog {} must stay bounded by the ~{} of pending supply, not diverge",
            c.backlog_bytes(),
            total_supply
        );
        // And it is still nonzero (the cap genuinely defers most of the supply).
        assert!(c.backlog_bytes() > 0, "the rate cap still defers real work");
    }
}

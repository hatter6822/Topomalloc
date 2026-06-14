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
//! 3. purge dirty spans **not on hot hugepages**,
//! 4. convert dirty → muzzy where cheap,
//! 5. subrelease cold-sparse partial hugepages (H-005-guarded by the mechanism),
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
    /// Rung 6 — emergency shrink bytes (Emergency only): forced global cache shrink.
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
}

/// How far ahead the demand reserve looks: the reserve covers roughly the bytes the
/// application would allocate in this window at the recent rate (§21.4). One refill
/// horizon — long enough to bridge a churn burst, short enough not to hoard.
const RESERVE_WINDOW_MS: u64 = 1_000;

/// The §21.4 demand reserve (W12-2c) — the anti-oscillation brake. Withholds from
/// release roughly the bytes the application will allocate during one
/// [`RESERVE_WINDOW_MS`] at `alloc_rate_bps`, enlarged when refills are expensive
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
    last_allocated_total: u64,
    last_freed_total: u64,
    last_pressure_notifications: u64,
    recent_peak_free: u64,
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
            last_allocated_total: 0,
            last_freed_total: 0,
            last_pressure_notifications: 0,
            recent_peak_free: 0,
            backlog_bytes: 0,
            planned_bytes_total: 0,
            demand_reserve_bytes: 0,
            ticks: 0,
            active_ticks: 0,
            next_arena: 0,
        }
    }

    /// The active decay configuration.
    pub fn config(&self) -> DecayConfig {
        self.config
    }

    /// Replace the decay configuration (a control-plane write, plan 07 W20).
    pub fn set_config(&mut self, config: DecayConfig) {
        self.config = config;
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

        // Track the recent peak of releasable-free memory for the reserve cap.
        let free_now = inputs
            .idle_cache_bytes
            .saturating_add(inputs.dirty_bytes)
            .saturating_add(inputs.muzzy_bytes)
            .saturating_add(inputs.empty_backed_hugepage_bytes);
        if free_now > self.recent_peak_free {
            self.recent_peak_free = free_now;
        }

        // Alloc rate (bytes/sec) from the cumulative counter delta over the interval.
        let alloc_rate_bps = rate_per_sec(
            inputs
                .allocated_bytes_total
                .saturating_sub(self.last_allocated_total),
            elapsed_ms,
        );

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

        // §21.5 pressure mode with hysteresis (escalate immediately, de-escalate only
        // past the margin). A rise in pressure notifications forces at least Soft.
        let pressure_rose = inputs.pressure_notifications > self.last_pressure_notifications;
        self.mode = self.classify_mode(&inputs, pressure_rose);

        // §21.4 demand reserve (the anti-oscillation brake).
        let reserve = demand_reserve(
            alloc_rate_bps,
            self.recent_peak_free,
            inputs.refill_miss_rate_ppk,
            self.mode,
        );
        self.demand_reserve_bytes = reserve;

        // The dirty-decay gate (§20.2): in Normal/Soft, only purge dirty that has been
        // continuously resident longer than `dirty_decay_ms`; Hard/Emergency ignore
        // the timer (accelerated purge, §21.5).
        let dirty_aged = self.mode.severity() >= PressureMode::Hard.severity()
            || now_ms.saturating_sub(self.dirty_since_ms) >= self.config.dirty_decay_ms;

        let mut plan = self.plan_ladder(&inputs, reserve, dirty_aged);

        // Per-tick rate budget (§20.2): bytes allowed this interval, plus any backlog
        // carried from prior under-served ticks. `0` rate ⇒ unlimited.
        let desired = plan.total_bytes().saturating_add(self.backlog_bytes);
        if self.config.release_rate_bytes_per_sec != 0 && self.mode != PressureMode::Emergency {
            // The backlog is already folded into `desired`, so the budget is just this
            // interval's allowance; the unmet remainder becomes the new backlog.
            let budget = rate_budget(self.config.release_rate_bytes_per_sec, elapsed_ms);
            let granted = budget.min(desired);
            scale_plan_to_budget(&mut plan, granted);
            self.backlog_bytes = desired.saturating_sub(granted);
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
    fn plan_ladder(&self, inputs: &ReleaseInputs, reserve: u64, dirty_aged: bool) -> ReleasePlan {
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

        // Rung 4 — convert dirty→muzzy where cheap (BoundedSlow lazy purge): the dirty
        // we did *not* fully purge this tick is marked discardable. Always eligible
        // when the lazy path is permitted; bytes already counted under rung 3 are
        // excluded so the two rungs do not double-count the same dirty bytes.
        if LatencyClass::BoundedSlow.permitted_under(self.max_latency) {
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

/// Bytes permitted by a per-second rate over an interval; a zero-length interval
/// grants a single byte-rate-free *floor* of one window so the first tick can act.
fn rate_budget(rate_bps: u64, elapsed_ms: u64) -> u64 {
    let ms = if elapsed_ms == 0 {
        RESERVE_WINDOW_MS
    } else {
        elapsed_ms
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

    // --- emergency reserve independence (§36.5, W12-3b) ---------------------

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
}

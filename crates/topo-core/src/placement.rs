// SPDX-License-Identifier: MIT
//! Lifetime, hotness & allocation-site placement policy (§24, plan 07 W14).
//!
//! This module is the **learning policy** (§24.4): it turns *sampled* allocation
//! observations into per-site [`AllocationSiteProfile`]s and distils each into an
//! advisory [`PlaceHints`] the hugepage filler consumes to group cold / short-lived
//! / long-lived-hot objects (§24.6–§24.8). Like the W12 [`ReleaseController`] it is a
//! **pure, `no_std`, host-driven** object: it reads no clock of its own, makes no
//! provider calls, and never allocates. The sampler (the minimal W17-3 slice in
//! `crate::sampling`, wired live in `topo-abi`) feeds it `(now_ms, stack_id, …)`; the
//! placement decision falls out of [`SiteProfileTable::place_hints`].
//!
//! ## The non-negotiable safety boundary (§24.5)
//!
//! Everything here is **policy**. A profile may be missing, stale, or *wrong*, and a
//! wrong profile can only steer which hugepage a run lands in — it can **never** change
//! an allocation's size, alignment, validity, or free path. The output type is
//! [`PlaceHints`] (hotness + lifetime), the *same* advisory the filler already treats
//! as score-only input (`huge::PlaceHints` — "a wrong hint can hurt fragmentation but
//! can never misplace a live object"). So the §24.5 boundary holds **by construction**:
//! there is no path from a profile to a size/alignment/validity decision. The
//! `placement_never_changes_*` tests pin it as the fixed wall W14-3 requires.
//!
//! Because it adds no abstract state-machine transition (it only *chooses* a hint the
//! certified filler then scores), there is no Lean obligation — exactly as for the W13
//! NUMA router (placement is policy, §2.4).
//!
//! [`ReleaseController`]: crate::ReleaseController
//! [`PlaceHints`]: crate::huge::PlaceHints

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::flags::Lifetime;
use crate::generated::tables::SIZE_CLASSES;
use crate::huge::{Hotness, PlaceHints};

/// An opaque **allocation-site identity** (§24.4 `stack_id`): a hash of the captured
/// call stack at a sampled allocation. The allocator never dereferences it — it is only
/// a profile-table key — so a collision degrades placement quality, never safety.
/// [`UNKNOWN`](StackId::UNKNOWN) is the reserved "no site captured" key (an empty/failed
/// unwind), so the table never confuses an un-attributed sample with a real site.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct StackId(pub u64);

impl StackId {
    /// The reserved key for a sample whose stack could not be captured.
    pub const UNKNOWN: StackId = StackId(0);

    /// Whether this is a real captured site (not [`UNKNOWN`](Self::UNKNOWN)).
    #[inline]
    pub const fn is_known(self) -> bool {
        self.0 != 0
    }
}

/// The §24.2 **lifetime classes**: the allocator's *inferred* taxonomy of how long an
/// object lives, richer than the coarse user-supplied [`Lifetime`] hint (§10.4). The
/// numeric order is monotone in expected lifetime, so "longer-lived than" is a `<`
/// comparison and [`dominant`](LifetimeHistogram::dominant) / right-censoring can reason
/// about "at least this class".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[repr(u8)]
pub enum LifetimeClass {
    /// No lifetime evidence yet (the default before any free is observed).
    #[default]
    Unknown = 0,
    /// Freed almost immediately (§24.2 *Ephemeral*).
    Ephemeral = 1,
    /// Request / transaction lifetime (§24.2 *Short*).
    Short = 2,
    /// Seconds to minutes (§24.2 *Medium*).
    Medium = 3,
    /// A process phase or object-cache lifetime (§24.2 *Long*).
    Long = 4,
    /// Expected to live until arena reset / process exit (§24.2 *Persistent*).
    Persistent = 5,
}

impl LifetimeClass {
    /// The number of distinct classes (the [`LifetimeHistogram`] width).
    pub const COUNT: usize = 6;

    // §24.2 age thresholds (milliseconds). Tunable policy, *not* a safety input. Chosen
    // to mirror the SPEC prose: "freed quickly" / "request lifetime" / "seconds to
    // minutes" / "process phase". A measured age maps to the class whose upper bound it
    // is strictly below; anything at/above the Long bound is Persistent.
    const EPHEMERAL_MAX_MS: u64 = 1; // < 1 ms: gone before the next tick.
    const SHORT_MAX_MS: u64 = 1_000; // < 1 s: a request/transaction.
    const MEDIUM_MAX_MS: u64 = 60_000; // < 1 min: seconds to minutes.
    const LONG_MAX_MS: u64 = 3_600_000; // < 1 h: a process phase.

    /// Classify a **measured** object age (`free_ms - alloc_ms`) into its §24.2 class
    /// (total; never returns [`Unknown`](Self::Unknown) — a measured age is always known).
    #[inline]
    pub const fn from_age_ms(age_ms: u64) -> LifetimeClass {
        if age_ms < Self::EPHEMERAL_MAX_MS {
            LifetimeClass::Ephemeral
        } else if age_ms < Self::SHORT_MAX_MS {
            LifetimeClass::Short
        } else if age_ms < Self::MEDIUM_MAX_MS {
            LifetimeClass::Medium
        } else if age_ms < Self::LONG_MAX_MS {
            LifetimeClass::Long
        } else {
            LifetimeClass::Persistent
        }
    }

    /// The histogram index for this class (`0..COUNT`).
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Recover a class from its histogram index (total: an out-of-range index reads as
    /// [`Unknown`](Self::Unknown), so the conversion never panics).
    #[inline]
    pub const fn from_index(i: usize) -> LifetimeClass {
        match i {
            1 => LifetimeClass::Ephemeral,
            2 => LifetimeClass::Short,
            3 => LifetimeClass::Medium,
            4 => LifetimeClass::Long,
            5 => LifetimeClass::Persistent,
            _ => LifetimeClass::Unknown,
        }
    }

    /// The stable string used in profile dumps / diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            LifetimeClass::Unknown => "unknown",
            LifetimeClass::Ephemeral => "ephemeral",
            LifetimeClass::Short => "short",
            LifetimeClass::Medium => "medium",
            LifetimeClass::Long => "long",
            LifetimeClass::Persistent => "persistent",
        }
    }

    /// Project the inferred class onto the coarse 4-valued filler [`Lifetime`] hint
    /// (§10.4), the axis the hugepage filler groups by (§19.5). The richer Ephemeral /
    /// Persistent classes fold into the nearest coarse bucket (Ephemeral→Short,
    /// Persistent→Long), and [`Unknown`](Self::Unknown) maps to
    /// [`Unspecified`](Lifetime::Unspecified) — i.e. *no* grouping signal, the safe
    /// default.
    #[inline]
    pub const fn to_hint(self) -> Lifetime {
        match self {
            LifetimeClass::Unknown => Lifetime::Unspecified,
            LifetimeClass::Ephemeral | LifetimeClass::Short => Lifetime::Short,
            LifetimeClass::Medium => Lifetime::Medium,
            LifetimeClass::Long | LifetimeClass::Persistent => Lifetime::Long,
        }
    }
}

/// A per-class **lifetime histogram** (§24.4 `lifetime_histogram`): counts of observed
/// (and right-censored, §31.4) object lifetimes by [`LifetimeClass`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LifetimeHistogram {
    counts: [u32; LifetimeClass::COUNT],
    /// Of [`total`](Self::total), how many are *right-censored* — still live at the last
    /// dump, so their recorded class is a **lower bound** on the true lifetime (§31.4
    /// "account for right-censored lifetimes"). Kept separate so a dump can mark them.
    censored: u32,
}

impl LifetimeHistogram {
    /// Record one **completed** lifetime in `class` (a sampled object that was freed).
    #[inline]
    fn record(&mut self, class: LifetimeClass) {
        let c = &mut self.counts[class.index()];
        *c = c.saturating_add(1);
    }

    /// Record one **right-censored** lifetime: the object is still live, so its true
    /// class is *at least* `floor` (§31.4). Counted at `floor` (a conservative lower
    /// bound) and tallied in [`censored`](Self::censored).
    #[inline]
    fn record_censored(&mut self, floor: LifetimeClass) {
        let c = &mut self.counts[floor.index()];
        *c = c.saturating_add(1);
        self.censored = self.censored.saturating_add(1);
    }

    /// Total observations across all classes.
    #[inline]
    pub fn total(&self) -> u32 {
        self.counts.iter().fold(0u32, |a, &c| a.saturating_add(c))
    }

    /// How many of [`total`](Self::total) are right-censored (still-live lower bounds).
    #[inline]
    pub fn censored(&self) -> u32 {
        self.censored
    }

    /// The count in `class`.
    #[inline]
    pub fn count(&self, class: LifetimeClass) -> u32 {
        self.counts[class.index()]
    }

    /// The **dominant** (most-observed) class and its count. Ties break toward the
    /// **longer-lived** class (the higher index), which is the conservative placement
    /// choice — over-estimating lifetime keeps a maybe-long object out of the
    /// short-lived churn pool. Returns [`Unknown`](LifetimeClass::Unknown) with count 0
    /// for an empty histogram.
    #[inline]
    pub fn dominant(&self) -> (LifetimeClass, u32) {
        let mut best = LifetimeClass::Unknown;
        let mut best_n = 0u32;
        // Iterate ascending and accept ties (`>=`) so the *highest* index among equal
        // counts wins (longer-lived bias). Skip the Unknown bucket itself as a candidate
        // — it is never a *measured* class, only the empty default.
        for i in 1..LifetimeClass::COUNT {
            let n = self.counts[i];
            if n >= best_n && n > 0 {
                best_n = n;
                best = LifetimeClass::from_index(i);
            }
        }
        (best, best_n)
    }
}

/// The number of monitored size-class counters in a [`SizeClassDist`] (§24.4). A call
/// site allocates only a handful of distinct sizes, so a small Space-Saving summary
/// captures the distribution within a bounded, allocation-free footprint.
pub const SIZE_DIST_K: usize = 4;

/// Reserved [`SizeClassDist`] buckets for the non-small request kinds, distinct from any
/// small size-class index (which are `0..num_size_classes`, far below these sentinels).
impl SizeClassDist {
    /// A page-rounded medium extent (above the small path, below the hugepage threshold).
    pub const BUCKET_MEDIUM: u16 = u16::MAX - 1;
    /// A large (hugepage/region-backed) allocation.
    pub const BUCKET_LARGE: u16 = u16::MAX;
}

/// A bounded **size-class distribution** (§24.4 `size_class_distribution`) kept as a
/// Space-Saving summary [R: Metwally et al.]: at most [`SIZE_DIST_K`] monitored
/// `(bucket, count)` counters with a per-counter overcount `error`, so the *true*
/// frequency of a monitored bucket lies in `[count - error, count]` and any bucket whose
/// true frequency exceeds `total / K` is guaranteed to be monitored. Allocation-free and
/// `Copy`, so a profile stores it inline.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SizeClassDist {
    buckets: [u16; SIZE_DIST_K],
    counts: [u32; SIZE_DIST_K],
    errors: [u32; SIZE_DIST_K],
    /// How many of the `K` counters are in use (`0..=K`).
    len: u8,
    /// Total observations (so a query can compute the `total / K` admission threshold).
    total: u32,
}

impl SizeClassDist {
    /// Observe one allocation of `bucket` (a small size-class index, or
    /// [`BUCKET_MEDIUM`](Self::BUCKET_MEDIUM) / [`BUCKET_LARGE`](Self::BUCKET_LARGE)).
    /// Space-Saving: bump if monitored; else fill a free counter; else evict the
    /// minimum-count counter, inheriting its count as this bucket's overcount `error`.
    pub fn observe(&mut self, bucket: u16) {
        self.total = self.total.saturating_add(1);
        let len = self.len as usize;
        // Already monitored → bump.
        for i in 0..len {
            if self.buckets[i] == bucket {
                self.counts[i] = self.counts[i].saturating_add(1);
                return;
            }
        }
        // A free counter → claim it.
        if len < SIZE_DIST_K {
            self.buckets[len] = bucket;
            self.counts[len] = 1;
            self.errors[len] = 0;
            self.len += 1;
            return;
        }
        // Full → evict the minimum-count counter (Space-Saving). The new bucket inherits
        // `min` as its count and records `min` as its overcount bound.
        let mut min_i = 0usize;
        for i in 1..SIZE_DIST_K {
            if self.counts[i] < self.counts[min_i] {
                min_i = i;
            }
        }
        let min = self.counts[min_i];
        self.buckets[min_i] = bucket;
        self.counts[min_i] = min.saturating_add(1);
        self.errors[min_i] = min;
    }

    /// The most-allocated bucket and its (upper-bound) count, or `None` if nothing has
    /// been observed.
    pub fn dominant(&self) -> Option<(u16, u32)> {
        let len = self.len as usize;
        if len == 0 {
            return None;
        }
        let mut best = 0usize;
        for i in 1..len {
            if self.counts[i] > self.counts[best] {
                best = i;
            }
        }
        Some((self.buckets[best], self.counts[best]))
    }

    /// The monitored `(bucket, count, error)` entries (for profile dumps).
    pub fn entries(&self) -> impl Iterator<Item = (u16, u32, u32)> + '_ {
        (0..self.len as usize).map(move |i| (self.buckets[i], self.counts[i], self.errors[i]))
    }

    /// Total observations folded into the summary.
    #[inline]
    pub fn total(&self) -> u32 {
        self.total
    }
}

/// The number of placement **buckets** the learned-hint table is keyed by: one per small
/// size class plus the two non-small kinds ([`BUCKET_MEDIUM`](SizeClassDist::BUCKET_MEDIUM)
/// / [`BUCKET_LARGE`](SizeClassDist::BUCKET_LARGE)). A request's bucket is the coarse,
/// hot-path-cheap key the engine has for free (size class), the §24.1-sanctioned
/// "size class" placement input.
pub const NUM_BUCKETS: usize = SIZE_CLASSES.len() + 2;

/// Map a [`SizeClassDist`] bucket id to its dense `0..NUM_BUCKETS` index: a small size
/// class maps to itself; the medium / large sentinels map to the two trailing slots. Total
/// (an out-of-range small index clamps to the large slot, never panics).
#[inline]
pub fn bucket_index(bucket: u16) -> usize {
    match bucket {
        SizeClassDist::BUCKET_LARGE => SIZE_CLASSES.len() + 1,
        SizeClassDist::BUCKET_MEDIUM => SIZE_CLASSES.len(),
        sc if (sc as usize) < SIZE_CLASSES.len() => sc as usize,
        _ => SIZE_CLASSES.len() + 1,
    }
}

/// A coarse, bounded **placement class** (§24.6–§24.8) derived from a [`PlaceHints`], used
/// to *group* objects — it tags the hugepage/span pool an object joins so like-placed
/// objects cluster. Kept to four values so grouping never multiplies pools unboundedly:
///
/// * [`Hot`](PlaceClass::Hot) — hot **or** long-lived ⇒ pack densely, keep stable (§24.8);
/// * [`Cold`](PlaceClass::Cold) — cold (rarely accessed) ⇒ segregate from hot (§24.6);
/// * [`Short`](PlaceClass::Short) — neutral-hotness, short-lived ⇒ group so a whole
///   span/hugepage empties together (§24.7);
/// * [`Default`](PlaceClass::Default) — no actionable signal.
///
/// Hotness dominates the choice (it is the §24.6/§24.8 separation axis); lifetime breaks
/// the neutral-hotness tie (§24.7). Like every W14 output it is **advisory** — a wrong
/// class changes only *where* an object lands, never its size/alignment/validity (§24.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum PlaceClass {
    /// No actionable placement signal.
    #[default]
    Default = 0,
    /// Cold / rarely accessed — segregate from hot (§24.6).
    Cold = 1,
    /// Hot or long-lived — dense, stable (§24.8).
    Hot = 2,
    /// Neutral-hotness, short-lived — group to empty together (§24.7).
    Short = 3,
}

impl PlaceClass {
    /// The number of distinct classes (for bounding pool fan-out).
    pub const COUNT: usize = 4;

    /// Derive the class from advisory [`PlaceHints`] (§24.6–§24.8).
    #[inline]
    pub fn from_hints(h: PlaceHints) -> PlaceClass {
        match h.hotness {
            Hotness::Hot => PlaceClass::Hot,
            Hotness::Cold => PlaceClass::Cold,
            Hotness::Neutral => match h.lifetime {
                // Long-lived neutral objects are stable ⇒ pack with the dense set (§24.8).
                Lifetime::Long => PlaceClass::Hot,
                // Short-lived neutral objects churn ⇒ group to empty together (§24.7).
                Lifetime::Short => PlaceClass::Short,
                _ => PlaceClass::Default,
            },
        }
    }

    /// The `u8` tag stored on a span / in stats.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a class from its tag (total: an unknown tag reads as
    /// [`Default`](PlaceClass::Default)).
    #[inline]
    pub const fn from_u8(v: u8) -> PlaceClass {
        match v {
            1 => PlaceClass::Cold,
            2 => PlaceClass::Hot,
            3 => PlaceClass::Short,
            _ => PlaceClass::Default,
        }
    }

    /// The stable string used in stats/diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            PlaceClass::Default => "default",
            PlaceClass::Cold => "cold",
            PlaceClass::Hot => "hot",
            PlaceClass::Short => "short",
        }
    }
}

/// The §24.4 **sample count** at which a single learned dimension (hotness from allocs,
/// lifetime from frees) reaches full per-dimension maturity. Below it confidence scales
/// linearly with evidence; at/above it the dimension is "well-sampled". Policy, tunable.
pub const CONFIDENT_SAMPLES: u32 = 32;

/// Default minimum confidence (basis points, 1/10000) a learned dimension needs before
/// [`SiteProfileTable::place_hints`] acts on it (§24.6 "when the allocator has
/// confidence"). At/above this a confident profile steers placement; below it the
/// dimension contributes the neutral default — so a thin or noisy profile is a safe
/// no-op. Policy, tunable via [`SiteProfileTable::set_min_confidence_bp`].
pub const DEFAULT_MIN_CONFIDENCE_BP: u32 = 6_000;

/// One basis-point full scale (1.0 == 10000 bp). Confidences and ratios are integer
/// basis points so the core stays floating-point-free (§6).
const BP_ONE: u64 = 10_000;

/// A learned **allocation-site profile** (§24.4): everything the allocator infers about
/// one call site from its sampled allocations and frees. Every field is advisory; a
/// missing or wrong profile can only mis-steer placement, never break safety (§24.5).
/// `Copy` and allocation-free, so [`SiteProfileTable`] stores it inline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocationSiteProfile {
    /// The site identity (§24.4 `stack_id`).
    stack_id: StackId,
    /// Bounded size-class distribution (§24.4 `size_class_distribution`).
    size_dist: SizeClassDist,
    /// Lifetime histogram, incl. right-censored still-live objects (§24.4
    /// `lifetime_histogram`).
    lifetimes: LifetimeHistogram,
    /// Recency-weighted (event-driven EWMA) hotness estimate, in Q8 fixed point
    /// (`value × 256`), so a hotness *phase change* is tracked rather than averaged away
    /// (§24.4 `hotness_estimate`).
    hotness_q8: u16,
    /// EWMA of the absolute deviation of the hotness samples from
    /// [`hotness_q8`](Self::hotness_q8) (Q8) — a cheap mean-absolute-deviation *stability*
    /// signal: a noisy/bimodal site has high MAD and is treated as unconfident (so it never
    /// drives a confident hot/cold grouping), while a steady site has ~0 MAD.
    hotness_mad_q8: u16,
    /// Sampled allocations observed (the hotness/size evidence count).
    alloc_count: u32,
    /// Sampled completed frees observed (the lifetime evidence count).
    free_count: u32,
    /// Sampled live bytes currently attributed to this site (§24.4 `sampled_live_bytes`):
    /// allocated minus freed sampled bytes, saturating at 0.
    sampled_live_bytes: u64,
    /// EWMA of the inter-allocation interval (ms, Q8), for a recency-weighted
    /// §24.4 `allocation_rate` that reflects recent behaviour rather than a lifetime
    /// average (`0` until a second allocation gives a first interval).
    alloc_interval_q8: u32,
    /// EWMA of the inter-free interval (ms, Q8), for `free_rate`.
    free_interval_q8: u32,
    /// Last allocation time (ms), for the inter-allocation interval.
    last_alloc_ms: u64,
    /// Last free time (ms), for the inter-free interval.
    last_free_ms: u64,
}

/// Event-driven EWMA shift (smoothing `α = 1/8`): `ewma += (sample − ewma) / 8`. A fixed
/// fraction per *event* (not per unit time), so it is recency-weighted without needing a
/// floating-point time-decay (§6 keeps the core FP-free).
const EWMA_SHIFT: u32 = 3;

/// Mean-absolute-deviation (hotness units, `0..=255`) at/above which the hotness signal is
/// considered too noisy to act on (stability `0`). Tunable policy.
const HOTNESS_MAD_MAX: u32 = 48;

/// One step of the event-driven EWMA over `u32` Q-values (signed delta, clamped ≥ 0).
#[inline]
fn ewma_step(cur: u32, sample: u32) -> u32 {
    let delta = sample as i64 - cur as i64;
    (cur as i64 + (delta >> EWMA_SHIFT)).max(0) as u32
}

impl AllocationSiteProfile {
    /// A fresh, evidence-free profile for `stack_id` seeded at `now_ms`.
    #[inline]
    pub fn new(stack_id: StackId, now_ms: u64) -> Self {
        AllocationSiteProfile {
            stack_id,
            size_dist: SizeClassDist::default(),
            lifetimes: LifetimeHistogram::default(),
            hotness_q8: 0,
            hotness_mad_q8: 0,
            alloc_count: 0,
            free_count: 0,
            sampled_live_bytes: 0,
            alloc_interval_q8: 0,
            free_interval_q8: 0,
            last_alloc_ms: now_ms,
            last_free_ms: now_ms,
        }
    }

    /// The site identity.
    #[inline]
    pub fn stack_id(&self) -> StackId {
        self.stack_id
    }

    /// Record a sampled **allocation**: `bucket` (size class / medium / large), `bytes`
    /// usable, and the `0..=255` hotness hint, at `now_ms`.
    pub fn record_alloc(&mut self, bucket: u16, bytes: u64, hotness_hint: u8, now_ms: u64) {
        self.size_dist.observe(bucket);
        let sample_q8 = (hotness_hint as u32) << 8;
        if self.alloc_count == 0 {
            // Seed the EWMA on the first sample (no deviation yet).
            self.hotness_q8 = sample_q8 as u16;
            self.hotness_mad_q8 = 0;
        } else {
            // Deviation against the *current* estimate, then update both EWMAs.
            let dev = (sample_q8 as i64 - self.hotness_q8 as i64).unsigned_abs() as u32;
            self.hotness_q8 = ewma_step(self.hotness_q8 as u32, sample_q8) as u16;
            self.hotness_mad_q8 = ewma_step(self.hotness_mad_q8 as u32, dev) as u16;
            // Inter-allocation interval EWMA (recency-weighted rate). `saturating_mul`
            // (not `<< 8`) so an absurd interval clamps rather than wrapping the shift.
            let dt = (now_ms.saturating_sub(self.last_alloc_ms))
                .saturating_mul(256)
                .min(u32::MAX as u64) as u32;
            self.alloc_interval_q8 = if self.alloc_interval_q8 == 0 {
                dt
            } else {
                ewma_step(self.alloc_interval_q8, dt)
            };
        }
        self.alloc_count = self.alloc_count.saturating_add(1);
        self.sampled_live_bytes = self.sampled_live_bytes.saturating_add(bytes);
        self.last_alloc_ms = now_ms;
    }

    /// Record a sampled **completed free**: the object lived `age_ms`, returning `bytes`.
    pub fn record_free(&mut self, age_ms: u64, bytes: u64, now_ms: u64) {
        self.lifetimes.record(LifetimeClass::from_age_ms(age_ms));
        if self.free_count > 0 {
            let dt = (now_ms.saturating_sub(self.last_free_ms))
                .saturating_mul(256)
                .min(u32::MAX as u64) as u32;
            self.free_interval_q8 = if self.free_interval_q8 == 0 {
                dt
            } else {
                ewma_step(self.free_interval_q8, dt)
            };
        }
        self.free_count = self.free_count.saturating_add(1);
        self.sampled_live_bytes = self.sampled_live_bytes.saturating_sub(bytes);
        self.last_free_ms = now_ms;
    }

    /// Record a **right-censored** lifetime (§31.4): a sampled object of this site is
    /// still live at dump, having so far lived `age_ms`, so its class is *at least*
    /// `from_age_ms(age_ms)`. Does not touch `sampled_live_bytes` (the object is still
    /// live) — it only sharpens the lifetime histogram's tail.
    pub fn record_censored(&mut self, age_ms: u64) {
        self.lifetimes
            .record_censored(LifetimeClass::from_age_ms(age_ms));
    }

    /// The recency-weighted hotness estimate (`0..=255`), or `0` (cold/neutral) if nothing
    /// has been seen yet.
    #[inline]
    pub fn hotness_estimate(&self) -> u8 {
        (self.hotness_q8 >> 8) as u8
    }

    /// The dominant size-class **bucket** (§24.4), or [`BUCKET_LARGE`](SizeClassDist::BUCKET_LARGE)
    /// when nothing has been observed (the conservative catch-all).
    #[inline]
    pub fn dominant_bucket(&self) -> u16 {
        self.size_dist
            .dominant()
            .map(|(b, _)| b)
            .unwrap_or(SizeClassDist::BUCKET_LARGE)
    }

    /// The lifetime histogram (§24.4).
    #[inline]
    pub fn lifetimes(&self) -> &LifetimeHistogram {
        &self.lifetimes
    }

    /// The size-class distribution (§24.4).
    #[inline]
    pub fn size_distribution(&self) -> &SizeClassDist {
        &self.size_dist
    }

    /// Sampled live bytes attributed to this site (§24.4).
    #[inline]
    pub fn sampled_live_bytes(&self) -> u64 {
        self.sampled_live_bytes
    }

    /// Sampled allocation/free counts (the evidence behind the confidences).
    #[inline]
    pub fn counts(&self) -> (u32, u32) {
        (self.alloc_count, self.free_count)
    }

    /// The §24.4 sampled **allocation rate** in *sampled* events per second, recency-weighted
    /// (the EWMA of the recent inter-allocation interval — scale by the sampling ratio for an
    /// absolute rate). `0` until a second allocation establishes a first interval.
    #[inline]
    pub fn allocation_rate(&self) -> u64 {
        Self::rate_from_interval(self.alloc_interval_q8)
    }

    /// The §24.4 sampled **free rate** in sampled events per second, recency-weighted.
    #[inline]
    pub fn free_rate(&self) -> u64 {
        Self::rate_from_interval(self.free_interval_q8)
    }

    /// Convert an inter-event interval EWMA (ms, Q8) to whole events per second. Integer
    /// throughout (§6); a zero interval (fewer than two events) yields `0`.
    #[inline]
    fn rate_from_interval(interval_q8: u32) -> u64 {
        if interval_q8 == 0 {
            return 0;
        }
        // events/sec = 1000 ms/sec ÷ interval_ms = (1000 << 8) ÷ interval_q8.
        (1_000u64 << 8) / interval_q8 as u64
    }

    /// The hotness signal's **stability** (bp): full when the mean-absolute-deviation of the
    /// hotness samples is ~0, falling linearly to `0` at `HOTNESS_MAD_MAX`. A bimodal /
    /// noisy site is unstable, so it never drives a confident hot/cold grouping (the
    /// recency-weighted estimate alone could otherwise flip-flop).
    #[inline]
    pub fn hotness_stability_bp(&self) -> u32 {
        let mad = (self.hotness_mad_q8 >> 8) as u32; // hotness units 0..=255
        if mad >= HOTNESS_MAD_MAX {
            0
        } else {
            ((HOTNESS_MAD_MAX - mad) as u64 * BP_ONE / HOTNESS_MAD_MAX as u64) as u32
        }
    }

    /// The **hotness** confidence (bp): sample maturity × signal stability. High only when
    /// the site is both well-sampled *and* steady in its hotness — a noisy/bimodal site
    /// stays low (treated as neutral) even after many samples.
    #[inline]
    pub fn hotness_confidence_bp(&self) -> u32 {
        ((maturity_bp(self.alloc_count) as u64) * self.hotness_stability_bp() as u64 / BP_ONE)
            as u32
    }

    /// The **lifetime** confidence (bp): lifetime-observation maturity × histogram
    /// concentration (the dominant class's share). High only when many observations *agree*
    /// on a class — a site with few or scattered lifetimes stays low and is treated as
    /// Unknown. The observation count is the histogram total, which includes right-censored
    /// (still-live) lower bounds (§31.4), so a confidently long-lived-but-not-yet-dead site
    /// is recognised rather than dismissed for lacking completed frees; both factors share
    /// that denominator so the measure is internally consistent.
    #[inline]
    pub fn lifetime_confidence_bp(&self) -> u32 {
        let observed = self.lifetimes.total();
        if observed == 0 {
            return 0;
        }
        let (_, dom) = self.lifetimes.dominant();
        let concentration = (dom as u64).saturating_mul(BP_ONE) / observed as u64;
        ((maturity_bp(observed) as u64).saturating_mul(concentration) / BP_ONE) as u32
    }

    /// The §24.4 **confidence** (bp): the conservative `min` of the hotness and lifetime
    /// confidences — the profile is only "confident" overall once *both* learned
    /// dimensions are well-established. [`place_hints`](SiteProfileTable::place_hints)
    /// gates each dimension on its own confidence (more permissive); this single number
    /// is the headline trust measure surfaced in dumps/stats.
    #[inline]
    pub fn confidence_bp(&self) -> u32 {
        self.hotness_confidence_bp()
            .min(self.lifetime_confidence_bp())
    }

    /// The dominant inferred [`LifetimeClass`] (Unknown until frees are seen).
    #[inline]
    pub fn dominant_lifetime(&self) -> LifetimeClass {
        self.lifetimes.dominant().0
    }

    /// Distil the profile into advisory [`PlaceHints`] for the filler, acting on each
    /// learned dimension only when its own confidence reaches `min_bp` (§24.6–§24.8):
    ///
    /// * hotness ⇒ the mean hotness, but only once well-sampled, and only if it maps to a
    ///   non-neutral [`Hotness`] (so a confidently *neutral* site stays neutral);
    /// * lifetime ⇒ the dominant class projected onto the coarse filler hint, once the
    ///   lifetime histogram is confident.
    ///
    /// Either, both, or neither dimension may fire; an un-fired dimension is the neutral
    /// default. The result is **score-only** input to the filler (§24.5) — it can never
    /// change size/alignment/validity.
    pub fn place_hints(&self, min_bp: u32) -> PlaceHints {
        let hotness = if self.hotness_confidence_bp() >= min_bp {
            Hotness::from_hint(self.hotness_estimate())
        } else {
            Hotness::Neutral
        };
        let lifetime = if self.lifetime_confidence_bp() >= min_bp {
            self.dominant_lifetime().to_hint()
        } else {
            Lifetime::Unspecified
        };
        PlaceHints { hotness, lifetime }
    }
}

/// Linear sample maturity in basis points: `min(n, CONFIDENT_SAMPLES) / CONFIDENT_SAMPLES`,
/// saturating at full scale. Monotone non-decreasing in `n`.
#[inline]
fn maturity_bp(n: u32) -> u32 {
    let capped = n.min(CONFIDENT_SAMPLES) as u64;
    (capped.saturating_mul(BP_ONE) / CONFIDENT_SAMPLES as u64) as u32
}

/// Running summary counters for the [`SiteProfileTable`] (the operational "tax", plan
/// 07): surfaced in `topo-stats` JSON and the `topo.placement.*` control namespace so the
/// learning policy's behaviour is observable. These are *profiling* counters, not a
/// managed-memory byte class, so they do **not** enter the §8.6 VM reconciliation —
/// `sampled_live_bytes` is a sampled estimate, not allocator-owned bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlacementStats {
    /// Distinct sites currently held in the table.
    pub sites_tracked: u32,
    /// Of those, how many are confident enough to steer placement
    /// (`confidence_bp >= min_confidence_bp`).
    pub confident_sites: u32,
    /// Cumulative sampled allocations recorded.
    pub alloc_samples: u64,
    /// Cumulative sampled completed frees recorded.
    pub free_samples: u64,
    /// Cumulative right-censored (still-live-at-dump) lifetimes folded in.
    pub censored_samples: u64,
    /// Cumulative site evictions (a new site displaced a less-useful one — capacity
    /// pressure / collisions).
    pub evictions: u64,
    /// Sum of `sampled_live_bytes` across tracked sites (a sampled live-heap estimate).
    pub sampled_live_bytes: u64,
}

/// One open-addressing slot of the [`SiteProfileTable`].
#[derive(Clone, Copy)]
struct Slot {
    /// `true` once this slot holds a real profile.
    occupied: bool,
    profile: AllocationSiteProfile,
}

impl Slot {
    const EMPTY: Slot = Slot {
        occupied: false,
        // A placeholder profile; never read while `occupied == false`.
        profile: AllocationSiteProfile {
            stack_id: StackId::UNKNOWN,
            size_dist: SizeClassDist {
                buckets: [0; SIZE_DIST_K],
                counts: [0; SIZE_DIST_K],
                errors: [0; SIZE_DIST_K],
                len: 0,
                total: 0,
            },
            lifetimes: LifetimeHistogram {
                counts: [0; LifetimeClass::COUNT],
                censored: 0,
            },
            hotness_q8: 0,
            hotness_mad_q8: 0,
            alloc_count: 0,
            free_count: 0,
            sampled_live_bytes: 0,
            alloc_interval_q8: 0,
            free_interval_q8: 0,
            last_alloc_ms: 0,
            last_free_ms: 0,
        },
    };
}

/// The **learning-policy table** (§24.4) and the W17-3d profile aggregator: a
/// fixed-capacity, allocation-free, host-driven map from [`StackId`] to
/// [`AllocationSiteProfile`]. It is a **16-way set-associative cache** (the textbook
/// fixed-capacity design): a site hashes to one set of `WAYS` slots and lives in exactly
/// one of them, so every operation is `O(WAYS)` and `no_std`. A full set evicts its
/// **least-useful** (lowest-confidence, stalest-tie) slot — every slot is used (no
/// overlapping probe windows to waste capacity), and replacement is clean and local.
///
/// `CAP` must be a power of two (asserted) so the hash maps with a mask. The table is
/// large (profiles stored inline); construct it behind a `Box`/`static` rather than on
/// the stack.
///
/// **Thread-safety.** Like [`ReleaseController`](crate::ReleaseController) this is a
/// single-owner object: the sampled (rare) path mutates it under the host's lock. The
/// hot-path *sampling decision* (`crate::sampling::Sampler`) is lock-free and never
/// touches the table; only a fired sample does (§31.4 "avoid hot-path locks").
pub struct SiteProfileTable<const CAP: usize> {
    slots: [Slot; CAP],
    /// Live count of occupied slots.
    occupied: u32,
    /// Cumulative counters mirrored into [`stats`](Self::stats).
    alloc_samples: u64,
    free_samples: u64,
    censored_samples: u64,
    evictions: u64,
    /// The §24.6 confidence gate `place_hints` applies.
    min_confidence_bp: u32,
}

/// Set associativity: a site lives in one of this many slots of its set. 16-way keeps the
/// forced-eviction rate low while the table has room, and bounds every operation to 16
/// steps. `CAP` must be a multiple (a power-of-two ≥ this).
const WAYS: usize = 16;

impl<const CAP: usize> SiteProfileTable<CAP> {
    /// `CAP` must be a power of two and at least [`WAYS`] (so it splits into whole sets).
    const _CAP_OK: () = assert!(
        CAP.is_power_of_two() && CAP >= WAYS,
        "SiteProfileTable CAP must be a power of two >= WAYS"
    );

    /// The number of associativity sets (`CAP / WAYS`, a power of two).
    const NUM_SETS: usize = CAP / WAYS;

    /// An empty table with the default §24.6 confidence gate.
    #[allow(clippy::new_without_default)] // `CAP` makes a blanket `Default` awkward; `new` is clearer.
    pub const fn new() -> Self {
        // Touch the const assertion so a bad `CAP` is a compile error.
        let () = Self::_CAP_OK;
        SiteProfileTable {
            slots: [Slot::EMPTY; CAP],
            occupied: 0,
            alloc_samples: 0,
            free_samples: 0,
            censored_samples: 0,
            evictions: 0,
            min_confidence_bp: DEFAULT_MIN_CONFIDENCE_BP,
        }
    }

    /// Set the §24.6 confidence gate (bp) `place_hints` applies. Higher ⇒ the policy acts
    /// only on very well-established sites (more conservative).
    #[inline]
    pub fn set_min_confidence_bp(&mut self, bp: u32) {
        self.min_confidence_bp = bp.min(BP_ONE as u32);
    }

    /// The current confidence gate (bp).
    #[inline]
    pub fn min_confidence_bp(&self) -> u32 {
        self.min_confidence_bp
    }

    /// Capacity (number of slots).
    #[inline]
    pub fn capacity(&self) -> usize {
        CAP
    }

    /// The base slot index of `key`'s associativity set (its `WAYS` slots are
    /// `[base, base + WAYS)`).
    #[inline]
    fn set_base(key: StackId) -> usize {
        let set = (splitmix64(key.0) as usize) & (Self::NUM_SETS - 1);
        set * WAYS
    }

    /// Find the slot holding `key` within its set, if present (`O(WAYS)`).
    fn find(&self, key: StackId) -> Option<usize> {
        let base = Self::set_base(key);
        for i in base..base + WAYS {
            let s = &self.slots[i];
            if s.occupied && s.profile.stack_id == key {
                return Some(i);
            }
        }
        None
    }

    /// Find `key`'s slot in its set, or choose one to (re)use for it: an exact hit, else a
    /// free slot in the set, else the **least-useful** occupied slot (lowest confidence,
    /// stalest tie — evicted). Never fails: a full set always yields a victim. The evicted
    /// profile's evidence is dropped, a bounded, intended loss that keeps the table on the
    /// sites that matter.
    fn find_or_make(&mut self, key: StackId, now_ms: u64) -> usize {
        let base = Self::set_base(key);
        let mut free: Option<usize> = None;
        let mut victim = base;
        let mut victim_conf = u32::MAX;
        let mut victim_age = u64::MAX;
        for i in base..base + WAYS {
            let s = &self.slots[i];
            if s.occupied {
                if s.profile.stack_id == key {
                    return i;
                }
                let c = s.profile.confidence_bp();
                let age = s.profile.last_alloc_ms;
                if c < victim_conf || (c == victim_conf && age < victim_age) {
                    victim = i;
                    victim_conf = c;
                    victim_age = age;
                }
            } else if free.is_none() {
                free = Some(i);
            }
        }
        if let Some(i) = free {
            self.slots[i].occupied = true;
            self.slots[i].profile = AllocationSiteProfile::new(key, now_ms);
            self.occupied += 1;
            return i;
        }
        // Full set: replace the least-useful slot in place (no slot freed, so no other
        // site's lookup is disturbed — clean set-associative replacement).
        self.slots[victim].profile = AllocationSiteProfile::new(key, now_ms);
        self.evictions += 1;
        victim
    }

    /// Record a sampled **allocation** at `stack_id` (W17-3d aggregation). `bucket` is the
    /// size-class / medium / large bucket, `bytes` the usable size, `hotness_hint` the
    /// `0..=255` request hotness. An [`UNKNOWN`](StackId::UNKNOWN) site is ignored (an
    /// un-attributed sample carries no site signal).
    pub fn record_alloc(
        &mut self,
        stack_id: StackId,
        bucket: u16,
        bytes: u64,
        hotness_hint: u8,
        now_ms: u64,
    ) {
        self.alloc_samples = self.alloc_samples.saturating_add(1);
        if !stack_id.is_known() {
            return;
        }
        let i = self.find_or_make(stack_id, now_ms);
        self.slots[i]
            .profile
            .record_alloc(bucket, bytes, hotness_hint, now_ms);
    }

    /// Record a sampled **completed free** at `stack_id`: the object lived `age_ms` and
    /// returned `bytes`. A site not in the table is ignored safely (it may have been
    /// evicted, or its alloc never sampled) — the free path stays correct regardless
    /// (§24.5).
    pub fn record_free(&mut self, stack_id: StackId, age_ms: u64, bytes: u64, now_ms: u64) {
        self.free_samples = self.free_samples.saturating_add(1);
        if let Some(i) = self.find(stack_id) {
            self.slots[i].profile.record_free(age_ms, bytes, now_ms);
        }
    }

    /// Fold a **right-censored** still-live object into `stack_id`'s lifetime tail
    /// (§31.4): it has so far lived `age_ms`. A missing site is ignored.
    pub fn record_censored(&mut self, stack_id: StackId, age_ms: u64) {
        self.censored_samples = self.censored_samples.saturating_add(1);
        if let Some(i) = self.find(stack_id) {
            self.slots[i].profile.record_censored(age_ms);
        }
    }

    /// The profile for `stack_id`, if tracked.
    #[inline]
    pub fn profile(&self, stack_id: StackId) -> Option<&AllocationSiteProfile> {
        self.find(stack_id).map(|i| &self.slots[i].profile)
    }

    /// The **placement decision** for `stack_id` (§24.4): the advisory [`PlaceHints`]
    /// distilled from its confident profile, or the neutral default if the site is
    /// untracked or not yet confident. Total and `&self` — a pure query, safe to call on
    /// any key, that can only ever return score-only hints (§24.5).
    #[inline]
    pub fn place_hints(&self, stack_id: StackId) -> PlaceHints {
        match self.find(stack_id) {
            Some(i) => self.slots[i].profile.place_hints(self.min_confidence_bp),
            None => PlaceHints::default(),
        }
    }

    /// Visit every tracked [`AllocationSiteProfile`] (for a §31.3 profile dump). The table
    /// is left unchanged.
    pub fn for_each_profile<F: FnMut(&AllocationSiteProfile)>(&self, mut f: F) {
        for s in self.slots.iter() {
            if s.occupied {
                f(&s.profile);
            }
        }
    }

    /// A running [`PlacementStats`] snapshot (the plan-07 observability tax). Recomputes
    /// `confident_sites` / `sampled_live_bytes` by a single bounded scan of the slots.
    pub fn stats(&self) -> PlacementStats {
        let mut confident = 0u32;
        let mut live_bytes = 0u64;
        for s in self.slots.iter() {
            if s.occupied {
                if s.profile.confidence_bp() >= self.min_confidence_bp {
                    confident += 1;
                }
                live_bytes = live_bytes.saturating_add(s.profile.sampled_live_bytes);
            }
        }
        PlacementStats {
            sites_tracked: self.occupied,
            confident_sites: confident,
            alloc_samples: self.alloc_samples,
            free_samples: self.free_samples,
            censored_samples: self.censored_samples,
            evictions: self.evictions,
            sampled_live_bytes: live_bytes,
        }
    }

    /// Publish the learned, confident per-bucket placement hints into `out` for the
    /// allocator's hot path to apply (closing the learn → place loop). For each placement
    /// **bucket** (size class / medium / large) it computes the *consensus* hint across the
    /// confident profiles whose dominant bucket is that one: if they agree, that hint is
    /// published; if they disagree, or none is confident, the bucket is left neutral — so a
    /// learned hint steers placement only when the evidence is both confident **and**
    /// consistent. Cheap (`O(tracked sites)`); the host calls it from the rare sampled path.
    ///
    /// Keyed by bucket (not by arena): the bucket is the hot-path-cheap key the engine has
    /// for free (§24.1 "size class"); arena-scoped learning is a future refinement (per-arena
    /// *NUMA* placement is already explicit via [`NumaPolicy`](crate::NumaPolicy), and the
    /// span path additionally carries its arena tag).
    pub fn write_learned_hints(&self, out: &LearnedHints) {
        out.reset();
        for s in self.slots.iter() {
            if !s.occupied {
                continue;
            }
            let p = &s.profile;
            // Only confident profiles steer placement (§24.6 "when … confidence").
            let h = p.place_hints(self.min_confidence_bp);
            if h == PlaceHints::default() {
                continue; // nothing actionable learned for this site
            }
            out.merge(bucket_index(p.dominant_bucket()), h);
        }
        out.publish();
    }
}

/// The lock-free **applied-hints table** the allocator's hot path reads to place an
/// allocation by its learned profile (the live end of the learn → place loop). One packed
/// [`PlaceHints`] per placement bucket; the sampler republishes it from
/// [`SiteProfileTable::write_learned_hints`] on the rare sampled path, and the engine reads
/// it with a single atomic load per allocation (no lock — §31.4). When nothing has
/// been published it short-circuits to the neutral default, so it is free when profiling is
/// off. Like every W14 output the value is **advisory** (§24.5): it can only change *where*
/// an object lands, never its size/alignment/validity.
pub struct LearnedHints {
    /// One packed-hint byte per bucket (see [`pack_hint`] / [`unpack_hint`]).
    slots: [AtomicU8; NUM_BUCKETS],
    /// `true` once any non-neutral hint has been published — lets the reader skip the per-
    /// bucket load entirely in the common (nothing-learned / profiling-off) case.
    any: AtomicBool,
}

impl Default for LearnedHints {
    fn default() -> Self {
        LearnedHints::new()
    }
}

impl LearnedHints {
    /// An empty table (no hints published).
    pub const fn new() -> LearnedHints {
        LearnedHints {
            slots: [const { AtomicU8::new(0) }; NUM_BUCKETS],
            any: AtomicBool::new(false),
        }
    }

    /// The learned hint for `bucket`, or the neutral default if nothing is published for it
    /// (or nothing at all). One atomic acquire load in the common case. Total for any `bucket`.
    #[inline]
    pub fn lookup(&self, bucket: u16) -> PlaceHints {
        if !self.any.load(Ordering::Acquire) {
            return PlaceHints::default();
        }
        unpack_hint(self.slots[bucket_index(bucket)].load(Ordering::Relaxed))
    }

    /// Clear every bucket to neutral, so [`lookup`](Self::lookup) returns the default and the
    /// allocation path stops applying learned placement (e.g. when profiling is disabled).
    /// Also the start of a republish, before `publish` re-arms it.
    pub fn reset(&self) {
        self.any.store(false, Ordering::Release);
        for s in self.slots.iter() {
            s.store(0, Ordering::Relaxed);
        }
    }

    /// Fold a confident site's hint into `bucket`'s consensus: set it if empty, keep it if it
    /// agrees, or mark the bucket conflicted (→ neutral) if it disagrees.
    fn merge(&self, bucket: usize, h: PlaceHints) {
        let cur = self.slots[bucket].load(Ordering::Relaxed);
        let next = match unpack_state(cur) {
            HintState::Unset => pack_hint(h),
            HintState::Set if unpack_hint(cur) == h => cur, // agrees
            _ => CONFLICT, // disagreement or already conflicted ⇒ neutral
        };
        self.slots[bucket].store(next, Ordering::Relaxed);
    }

    /// Re-arm the table after a republish if any bucket carries an actionable hint.
    fn publish(&self) {
        let any = self
            .slots
            .iter()
            .any(|s| unpack_state(s.load(Ordering::Relaxed)) == HintState::Set);
        self.any.store(any, Ordering::Release);
    }
}

/// Publication state of a packed learned-hint byte.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HintState {
    /// No confident site has claimed this bucket.
    Unset,
    /// Exactly one consensus hint (the low bits encode it).
    Set,
    /// Confident sites disagreed ⇒ read as neutral.
    Conflict,
}

/// The reserved "conflict ⇒ neutral" byte (state bits = 2).
const CONFLICT: u8 = 0b10;

/// Pack a [`PlaceHints`] into a learned-hint byte: state(bits0-1)=Set, hotness(bits2-3),
/// lifetime(bits4-5).
#[inline]
fn pack_hint(h: PlaceHints) -> u8 {
    let hot = h.hotness as u8 & 0b11;
    let life = match h.lifetime {
        Lifetime::Unspecified => 0,
        Lifetime::Short => 1,
        Lifetime::Medium => 2,
        Lifetime::Long => 3,
    };
    0b01 | (hot << 2) | (life << 4)
}

/// The publication state encoded in a learned-hint byte.
#[inline]
fn unpack_state(b: u8) -> HintState {
    match b & 0b11 {
        0 => HintState::Unset,
        1 => HintState::Set,
        _ => HintState::Conflict,
    }
}

/// Decode a learned-hint byte to [`PlaceHints`]; an unset or conflicted byte is the neutral
/// default (so a disagreement never steers placement).
#[inline]
fn unpack_hint(b: u8) -> PlaceHints {
    if unpack_state(b) != HintState::Set {
        return PlaceHints::default();
    }
    // Inverse of `h.hotness as u8` (Cold=0, Neutral=1, Hot=2).
    let hotness = match (b >> 2) & 0b11 {
        0 => Hotness::Cold,
        2 => Hotness::Hot,
        _ => Hotness::Neutral,
    };
    let lifetime = match (b >> 4) & 0b11 {
        1 => Lifetime::Short,
        2 => Lifetime::Medium,
        3 => Lifetime::Long,
        _ => Lifetime::Unspecified,
    };
    PlaceHints { hotness, lifetime }
}

/// A fast, well-mixed integer hash (SplitMix64 finalizer). Deterministic and `const`, so
/// the table's probe layout is reproducible across runs — useful for the differential
/// tests. Used only to scatter [`StackId`]s across buckets; collisions cost placement
/// quality, never safety.
#[inline]
const fn splitmix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small table for stack-friendly tests.
    type T = SiteProfileTable<16>;

    #[test]
    fn lifetime_class_from_age_is_monotone_and_total() {
        // Boundaries map to the documented §24.2 classes.
        assert_eq!(LifetimeClass::from_age_ms(0), LifetimeClass::Ephemeral);
        assert_eq!(LifetimeClass::from_age_ms(1), LifetimeClass::Short);
        assert_eq!(LifetimeClass::from_age_ms(999), LifetimeClass::Short);
        assert_eq!(LifetimeClass::from_age_ms(1_000), LifetimeClass::Medium);
        assert_eq!(LifetimeClass::from_age_ms(59_999), LifetimeClass::Medium);
        assert_eq!(LifetimeClass::from_age_ms(60_000), LifetimeClass::Long);
        assert_eq!(LifetimeClass::from_age_ms(3_599_999), LifetimeClass::Long);
        assert_eq!(
            LifetimeClass::from_age_ms(3_600_000),
            LifetimeClass::Persistent
        );
        assert_eq!(
            LifetimeClass::from_age_ms(u64::MAX),
            LifetimeClass::Persistent
        );
        // Monotone non-decreasing in age across a sweep.
        let mut prev = LifetimeClass::Ephemeral;
        for ms in [0u64, 1, 1_000, 60_000, 3_600_000, u64::MAX] {
            let c = LifetimeClass::from_age_ms(ms);
            assert!(c >= prev);
            prev = c;
        }
    }

    #[test]
    fn lifetime_class_hint_roundtrip_and_projection() {
        // index/from_index round-trips every class.
        for i in 0..LifetimeClass::COUNT {
            assert_eq!(LifetimeClass::from_index(i).index(), i);
        }
        // Projection onto the coarse filler hint (§19.5 grouping axis).
        assert_eq!(LifetimeClass::Unknown.to_hint(), Lifetime::Unspecified);
        assert_eq!(LifetimeClass::Ephemeral.to_hint(), Lifetime::Short);
        assert_eq!(LifetimeClass::Short.to_hint(), Lifetime::Short);
        assert_eq!(LifetimeClass::Medium.to_hint(), Lifetime::Medium);
        assert_eq!(LifetimeClass::Long.to_hint(), Lifetime::Long);
        assert_eq!(LifetimeClass::Persistent.to_hint(), Lifetime::Long);
    }

    #[test]
    fn histogram_dominant_breaks_ties_toward_longer_lived() {
        let mut h = LifetimeHistogram::default();
        h.record(LifetimeClass::Short);
        h.record(LifetimeClass::Long);
        // Tie at 1 each → the longer-lived (Long) wins (conservative placement).
        assert_eq!(h.dominant(), (LifetimeClass::Long, 1));
        h.record(LifetimeClass::Short);
        // Now Short leads 2:1.
        assert_eq!(h.dominant(), (LifetimeClass::Short, 2));
        assert_eq!(h.total(), 3);
        assert_eq!(h.censored(), 0);
        // Censored folds in at the floor and is tallied separately.
        h.record_censored(LifetimeClass::Persistent);
        assert_eq!(h.count(LifetimeClass::Persistent), 1);
        assert_eq!(h.censored(), 1);
        assert_eq!(h.total(), 4);
    }

    #[test]
    fn size_dist_space_saving_keeps_top_buckets() {
        let mut d = SizeClassDist::default();
        // Bucket 3 is the clear leader; a flood of singletons can't displace it.
        for _ in 0..100 {
            d.observe(3);
        }
        for b in 0..50u16 {
            d.observe(100 + b);
        }
        let (bucket, count) = d.dominant().expect("nonempty");
        assert_eq!(bucket, 3, "the heavy hitter survives Space-Saving eviction");
        assert!(count >= 100);
        assert_eq!(d.total(), 150);
        // Never exceeds K monitored counters.
        assert!(d.entries().count() <= SIZE_DIST_K);
        // Medium/large sentinels coexist with small indices.
        let mut d2 = SizeClassDist::default();
        d2.observe(SizeClassDist::BUCKET_LARGE);
        d2.observe(SizeClassDist::BUCKET_LARGE);
        d2.observe(7);
        assert_eq!(d2.dominant(), Some((SizeClassDist::BUCKET_LARGE, 2)));
    }

    #[test]
    fn profile_confidence_grows_with_consistent_evidence() {
        let mut p = AllocationSiteProfile::new(StackId(1), 0);
        assert_eq!(p.confidence_bp(), 0);
        assert_eq!(
            p.place_hints(DEFAULT_MIN_CONFIDENCE_BP),
            PlaceHints::default()
        );
        // Many consistent short-lived frees + hot allocs → confident, hot+short hint.
        let mut t = 0u64;
        for _ in 0..CONFIDENT_SAMPLES {
            p.record_alloc(8, 4096, 200, t);
            t += 10;
            p.record_free(5 /* ms: ephemeral→short bucket */, 4096, t);
            t += 10;
        }
        assert!(p.hotness_confidence_bp() >= DEFAULT_MIN_CONFIDENCE_BP);
        assert!(p.lifetime_confidence_bp() >= DEFAULT_MIN_CONFIDENCE_BP);
        let h = p.place_hints(DEFAULT_MIN_CONFIDENCE_BP);
        assert_eq!(h.hotness, Hotness::Hot, "mean hotness 200 ⇒ Hot");
        assert_eq!(h.lifetime, Lifetime::Short, "5 ms lifetimes ⇒ Short");
        // Rates are well-defined and non-zero over the elapsed window.
        assert!(p.allocation_rate() > 0);
        assert!(p.free_rate() > 0);
    }

    #[test]
    fn noisy_hotness_is_unstable_and_does_not_steer_placement() {
        // Alternating cold(0)/hot(255) allocations give a high mean-absolute-deviation, so
        // the hotness signal is *unstable*: its confidence stays low and it exerts no
        // grouping pressure (the recency-weighted estimate alone could otherwise flip-flop).
        // The lifetime axis, observed consistently, is unaffected.
        let mut p = AllocationSiteProfile::new(StackId(2), 0);
        let mut t = 0;
        for k in 0..CONFIDENT_SAMPLES {
            p.record_alloc(8, 64, if k % 2 == 0 { 0 } else { 255 }, t);
            t += 1;
            p.record_free(2_000, 64, t); // medium lifetimes, consistent
            t += 1;
        }
        // High MAD ⇒ ~zero stability ⇒ hotness confidence below the gate ⇒ Neutral.
        assert!(
            p.hotness_stability_bp() < 2_000,
            "noisy hotness is unstable"
        );
        assert!(p.hotness_confidence_bp() < DEFAULT_MIN_CONFIDENCE_BP);
        let h = p.place_hints(DEFAULT_MIN_CONFIDENCE_BP);
        assert_eq!(
            h.hotness,
            Hotness::Neutral,
            "unstable hotness does not steer"
        );
        assert_eq!(
            h.lifetime,
            Lifetime::Medium,
            "consistent lifetime still steers"
        );
    }

    #[test]
    fn table_records_and_resolves_hints() {
        let mut t = T::new();
        assert_eq!(t.place_hints(StackId(7)), PlaceHints::default());
        let mut clock = 0u64;
        for _ in 0..CONFIDENT_SAMPLES {
            t.record_alloc(StackId(7), 16, 256, 0 /* cold */, clock);
            clock += 5;
            // Long-lived (>1 min) so the class is Long.
            t.record_free(StackId(7), 120_000, 256, clock);
            clock += 5;
        }
        let h = t.place_hints(StackId(7));
        assert_eq!(h.hotness, Hotness::Cold, "cold site ⇒ cold placement");
        assert_eq!(h.lifetime, Lifetime::Long);
        let st = t.stats();
        assert_eq!(st.sites_tracked, 1);
        assert_eq!(st.confident_sites, 1);
        assert_eq!(st.alloc_samples, CONFIDENT_SAMPLES as u64);
        assert_eq!(st.free_samples, CONFIDENT_SAMPLES as u64);
    }

    #[test]
    fn table_free_of_untracked_site_is_a_safe_noop() {
        // §24.5: recording a free for a site we never saw (or that was evicted) must not
        // panic and must leave the table coherent.
        let mut t = T::new();
        t.record_free(StackId(999), 10, 64, 1);
        t.record_censored(StackId(999), 10);
        assert_eq!(t.stats().sites_tracked, 0);
        assert_eq!(t.stats().free_samples, 1);
    }

    #[test]
    fn table_sampled_live_bytes_track_alloc_minus_free() {
        let mut t = T::new();
        t.record_alloc(StackId(3), 8, 4096, 0, 0);
        t.record_alloc(StackId(3), 8, 4096, 0, 1);
        assert_eq!(t.profile(StackId(3)).unwrap().sampled_live_bytes(), 8192);
        t.record_free(StackId(3), 10, 4096, 2);
        assert_eq!(t.profile(StackId(3)).unwrap().sampled_live_bytes(), 4096);
        // Over-free saturates at zero rather than wrapping.
        t.record_free(StackId(3), 10, 1 << 40, 3);
        assert_eq!(t.profile(StackId(3)).unwrap().sampled_live_bytes(), 0);
    }

    #[test]
    fn table_evicts_least_useful_under_capacity_pressure() {
        // Force many sites that all hash into overlapping windows; the table must stay
        // bounded (never exceed CAP) and prefer keeping confident sites.
        let mut t = SiteProfileTable::<16>::new();
        // A confident site we want retained.
        let keep = StackId(0xA11CE);
        let mut clock = 0u64;
        for _ in 0..CONFIDENT_SAMPLES {
            t.record_alloc(keep, 8, 64, 200, clock);
            clock += 1;
            t.record_free(keep, 5, 64, clock);
            clock += 1;
        }
        assert!(t.profile(keep).unwrap().confidence_bp() > 0);
        // Now hammer the table with many distinct one-shot sites.
        for k in 0..1000u64 {
            t.record_alloc(StackId(0x1_0000 + k), 8, 64, 0, clock);
            clock += 1;
        }
        // Bounded: occupancy never exceeds capacity.
        assert!(t.stats().sites_tracked <= t.capacity() as u32);
        assert!(
            t.stats().evictions > 0,
            "capacity pressure caused evictions"
        );
    }

    #[test]
    fn right_censored_sharpens_the_long_tail() {
        // A site whose sampled objects are mostly still alive at dump: censoring lets the
        // histogram reflect "at least Long" rather than under-counting long lifetimes.
        let mut p = AllocationSiteProfile::new(StackId(5), 0);
        for _ in 0..10 {
            p.record_alloc(8, 64, 0, 0);
        }
        // Only two completed frees (short), but eight still-live at dump for ~10 min.
        p.record_free(10, 64, 0);
        p.record_free(20, 64, 0);
        for _ in 0..8 {
            p.record_censored(600_000); // ~10 minutes alive so far ⇒ Long floor
        }
        // The censored mass makes Long the dominant (correct) class.
        assert_eq!(p.dominant_lifetime(), LifetimeClass::Long);
        assert_eq!(p.lifetimes().censored(), 8);
    }

    #[test]
    fn strong_right_censored_evidence_makes_a_long_lived_site_confident() {
        // A site whose objects are all still alive at dump (no completed frees yet) must
        // still be recognised as confidently Long — right-censored lower bounds are
        // lifetime evidence, so the policy can act on a long-lived-but-not-yet-dead site
        // rather than dismissing it for lacking deaths.
        let mut p = AllocationSiteProfile::new(StackId(9), 0);
        for _ in 0..CONFIDENT_SAMPLES {
            p.record_alloc(8, 4096, 200 /* hot */, 0);
        }
        // No completed frees; every sampled object is censored at a Long age.
        for _ in 0..CONFIDENT_SAMPLES {
            p.record_censored(600_000);
        }
        assert!(
            p.lifetime_confidence_bp() >= DEFAULT_MIN_CONFIDENCE_BP,
            "censored-only long-lived evidence is confident"
        );
        let h = p.place_hints(DEFAULT_MIN_CONFIDENCE_BP);
        assert_eq!(
            h.lifetime,
            Lifetime::Long,
            "still-live long objects ⇒ Long hint"
        );
        assert_eq!(h.hotness, Hotness::Hot);
        // The (0,0) counts surface as no *completed* frees, but the histogram has evidence.
        assert_eq!(p.counts(), (CONFIDENT_SAMPLES, 0));
        assert_eq!(p.lifetimes().total(), CONFIDENT_SAMPLES);
    }

    #[test]
    fn place_class_derivation_covers_the_grouping_axes() {
        use PlaceClass::*;
        let hc = |hot, life| {
            PlaceClass::from_hints(PlaceHints {
                hotness: hot,
                lifetime: life,
            })
        };
        // Hotness dominates (§24.6/§24.8).
        assert_eq!(hc(Hotness::Hot, Lifetime::Short), Hot);
        assert_eq!(hc(Hotness::Cold, Lifetime::Long), Cold);
        // Neutral hotness: lifetime breaks the tie (§24.7/§24.8).
        assert_eq!(hc(Hotness::Neutral, Lifetime::Long), Hot);
        assert_eq!(hc(Hotness::Neutral, Lifetime::Short), Short);
        assert_eq!(hc(Hotness::Neutral, Lifetime::Medium), Default);
        assert_eq!(hc(Hotness::Neutral, Lifetime::Unspecified), Default);
        // u8 round-trips.
        for c in [Default, Cold, Hot, Short] {
            assert_eq!(PlaceClass::from_u8(c.as_u8()), c);
        }
    }

    #[test]
    fn ewma_rate_is_recency_weighted() {
        // A site that allocates slowly, then bursts, reports a rate that reflects the recent
        // burst — not a lifetime average dragged down by the slow start.
        let mut p = AllocationSiteProfile::new(StackId(11), 0);
        let mut t = 0u64;
        for _ in 0..8 {
            t += 1000; // 1 alloc/sec
            p.record_alloc(8, 64, 0, t);
        }
        let slow = p.allocation_rate();
        for _ in 0..16 {
            t += 10; // 100 allocs/sec burst
            p.record_alloc(8, 64, 0, t);
        }
        let fast = p.allocation_rate();
        assert!(slow <= 2, "slow phase ~1/sec, got {slow}");
        assert!(
            fast > slow * 5,
            "rate tracks the recent burst: {slow} -> {fast}"
        );
    }

    #[test]
    fn learned_hints_publish_consensus_and_neutralize_conflict() {
        let mut t: SiteProfileTable<64> = SiteProfileTable::new();
        let learned = LearnedHints::new();
        // Nothing learned yet ⇒ neutral, and the `any` short-circuit holds.
        t.write_learned_hints(&learned);
        assert_eq!(learned.lookup(8), PlaceHints::default());

        // Two confident sites whose dominant bucket is size-class 8 AGREE on cold+short.
        let mut clock = 0u64;
        for site in [StackId(100), StackId(101)] {
            for _ in 0..CONFIDENT_SAMPLES {
                t.record_alloc(site, 8, 64, 0 /* cold */, clock);
                clock += 1;
                t.record_free(site, 5 /* short */, 64, clock);
                clock += 1;
            }
        }
        // A confident site on bucket 9 is hot+long.
        for _ in 0..CONFIDENT_SAMPLES {
            t.record_alloc(StackId(102), 9, 64, 255, clock);
            clock += 1;
            t.record_free(StackId(102), 120_000, 64, clock);
            clock += 1;
        }
        t.write_learned_hints(&learned);
        // Bucket 8: consensus cold+short.
        let h8 = learned.lookup(8);
        assert_eq!(h8.hotness, Hotness::Cold);
        assert_eq!(h8.lifetime, Lifetime::Short);
        // Bucket 9: hot+long.
        let h9 = learned.lookup(9);
        assert_eq!(h9.hotness, Hotness::Hot);
        assert_eq!(h9.lifetime, Lifetime::Long);

        // Now add a confident site on bucket 8 that DISAGREES (hot+long) ⇒ conflict ⇒ neutral.
        for _ in 0..CONFIDENT_SAMPLES {
            t.record_alloc(StackId(103), 8, 64, 255, clock);
            clock += 1;
            t.record_free(StackId(103), 120_000, 64, clock);
            clock += 1;
        }
        t.write_learned_hints(&learned);
        assert_eq!(
            learned.lookup(8),
            PlaceHints::default(),
            "disagreeing confident sites neutralize the bucket"
        );
        // Bucket 9 still confident (unaffected by bucket 8's conflict).
        assert_eq!(learned.lookup(9).hotness, Hotness::Hot);
    }
}

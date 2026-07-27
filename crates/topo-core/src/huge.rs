// SPDX-License-Identifier: MIT
//! The hugepage-aware backend: filler, huge cache, region cache (§19, plan 04 W11).
//!
//! The hugepage backend keeps live memory packed into a small number of
//! **hugepages** (a `HUGEPAGE_SIZE`-byte unit — 2 MiB on x86-64, [`PAGES_PER_HUGEPAGE`]
//! allocator pages) so that few hugepages hold the live set and empty hugepages
//! release easily (§19.1, Temeraire \[R3\]). It has four §19.2 components:
//!
//! * **HugeAllocator** — reserves hugepage-aligned virtual ranges (§19.2, W11-1a);
//!   here the [`HugePageFiller`] tiles one provider-reserved, hugepage-aligned
//!   region into [`PAGES_PER_HUGEPAGE`]-page hugepages.
//! * **HugeCache** — keeps empty *backed* hugepages around for quick reuse so a
//!   burst does not immediately fault (§19.2/§19.5, W11-1b): an emptied hugepage
//!   stays [`EmptyBacked`](HugeBin::EmptyBacked) (committed) rather than being
//!   released straight away.
//! * **HugePageFiller** — packs sub-hugepage page-runs into hugepages, scored over
//!   approximate **bins** (§19.3/§19.4, W11-2): each hugepage sits in **exactly
//!   one** of nine [`HugeBin`]s, consistent with its occupancy and state (H-003).
//! * **RegionCache** — caches awkward (just-over-a-hugepage) sizes so they avoid
//!   rounding to whole hugepages (§18.6, W11-3); see the `RegionCache` type below.
//!
//! **Bins are correctness; the score is policy (DD-2, §2.4).** [`classify_bin`] is a
//! *total* function of `(used, total, subreleased, hotness)` — H-003 holds by
//! construction because the filed bin is recomputed from occupancy on every change.
//! The placement **score** (the private `HugePageFiller::score`) may be arbitrarily wrong
//! without ever misplacing a live object: a run is carved from a hugepage's free
//! bitmap, so two live objects can never overlap regardless of the score.
//!
//! **Backend-agnostic (W11-6, §36.9).** The filler's geometry and scoring inputs are
//! page counts and a hotness tag — never "this is a hardware hugepage". So the same
//! model packs spans over **contiguous normal-frame runs** on seLe4n exactly as over
//! x86 hugepages; the [`HugePageBackend`] just supplies the backing through the
//! [`TopoBackingProvider`] seam (POSIX `mmap` or a seLe4n frame run).
//!
//! **Structure (mirrors [`crate::extent`]).** [`HugePageFiller`] is the pure,
//! single-threaded bookkeeping core (per-hugepage [`live`](HugePageFiller)/
//! committed/released page bitmaps + the nine bin lists), directly unit-tested via
//! `&mut self`; [`HugePageBackend`] wraps it behind the §27.2 backend lock and drives
//! the provider (`commit` on placement, `decommit` on subrelease/release), exposing
//! the §18.6 [`RegionCacheHook`] the large path already consults.
//!
//! **Hugepage invariants (§19.8, B.4).** The filler enforces, and
//! [`check_invariants`](HugePageFiller::check_invariants) verifies:
//!
//! * **H-001** a live page is committed and not released (`live ⊆ committed`,
//!   `live ∩ released = ∅`);
//! * **H-002** a hugepage's occupancy equals the sum of its contained allocations'
//!   pages (the `live` bitmap *is* that sum — popcount == recorded `used`);
//! * **H-003** the filed bin equals [`classify_bin`] of the current occupancy/state;
//! * **H-004** an empty hugepage's pages are only released once it holds no live
//!   object (subrelease/release require `used == 0` over the target run);
//! * **H-005** [`subrelease`](HugePageFiller::subrelease) **refuses** any run that
//!   intersects a live page — the single most dangerous hugepage op is guarded and
//!   individually tested.
//!
//! The matching Lean model is `lean/TopoMalloc/HugePageFiller.lean` (the
//! `classify_bin`/H-002/H-003/H-005 obligations), pinned to this code by the
//! `huge_bin_classification_matches_lean` test and the `lake exe check`
//! `hugeBinGate` (the §19.4 analogue of the §20.1 `extentStateGate`).

use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::backend::{Region, TopoBackingProvider};
use crate::bootstrap::MetadataAlloc;
use crate::error::BackendError;
use crate::extent::{BackendLock, RegionCacheHook, StateBytes};
use crate::flags::{Hints, HugepagePolicy, Lifetime};
use crate::generated::tables::{HUGE_THRESHOLD, PAGE_SIZE};
use crate::ids::ArenaId;
use crate::overflow::pages_for;
use crate::release::{ReleaseController, ReleaseInputs, ReleasePlan};

/// A sentinel "no slot" index for the intrusive bin links (`u32::MAX`).
const NIL: u32 = u32::MAX;

/// The platform hugepage size in bytes (§19, Appendix C "a configurable platform
/// property"). Pinned to [`HUGE_THRESHOLD`] (2 MiB) — the request size at and above
/// which the hugepage/region backend serves (§9.2/§18.5), which is exactly the
/// hardware hugepage on x86-64 Linux. A whole power-of-two number of allocator
/// [`PAGE_SIZE`] pages (asserted below), so a hugepage tiles cleanly into pages.
pub const HUGEPAGE_SIZE: usize = HUGE_THRESHOLD;

/// Allocator pages per hugepage ([`HUGEPAGE_SIZE`] / [`PAGE_SIZE`]) — the size of a
/// hugepage's page-occupancy bitmap (128 on the tuned table: 2 MiB / 16 KiB).
pub const PAGES_PER_HUGEPAGE: usize = HUGEPAGE_SIZE / PAGE_SIZE;

/// `u64` words in a per-hugepage page bitmap (`ceil(PAGES_PER_HUGEPAGE / 64)`).
const BITMAP_WORDS: usize = PAGES_PER_HUGEPAGE.div_ceil(64);

// A hugepage must be a whole power-of-two number of allocator pages, so it tiles
// cleanly and `HUGEPAGE_SIZE`/`PAGE_SIZE` alignment is a simple mask.
const _: () = assert!(HUGEPAGE_SIZE.is_power_of_two(), "hugepage size must be 2^k");
const _: () = assert!(
    HUGEPAGE_SIZE.is_multiple_of(PAGE_SIZE) && PAGES_PER_HUGEPAGE >= 1,
    "a hugepage must be a whole number of allocator pages"
);
const _: () = assert!(
    PAGES_PER_HUGEPAGE <= BITMAP_WORDS * 64,
    "the page bitmap must cover every page of a hugepage"
);

/// A hotness class for a page-run or a hugepage (§19.3 `hotness_match`, §19.5
/// "pack hot objects densely"). Advisory placement input — never a safety input
/// (DD-2: the score is policy). Derived from the request's [`Hints::hotness`] by
/// [`from_hint`](Hotness::from_hint) (`0` ⇒ [`Cold`](Hotness::Cold); `1..=127` ⇒
/// [`Neutral`](Hotness::Neutral); `≥ 128` ⇒ [`Hot`](Hotness::Hot)) and, for a
/// hugepage, the join of its residents' hotness.
///
/// [`Hints::hotness`]: crate::flags::Hints::hotness
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Hotness {
    /// Cold: idle / explicitly cold — the prime partial-subrelease candidate (§19.6).
    Cold = 0,
    /// Neutral: no hotness signal (the default).
    #[default]
    Neutral = 1,
    /// Hot: frequently accessed — pack densely and preserve coverage (§19.5).
    Hot = 2,
}

impl Hotness {
    /// Decode from the atomic byte (total: any unknown value reads as
    /// [`Neutral`](Hotness::Neutral), the safe no-signal default).
    #[inline]
    const fn from_u8(v: u8) -> Hotness {
        match v {
            0 => Hotness::Cold,
            2 => Hotness::Hot,
            _ => Hotness::Neutral,
        }
    }

    /// Classify a request's `0..=255` hotness hint (§10.4): `0` is cold, a high
    /// value hot, the middle neutral. The thresholds are policy (the band edges
    /// only steer placement, never safety).
    #[inline]
    pub const fn from_hint(hotness: u8) -> Hotness {
        if hotness == 0 {
            Hotness::Cold
        } else if hotness >= 128 {
            Hotness::Hot
        } else {
            Hotness::Neutral
        }
    }

    /// The join of two hotness tags (a hugepage takes the hottest resident, so a
    /// single hot object keeps its hugepage hot — §19.5 "pack hot objects densely").
    #[inline]
    const fn join(self, other: Hotness) -> Hotness {
        if (self as u8) >= (other as u8) {
            self
        } else {
            other
        }
    }
}

/// Encode a [`Lifetime`] hint as the `u8` a [`HugePageSlot`] stores (the join axis is
/// `Unspecified < Short < Medium < Long`).
#[inline]
fn lifetime_to_u8(l: Lifetime) -> u8 {
    match l {
        Lifetime::Unspecified => 0,
        Lifetime::Short => 1,
        Lifetime::Medium => 2,
        Lifetime::Long => 3,
    }
}

/// Decode a stored lifetime byte (total: an unknown value reads as
/// [`Unspecified`](Lifetime::Unspecified)).
#[inline]
fn lifetime_from_u8(v: u8) -> Lifetime {
    match v {
        1 => Lifetime::Short,
        2 => Lifetime::Medium,
        3 => Lifetime::Long,
        _ => Lifetime::Unspecified,
    }
}

/// Join two lifetime hints, taking the **longer** (a hugepage's lifetime is the
/// longest-lived resident, so a long-lived object keeps its hugepage long-lived —
/// §19.5 "pack same-lifetime objects together").
#[inline]
fn lifetime_join(a: Lifetime, b: Lifetime) -> Lifetime {
    if lifetime_to_u8(a) >= lifetime_to_u8(b) {
        a
    } else {
        b
    }
}

/// Advisory **placement hints** for the filler (§19.3/§19.5, W11-2b/W11-4a): the
/// request's hotness and expected lifetime. Backend-agnostic and **never a safety
/// input** — they only steer the placement score (DD-2: "the score is policy"), so a
/// wrong hint can hurt fragmentation but can never misplace a live object. The §18.6
/// [`RegionCacheHook`] seam carries no hints, so a request routed through it places
/// with [`PlaceHints::default`] ([`Neutral`](Hotness::Neutral)/
/// [`Unspecified`](Lifetime::Unspecified)); the richer
/// [`HugePageBackend::allocate`] entry point (used by the engine's large path under
/// the `hugepage_optimized` profile) carries the real hints.
///
/// NUMA placement is **not** a per-candidate score term: the live [`NodeRouter`](crate::NodeRouter)
/// places by routing the request to the right node's backend (W13), so the filler — which
/// owns one node's region — needs no node hint.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PlaceHints {
    /// Hotness class (§19.3 `hotness_match` / §19.5 dense packing).
    pub hotness: Hotness,
    /// Expected lifetime (§19.5 same-lifetime grouping).
    pub lifetime: Lifetime,
}

impl PlaceHints {
    /// Hints with the given hotness and an unspecified lifetime (the common case).
    #[inline]
    pub const fn hot(hotness: Hotness) -> PlaceHints {
        PlaceHints {
            hotness,
            lifetime: Lifetime::Unspecified,
        }
    }
}

/// hugepage is in **exactly one** (H-003); [`classify_bin`] is the total function
/// that decides, and the filler re-files a hugepage whenever its occupancy or state
/// changes. The numeric order is the `repr(u8)` the bin lists index by — it is *not*
/// the packing-preference order (that is the private `PACKING_ORDER`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum HugeBin {
    /// `used == 0`, no subreleased pages: a fully-empty, fully-or-partly **backed**
    /// hugepage held for quick reuse (the HugeCache, §19.2/§19.5).
    EmptyBacked = 0,
    /// Low occupancy `(0, 1/8)`: just a few live pages.
    NearlyEmpty = 1,
    /// Sparse occupancy `[1/8, 3/8)`, not cold.
    Sparse = 2,
    /// Moderate occupancy `[3/8, 5/8)`.
    Medium = 3,
    /// High occupancy `[5/8, 1)`, not hot.
    NearlyFull = 4,
    /// `used == total`: completely full (not hot).
    Full = 5,
    /// Has at least one subreleased page (§19.6) — a hugepage with a returned hole.
    PartialSubreleased = 6,
    /// Sparse occupancy `[1/8, 3/8)` **and** cold: the prime subrelease candidate.
    ColdSparse = 7,
    /// High occupancy or full **and** hot: pack densely, preserve coverage (§19.5).
    HotDense = 8,
}

impl HugeBin {
    /// The number of distinct bins (for fixed-size bin-head arrays and gating).
    pub const COUNT: usize = 9;

    /// The stable string used in stats/diagnostics (§19.4 bin names).
    pub const fn as_str(self) -> &'static str {
        match self {
            HugeBin::EmptyBacked => "empty_backed",
            HugeBin::NearlyEmpty => "nearly_empty",
            HugeBin::Sparse => "sparse",
            HugeBin::Medium => "medium",
            HugeBin::NearlyFull => "nearly_full",
            HugeBin::Full => "full",
            HugeBin::PartialSubreleased => "partial_subreleased",
            HugeBin::ColdSparse => "cold_sparse",
            HugeBin::HotDense => "hot_dense",
        }
    }
}

/// **§19.4 hugepage bin classification (H-003).** The *total* function mapping a
/// hugepage's `(used, total, subreleased, hotness)` to its unique [`HugeBin`]:
///
/// * any subreleased page ⇒ [`PartialSubreleased`](HugeBin::PartialSubreleased) (a
///   state override — a hole dominates the occupancy reading);
/// * else `used == 0` ⇒ [`EmptyBacked`](HugeBin::EmptyBacked);
/// * else `used == total` ⇒ [`HotDense`](HugeBin::HotDense) if hot, else
///   [`Full`](HugeBin::Full);
/// * else by occupancy eighths `band = used*8/total` (0..=7 since `0 < used < total`):
///   `0` ⇒ nearly_empty; `1..=2` ⇒ cold_sparse if cold else sparse; `3..=4` ⇒
///   medium; `5..=7` ⇒ hot_dense if hot else nearly_full.
///
/// **H-003 by construction:** the filed bin is *always* recomputed by this function
/// from the live occupancy, so it can never disagree with occupancy/state. Pinned
/// 1:1 to the Lean `classifyBin` (`HugePageFiller.lean`) by
/// `huge_bin_classification_matches_lean` and the `lake exe check` `hugeBinGate`.
///
/// Preconditions (the filler always upholds them): `total > 0`, `used + subreleased
/// <= total`, and `total` small enough that `used * 8` does not overflow (trivially,
/// `total <= PAGES_PER_HUGEPAGE`).
pub const fn classify_bin(
    used: usize,
    total: usize,
    subreleased: usize,
    hotness: Hotness,
) -> HugeBin {
    if subreleased > 0 {
        return HugeBin::PartialSubreleased;
    }
    if used == 0 {
        return HugeBin::EmptyBacked;
    }
    if used >= total {
        return match hotness {
            Hotness::Hot => HugeBin::HotDense,
            _ => HugeBin::Full,
        };
    }
    // 0 < used < total, so band ∈ {0,…,7}. `saturating_mul` hardens this public
    // function against a caller passing a `used` so large that `used * 8` would wrap
    // (every in-crate caller is bounded by `PAGES_PER_HUGEPAGE`, so for them this is
    // an exact `used * 8`; saturation only ever caps the band at 7, never misclassifies
    // a real hugepage). The Lean `classifyBin` uses `Nat` (unbounded), so the §19.4
    // differential gate — which evaluates only bounded inputs — is unaffected.
    let band = used.saturating_mul(8) / total;
    match band {
        0 => HugeBin::NearlyEmpty,
        1 | 2 => match hotness {
            Hotness::Cold => HugeBin::ColdSparse,
            _ => HugeBin::Sparse,
        },
        3 | 4 => HugeBin::Medium,
        _ => match hotness {
            Hotness::Hot => HugeBin::HotDense,
            _ => HugeBin::NearlyFull,
        },
    }
}

/// The bin scan order for **packing** placement (§19.5 "prefer filling already
/// partially used hugepages over opening new"): fullest-that-can-still-fit first,
/// so a request lands in the densest hugepage with room, leaving emptier ones
/// intact for release. [`Full`](HugeBin::Full) is omitted (no room);
/// [`EmptyBacked`](HugeBin::EmptyBacked) is last (opening / reusing an empty
/// hugepage is the fallback). The order is fixed, so placement is **deterministic**
/// (W11-2b).
const PACKING_ORDER: [HugeBin; 8] = [
    HugeBin::HotDense,
    HugeBin::NearlyFull,
    HugeBin::Medium,
    HugeBin::Sparse,
    HugeBin::ColdSparse,
    HugeBin::NearlyEmpty,
    HugeBin::PartialSubreleased,
    HugeBin::EmptyBacked,
];

/// The §19.7 hugepage coverage metrics (W11-5), summed over every managed hugepage.
/// All byte counts; [`coverage_ratio_bp`](Self::coverage_ratio_bp) is computed, not
/// stored. Reconciled into [`AllocatorStats`](crate::AllocatorStats) and the stats
/// JSON (plan 07). The default is all-zero (a backend with no touched hugepage).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HugeStats {
    /// Bytes covered by hugepages the filler has touched (`§19.7 coverage_bytes`):
    /// `touched_hugepages × HUGEPAGE_SIZE`.
    pub coverage_bytes: u64,
    /// Live bytes on **intact** (no-subrelease) hugepages
    /// (`live_bytes_on_intact_hugepages`) — the numerator of the coverage ratio.
    pub live_bytes_on_intact: u64,
    /// Live bytes on **partially-subreleased** hugepages
    /// (`live_bytes_on_partial_hugepages`).
    pub live_bytes_on_partial: u64,
    /// Backed bytes in **empty** hugepages held for reuse (`empty_backed_bytes`, the
    /// HugeCache reserve).
    pub empty_backed_bytes: u64,
    /// Released bytes in empty hugepages (`empty_released_bytes`).
    pub empty_released_bytes: u64,
    /// Subreleased subpage bytes on hugepages that **still hold a live object**
    /// (`partial_subreleased_bytes`, the §19.6 metric) — an empty hugepage's released
    /// bytes are [`empty_released_bytes`](Self::empty_released_bytes) instead, so the
    /// two never overlap (their sum is the total released byte count).
    pub partial_subreleased_bytes: u64,
    /// Backed-but-free bytes on in-use hugepages (`fragmentation_bytes`): internal
    /// slack the live set does not occupy.
    pub fragmentation_bytes: u64,
    /// Total live bytes across all hugepages (denominator of the coverage ratio;
    /// not a named §19.7 field but the divisor it defines).
    pub live_total_bytes: u64,
    /// Hugepage count in each of the nine §19.4 [`HugeBin`]s, indexed by `HugeBin as
    /// usize` (W11-4a "policy observable in stats"): the packing policy's *effect* —
    /// how the live set is distributed across empty/sparse/dense/subreleased
    /// hugepages. Sums to the touched-hugepage count.
    pub bins: [u32; HugeBin::COUNT],
}

impl HugeStats {
    /// The §19.7 recommended coverage ratio in basis points (`0..=10000`): live
    /// bytes on intact hugepages over total live bytes, ×10000 (integer — the core
    /// is floating-point-free, §6). `10000` when there is no live memory (vacuously
    /// fully covered). The SPEC formula is
    /// `live_bytes_on_intact_hugepages / max(live_bytes_total, 1)`.
    pub const fn coverage_ratio_bp(self) -> u32 {
        if self.live_total_bytes == 0 {
            return 10_000;
        }
        // live_on_intact ≤ live_total, so the ratio is ≤ 1.0 ⇒ ≤ 10000 bp; the
        // ×10000 cannot overflow u64 for any realistic heap (live_total ≤ address
        // space ≪ u64::MAX / 10000).
        let bp = self.live_bytes_on_intact.saturating_mul(10_000) / self.live_total_bytes;
        if bp > 10_000 {
            10_000
        } else {
            bp as u32
        }
    }

    /// Field-wise saturating sum, for aggregating several filler backends' coverage
    /// into one total (the shared backend plus each per-arena hooked region, as
    /// [`StateBytes::add`](crate::StateBytes::add) does for §20.1 state).
    pub const fn add(self, o: HugeStats) -> HugeStats {
        HugeStats {
            coverage_bytes: self.coverage_bytes.saturating_add(o.coverage_bytes),
            live_bytes_on_intact: self
                .live_bytes_on_intact
                .saturating_add(o.live_bytes_on_intact),
            live_bytes_on_partial: self
                .live_bytes_on_partial
                .saturating_add(o.live_bytes_on_partial),
            empty_backed_bytes: self.empty_backed_bytes.saturating_add(o.empty_backed_bytes),
            empty_released_bytes: self
                .empty_released_bytes
                .saturating_add(o.empty_released_bytes),
            partial_subreleased_bytes: self
                .partial_subreleased_bytes
                .saturating_add(o.partial_subreleased_bytes),
            fragmentation_bytes: self
                .fragmentation_bytes
                .saturating_add(o.fragmentation_bytes),
            live_total_bytes: self.live_total_bytes.saturating_add(o.live_total_bytes),
            bins: {
                let mut b = self.bins;
                let mut i = 0;
                while i < HugeBin::COUNT {
                    b[i] = b[i].saturating_add(o.bins[i]);
                    i += 1;
                }
                b
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Page bitmap helpers (`[u64; BITMAP_WORDS]`)
// ---------------------------------------------------------------------------

/// Whether bit `i` is set in `bm` (`i < PAGES_PER_HUGEPAGE`).
#[inline]
fn bit_get(bm: &[u64; BITMAP_WORDS], i: usize) -> bool {
    (bm[i / 64] >> (i % 64)) & 1 != 0
}

/// Set bit `i` in `bm`.
#[inline]
fn bit_set(bm: &mut [u64; BITMAP_WORDS], i: usize) {
    bm[i / 64] |= 1u64 << (i % 64);
}

/// Clear bit `i` in `bm`.
#[inline]
fn bit_clear(bm: &mut [u64; BITMAP_WORDS], i: usize) {
    bm[i / 64] &= !(1u64 << (i % 64));
}

/// Population count of `bm` (set pages).
#[inline]
fn popcount(bm: &[u64; BITMAP_WORDS]) -> usize {
    let mut n = 0u32;
    let mut w = 0;
    while w < BITMAP_WORDS {
        n += bm[w].count_ones();
        w += 1;
    }
    n as usize
}

/// Whether pages `[start, start + len)` are all clear in `bm` (`start + len
/// <= PAGES_PER_HUGEPAGE`).
#[inline]
fn run_is_clear(bm: &[u64; BITMAP_WORDS], start: usize, len: usize) -> bool {
    let mut i = start;
    let end = start + len;
    while i < end {
        if bit_get(bm, i) {
            return false;
        }
        i += 1;
    }
    true
}

/// Whether `addr` lies in the half-open address range `[base, base + len)`, computed
/// **without** the `base + len` addition (which could wrap near the top of the address
/// space and answer wrongly). The `&&` short-circuits, so `addr - base` is only evaluated
/// once `addr >= base` holds — it can never underflow — and `addr - base < len` is the
/// overflow-free equivalent of `addr < base + len`.
#[inline]
const fn region_contains(base: usize, len: usize, addr: usize) -> bool {
    addr >= base && addr - base < len
}

/// Count set bits in `bm` over the page range `[start, start + len)`.
#[inline]
fn run_popcount(bm: &[u64; BITMAP_WORDS], start: usize, len: usize) -> usize {
    let mut n = 0usize;
    let mut i = start;
    let end = start + len;
    while i < end {
        if bit_get(bm, i) {
            n += 1;
        }
        i += 1;
    }
    n
}

/// One hugepage descriptor. All fields are integers, so a zeroed slot is a valid
/// untouched hugepage (`touched == 0`, all bitmaps empty). `Copy`, so the
/// bookkeeping reads/writes whole slots by value (the [`crate::extent::ExtentMap`]
/// `Slot` discipline — borrow-check-clean, one pool accessor).
#[derive(Clone, Copy)]
#[repr(C)]
struct HugePageSlot {
    /// Allocated (live-object) pages. `used == popcount(live)`. (H-002)
    live: [u64; BITMAP_WORDS],
    /// Physically-backed pages. Invariant `live ⊆ committed` (H-001).
    committed: [u64; BITMAP_WORDS],
    /// Subreleased pages (were committed, now decommitted; need recommit, M-005).
    /// Invariant `committed ∩ released == ∅`.
    released: [u64; BITMAP_WORDS],
    /// **Allocation-start pages.** Bit `off` is set iff a live packed allocation
    /// *starts* at page `off` (set by [`carve`](HugePageFiller::carve), cleared by
    /// [`free`](HugePageFiller::free)). `heads ⊆ live`. Lets [`free`](HugePageFiller::free)
    /// reject a forged/partial [`Region`] that names an interior subrange of a live
    /// allocation instead of its exact extent (M-004/S-007), which would otherwise
    /// leave a live remainder and alias on reuse.
    heads: [u64; BITMAP_WORDS],
    /// Bin-list links (when `touched == 1`).
    bin_prev: u32,
    bin_next: u32,
    /// Whole-hugepage **run length** (hugepages) when this hugepage *starts* a
    /// multi-hugepage run ([`reserve_hugepages`](HugePageFiller::reserve_hugepages)),
    /// else `0`. [`free_hugepages`](HugePageFiller::free_hugepages) requires the freed
    /// count to equal it exactly — rejecting a forged partial-run free.
    run_len: u32,
    /// `1` iff this hugepage belongs to a multi-hugepage run (every hugepage of the
    /// run, not just its start). The packed [`free`](HugePageFiller::free) refuses a
    /// run member (it must be freed as a whole run), so a forged sub-hugepage `Region`
    /// can never partially free a run.
    in_run: u8,
    /// Joined [`Hotness`] of the hugepage's residents (`u8`).
    hotness: u8,
    /// Joined [`Lifetime`] of the hugepage's residents (`u8`, longest-lived — §19.5).
    lifetime: u8,
    /// The filed [`HugeBin`] (`u8`) when `touched == 1`.
    bin: u8,
    /// `1` once the hugepage has been opened into a bin (and stays, as the
    /// HugeCache, until reclaimed); `0` while untouched (reserved, in no bin).
    touched: u8,
}

impl HugePageSlot {
    /// Live (allocated) page count — the occupancy (H-002).
    #[inline]
    fn used(&self) -> usize {
        popcount(&self.live)
    }

    /// Subreleased page count.
    #[inline]
    fn subreleased(&self) -> usize {
        popcount(&self.released)
    }

    /// The bin this hugepage *should* be in, from its current occupancy/state (the
    /// H-003 target the filer keeps the filed [`bin`](Self::bin) equal to).
    #[inline]
    fn target_bin(&self) -> HugeBin {
        classify_bin(
            self.used(),
            PAGES_PER_HUGEPAGE,
            self.subreleased(),
            Hotness::from_u8(self.hotness),
        )
    }
}

/// The §27.5-sized cost of one hugepage descriptor, exposed for metadata-arena
/// sizing (the [`crate::extent::EXTENT_SLOT_BYTES`] discipline).
pub(crate) const HUGE_SLOT_BYTES: usize = core::mem::size_of::<HugePageSlot>();

/// A successful placement of a page-run by [`HugePageFiller::place`] (W11-2/W11-4a).
/// The filler has marked the run **live** and re-filed the hugepage's bin; the
/// caller ([`HugePageBackend`]) must then `commit` [`commit_run`](Self::commit_run)
/// (if `Some`) through the provider and confirm with
/// [`HugePageFiller::mark_committed`], or roll back with
/// [`HugePageFiller::free`] on a commit failure (W4-5 failure-safety).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    /// The hugepage slot the run was carved from.
    pub hugepage: u32,
    /// Base address of the placed run.
    pub base: usize,
    /// Number of pages placed.
    pub pages: usize,
    /// The `(region_offset, len)` the backend must `commit` because some page in the
    /// run was not yet backed (uncommitted/subreleased), or `None` if the whole run
    /// was already committed (a HugeCache hit — no fault, W11-1b).
    pub commit_run: Option<(usize, usize)>,
}

/// The outcome of a [`HugePageFiller::free`]: whether the run was a valid live
/// allocation (now freed), and whether its hugepage is now completely empty (the
/// H-004 precondition the backend needs before releasing the hugepage).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FreeReport {
    /// The run was a live allocation of a touched hugepage and is now free. `false`
    /// for a foreign/untouched/non-page-aligned base or a double free (no mutation).
    pub valid: bool,
    /// The hugepage holds no live page after this free — releasable (H-004).
    pub now_empty: bool,
}

impl FreeReport {
    /// The "not a live run of this filler" outcome (no mutation occurred).
    const INVALID: FreeReport = FreeReport {
        valid: false,
        now_empty: false,
    };
}

/// A reservation of `hugepages` **address-contiguous whole hugepages** by
/// [`HugePageFiller::reserve_hugepages`] (the §19.2 HugeAllocator path, W11-1a) — the
/// backing for a large allocation of one or more hugepages. Every page of every
/// hugepage in the run is marked live; the run is hugepage-aligned. As with
/// [`Placement`], the caller commits [`commit_run`](Self::commit_run) and confirms
/// with [`HugePageFiller::mark_run_committed`], or rolls back with
/// [`HugePageFiller::free_hugepages`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HugeRun {
    /// Base address of the run (hugepage-aligned).
    pub base: usize,
    /// Number of whole hugepages reserved.
    pub hugepages: usize,
    /// The `(region_offset, len)` to `commit` if any page of the run was not yet
    /// backed, or `None` if the whole run was already committed (a HugeCache hit).
    pub commit_run: Option<(usize, usize)>,
}

/// The byte sub-range a [`HugePageFiller::subrelease`] decided to return to the OS
/// (§19.6, W11-4b): the backend `decommit`s `[region_offset, region_offset + len)`.
/// The filler has already marked those pages released and re-filed the bin.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Subrelease {
    /// The hugepage the subrelease acts on.
    pub hugepage: u32,
    /// Region offset of the released run.
    pub region_offset: usize,
    /// Bytes released.
    pub len: usize,
}

/// Why a [`HugePageBackend::new`] construction failed (a *safe* failure — any
/// reservation already taken is rolled back, so nothing leaks). This is the
/// **backend-construction** error channel; **placement** failures are signalled by
/// [`place`](HugePageFiller::place) returning `None`, never by this type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HugeError {
    /// The descriptor metadata could not be allocated (the filler's slot pool or the
    /// region cache): the provider reservation has been released, leaving no region.
    NoSpace,
    /// The configured capacity is invalid: zero hugepages, or a hugepage count whose
    /// `capacity × HUGEPAGE_SIZE` byte length overflows `usize`.
    InvalidRequest,
    /// The backing provider's region reservation failed (§36.6); no region was taken.
    Backend(BackendError),
}

/// The pure, single-threaded hugepage filler core (§19.3–§19.6, W11-2/W11-4). Tiles
/// one hugepage-aligned region into [`PAGES_PER_HUGEPAGE`]-page hugepages, packs
/// page-runs into them over the nine [`HugeBin`]s, and tracks occupancy/coverage —
/// all as address + bitmap bookkeeping, with **no** provider calls (those belong to
/// [`HugePageBackend`]). Directly unit-tested via `&mut self`.
pub struct HugePageFiller {
    /// Metadata-backed hugepage descriptor pool (`capacity` slots, zeroed at
    /// construction so every slot starts a valid untouched hugepage; never freed —
    /// monotonic metadata, §17.4). Accessed only through [`get`](Self::get)/
    /// [`put`](Self::put) under `&self`/`&mut self`.
    slots: NonNull<HugePageSlot>,
    /// Hugepage capacity (slots).
    capacity: u32,
    /// Base of the tiled hugepage region (hugepage-aligned).
    region_base: usize,
    /// Lowest never-touched slot — the next fresh hugepage to open (§19.2 reserve).
    next_untouched: u32,
    /// Per-bin list heads (intrusive, over touched hugepages). Index by `HugeBin as
    /// usize`.
    bins: [u32; HugeBin::COUNT],
    /// Per-bin list tails, where [`bin_insert`](HugePageFiller::bin_insert) files a
    /// **full** hugepage. Same indexing as [`bins`](Self::bins).
    bin_tails: [u32; HugeBin::COUNT],
    /// Cumulative subreleases performed (the §19.6 metric the control plane reads).
    subrelease_events: u64,
}

// SAFETY: `HugePageFiller` owns its slot memory exclusively (a `NonNull` into
// never-freed metadata) and mutates only through `&mut self`. It holds no
// thread-shared state of its own; `HugePageBackend` adds the §27.2 backend lock for
// concurrent use.
unsafe impl Send for HugePageFiller {}

impl HugePageFiller {
    /// Build a filler over `capacity` hugepages based at `region_base` (which must
    /// be [`HUGEPAGE_SIZE`]-aligned). `None` on a zero/oversized capacity, a
    /// misaligned base, an address-space-wrapping region, or metadata exhaustion
    /// (safe failure).
    pub fn new(
        meta: &dyn MetadataAlloc,
        region_base: usize,
        capacity: usize,
    ) -> Option<HugePageFiller> {
        if capacity == 0
            || capacity > (NIL as usize)
            || !region_base.is_multiple_of(HUGEPAGE_SIZE)
            || capacity
                .checked_mul(HUGEPAGE_SIZE)?
                .checked_add(region_base)
                .is_none()
        {
            return None;
        }
        let bytes = capacity.checked_mul(core::mem::size_of::<HugePageSlot>())?;
        let mem = meta.alloc(bytes, core::mem::align_of::<HugePageSlot>())?;
        // SAFETY: `mem` is a fresh, exclusively-owned, aligned region of exactly
        // `bytes` bytes; zeroing yields `capacity` valid `HugePageSlot`s (every field
        // is an integer; the all-zero pattern is a valid untouched hugepage).
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        Some(HugePageFiller {
            slots: mem.cast::<HugePageSlot>(),
            capacity: capacity as u32,
            region_base,
            next_untouched: 0,
            bins: [NIL; HugeBin::COUNT],
            bin_tails: [NIL; HugeBin::COUNT],
            subrelease_events: 0,
        })
    }

    // --- slot pool accessors (the only `unsafe` in the bookkeeping) ----------

    /// Read slot `i` by value (`i < capacity`). The bounds check is a load-bearing
    /// `assert!` (not `debug_assert!`): [`Placement`]/[`HugeRun`]/[`Subrelease`] are
    /// public, copyable confirmation tokens, so a downstream crate could pass a forged
    /// one to a safe `pub fn` ([`mark_committed`](Self::mark_committed) etc.); the
    /// assert turns an out-of-range index into a clean panic instead of an
    /// out-of-bounds metadata read from safe code (S-007 — no safe path to UB).
    #[inline]
    fn get(&self, i: u32) -> HugePageSlot {
        assert!(i < self.capacity, "hugepage slot index out of range");
        // SAFETY: the `assert!` above establishes `i < capacity`; the slot memory is
        // initialized (zeroed then only overwritten with valid slots), `&self`
        // precludes a concurrent writer, and `HugePageSlot: Copy` so the read leaves
        // the pool untouched.
        unsafe { self.slots.as_ptr().add(i as usize).read() }
    }

    /// Write slot `i` (`i < capacity`). Load-bearing `assert!` as in [`get`](Self::get)
    /// — a forged public token can never drive an out-of-bounds metadata write.
    #[inline]
    fn put(&mut self, i: u32, s: HugePageSlot) {
        assert!(i < self.capacity, "hugepage slot index out of range");
        // SAFETY: the `assert!` establishes `i < capacity` and `&mut self` guarantees
        // exclusive pool access.
        unsafe { self.slots.as_ptr().add(i as usize).write(s) };
    }

    /// The base address of hugepage `i`.
    #[inline]
    fn huge_base(&self, i: u32) -> usize {
        self.region_base + (i as usize) * HUGEPAGE_SIZE
    }

    /// The hugepage index covering `addr`, or `None` if `addr` is outside the
    /// region. Total (saturating) — never wraps on a foreign address.
    #[inline]
    fn index_of(&self, addr: usize) -> Option<u32> {
        if addr < self.region_base {
            return None;
        }
        let off = addr - self.region_base;
        let i = off / HUGEPAGE_SIZE;
        if i < self.capacity as usize {
            Some(i as u32)
        } else {
            None
        }
    }

    // --- bin index (touched hugepages) ---------------------------------------

    /// Insert touched hugepage `i` into `bin`'s list and record the filed bin on the
    /// slot: a **completely full** hugepage at the *tail*, every other one at the head.
    ///
    /// A full hugepage can never fit a run, so filing it behind the ones that still can
    /// is what lets [`place`](Self::place)'s bounded per-bin scan spend its budget on real
    /// candidates. Full hugepages are *not* confined to the [`Full`](HugeBin::Full) bin
    /// `PACKING_ORDER` excludes — §19.4 files a **hot** full one in
    /// [`HotDense`](HugeBin::HotDense), the first bin scanned — so without this a hot
    /// workload's full hugepages would crowd the fittable ones past the cap.
    fn bin_insert(&mut self, i: u32, bin: HugeBin) {
        let b = bin as usize;
        let mut s = self.get(i);
        let at_tail = s.used() >= PAGES_PER_HUGEPAGE;
        s.bin = bin as u8;
        if at_tail {
            let tail = self.bin_tails[b];
            s.bin_prev = tail;
            s.bin_next = NIL;
            self.put(i, s);
            if tail != NIL {
                let mut t = self.get(tail);
                t.bin_next = i;
                self.put(tail, t);
            } else {
                self.bins[b] = i;
            }
            self.bin_tails[b] = i;
        } else {
            let head = self.bins[b];
            s.bin_prev = NIL;
            s.bin_next = head;
            self.put(i, s);
            if head != NIL {
                let mut h = self.get(head);
                h.bin_prev = i;
                self.put(head, h);
            } else {
                self.bin_tails[b] = i;
            }
            self.bins[b] = i;
        }
    }

    /// Remove touched hugepage `i` from its current filed bin.
    fn bin_remove(&mut self, i: u32) {
        let s = self.get(i);
        let bin = s.bin as usize;
        if s.bin_prev != NIL {
            let mut p = self.get(s.bin_prev);
            p.bin_next = s.bin_next;
            self.put(s.bin_prev, p);
        } else {
            debug_assert_eq!(self.bins[bin], i, "hugepage not at the head of its bin");
            self.bins[bin] = s.bin_next;
        }
        if s.bin_next != NIL {
            let mut n = self.get(s.bin_next);
            n.bin_prev = s.bin_prev;
            self.put(s.bin_next, n);
        } else {
            debug_assert_eq!(
                self.bin_tails[bin], i,
                "hugepage not at the tail of its bin"
            );
            self.bin_tails[bin] = s.bin_prev;
        }
    }

    /// Re-file touched hugepage `i` into the bin its current occupancy/state implies
    /// (H-003) **and the end of that bin its fullness implies**. Called after any change
    /// to a hugepage's `live`/`released`/`hotness`.
    ///
    /// Re-filing on the bin alone is not enough: occupancy also moves a hugepage between
    /// the fittable group (head) and the full group (tail) *without* changing its bin — a
    /// hot hugepage at 7/8 filling up stays [`HotDense`](HugeBin::HotDense). Left in
    /// place it would ossify in front of hugepages that can still fit a run, which is
    /// exactly what the split in [`bin_insert`](Self::bin_insert) exists to prevent. Since
    /// every occupancy change re-files, the resulting order is exact: every hugepage that
    /// can fit a run precedes every one that cannot (asserted by
    /// [`check_invariants`](Self::check_invariants)).
    fn refile(&mut self, i: u32) {
        let s = self.get(i);
        let target = s.target_bin();
        let at_right_end = if s.used() >= PAGES_PER_HUGEPAGE {
            self.bin_tails[target as usize] == i
        } else {
            self.bins[target as usize] == i
        };
        if s.bin != target as u8 || !at_right_end {
            self.bin_remove(i);
            self.bin_insert(i, target);
        }
    }

    // --- queries -------------------------------------------------------------

    /// The hugepage capacity (slots).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    /// The number of hugepages opened so far (touched into a bin).
    #[inline]
    pub fn touched(&self) -> usize {
        self.next_untouched as usize
    }

    /// Cumulative subrelease events (§19.6 metric).
    #[inline]
    pub fn subrelease_events(&self) -> u64 {
        self.subrelease_events
    }

    /// The filed [`HugeBin`] of the hugepage covering `addr`, or `None` if `addr`
    /// is outside the region or its hugepage is untouched.
    pub fn bin_of(&self, addr: usize) -> Option<HugeBin> {
        let i = self.index_of(addr)?;
        let s = self.get(i);
        if s.touched == 0 {
            return None;
        }
        Some(bin_from_u8(s.bin))
    }

    /// Live (allocated) pages in the hugepage covering `addr` (`0` if untouched /
    /// out of region) — the occupancy H-002 ties to contained allocations.
    pub fn used_pages_of(&self, addr: usize) -> usize {
        match self.index_of(addr) {
            Some(i) => self.get(i).used(),
            None => 0,
        }
    }

    /// The next **empty-backed** hugepage at slot index `>= from` — `(index, base,
    /// committed_pages)` — or `None` if none remains in `[from, touched)`. "Empty
    /// backed" = touched, `used == 0`, and still holding at least one **committed**
    /// page. The committed-page count is its live RSS (the pages the HugeCache holds
    /// backed for reuse). The demand-reserve shrink (W11-1b) walks this by ascending
    /// index so it deterministically visits every empty hugepage exactly once and
    /// always makes progress — unlike repeatedly taking the bin head, which can stall
    /// on a hugepage that cannot be released.
    ///
    /// A **partially subreleased** empty hugepage (some pages already returned, the
    /// rest still committed) is included: it is empty, it holds real RSS, and
    /// [`coverage`](Self::coverage) already reports its committed bytes as
    /// `empty_backed_bytes` — the supply the W12 controller plans against. Excluding
    /// it here (as this used to) made that supply un-reclaimable, so every tick
    /// re-planned release work the mechanism refused to execute (stranded RSS + a
    /// non-converging plan). A fully-released empty hugepage has nothing to reclaim
    /// and is skipped by the `committed > 0` test.
    fn next_empty_backed(&self, from: u32) -> Option<(u32, usize, usize)> {
        let mut i = from;
        while i < self.next_untouched {
            let s = self.get(i);
            if s.touched != 0 && s.used() == 0 {
                let committed = popcount(&s.committed);
                if committed != 0 {
                    return Some((i, self.huge_base(i), committed));
                }
            }
            i += 1;
        }
        None
    }

    /// The number of hugepages [`next_empty_backed`](Self::next_empty_backed) would
    /// visit — i.e. empty hugepages that still hold committed backing. This is the
    /// population the demand **reserve** counts (and whose committed bytes
    /// [`coverage`](Self::coverage) reports as `empty_backed_bytes`), so the reserve
    /// and the supply are drawn from the same set.
    fn empty_backed_hugepages(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0u32;
        while i < self.next_untouched {
            let s = self.get(i);
            if s.touched != 0 && s.used() == 0 && popcount(&s.committed) != 0 {
                n += 1;
            }
            i += 1;
        }
        n
    }

    /// The next **maximal committed run** of hugepage slot `hp` at page offset
    /// `>= from`, as `(offset, pages)`, or `None` when none remains.
    ///
    /// [`subrelease`](Self::subrelease) takes a *contiguous run*, so a caller that
    /// wants to reclaim a hugepage's whole RSS must walk its runs: a committed set is
    /// **not** in general the prefix `[0, popcount)` (an over-aligned placement leaves
    /// stride holes, and a subrelease-then-partial-refill leaves a released hole), and
    /// passing the popcount as a run length makes `subrelease` refuse the whole
    /// hugepage — reclaiming nothing.
    fn next_committed_run(&self, hp: u32, from: usize) -> Option<(usize, usize)> {
        if hp >= self.next_untouched {
            return None;
        }
        let s = self.get(hp);
        let mut off = from;
        while off < PAGES_PER_HUGEPAGE && !bit_get(&s.committed, off) {
            off += 1;
        }
        if off >= PAGES_PER_HUGEPAGE {
            return None;
        }
        let mut end = off;
        while end < PAGES_PER_HUGEPAGE && bit_get(&s.committed, end) {
            end += 1;
        }
        Some((off, end - off))
    }

    // --- placement (W11-2b candidate selection, W11-4a packing) --------------

    /// Whether `align` (a power of two ≤ [`HUGEPAGE_SIZE`]) admits an in-hugepage
    /// page offset, and the page stride between admissible offsets. `align ≤
    /// PAGE_SIZE` ⇒ every page-aligned offset works (stride 1); larger ⇒ offsets a
    /// multiple of `align / PAGE_SIZE` (a hugepage base is `HUGEPAGE_SIZE`-aligned, so
    /// `base + o*PAGE_SIZE` is `align`-aligned iff `o` is a multiple of that stride).
    #[inline]
    fn align_stride(align: usize) -> Option<usize> {
        if !align.is_power_of_two() || align > HUGEPAGE_SIZE {
            return None;
        }
        Some(if align <= PAGE_SIZE {
            1
        } else {
            align / PAGE_SIZE
        })
    }

    /// **§19.3 placement score (policy, DD-2).** A deterministic `i64` ranking a
    /// candidate hugepage for a `pages`-page run at `run_offset`, combining the §19.3
    /// terms over **backend-agnostic** inputs (page counts + the hotness/lifetime
    /// hints, never "is a hardware hugepage" — so W11-6/seLe4n reuse it). Higher is
    /// better; ties break on lower base in [`place`](Self::place). A wrong score never
    /// misplaces a live object (the run is carved from the free bitmap regardless).
    ///
    /// The §19.3 score, exactly as returned below: `packing + hotness_match +
    /// lifetime_match + release_preservation + commit_bonus − fragmentation −
    /// partial_subrelease`, where `lifetime_match` is *itself* negative on a mismatch
    /// (there is no separate lifetime-mismatch penalty term — the mismatch is the
    /// negative match). NUMA locality is not a term here: the live router segregates nodes
    /// by routing to per-node backends (W13), so every candidate in this filler shares a
    /// node and there is nothing to discriminate.
    fn score(s: &HugePageSlot, hints: PlaceHints, run_offset: usize, pages: usize) -> i64 {
        let used = s.used() as i64;
        let total = PAGES_PER_HUGEPAGE as i64;
        let subreleased = s.subreleased() as i64;
        let committed = popcount(&s.committed) as i64;
        // packing_bonus (§19.5): prefer fuller hugepages — fill partially-used over
        // opening new, so emptier hugepages stay releasable.
        let packing = used;
        // hotness_match_bonus: matching hotness keeps hot objects together (§19.5).
        let hotness_match = if s.hotness == hints.hotness as u8 {
            4
        } else {
            0
        };
        // lifetime grouping (§19.5): bonus for same-lifetime placement, penalty for
        // mixing very short-lived with long-lived objects (which fragments the
        // long-lived hugepage as the short-lived ones churn). Weighted so a *severe*
        // mismatch (short vs long, gap 2) on a non-dense hugepage can lose to opening
        // a fresh hugepage (`place`'s open-fresh-on-mismatch), realising §19.5 "avoid
        // mixing ... when possible" without disturbing a dense hugepage's coverage.
        let hp_life = lifetime_from_u8(s.lifetime);
        let life_gap =
            (lifetime_to_u8(hp_life) as i64 - lifetime_to_u8(hints.lifetime) as i64).abs();
        let lifetime_match = if used == 0 {
            0 // an empty hugepage has no resident lifetime to match
        } else if life_gap == 0 {
            3
        } else {
            -life_gap * LIFETIME_MISMATCH_WEIGHT
        };
        // NUMA locality is **not** a score term: the live router places a request on the
        // right node's backend by routing (W13), so every candidate in this filler is on the
        // same node — there is nothing to discriminate here.
        // release_preservation_bonus: avoid disturbing an empty hugepage held for
        // release/reuse (the HugeCache reserve) — a small penalty for opening one.
        let release_pres = if used == 0 { -3 } else { 0 };
        // packing/locality: prefer a run whose pages are already committed (no fault,
        // §19.5 "avoid immediate page faults") — a bonus when the whole run is backed.
        let commit_bonus = if run_is_fully_set(&s.committed, run_offset, pages) {
            2
        } else {
            0
        };
        // fragmentation_penalty: prefer a placement that leaves the hugepage tightly
        // packed — penalize the backed-but-free pages that would remain after the run
        // (a tight fit strands less reusable-but-idle backing), capped so it never
        // dominates packing. A run abutting the live frontier (low `run_offset`)
        // strands no gap before it; account that too.
        let backed_free_after = (committed - used - pages as i64).max(0).min(total);
        let fragmentation = (backed_free_after / 8) + (run_offset as i64).min(8);
        // partial_subrelease_penalty (§19.3): avoid re-filling a subreleased hugepage.
        let subrelease_pen = subreleased;
        packing + hotness_match + lifetime_match + release_pres + commit_bonus
            - fragmentation
            - subrelease_pen
    }

    /// The score a **brand-new (fresh) hugepage** would get for this request — used by
    /// [`place`](Self::place) to decide whether to open one rather than mix into a
    /// poorly-matched existing candidate (§19.5 "avoid mixing when possible"). A fresh
    /// hugepage has `used == 0` (so packing 0, no resident lifetime to match), is not
    /// yet committed (no commit bonus), and carries the `release_preservation`
    /// penalty for opening it.
    fn fresh_score(hints: PlaceHints) -> i64 {
        let hotness_match = if hints.hotness == Hotness::Neutral {
            4
        } else {
            0
        };
        // packing(0) + hotness_match + lifetime_match(0) + release_pres(-3) +
        // commit_bonus(0) − fragmentation(0) − subrelease(0).
        hotness_match - 3
    }

    /// **Place a `pages`-page run** at `align` with placement `hints` (§19.3/§19.5,
    /// W11-2b/W11-4a). Scans the bins in `PACKING_ORDER` (fullest-fitting first —
    /// **no full scan of all hugepages**, W11-2b), scores the fitting candidates, and
    /// carves the best (deterministic: best score, then lowest base). Opens a fresh
    /// hugepage only if no touched one fits. Marks the run **live** and re-files the
    /// bin; the caller commits [`Placement::commit_run`] and confirms with
    /// [`mark_committed`](Self::mark_committed) (or rolls back via [`free`](Self::free)).
    ///
    /// `None` if the request is malformed or the region is full/too fragmented — the
    /// filler is **unchanged** on failure (W4-5).
    ///
    /// SPEC-transition: hugepage span placement (§19.3 filler)
    pub fn place(&mut self, pages: usize, align: usize, hints: PlaceHints) -> Option<Placement> {
        if pages == 0 || pages > PAGES_PER_HUGEPAGE {
            return None;
        }
        let stride = Self::align_stride(align)?;

        // Candidate selection over the packing-ordered bins (bounded scan, W11-2b):
        // the densest bin that yields any fit wins (packing), best score within it.
        let mut best: Option<(u32, usize, i64)> = None; // (hugepage, run_offset, score)
        let mut scanned;
        for &bin in PACKING_ORDER.iter() {
            let mut i = self.bins[bin as usize];
            let mut found_in_bin = false;
            // The cap is **per bin**, not cumulative. `HotDense` is the first bin scanned
            // and `classify_bin` files a *completely full* hugepage there whenever it is
            // hot (only the non-hot full case becomes `Full`, which `PACKING_ORDER` omits),
            // so a hot workload accumulates full hugepages in the very first bin. With a
            // cumulative cap those alone exhausted the whole placement budget and aborted
            // the entire descent, so every request opened a fresh hugepage even though
            // `NearlyFull`/`Medium`/`Sparse` ones had room — until the region ran out and
            // the backend stopped serving altogether, exactly inverting the §19.5 packing
            // policy. Per-bin, exhausting one bin's budget only moves on to the next.
            scanned = 0;
            while i != NIL {
                let s = self.get(i);
                // The cap counts **every** node visited, a full hugepage included, so the
                // walk is bounded by `SCAN_CAP` regardless of what the bin holds — the
                // placement cost cannot grow with the backend's capacity. `bin_insert`
                // files full hugepages at the tail, behind every one that can still fit a
                // run, so the budget is still spent on real candidates first.
                scanned += 1;
                // A full hugepage can never fit a run; skip it without walking its bitmap.
                // `Full` is excluded from `PACKING_ORDER` for exactly this reason, but a
                // *hot* full hugepage is filed `HotDense`, which is not.
                if s.used() >= PAGES_PER_HUGEPAGE {
                    i = s.bin_next;
                    if scanned >= SCAN_CAP {
                        break;
                    }
                    continue;
                }
                if let Some(off) = find_run_aligned(&s.live, pages, stride) {
                    found_in_bin = true;
                    let sc = Self::score(&s, hints, off, pages);
                    let better = match best {
                        None => true,
                        Some((bi, _, bsc)) => {
                            sc > bsc || (sc == bsc && self.huge_base(i) < self.huge_base(bi))
                        }
                    };
                    if better {
                        best = Some((i, off, sc));
                    }
                }
                i = s.bin_next;
                // Approximate-bin cap (§19.3 "MAY use approximate bins"): never scan
                // an unbounded number of hugepages within one bin.
                if scanned >= SCAN_CAP {
                    break;
                }
            }
            // Packing (§19.5): a fit in a denser bin is preferred — stop descending
            // to emptier bins once this bin yields one.
            if found_in_bin {
                break;
            }
        }

        let (hp, off) = match best {
            // §19.5 "avoid mixing ... when possible": if the best existing candidate
            // scores *below* a fresh hugepage (a severe lifetime mismatch on a
            // non-dense hugepage outweighs its packing bonus) and the region still has
            // a fresh hugepage, open one to segregate lifetimes. A dense or
            // well-matched candidate keeps a score above the fresh threshold and is
            // used, preserving coverage and density.
            Some((hp, off, sc)) => {
                if sc < Self::fresh_score(hints) && self.next_untouched < self.capacity {
                    match self.open_fresh() {
                        Some(fresh) => (fresh, 0),
                        None => (hp, off),
                    }
                } else {
                    (hp, off)
                }
            }
            // No touched hugepage fits: open a fresh one (HugeAllocator, §19.2).
            None => {
                let fresh = self.open_fresh()?;
                debug_assert!(find_run_aligned(&self.get(fresh).live, pages, stride) == Some(0));
                (fresh, 0)
            }
        };
        Some(self.carve(hp, off, pages, hints))
    }

    /// Open the next never-touched hugepage into the [`EmptyBacked`](HugeBin::EmptyBacked)
    /// bin (a fresh, fully-reserved hugepage — the §19.2 HugeAllocator reservation),
    /// or `None` if the region is exhausted.
    fn open_fresh(&mut self) -> Option<u32> {
        if self.next_untouched >= self.capacity {
            return None;
        }
        let i = self.next_untouched;
        self.next_untouched += 1;
        let mut s = self.get(i);
        debug_assert_eq!(s.touched, 0, "opening an already-touched hugepage");
        s.touched = 1;
        s.hotness = Hotness::Neutral as u8;
        s.lifetime = lifetime_to_u8(Lifetime::Unspecified);
        self.put(i, s);
        // used == 0, no subrelease ⇒ EmptyBacked (H-003).
        self.bin_insert(i, HugeBin::EmptyBacked);
        Some(i)
    }

    /// Mark the `pages`-page run at page offset `off` of hugepage `hp` live and
    /// re-file the bin, returning the [`Placement`] (with the `commit_run` the caller
    /// must back). Joins the request's hotness/lifetime into the hugepage's (§19.5).
    /// Pure bookkeeping (the caller drives the provider).
    fn carve(&mut self, hp: u32, off: usize, pages: usize, hints: PlaceHints) -> Placement {
        let mut s = self.get(hp);
        debug_assert!(run_is_clear(&s.live, off, pages), "carving over a live run");
        // Does any page in the run need committing (uncommitted or subreleased)?
        let needs_commit = run_popcount(&s.committed, off, pages) != pages;
        let mut k = off;
        while k < off + pages {
            bit_set(&mut s.live, k);
            k += 1;
        }
        // Mark the allocation's first page so `free` can reject a partial/interior
        // [`Region`] (exact-extent validation, M-004/S-007).
        bit_set(&mut s.heads, off);
        s.hotness = Hotness::from_u8(s.hotness).join(hints.hotness) as u8;
        s.lifetime = lifetime_to_u8(lifetime_join(lifetime_from_u8(s.lifetime), hints.lifetime));
        self.put(hp, s);
        self.refile(hp);
        let base = self.huge_base(hp) + off * PAGE_SIZE;
        let commit_run = if needs_commit {
            Some((self.region_offset(base), pages * PAGE_SIZE))
        } else {
            None
        };
        Placement {
            hugepage: hp,
            base,
            pages,
            commit_run,
        }
    }

    /// Validate a **public, forgeable confirmation token**'s `(hugepage, base, pages)`
    /// against the region geometry, returning the in-hugepage page offset iff it names
    /// a real, in-bounds, page-aligned run of hugepage `hp` — else `None`.
    /// [`Placement`]/[`HugeRun`]/[`Subrelease`] are `pub` with `pub` fields, so a
    /// downstream crate could hand a forged one to a safe `pub fn`; the confirmation
    /// methods run this first so a bad token is ignored, never resolved into an
    /// out-of-bounds slot or a wrapping address (S-007 — no safe path to UB).
    fn validate_run(&self, hp: u32, base: usize, pages: usize) -> Option<usize> {
        if hp >= self.capacity || pages == 0 {
            return None;
        }
        let byte_off = base.checked_sub(self.huge_base(hp))?;
        if !byte_off.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let off = byte_off / PAGE_SIZE;
        if off >= PAGES_PER_HUGEPAGE || pages > PAGES_PER_HUGEPAGE - off {
            return None;
        }
        Some(off)
    }

    /// Confirm a [`Placement`]'s backing after the caller's `commit` succeeded
    /// (M-005): mark every page of the run committed and clear any subreleased bit
    /// (the pages are backed again). Restores `live ⊆ committed` and
    /// `committed ∩ released == ∅` (H-001). A forged/foreign token is ignored
    /// (`validate_run`).
    pub fn mark_committed(&mut self, p: &Placement) {
        let Some(off) = self.validate_run(p.hugepage, p.base, p.pages) else {
            return;
        };
        let mut s = self.get(p.hugepage);
        let mut k = off;
        while k < off + p.pages {
            bit_set(&mut s.committed, k);
            bit_clear(&mut s.released, k);
            k += 1;
        }
        self.put(p.hugepage, s);
        // Clearing subreleased bits can change the bin (PartialSubreleased → …).
        self.refile(p.hugepage);
    }

    // --- free (W11-4a return) ------------------------------------------------

    /// Free the `pages`-page run based at `base` (the inverse of a [`place`](Self::place)
    /// / the private `carve`): clear its live bits and re-file the bin. The pages
    /// stay **committed** (retain — the HugeCache keeps them backed for cheap reuse,
    /// §19.2/§20.5). The run **must be fully live** (the base/length of a prior
    /// placement); a foreign/untouched/non-page-aligned base or a double free is
    /// rejected ([`FreeReport::valid`] `== false`) with no mutation (M-004 — never
    /// re-free a range that may have been recycled). [`FreeReport::now_empty`]
    /// reports whether the hugepage is now fully empty (the H-004 release gate).
    ///
    /// SPEC-transition: hugepage span free (§19 filler, object `Live -> free`)
    pub fn free(&mut self, base: usize, pages: usize) -> FreeReport {
        let hp = match self.index_of(base) {
            Some(i) => i,
            None => return FreeReport::INVALID,
        };
        let s0 = self.get(hp);
        if s0.touched == 0 || pages == 0 || !(base - self.region_base).is_multiple_of(PAGE_SIZE) {
            return FreeReport::INVALID;
        }
        let off = (base - self.huge_base(hp)) / PAGE_SIZE;
        if off + pages > PAGES_PER_HUGEPAGE {
            return FreeReport::INVALID;
        }
        // The whole run must be live — else this is a double free or a wrong range
        // (M-004: do not act on a non-live run).
        if run_popcount(&s0.live, off, pages) != pages {
            return FreeReport::INVALID;
        }
        // Exact-extent validation (M-004/S-007): `Region` is a public, copyable
        // descriptor, so a forged one could name a *partial/interior* subrange of a
        // live allocation; freeing it would clear only some of the allocation's live
        // bits, leaving a live remainder that aliases when the freed pages are reused.
        // Require `base` to be an allocation start, `pages` its exact length, and the
        // hugepage not part of a multi-hugepage run (which frees via `free_hugepages`).
        if s0.in_run != 0 || !bit_get(&s0.heads, off) {
            return FreeReport::INVALID;
        }
        let mut e = off + 1;
        while e < off + pages {
            if bit_get(&s0.heads, e) {
                return FreeReport::INVALID; // `pages` spans into a later allocation
            }
            e += 1;
        }
        // `pages` must not stop short of the allocation's real end: the page at
        // `off + pages` must be a boundary — the hugepage edge, a later allocation's
        // head, or a non-live page — never the live interior of *this* allocation.
        if off + pages < PAGES_PER_HUGEPAGE
            && bit_get(&s0.live, off + pages)
            && !bit_get(&s0.heads, off + pages)
        {
            return FreeReport::INVALID;
        }
        let mut s = self.get(hp);
        bit_clear(&mut s.heads, off);
        let mut k = off;
        while k < off + pages {
            bit_clear(&mut s.live, k);
            k += 1;
        }
        let now_empty = popcount(&s.live) == 0;
        if now_empty {
            // An emptied hugepage forgets its hotness/lifetime (no residents); the
            // next resident sets them afresh.
            s.hotness = Hotness::Neutral as u8;
            s.lifetime = lifetime_to_u8(Lifetime::Unspecified);
        }
        self.put(hp, s);
        self.refile(hp);
        FreeReport {
            valid: true,
            now_empty,
        }
    }

    /// **In-place tail trim (§25.3 / W15-3b cache-served shrink).** Shrink the live
    /// allocation based at `base` from `old_pages` to `new_pages` (`1 ≤ new < old`),
    /// freeing its tail pages `[off+new, off+old)` back to the filler so it can pack
    /// them into this hugepage again. The kept prefix `[off, off+new)` stays live
    /// **with its head**, so a later [`free`](Self::free)`(base, new_pages)` validates
    /// exactly. Returns `true` on success, `false` (no change) for a bad/non-live
    /// range — the same exact-extent validation as `free` (the run must be a single
    /// live allocation based at `off`).
    ///
    /// Unlike `free`/`subrelease`, this is **not** reachable from a public, forgeable
    /// `Region` (S-007): it is called only by the trusted realloc path, which supplies
    /// the true `old_pages` from the live descriptor — so it cannot be tricked into a
    /// partial free of someone else's allocation. The freed tail stays **committed**
    /// (reusable by the filler); the W12 release controller subreleases cold pages
    /// later, exactly as the extent shrink leaves its tail Dirty under the retain
    /// policy.
    ///
    /// SPEC-transition: large in-place shrink (§25.3, W15-3b) — frees the
    /// allocation's tail pages: the same page-bitmap free as [`free`](Self::free) /
    /// [`subrelease`](Self::subrelease) over a sub-run, so the §19.8 H-invariants are
    /// preserved, debug-asserted by [`check_invariants`](Self::check_invariants).
    pub fn trim(&mut self, base: usize, old_pages: usize, new_pages: usize) -> bool {
        if new_pages == 0 || new_pages >= old_pages {
            return false;
        }
        let hp = match self.index_of(base) {
            Some(i) => i,
            None => return false,
        };
        let s0 = self.get(hp);
        if s0.touched == 0 || !(base - self.region_base).is_multiple_of(PAGE_SIZE) {
            return false;
        }
        let off = (base - self.huge_base(hp)) / PAGE_SIZE;
        if off + old_pages > PAGES_PER_HUGEPAGE {
            return false;
        }
        // Exact-extent validation (M-004/S-007), identical to `free`: `base` must be an
        // allocation start, `old_pages` its exact live length, not part of a multi-
        // hugepage run, with a real boundary after it — never a partial/interior range.
        if s0.in_run != 0 || !bit_get(&s0.heads, off) {
            return false;
        }
        if run_popcount(&s0.live, off, old_pages) != old_pages {
            return false;
        }
        let mut e = off + 1;
        while e < off + old_pages {
            if bit_get(&s0.heads, e) {
                return false; // `old_pages` spans into a later allocation
            }
            e += 1;
        }
        if off + old_pages < PAGES_PER_HUGEPAGE
            && bit_get(&s0.live, off + old_pages)
            && !bit_get(&s0.heads, off + old_pages)
        {
            return false; // `old_pages` stops short of this allocation's real end
        }
        // Free only the tail `[off+new, off+old)`; the head at `off` stays, so the
        // kept prefix remains the same live allocation (just shorter).
        let mut s = self.get(hp);
        let mut k = off + new_pages;
        while k < off + old_pages {
            bit_clear(&mut s.live, k);
            k += 1;
        }
        self.put(hp, s);
        self.refile(hp);
        debug_assert!(self.check_invariants());
        true
    }

    // --- partial subrelease (W11-4b, H-005) ----------------------------------

    /// **§19.6 partial subrelease (W11-4b).** Attempt to return the `pages`-page run
    /// based at `base` to the OS. **Refuses** (returns `None`, no change) unless
    /// **every §19.6 guard** passes:
    ///
    /// * **H-005** — the run intersects **no live page** (`run_is_clear(live)`); this
    ///   is the single load-bearing safety guard;
    /// * the run is page-aligned to the release granularity (always — the filler works
    ///   in whole pages) and currently **backed** (committed, not already released);
    /// * the hugepage is **cold/sparse** *or* `pressure_high` (§19.6 coldness/pressure
    ///   gate);
    /// * the **cost/benefit gate** passes: the predicted RSS benefit (the `pages`
    ///   reclaimed) is at least the predicted fragmentation cost, which scales with the
    ///   hugepage's live coverage (`used / FRAGMENTATION_COST_DIVISOR`). So a small run
    ///   from a *dense* hugepage is refused (its TLB coverage is repaid only by a larger
    ///   run), while any run from a near-empty hugepage passes. `pressure_high` forces
    ///   every backed-free byte back regardless of the gate (§20.5 emergency).
    ///
    /// On success it marks the run released, re-files the bin, bumps the subrelease
    /// metric, and returns the [`Subrelease`] for the caller to `decommit`. The
    /// caller restores via [`unsubrelease`](Self::unsubrelease) on a decommit failure
    /// (recommitting the pages).
    ///
    /// SPEC-transition: partial subrelease (§19.6)
    pub fn subrelease(
        &mut self,
        base: usize,
        pages: usize,
        pressure_high: bool,
    ) -> Option<Subrelease> {
        let hp = self.index_of(base)?;
        let s = self.get(hp);
        // Reject an unaligned base before deriving `off` (else a mid-page base would
        // truncate `off` and the H-005 check below could test the wrong pages —
        // defense-in-depth, mirroring `free`).
        if s.touched == 0 || pages == 0 || !(base - self.region_base).is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let off = (base - self.huge_base(hp)) / PAGE_SIZE;
        if off + pages > PAGES_PER_HUGEPAGE {
            return None;
        }
        // H-005: never release a subrange intersecting a live object.
        if !run_is_clear(&s.live, off, pages) {
            return None;
        }
        // The run must be currently backed (committed) — there is nothing to return
        // from an unbacked run, and forcing it released would double-count (H-004:
        // only backed-free pages are released).
        if run_popcount(&s.committed, off, pages) != pages {
            return None;
        }
        // Coldness / pressure gate (§19.6): only a cold/sparse hugepage, or one under
        // pressure, may subrelease (preserve hugepage coverage otherwise, §20.5).
        let bin = bin_from_u8(s.bin);
        let cold_or_sparse = matches!(
            bin,
            HugeBin::ColdSparse | HugeBin::Sparse | HugeBin::NearlyEmpty | HugeBin::EmptyBacked
        ) || Hotness::from_u8(s.hotness) == Hotness::Cold;
        if !(cold_or_sparse || pressure_high) {
            return None;
        }
        // §19.6 cost/benefit gate: predicted RSS benefit (`pages` reclaimed) vs
        // predicted fragmentation cost. The cost scales with the hugepage's live
        // coverage (`used`): subreleasing from a denser hugepage risks more TLB
        // coverage and is repaid only by a larger run, while a near-empty hugepage's
        // coverage is already low so even a small run is worthwhile. So subrelease
        // only when `benefit ≥ cost` — unless pressure forces every byte back.
        let predicted_cost = s.used() / FRAGMENTATION_COST_DIVISOR;
        if !pressure_high && pages < predicted_cost {
            return None;
        }
        // Commit the subrelease: mark the run released (committed → released).
        let mut s = self.get(hp);
        let mut k = off;
        while k < off + pages {
            bit_clear(&mut s.committed, k);
            bit_set(&mut s.released, k);
            k += 1;
        }
        self.put(hp, s);
        self.refile(hp);
        self.subrelease_events += 1;
        Some(Subrelease {
            hugepage: hp,
            region_offset: self.region_offset(base),
            len: pages * PAGE_SIZE,
        })
    }

    /// Roll back a [`Subrelease`] whose `decommit` failed: recommit the released run
    /// (mark it committed again, clear released). Leaves the filler well-formed
    /// (W4-5) — the pages are simply backed-free once more.
    pub fn unsubrelease(&mut self, sr: &Subrelease) {
        let pages = sr.len / PAGE_SIZE;
        // Validate the forgeable token before touching the slot pool (S-007).
        let Some(base) = self.region_base.checked_add(sr.region_offset) else {
            return;
        };
        let Some(off) = self.validate_run(sr.hugepage, base, pages) else {
            return;
        };
        let mut s = self.get(sr.hugepage);
        let mut k = off;
        while k < off + pages {
            bit_set(&mut s.committed, k);
            bit_clear(&mut s.released, k);
            k += 1;
        }
        self.put(sr.hugepage, s);
        self.refile(sr.hugepage);
    }

    // --- whole-hugepage runs (W11-1a, the large/awkward path) ----------------

    /// **Reserve `n` address-contiguous whole hugepages** (§19.2 HugeAllocator,
    /// W11-1a) for a large allocation of `n` hugepages. Prefers `n` consecutive
    /// already-empty hugepages (the HugeCache, no new address space) and otherwise
    /// extends the touched frontier with fresh ones. Marks **every page** of each
    /// hugepage live and re-files each to [`Full`](HugeBin::Full); the caller commits
    /// the run and confirms with [`mark_run_committed`](Self::mark_run_committed) (or
    /// rolls back with [`free_hugepages`](Self::free_hugepages)). `None` if no `n`
    /// contiguous free hugepages exist (safe failure, unchanged filler).
    ///
    /// SPEC-transition: hugepage run reservation (§19.2 / §18.5 large path)
    pub fn reserve_hugepages(&mut self, n: usize) -> Option<HugeRun> {
        if n == 0 || n > self.capacity as usize {
            return None;
        }
        // Prefer reusing empty touched hugepages (HugeCache); else extend the
        // frontier with fresh ones (both keep the run address-contiguous and the
        // `next_untouched` frontier monotonic).
        let start = match self.find_consecutive_empty(n) {
            Some(s) => s,
            None => {
                if (self.next_untouched as usize).checked_add(n)? > self.capacity as usize {
                    return None;
                }
                let s = self.next_untouched;
                self.next_untouched += n as u32;
                s
            }
        };
        let mut needs_commit = false;
        let mut j = start;
        while j < start + n as u32 {
            let mut s = self.get(j);
            if s.touched == 0 {
                s.touched = 1;
                s.bin = HugeBin::EmptyBacked as u8;
                s.bin_prev = NIL;
                s.bin_next = NIL;
                self.put(j, s);
                // Insert into the EmptyBacked bin so the carve below can re-file it.
                self.bin_insert(j, HugeBin::EmptyBacked);
            }
            // Carve the whole hugepage live (a `Full` page run).
            let p = self.carve(j, 0, PAGES_PER_HUGEPAGE, PlaceHints::default());
            needs_commit |= p.commit_run.is_some();
            j += 1;
        }
        self.mark_run(start, n);
        let base = self.huge_base(start);
        let commit_run = if needs_commit {
            Some((self.region_offset(base), n * HUGEPAGE_SIZE))
        } else {
            None
        };
        Some(HugeRun {
            base,
            hugepages: n,
            commit_run,
        })
    }

    /// Mark hugepages `[start, start + n)` as one multi-hugepage **run**: every hugepage
    /// is a member ([`in_run`](HugePageSlot::in_run)), and the start records the run
    /// length `n` ([`run_len`](HugePageSlot::run_len)). Lets
    /// [`free_hugepages`](Self::free_hugepages) validate an *exact-run* free and the
    /// packed [`free`](Self::free) refuse a run member — so a forged sub-hugepage
    /// [`Region`] can never partially free a run (M-004/S-007).
    fn mark_run(&mut self, start: u32, n: usize) {
        let mut j = start;
        while j < start + n as u32 {
            let mut s = self.get(j);
            s.in_run = 1;
            s.run_len = if j == start { n as u32 } else { 0 };
            self.put(j, s);
            j += 1;
        }
    }

    /// Re-reserve `n` contiguous whole hugepages **at a specific `base`** — the
    /// §18.6 region cache's reuse primitive (W11-3): a previously-freed run is
    /// re-claimed in O(1) (no scan) **iff** its hugepages are still empty
    /// (`used == 0`). `None` if `base` is not a hugepage start, the run runs past the
    /// region, or any hugepage has since been re-used (a stale cache entry — the
    /// caller drops it, never double-vending). On success the run is carved
    /// [`Full`](HugeBin::Full); the caller commits and confirms as for
    /// [`reserve_hugepages`](Self::reserve_hugepages).
    pub fn reserve_hugepages_at(&mut self, base: usize, n: usize) -> Option<HugeRun> {
        let start = self.index_of(base)?;
        if base != self.huge_base(start) || n == 0 || (start as usize) + n > self.capacity as usize
        {
            return None;
        }
        // Every hugepage of the run must be touched + empty (else stale / unbacked).
        let mut j = start;
        while j < start + n as u32 {
            let s = self.get(j);
            if s.touched == 0 || s.used() != 0 {
                return None;
            }
            j += 1;
        }
        let mut needs_commit = false;
        let mut j = start;
        while j < start + n as u32 {
            let p = self.carve(j, 0, PAGES_PER_HUGEPAGE, PlaceHints::default());
            needs_commit |= p.commit_run.is_some();
            j += 1;
        }
        self.mark_run(start, n);
        let commit_run = if needs_commit {
            Some((self.region_offset(base), n * HUGEPAGE_SIZE))
        } else {
            None
        };
        Some(HugeRun {
            base,
            hugepages: n,
            commit_run,
        })
    }

    /// Find `n` address-contiguous **empty** (`used == 0`) touched hugepages within
    /// the touched region `[0, next_untouched)`, returning the lowest such start
    /// index (the HugeCache reuse scan). `None` if no such run exists.
    fn find_consecutive_empty(&self, n: usize) -> Option<u32> {
        let mut run = 0usize;
        let mut i = 0u32;
        while i < self.next_untouched {
            if self.get(i).used() == 0 {
                run += 1;
                if run == n {
                    return Some(i + 1 - n as u32);
                }
            } else {
                run = 0;
            }
            i += 1;
        }
        None
    }

    /// Confirm a [`HugeRun`]'s backing after the caller's `commit` succeeded (M-005):
    /// mark every page of the run committed and clear any subreleased bit, then
    /// re-file each hugepage. Restores `live ⊆ committed` (H-001). A forged/foreign
    /// token (base not a real hugepage start, or a run running past the region) is
    /// ignored rather than resolved into an out-of-bounds slot (S-007).
    pub fn mark_run_committed(&mut self, run: &HugeRun) {
        let start = match self.index_of(run.base) {
            Some(s) if run.base == self.huge_base(s) => s,
            _ => return,
        };
        if run.hugepages == 0
            || run.hugepages > self.capacity as usize
            || start as usize > self.capacity as usize - run.hugepages
        {
            return;
        }
        let mut j = start;
        while j < start + run.hugepages as u32 {
            let mut s = self.get(j);
            s.committed = s.live; // the whole hugepage is live ⇒ committed
            s.released = [0; BITMAP_WORDS];
            self.put(j, s);
            self.refile(j);
            j += 1;
        }
    }

    /// Free a whole-hugepage run reserved by [`reserve_hugepages`](Self::reserve_hugepages):
    /// clear every page of each of the `n` hugepages (they return to
    /// [`EmptyBacked`](HugeBin::EmptyBacked) in the HugeCache, still committed for
    /// reuse). Validates the run was fully live; rejects a foreign/partial run with
    /// no mutation ([`FreeReport::valid`] `== false`). `now_empty` is always `true`
    /// for a valid free (every page is cleared).
    pub fn free_hugepages(&mut self, base: usize, n: usize) -> FreeReport {
        let start = match self.index_of(base) {
            Some(i) => i,
            None => return FreeReport::INVALID,
        };
        if n == 0
            || (start as usize) + n > self.capacity as usize
            || !(base - self.region_base).is_multiple_of(HUGEPAGE_SIZE)
        {
            return FreeReport::INVALID;
        }
        // Exact-run validation (M-004/S-007): `base` must START a run of EXACTLY `n`
        // hugepages (`run_len == n`), and every hugepage of it must be a fully-live
        // member. So a forged partial-run free (`n' ≠ n`, or a base that is an interior
        // hugepage of a larger run) is rejected — it can never free a strict subset of
        // a multi-hugepage allocation and leave a live remainder.
        if self.get(start).run_len as usize != n {
            return FreeReport::INVALID;
        }
        let mut j = start;
        while j < start + n as u32 {
            let s = self.get(j);
            if s.touched == 0 || s.in_run == 0 || s.used() != PAGES_PER_HUGEPAGE {
                return FreeReport::INVALID;
            }
            j += 1;
        }
        let mut j = start;
        while j < start + n as u32 {
            let mut s = self.get(j);
            s.live = [0; BITMAP_WORDS];
            s.heads = [0; BITMAP_WORDS];
            s.in_run = 0;
            s.run_len = 0;
            s.hotness = Hotness::Neutral as u8;
            s.lifetime = lifetime_to_u8(Lifetime::Unspecified);
            self.put(j, s);
            self.refile(j);
            j += 1;
        }
        FreeReport {
            valid: true,
            now_empty: true,
        }
    }

    /// Mark the hugepage covering `addr` **cold** (§19.6) — the release controller's
    /// idle-decay signal (plan 04 W12). A cold hugepage's free pages become
    /// subrelease candidates even when denser than sparse, and a *sparse* cold
    /// hugepage re-files to [`ColdSparse`](HugeBin::ColdSparse) (the prime subrelease
    /// bin). Coldness is an idle property the controller sets, **not** a per-request
    /// hotness hint (which only ever marks a hugepage *hot*, via the resident join).
    /// Returns `false` for a foreign/untouched address.
    pub fn mark_cold(&mut self, addr: usize) -> bool {
        let i = match self.index_of(addr) {
            Some(i) => i,
            None => return false,
        };
        let mut s = self.get(i);
        if s.touched == 0 {
            return false;
        }
        s.hotness = Hotness::Cold as u8;
        self.put(i, s);
        self.refile(i);
        true
    }

    /// The region offset of an absolute address within the tiled region.
    #[inline]
    fn region_offset(&self, addr: usize) -> usize {
        addr - self.region_base
    }

    // --- coverage metrics (§19.7, W11-5) -------------------------------------

    /// The §19.7 hugepage coverage metrics over every touched hugepage (a slow stats
    /// walk). Every byte is counted in **exactly one** category, so the totals
    /// reconcile: `live_total == live_on_intact + live_on_partial`, and the released
    /// bytes partition into `empty_released_bytes` (on empty hugepages) +
    /// `partial_subreleased_bytes` (on hugepages that still hold a live object).
    pub fn coverage(&self) -> HugeStats {
        let mut st = HugeStats::default();
        let mut i = 0u32;
        while i < self.next_untouched {
            let s = self.get(i);
            if s.touched != 0 {
                let used = s.used();
                let committed = popcount(&s.committed);
                let released = s.subreleased();
                let live_b = (used * PAGE_SIZE) as u64;
                st.coverage_bytes += HUGEPAGE_SIZE as u64;
                st.live_total_bytes += live_b;
                st.bins[s.bin as usize] += 1; // §19.4 bin distribution (W11-4a)
                if used == 0 {
                    // Empty hugepage: split its bytes into backed vs released — its
                    // released bytes are `empty_released`, not `partial_subreleased`
                    // (there is no live object to make the hugepage "partial").
                    st.empty_backed_bytes += (committed * PAGE_SIZE) as u64;
                    st.empty_released_bytes += (released * PAGE_SIZE) as u64;
                } else {
                    // In-use hugepage: live bytes on an intact vs partial hugepage,
                    // backed-but-free pages are fragmentation, and any released bytes
                    // are the §19.6 partial-subrelease metric.
                    if released == 0 {
                        st.live_bytes_on_intact += live_b;
                    } else {
                        st.live_bytes_on_partial += live_b;
                        st.partial_subreleased_bytes += (released * PAGE_SIZE) as u64;
                    }
                    let backed_free = committed.saturating_sub(used);
                    st.fragmentation_bytes += (backed_free * PAGE_SIZE) as u64;
                }
            }
            i += 1;
        }
        st
    }

    // --- invariants (§19.8, B.4) ---------------------------------------------

    /// Whether the filler is well-formed (the H-001..H-003 oracle, B.4): for every
    /// touched hugepage `live ⊆ committed` and `committed ∩ released == ∅` (H-001),
    /// the filed bin equals [`classify_bin`] of the live occupancy (H-003 — H-002 is
    /// definitional, the `live` bitmap *is* the contained-allocation sum), the bin
    /// lists are consistent, and untouched slots are empty and unbinned.
    /// `debug_assert`ed after every mutation by the backend.
    pub fn check_invariants(&self) -> bool {
        // 1. Per-slot state + bin agreement.
        let mut binned = [0usize; HugeBin::COUNT];
        let mut i = 0u32;
        while i < self.capacity {
            let s = self.get(i);
            if i >= self.next_untouched {
                // Beyond the touched frontier: must be a pristine untouched slot.
                if s.touched != 0
                    || popcount(&s.live) != 0
                    || popcount(&s.committed) != 0
                    || popcount(&s.heads) != 0
                    || s.in_run != 0
                    || s.run_len != 0
                {
                    return false;
                }
            } else if s.touched == 1 {
                // H-001: live ⊆ committed and committed ∩ released == ∅.
                let mut w = 0;
                while w < BITMAP_WORDS {
                    if s.live[w] & !s.committed[w] != 0 {
                        return false; // a live page not committed
                    }
                    if s.committed[w] & s.released[w] != 0 {
                        return false; // a page both committed and released
                    }
                    if s.live[w] & s.released[w] != 0 {
                        return false; // a live page released (H-001/H-005)
                    }
                    if s.heads[w] & !s.live[w] != 0 {
                        return false; // an allocation head is not live (heads ⊆ live)
                    }
                    w += 1;
                }
                // Run consistency (S-007 exact-extent metadata): a run member is fully
                // live, and a `run_len` start is itself a member.
                if s.in_run != 0 && s.used() != PAGES_PER_HUGEPAGE {
                    return false;
                }
                if s.run_len != 0 && s.in_run == 0 {
                    return false;
                }
                // H-003: filed bin == classify_bin(occupancy/state).
                if bin_from_u8(s.bin) as u8 != s.target_bin() as u8 {
                    return false;
                }
                binned[s.bin as usize] += 1;
            } else {
                return false; // a slot below the frontier must be touched
            }
            i += 1;
        }
        // 2. Each bin list is well-formed, ordered fittable-before-full, and counts match.
        for (b, (&head, &expected)) in self.bins.iter().zip(binned.iter()).enumerate() {
            let mut j = head;
            let mut prev = NIL;
            let mut seen = 0usize;
            let mut seen_full = false;
            while j != NIL {
                let s = self.get(j);
                if s.touched != 1 || s.bin as usize != b || s.bin_prev != prev {
                    return false;
                }
                // Every hugepage that can still fit a run precedes every one that cannot
                // (`bin_insert` files full ones at the tail, `refile` keeps them there).
                // This is what makes `place`'s bounded per-bin scan spend its budget on
                // real candidates instead of on hugepages with no room.
                let full = s.used() >= PAGES_PER_HUGEPAGE;
                if seen_full && !full {
                    return false;
                }
                seen_full |= full;
                prev = j;
                j = s.bin_next;
                seen += 1;
                if seen > self.capacity as usize {
                    return false; // cycle guard
                }
            }
            if seen != expected {
                return false; // a touched hugepage missing from / extra in its bin
            }
            if self.bin_tails[b] != prev {
                return false; // the tail pointer must name the list's last node
            }
        }
        true
    }
}

/// Whether every page in `[start, start + len)` is set in `bm` (used by the score's
/// "whole run already committed" test).
#[inline]
fn run_is_fully_set(bm: &[u64; BITMAP_WORDS], start: usize, len: usize) -> bool {
    run_popcount(bm, start, len) == len
}

/// Find the first `stride`-aligned page offset whose `[off, off+pages)` run is clear
/// in `live` (no allocated page), or `None`. The §19.3 in-hugepage fit search.
#[inline]
fn find_run_aligned(live: &[u64; BITMAP_WORDS], pages: usize, stride: usize) -> Option<usize> {
    let mut off = 0usize;
    while off + pages <= PAGES_PER_HUGEPAGE {
        if run_is_clear(live, off, pages) {
            return Some(off);
        }
        off += stride;
    }
    None
}

/// Decode a filed-bin byte (total: an unknown value reads as
/// [`EmptyBacked`](HugeBin::EmptyBacked), the safe "in no special state" reading).
#[inline]
fn bin_from_u8(v: u8) -> HugeBin {
    match v {
        1 => HugeBin::NearlyEmpty,
        2 => HugeBin::Sparse,
        3 => HugeBin::Medium,
        4 => HugeBin::NearlyFull,
        5 => HugeBin::Full,
        6 => HugeBin::PartialSubreleased,
        7 => HugeBin::ColdSparse,
        8 => HugeBin::HotDense,
        _ => HugeBin::EmptyBacked,
    }
}

// ---------------------------------------------------------------------------
// Filler tuning constants (§19.3/§19.6 policy — centralized, not scattered).
//
// These steer *placement and release policy only*; none is a safety input (DD-2),
// so a different value can change fragmentation/RSS behaviour but never correctness.
// They are gathered here (rather than buried at use sites) so the policy surface is
// auditable in one place; unlike the size-class table (DD-1) they are not machine-
// checked, because the §33.4 theorems hold for *any* value (the score/gate are
// pure policy). The release controller (plan 04 W12) may make some of these dynamic.
// ---------------------------------------------------------------------------

/// The per-placement candidate-scan cap (§19.3 "MAY use approximate bins rather than
/// scanning all hugepages"): a single [`place`](HugePageFiller::place) examines at
/// most this many hugepages across the packing-ordered bins, so placement is bounded
/// regardless of how many hugepages exist (W11-2b "no full scan").
const SCAN_CAP: usize = 64;

/// The §19.6 cost/benefit divisor: a subrelease's predicted fragmentation cost is the
/// hugepage's live page count divided by this, so a near-empty hugepage (low live
/// coverage) admits even small subreleases while a denser one needs a larger run to
/// repay the lost coverage. `8` makes a fully-live hugepage cost `PAGES_PER_HUGEPAGE/8`
/// (16 pages) — i.e. only a substantial run is worth disrupting a dense hugepage.
const FRAGMENTATION_COST_DIVISOR: usize = 8;

/// The §19.5 lifetime-mismatch weight: the placement score penalizes mixing objects
/// of different lifetimes by `lifetime_gap × this`. `4` makes a severe mismatch
/// (short vs long, gap 2 ⇒ −8) able to overcome the packing bonus of a *non-dense*
/// hugepage (so it opens a fresh one and segregates), while a *dense* hugepage's
/// larger packing bonus still wins (preserving its coverage) — "avoid mixing ... when
/// possible".
const LIFETIME_MISMATCH_WEIGHT: i64 = 4;

// ===========================================================================
// §18.6 region cache for awkward sizes (W11-3)
// ===========================================================================

/// The §18.6 **region cache** for awkward sizes (W11-3): a small, fixed-capacity
/// index of recently-freed **whole-hugepage runs** keyed by their hugepage count, so
/// an allocation *slightly larger than a hugepage* reuses a rounded run instead of
/// re-reserving (and re-faulting) fresh hugepages each time. This **bounds the
/// rounding waste**: a stream of same-awkward-size allocations is served by reusing
/// freed runs rather than each independently rounding up to whole hugepages.
///
/// A cached run is **empty-backed** in the [`HugePageFiller`] (it has been freed via
/// [`free_hugepages`](HugePageFiller::free_hugepages), so coverage counts it as a
/// reserve, not live) — the cache only records its `(base, count)` for **O(1) warm
/// reuse** that skips the filler's bin scan. A hit re-reserves the run in place
/// ([`reserve_hugepages_at`](HugePageFiller::reserve_hugepages_at)); an entry whose
/// hugepages were meanwhile re-used by the scan path is detected as **stale** and
/// dropped (never double-vended).
struct RegionCache {
    /// Metadata-backed cache entries (`(base, hugepages)`; `hugepages == 0` ⇒ empty).
    entries: NonNull<RegionCacheEntry>,
    /// Cache capacity (entries).
    slots: u32,
    /// Live entry count (for stats/tests).
    live: u32,
}

/// One region-cache slot: the `(base, count)` of a freed whole-hugepage run recorded
/// for warm reuse (the hugepages themselves are empty-backed in the filler).
#[derive(Clone, Copy)]
#[repr(C)]
struct RegionCacheEntry {
    /// Base of the cached run (hugepage-aligned); meaningful when `hugepages != 0`.
    base: usize,
    /// Hugepage count of the cached run, or `0` if the slot is empty.
    hugepages: u32,
}

// SAFETY: `RegionCache` owns its entry array exclusively (a `NonNull` into
// never-freed metadata) and is only ever touched under the backend lock by
// `HugePageBackend`; it has no interior mutability of its own.
unsafe impl Send for RegionCache {}

impl RegionCache {
    /// Build a region cache of `slots` entries from `meta` (zeroed — every entry
    /// starts empty). `None` on a zero/oversized slot count or metadata exhaustion.
    fn new(meta: &dyn MetadataAlloc, slots: usize) -> Option<RegionCache> {
        if slots == 0 || slots > (NIL as usize) {
            return None;
        }
        let bytes = slots.checked_mul(core::mem::size_of::<RegionCacheEntry>())?;
        let mem = meta.alloc(bytes, core::mem::align_of::<RegionCacheEntry>())?;
        // SAFETY: fresh, exclusively-owned, aligned `bytes`-byte region; zeroing
        // yields `slots` valid empty `RegionCacheEntry`s (all-integer fields).
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        Some(RegionCache {
            entries: mem.cast::<RegionCacheEntry>(),
            slots: slots as u32,
            live: 0,
        })
    }

    #[inline]
    fn entry(&self, i: u32) -> RegionCacheEntry {
        debug_assert!(i < self.slots, "region-cache slot index out of range");
        // SAFETY: `i < slots` (every caller loops `i < self.slots`), the array is
        // initialized (zeroed at construction), and `Copy` makes the read non-mutating.
        unsafe { self.entries.as_ptr().add(i as usize).read() }
    }

    #[inline]
    fn set_entry(&mut self, i: u32, e: RegionCacheEntry) {
        debug_assert!(i < self.slots, "region-cache slot index out of range");
        // SAFETY: `i < slots` (as above) and `&mut self` grants exclusive access.
        unsafe { self.entries.as_ptr().add(i as usize).write(e) };
    }

    /// Take a cached run of exactly `hugepages` hugepages, **re-reserving** it in the
    /// `filler` (§18.6 warm reuse), or `None` if none is cached or every candidate is
    /// stale (re-used by the scan path since being cached). A stale entry is dropped
    /// when encountered, so the cache self-prunes and never double-vends.
    fn take(&mut self, hugepages: usize, filler: &mut HugePageFiller) -> Option<HugeRun> {
        let n = hugepages as u32;
        if n == 0 {
            return None;
        }
        let mut i = 0u32;
        while i < self.slots {
            let e = self.entry(i);
            if e.hugepages == n {
                // Remove the entry first (so a stale one is not retried).
                self.set_entry(
                    i,
                    RegionCacheEntry {
                        base: 0,
                        hugepages: 0,
                    },
                );
                self.live -= 1;
                if let Some(run) = filler.reserve_hugepages_at(e.base, hugepages) {
                    return Some(run); // warm hit
                }
                // Stale (the run was re-used by the scan path): keep scanning.
            }
            i += 1;
        }
        None
    }

    /// Record the freed run `(base, hugepages)` for warm reuse, or `false` if the
    /// cache is full (the run is still empty-backed in the filler and reusable via
    /// the scan path, so nothing is lost — the waste stays bounded).
    fn store(&mut self, base: usize, hugepages: usize) -> bool {
        let mut i = 0u32;
        while i < self.slots {
            if self.entry(i).hugepages == 0 {
                self.set_entry(
                    i,
                    RegionCacheEntry {
                        base,
                        hugepages: hugepages as u32,
                    },
                );
                self.live += 1;
                return true;
            }
            i += 1;
        }
        false
    }

    /// Forget every cached entry (the runs stay empty-backed in the filler; used at
    /// teardown, where the whole reservation is released anyway).
    fn clear(&mut self) {
        let mut i = 0u32;
        while i < self.slots {
            self.set_entry(
                i,
                RegionCacheEntry {
                    base: 0,
                    hugepages: 0,
                },
            );
            i += 1;
        }
        self.live = 0;
    }
}

/// Byte size of one region-cache slot, for metadata-arena sizing.
pub(crate) const REGION_CACHE_SLOT_BYTES: usize = core::mem::size_of::<RegionCacheEntry>();

// ===========================================================================
// The provider-driven hugepage backend (§27.2 lock, RegionCacheHook seam, W11)
// ===========================================================================

/// Sizing for a [`HugePageBackend`]: the hugepage region and the §18.6 region-cache
/// capacity. Grouped so construction stays a few clear arguments.
#[derive(Clone, Copy, Debug)]
pub struct HugeConfig {
    /// Number of hugepages to reserve (the region is `capacity_hugepages ×
    /// HUGEPAGE_SIZE` bytes, virtual on POSIX — lazily faulted, committed on demand).
    pub capacity_hugepages: usize,
    /// §18.6 region-cache capacity (awkward-size runs retained for reuse, W11-3).
    pub region_cache_slots: usize,
}

impl HugeConfig {
    /// A modest default (`capacity` hugepages, a small region cache).
    pub const fn with_capacity(capacity_hugepages: usize) -> HugeConfig {
        HugeConfig {
            capacity_hugepages,
            region_cache_slots: 16,
        }
    }

    /// The metadata bytes a [`HugePageBackend`] built from this config consumes up
    /// front (the filler's hugepage-descriptor pool plus the region cache),
    /// saturating rather than wrapping on absurd configs — for sizing the metadata
    /// arena (the [`AllocatorConfig::fixed_pool_metadata_bytes`](crate::AllocatorConfig::fixed_pool_metadata_bytes)
    /// discipline). Excludes the provider reservation, which is virtual address space.
    pub const fn metadata_bytes(&self) -> usize {
        self.capacity_hugepages
            .saturating_mul(HUGE_SLOT_BYTES)
            .saturating_add(
                self.region_cache_slots
                    .saturating_mul(REGION_CACHE_SLOT_BYTES),
            )
    }
}

/// The lock-guarded filler + region cache.
struct HugeInner {
    filler: HugePageFiller,
    cache: RegionCache,
}

/// The §19 **hugepage backend**: a [`HugePageFiller`] + a §18.6 `RegionCache` over a
/// provider-reserved, hugepage-aligned region, guarded by the §27.2 backend lock and
/// driving the [`TopoBackingProvider`] for every physical-state transition (`commit`
/// on placement, `decommit` on subrelease/release). It implements the §18.6
/// [`RegionCacheHook`] the large-allocation path already consults, so a large/medium
/// request is packed into hugepages (sub-hugepage) or served as a whole-hugepage run
/// (large/awkward) — **identically over POSIX or the seLe4n simulator** (D2; the
/// placement model is backend-agnostic, W11-6/§36.9).
///
/// Every operation is fallible and leaves the backend well-formed on failure (W4-5):
/// a `commit` failure rolls the filler placement back; a `decommit` failure on
/// subrelease recommits. POSIX is the degenerate single-authority case.
pub struct HugePageBackend<P: TopoBackingProvider> {
    provider: P,
    /// The whole hugepage-aligned reservation (released on `Drop`/`teardown`).
    region: Region,
    arena: ArenaId,
    lock: BackendLock,
    inner: UnsafeCell<HugeInner>,
    /// Whether the region has been returned to the provider (release exactly once).
    released: bool,
}

// SAFETY: every access to `inner` goes through `lock` (the §27.2 backend lock);
// `region`/`arena` are immutable after construction; the provider is `Sync`. So
// concurrent `&self` use is data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for HugePageBackend<P> {}
// SAFETY: the backend owns its `inner` (metadata-backed, never aliased) and a `Send`
// provider; moving it across threads moves both with no shared aliasing.
unsafe impl<P: TopoBackingProvider + Send> Send for HugePageBackend<P> {}

/// An RAII hold of the backend lock exposing the guarded [`HugeInner`] (releases on
/// drop, including on an unwinding `debug_assert!`).
struct HugeGuard<'a> {
    lock: &'a BackendLock,
    inner: &'a mut HugeInner,
}

impl Drop for HugeGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.release();
    }
}

impl<P: TopoBackingProvider> HugePageBackend<P> {
    /// Reserve a hugepage-aligned region of `cfg.capacity_hugepages` hugepages from
    /// `provider` for `arena` and build a hugepage backend over it (with metadata —
    /// the filler's descriptor pool and the region cache — from `meta`). The region
    /// begins fully reserved (uncommitted); backing is committed on demand. Returns
    /// the provider's error if the reservation fails, or [`HugeError::NoSpace`] if the
    /// metadata cannot be allocated (the reservation is rolled back, so nothing leaks).
    pub fn new(
        provider: P,
        meta: &dyn MetadataAlloc,
        arena: ArenaId,
        cfg: HugeConfig,
    ) -> Result<Self, HugeError> {
        if cfg.capacity_hugepages == 0 {
            return Err(HugeError::InvalidRequest);
        }
        let region_len = cfg
            .capacity_hugepages
            .checked_mul(HUGEPAGE_SIZE)
            .ok_or(HugeError::InvalidRequest)?;
        let region = provider
            .reserve(arena, region_len, HUGEPAGE_SIZE)
            .map_err(HugeError::Backend)?;
        let base = region.base as usize;
        let built = HugePageFiller::new(meta, base, cfg.capacity_hugepages).and_then(|filler| {
            RegionCache::new(meta, cfg.region_cache_slots).map(|cache| (filler, cache))
        });
        match built {
            Some((filler, cache)) => Ok(Self {
                provider,
                region,
                arena,
                lock: BackendLock::new(),
                inner: UnsafeCell::new(HugeInner { filler, cache }),
                released: false,
            }),
            None => {
                // Roll back the reservation so a failed construction leaks nothing.
                let _ = provider.release(arena, region);
                Err(HugeError::NoSpace)
            }
        }
    }

    /// Acquire the backend lock and expose the guarded filler + cache (RAII release).
    #[inline]
    fn lock(&self) -> HugeGuard<'_> {
        self.lock.acquire();
        HugeGuard {
            lock: &self.lock,
            // SAFETY: the lock is held, granting exclusive access to `inner`.
            inner: unsafe { &mut *self.inner.get() },
        }
    }

    /// The region offset of an absolute address within the reservation.
    #[inline]
    fn region_offset(&self, addr: usize) -> usize {
        addr - (self.region.base as usize)
    }

    /// A same-provenance [`Region`] for `[addr, addr + len)` within the reservation
    /// (so the handed-out pointer carries the backing's provenance, not a bare cast).
    #[inline]
    fn subregion(&self, addr: usize, len: usize) -> Region {
        Region {
            base: self.region.base.wrapping_add(self.region_offset(addr)),
            len,
        }
    }

    /// The provider's name (for diagnostics).
    #[inline]
    pub fn backend_name(&self) -> &'static str {
        self.provider.name()
    }

    /// Borrow the backing provider (e.g. to read a `HookProvider`'s hook-failure
    /// counts when the hugepage backend rides a custom backing).
    #[inline]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// The whole reserved [`Region`] this backend owns.
    #[inline]
    pub fn reserved_region(&self) -> Region {
        self.region
    }

    /// Whether `addr` lies within this backend's reserved region (used by the live
    /// [`NodeRouter`](crate::NodeRouter) to route a freed region back to its owner).
    /// Overflow-free: it uses the `addr - base < len` form, never the wrap-prone
    /// `base + len` (the private `region_contains` helper).
    #[inline]
    pub fn owns_addr(&self, addr: usize) -> bool {
        region_contains(self.region.base as usize, self.region.len, addr)
    }

    /// **Best-effort NUMA-bind this backend's whole region to OS node `os_node`** (§15.5,
    /// W13) — applied once, before the region is faulted, so future faults prefer
    /// `os_node` ([`TopoBackingProvider::bind_node`], Linux `mbind`). Returns `true` on
    /// success; a `false` is a recorded bind failure the caller surfaces in stats, never
    /// fatal (a missed bind only loses locality, §2.4).
    pub fn bind_region(&self, os_node: u32) -> bool {
        self.provider.bind_node(self.region, os_node).is_ok()
    }

    /// **Allocate `bytes` at `align` with placement `hints`** (the richer entry point;
    /// the [`RegionCacheHook`] seam uses [`PlaceHints::default`]). Packs a sub-hugepage
    /// request into a hugepage via the filler, or serves a whole-hugepage run
    /// (large/awkward) from the region cache or a fresh reservation. Commits the
    /// backing through the provider; on a commit failure the placement is rolled back
    /// and `None` returned (W4-5). `None` also if the request cannot be served (the
    /// caller falls through to the plain extent path).
    ///
    /// SPEC-transition: `huge_allocate` (§18.5 large path / §19 filler)
    pub fn allocate(&self, bytes: usize, align: usize, hints: PlaceHints) -> Option<Region> {
        // After `teardown` has returned the reservation to the provider, the filler's
        // address bookkeeping still describes the (now-unmapped) region. Refuse to
        // carve from it — otherwise a caller that reuses a torn-down backend would get
        // a non-null pointer into unmapped memory (on POSIX `commit` is a no-op, so the
        // fault would surface only on first access). `released` is written solely under
        // the exclusive `&mut self` of `teardown`/`Drop`, so this `&self` read is
        // race-free.
        if self.released {
            return None;
        }
        let pages = pages_for(bytes, PAGE_SIZE)?;
        if pages == 0 || !align.is_power_of_two() || align > HUGEPAGE_SIZE {
            return None;
        }
        if pages <= PAGES_PER_HUGEPAGE {
            self.allocate_packed(pages, align, hints)
        } else {
            self.allocate_run(pages)
        }
    }

    /// Sub-hugepage packing (`pages <= PAGES_PER_HUGEPAGE`): place via the filler and
    /// commit the run.
    fn allocate_packed(&self, pages: usize, align: usize, hints: PlaceHints) -> Option<Region> {
        let g = self.lock();
        let p = g.inner.filler.place(pages, align, hints)?;
        if let Some((offset, len)) = p.commit_run {
            if self.provider.commit(self.region, offset, len).is_err() {
                // Roll back the placement so the filler is unchanged (W4-5).
                g.inner.filler.free(p.base, p.pages);
                debug_assert!(g.inner.filler.check_invariants());
                return None;
            }
        }
        g.inner.filler.mark_committed(&p);
        debug_assert!(g.inner.filler.check_invariants());
        Some(self.subregion(p.base, pages * PAGE_SIZE))
    }

    /// Whole-hugepage run (`pages > PAGES_PER_HUGEPAGE`): reuse a warm cached run
    /// (§18.6) or reserve a fresh contiguous run, then commit. The returned region's
    /// length is the **page-rounded request** (`pages × PAGE_SIZE`) — not the
    /// hugepage-rounded reservation — so the engine's byte accounting stays exact;
    /// the rounding slack is the bounded waste the region cache amortizes (W11-3).
    fn allocate_run(&self, pages: usize) -> Option<Region> {
        let n = pages.div_ceil(PAGES_PER_HUGEPAGE);
        let g = self.lock();
        let inner = &mut *g.inner;
        // §18.6 warm reuse re-reserves an empty-backed cached run in the filler; a
        // miss reserves fresh (scan-reuse or extend the region).
        let run = match inner.cache.take(n, &mut inner.filler) {
            Some(run) => run,
            None => inner.filler.reserve_hugepages(n)?,
        };
        if let Some((offset, len)) = run.commit_run {
            if self.provider.commit(self.region, offset, len).is_err() {
                inner.filler.free_hugepages(run.base, n);
                debug_assert!(inner.filler.check_invariants());
                return None;
            }
        }
        inner.filler.mark_run_committed(&run);
        debug_assert!(inner.filler.check_invariants());
        Some(self.subregion(run.base, pages * PAGE_SIZE))
    }

    /// **Free a `region`** this backend handed out (the inverse of [`allocate`](Self::allocate)):
    /// a sub-hugepage region returns its pages to the filler (kept backed — HugeCache),
    /// a whole-hugepage run is offered to the §18.6 region cache (kept reserved for
    /// reuse) or returned to the filler if the cache is full. Returns `false` for a
    /// foreign region (not from this backend) or a double free — never acting on a
    /// non-owned region.
    ///
    /// SPEC-transition: `huge free` (§19 filler return)
    pub fn free_region(&self, region: Region) -> bool {
        // A torn-down backend is inert: the reservation is gone, so there is nothing to
        // free (and nothing should mutate the abandoned bookkeeping). Mirrors the
        // `allocate`/`subrelease` guards; `released` is written only under the exclusive
        // `&mut self` of `teardown`/`Drop`, so this `&self` read is race-free.
        if self.released {
            return false;
        }
        let base = region.base as usize;
        let (rlo, rhi) = self.region.addr_range();
        if base < rlo || base >= rhi || region.len == 0 {
            return false; // not ours
        }
        let pages = region.len / PAGE_SIZE;
        let g = self.lock();
        let inner = &mut *g.inner;
        let ok = if pages <= PAGES_PER_HUGEPAGE {
            inner.filler.free(base, pages).valid
        } else {
            // Free the run to empty-backed (accurate coverage), then record it in the
            // §18.6 region cache for warm reuse (best-effort: a full cache just means
            // the next same-size request reuses it via the filler's scan instead).
            let n = pages.div_ceil(PAGES_PER_HUGEPAGE);
            let fr = inner.filler.free_hugepages(base, n);
            if fr.valid {
                inner.cache.store(base, n);
            }
            fr.valid
        };
        debug_assert!(inner.filler.check_invariants());
        ok
    }

    /// **In-place tail trim** of a cache-served allocation (§25.3 / W15-3b
    /// cache-served shrink): shrink the live sub-hugepage allocation described by
    /// `region` (its base + current usable length) to `new_len` page-rounded bytes,
    /// freeing its tail pages back to the filler. Returns `Some(freed_bytes)` on
    /// success, or `None` (no change) if `region` is not ours, the request is not a
    /// strict page-rounded shrink, the run is not a single live allocation, or it is
    /// a **multi-hugepage run** (whose tail-hugepage release is the W12 controller's
    /// job, not an in-place realloc trim). The freed tail stays committed (reusable
    /// by the filler), as the extent shrink leaves its tail Dirty under the retain
    /// policy; W12 subreleases cold pages later.
    pub fn trim_region(&self, region: Region, new_len: usize) -> Option<usize> {
        if self.released {
            return None;
        }
        let base = region.base as usize;
        let (rlo, rhi) = self.region.addr_range();
        if base < rlo || base >= rhi || region.len == 0 {
            return None; // not ours
        }
        let old_pages = region.len / PAGE_SIZE;
        let new_pages = pages_for(new_len.max(1), PAGE_SIZE)?;
        // Only a strict shrink within a single hugepage trims in place.
        if new_pages >= old_pages || old_pages > PAGES_PER_HUGEPAGE {
            return None;
        }
        let g = self.lock();
        if g.inner.filler.trim(base, old_pages, new_pages) {
            debug_assert!(g.inner.filler.check_invariants());
            Some((old_pages - new_pages) * PAGE_SIZE)
        } else {
            None
        }
    }

    /// **Partial subrelease** of the `pages`-page run at `base` (§19.6, W11-4b): the
    /// filler decides under the §19.6 guards (H-005 etc.), and on success this
    /// **revokes the run's descendant capabilities then `decommit`s it** through the
    /// provider, dropping RSS. `pressure_high` is the release controller's pressure
    /// signal (plan 04 W12). Returns the bytes subreleased (`0` if a guard refused, a
    /// revoke failed, or the decommit failed and was rolled back).
    ///
    /// **§36.6 revoke-before-recycle:** decommit returns the run's physical frames to
    /// a provider pool that could re-type them for another authority domain, so the
    /// run's derived frame/mapping capabilities are revoked **first** (a no-op on
    /// POSIX — single ambient authority — and real capability revocation on the
    /// seLe4n provider, plan 09; mirrors [`ExtentManager::release`](crate::ExtentManager::release)).
    /// A revoke failure leaves the run committed and the filler well-formed (W4-5).
    ///
    /// SPEC-transition: partial subrelease + revoke + decommit (§19.6/§20.4/§36.6)
    pub fn subrelease(&self, base: usize, pages: usize, pressure_high: bool) -> usize {
        // After teardown the reservation has been `munmap`'d and the address range may
        // have been re-mapped by the OS for something else; a `decommit`
        // (`MADV_DONTNEED`) over it could then discard *unrelated* memory. Refuse once
        // released (also makes `release_empty_excess`/`release_tick`, which drive this,
        // inert after teardown). `released` is written only under `teardown`/`Drop`'s
        // exclusive `&mut self`, so this `&self` read is race-free.
        if self.released {
            return 0;
        }
        let g = self.lock();
        let sr = match g.inner.filler.subrelease(base, pages, pressure_high) {
            Some(sr) => sr,
            None => return 0,
        };
        let sub = self.subregion(self.region.base as usize + sr.region_offset, sr.len);
        // §36.6: revoke derived capabilities before the frames return to the pool.
        if self.provider.revoke_descendants(self.arena, sub).is_err() {
            // Revoke failed: undo the subrelease (recommit) and refuse.
            g.inner.filler.unsubrelease(&sr);
            debug_assert!(g.inner.filler.check_invariants());
            return 0;
        }
        if self
            .provider
            .decommit(self.region, sr.region_offset, sr.len)
            .is_err()
        {
            // Decommit failed: recommit (undo the subrelease) so we stay well-formed.
            g.inner.filler.unsubrelease(&sr);
            debug_assert!(g.inner.filler.check_invariants());
            return 0;
        }
        debug_assert!(g.inner.filler.check_invariants());
        sr.len
    }

    /// **Demand-reserve shrink (W11-1b's "demand-reserve hook (W12)").** Return the
    /// backing of empty-backed hugepages held beyond `reserve` to the OS, keeping at
    /// most `reserve` empty-backed hugepages cached for fast burst reuse (the
    /// HugeCache) and reclaiming the rest of the RSS. Returns the bytes released.
    ///
    /// Each released hugepage is [`subrelease`](Self::subrelease)d over exactly its
    /// **committed** pages (revoke + decommit), so it leaves the `EmptyBacked` bin (its
    /// virtual range is retained for recommit-on-reuse, §20.5). An empty hugepage that
    /// only ever held a *packed sub-hugepage* allocation is only **partially**
    /// committed, so releasing all `PAGES_PER_HUGEPAGE` would hit the §19.6
    /// "whole-run-committed" guard and reclaim nothing — instead we release precisely
    /// its committed prefix (its true RSS). The walk is by ascending slot index (not
    /// the bin head), so one un-releasable hugepage never blocks reclaiming the others.
    /// The **policy** — what `reserve` to choose and when to call this — belongs to the
    /// release controller (plan 04 W12); this is the **mechanism** it drives. Bounds the
    /// HugeCache so a churny workload's empty hugepages do not pin RSS indefinitely.
    pub fn release_empty_excess(&self, reserve: usize) -> usize {
        let mut released = 0usize;
        let mut kept = 0usize;
        let mut from = 0u32;
        loop {
            // Find the next empty-backed hugepage (and its committed footprint) under
            // the lock; release it outside (re-acquiring). Advancing `from` past each
            // visited hugepage guarantees the walk terminates and visits each once.
            let next = {
                let g = self.lock();
                g.inner.filler.next_empty_backed(from)
            };
            let (idx, base, committed) = match next {
                Some(t) => t,
                None => break,
            };
            from = idx + 1;
            // Keep the first `reserve` empty hugepages backed for fast burst reuse.
            if kept < reserve {
                kept += 1;
                continue;
            }
            debug_assert!(
                committed > 0,
                "next_empty_backed only yields backed empties"
            );
            let _ = committed;
            // Release **every maximal committed run** of this hugepage (its RSS).
            // `subrelease` takes a contiguous run, and a committed set is not in
            // general the prefix `[0, popcount)` — an over-aligned placement leaves
            // stride holes and a subrelease-then-partial-refill leaves a released hole
            // — so passing the popcount as a run length made `subrelease` refuse and
            // reclaim nothing. Walking the runs reclaims exactly the backed pages,
            // whatever their layout. A guard or revoke refusal returns 0; skip that run
            // and keep reclaiming the others (a concurrent change just makes the
            // subrelease a re-validated no-op). `search_from` advances past each run
            // examined, so the walk is O(PAGES_PER_HUGEPAGE) and always terminates.
            let mut search_from = 0usize;
            loop {
                let run = {
                    let g = self.lock();
                    g.inner.filler.next_committed_run(idx, search_from)
                };
                let Some((off, pages)) = run else { break };
                search_from = off + pages;
                match self.subrelease(base + off * PAGE_SIZE, pages, true) {
                    0 => continue,
                    bytes => released += bytes,
                }
            }
        }
        released
    }

    /// **Drive one release-controller tick against this backend (plan 04 W12 live
    /// wiring).** Samples this backend's §19.7 coverage into the §21.2 `inputs`
    /// (the empty-backed-hugepage supply and the coverage ratio — the rest the host
    /// fills), ticks `controller` to get the §21.3 [`ReleasePlan`], then **executes the
    /// rung-2 release live**: it returns empty-backed hugepages beyond the §21.4 demand
    /// reserve to the OS via [`release_empty_excess`](Self::release_empty_excess) — the
    /// exact W11-1b "demand-reserve hook (W12)" handoff. Honors the plan's (possibly
    /// rate-capped, possibly Emergency-full) rung-2 byte budget by choosing the reserve
    /// that releases precisely that many whole hugepages.
    ///
    /// Returns the plan (so the host can execute the remaining extent/cache rungs via
    /// the existing §20.4 ops) and the bytes actually released here. The controller is
    /// the host's; this method holds no controller state — the policy stays pure
    /// ([`crate::release`]), this is only the mechanism it drives.
    pub fn release_tick(
        &self,
        controller: &mut ReleaseController,
        now_ms: u64,
        mut inputs: ReleaseInputs,
    ) -> (ReleasePlan, u64) {
        let cov = self.coverage();
        inputs.empty_backed_hugepage_bytes = cov.empty_backed_bytes;
        inputs.hugepage_coverage_ratio_bp = cov.coverage_ratio_bp();
        let plan = controller.tick(now_ms, inputs);
        // Execute rung 2: keep exactly `empty − planned` empty hugepages, release the
        // rest. `release_empty_excess` takes a hugepage *count* to retain.
        let released = if plan.release_empty_hugepages_bytes > 0 {
            let hp = HUGEPAGE_SIZE as u64;
            // The actual empty-backed hugepage *count*, not
            // `empty_backed_bytes / HUGEPAGE_SIZE` — a packed-then-emptied hugepage is
            // only partially committed, so the byte form would undercount it. It counts
            // exactly the population `release_empty_excess` walks (empty **and** still
            // holding committed pages), not the §19.4 `EmptyBacked` bin: a partially
            // subreleased empty hugepage is filed `PartialSubreleased` yet contributes
            // its committed bytes to `empty_backed_bytes` above, so using the bin count
            // here would draw the reserve and the supply from different populations.
            let empty_hp = self.empty_backed_hugepages();
            // Round the rung-2 byte budget UP to whole hugepages: rung 2 releases in
            // whole-hugepage units, so a positive *sub-hugepage* budget still makes
            // progress (releasing one hugepage) instead of flooring to zero and planning
            // release work forever while reclaiming nothing — the livelock. The rate
            // cap's remainder is carried by the controller's §20.3 backlog; the
            // per-tick overshoot is bounded by one hugepage.
            let release_hp = plan.release_empty_hugepages_bytes.div_ceil(hp) as usize;
            let reserve_hp = empty_hp.saturating_sub(release_hp);
            self.release_empty_excess(reserve_hp) as u64
        } else {
            0
        };
        (plan, released)
    }

    /// The §19.7 coverage metrics (W11-5) for this backend's hugepages.
    pub fn coverage(&self) -> HugeStats {
        self.lock().inner.filler.coverage()
    }

    /// This backend's §20.1 physical-state byte breakdown over its whole reservation, so a
    /// cache-served allocation is accounted in the §8.6 stats identities exactly like an
    /// extent-served one (the `RegionCacheHook::state_bytes` contract).
    ///
    /// The mapping from the §19.7 coverage view:
    /// * **active** — live (handed-out) bytes;
    /// * **dirty** — backed-but-free: the fragmentation bytes on in-use hugepages plus the
    ///   committed bytes of fully-empty ones (both hold real RSS and are cheap to reuse);
    /// * **released** — decommitted: the released bytes of empty hugepages plus the
    ///   partially-subreleased bytes of in-use ones;
    /// * **reserved** — the rest of the reservation (never-committed virtual), computed as
    ///   the remainder so `total()` is exactly the reservation length and the §8.6
    ///   `virtual == active + pageheap_free` identity closes by construction.
    ///
    /// Muzzy is always `0`: the filler decommits directly (there is no lazy-purge state).
    pub fn state_bytes(&self) -> StateBytes {
        let cov = self.coverage();
        let region_len = self.region.len;
        let active = cov.live_total_bytes as usize;
        let dirty = cov.fragmentation_bytes as usize + cov.empty_backed_bytes as usize;
        let released = cov.empty_released_bytes as usize + cov.partial_subreleased_bytes as usize;
        StateBytes {
            reserved: region_len.saturating_sub(active + dirty + released),
            active,
            dirty,
            muzzy: 0,
            released,
        }
    }

    /// The number of **empty-backed** hugepages — empty (`used == 0`) and still
    /// holding committed backing. This is exactly the population
    /// [`release_empty_excess`](Self::release_empty_excess) walks and whose committed
    /// bytes [`coverage`](Self::coverage) reports as `empty_backed_bytes`, so the
    /// demand reserve and the release supply agree (W11-1b / W12 rung 2).
    pub fn empty_backed_hugepages(&self) -> usize {
        self.lock().inner.filler.empty_backed_hugepages()
    }

    /// Cumulative subrelease events (§19.6 metric).
    pub fn subrelease_events(&self) -> u64 {
        self.lock().inner.filler.subrelease_events()
    }

    /// Mark the hugepage covering `addr` cold (§19.6) — the W12 release-controller
    /// idle-decay hook; see [`HugePageFiller::mark_cold`].
    pub fn mark_cold(&self, addr: usize) -> bool {
        self.lock().inner.filler.mark_cold(addr)
    }

    /// Touched hugepages so far (opened into a bin).
    pub fn touched_hugepages(&self) -> usize {
        self.lock().inner.filler.touched()
    }

    /// Whether the backend's filler is well-formed (the §19.8 B.4 oracle).
    pub fn check_invariants(&self) -> bool {
        self.lock().inner.filler.check_invariants()
    }

    /// **Explicitly** return the reservation to the provider, surfacing the result
    /// (vs the discard `Drop` must do). Idempotent with `Drop`. Drains the region
    /// cache first so cached reservations are not double-released.
    pub fn teardown(&mut self) -> Result<(), BackendError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        // The cache only indexes empty-backed sub-runs of the one reservation;
        // releasing the whole region reclaims them, so we just forget the index.
        {
            let g = self.lock();
            g.inner.cache.clear();
        }
        self.provider.release(self.arena, self.region)
    }
}

impl<P: TopoBackingProvider> RegionCacheHook for HugePageBackend<P> {
    #[inline]
    fn state_bytes(&self) -> StateBytes {
        HugePageBackend::state_bytes(self)
    }

    #[inline]
    fn try_alloc(&self, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        // Honor the request's §10.4 hugepage preference: a `NO_HUGEPAGE`
        // ([`HugepagePolicy::Avoid`]) request must NOT be served from the hugepage
        // backend even under the `hugepage_optimized` profile — decline so the large
        // path falls through to the plain extent manager (the user-visible avoid flag
        // is respected per allocation). `Default`/`Prefer` both use the backend.
        if matches!(hints.hugepage, HugepagePolicy::Avoid) {
            return None;
        }
        // Translate the request's advisory hints (§10.4) into the filler's placement
        // hints (§19.3/§19.5): the `0..=255` hotness hint maps to cold/neutral/hot,
        // the lifetime passes through. So the engine's large path packs by hotness and
        // lifetime through the seam (W11 B5).
        self.allocate(
            bytes,
            align,
            PlaceHints {
                hotness: Hotness::from_hint(hints.hotness),
                lifetime: hints.lifetime,
            },
        )
    }

    #[inline]
    fn try_cache(&self, region: Region) -> bool {
        self.free_region(region)
    }

    fn try_cache_revoking(&self, region: Region, arena: ArenaId) -> bool {
        // §36.6/§36.13 revoke-before-recycle: an arena-drain free of a cache-served
        // large must revoke the draining arena's descendant capabilities to these
        // frames BEFORE they re-enter the HugeCache for reuse by another authority
        // domain — else a capability-backed arena destroy/reset would leak access to
        // recycled pages. A revoke failure refuses the cache (returns `false`); the
        // region stays owned (the §36.13 partial-failure signal). On POSIX (single
        // ambient authority) `revoke_descendants` is a no-op, so this is exactly
        // `try_cache`. Mirrors [`subrelease`](Self::subrelease)'s revoke-before-decommit.
        if self.provider.revoke_descendants(arena, region).is_err() {
            return false;
        }
        self.free_region(region)
    }

    #[inline]
    fn try_trim(&self, region: Region, new_len: usize) -> Option<usize> {
        // W15-3b cache-served shrink: trim the allocation's tail pages back to the
        // filler in place (sub-hugepage only; a run keeps whole).
        self.trim_region(region, new_len)
    }
}

impl<P: TopoBackingProvider> Drop for HugePageBackend<P> {
    fn drop(&mut self) {
        // Return the whole reservation unless `teardown` already did (so it is
        // released exactly once). A failure here cannot be reported from `drop`;
        // providers leave their state well-formed (§36.6) and the metadata-backed
        // pools simply go away with the arena. Callers that must observe a release
        // failure use `teardown`.
        if !self.released {
            let _ = self.provider.release(self.arena, self.region);
        }
    }
}

#[cfg(test)]
mod filler_tests {
    use super::*;
    use crate::bootstrap::BumpArena;

    /// A leaked heap metadata arena valid for the process (the extent/span test
    /// pattern), so vended slot pools outlive every filler built over them.
    fn meta(bytes: usize) -> &'static BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
        Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
    }

    /// A filler over `cap` synthetic hugepages based at a hugepage-aligned address
    /// (pure address bookkeeping — no real bytes, like the `ExtentMap` tests).
    fn filler(cap: usize) -> HugePageFiller {
        HugePageFiller::new(meta(1 << 20), 0x4000_0000_0000, cap).expect("filler")
    }

    /// Place + immediately confirm the commit (the backend's success path), so the
    /// filler is settled (`live ⊆ committed`). Returns the placement.
    fn place_ok(f: &mut HugePageFiller, pages: usize, align: usize, hot: Hotness) -> Placement {
        let p = f
            .place(pages, align, PlaceHints::hot(hot))
            .expect("placement");
        f.mark_committed(&p);
        assert!(f.check_invariants());
        p
    }

    /// W15-3b D: `trim` frees an allocation's tail pages back to the filler while the
    /// kept prefix stays a valid single allocation; it rejects non-strict shrinks and
    /// non-allocation-start bases (the same exact-extent guard as `free`, S-007).
    #[test]
    fn trim_frees_the_tail_and_keeps_the_prefix_a_valid_allocation() {
        let mut f = filler(1);
        let p = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Neutral);
        let base = p.base;
        assert_eq!(p.pages, 4);

        // Trim 4 → 2: the tail [2,4) returns to the filler; the prefix [0,2) stays live.
        assert!(
            f.trim(base, 4, 2),
            "trim a live 4-page allocation to 2 pages"
        );
        assert!(f.check_invariants());
        // The trimmed prefix is still one valid allocation — `free(base, 2)` validates
        // exactly (head at base, exact 2-page extent, boundary after).
        assert!(
            f.free(base, 2).valid,
            "the trimmed prefix frees cleanly as a 2-page run"
        );
        assert!(f.check_invariants());

        // Rejections (no change), mirroring `free`'s exact-extent guard.
        let q = place_ok(&mut f, 3, PAGE_SIZE, Hotness::Neutral);
        assert!(!f.trim(q.base, 3, 3), "new == old is not a shrink");
        assert!(!f.trim(q.base, 3, 0), "new == 0 is rejected");
        assert!(
            !f.trim(q.base + PAGE_SIZE, 2, 1),
            "an interior page is not an allocation start"
        );
        assert!(!f.trim(q.base, 5, 2), "old beyond the live run is rejected");
        // The intact allocation still frees normally.
        assert!(f.free(q.base, 3).valid);
        assert!(f.check_invariants());
    }

    #[test]
    fn region_contains_is_overflow_safe() {
        // Ordinary half-open `[base, base + len)` membership.
        assert!(region_contains(1000, 100, 1000), "inclusive start");
        assert!(region_contains(1000, 100, 1099), "last byte");
        assert!(!region_contains(1000, 100, 1100), "exclusive end");
        assert!(!region_contains(1000, 100, 999), "before the start");
        assert!(
            !region_contains(1000, 0, 1000),
            "an empty region contains nothing"
        );

        // Near the top of the address space, where `base + len` **wraps**: membership must
        // still be correct. The old `addr < base + len` form computed `base + len`, which
        // overflowed to a small value and answered wrongly; the `addr - base < len` form
        // never forms that sum.
        let base = usize::MAX - 50;
        let len = 100; // base + len = usize::MAX + 50 ⇒ would wrap to 49
        assert_eq!(base.wrapping_add(len), 49, "the sum genuinely wraps here");
        assert!(region_contains(base, len, base), "start, even near the top");
        assert!(
            region_contains(base, len, usize::MAX),
            "50 bytes in — the wrapping `base + len` form answered this wrong"
        );
        assert!(
            !region_contains(base, len, base - 1),
            "just before the start"
        );
        // A low (wrapped-around) address is NOT in a real, non-wrapping reservation.
        assert!(
            !region_contains(base, len, 10),
            "wrapped low address is not a member"
        );
        assert!(
            !region_contains(base, len, 48),
            "nor is the wrapped end region"
        );
    }

    #[test]
    fn classify_bin_is_total_and_covers_every_bin() {
        // Every (used,total,subreleased,hotness) maps to exactly one bin; spot-check
        // each of the nine is reachable (the H-003 classification).
        let n = PAGES_PER_HUGEPAGE;
        assert_eq!(
            classify_bin(0, n, 0, Hotness::Neutral),
            HugeBin::EmptyBacked
        );
        assert_eq!(
            classify_bin(1, n, 0, Hotness::Neutral),
            HugeBin::NearlyEmpty
        );
        assert_eq!(classify_bin(n / 4, n, 0, Hotness::Neutral), HugeBin::Sparse);
        assert_eq!(
            classify_bin(n / 4, n, 0, Hotness::Cold),
            HugeBin::ColdSparse
        );
        assert_eq!(classify_bin(n / 2, n, 0, Hotness::Neutral), HugeBin::Medium);
        assert_eq!(
            classify_bin(7 * n / 8, n, 0, Hotness::Neutral),
            HugeBin::NearlyFull
        );
        assert_eq!(
            classify_bin(7 * n / 8, n, 0, Hotness::Hot),
            HugeBin::HotDense
        );
        assert_eq!(classify_bin(n, n, 0, Hotness::Neutral), HugeBin::Full);
        assert_eq!(classify_bin(n, n, 0, Hotness::Hot), HugeBin::HotDense);
        assert_eq!(
            classify_bin(1, n, 1, Hotness::Neutral),
            HugeBin::PartialSubreleased
        );
    }

    #[test]
    fn huge_bin_classification_matches_lean() {
        // Differential gate (W11): `classify_bin` must agree with the Lean
        // `TopoMalloc.Huge.HugeBin.classifyBin` on the *identical* representative
        // inputs the `lake exe check` `hugeBinGate` checks (one per bin, total = 128 =
        // PAGES_PER_HUGEPAGE). If either side's §19.4 classification drifts, one of
        // these diverges and CI fails (the §19.4 analogue of the §20.1
        // `extent_state_transition_matches_lean`).
        let n = PAGES_PER_HUGEPAGE; // 128, matching the Lean gate's `total`
        assert_eq!(n, 128, "the Lean hugeBinGate pins total = 128");
        let cases: &[(usize, usize, Hotness, HugeBin)] = &[
            (0, 0, Hotness::Neutral, HugeBin::EmptyBacked),
            (1, 0, Hotness::Neutral, HugeBin::NearlyEmpty),
            (32, 0, Hotness::Neutral, HugeBin::Sparse),
            (32, 0, Hotness::Cold, HugeBin::ColdSparse),
            (64, 0, Hotness::Neutral, HugeBin::Medium),
            (112, 0, Hotness::Neutral, HugeBin::NearlyFull),
            (112, 0, Hotness::Hot, HugeBin::HotDense),
            (128, 0, Hotness::Neutral, HugeBin::Full),
            (128, 0, Hotness::Hot, HugeBin::HotDense),
            (5, 3, Hotness::Neutral, HugeBin::PartialSubreleased),
        ];
        for &(used, sub, hot, want) in cases {
            assert_eq!(
                classify_bin(used, n, sub, hot),
                want,
                "classify_bin({used}, {n}, {sub}, {hot:?}) drifted from the Lean model"
            );
        }
    }

    #[test]
    fn placement_carves_distinct_aligned_runs() {
        let mut f = filler(4);
        // Two 4-page placements in the same hugepage are disjoint and page-aligned.
        let a = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Neutral);
        let b = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Neutral);
        assert_ne!(a.base, b.base);
        assert_eq!(a.base % PAGE_SIZE, 0);
        assert_eq!(b.base % PAGE_SIZE, 0);
        // They land in the SAME hugepage (packing: fill before opening new).
        assert_eq!(a.hugepage, b.hugepage, "packing fills one hugepage first");
        assert_eq!(f.touched(), 1);
        assert_eq!(f.used_pages_of(a.base), 8);
    }

    #[test]
    fn first_placement_needs_commit_then_reuse_is_a_cache_hit() {
        // W11-1b: a fresh hugepage's pages need committing; after free+reuse the same
        // pages are still backed, so the reuse is a HugeCache hit (no commit_run).
        let mut f = filler(2);
        let p = f
            .place(2, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("place");
        assert!(p.commit_run.is_some(), "fresh pages need a commit");
        f.mark_committed(&p);
        assert!(f.free(p.base, 2).valid);
        let q = f
            .place(2, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("reuse");
        assert_eq!(q.base, p.base, "empty-backed reuse hits the same pages");
        assert!(
            q.commit_run.is_none(),
            "backed reuse avoids an immediate fault"
        );
        f.mark_committed(&q);
        assert!(f.check_invariants());
    }

    #[test]
    fn free_empties_hugepage_and_reports_it() {
        let mut f = filler(2);
        let a = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Neutral);
        let b = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Neutral);
        let ra = f.free(a.base, 8);
        assert!(ra.valid && !ra.now_empty, "freed, but 8 live pages remain");
        let rb = f.free(b.base, 8);
        assert!(rb.valid && rb.now_empty, "now empty");
        assert_eq!(f.used_pages_of(a.base), 0);
        assert_eq!(f.bin_of(a.base), Some(HugeBin::EmptyBacked));
        assert!(f.check_invariants());
    }

    #[test]
    fn bins_track_occupancy_h003() {
        // H-003: as a hugepage fills, its bin follows occupancy.
        let mut f = filler(1);
        let n = PAGES_PER_HUGEPAGE;
        let p1 = place_ok(&mut f, 1, PAGE_SIZE, Hotness::Neutral);
        assert_eq!(f.bin_of(p1.base), Some(HugeBin::NearlyEmpty));
        let _p2 = place_ok(&mut f, n / 2, PAGE_SIZE, Hotness::Neutral);
        // used = 1 + n/2 ⇒ just over half ⇒ Medium or NearlyFull depending on rounding.
        assert!(matches!(
            f.bin_of(p1.base),
            Some(HugeBin::Medium) | Some(HugeBin::NearlyFull)
        ));
        // Fill the rest ⇒ Full.
        let used = f.used_pages_of(p1.base);
        let _p3 = place_ok(&mut f, n - used, PAGE_SIZE, Hotness::Neutral);
        assert_eq!(f.bin_of(p1.base), Some(HugeBin::Full));
        assert!(f.check_invariants());
    }

    #[test]
    fn subrelease_refuses_a_run_intersecting_a_live_object_h005() {
        // H-005 (the headline guard): a subrelease overlapping a live object is
        // refused, no state changed.
        let mut f = filler(1);
        let live = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Cold);
        // Attempt to subrelease the very pages that are live.
        assert!(
            f.subrelease(live.base, 4, /*pressure*/ true).is_none(),
            "subrelease must refuse a run intersecting a live object (H-005)"
        );
        // And a run partially overlapping the live object (1 of its pages).
        assert!(f.subrelease(live.base, 1, true).is_none());
        assert_eq!(f.used_pages_of(live.base), 4, "live object untouched");
        assert!(f.check_invariants());
    }

    #[test]
    fn subrelease_returns_a_cold_free_run_and_recommit_restores() {
        let mut f = filler(1);
        // Fill a hugepage, then free a run so it is cold-free-backed, then subrelease.
        let a = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Cold);
        let b = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Cold);
        assert!(f.free(b.base, 4).valid);
        // `b`'s pages are now free + backed; the hugepage is cold ⇒ subrelease allowed.
        let sr = f
            .subrelease(b.base, 4, /*pressure*/ false)
            .expect("cold subrelease");
        assert_eq!(sr.len, 4 * PAGE_SIZE);
        assert_eq!(f.bin_of(a.base), Some(HugeBin::PartialSubreleased));
        let cov = f.coverage();
        assert_eq!(cov.partial_subreleased_bytes, 4 * PAGE_SIZE as u64);
        assert_eq!(f.subrelease_events(), 1);
        // Recommitting (e.g. on decommit failure or reuse) restores the pages.
        f.unsubrelease(&sr);
        assert_ne!(f.bin_of(a.base), Some(HugeBin::PartialSubreleased));
        assert!(f.check_invariants());
    }

    #[test]
    fn subrelease_refuses_when_not_cold_and_no_pressure() {
        // §19.6 coldness/pressure gate: a hot, dense hugepage's free pages are not
        // subreleased without pressure (preserve coverage).
        let mut f = filler(1);
        let n = PAGES_PER_HUGEPAGE;
        let _hot = place_ok(&mut f, n - 4, PAGE_SIZE, Hotness::Hot); // hot + nearly full
        let tail = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Hot);
        assert!(f.free(tail.base, 4).valid);
        // Hot + dense, no pressure ⇒ refuse.
        assert!(f.subrelease(tail.base, 4, /*pressure*/ false).is_none());
        // Under pressure ⇒ allowed.
        assert!(f.subrelease(tail.base, 4, /*pressure*/ true).is_some());
        assert!(f.check_invariants());
    }

    #[test]
    fn placement_groups_by_lifetime_overriding_the_base_tiebreak() {
        // §19.5 lifetime grouping: with two equally-occupied candidate hugepages —
        // a Long-lived one at the *lower* base and a Short-lived one at the higher
        // base — a new Short request prefers the Short hugepage, even though the
        // deterministic base tie-break alone would pick the lower-base (Long) one.
        // So the lifetime-match score genuinely steers placement.
        let short = PlaceHints {
            hotness: Hotness::Neutral,
            lifetime: Lifetime::Short,
        };
        let long = PlaceHints {
            hotness: Hotness::Neutral,
            lifetime: Lifetime::Long,
        };
        let mut f = filler(4);
        // Build two equally-occupied candidates — hugepage 0 (Long) and hugepage 1
        // (Short), each with one 8-page resident at the END of the hugepage — *without*
        // a partial free (which exact-extent validation now forbids). Fill the hugepage
        // with a separate filler allocation, place the resident in the last 8 pages,
        // then free the filler *exactly*, leaving the resident of the intended lifetime
        // at a high offset (so the next placement lands at offset 0, as the policy
        // arithmetic this test pins expects).
        let fill0 = f.place(PAGES_PER_HUGEPAGE - 8, PAGE_SIZE, long).unwrap(); // opens hp0
        f.mark_committed(&fill0);
        let a = f.place(8, PAGE_SIZE, long).unwrap(); // hp0: 8 Long, at [N-8, N)
        f.mark_committed(&a);
        let fill1 = f.place(PAGES_PER_HUGEPAGE - 8, PAGE_SIZE, short).unwrap(); // hp0 full ⇒ hp1
        f.mark_committed(&fill1);
        let b = f.place(8, PAGE_SIZE, short).unwrap(); // hp1: 8 Short, at [N-8, N)
        f.mark_committed(&b);
        assert_eq!(a.hugepage, 0);
        assert_eq!(b.hugepage, 1);
        assert!(f.free(fill0.base, PAGES_PER_HUGEPAGE - 8).valid); // hp0: 8 live, Long
        assert!(f.free(fill1.base, PAGES_PER_HUGEPAGE - 8).valid); // hp1: 8 live, Short
        assert_eq!(f.bin_of(a.base), Some(HugeBin::NearlyEmpty));
        assert_eq!(f.bin_of(b.base), Some(HugeBin::NearlyEmpty));
        // A new Short request: lifetime match (hugepage 1) must beat the lower-base
        // tie-break (hugepage 0, Long).
        let p = f.place(8, PAGE_SIZE, short).expect("placement");
        assert_eq!(
            p.hugepage, 1,
            "the Short request grouped with the Short hugepage, not the lower-base Long one"
        );
        f.mark_committed(&p);
        assert!(f.check_invariants());
    }

    #[test]
    fn placement_opens_fresh_to_avoid_mixing_lifetimes() {
        // §19.5 "avoid mixing very short-lived and long-lived objects when possible":
        // a short-lived request prefers a fresh hugepage over mixing into a non-dense
        // long-lived one, while a same-lifetime request still packs together.
        let long = PlaceHints {
            hotness: Hotness::Neutral,
            lifetime: Lifetime::Long,
        };
        let short = PlaceHints {
            hotness: Hotness::Neutral,
            lifetime: Lifetime::Short,
        };
        let mut f = filler(4);
        let a = f.place(1, PAGE_SIZE, long).unwrap();
        f.mark_committed(&a);
        assert_eq!(a.hugepage, 0);
        // Short-lived ⇒ segregate into a fresh hugepage rather than mix into the long one.
        let s = f.place(1, PAGE_SIZE, short).unwrap();
        f.mark_committed(&s);
        assert_ne!(
            s.hugepage, a.hugepage,
            "short-lived object segregated from the long-lived hugepage"
        );
        // Another long-lived request packs back into the long hugepage (same lifetime).
        let b = f.place(1, PAGE_SIZE, long).unwrap();
        f.mark_committed(&b);
        assert_eq!(
            b.hugepage, a.hugepage,
            "same-lifetime objects pack together"
        );
        assert!(f.check_invariants());
    }

    #[test]
    fn subrelease_cost_benefit_scales_with_hugepage_density() {
        // §19.6 cost/benefit gate: a small subrelease from a DENSE (cold) hugepage is
        // refused (predicted fragmentation cost > benefit), but the same run passes
        // under pressure, and a large run passes regardless. (The sparse-hugepage case
        // — small runs always worthwhile — is covered by the sparse tests above.)
        let mut f = filler(1);
        let a = place_ok(&mut f, 120, PAGE_SIZE, Hotness::Neutral);
        let b = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Neutral);
        assert!(f.free(b.base, 8).valid); // used = 120, 8 free pages
        assert!(f.mark_cold(a.base)); // the controller marks it cold (else not subreleasable)
                                      // cost = used/8 = 120/8 = 15; benefit = 8 < 15 ⇒ refused without pressure.
        assert!(
            f.subrelease(b.base, 8, /*pressure*/ false).is_none(),
            "a small run from a dense hugepage: cost outweighs benefit"
        );
        // Under pressure, every byte is reclaimed.
        assert!(f.subrelease(b.base, 8, /*pressure*/ true).is_some());
        let _ = a;
        assert!(f.check_invariants());
    }

    #[test]
    fn placement_prefers_partially_used_hugepage_over_opening_new() {
        // W11-4a packing: with one partially-used hugepage and capacity to spare, a
        // new small request fills the existing hugepage rather than opening a new one.
        let mut f = filler(4);
        let a = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Neutral);
        let b = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Neutral);
        assert_eq!(a.hugepage, b.hugepage);
        assert_eq!(
            f.touched(),
            1,
            "packed into one hugepage, none opened needlessly"
        );
    }

    #[test]
    fn full_hot_hugepages_do_not_starve_the_packing_descent() {
        // Regression: `classify_bin` files a **completely full** hugepage in `HotDense`
        // when it is hot, and `HotDense` is the first bin `PACKING_ORDER` scans. With a
        // cumulative `SCAN_CAP` those full hugepages alone exhausted the whole placement
        // budget and `break 'bins` aborted the entire descent — so every subsequent
        // placement opened a *fresh* hugepage even though partially-used ones had room,
        // until the region was exhausted and the backend stopped serving. The cap is now
        // per bin and a full hugepage is skipped without spending budget.
        let mut f = filler(SCAN_CAP + 8);
        let hot = PlaceHints {
            hotness: Hotness::Hot,
            ..PlaceHints::default()
        };

        // Fill SCAN_CAP + 2 hugepages completely, hot ⇒ each is filed `HotDense`.
        for _ in 0..SCAN_CAP + 2 {
            let p = f
                .place(PAGES_PER_HUGEPAGE, PAGE_SIZE, hot)
                .expect("full hugepage");
            f.mark_committed(&p);
        }

        // One more hugepage, carved into two runs so a whole run can be freed (the
        // filler validates the exact extent, M-004). Freeing the big one leaves it with
        // plenty of room and files it in a much emptier bin.
        let big = f
            .place(PAGES_PER_HUGEPAGE - 8, PAGE_SIZE, hot)
            .expect("big run");
        f.mark_committed(&big);
        let small = f.place(8, PAGE_SIZE, hot).expect("small run");
        f.mark_committed(&small);
        let partial_hp = big.base / HUGEPAGE_SIZE;
        assert_eq!(
            small.base / HUGEPAGE_SIZE,
            partial_hp,
            "both runs share one hugepage"
        );
        assert!(f.free(big.base, PAGES_PER_HUGEPAGE - 8).valid);
        let touched_before = f.touched();

        // A new hot request must PACK into that hugepage's free room, not open a fresh
        // one — even though SCAN_CAP + 2 full `HotDense` hugepages precede it.
        let p = f.place(4, PAGE_SIZE, hot).expect("placement");
        f.mark_committed(&p);
        assert_eq!(
            f.touched(),
            touched_before,
            "the placement opened a fresh hugepage instead of packing into the one with room"
        );
        assert_eq!(
            p.base / HUGEPAGE_SIZE,
            partial_hp,
            "the run landed in the hugepage that had room"
        );
        assert!(f.check_invariants());
    }

    /// Regression (bounded placement, §19.3): the per-bin scan counts **every** node it
    /// walks against `SCAN_CAP`, hugepages it skips for being full included. Skipping them
    /// for free made the walk unbounded — §19.4 files a *hot* full hugepage in `HotDense`,
    /// the first bin `PACKING_ORDER` scans, not the excluded `Full` one, so a hot workload
    /// made every placement traverse its entire backend and allocation latency grew with
    /// the region's capacity.
    ///
    /// Counting alone would trade that for a packing loss (the budget spent on hugepages
    /// with no room), so `bin_insert` files a full hugepage at the *tail*: every hugepage
    /// that can still fit a run precedes every one that cannot. That ordering is what this
    /// test pins — the fittable hugepage stays inside the scan budget no matter how many
    /// full ones accumulate.
    #[test]
    fn a_bin_full_of_full_hugepages_stays_within_the_scan_budget() {
        let mut f = filler(4 * SCAN_CAP);
        let hot = PlaceHints {
            hotness: Hotness::Hot,
            ..PlaceHints::default()
        };

        // One hugepage with room to spare, dense and hot enough to be filed `HotDense`.
        let big = f
            .place(PAGES_PER_HUGEPAGE - 8, PAGE_SIZE, hot)
            .expect("big run");
        f.mark_committed(&big);

        // Then far more than a budget's worth of *completely full* hot hugepages, each
        // filed in that same bin.
        for _ in 0..3 * SCAN_CAP {
            let p = f
                .place(PAGES_PER_HUGEPAGE, PAGE_SIZE, hot)
                .expect("full hugepage");
            f.mark_committed(&p);
        }
        assert!(f.check_invariants());

        // The hugepage with room is still within the bin's scan budget of its head — the
        // full ones are all behind it.
        let bin = f.bin_of(big.base).expect("filed in a bin");
        let mut j = f.bins[bin as usize];
        let mut pos = 0usize;
        while j != NIL && j != big.hugepage {
            j = f.get(j).bin_next;
            pos += 1;
        }
        assert_eq!(j, big.hugepage, "the fittable hugepage is in its bin");
        assert!(
            pos < SCAN_CAP,
            "the fittable hugepage sits at position {pos}, past the {SCAN_CAP}-node budget"
        );

        // So the capped scan finds it: the placement packs instead of opening a fresh one.
        let touched_before = f.touched();
        let p = f.place(4, PAGE_SIZE, hot).expect("placement");
        f.mark_committed(&p);
        assert_eq!(
            f.touched(),
            touched_before,
            "the placement opened a fresh hugepage instead of packing into the one with room"
        );
        assert_eq!(p.hugepage, big.hugepage);
        assert!(f.check_invariants());
    }

    #[test]
    fn opens_new_hugepage_when_current_is_full() {
        let mut f = filler(2);
        let n = PAGES_PER_HUGEPAGE;
        let a = place_ok(&mut f, n, PAGE_SIZE, Hotness::Neutral); // fills hugepage 0
        let b = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Neutral); // must open hugepage 1
        assert_ne!(a.hugepage, b.hugepage);
        assert_eq!(f.touched(), 2);
        assert!(f.check_invariants());
    }

    #[test]
    fn place_rejects_zero_oversized_and_unsatisfiable_alignment() {
        let mut f = filler(1);
        assert!(f
            .place(0, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .is_none());
        assert!(f
            .place(
                PAGES_PER_HUGEPAGE + 1,
                PAGE_SIZE,
                PlaceHints::hot(Hotness::Neutral)
            )
            .is_none());
        // Alignment larger than a hugepage cannot be satisfied within one.
        assert!(f
            .place(1, HUGEPAGE_SIZE * 2, PlaceHints::hot(Hotness::Neutral))
            .is_none());
        assert!(f.check_invariants());
    }

    #[test]
    fn region_exhaustion_is_a_safe_none() {
        // One hugepage; fill it; the next placement that needs a fresh hugepage fails
        // cleanly (NoSpace as None), leaving the filler well-formed.
        let mut f = filler(1);
        let n = PAGES_PER_HUGEPAGE;
        let _a = place_ok(&mut f, n, PAGE_SIZE, Hotness::Neutral);
        assert!(f
            .place(1, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .is_none());
        assert!(f.check_invariants());
    }

    #[test]
    fn coverage_metrics_reconcile() {
        // §19.7: the coverage identities hold (live = intact + partial; ratio sane).
        let mut f = filler(3);
        let _a = place_ok(&mut f, 16, PAGE_SIZE, Hotness::Neutral);
        let _b = place_ok(&mut f, 32, PAGE_SIZE, Hotness::Neutral);
        let cov = f.coverage();
        assert_eq!(cov.live_total_bytes, (16 + 32) * PAGE_SIZE as u64);
        assert_eq!(
            cov.live_total_bytes,
            cov.live_bytes_on_intact + cov.live_bytes_on_partial
        );
        // No subrelease yet ⇒ all live is on intact hugepages ⇒ ratio 100%.
        assert_eq!(cov.partial_subreleased_bytes, 0);
        assert_eq!(cov.coverage_ratio_bp(), 10_000);
        // §19.4/H-003: every touched hugepage is counted in exactly one bin, so the
        // bin distribution sums to the touched-hugepage count (coverage_bytes /
        // HUGEPAGE_SIZE). This holds regardless of how placement distributed the runs.
        let binned: u32 = cov.bins.iter().sum();
        assert_eq!(
            binned as u64,
            cov.coverage_bytes / HUGEPAGE_SIZE as u64,
            "bin distribution must reconcile with touched-hugepage count"
        );
        assert!(f.check_invariants());
    }

    #[test]
    fn coverage_ratio_drops_with_partial_subrelease() {
        // Fill one hugepage completely (a + b + c = 128 live pages), then free the
        // middle allocation `b` and subrelease its hole: the hugepage is now partial
        // with 112 live pages, so all live is on a partial hugepage ⇒ ratio 0%.
        let mut f = filler(1);
        let n = PAGES_PER_HUGEPAGE;
        let a = place_ok(&mut f, 32, PAGE_SIZE, Hotness::Cold);
        let b = place_ok(&mut f, 16, PAGE_SIZE, Hotness::Cold);
        let c = place_ok(&mut f, n - 48, PAGE_SIZE, Hotness::Cold);
        let r = f.free(b.base, 16);
        assert!(r.valid && !r.now_empty, "hugepage still holds a and c");
        let _sr = f
            .subrelease(b.base, 16, /*pressure*/ true)
            .expect("subrelease the hole");
        let cov = f.coverage();
        assert_eq!(
            cov.live_bytes_on_partial,
            (n - 16) as u64 * PAGE_SIZE as u64
        );
        assert_eq!(cov.live_bytes_on_intact, 0);
        assert_eq!(cov.partial_subreleased_bytes, 16 * PAGE_SIZE as u64);
        // ratio = live_on_intact / live_total = 0 / 112 = 0%.
        assert_eq!(cov.coverage_ratio_bp(), 0);
        let _ = (a, c);
        assert!(f.check_invariants());
    }

    #[test]
    fn empty_subreleased_hugepage_counts_as_empty_released_not_partial() {
        // A hugepage that is fully freed then subreleased is empty (no live object),
        // so its released bytes are `empty_released`, NOT `partial_subreleased` — the
        // two metrics partition the released bytes and never double-count.
        let mut f = filler(1);
        let a = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Cold);
        assert!(f.free(a.base, 8).valid); // now empty (no live object)
        let _sr = f
            .subrelease(a.base, 8, /*pressure*/ true)
            .expect("subrelease the empty run");
        let cov = f.coverage();
        assert_eq!(cov.live_total_bytes, 0, "no live object remains");
        assert_eq!(
            cov.empty_released_bytes,
            8 * PAGE_SIZE as u64,
            "released bytes on an empty hugepage are empty_released"
        );
        assert_eq!(
            cov.partial_subreleased_bytes, 0,
            "not partial — nothing live remains on the hugepage"
        );
        assert!(f.check_invariants());
    }

    #[test]
    fn subreleased_hole_is_reused_and_healed_on_refill() {
        // Reusing a subreleased run recommits it (M-005), healing the partial
        // hugepage back to intact — the desirable "refill the hole" behaviour.
        let mut f = filler(1);
        let a = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Cold);
        let b = place_ok(&mut f, 8, PAGE_SIZE, Hotness::Cold);
        assert!(f.free(b.base, 8).valid);
        let sr = f.subrelease(b.base, 8, true).expect("subrelease");
        assert_eq!(f.bin_of(a.base), Some(HugeBin::PartialSubreleased));
        // A new placement reuses the subreleased pages (needs recommit) and heals it.
        let c = f
            .place(8, PAGE_SIZE, PlaceHints::hot(Hotness::Cold))
            .expect("refill");
        assert_eq!(c.base, b.base, "the freed/subreleased run is reused");
        assert!(
            c.commit_run.is_some(),
            "subreleased pages must be recommitted (M-005)"
        );
        f.mark_committed(&c);
        assert_ne!(
            f.bin_of(a.base),
            Some(HugeBin::PartialSubreleased),
            "healed"
        );
        assert_eq!(f.coverage().partial_subreleased_bytes, 0);
        let _ = sr;
        assert!(f.check_invariants());
    }

    #[test]
    fn reserve_hugepages_is_contiguous_aligned_and_reusable() {
        // W11-1a: a multi-hugepage run is hugepage-aligned, contiguous, all Full, and
        // its hugepages return to the HugeCache (empty-backed) on free for reuse.
        let mut f = filler(4);
        let run = f.reserve_hugepages(2).expect("2-hugepage run");
        f.mark_run_committed(&run);
        assert_eq!(run.base % HUGEPAGE_SIZE, 0);
        assert_eq!(run.hugepages, 2);
        assert_eq!(f.bin_of(run.base), Some(HugeBin::Full));
        assert_eq!(f.bin_of(run.base + HUGEPAGE_SIZE), Some(HugeBin::Full));
        assert_eq!(f.coverage().live_total_bytes, 2 * HUGEPAGE_SIZE as u64);
        assert!(f.check_invariants());
        // Free returns both hugepages to empty-backed; a later run reuses the same
        // address space (HugeCache hit — no commit needed).
        assert!(f.free_hugepages(run.base, 2).valid);
        assert_eq!(f.bin_of(run.base), Some(HugeBin::EmptyBacked));
        let run2 = f.reserve_hugepages(2).expect("reuse");
        assert_eq!(run2.base, run.base, "empty hugepages reused");
        assert!(run2.commit_run.is_none(), "backed reuse avoids a fault");
        assert!(f.check_invariants());
    }

    #[test]
    fn reserve_hugepages_rejects_oversized_and_exhaustion() {
        let mut f = filler(2);
        assert!(f.reserve_hugepages(0).is_none());
        assert!(f.reserve_hugepages(3).is_none(), "more than capacity");
        let r = f.reserve_hugepages(2).expect("fills the region");
        f.mark_run_committed(&r);
        assert!(f.reserve_hugepages(1).is_none(), "region full");
        // A partial/foreign free is rejected.
        assert!(!f.free_hugepages(r.base + HUGEPAGE_SIZE, 2).valid);
        assert!(f.check_invariants());
    }

    #[test]
    fn many_alloc_free_cycles_keep_invariants() {
        // A churn workload over a few hugepages keeps every invariant green and never
        // leaks a hugepage (a long-running pack/unpack stress).
        let mut f = filler(8);
        let mut live: Vec<(usize, usize)> = Vec::new();
        let mut rng = 0x1234_5678u64;
        for _ in 0..2000 {
            // xorshift for determinism (no external dep).
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let alloc = live.is_empty() || (rng & 1 == 0);
            if alloc {
                let pages = 1 + (rng as usize % 8);
                if let Some(p) = f.place(pages, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral)) {
                    f.mark_committed(&p);
                    live.push((p.base, pages));
                }
            } else {
                let idx = (rng as usize) % live.len();
                let (base, pages) = live.swap_remove(idx);
                assert!(f.free(base, pages).valid);
            }
            assert!(f.check_invariants());
        }
        // Drain everything; the region returns to fully empty-backed.
        for (base, pages) in live {
            assert!(f.free(base, pages).valid);
        }
        let cov = f.coverage();
        assert_eq!(cov.live_total_bytes, 0);
        assert!(f.check_invariants());
    }

    #[test]
    fn invalid_inputs_are_rejected_without_mutation() {
        // M-004 defense-in-depth: every filler entry point that takes an address/length
        // rejects a malformed request (zero length, foreign/out-of-region address,
        // untouched hugepage, or a double free) with **no** state change — so a stray or
        // recycled pointer can never corrupt the bookkeeping. The whole `index_of`-None
        // and `pages == 0` / re-free family in one place.
        let mut f = filler(2);
        let p = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Cold);
        let hp0_base = p.base - (p.base % HUGEPAGE_SIZE); // hugepage 0 base (region base)
        let below = 0x1000usize; // far below the region
        let beyond = hp0_base + 100 * HUGEPAGE_SIZE; // past the 2-hugepage capacity
        let untouched = hp0_base + HUGEPAGE_SIZE; // hugepage 1: in region, never opened

        // Zero-length requests are rejected.
        assert!(!f.free(p.base, 0).valid, "zero-page free rejected");
        assert!(
            f.subrelease(p.base, 0, true).is_none(),
            "zero-page subrelease rejected"
        );
        // Foreign / out-of-region addresses are rejected by every entry point.
        for bad in [below, beyond] {
            assert!(!f.free(bad, 4).valid);
            assert!(f.subrelease(bad, 4, true).is_none());
            assert!(!f.mark_cold(bad));
            assert!(!f.free_hugepages(bad, 1).valid);
        }
        // An in-region but untouched hugepage is rejected (touched == 0 path).
        assert!(!f.mark_cold(untouched));
        assert!(!f.free(untouched, 1).valid);
        assert!(f.subrelease(untouched, 1, true).is_none());

        // A double free of a sub-hugepage run is rejected (the run is no longer live).
        assert!(f.free(p.base, 4).valid, "first free succeeds");
        assert!(!f.free(p.base, 4).valid, "double free rejected (M-004)");

        // A double free of a whole-hugepage *run* is likewise rejected.
        let run = f.reserve_hugepages(1).expect("1-hugepage run");
        f.mark_run_committed(&run);
        assert!(
            f.free_hugepages(run.base, 1).valid,
            "first run free succeeds"
        );
        assert!(
            !f.free_hugepages(run.base, 1).valid,
            "run double free rejected (the hugepage is now empty, not full)"
        );
        assert!(f.check_invariants(), "no mutation from any rejected input");
    }

    #[test]
    fn forged_confirmation_tokens_are_ignored_not_out_of_bounds() {
        // S-007 hardening: `Placement`/`HugeRun`/`Subrelease` are public, copyable
        // confirmation tokens, so a downstream crate could forge one with an
        // out-of-range index/base and hand it to a safe confirmation method. Each must
        // be ignored (no mutation), never resolved into an out-of-bounds slot access or
        // a wrapping address.
        let mut f = filler(2);
        let p = place_ok(&mut f, 4, PAGE_SIZE, Hotness::Neutral);
        let hp0 = p.base - (p.base % HUGEPAGE_SIZE); // hugepage 0 base (= region base)
        let live_before = f.used_pages_of(p.base);

        // Out-of-range hugepage index.
        f.mark_committed(&Placement {
            hugepage: 9999,
            base: hp0,
            pages: 1,
            commit_run: None,
        });
        // Valid base, absurd hugepage count (would overflow the slot walk).
        f.mark_run_committed(&HugeRun {
            base: hp0,
            hugepages: usize::MAX,
            commit_run: None,
        });
        // Out-of-range hugepage + wrapping region offset.
        f.unsubrelease(&Subrelease {
            hugepage: 9999,
            region_offset: usize::MAX,
            len: PAGE_SIZE,
        });

        assert_eq!(
            f.used_pages_of(p.base),
            live_before,
            "no mutation from any forged token"
        );
        assert!(f.check_invariants());
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use std::alloc::{alloc as host_alloc, dealloc, Layout};
    use std::ptr;
    use std::sync::atomic::{AtomicU32, Ordering as O};
    use std::sync::Mutex;

    fn meta(bytes: usize) -> &'static BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let p = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
        Box::leak(Box::new(unsafe { BumpArena::new(p, len) }))
    }

    /// A real-memory provider for the backend tests (the `extent.rs` `HostProvider`
    /// pattern): `reserve` hands out a host allocation (already writable), `decommit`
    /// models `MADV_DONTNEED` by zeroing, and `commit` can be made to fail once to
    /// exercise the well-formed-on-failure rollback (W4-5).
    struct HostProvider {
        owned: Mutex<Vec<(usize, Layout)>>,
        fail_reserve: AtomicU32,
        fail_commit: AtomicU32,
        fail_revoke: AtomicU32,
        fail_decommit: AtomicU32,
        commits: AtomicU32,
        decommits: AtomicU32,
        revokes: AtomicU32,
    }

    impl HostProvider {
        fn new() -> Self {
            Self {
                owned: Mutex::new(Vec::new()),
                fail_reserve: AtomicU32::new(0),
                fail_commit: AtomicU32::new(0),
                fail_revoke: AtomicU32::new(0),
                fail_decommit: AtomicU32::new(0),
                commits: AtomicU32::new(0),
                decommits: AtomicU32::new(0),
                revokes: AtomicU32::new(0),
            }
        }
        fn fail_next_reserve(&self) {
            self.fail_reserve.store(1, O::Relaxed);
        }
        fn fail_next_commit(&self) {
            self.fail_commit.store(1, O::Relaxed);
        }
        fn fail_next_revoke(&self) {
            self.fail_revoke.store(1, O::Relaxed);
        }
        fn fail_next_decommit(&self) {
            self.fail_decommit.store(1, O::Relaxed);
        }
    }

    impl TopoBackingProvider for HostProvider {
        fn reserve(&self, _a: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
            if self.fail_reserve.swap(0, O::Relaxed) == 1 {
                return Err(BackendError::OutOfMemory);
            }
            if size == 0 || !align.is_power_of_two() {
                return Err(BackendError::InvalidRequest);
            }
            let layout =
                Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
            // SAFETY: nonzero size + valid power-of-two alignment (checked above).
            let base = unsafe { host_alloc(layout) };
            if base.is_null() {
                return Err(BackendError::OutOfMemory);
            }
            self.owned.lock().unwrap().push((base as usize, layout));
            Ok(Region { base, len: size })
        }
        fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            self.commits.fetch_add(1, O::Relaxed);
            if self.fail_commit.swap(0, O::Relaxed) == 1 {
                return Err(BackendError::OutOfMemory);
            }
            Ok(())
        }
        fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
            self.decommits.fetch_add(1, O::Relaxed);
            // Fail *before* discarding (a failed decommit leaves contents intact).
            if self.fail_decommit.swap(0, O::Relaxed) == 1 {
                return Err(BackendError::OutOfMemory);
            }
            // Model MADV_DONTNEED: discard the contents (a later read faults a fresh
            // zero page). Bounds: `[offset, offset+len) <= region.len`.
            // SAFETY: in-bounds sub-range of the committed reservation.
            unsafe { ptr::write_bytes(region.base.add(offset), 0, len) };
            Ok(())
        }
        fn revoke_descendants(&self, _a: ArenaId, _r: Region) -> Result<(), BackendError> {
            self.revokes.fetch_add(1, O::Relaxed);
            if self.fail_revoke.swap(0, O::Relaxed) == 1 {
                return Err(BackendError::InvalidRequest);
            }
            Ok(())
        }
        fn release(&self, _a: ArenaId, region: Region) -> Result<(), BackendError> {
            let mut owned = self.owned.lock().unwrap();
            let base = region.base as usize;
            let idx = owned
                .iter()
                .position(|&(b, _)| b == base)
                .ok_or(BackendError::InvalidRequest)?;
            let (_, layout) = owned.swap_remove(idx);
            // SAFETY: exactly the pointer/layout from the matching `reserve`.
            unsafe { dealloc(base as *mut u8, layout) };
            Ok(())
        }
        fn name(&self) -> &'static str {
            "host-huge-test"
        }
    }

    impl Drop for HostProvider {
        fn drop(&mut self) {
            for (base, layout) in self.owned.get_mut().unwrap().drain(..) {
                // SAFETY: same invariant as `release`.
                unsafe { dealloc(base as *mut u8, layout) };
            }
        }
    }

    fn backend(capacity: usize) -> HugePageBackend<HostProvider> {
        HugePageBackend::new(
            HostProvider::new(),
            meta(1 << 20),
            ArenaId::DEFAULT,
            HugeConfig::with_capacity(capacity),
        )
        .expect("hugepage backend")
    }

    /// Write a recognizable pattern over a region and read it back (the region must
    /// be committed + writable host memory of `region.len` bytes).
    fn touch(region: Region) {
        // SAFETY: `region` is committed backing of `region.len` bytes from the backend.
        unsafe {
            ptr::write_bytes(region.base, 0xa5, region.len);
            assert_eq!(*region.base, 0xa5);
            assert_eq!(*region.base.add(region.len - 1), 0xa5);
        }
    }

    #[test]
    fn packed_allocation_is_writable_and_frees() {
        let b = backend(2);
        let r = b
            .allocate(64 * 1024, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("alloc");
        assert_eq!(r.len, 64 * 1024); // page-rounded (4 pages of 16 KiB)
        assert_eq!(r.base as usize % PAGE_SIZE, 0);
        touch(r);
        let cov = b.coverage();
        assert_eq!(cov.live_total_bytes, 64 * 1024);
        assert!(b.free_region(r), "free of a live region succeeds");
        assert_eq!(b.coverage().live_total_bytes, 0);
        assert!(b.check_invariants());
        // Double free is rejected (region no longer live).
        assert!(!b.free_region(r));
        assert!(b.check_invariants());
    }

    #[test]
    fn region_cache_hook_round_trips_through_the_trait() {
        // The §18.6 RegionCacheHook seam: try_alloc serves, try_cache returns.
        let b = backend(2);
        let r = b
            .try_alloc(96 * 1024, PAGE_SIZE, Hints::default())
            .expect("hook alloc");
        touch(r);
        assert!(b.try_cache(r), "hook free returns the region");
        // A foreign region is declined.
        let mut x = 0u8;
        let foreign = Region {
            base: &mut x as *mut u8,
            len: PAGE_SIZE,
        };
        assert!(!b.try_cache(foreign));
        assert!(b.check_invariants());
    }

    #[test]
    fn multi_hugepage_run_serves_then_region_cache_reuses() {
        // W11-1a/W11-3: an allocation larger than a hugepage is served as a contiguous
        // whole-hugepage run; freeing caches it; the next same-rounded request is a
        // region-cache hit (bounded awkward-size waste — no second reservation).
        let b = backend(4);
        // 1 hugepage + 1 page ⇒ rounds to 2 hugepages (the awkward case).
        let bytes = HUGEPAGE_SIZE + PAGE_SIZE;
        let r = b
            .allocate(bytes, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("run");
        assert_eq!(
            r.len, bytes,
            "usable is the page-rounded request, not rounded up"
        );
        assert_eq!(r.base as usize % HUGEPAGE_SIZE, 0, "hugepage-aligned");
        touch(r);
        let touched_after_first = b.touched_hugepages();
        assert_eq!(touched_after_first, 2, "two hugepages reserved for the run");
        assert!(b.free_region(r), "freed → cached in the region cache");
        // A second awkward allocation of the same rounded size reuses the cached run
        // (no new hugepages opened — waste is bounded).
        let r2 = b
            .allocate(bytes, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("reuse");
        assert_eq!(r2.base, r.base, "region-cache hit reuses the same run");
        assert_eq!(
            b.touched_hugepages(),
            touched_after_first,
            "no extra reservation"
        );
        assert!(b.free_region(r2));
        assert!(b.check_invariants());
    }

    #[test]
    fn region_cache_prunes_a_stale_entry_without_double_vending() {
        // §18.6 region cache safety: a cached run can go stale if a smaller allocation
        // reuses part of it via the filler's scan path. The cache detects the stale
        // entry (re-reservation fails) and prunes it, reserving fresh — never handing
        // out a hugepage that is already live.
        let b = backend(4);
        let big = HUGEPAGE_SIZE + PAGE_SIZE; // rounds to a 2-hugepage run
        let r = b
            .allocate(big, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("run");
        assert!(b.free_region(r), "freed → empty-backed + cached");
        // A whole-hugepage allocation reuses the empty run's head via the scan path.
        let one = b
            .allocate(HUGEPAGE_SIZE, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("1-hp");
        assert_eq!(
            one.base, r.base,
            "scan reuses the freed run's head hugepage"
        );
        // The cached 2-hugepage entry is now stale; a 2-hugepage allocation prunes it
        // and reserves a fresh run that does NOT alias the live whole-hugepage alloc.
        let r2 = b
            .allocate(big, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("run2");
        assert_ne!(
            r2.base, one.base,
            "fresh run does not alias the live allocation"
        );
        assert!(b.free_region(one));
        assert!(b.free_region(r2));
        assert!(b.check_invariants());
    }

    #[test]
    fn commit_failure_rolls_back_and_stays_well_formed() {
        // W4-5: an injected commit failure leaves the backend unchanged (the placement
        // is rolled back), and the next allocation succeeds.
        let b = backend(2);
        b.provider().fail_next_commit();
        assert!(
            b.allocate(64 * 1024, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
                .is_none(),
            "commit failure ⇒ allocation fails cleanly"
        );
        assert_eq!(b.coverage().live_total_bytes, 0, "nothing left live");
        assert!(b.check_invariants());
        // Recovery: the next allocation succeeds.
        let r = b
            .allocate(64 * 1024, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("recovers");
        touch(r);
        assert!(b.free_region(r));
        assert!(b.check_invariants());
    }

    #[test]
    fn multi_hugepage_run_commit_failure_rolls_back() {
        // W4-5 for the whole-hugepage-run path: an injected commit failure on a
        // multi-hugepage reservation rolls the filler back (no hugepage left live or
        // half-committed), and a later allocation succeeds.
        let b = backend(4);
        b.provider().fail_next_commit();
        let bytes = HUGEPAGE_SIZE + PAGE_SIZE; // rounds to a 2-hugepage run
        assert!(
            b.allocate(bytes, PAGE_SIZE, PlaceHints::default())
                .is_none(),
            "commit failure on the run path ⇒ allocation fails cleanly"
        );
        assert_eq!(b.coverage().live_total_bytes, 0, "no hugepage left live");
        assert_eq!(
            b.touched_hugepages(),
            2,
            "the two reserved hugepages are now empty"
        );
        assert!(b.check_invariants());
        // Recovery: a later run reuses the rolled-back hugepages.
        let r = b
            .allocate(bytes, PAGE_SIZE, PlaceHints::default())
            .expect("recovers");
        touch(r);
        assert!(b.free_region(r));
        assert!(b.check_invariants());
    }

    #[test]
    fn subrelease_decommit_failure_recommits_and_refuses() {
        // W4-5: if the provider `decommit` fails *after* a successful revoke, the
        // backend recommits (undoes the subrelease) and reports 0 bytes — the run is
        // backed-free again and the filler is well-formed (no released-but-still-mapped
        // page, no live page disturbed).
        let b = backend(1);
        let a = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("a");
        let c = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("c");
        touch(a);
        touch(c);
        assert!(b.free_region(c));
        b.provider().fail_next_decommit();
        assert_eq!(
            b.subrelease(c.base as usize, 8, /*pressure*/ false),
            0,
            "a decommit failure rolls the subrelease back"
        );
        // The run is backed-free again (no partial subrelease recorded).
        assert_eq!(b.coverage().partial_subreleased_bytes, 0);
        assert!(b.check_invariants());
        // A retry (no injected failure) now succeeds and decommits.
        assert_eq!(b.subrelease(c.base as usize, 8, false), 8 * PAGE_SIZE);
        assert!(b.free_region(a));
        assert!(b.check_invariants());
    }

    #[test]
    fn subrelease_decommits_a_sparse_run_and_h005_holds() {
        let b = backend(1);
        // A sparsely-occupied hugepage: a small live object `a` plus a freed run `c`.
        let a = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("a");
        let c = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("c");
        touch(a);
        touch(c);
        assert!(b.free_region(c));
        // The hugepage is now sparse (8/128 used) ⇒ its freed pages may be released
        // even without pressure (the §19.6 sparse gate): RSS drops (decommit called).
        let n = b.subrelease(c.base as usize, 8, /*pressure*/ false);
        assert_eq!(
            n,
            8 * PAGE_SIZE,
            "the sparse free run is returned to the OS"
        );
        assert_eq!(b.coverage().partial_subreleased_bytes, 8 * PAGE_SIZE as u64);
        // H-005: subreleasing the *live* run `a` is refused (returns 0).
        assert_eq!(b.subrelease(a.base as usize, 8, true), 0);
        assert!(b.free_region(a));
        assert!(b.check_invariants());
    }

    #[test]
    fn mark_cold_enables_subrelease_of_a_dense_hugepage() {
        // The §19.6 *cold* gate (the W12 idle-decay hook): a denser (Medium)
        // hugepage's free run is preserved without pressure, until the release
        // controller marks the hugepage cold.
        let b = backend(1);
        let n = PAGES_PER_HUGEPAGE;
        let a = b
            .allocate(
                (n / 2) * PAGE_SIZE,
                PAGE_SIZE,
                PlaceHints::hot(Hotness::Neutral),
            )
            .expect("a");
        let c = b
            .allocate(
                (n / 2) * PAGE_SIZE,
                PAGE_SIZE,
                PlaceHints::hot(Hotness::Neutral),
            )
            .expect("c");
        touch(a);
        touch(c);
        assert!(b.free_region(c));
        // Medium + neutral + no pressure ⇒ preserved (coverage kept, §20.5).
        assert_eq!(b.subrelease(c.base as usize, n / 2, /*pressure*/ false), 0);
        // The controller marks the idle hugepage cold ⇒ subrelease is now allowed.
        assert!(b.mark_cold(c.base as usize));
        assert_eq!(
            b.subrelease(c.base as usize, n / 2, false),
            (n / 2) * PAGE_SIZE
        );
        assert!(b.free_region(a));
        assert!(b.check_invariants());
    }

    #[test]
    fn subrelease_revokes_before_decommit_and_a_revoke_failure_refuses() {
        // §36.6 revoke-before-recycle: subrelease revokes the run's descendants before
        // decommitting; a revoke failure refuses the subrelease (no decommit), leaving
        // the backend well-formed (W4-5).
        let b = backend(1);
        let a = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("a");
        let c = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("c");
        touch(a);
        touch(c);
        assert!(b.free_region(c));
        let decommits_before = b.provider().decommits.load(O::Relaxed);
        // Inject a revoke failure: the subrelease must refuse and NOT decommit.
        b.provider().fail_next_revoke();
        assert_eq!(
            b.subrelease(c.base as usize, 8, /*pressure*/ false),
            0,
            "a revoke failure refuses the subrelease"
        );
        assert_eq!(
            b.provider().decommits.load(O::Relaxed),
            decommits_before,
            "no decommit after a failed revoke"
        );
        assert!(
            b.provider().revokes.load(O::Relaxed) >= 1,
            "revoke was attempted"
        );
        // A normal subrelease revokes then decommits.
        assert_eq!(b.subrelease(c.base as usize, 8, false), 8 * PAGE_SIZE);
        assert!(
            b.provider().decommits.load(O::Relaxed) > decommits_before,
            "decommit followed a successful revoke"
        );
        assert!(b.free_region(a));
        assert!(b.check_invariants());
    }

    #[test]
    fn release_empty_excess_shrinks_the_hugecache_to_the_reserve() {
        // W11-1b demand-reserve mechanism: freeing whole hugepages fills the HugeCache
        // (empty-backed); `release_empty_excess(reserve)` returns the excess backing to
        // the OS, keeping `reserve` empty-backed hugepages for burst reuse.
        let b = backend(4);
        let regions: Vec<Region> = (0..4)
            .map(|_| {
                b.allocate(HUGEPAGE_SIZE, PAGE_SIZE, PlaceHints::default())
                    .expect("whole hugepage")
            })
            .collect();
        for r in &regions {
            assert!(b.free_region(*r));
        }
        // Four empty-backed hugepages, nothing released yet.
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 4 * HUGEPAGE_SIZE as u64);
        assert_eq!(cov.empty_released_bytes, 0);
        // Shrink the reserve to one: releases the other three.
        let released = b.release_empty_excess(1);
        assert_eq!(
            released,
            3 * HUGEPAGE_SIZE,
            "three hugepages' backing returned"
        );
        let cov = b.coverage();
        assert_eq!(cov.empty_released_bytes, 3 * HUGEPAGE_SIZE as u64);
        assert_eq!(
            cov.empty_backed_bytes, HUGEPAGE_SIZE as u64,
            "one kept as reserve"
        );
        // Idempotent at the reserve.
        assert_eq!(b.release_empty_excess(1), 0);
        assert!(b.check_invariants());
    }

    #[test]
    fn teardown_releases_exactly_once() {
        let mut b = backend(2);
        let r = b
            .allocate(32 * 1024, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("alloc");
        touch(r);
        assert!(b.free_region(r));
        assert!(
            b.teardown().is_ok(),
            "explicit teardown surfaces the release result"
        );
        assert!(b.teardown().is_ok(), "idempotent");
        // Drop now runs; the release happened exactly once (no double free / panic).
    }

    #[test]
    fn construction_rolls_back_reservation_on_metadata_exhaustion() {
        // W4-5: a metadata arena too small makes `new` fail and release its
        // reservation rather than leaking it.
        let r = HugePageBackend::new(
            HostProvider::new(),
            meta(8), // far too small for the descriptor pool
            ArenaId::DEFAULT,
            HugeConfig::with_capacity(64),
        );
        assert!(matches!(r, Err(HugeError::NoSpace)));
    }

    #[test]
    fn new_surfaces_a_provider_reserve_failure_without_taking_a_region() {
        // §36.6/W4-5: when the provider's region reservation fails, `new` reports it
        // as `HugeError::Backend` and takes no region (nothing to leak — the failure is
        // upstream of the metadata allocation). The `Drop` of the discarded provider
        // then finds an empty `owned` list (no double free). This is the `Backend`
        // error channel that the metadata-exhaustion test (`NoSpace`) does not cover.
        let prov = HostProvider::new();
        prov.fail_next_reserve();
        let r = HugePageBackend::new(
            prov,
            meta(1 << 20),
            ArenaId::DEFAULT,
            HugeConfig::with_capacity(4),
        );
        assert!(
            matches!(r, Err(HugeError::Backend(BackendError::OutOfMemory))),
            "a provider reserve failure surfaces as HugeError::Backend"
        );
    }

    #[test]
    fn exactly_one_hugepage_is_served_through_the_packed_path() {
        // Boundary: a request of exactly HUGEPAGE_SIZE is `PAGES_PER_HUGEPAGE` pages, so
        // `pages <= PAGES_PER_HUGEPAGE` routes it through the *packed* path (the filler's
        // `place`), filling a single hugepage to `Full` — not the whole-hugepage *run*
        // path (which serves `pages > PAGES_PER_HUGEPAGE`). One page more crosses to the
        // run path. This pins the packed/run routing boundary in `allocate`.
        let b = backend(3);
        let exact = b
            .allocate(HUGEPAGE_SIZE, PAGE_SIZE, PlaceHints::hot(Hotness::Neutral))
            .expect("exactly one hugepage");
        assert_eq!(exact.len, HUGEPAGE_SIZE);
        assert_eq!(exact.base as usize % PAGE_SIZE, 0);
        touch(exact);
        assert_eq!(b.touched_hugepages(), 1, "served from a single hugepage");
        let cov = b.coverage();
        assert_eq!(cov.live_total_bytes, HUGEPAGE_SIZE as u64);
        assert_eq!(cov.bins[HugeBin::Full as usize], 1, "filled to Full");
        // One page over the boundary takes the run path (a second hugepage opens).
        let over = b
            .allocate(
                HUGEPAGE_SIZE + PAGE_SIZE,
                PAGE_SIZE,
                PlaceHints::hot(Hotness::Neutral),
            )
            .expect("one page over ⇒ run path");
        assert_eq!(
            over.base as usize % HUGEPAGE_SIZE,
            0,
            "run is hugepage-aligned"
        );
        assert_eq!(b.touched_hugepages(), 3, "the run took two more hugepages");
        assert!(b.free_region(exact));
        assert!(b.free_region(over));
        assert!(b.check_invariants());
    }

    #[test]
    fn release_empty_excess_zero_releases_the_whole_hugecache() {
        // The reserve=0 edge: keep *no* empty-backed hugepage, so every empty hugepage's
        // backing is returned to the OS. Complements the reserve=1 test, and confirms
        // the loop terminates (each released empty hugepage leaves the EmptyBacked bin).
        let b = backend(3);
        let regions: Vec<Region> = (0..3)
            .map(|_| {
                b.allocate(HUGEPAGE_SIZE, PAGE_SIZE, PlaceHints::default())
                    .expect("whole hugepage")
            })
            .collect();
        for r in &regions {
            assert!(b.free_region(*r));
        }
        assert_eq!(b.coverage().empty_backed_bytes, 3 * HUGEPAGE_SIZE as u64);
        let released = b.release_empty_excess(0);
        assert_eq!(
            released,
            3 * HUGEPAGE_SIZE,
            "reserve 0 releases every empty hugepage"
        );
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 0, "nothing kept");
        assert_eq!(cov.empty_released_bytes, 3 * HUGEPAGE_SIZE as u64);
        assert_eq!(b.release_empty_excess(0), 0, "idempotent once drained");
        assert!(b.check_invariants());
    }

    #[test]
    fn release_empty_excess_reclaims_partially_committed_empties() {
        // Regression for the demand-reserve bug: a hugepage that only ever held a
        // *packed sub-hugepage* allocation is only PARTIALLY committed when emptied, so
        // the old `subrelease(base, PAGES_PER_HUGEPAGE)` hit the "whole-run-committed"
        // guard, reclaimed 0, AND broke the loop — stranding that RSS and every empty
        // hugepage after it. Now we release each empty hugepage's committed prefix and
        // walk by index, so partially-committed empties are reclaimed and one
        // un-releasable hugepage never blocks the rest.
        let b = backend(4);
        // Two 120-page packed allocs land in separate hugepages (120 will not fit
        // beside another 120 in one 128-page hugepage), each committing only 120/128.
        let a = b
            .allocate(120 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("a");
        let c = b
            .allocate(120 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("c");
        assert_ne!(
            a.base as usize / HUGEPAGE_SIZE,
            c.base as usize / HUGEPAGE_SIZE,
            "the two 120-page allocs occupy separate hugepages"
        );
        touch(a);
        touch(c);
        assert!(b.free_region(a));
        assert!(b.free_region(c));
        // Two empty-backed hugepages, each with only 120 of 128 pages committed.
        let cov = b.coverage();
        assert_eq!(
            cov.empty_backed_bytes,
            2 * 120 * PAGE_SIZE as u64,
            "partially-committed empties"
        );
        assert_eq!(cov.empty_released_bytes, 0);
        // reserve = 0 must reclaim BOTH (not 0, and not just the first).
        let released = b.release_empty_excess(0);
        assert_eq!(
            released,
            2 * 120 * PAGE_SIZE,
            "both partially-committed empties' committed RSS reclaimed"
        );
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 0, "no committed RSS left");
        assert_eq!(cov.empty_released_bytes, 2 * 120 * PAGE_SIZE as u64);
        assert!(b.check_invariants());
    }

    #[test]
    fn release_empty_excess_reclaims_a_committed_set_with_a_hole() {
        // Regression: `next_empty_backed` reports the committed **popcount**, and
        // `subrelease` interprets its `pages` argument as a contiguous *run* starting at
        // the given base. Passing the popcount therefore assumed the committed set was
        // the prefix `[0, popcount)`. It is not in general: an over-aligned placement
        // leaves stride holes. With a hole, `run_popcount(committed, 0, count) < count`,
        // `subrelease` refused, and the hugepage's whole RSS was stranded for the
        // process's lifetime while `coverage()` kept advertising it as reclaimable.
        let b = backend(4);
        // Two over-aligned placements: `align_stride(4 * PAGE_SIZE) == 4`, so the second
        // run must start at a page offset that is a multiple of 4 — leaving page 3
        // never committed.
        let a = b
            .allocate(3 * PAGE_SIZE, 4 * PAGE_SIZE, PlaceHints::default())
            .expect("a");
        let c = b
            .allocate(3 * PAGE_SIZE, 4 * PAGE_SIZE, PlaceHints::default())
            .expect("c");
        assert_eq!(
            a.base as usize / HUGEPAGE_SIZE,
            c.base as usize / HUGEPAGE_SIZE,
            "both over-aligned runs pack into one hugepage"
        );
        assert_eq!(
            c.base as usize - a.base as usize,
            4 * PAGE_SIZE,
            "the second run is stride-aligned, leaving page 3 uncommitted"
        );
        touch(a);
        touch(c);
        assert!(b.free_region(a));
        assert!(b.free_region(c));
        // One empty hugepage whose committed set is {0,1,2, 4,5,6} — popcount 6, but
        // *not* the prefix [0,6).
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 6 * PAGE_SIZE as u64);
        // Every committed byte must come back, hole or not.
        let released = b.release_empty_excess(0);
        assert_eq!(
            released,
            6 * PAGE_SIZE,
            "both committed runs reclaimed across the hole"
        );
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 0, "no committed RSS left");
        assert_eq!(cov.empty_released_bytes, 6 * PAGE_SIZE as u64);
        assert_eq!(b.release_empty_excess(0), 0, "idempotent once drained");
        assert!(b.check_invariants());
    }

    #[test]
    fn release_empty_excess_reclaims_a_partially_subreleased_empty() {
        // Regression: `next_empty_backed` required `subreleased() == 0`, so an empty
        // hugepage that had been subreleased and then partially refilled was invisible
        // to the release mechanism — while `coverage()` still counted its committed
        // bytes into `empty_backed_bytes`, the very supply `release_tick` plans rung-2
        // work against. The controller re-planned the same non-executable release every
        // tick and the RSS was never reclaimed.
        // Capacity 1 so the refill below cannot escape into a fresh hugepage — it must
        // land back in the subreleased one, which is the state under test.
        let b = backend(1);
        // 1. Fill 8 pages, free them, and release the whole hugepage.
        let a = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("a");
        touch(a);
        assert!(b.free_region(a));
        assert_eq!(b.release_empty_excess(0), 8 * PAGE_SIZE);
        // 2. Refill only 2 of the 8 released pages, then free them: the hugepage is
        //    empty again but still carries 6 subreleased pages and 2 committed ones.
        let c = b
            .allocate(2 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("c");
        touch(c);
        assert!(b.free_region(c));
        let cov = b.coverage();
        assert_eq!(
            cov.empty_backed_bytes,
            2 * PAGE_SIZE as u64,
            "empty, but still 2 committed pages of RSS"
        );
        assert_eq!(cov.empty_released_bytes, 6 * PAGE_SIZE as u64);
        assert_eq!(
            b.empty_backed_hugepages(),
            1,
            "the reserve must see the same hugepage the release walk does"
        );
        // The remaining committed RSS must be reclaimable.
        assert_eq!(
            b.release_empty_excess(0),
            2 * PAGE_SIZE,
            "a partially-subreleased empty is still reclaimable"
        );
        let cov = b.coverage();
        assert_eq!(cov.empty_backed_bytes, 0);
        assert_eq!(cov.empty_released_bytes, 8 * PAGE_SIZE as u64);
        assert_eq!(b.empty_backed_hugepages(), 0, "nothing left to reclaim");
        assert!(b.check_invariants());
    }

    #[test]
    fn the_backend_is_inert_after_teardown() {
        // After `teardown` returns the reservation, the filler still describes the now
        // -unmapped region. Every mutating op must be inert: `allocate` must not hand
        // out a pointer into unmapped space, and `subrelease`/`release_empty_excess`
        // must not `decommit` an address the OS may have re-mapped (which could discard
        // unrelated memory). Capture a live region + its base BEFORE teardown.
        let mut b = backend(2);
        let r = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("alloc");
        let base = r.base as usize;
        assert!(b.teardown().is_ok());
        assert!(
            b.allocate(64 * 1024, PAGE_SIZE, PlaceHints::default())
                .is_none(),
            "no allocation may be served after teardown"
        );
        assert_eq!(
            b.subrelease(base, 8, /*pressure*/ true),
            0,
            "no decommit on the released (possibly re-mapped) region"
        );
        assert_eq!(
            b.release_empty_excess(0),
            0,
            "the demand-reserve shrink is inert after teardown"
        );
        assert!(!b.free_region(r), "free is inert after teardown");
        // Idempotent with the eventual Drop (released exactly once).
    }

    #[test]
    fn release_tick_rounds_a_sub_hugepage_budget_up_to_one_hugepage() {
        // #4: when the controller's rate cap produces a positive rung-2 budget smaller
        // than one hugepage, release_tick must still release one whole hugepage (the
        // release granularity) rather than flooring to zero and reclaiming nothing on
        // every tick forever. Drive it with a tiny rate cap over a full HugeCache.
        use crate::arena::DecayConfig;
        let b = backend(4);
        let regions: Vec<Region> = (0..3)
            .map(|_| {
                b.allocate(HUGEPAGE_SIZE, PAGE_SIZE, PlaceHints::default())
                    .expect("hp")
            })
            .collect();
        for r in &regions {
            assert!(b.free_region(*r)); // three empty-backed hugepages
        }
        // 1 KiB/s rate cap under Hard pressure (rung 2 active; reserve 0 at an idle
        // alloc rate); every other input is zero, so rung 2 alone is rate-capped.
        let cfg = DecayConfig {
            release_rate_bytes_per_sec: 1024,
            ..DecayConfig::low_rss()
        };
        let mut c = ReleaseController::new(cfg);
        let inputs = ReleaseInputs {
            cgroup_current: 95,
            cgroup_max: 100,
            ..ReleaseInputs::default()
        };
        let _ = b.release_tick(&mut c, 0, inputs); // establish the clock
        let (plan, released) = b.release_tick(&mut c, 1, inputs); // +1s ⇒ ~1 KiB budget
        assert!(
            plan.release_empty_hugepages_bytes > 0,
            "rung 2 wants to release"
        );
        assert!(
            plan.release_empty_hugepages_bytes < HUGEPAGE_SIZE as u64,
            "but the rate cap holds the budget below one hugepage"
        );
        assert_eq!(
            released, HUGEPAGE_SIZE as u64,
            "rounds up to release one whole hugepage — progress, not zero"
        );
        assert!(b.check_invariants());
    }

    #[test]
    fn try_alloc_declines_a_no_hugepage_request() {
        // §10.4: a NO_HUGEPAGE (`HugepagePolicy::Avoid`) request must be declined by the
        // region-cache hook so the engine's large path falls through to the plain
        // extent manager — the avoid flag is honored per allocation even under the
        // hugepage profile. Default/Prefer are still served from the backend.
        let b = backend(2);
        let avoid = Hints {
            hugepage: HugepagePolicy::Avoid,
            ..Hints::default()
        };
        assert!(
            b.try_alloc(96 * 1024, PAGE_SIZE, avoid).is_none(),
            "NO_HUGEPAGE declines the hugepage backend (falls through)"
        );
        let r = b
            .try_alloc(96 * 1024, PAGE_SIZE, Hints::default())
            .expect("a default request is still served");
        assert!(b.try_cache(r));
        assert!(b.check_invariants());
    }

    #[test]
    fn free_region_rejects_a_partial_or_interior_region() {
        // #3 (S-007): `Region` is a public, copyable descriptor, so a forged one could
        // name a partial/interior subrange of a live allocation. `free_region` must
        // reject it — freeing a subrange would clear only some of the allocation's live
        // bits, leaving a live remainder that aliases when the freed pages are reused.
        let b = backend(4);

        // Packed path: a 16-page allocation.
        let a = b
            .allocate(16 * PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("a");
        touch(a);
        let shortened = Region {
            base: a.base,
            len: 8 * PAGE_SIZE,
        };
        assert!(
            !b.free_region(shortened),
            "a shortened (partial-prefix) region is rejected"
        );
        let interior = Region {
            base: (a.base as usize + 4 * PAGE_SIZE) as *mut u8,
            len: 8 * PAGE_SIZE,
        };
        assert!(
            !b.free_region(interior),
            "an interior subrange region is rejected"
        );
        // The allocation is untouched by the rejected frees, and its exact region frees.
        assert_eq!(b.coverage().live_total_bytes, 16 * PAGE_SIZE as u64);
        assert!(b.free_region(a), "the exact region frees");

        // Whole-run path: a 2-hugepage run (HUGEPAGE_SIZE + 1 page rounds up to 2 hp).
        let run = b
            .allocate(HUGEPAGE_SIZE + PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("run");
        touch(run);
        // A one-hugepage prefix routes to the packed free, which refuses a run member.
        let run_prefix = Region {
            base: run.base,
            len: HUGEPAGE_SIZE,
        };
        assert!(
            !b.free_region(run_prefix),
            "a single-hugepage prefix of a multi-hugepage run is rejected"
        );
        // A region spanning more hugepages than the run routes to free_hugepages, whose
        // exact run-length check rejects it.
        let run_oversized = Region {
            base: run.base,
            len: 3 * HUGEPAGE_SIZE,
        };
        assert!(
            !b.free_region(run_oversized),
            "a region larger than the run is rejected"
        );
        assert!(b.free_region(run), "the exact run frees");
        assert!(b.check_invariants());
    }

    #[test]
    fn try_cache_revoking_revokes_before_recycling() {
        // #6 (§36.6/§36.13): an arena-drain free of a cache-served large must revoke the
        // region's descendant capabilities BEFORE it re-enters the HugeCache for reuse
        // by another authority domain. A revoke failure refuses the cache (the region is
        // not recycled — partial failure).
        let b = backend(4);
        let r = b
            .allocate(HUGEPAGE_SIZE + PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("run");
        touch(r);
        let revokes_before = b.provider().revokes.load(O::Relaxed);
        assert!(
            b.try_cache_revoking(r, ArenaId::DEFAULT),
            "the revoking cache path takes the region"
        );
        assert!(
            b.provider().revokes.load(O::Relaxed) > revokes_before,
            "the region's descendants were revoked before it was recycled"
        );
        assert!(b.check_invariants());

        // A revoke failure refuses the cache: the region stays owned (not recycled).
        let r2 = b
            .allocate(HUGEPAGE_SIZE + PAGE_SIZE, PAGE_SIZE, PlaceHints::default())
            .expect("run2");
        touch(r2);
        b.provider().fail_next_revoke();
        assert!(
            !b.try_cache_revoking(r2, ArenaId::DEFAULT),
            "a revoke failure refuses the cache (region not recycled)"
        );
        assert!(b.free_region(r2), "the still-owned region frees normally");
        assert!(b.check_invariants());
    }
}

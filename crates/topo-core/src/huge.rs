// SPDX-License-Identifier: MIT
//! The hugepage-aware backend: filler, huge cache, region cache (§19, plan 04 W11).
//!
//! The hugepage backend keeps live memory packed into a small number of
//! **hugepages** (a `HUGEPAGE_SIZE`-byte unit — 2 MiB on x86-64, [`PAGES_PER_HUGEPAGE`]
//! allocator pages) so that few hugepages hold the live set and empty hugepages
//! release easily (§19.1, Temeraire [R3]). It has four §19.2 components:
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
//!   rounding to whole hugepages (§18.6, W11-3); see [`RegionCache`].
//!
//! **Bins are correctness; the score is policy (DD-2, §2.4).** [`classify_bin`] is a
//! *total* function of `(used, total, subreleased, hotness)` — H-003 holds by
//! construction because the filed bin is recomputed from occupancy on every change.
//! The placement **score** ([`HugePageFiller::score`]) may be arbitrarily wrong
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
use crate::extent::{BackendLock, RegionCacheHook};
use crate::generated::tables::{HUGE_THRESHOLD, PAGE_SIZE};
use crate::ids::ArenaId;
use crate::overflow::pages_for;

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
/// (DD-2: the score is policy). Derived from the request's [`Hints::hotness`]
/// (`0` ⇒ [`Cold`](Hotness::Cold)/[`Neutral`](Hotness::Neutral); high ⇒
/// [`Hot`](Hotness::Hot)) and, for a hugepage, the join of its residents' hotness.
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

/// The nine §19.4 hugepage bins (filler classes), by free space **and** state. Each
/// hugepage is in **exactly one** (H-003); [`classify_bin`] is the total function
/// that decides, and the filler re-files a hugepage whenever its occupancy or state
/// changes. The numeric order is the `repr(u8)` the bin lists index by — it is *not*
/// the packing-preference order (that is [`PACKING_ORDER`]).
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
    // 0 < used < total, so band ∈ {0,…,7}.
    let band = used * 8 / total;
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
/// All byte counts; [`coverage_ratio`](Self::coverage_ratio) is computed, not
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
    /// Bin-list links (when `touched == 1`).
    bin_prev: u32,
    bin_next: u32,
    /// Joined [`Hotness`] of the hugepage's residents (`u8`).
    hotness: u8,
    /// The filed [`HugeBin`] (`u8`) when `touched == 1`.
    bin: u8,
    /// `1` once the hugepage has been opened into a bin (and stays, as the
    /// HugeCache, until reclaimed); `0` while untouched (reserved, in no bin).
    touched: u8,
    _pad: u8,
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

/// Why a placement could not be made (a *safe* failure — the filler is unchanged).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HugeError {
    /// No hugepage (existing or fresh) can hold the request: the region is full or
    /// too fragmented (a clean fall-through, never a wrap).
    NoSpace,
    /// The request is malformed (zero pages, more than a hugepage, or an alignment
    /// that no in-hugepage offset can satisfy).
    InvalidRequest,
    /// The backing provider failed (§36.6); the filler is rolled back, well-formed.
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
            subrelease_events: 0,
        })
    }

    // --- slot pool accessors (the only `unsafe` in the bookkeeping) ----------

    /// Read slot `i` by value (`i < capacity`, a caller invariant).
    #[inline]
    fn get(&self, i: u32) -> HugePageSlot {
        debug_assert!(i < self.capacity, "hugepage slot index out of range");
        // SAFETY: `i < capacity`, the slot memory is initialized (zeroed then only
        // overwritten with valid slots), `&self` precludes a concurrent writer, and
        // `HugePageSlot: Copy` so the read leaves the pool untouched.
        unsafe { self.slots.as_ptr().add(i as usize).read() }
    }

    /// Write slot `i`.
    #[inline]
    fn put(&mut self, i: u32, s: HugePageSlot) {
        debug_assert!(i < self.capacity, "hugepage slot index out of range");
        // SAFETY: `i < capacity` and `&mut self` guarantees exclusive pool access.
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

    /// Insert touched hugepage `i` at the head of `bin`'s list and record the filed
    /// bin on the slot.
    fn bin_insert(&mut self, i: u32, bin: HugeBin) {
        let head = self.bins[bin as usize];
        let mut s = self.get(i);
        s.bin = bin as u8;
        s.bin_prev = NIL;
        s.bin_next = head;
        self.put(i, s);
        if head != NIL {
            let mut h = self.get(head);
            h.bin_prev = i;
            self.put(head, h);
        }
        self.bins[bin as usize] = i;
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
        }
    }

    /// Re-file touched hugepage `i` into the bin its current occupancy/state implies
    /// (H-003), if that differs from its filed bin. Called after any change to a
    /// hugepage's `live`/`released`/`hotness`.
    fn refile(&mut self, i: u32) {
        let s = self.get(i);
        let target = s.target_bin();
        if s.bin != target as u8 {
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

    /// Number of touched hugepages currently in `bin` (a slow stats walk).
    pub fn bin_len(&self, bin: HugeBin) -> usize {
        let mut n = 0usize;
        let mut i = self.bins[bin as usize];
        while i != NIL {
            n += 1;
            i = self.get(i).bin_next;
            if n > self.capacity as usize {
                break; // cycle guard (defensive)
            }
        }
        n
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
    /// terms over **backend-agnostic** inputs (page counts + hotness, never "is a
    /// hardware hugepage" — so W11-6/seLe4n reuse it). Higher is better; ties break
    /// on lower base in [`place`](Self::place). A wrong score never misplaces a live
    /// object (the run is carved from the free bitmap regardless).
    fn score(s: &HugePageSlot, req_hotness: Hotness, run_offset: usize, pages: usize) -> i64 {
        let used = s.used() as i64;
        let subreleased = s.subreleased() as i64;
        // packing_bonus (§19.5): prefer fuller hugepages — fill partially-used over
        // opening new, so emptier hugepages stay releasable.
        let packing = used;
        // hotness_match_bonus: matching hotness keeps hot objects together (§19.5).
        let hotness_match = if s.hotness == req_hotness as u8 { 4 } else { 0 };
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
        // fragmentation_penalty: a run that does not abut page 0 or the live frontier
        // leaves a hole; approximate by the gap before the run.
        let fragmentation = (run_offset as i64).min(8);
        // partial_subrelease_penalty (§19.3): avoid re-filling a subreleased hugepage.
        let subrelease_pen = subreleased;
        packing + hotness_match + release_pres + commit_bonus - fragmentation - subrelease_pen
    }

    /// **Place a `pages`-page run** at `align` with `hotness` (§19.3/§19.5, W11-2b/
    /// W11-4a). Scans the bins in [`PACKING_ORDER`] (fullest-fitting first — **no
    /// full scan of all hugepages**, W11-2b), scores the fitting candidates, and
    /// carves the best (deterministic: best score, then lowest base). Opens a fresh
    /// hugepage only if no touched one fits. Marks the run **live** and re-files the
    /// bin; the caller commits [`Placement::commit_run`] and confirms with
    /// [`mark_committed`](Self::mark_committed) (or rolls back via [`free`](Self::free)).
    ///
    /// `None` if the request is malformed or the region is full/too fragmented — the
    /// filler is **unchanged** on failure (W4-5).
    ///
    /// SPEC-transition: hugepage span placement (§19.3 filler)
    pub fn place(&mut self, pages: usize, align: usize, hotness: Hotness) -> Option<Placement> {
        if pages == 0 || pages > PAGES_PER_HUGEPAGE {
            return None;
        }
        let stride = Self::align_stride(align)?;

        // Candidate selection over the packing-ordered bins (bounded scan, W11-2b):
        // the densest bin that yields any fit wins (packing), best score within it.
        let mut best: Option<(u32, usize, i64)> = None; // (hugepage, run_offset, score)
        let mut scanned = 0usize;
        'bins: for &bin in PACKING_ORDER.iter() {
            let mut i = self.bins[bin as usize];
            let mut found_in_bin = false;
            while i != NIL {
                let s = self.get(i);
                if let Some(off) = find_run_aligned(&s.live, pages, stride) {
                    found_in_bin = true;
                    let sc = Self::score(&s, hotness, off, pages);
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
                scanned += 1;
                // Approximate-bin cap (§19.3 "MAY use approximate bins"): never scan
                // an unbounded number of hugepages on a single placement.
                if scanned >= SCAN_CAP {
                    break 'bins;
                }
            }
            // Packing (§19.5): a fit in a denser bin is preferred — stop descending
            // to emptier bins once this bin yields one.
            if found_in_bin {
                break;
            }
        }

        let (hp, off) = match best {
            Some((hp, off, _)) => (hp, off),
            // No touched hugepage fits: open a fresh one (HugeAllocator, §19.2).
            None => {
                let fresh = self.open_fresh()?;
                debug_assert!(find_run_aligned(&self.get(fresh).live, pages, stride) == Some(0));
                (fresh, 0)
            }
        };
        Some(self.carve(hp, off, pages, hotness))
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
        self.put(i, s);
        // used == 0, no subrelease ⇒ EmptyBacked (H-003).
        self.bin_insert(i, HugeBin::EmptyBacked);
        Some(i)
    }

    /// Mark the `pages`-page run at page offset `off` of hugepage `hp` live and
    /// re-file the bin, returning the [`Placement`] (with the `commit_run` the caller
    /// must back). Pure bookkeeping (the caller drives the provider).
    fn carve(&mut self, hp: u32, off: usize, pages: usize, hotness: Hotness) -> Placement {
        let mut s = self.get(hp);
        debug_assert!(run_is_clear(&s.live, off, pages), "carving over a live run");
        // Does any page in the run need committing (uncommitted or subreleased)?
        let needs_commit = run_popcount(&s.committed, off, pages) != pages;
        let mut k = off;
        while k < off + pages {
            bit_set(&mut s.live, k);
            k += 1;
        }
        s.hotness = Hotness::from_u8(s.hotness).join(hotness) as u8;
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

    /// Confirm a [`Placement`]'s backing after the caller's `commit` succeeded
    /// (M-005): mark every page of the run committed and clear any subreleased bit
    /// (the pages are backed again). Restores `live ⊆ committed` and
    /// `committed ∩ released == ∅` (H-001).
    pub fn mark_committed(&mut self, p: &Placement) {
        let off = (p.base - self.huge_base(p.hugepage)) / PAGE_SIZE;
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
    /// /[`carve`](Self::carve)): clear its live bits and re-file the bin. The pages
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
        let mut s = self.get(hp);
        let mut k = off;
        while k < off + pages {
            bit_clear(&mut s.live, k);
            k += 1;
        }
        let now_empty = popcount(&s.live) == 0;
        if now_empty {
            // An emptied hugepage forgets its hotness (it has no residents); the next
            // resident sets it afresh.
            s.hotness = Hotness::Neutral as u8;
        }
        self.put(hp, s);
        self.refile(hp);
        FreeReport {
            valid: true,
            now_empty,
        }
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
    /// * the predicted RSS benefit (`pages`) exceeds the predicted fragmentation cost
    ///   (modeled as `1`, since any returned run is ≥ 1 page) — trivially true here,
    ///   but the comparison is the §19.6 cost/benefit gate, made explicit.
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
        // Predicted benefit (pages returned) vs predicted cost (≥ 1): the §19.6
        // cost/benefit gate. `pages >= 1` here, so this holds, but it is the explicit
        // comparison the policy requires.
        if pages < 1 {
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
        let base = self.region_base + sr.region_offset;
        let off = (base - self.huge_base(sr.hugepage)) / PAGE_SIZE;
        let pages = sr.len / PAGE_SIZE;
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
            let p = self.carve(j, 0, PAGES_PER_HUGEPAGE, Hotness::Neutral);
            needs_commit |= p.commit_run.is_some();
            j += 1;
        }
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
            let p = self.carve(j, 0, PAGES_PER_HUGEPAGE, Hotness::Neutral);
            needs_commit |= p.commit_run.is_some();
            j += 1;
        }
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
    /// re-file each hugepage. Restores `live ⊆ committed` (H-001).
    pub fn mark_run_committed(&mut self, run: &HugeRun) {
        let start = self.index_of(run.base).expect("run base in region");
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
        // Validate every hugepage of the run is fully live (else a wrong/partial run).
        let mut j = start;
        while j < start + n as u32 {
            let s = self.get(j);
            if s.touched == 0 || s.used() != PAGES_PER_HUGEPAGE {
                return FreeReport::INVALID;
            }
            j += 1;
        }
        let mut j = start;
        while j < start + n as u32 {
            let mut s = self.get(j);
            s.live = [0; BITMAP_WORDS];
            s.hotness = Hotness::Neutral as u8;
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
                if s.touched != 0 || popcount(&s.live) != 0 || popcount(&s.committed) != 0 {
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
                    w += 1;
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
        // 2. Each bin list is well-formed and counts match.
        for (b, (&head, &expected)) in self.bins.iter().zip(binned.iter()).enumerate() {
            let mut j = head;
            let mut prev = NIL;
            let mut seen = 0usize;
            while j != NIL {
                let s = self.get(j);
                if s.touched != 1 || s.bin as usize != b || s.bin_prev != prev {
                    return false;
                }
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

/// The per-placement candidate-scan cap (§19.3 "MAY use approximate bins rather than
/// scanning all hugepages"): a single [`place`](HugePageFiller::place) examines at
/// most this many hugepages across the packing-ordered bins, so placement is bounded
/// regardless of how many hugepages exist (W11-2b "no full scan").
const SCAN_CAP: usize = 64;

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

/// The §19 **hugepage backend**: a [`HugePageFiller`] + §18.6 [`RegionCache`] over a
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

    /// **Allocate `bytes` at `align` with a `hotness` hint** (the richer entry point;
    /// the [`RegionCacheHook`] seam uses [`Hotness::Neutral`]). Packs a sub-hugepage
    /// request into a hugepage via the filler, or serves a whole-hugepage run
    /// (large/awkward) from the region cache or a fresh reservation. Commits the
    /// backing through the provider; on a commit failure the placement is rolled back
    /// and `None` returned (W4-5). `None` also if the request cannot be served (the
    /// caller falls through to the plain extent path).
    ///
    /// SPEC-transition: `huge_allocate` (§18.5 large path / §19 filler)
    pub fn allocate(&self, bytes: usize, align: usize, hotness: Hotness) -> Option<Region> {
        let pages = pages_for(bytes, PAGE_SIZE)?;
        if pages == 0 || !align.is_power_of_two() || align > HUGEPAGE_SIZE {
            return None;
        }
        if pages <= PAGES_PER_HUGEPAGE {
            self.allocate_packed(pages, align, hotness)
        } else {
            self.allocate_run(pages)
        }
    }

    /// Sub-hugepage packing (`pages <= PAGES_PER_HUGEPAGE`): place via the filler and
    /// commit the run.
    fn allocate_packed(&self, pages: usize, align: usize, hotness: Hotness) -> Option<Region> {
        let g = self.lock();
        let p = g.inner.filler.place(pages, align, hotness)?;
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

    /// **Partial subrelease** of the `pages`-page run at `base` (§19.6, W11-4b): the
    /// filler decides under the §19.6 guards (H-005 etc.), and on success this
    /// `decommit`s the run through the provider, dropping RSS. `pressure_high` is the
    /// release controller's pressure signal (plan 04 W12). Returns the bytes
    /// subreleased (`0` if a guard refused or the decommit failed and was rolled back).
    ///
    /// SPEC-transition: partial subrelease + decommit (§19.6/§20.4)
    pub fn subrelease(&self, base: usize, pages: usize, pressure_high: bool) -> usize {
        let g = self.lock();
        let sr = match g.inner.filler.subrelease(base, pages, pressure_high) {
            Some(sr) => sr,
            None => return 0,
        };
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

    /// The §19.7 coverage metrics (W11-5) for this backend's hugepages.
    pub fn coverage(&self) -> HugeStats {
        self.lock().inner.filler.coverage()
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
    fn try_alloc(&self, bytes: usize, align: usize) -> Option<Region> {
        // The §18.6 seam carries no hotness hint, so placement is neutral; the richer
        // `allocate` entry point takes a hint for callers that have one.
        self.allocate(bytes, align, Hotness::Neutral)
    }

    #[inline]
    fn try_cache(&self, region: Region) -> bool {
        self.free_region(region)
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
        let p = f.place(pages, align, hot).expect("placement");
        f.mark_committed(&p);
        assert!(f.check_invariants());
        p
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
        let p = f.place(2, PAGE_SIZE, Hotness::Neutral).expect("place");
        assert!(p.commit_run.is_some(), "fresh pages need a commit");
        f.mark_committed(&p);
        assert!(f.free(p.base, 2).valid);
        let q = f.place(2, PAGE_SIZE, Hotness::Neutral).expect("reuse");
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
        assert!(f.place(0, PAGE_SIZE, Hotness::Neutral).is_none());
        assert!(f
            .place(PAGES_PER_HUGEPAGE + 1, PAGE_SIZE, Hotness::Neutral)
            .is_none());
        // Alignment larger than a hugepage cannot be satisfied within one.
        assert!(f.place(1, HUGEPAGE_SIZE * 2, Hotness::Neutral).is_none());
        assert!(f.check_invariants());
    }

    #[test]
    fn region_exhaustion_is_a_safe_none() {
        // One hugepage; fill it; the next placement that needs a fresh hugepage fails
        // cleanly (NoSpace as None), leaving the filler well-formed.
        let mut f = filler(1);
        let n = PAGES_PER_HUGEPAGE;
        let _a = place_ok(&mut f, n, PAGE_SIZE, Hotness::Neutral);
        assert!(f.place(1, PAGE_SIZE, Hotness::Neutral).is_none());
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
        let c = f.place(8, PAGE_SIZE, Hotness::Cold).expect("refill");
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
                if let Some(p) = f.place(pages, PAGE_SIZE, Hotness::Neutral) {
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
        fail_commit: AtomicU32,
        commits: AtomicU32,
        decommits: AtomicU32,
    }

    impl HostProvider {
        fn new() -> Self {
            Self {
                owned: Mutex::new(Vec::new()),
                fail_commit: AtomicU32::new(0),
                commits: AtomicU32::new(0),
                decommits: AtomicU32::new(0),
            }
        }
        fn fail_next_commit(&self) {
            self.fail_commit.store(1, O::Relaxed);
        }
    }

    impl TopoBackingProvider for HostProvider {
        fn reserve(&self, _a: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
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
            // Model MADV_DONTNEED: discard the contents (a later read faults a fresh
            // zero page). Bounds: `[offset, offset+len) <= region.len`.
            // SAFETY: in-bounds sub-range of the committed reservation.
            unsafe { ptr::write_bytes(region.base.add(offset), 0, len) };
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
            .allocate(64 * 1024, PAGE_SIZE, Hotness::Neutral)
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
        let r = b.try_alloc(96 * 1024, PAGE_SIZE).expect("hook alloc");
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
        let r = b.allocate(bytes, PAGE_SIZE, Hotness::Neutral).expect("run");
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
            .allocate(bytes, PAGE_SIZE, Hotness::Neutral)
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
        let r = b.allocate(big, PAGE_SIZE, Hotness::Neutral).expect("run");
        assert!(b.free_region(r), "freed → empty-backed + cached");
        // A whole-hugepage allocation reuses the empty run's head via the scan path.
        let one = b
            .allocate(HUGEPAGE_SIZE, PAGE_SIZE, Hotness::Neutral)
            .expect("1-hp");
        assert_eq!(
            one.base, r.base,
            "scan reuses the freed run's head hugepage"
        );
        // The cached 2-hugepage entry is now stale; a 2-hugepage allocation prunes it
        // and reserves a fresh run that does NOT alias the live whole-hugepage alloc.
        let r2 = b.allocate(big, PAGE_SIZE, Hotness::Neutral).expect("run2");
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
            b.allocate(64 * 1024, PAGE_SIZE, Hotness::Neutral).is_none(),
            "commit failure ⇒ allocation fails cleanly"
        );
        assert_eq!(b.coverage().live_total_bytes, 0, "nothing left live");
        assert!(b.check_invariants());
        // Recovery: the next allocation succeeds.
        let r = b
            .allocate(64 * 1024, PAGE_SIZE, Hotness::Neutral)
            .expect("recovers");
        touch(r);
        assert!(b.free_region(r));
        assert!(b.check_invariants());
    }

    #[test]
    fn subrelease_decommits_a_sparse_run_and_h005_holds() {
        let b = backend(1);
        // A sparsely-occupied hugepage: a small live object `a` plus a freed run `c`.
        let a = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, Hotness::Neutral)
            .expect("a");
        let c = b
            .allocate(8 * PAGE_SIZE, PAGE_SIZE, Hotness::Neutral)
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
            .allocate((n / 2) * PAGE_SIZE, PAGE_SIZE, Hotness::Neutral)
            .expect("a");
        let c = b
            .allocate((n / 2) * PAGE_SIZE, PAGE_SIZE, Hotness::Neutral)
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
    fn teardown_releases_exactly_once() {
        let mut b = backend(2);
        let r = b
            .allocate(32 * 1024, PAGE_SIZE, Hotness::Neutral)
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
}

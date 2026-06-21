// SPDX-License-Identifier: MIT
//! The back-end: extent and page management (§18, plan 04 W4-2, DD-1).
//!
//! The back-end owns **virtual address ranges and physical backing state** below
//! the span layer (§18.1). It reserves virtual memory through the
//! [`TopoBackingProvider`] seam, splits and
//! merges ranges to satisfy requests and fight fragmentation, tracks the
//! dirty/muzzy/released physical state (§20.1), and returns memory to the OS —
//! **without ever exposing a stale descriptor to pointer classification** (DD-1).
//!
//! **Structure (DD-1: a boundary-tag + free-extent index).** A managed region is
//! tiled by [`Extent`] descriptors held in a fixed-capacity, metadata-backed slot
//! pool. Two indices make the §18.4 operations cheap:
//!
//! * **by address** — every extent (free *and* allocated) is on one ascending,
//!   contiguous, intrusive doubly-linked list, so neighbour-coalescing is O(1)
//!   (the boundary-tag role: an extent's physical neighbours are its list
//!   neighbours, DD-1);
//! * **by size** — every *free* extent is additionally on a size-segregated free
//!   list (binned by `floor(log2(page_count))`), so best/first-fit is a bounded
//!   bin scan (W4-2a).
//!
//! **Why split and merge are separate (DD-1).** They have different preconditions
//! and different stale-descriptor hazards: a split must install both halves'
//! metadata *before* publishing them (failure mode F1), while a merge must retire
//! the absorbed descriptor *behind a generation* so no reader resolves a recycled
//! slot to the wrong range (failure mode F2 — "coalescing is where most backend
//! use-after-free hides"). The slot pool gives each extent a reuse
//! [`generation`](Extent::generation); an [`ExtentRef`] pairs an id with the
//! generation it was minted at, so a reference captured before a merge resolves to
//! `None` once the slot is recycled.
//!
//! **Concurrency (§27.2).** The back-end sits at the *Backend extent lock* — the
//! lowest data-structure lock in the §27.2 hierarchy. [`ExtentMap`] is the pure,
//! single-threaded bookkeeping core (directly unit-tested via `&mut self`);
//! [`ExtentManager`] wraps it behind that lock and drives the provider, exposing
//! `&self` like the rest of the allocator. Holding the backend lock during a
//! provider call is sound — the provider's own lock is below it in the hierarchy.
//!
//! **Safety invariants (mirrored in Lean).** Split/merge preserve range
//! disjointness and the region tiling (plan 02 W1-8a, `Theorems/Span.lean`);
//! release preserves live objects (W1-8c, `Theorems/Release.lean`). At runtime the
//! back-end enforces **M-004** (a range is decommitted/released only when it holds
//! no live object — structurally, only a *free* extent may be released) and
//! **M-005** (a released extent is recommitted before it is handed back out).
//! [`ExtentMap::check_invariants`] is the executable form of the tiling/index
//! well-formedness predicate and is asserted after every mutation in debug builds
//! and by the failure-injection test (W4-5).

use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::backend::{Region, TopoBackingProvider};
use crate::bootstrap::MetadataAlloc;
use crate::error::BackendError;
use crate::flags::Hints;
use crate::generated::tables::PAGE_SIZE;
use crate::ids::ArenaId;
use crate::lock::{LockRank, RankedLock};
use crate::overflow::{align_up, pages_for};

/// A sentinel "no slot" index for the intrusive links (`u32::MAX`).
const NIL: u32 = u32::MAX;

/// Number of size bins (`floor(log2(page_count))` buckets). `usize::BITS + 1`
/// covers every representable page count (a page count never exceeds
/// `usize::MAX / PAGE_SIZE`, so the top bins stay empty — but indexing is always
/// in range).
const NBINS: usize = usize::BITS as usize + 1;

/// Identifies an extent by its slot in the [`ExtentMap`] pool. Pair it with the
/// slot [`generation`](Extent::generation) in an [`ExtentRef`] when holding it
/// across an operation that could retire the slot (split/merge), so a recycled
/// slot is detected (DD-1 failure mode F2).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ExtentId(pub u32);

/// A generation-checked handle to an extent (DD-1 F2). Minted by
/// [`ExtentMap::view`]/alloc, validated by [`ExtentMap::resolve`]; a reference held
/// across a merge that recycled the slot resolves to `None` rather than to the
/// different range now occupying it — the "no stale descriptor visible to
/// classification" guarantee of §18.4.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExtentRef {
    /// The slot.
    pub id: ExtentId,
    /// The slot generation this reference was minted at.
    pub generation: u32,
}

/// Provenance of a freshly-vended region's committed bytes — what the large path
/// knows about the memory it is about to hand out. `calloc` zero-elision (W15-5,
/// §26.2) and the W18-5 verify-on-reuse canary (§29.6) both key off this.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RegionProvenance {
    /// The region was freshly committed from an unbacked source by a **zeroing**
    /// provider, so it reads as all-zero — `calloc` may elide its `memset`.
    pub zeroed: bool,
    /// The region was carved from a **retained, fully-committed, canary-filled**
    /// (`Dirty`) extent, so in a `junk-fill` build it reads as the FREE-pattern
    /// use-after-free canary over its whole length — verify-on-reuse is sound here.
    /// Mutually exclusive with [`zeroed`](Self::zeroed) (a canary is non-zero).
    pub canary: bool,
}

/// Physical-backing state of an extent (§18.2 `ExtentState` / §20.1). The
/// invariant `committed_len ∈ {0, len}` couples the state to the backing:
/// `Active`/`Dirty`/`Muzzy` are fully backed (`committed_len == len`),
/// `Reserved`/`Released` are unbacked (`committed_len == 0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ExtentState {
    /// Virtual range reserved, no physical backing yet (`committed_len == 0`).
    Reserved = 0,
    /// Allocated and handed out; a live object may exist (fully backed).
    Active = 1,
    /// Free, physically backed, may hold old data — reuse is cheap (§20.1 dirty).
    Dirty = 2,
    /// Free, lazily purged or scrubbed (§20.1 muzzy); still mapped.
    Muzzy = 3,
    /// Free, backing returned to the OS; reuse requires a recommit (M-005, §20.1
    /// released). The virtual range is **retained** so a stale pointer into it
    /// classifies as released (P-Map-005) rather than being silently reused.
    Released = 4,
}

impl ExtentState {
    #[inline]
    const fn from_u8(v: u8) -> ExtentState {
        match v {
            1 => ExtentState::Active,
            2 => ExtentState::Dirty,
            3 => ExtentState::Muzzy,
            4 => ExtentState::Released,
            _ => ExtentState::Reserved,
        }
    }

    /// Whether the extent is free (available to satisfy an allocation) — anything
    /// but [`Active`](ExtentState::Active). A free extent holds **no live object**,
    /// which is the structural runtime evidence M-004 requires before decommit /
    /// release.
    #[inline]
    pub const fn is_free(self) -> bool {
        !matches!(self, ExtentState::Active)
    }

    /// Whether the extent is fully physically backed (`committed_len == len`).
    #[inline]
    pub const fn is_backed(self) -> bool {
        matches!(
            self,
            ExtentState::Active | ExtentState::Dirty | ExtentState::Muzzy
        )
    }

    /// Whether `self -> to` is a legal §20.1 physical-backing transition of a single
    /// extent's lifecycle. Reflexive (an idempotent op — e.g. re-purging a muzzy
    /// extent — and a state-preserving split are always legal). The directed edges
    /// are exactly those the physical-state operations produce:
    ///
    ///  * **allocate** (`carve`): any free extent → `Active` (backing is then ensured
    ///    by the commit `alloc` performs, M-005);
    ///  * **free**: `Active` → `Dirty` (retain) or `Released` (eager unmap);
    ///  * **recommit** (`mark_committed`, M-005): an unbacked free `Reserved`/`Released`
    ///    → `Dirty`;
    ///  * **purge**: `Dirty` → `Muzzy`;
    ///  * **decommit/release**: a free `Reserved`/`Dirty`/`Muzzy` → `Released`.
    ///
    /// The forbidden edges carry real meaning: nothing returns to `Reserved` (the
    /// initial, never-backed state — once an extent is touched it never goes back),
    /// and `Active` may not step straight to `Muzzy` (a live extent must be *freed*
    /// before it can be purged — the M-004 "no purge of a live range" guard). The
    /// *structural* `split`/`merge` (§18.4) are **not** lifecycle transitions — they
    /// derive the result extent's state from the combined backing — so they are not
    /// modeled by this relation. This predicate is pinned 1:1 to the Lean
    /// `ExtentState.canTransition` model by the `extent_state_transition_matches_lean`
    /// test and the `lake exe check` `extentStateGate` (W4-2d), so the runtime
    /// machine and the proof cannot drift (the §20.1 analogue of the §36.6
    /// `ProviderState` differential).
    pub const fn can_transition(self, to: ExtentState) -> bool {
        use ExtentState::*;
        if self as u8 == to as u8 {
            return true; // reflexive: idempotent op / state-preserving structural rewrite
        }
        matches!(
            (self, to),
            (Reserved | Dirty | Muzzy | Released, Active) // allocate (carve)
                | (Active, Dirty | Released)              // free (retain | unmap)
                | (Reserved | Released, Dirty)            // recommit (M-005)
                | (Dirty, Muzzy)                          // purge
                | (Reserved | Dirty | Muzzy, Released) // decommit / release
        )
    }
}

/// A hugepage backing range for an extent (§18.2 `HugePageRange`). For M1 (no
/// hardware hugepages — that is W11, M5) an extent has no hugepage backing
/// (`len == 0`); the field and its merge-accounting hook exist so the hugepage
/// filler (W11) reuses the same extent machinery (§36.9: the placement model is
/// backend-agnostic over contiguous normal-frame runs).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HugeRange {
    /// Base of the backing hugepage run (`0` and `len == 0` ⇒ none).
    pub base: usize,
    /// Length of the backing hugepage run in bytes (`0` ⇒ none).
    pub len: usize,
}

impl HugeRange {
    /// The empty (no-hugepage) range.
    pub const NONE: HugeRange = HugeRange { base: 0, len: 0 };

    /// Whether this extent has hugepage backing.
    #[inline]
    pub const fn is_some(self) -> bool {
        self.len != 0
    }
}

/// A read-only snapshot of an [`Extent`] (§18.2), returned to callers so they
/// never touch the slot pool directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Extent {
    /// Slot identity.
    pub id: ExtentId,
    /// Owning arena.
    pub arena: ArenaId,
    /// Base address.
    pub base: usize,
    /// Length in bytes (always a whole number of [`PAGE_SIZE`] pages).
    pub len: usize,
    /// Bytes physically committed (`0` or `len`).
    pub committed_len: usize,
    /// Physical-backing state.
    pub state: ExtentState,
    /// Hugepage backing (W11 hook; `NONE` at M1).
    pub huge: HugeRange,
    /// Split/merge generation (§18.2 `split_generation`): bumped whenever this
    /// extent's geometry changes, so a captured snapshot is detectably stale.
    pub split_generation: u32,
    /// Slot reuse generation (the [`ExtentRef`] guard counter).
    pub generation: u32,
    /// §18.2 `flags` — back-end policy bits ([`ExtentFlags`]).
    pub flags: ExtentFlags,
}

impl Extent {
    /// The half-open address range `[base, base + len)`. Uses saturating
    /// arithmetic so it is **total** even on a hand-constructed [`Extent`] with
    /// nonsensical fields (manager-produced extents never overflow — construction
    /// rejects a region whose `base + len` would wrap).
    #[inline]
    pub const fn range(self) -> (usize, usize) {
        (self.base, self.base.saturating_add(self.len))
    }

    /// The end address `base + len` (saturating; see [`range`](Self::range)).
    #[inline]
    pub const fn end(self) -> usize {
        self.base.saturating_add(self.len)
    }
}

/// §18.2 `flags` — back-end policy bits carried on each [`Extent`]. Reserved for
/// the hugepage filler's per-extent placement/bin hints (W11, §19) and other
/// back-end policy; `NONE` until those land. Defined as a newtype so the bit
/// vocabulary can grow without changing the descriptor layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExtentFlags(pub u32);

impl ExtentFlags {
    /// No flags.
    pub const NONE: ExtentFlags = ExtentFlags(0);

    /// The raw flag word.
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// Bytes in each [`ExtentState`] across a managed region (§20.1), for stats
/// reconciliation (plan 07, W4-3a "states reconcile in stats"). The fields sum to
/// the region length; `committed()` equals the manager's `committed_bytes()`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StateBytes {
    /// Reserved (virtual only, unbacked) bytes.
    pub reserved: usize,
    /// Active (allocated/handed out) bytes.
    pub active: usize,
    /// Dirty (free, backed, may hold old data) bytes.
    pub dirty: usize,
    /// Muzzy (free, lazily purged/scrubbed) bytes.
    pub muzzy: usize,
    /// Released (free, decommitted, needs recommit) bytes.
    pub released: usize,
}

impl StateBytes {
    /// Total bytes across all states (== the managed region length).
    #[inline]
    pub const fn total(self) -> usize {
        self.reserved + self.active + self.dirty + self.muzzy + self.released
    }

    /// Physically-backed bytes (`active + dirty + muzzy`); equals the manager's
    /// `committed_bytes()`.
    #[inline]
    pub const fn committed(self) -> usize {
        self.active + self.dirty + self.muzzy
    }

    /// Free (not [`Active`](ExtentState::Active)) bytes.
    #[inline]
    pub const fn free(self) -> usize {
        self.reserved + self.dirty + self.muzzy + self.released
    }

    /// Field-wise sum, for aggregating several regions' breakdowns into one total
    /// (e.g. the shared backend plus each per-arena hooked region, W10). Saturating
    /// so an aggregate never wraps (`StateBytes` are byte counts bounded by the
    /// reserved address space; saturation is belt-and-suspenders).
    #[inline]
    pub const fn add(self, other: StateBytes) -> StateBytes {
        StateBytes {
            reserved: self.reserved.saturating_add(other.reserved),
            active: self.active.saturating_add(other.active),
            dirty: self.dirty.saturating_add(other.dirty),
            muzzy: self.muzzy.saturating_add(other.muzzy),
            released: self.released.saturating_add(other.released),
        }
    }
}

/// A back-end operation that could not complete. Every variant leaves the
/// back-end state well-formed (W4-5): the operation is refused, not half-done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExtentError {
    /// No free extent large enough, or the slot pool is exhausted (safe failure,
    /// §9.7 — never a wrap or an under-allocation).
    Exhausted,
    /// A size/alignment/page rounding overflowed `usize` (§9.7).
    Overflow,
    /// A malformed request (zero size, non-power-of-two alignment).
    InvalidRequest,
    /// The operation requires a **free** extent but the target is still
    /// [`Active`](ExtentState::Active) — refused to uphold M-004 (no
    /// decommit/release of a range that may hold a live object).
    NotFree,
    /// The [`ExtentRef`] named a slot that was recycled (DD-1 F2) or is unoccupied.
    Stale,
    /// The backing provider failed (§36.6). The back-end state is unchanged.
    Backend(BackendError),
}

impl From<BackendError> for ExtentError {
    fn from(e: BackendError) -> Self {
        ExtentError::Backend(e)
    }
}

/// Fit policy for [`ExtentMap::carve`] (§18.3 `extent_alloc` policy, W4-2a).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Fit {
    /// First adequate extent (lowest size bin, first in the list) — fastest.
    First,
    /// Smallest adequate extent — least fragmentation. Exact because the bins are
    /// size-segregated, so the smallest fit is in the lowest non-empty bin.
    #[default]
    Best,
}

/// Sink notified when the bookkeeping **splits or merges** an extent within a
/// provider-supplied region — the §23.2 `split`/`merge` hook point (plan 06 W10).
/// A *custom backing* (one supplied through the [`TopoBackingProvider`] seam, e.g.
/// a `HookProvider`) uses it to keep its own notion of extent boundaries (§23.1)
/// in step with the allocator's `ExtentMap`.
///
/// **Advisory (§23.4).** The `ExtentMap` is the source of truth for sub-extent
/// geometry: it subdivides the region it `reserve`d without otherwise involving
/// the provider, so a notification carries information the backing *may* track,
/// never permission the bookkeeping *needs*. The methods therefore return nothing
/// — a notifier records or forwards as it sees fit but can never veto or corrupt a
/// split/merge, so the back-end stays well-formed regardless (W10-3). The
/// addresses are absolute byte addresses within the managed region (the same
/// addresses the backing handed out via `reserve`/`alloc`).
pub trait ExtentNotify {
    /// The extent `[base, base + total_len)` was split into a prefix of
    /// `prefix_len` bytes (kept at `base`) and a suffix of `total_len - prefix_len`
    /// bytes (at `base + prefix_len`). `backed` is whether the extent was fully
    /// physically committed (so both halves are).
    fn on_split(&self, base: usize, total_len: usize, prefix_len: usize, backed: bool);

    /// The address-adjacent extents `[left_base, left_base + left_len)` and
    /// `[right_base, right_base + right_len)` (with `left_base + left_len ==
    /// right_base`) were merged into the left one. `backed` is whether both were
    /// fully committed (the merge only coalesces backing-compatible neighbours).
    fn on_merge(
        &self,
        left_base: usize,
        left_len: usize,
        right_base: usize,
        right_len: usize,
        backed: bool,
    );
}

/// The no-op notifier — the POSIX/seLe4n default. The allocator subdivides its
/// reserved region without telling the provider, exactly as before W10. A ZST, so
/// `carve`/`free` over `&NoNotify` compile to the identical pre-W10 code.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoNotify;

impl ExtentNotify for NoNotify {
    #[inline]
    fn on_split(&self, _base: usize, _total_len: usize, _prefix_len: usize, _backed: bool) {}
    #[inline]
    fn on_merge(&self, _lb: usize, _ll: usize, _rb: usize, _rl: usize, _backed: bool) {}
}

/// One pool slot. All fields are integers, so a zeroed block of slots is a valid
/// all-unoccupied pool (`from_raw_parts_mut` over zeroed metadata is sound). Copy,
/// so the bookkeeping reads/writes whole slots by value and never holds two slot
/// borrows at once (keeping the index logic borrow-check-clean and `unsafe`-free
/// beyond the single pool accessor).
#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    base: usize,
    len: usize,
    committed_len: usize,
    huge_base: usize,
    huge_len: usize,
    arena: u32,
    split_gen: u32,
    generation: u32,
    /// §18.2 `flags` (back-end policy bits; see [`ExtentFlags`]).
    flags: u32,
    /// Address-ordered list links (all occupied extents).
    addr_prev: u32,
    addr_next: u32,
    /// Size-bin free-list links (free extents only).
    bin_prev: u32,
    bin_next: u32,
    /// Unused-slot stack link (when `occupied == 0`).
    free_next: u32,
    state: u8,
    occupied: u8,
    /// W18-5 verify-on-reuse provenance (§29.6): `1` iff **every** committed byte of
    /// this extent currently holds the junk-fill FREE-pattern canary and has not been
    /// freshly committed, decommitted, or `MADV_FREE`'d since. Maintained as an
    /// invariant: set only by a [`free_in_canary`](ExtentMap::free_in_canary) to
    /// `Dirty` after the caller canary-filled the bytes; cleared on every transition
    /// that breaks it (fresh commit, decommit, dirty→muzzy, grow-absorb); inherited by
    /// a [`split`](ExtentMap::split_in) and **AND-joined** by a [`merge`](ExtentMap::merge_in).
    /// So `canary == 1` ⟹ the extent is `Dirty`, fully committed, and reads as the
    /// canary — which is exactly when verify-on-reuse is sound (a stale bit on a
    /// muzzy/decommitted extent can never arise, so a correct program is never
    /// false-aborted). Fits in the slot's tail padding (no size growth). A zeroed slot
    /// has `canary == 0` (the safe default), preserving the "zeroed pool is valid".
    canary: u8,
}

/// The size bin for a free extent of `pages` pages: `floor(log2(pages)) + 1`
/// (bin `b` holds pages in `[2^(b-1), 2^b)`), so bins are size-segregated and the
/// smallest fit is in the lowest non-empty adequate bin.
#[inline]
fn bin_index(pages: usize) -> usize {
    (usize::BITS - pages.leading_zeros()) as usize
}

/// The pure, single-threaded extent bookkeeping core (DD-1). Owns the
/// metadata-backed slot pool and the two indices; every operation keeps the
/// region tiled and the indices consistent ([`check_invariants`](Self::check_invariants)).
/// Directly unit-tested via `&mut self`; [`ExtentManager`] wraps it with the
/// backend lock and the provider.
pub struct ExtentMap {
    /// Metadata-backed slot array. Allocated once and zeroed at construction, so
    /// it is a valid `[Slot]` for its whole life; never freed (monotonic
    /// metadata, §17.4). Accessed only through [`get`](Self::get)/[`put`](Self::put)
    /// under `&self`/`&mut self`, which guarantee exclusive/shared access.
    slots: NonNull<Slot>,
    /// Slot capacity.
    cap: u32,
    /// Base of the managed virtual region (page-aligned).
    region_base: usize,
    /// Length of the managed virtual region in bytes (a whole number of pages).
    region_len: usize,
    /// Owning arena of the whole region.
    arena: ArenaId,
    /// Head of the unused-slot stack.
    free_slot_head: u32,
    /// Count of unused slots (so `carve` can pre-check it has enough for its
    /// up-to-two splits and never fail a split mid-way, W4-5).
    unused_slots: u32,
    /// Head/tail of the address-ordered list (ascending base).
    addr_head: u32,
    addr_tail: u32,
    /// Per-size-bin free-list heads.
    bins: [u32; NBINS],
    /// Free (non-`Active`) bytes currently in the index.
    free_bytes: usize,
    /// Committed bytes across all extents (for stats reconciliation, §20 / plan 07).
    committed_bytes: usize,
}

/// Byte size of one extent-descriptor slot, exposed so sizing helpers
/// (`AllocatorConfig::fixed_pool_metadata_bytes`) can pin their per-slot bound
/// at compile time instead of trusting a hand-written constant (W8).
pub(crate) const EXTENT_SLOT_BYTES: usize = core::mem::size_of::<Slot>();

// SAFETY: `ExtentMap` owns its slot memory exclusively (a `NonNull<Slot>` into
// never-freed metadata) and exposes mutation only through `&mut self`. It holds
// no thread-shared state of its own, so moving it across threads is sound; the
// `&self` reads it offers (`view`/`resolve`/`check_invariants`) touch only its own
// fields. `ExtentManager` adds the §27.2 backend lock for shared concurrent use.
unsafe impl Send for ExtentMap {}

impl ExtentMap {
    /// Build an extent map over `[region_base, region_base + region_len)` with a
    /// pool of `slot_cap` extent descriptors from `meta`. The region must be
    /// page-aligned with a page-multiple length and a nonzero `slot_cap`. Returns
    /// `None` if the slot pool cannot be allocated (safe failure).
    ///
    /// The region starts as a single [`Reserved`](ExtentState::Reserved) free
    /// extent (no physical backing; `commit`/`alloc` faults it in on demand —
    /// the §18.1 "commit or fault physical backing" responsibility).
    pub fn new(
        meta: &dyn MetadataAlloc,
        region_base: usize,
        region_len: usize,
        slot_cap: usize,
    ) -> Option<ExtentMap> {
        if region_len == 0
            || slot_cap == 0
            || !region_base.is_multiple_of(PAGE_SIZE)
            || !region_len.is_multiple_of(PAGE_SIZE)
            || slot_cap > (NIL as usize)
            || region_base.checked_add(region_len).is_none()
        {
            return None;
        }
        let bytes = slot_cap.checked_mul(core::mem::size_of::<Slot>())?;
        let mem = meta.alloc(bytes, core::mem::align_of::<Slot>())?;
        // SAFETY: `mem` is a fresh, exclusively-owned, `align_of::<Slot>()`-aligned
        // region of exactly `bytes` bytes; zeroing yields `slot_cap` valid `Slot`s
        // (every field is an integer, so the all-zero bit pattern is a legal value).
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        let slots = mem.cast::<Slot>();

        let mut map = ExtentMap {
            slots,
            cap: slot_cap as u32,
            region_base,
            region_len,
            arena: ArenaId::DEFAULT,
            free_slot_head: NIL,
            unused_slots: 0,
            addr_head: NIL,
            addr_tail: NIL,
            bins: [NIL; NBINS],
            free_bytes: 0,
            committed_bytes: 0,
        };

        // Build the unused-slot stack over every slot (1, 2, …, cap-1, then 0 last
        // so slot 0 is popped first — purely cosmetic). Each slot is already zeroed.
        for i in (0..slot_cap as u32).rev() {
            let mut s = map.get(i);
            s.free_next = map.free_slot_head;
            map.put(i, s);
            map.free_slot_head = i;
            map.unused_slots += 1;
        }

        // Place the initial Reserved free extent covering the whole region.
        let id = map.pop_slot()?; // cannot fail: we just pushed `slot_cap >= 1` slots
        let mut s = map.get(id.0);
        s.base = region_base;
        s.len = region_len;
        s.committed_len = 0;
        s.huge_base = 0;
        s.huge_len = 0;
        s.arena = ArenaId::DEFAULT.0;
        s.split_gen = 0;
        s.flags = ExtentFlags::NONE.0;
        s.state = ExtentState::Reserved as u8;
        s.occupied = 1;
        s.canary = 0; // unbacked Reserved: holds no canary (W18-5)
        s.addr_prev = NIL;
        s.addr_next = NIL;
        map.put(id.0, s);
        map.addr_head = id.0;
        map.addr_tail = id.0;
        map.bin_insert(id.0);
        map.free_bytes = region_len;

        debug_assert!(map.check_invariants());
        Some(map)
    }

    /// Set the arena every extent in this region belongs to (called once by
    /// [`ExtentManager::new`]; an extent's arena never changes, M-002).
    fn set_arena(&mut self, arena: ArenaId) {
        self.arena = arena;
        // The single initial extent is the only one present at construction.
        let head = self.addr_head;
        if head != NIL {
            let mut s = self.get(head);
            s.arena = arena.0;
            self.put(head, s);
        }
    }

    // --- slot pool accessors (the only `unsafe` in the bookkeeping) ----------

    /// Read slot `i` by value. `i < cap` is a caller invariant (every index here
    /// is a live link or a bounds-checked argument).
    #[inline]
    fn get(&self, i: u32) -> Slot {
        debug_assert!(i < self.cap, "extent slot index out of range");
        // SAFETY: `i < cap`, the slot memory is initialized (zeroed at construction
        // then only ever overwritten with valid `Slot`s), `&self` guarantees no
        // concurrent writer, and `Slot: Copy` so the read leaves the pool untouched.
        unsafe { self.slots.as_ptr().add(i as usize).read() }
    }

    /// Write slot `i`.
    #[inline]
    fn put(&mut self, i: u32, s: Slot) {
        debug_assert!(i < self.cap, "extent slot index out of range");
        // SAFETY: `i < cap` and `&mut self` guarantees exclusive access to the pool.
        unsafe { self.slots.as_ptr().add(i as usize).write(s) };
    }

    /// Pop an unused slot off the stack, or `None` if the pool is exhausted. Bumps
    /// the slot generation so an [`ExtentRef`] for a previous occupant is now stale
    /// (DD-1 F2). Does not mark it occupied — the caller fills it in first
    /// (install-before-publish, W4-2b F1).
    fn pop_slot(&mut self) -> Option<ExtentId> {
        let i = self.free_slot_head;
        if i == NIL {
            return None;
        }
        let mut s = self.get(i);
        self.free_slot_head = s.free_next;
        self.unused_slots -= 1;
        s.free_next = NIL;
        s.generation = s.generation.wrapping_add(1);
        self.put(i, s);
        Some(ExtentId(i))
    }

    /// Retire slot `i` to the unused stack. Bumps the generation so any captured
    /// [`ExtentRef`] for it is now stale (the merge "retire behind a generation",
    /// DD-1 F2). The caller has already unlinked it from both indices.
    fn push_slot(&mut self, i: u32) {
        let mut s = self.get(i);
        debug_assert_eq!(s.occupied, 1, "double-retiring an extent slot");
        s.occupied = 0;
        s.generation = s.generation.wrapping_add(1);
        s.free_next = self.free_slot_head;
        self.put(i, s);
        self.free_slot_head = i;
        self.unused_slots += 1;
    }

    // --- size-bin index (free extents) ---------------------------------------

    /// Insert free extent `i` at the head of its size bin.
    fn bin_insert(&mut self, i: u32) {
        let mut s = self.get(i);
        let b = bin_index(s.len / PAGE_SIZE);
        let head = self.bins[b];
        s.bin_prev = NIL;
        s.bin_next = head;
        self.put(i, s);
        if head != NIL {
            let mut h = self.get(head);
            h.bin_prev = i;
            self.put(head, h);
        }
        self.bins[b] = i;
    }

    /// Remove free extent `i` from its size bin.
    fn bin_remove(&mut self, i: u32) {
        let s = self.get(i);
        let b = bin_index(s.len / PAGE_SIZE);
        if s.bin_prev != NIL {
            let mut p = self.get(s.bin_prev);
            p.bin_next = s.bin_next;
            self.put(s.bin_prev, p);
        } else {
            debug_assert_eq!(self.bins[b], i, "free extent not at the head of its bin");
            self.bins[b] = s.bin_next;
        }
        if s.bin_next != NIL {
            let mut n = self.get(s.bin_next);
            n.bin_prev = s.bin_prev;
            self.put(s.bin_next, n);
        }
        let mut s = self.get(i);
        s.bin_prev = NIL;
        s.bin_next = NIL;
        self.put(i, s);
    }

    // --- address-ordered index (all extents) ---------------------------------

    /// Insert `new` immediately after `after` in the address list (`after == NIL`
    /// inserts at the head). `new`'s slot is fully initialized by the caller first.
    fn addr_insert_after(&mut self, new: u32, after: u32) {
        let next = if after == NIL {
            self.addr_head
        } else {
            self.get(after).addr_next
        };
        let mut s = self.get(new);
        s.addr_prev = after;
        s.addr_next = next;
        self.put(new, s);
        if after == NIL {
            self.addr_head = new;
        } else {
            let mut a = self.get(after);
            a.addr_next = new;
            self.put(after, a);
        }
        if next == NIL {
            self.addr_tail = new;
        } else {
            let mut n = self.get(next);
            n.addr_prev = new;
            self.put(next, n);
        }
    }

    /// Unlink `i` from the address list.
    fn addr_remove(&mut self, i: u32) {
        let s = self.get(i);
        if s.addr_prev != NIL {
            let mut p = self.get(s.addr_prev);
            p.addr_next = s.addr_next;
            self.put(s.addr_prev, p);
        } else {
            self.addr_head = s.addr_next;
        }
        if s.addr_next != NIL {
            let mut n = self.get(s.addr_next);
            n.addr_prev = s.addr_prev;
            self.put(s.addr_next, n);
        } else {
            self.addr_tail = s.addr_prev;
        }
    }

    // --- queries -------------------------------------------------------------

    /// The number of extent descriptors the pool can still create.
    #[inline]
    pub fn unused_slots(&self) -> usize {
        self.unused_slots as usize
    }

    /// Free bytes currently available across all free extents.
    #[inline]
    pub fn free_bytes(&self) -> usize {
        self.free_bytes
    }

    /// Bytes physically committed across all extents (for stats reconciliation,
    /// §20 / plan 07 — the dirty/muzzy/released accounting that W4-3a documents).
    #[inline]
    pub fn committed_bytes(&self) -> usize {
        self.committed_bytes
    }

    /// Bytes in each [`ExtentState`] across the region (§20.1), the breakdown that
    /// reconciles in stats (plan 07, W4-3a). Walks the address list (a slow-path
    /// stats read). The result satisfies `total() == region_len` and
    /// `committed() == committed_bytes()` — both `debug_assert`ed here.
    pub fn state_bytes(&self) -> StateBytes {
        let mut sb = StateBytes::default();
        let mut i = self.addr_head;
        while i != NIL {
            let s = self.get(i);
            match ExtentState::from_u8(s.state) {
                ExtentState::Reserved => sb.reserved += s.len,
                ExtentState::Active => sb.active += s.len,
                ExtentState::Dirty => sb.dirty += s.len,
                ExtentState::Muzzy => sb.muzzy += s.len,
                ExtentState::Released => sb.released += s.len,
            }
            i = s.addr_next;
        }
        debug_assert_eq!(
            sb.total(),
            self.region_len,
            "state_bytes must tile the region"
        );
        debug_assert_eq!(
            sb.committed(),
            self.committed_bytes,
            "committed bytes must agree"
        );
        sb
    }

    /// The managed region's address range.
    #[inline]
    pub fn region(&self) -> (usize, usize) {
        (self.region_base, self.region_base + self.region_len)
    }

    /// A snapshot of extent `id`, or `None` if the slot is unoccupied.
    pub fn view(&self, id: ExtentId) -> Option<Extent> {
        if id.0 >= self.cap {
            return None;
        }
        let s = self.get(id.0);
        if s.occupied == 0 {
            return None;
        }
        Some(self.snapshot(id.0, s))
    }

    /// A generation-checked reference to extent `id` (the [`ExtentRef`] guard).
    pub fn extent_ref(&self, id: ExtentId) -> Option<ExtentRef> {
        self.view(id).map(|e| ExtentRef {
            id,
            generation: e.generation,
        })
    }

    /// Resolve a reference, returning `None` (stale) if the slot was recycled or
    /// is unoccupied — the §18.4 "no stale descriptor" guard.
    pub fn resolve(&self, r: ExtentRef) -> Option<Extent> {
        let e = self.view(r.id)?;
        if e.generation == r.generation {
            Some(e)
        } else {
            None
        }
    }

    fn snapshot(&self, i: u32, s: Slot) -> Extent {
        Extent {
            id: ExtentId(i),
            arena: ArenaId(s.arena),
            base: s.base,
            len: s.len,
            committed_len: s.committed_len,
            state: ExtentState::from_u8(s.state),
            huge: HugeRange {
                base: s.huge_base,
                len: s.huge_len,
            },
            split_generation: s.split_gen,
            generation: s.generation,
            flags: ExtentFlags(s.flags),
        }
    }

    // --- split (W4-2b) -------------------------------------------------------

    /// Split extent `id` at `prefix_len` into `[base, base + prefix_len)` (kept in
    /// `id`) and a fresh extent `[base + prefix_len, end)`. Both results are
    /// page-aligned (the precondition is `prefix_len` a nonzero page multiple
    /// strictly inside the extent, §18.4). Returns the new (right) extent's id, or
    /// `None` if `prefix_len` is invalid or the slot pool is exhausted — in which
    /// case **nothing is mutated** (the pop is the first, only fallible, step), so
    /// the back-end stays well-formed (W4-5, failure mode F1: never publish a
    /// half-built half).
    ///
    /// Both halves inherit `id`'s state and backing: `committed_len ∈ {0, len}` is
    /// preserved on each. The `split_generation` of both is bumped so a snapshot
    /// taken before the split is detectably stale.
    ///
    /// SPEC-transition: `extent_split` (§18.3 / §33.4 `span_split_preserves_disjointness`)
    pub fn split(&mut self, id: ExtentId, prefix_len: usize) -> Option<ExtentId> {
        self.split_in(id, prefix_len, &NoNotify)
    }

    /// [`split`](Self::split), additionally notifying `notify` of the §23.2 split
    /// once it has succeeded (plan 06 W10). The notification is the *only*
    /// difference from [`split`](Self::split); the bookkeeping is identical, so the
    /// `&NoNotify` form is byte-for-byte the pre-W10 path.
    pub fn split_in(
        &mut self,
        id: ExtentId,
        prefix_len: usize,
        notify: &dyn ExtentNotify,
    ) -> Option<ExtentId> {
        let parent = self.view(id)?;
        // §18.4: both results page-aligned ⇒ prefix is a nonzero page multiple,
        // and strictly less than the extent (a zero-length tail is not a split).
        if prefix_len == 0 || prefix_len >= parent.len || !prefix_len.is_multiple_of(PAGE_SIZE) {
            return None;
        }
        // Allocate the right half's descriptor first (the only fallible step).
        let right = self.pop_slot()?;
        let suffix_len = parent.len - prefix_len;
        // Backing splits cleanly because `committed_len ∈ {0, len}`: a fully-backed
        // parent yields two fully-backed halves; an unbacked parent, two unbacked.
        let backed = parent.committed_len == parent.len;
        let right_base = parent.base + prefix_len;

        // The parent's canary provenance (W18-5): a split of a uniformly
        // canary-filled extent yields two canary-filled sub-ranges (the fill covered
        // the whole parent), so both halves inherit it.
        let parent_canary = self.get(id.0).canary;
        // Fully initialize the right half *before* it is linked anywhere
        // (install-before-publish, F1).
        let mut rs = self.get(right.0);
        rs.base = right_base;
        rs.len = suffix_len;
        rs.committed_len = if backed { suffix_len } else { 0 };
        rs.huge_base = 0;
        rs.huge_len = 0;
        rs.arena = parent.arena.0;
        rs.split_gen = parent.split_generation.wrapping_add(1);
        rs.flags = parent.flags.0; // the right half inherits the parent's policy bits
        rs.state = parent.state as u8;
        rs.occupied = 1;
        rs.canary = parent_canary; // W18-5: both halves cover the same canary bytes
        self.put(right.0, rs);

        // Shrink the parent (left half) and bump its generation.
        let mut ls = self.get(id.0);
        let was_free = ExtentState::from_u8(ls.state).is_free();
        if was_free {
            self.bin_remove(id.0); // its bin changes with its size
        }
        ls = self.get(id.0);
        ls.len = prefix_len;
        ls.committed_len = if backed { prefix_len } else { 0 };
        ls.split_gen = ls.split_gen.wrapping_add(1);
        self.put(id.0, ls);

        // Publish: link the right half after the left in address order, and bin
        // both halves if free.
        self.addr_insert_after(right.0, id.0);
        if was_free {
            self.bin_insert(id.0);
            self.bin_insert(right.0);
        }
        debug_assert!(self.check_invariants());
        // §23.2 split notification (W10): the bookkeeping has committed, so this is
        // purely informational (a custom backing may track the new boundary). Uses
        // the pre-split geometry (`parent`), the source of `prefix_len`.
        notify.on_split(parent.base, parent.len, prefix_len, backed);
        Some(right)
    }

    // --- merge / coalesce (W4-2c) --------------------------------------------

    /// Whether `left` and `right` may be merged (§18.4): address-adjacent, same
    /// arena, both free, **state-compatible** (both fully backed or both
    /// unbacked, so the merged `committed_len = left + right` keeps the
    /// `committed_len ∈ {0, len}` invariant), and hugepage-accounting consistent
    /// (neither hugepage-backed at M1).
    fn mergeable(&self, left: &Extent, right: &Extent) -> bool {
        left.end() == right.base                          // adjacency
            && left.arena == right.arena                  // arena-compat (M-002)
            && left.state.is_free()
            && right.state.is_free()                      // never merge a live range
            && (left.committed_len == left.len) == (right.committed_len == right.len) // backing-compat
            && !left.huge.is_some()
            && !right.huge.is_some() // hugepage-accounting (W11 refines)
    }

    /// Merge the address-adjacent free extents `left` and `right` into `left`
    /// (§18.4), retiring `right`'s descriptor behind a generation bump so no reader
    /// resolves the recycled slot to the merged range (DD-1 F2: "no stale
    /// descriptors visible to classification"). Returns `false` (no mutation) if
    /// the §18.4 compatibility gates fail.
    ///
    /// SPEC-transition: `extent_merge` (§18.3 / §33.4 `span_merge_preserves_disjointness`)
    pub fn merge(&mut self, left: ExtentId, right: ExtentId) -> bool {
        self.merge_in(left, right, &NoNotify)
    }

    /// [`merge`](Self::merge), additionally notifying `notify` of the §23.2 merge
    /// once it has succeeded (plan 06 W10). Informational only — see
    /// [`split_in`](Self::split_in).
    pub fn merge_in(&mut self, left: ExtentId, right: ExtentId, notify: &dyn ExtentNotify) -> bool {
        let (lv, rv) = match (self.view(left), self.view(right)) {
            (Some(l), Some(r)) => (l, r),
            _ => return false,
        };
        if left == right || !self.mergeable(&lv, &rv) {
            return false;
        }
        // The right half's canary provenance, read before its slot is retired (W18-5).
        let right_canary = self.get(right.0).canary;
        // Unlink both from their bins, and `right` from the address list.
        self.bin_remove(left.0);
        self.bin_remove(right.0);
        self.addr_remove(right.0);
        // Grow `left` to cover both; backing adds exactly (state-compatible).
        let mut ls = self.get(left.0);
        ls.len = lv.len + rv.len;
        ls.committed_len = lv.committed_len + rv.committed_len;
        ls.split_gen = ls.split_gen.wrapping_add(1);
        // W18-5: the merged range reads as the canary only if **both** halves did —
        // a non-canary neighbour taints the join, so verify-on-reuse stays sound.
        ls.canary &= right_canary;
        // The merged state is the conservative join: a backed merge is `Dirty`
        // ("may hold old data" subsumes muzzy/clean); an unbacked merge stays
        // `Released` (still needs a recommit, M-005).
        ls.state = if ls.committed_len == ls.len {
            ExtentState::Dirty as u8
        } else {
            ExtentState::Released as u8
        };
        self.put(left.0, ls);
        // Re-bin `left` (it grew) and retire `right`'s slot (generation bumped).
        self.bin_insert(left.0);
        self.push_slot(right.0);
        debug_assert!(self.check_invariants());
        // §23.2 merge notification (W10): informational, after the bookkeeping
        // commits. Uses the pre-merge geometries (`lv`/`rv`); `backed` is shared
        // because `mergeable` already required backing-compatibility.
        notify.on_merge(lv.base, lv.len, rv.base, rv.len, lv.committed_len == lv.len);
        true
    }

    /// Coalesce extent `id` with its free, compatible address-neighbours (§18.4),
    /// returning the surviving extent's id. Used by `free` to fight fragmentation.
    /// At most two merges (left and right neighbour), each O(1) via the address
    /// list — the boundary-tag coalescing of DD-1.
    pub fn coalesce(&mut self, id: ExtentId) -> ExtentId {
        self.coalesce_in(id, &NoNotify)
    }

    /// [`coalesce`](Self::coalesce), notifying `notify` of each §23.2 merge it
    /// performs (plan 06 W10).
    pub fn coalesce_in(&mut self, id: ExtentId, notify: &dyn ExtentNotify) -> ExtentId {
        let mut survivor = id;
        // Merge with the right neighbour into `survivor`.
        let next = self.get(survivor.0).addr_next;
        if next != NIL && self.merge_in(survivor, ExtentId(next), notify) {
            // `survivor` absorbed `next`.
        }
        // Merge with the left neighbour into it (the left becomes the survivor).
        let prev = self.get(survivor.0).addr_prev;
        if prev != NIL && self.merge_in(ExtentId(prev), survivor, notify) {
            survivor = ExtentId(prev);
        }
        survivor
    }

    /// The address-adjacent successor of `id` (the next-higher-based extent), or
    /// `None` at the region tail. Used by in-place **grow** to find the free
    /// neighbour to absorb (W15-3a).
    fn addr_next(&self, id: ExtentId) -> Option<ExtentId> {
        let n = self.get(id.0).addr_next;
        (n != NIL).then_some(ExtentId(n))
    }

    /// Grow the **Active** extent `id` by absorbing its address-adjacent **free**
    /// neighbour `next` — exactly the bytes to add, which the caller has already
    /// committed so both halves are fully backed (§18.3 grow, the dual of
    /// [`split_in`](Self::split_in)). `next`'s descriptor is retired behind a
    /// generation bump so no reader resolves the recycled slot (DD-1 F2). The
    /// kept extent `id` keeps its id and generation (live pointers stay valid);
    /// only its length grows.
    ///
    /// SPEC-transition: `extent_merge` (§18.3 / §33.4 `span_merge_preserves_disjointness`)
    fn absorb_next_in(&mut self, id: ExtentId, next: ExtentId, notify: &dyn ExtentNotify) {
        let lv = self.get(id.0);
        let rv = self.get(next.0);
        debug_assert_eq!(
            ExtentState::from_u8(lv.state),
            ExtentState::Active,
            "absorb grows an Active extent"
        );
        debug_assert!(
            ExtentState::from_u8(rv.state).is_free(),
            "absorb consumes a free neighbour"
        );
        debug_assert_eq!(lv.base + lv.len, rv.base, "absorb requires adjacency");
        debug_assert_eq!(
            rv.committed_len, rv.len,
            "absorbed neighbour must be fully committed (M-005)"
        );
        let (lbase, llen, rbase, rlen) = (lv.base, lv.len, rv.base, rv.len);
        // Unlink the neighbour from the free index + address list, fold its bytes
        // into the active extent, and retire its slot.
        self.bin_remove(next.0);
        self.addr_remove(next.0);
        let mut s = self.get(id.0);
        s.len += rlen;
        s.committed_len += rv.committed_len;
        s.split_gen = s.split_gen.wrapping_add(1);
        s.canary = 0; // W18-5: a grown (Active) extent's range is mixed provenance
        self.put(id.0, s);
        self.free_bytes -= rlen;
        self.push_slot(next.0);
        debug_assert!(self.check_invariants());
        // §23.2 merge notification (W10): the active range absorbed the free one; a
        // custom backing tracks the new boundary. Both fully backed (asserted above).
        notify.on_merge(lbase, llen, rbase, rlen, true);
    }

    // --- allocation (W4-2b) --------------------------------------------------

    /// Find a free extent that can satisfy `needed_len` bytes at `align`, returning
    /// `(slot, aligned_base, prefix_len)`. `needed_len` is a page multiple and
    /// `align` a power of two. Best-fit is exact (bins are size-segregated). Any
    /// alignment prefix is a page multiple (the region and every extent base are
    /// page-aligned and `align` is a power of two), so the resulting splits stay
    /// page-aligned (§18.4).
    fn find_fit(&self, needed_len: usize, align: usize, fit: Fit) -> Option<(u32, usize, usize)> {
        let needed_pages = needed_len / PAGE_SIZE;
        let start = bin_index(needed_pages);
        let mut best: Option<(u32, usize, usize)> = None;
        for b in start..NBINS {
            let mut i = self.bins[b];
            while i != NIL {
                let s = self.get(i);
                // Aligned base within this extent, and the prefix it costs.
                if let Some(aligned) = align_up(s.base, align) {
                    let prefix = aligned - s.base; // aligned >= base ⇒ no underflow
                                                   // `prefix + needed_len` cannot overflow: both are bounded by the
                                                   // region length, itself a valid `usize` range.
                    if let Some(total) = prefix.checked_add(needed_len) {
                        if s.len >= total {
                            match fit {
                                Fit::First => return Some((i, aligned, prefix)),
                                Fit::Best => {
                                    let better = match best {
                                        None => true,
                                        Some((bi, _, _)) => s.len < self.get(bi).len,
                                    };
                                    if better {
                                        best = Some((i, aligned, prefix));
                                    }
                                }
                            }
                        }
                    }
                }
                i = self.get(i).bin_next;
            }
            // Best-fit: the bins are size-segregated, so once a bin yields a fit the
            // smallest fit overall is the smallest within it — no need to look higher.
            if matches!(fit, Fit::Best) && best.is_some() {
                break;
            }
        }
        best
    }

    /// Carve a free extent of exactly `needed_len` bytes aligned to `align` out of
    /// the index, marking it [`Active`](ExtentState::Active) and returning its id.
    /// Pure bookkeeping (no provider call): `committed_len` is left as the carved
    /// extent's prior backing — [`ExtentManager`] commits the uncommitted part
    /// before handing it out (M-005). `needed_len` must be a nonzero page multiple
    /// and `align` a power of two. `None` on no-fit or slot exhaustion (safe
    /// failure); the index is unchanged on failure.
    ///
    /// SPEC-transition: `extent_alloc` (§18.3)
    pub fn carve(&mut self, needed_len: usize, align: usize, fit: Fit) -> Option<ExtentId> {
        self.carve_in(needed_len, align, fit, &NoNotify)
    }

    /// [`carve`](Self::carve), notifying `notify` of each §23.2 split the carve
    /// performs (an alignment-prefix trim and/or a size-tail trim, plan 06 W10).
    /// The notification is the *only* difference; the carve's pre-check-then-commit
    /// failure-safety (W4-5: a refused carve never half-mutates) is unchanged.
    pub fn carve_in(
        &mut self,
        needed_len: usize,
        align: usize,
        fit: Fit,
        notify: &dyn ExtentNotify,
    ) -> Option<ExtentId> {
        if needed_len == 0 || !needed_len.is_multiple_of(PAGE_SIZE) || !align.is_power_of_two() {
            return None;
        }
        let (slot, aligned, prefix) = self.find_fit(needed_len, align, fit)?;
        // A carve performs a prefix split (if misaligned) and/or a tail split (if the
        // fit is larger than needed), each needing one fresh slot. Pre-check the pool
        // for *exactly* the splits this carve will perform, so neither split can fail
        // mid-way (W4-5: a refused carve never half-mutates).
        let avail = self.get(slot).len;
        let splits = (prefix > 0) as u32 + ((avail - prefix > needed_len) as u32);
        if self.unused_slots < splits {
            return None; // would need a split but the pool is exhausted (safe)
        }

        let mut active = ExtentId(slot);
        // Trim the alignment prefix: split off `[base, aligned)` as a free head,
        // and continue with the aligned remainder.
        if prefix > 0 {
            let right = self.split_in(active, prefix, notify)?; // left free, right aligned
            active = right;
        }
        // Trim the size remainder: split off the tail past `needed_len` as free.
        let active_len = self.get(active.0).len;
        if active_len > needed_len {
            let _tail = self.split_in(active, needed_len, notify)?; // tail stays free
        }
        debug_assert_eq!(self.get(active.0).base, aligned);
        debug_assert_eq!(self.get(active.0).len, needed_len);

        // Mark the carved extent Active and remove it from the free index.
        self.bin_remove(active.0);
        let mut s = self.get(active.0);
        let was_free_bytes = s.len;
        debug_assert!(
            ExtentState::from_u8(s.state).can_transition(ExtentState::Active),
            "carve: illegal §20.1 transition to Active"
        );
        s.state = ExtentState::Active as u8;
        self.put(active.0, s);
        self.free_bytes -= was_free_bytes;
        debug_assert!(self.check_invariants());
        Some(active)
    }

    // --- free / physical-state transitions (W4-2d) ---------------------------

    /// Return an [`Active`](ExtentState::Active) extent to the free index in
    /// `new_state` (the back-end's chosen physical state — [`Dirty`](ExtentState::Dirty)
    /// to retain backing, or `Released` if the caller already decommitted), then
    /// coalesce with free neighbours (§18.4). Returns the surviving extent's id.
    /// Infallible — freeing never needs a slot (merge only retires them) and never
    /// fails (you cannot refuse to free).
    ///
    /// SPEC-transition: `extent free` (object/span `Live -> CentralFree`/`Dirty`, §7.2/§20.1)
    pub fn free(&mut self, id: ExtentId, new_state: ExtentState) -> Option<ExtentId> {
        self.free_in(id, new_state, &NoNotify)
    }

    /// [`free`](Self::free), notifying `notify` of each §23.2 merge the coalesce
    /// performs (plan 06 W10).
    pub fn free_in(
        &mut self,
        id: ExtentId,
        new_state: ExtentState,
        notify: &dyn ExtentNotify,
    ) -> Option<ExtentId> {
        self.free_in_canary(id, new_state, notify, false)
    }

    /// [`free_in`](Self::free_in), additionally recording whether the caller has
    /// **canary-filled** the freed bytes (W18-5, §29.6): when `canary` and the
    /// extent retains its backing (`Dirty`), the surviving extent is flagged so a
    /// later reuse of these exact committed bytes can verify the use-after-free
    /// canary. The flag is set **before** the coalesce so a merge AND-joins it with
    /// the neighbours' (a non-canary neighbour correctly clears it). A `false`
    /// `canary` (or a non-`Dirty` free) clears the flag — the conservative default.
    pub fn free_in_canary(
        &mut self,
        id: ExtentId,
        new_state: ExtentState,
        notify: &dyn ExtentNotify,
        canary: bool,
    ) -> Option<ExtentId> {
        let e = self.view(id)?;
        debug_assert_eq!(e.state, ExtentState::Active, "freeing a non-Active extent");
        debug_assert!(
            e.state.can_transition(new_state),
            "free: illegal §20.1 transition"
        );
        let mut s = self.get(id.0);
        s.state = new_state as u8;
        // W18-5: only a retained (`Dirty`) extent whose bytes were just canary-filled
        // is verify-on-reuse eligible; everything else (released/unmapped, or not
        // filled) is conservatively non-canary.
        s.canary = (canary && new_state == ExtentState::Dirty) as u8;
        self.put(id.0, s);
        self.free_bytes += e.len;
        self.bin_insert(id.0);
        let survivor = self.coalesce_in(id, notify);
        debug_assert!(self.check_invariants());
        Some(survivor)
    }

    /// Mark extent `id` fully committed (the bookkeeping half of a `commit`;
    /// [`ExtentManager`] calls the provider). Idempotent. Returns the byte count
    /// that newly became committed (so the manager commits exactly that range).
    fn mark_committed(&mut self, id: ExtentId) -> usize {
        let mut s = self.get(id.0);
        let newly = s.len - s.committed_len;
        s.committed_len = s.len;
        // W18-5: freshly committed pages are OS-fresh (zero/garbage), not the canary,
        // so a partial-commit carve is no longer uniformly canary-filled.
        if newly > 0 {
            s.canary = 0;
        }
        // A free extent that was Released/Reserved becomes Dirty once backed again.
        let prev = ExtentState::from_u8(s.state);
        if prev.is_free() && !prev.is_backed() {
            debug_assert!(
                prev.can_transition(ExtentState::Dirty),
                "recommit: illegal §20.1 transition to Dirty"
            );
            s.state = ExtentState::Dirty as u8;
        }
        self.put(id.0, s);
        self.committed_bytes += newly;
        newly
    }

    /// Mark extent `id` decommitted and `new_state` (the bookkeeping half of a
    /// `decommit`/`release`/`purge`). Returns the byte count that was committed
    /// before (so the manager decommits exactly that range).
    fn mark_decommitted(&mut self, id: ExtentId, new_state: ExtentState) -> usize {
        let mut s = self.get(id.0);
        let was = s.committed_len;
        s.committed_len = 0;
        s.canary = 0; // W18-5: decommitted bytes are zero-on-next-touch — no canary
        debug_assert!(
            ExtentState::from_u8(s.state).can_transition(new_state),
            "decommit: illegal §20.1 transition"
        );
        s.state = new_state as u8;
        self.put(id.0, s);
        self.committed_bytes -= was;
        was
    }

    /// Set extent `id`'s state without touching its backing (e.g. dirty → muzzy on
    /// a lazy purge, where the pages stay mapped). Returns `false` if `id` is
    /// unoccupied.
    fn set_state(&mut self, id: ExtentId, new_state: ExtentState) -> bool {
        if self.view(id).is_none() {
            return false;
        }
        let mut s = self.get(id.0);
        debug_assert!(
            ExtentState::from_u8(s.state).can_transition(new_state),
            "set_state: illegal §20.1 transition"
        );
        // W18-5: leaving Dirty (e.g. dirty→muzzy on a lazy purge — the kernel may
        // reclaim/zero `MADV_FREE`'d pages) makes the bytes no longer a reliable
        // canary. Only a Dirty, fully-committed extent is verify-on-reuse eligible.
        if new_state != ExtentState::Dirty {
            s.canary = 0;
        }
        s.state = new_state as u8;
        self.put(id.0, s);
        true
    }

    /// Whether extent `id` currently carries the W18-5 verify-on-reuse canary (its
    /// committed bytes read as the junk-fill FREE pattern). See [`Slot::canary`].
    fn extent_canary(&self, id: ExtentId) -> bool {
        self.get(id.0).canary == 1
    }

    /// Set extent `id`'s §18.2 policy `flags` (caller pre-validated `id` is live).
    fn set_flags(&mut self, id: ExtentId, flags: ExtentFlags) {
        let mut s = self.get(id.0);
        s.flags = flags.0;
        self.put(id.0, s);
    }

    // --- invariants (W4-5 oracle) --------------------------------------------

    /// Whether the back-end is well-formed: the address list tiles the region
    /// exactly (sorted, contiguous, gap-free, no overlap), every occupied extent is
    /// listed once, every free extent is in exactly its size bin, no `Active`
    /// extent is binned, `committed_len ∈ {0, len}` matches the state, and the slot
    /// accounting balances. This is the executable form of the §18 tiling
    /// well-formedness predicate (the W4-5 invariant the failure-injection test
    /// keeps green); `debug_assert`ed after every mutation.
    pub fn check_invariants(&self) -> bool {
        // 1. Address list tiles the region exactly, in ascending order.
        let mut cursor = self.region_base;
        let mut i = self.addr_head;
        let mut prev = NIL;
        let mut listed = 0usize;
        let mut free_bytes = 0usize;
        let mut committed = 0usize;
        while i != NIL {
            let s = self.get(i);
            if s.occupied != 1 {
                return false;
            }
            if s.addr_prev != prev {
                return false;
            }
            if s.base != cursor {
                return false; // gap or overlap
            }
            if s.len == 0 || !s.len.is_multiple_of(PAGE_SIZE) {
                return false;
            }
            let st = ExtentState::from_u8(s.state);
            // `committed_len ∈ {0, len}` always (backing is all-or-nothing here).
            if s.committed_len != 0 && s.committed_len != s.len {
                return false;
            }
            // The state↔backing coupling holds for *free* extents (Dirty/Muzzy are
            // backed, Reserved/Released are not). An `Active` extent may be
            // transiently uncommitted while the manager commits it — M-005
            // guarantees backing before the range is *used*, not while it is being
            // carved — so the coupling is not required for `Active`.
            if st.is_free() && st.is_backed() != (s.committed_len == s.len) {
                return false;
            }
            // Bin membership: free ⇔ in its size bin; Active ⇔ not binned.
            let in_bin = self.is_in_bin(i);
            if st.is_free() != in_bin {
                return false;
            }
            if st.is_free() {
                free_bytes += s.len;
            }
            committed += s.committed_len;
            cursor = match cursor.checked_add(s.len) {
                Some(c) => c,
                None => return false,
            };
            prev = i;
            i = s.addr_next;
            listed += 1;
            // Manifest linear bound. The list is already cycle-free by construction
            // (the `addr_prev == prev` check above rejects any revisited node, and
            // `cursor` strictly increases), but bounding the walk by capacity keeps
            // the oracle total even if a future refactor weakens those checks —
            // matching the bin walks below.
            if listed > self.cap as usize {
                return false;
            }
        }
        if cursor != self.region_base + self.region_len {
            return false; // does not cover the whole region
        }
        if self.addr_tail != prev {
            return false;
        }
        if free_bytes != self.free_bytes || committed != self.committed_bytes {
            return false;
        }
        // 2. Slot accounting: listed (occupied) + unused == capacity.
        if listed + self.unused_slots as usize != self.cap as usize {
            return false;
        }
        // 3. Every binned extent is free and in the correct bin.
        for b in 0..NBINS {
            let mut j = self.bins[b];
            let mut seen = 0usize;
            while j != NIL {
                let s = self.get(j);
                if s.occupied != 1
                    || !ExtentState::from_u8(s.state).is_free()
                    || bin_index(s.len / PAGE_SIZE) != b
                {
                    return false;
                }
                j = s.bin_next;
                seen += 1;
                if seen > self.cap as usize {
                    return false; // cycle guard
                }
            }
        }
        true
    }

    /// Whether extent `i` is reachable from its size bin's head (debug check).
    fn is_in_bin(&self, i: u32) -> bool {
        let s = self.get(i);
        let b = bin_index(s.len / PAGE_SIZE);
        let mut j = self.bins[b];
        let mut steps = 0usize;
        while j != NIL {
            if j == i {
                return true;
            }
            j = self.get(j).bin_next;
            steps += 1;
            if steps > self.cap as usize {
                return false;
            }
        }
        false
    }
}

// ===========================================================================
// The lock-guarded, provider-driving back-end (§27.2 backend lock, W4-2d/3/4/5).
// ===========================================================================

/// The §27.2 *Backend extent lock* (rank [`LockRank::BACKEND`]) — a ranked
/// test-and-test-and-set spinlock (the single [`RankedLock`] primitive, routed
/// through the W16-1b lock-order checker). The backend's critical sections are a
/// handful of slot edits plus one provider call; it is the lowest data-structure
/// lock in the hierarchy, so holding it across a provider call cannot invert the
/// §27.2 order. `pub(crate)` so the large-allocation path ([`crate::large`]) and
/// the hugepage backend ([`crate::huge`]) reuse it for their descriptor-pool
/// critical sections (all rank `BACKEND`; they are never held simultaneously, so
/// the shared rank is correct — see the [`crate::lock`] module docs).
pub(crate) type BackendLock = RankedLock<{ LockRank::BACKEND }>;

/// Retain-versus-unmap policy for freed extents (§20.5, W4-3b). Retaining virtual
/// address space improves reuse and metadata stability; unmapping (here:
/// decommitting backing eagerly on free) reduces RSS and turns a use-after-free
/// into a fault-or-released-classification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetainPolicy {
    /// Keep backing on free (the extent becomes [`Dirty`](ExtentState::Dirty), reuse
    /// is cheap). The default on 64-bit server platforms (§20.5).
    Retain,
    /// Decommit backing on free (the extent becomes [`Released`](ExtentState::Released),
    /// needing a recommit before reuse — M-005). More aggressive RSS reclaim and
    /// use-after-free catching for `debug`/`low-rss` and address-space-scarce
    /// (32-bit) targets (§20.5).
    Unmap,
}

impl RetainPolicy {
    /// The policy implied by the build profile (§20.5, W4-3b): retain on a 64-bit
    /// performance build; unmap more aggressively under `debug`/`low-rss` or on a
    /// 32-bit (address-space-scarce) target.
    pub const fn from_profile() -> RetainPolicy {
        if cfg!(any(feature = "debug", feature = "low-rss")) || cfg!(target_pointer_width = "32") {
            RetainPolicy::Unmap
        } else {
            RetainPolicy::Retain
        }
    }
}

/// The §18.6 region cache: a hook for allocations slightly larger than a hugepage,
/// which would waste memory if rounded up to a whole number of hugepages. The
/// concrete cache lands with the hugepage backend (W11-3, M5); this trait is the
/// **hook point** so the large-allocation path (W4-4) can consult it without
/// depending on the hugepage code, and the default implementations make a
/// hookless build fall straight through to the extent manager.
pub trait RegionCacheHook {
    /// Try to satisfy a `bytes`/`align` large request from cached awkward-sized
    /// regions (§18.6), returning a region the cache now considers handed out, or
    /// `None` to fall through to the extent manager. `hints` carries the request's
    /// advisory placement preferences (hotness/lifetime, §19.3/§19.5) so a hugepage
    /// backend can pack hot/cold and same-lifetime objects together (W11); a cache
    /// that does not place by hints simply ignores them. Default: `None`.
    fn try_alloc(&self, bytes: usize, align: usize, hints: Hints) -> Option<Region> {
        let _ = (bytes, align, hints);
        None
    }

    /// Offer a freed large `region` back to the cache; returns `true` if the cache
    /// took ownership (so the manager must not release it). Default: `false`.
    fn try_cache(&self, region: Region) -> bool {
        let _ = region;
        false
    }

    /// Like [`try_cache`](Self::try_cache), but for an **arena drain** (destroy/reset):
    /// **revoke** the region's descendant capabilities for `arena` before it re-enters
    /// the cache for reuse by another authority domain (§36.6/§36.13
    /// revoke-before-recycle). Returns `true` if the cache took ownership; `false` if
    /// it declined **or a revoke failed** (the region is then not recycled — the §36.13
    /// partial-failure signal). The default forwards to [`try_cache`](Self::try_cache):
    /// a cache with no capability backing (POSIX single ambient authority) has nothing
    /// to revoke, so caching is already isolation-safe.
    fn try_cache_revoking(&self, region: Region, arena: ArenaId) -> bool {
        let _ = arena;
        self.try_cache(region)
    }

    /// **In-place tail trim** of a cache-served large allocation (§25.3 / W15-3b
    /// cache-served shrink): shrink the live allocation described by `region` (its
    /// current base + usable length) to `new_len` page-rounded bytes, returning the
    /// **freed tail byte count** if the cache trimmed it in place, or `None` to leave
    /// it whole (the realloc caller then keeps the allocation as-is). The default
    /// declines — a cache that owns no page geometry has no tail to return. A
    /// hugepage backend trims the allocation's tail pages back to its filler (W11).
    fn try_trim(&self, region: Region, new_len: usize) -> Option<usize> {
        let _ = (region, new_len);
        None
    }
}

/// The no-op region cache used until W11-3 supplies a real one (§18.6).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRegionCache;

impl RegionCacheHook for NoRegionCache {}

/// Adapts a [`TopoBackingProvider`] to the [`ExtentNotify`] sink (plan 06 W10): a
/// bookkeeping split/merge inside the manager's region is dispatched to the
/// provider's §23.2 [`split`](TopoBackingProvider::split) /
/// [`merge`](TopoBackingProvider::merge) hook (a no-op default on POSIX/seLe4n; the
/// custom-backing notification on a `HookProvider`). The absolute extent base the
/// `ExtentMap` reports is rebased onto the manager's reserved region, so the
/// dispatched [`Region`] pointer carries the backing's own provenance rather than a
/// bare integer cast. The provider's `Result` is intentionally discarded — the
/// notification is advisory (§23.4): the bookkeeping has already committed and is
/// well-formed; a backing that wishes to surface a hook failure records it itself
/// (e.g. `HookProvider`'s counters).
struct ProviderNotify<'p, P: TopoBackingProvider> {
    provider: &'p P,
    /// The manager's whole reserved region — its `base` is the provenance root.
    region: Region,
    /// The arena the manager serves (passed through to the §23.2 hook).
    arena: ArenaId,
}

impl<P: TopoBackingProvider> ProviderNotify<'_, P> {
    /// A same-provenance [`Region`] for the sub-extent `[base, base + len)` within
    /// the reserved region (`base` is an absolute address the `ExtentMap` reports).
    #[inline]
    fn subregion(&self, base: usize, len: usize) -> Region {
        let offset = base.wrapping_sub(self.region.base as usize);
        Region {
            base: self.region.base.wrapping_add(offset),
            len,
        }
    }
}

impl<P: TopoBackingProvider> ExtentNotify for ProviderNotify<'_, P> {
    #[inline]
    fn on_split(&self, base: usize, total_len: usize, prefix_len: usize, backed: bool) {
        let region = self.subregion(base, total_len);
        let _ = self.provider.split(
            self.arena,
            region,
            prefix_len,
            total_len - prefix_len,
            backed,
        );
    }

    #[inline]
    fn on_merge(&self, lb: usize, ll: usize, rb: usize, rl: usize, backed: bool) {
        let left = self.subregion(lb, ll);
        let right = self.subregion(rb, rl);
        let _ = self.provider.merge(self.arena, left, right, backed);
    }
}

/// The back-end extent manager (§18): an [`ExtentMap`] over a provider-reserved
/// region, guarded by the §27.2 backend lock and driving the
/// [`TopoBackingProvider`] for every physical-state transition. POSIX is the
/// degenerate single-authority case; the same manager runs over the seLe4n
/// simulator (D2).
///
/// Every operation is fallible and leaves the back-end well-formed on failure
/// (W4-5): the physical (provider) step is sequenced so that a provider failure
/// rolls the bookkeeping back to a consistent state rather than stranding a
/// half-committed extent.
pub struct ExtentManager<P: TopoBackingProvider> {
    provider: P,
    /// The whole reserved region (the unit `release`d on `Drop`; physical-state
    /// ops pass it plus the sub-extent's offset/len to the provider).
    region: Region,
    arena: ArenaId,
    retain: RetainPolicy,
    lock: BackendLock,
    map: UnsafeCell<ExtentMap>,
    /// Whether the region has already been returned to the provider — set by an
    /// explicit [`teardown`](Self::teardown) or by `Drop`, so the `release` happens
    /// exactly once. Only ever touched under exclusive access (`&mut self` / drop),
    /// never through a shared `&self`, so it needs no synchronization (W10 fallible
    /// teardown).
    released: bool,
}

// SAFETY: every access to `map` goes through `lock` (the §27.2 backend lock),
// which serializes mutators; `region`/`arena`/`retain` are immutable after
// construction; and the provider is `Sync`. So concurrent `&self` use is
// data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for ExtentManager<P> {}
// SAFETY: the manager owns its `map` (metadata-backed, never aliased) and a `Send`
// provider; moving it across threads moves both with no shared aliasing.
unsafe impl<P: TopoBackingProvider + Send> Send for ExtentManager<P> {}

/// An RAII hold of the backend lock exposing the guarded [`ExtentMap`]. Releases
/// the lock on drop — including on an unwinding `debug_assert!` — so the lock is
/// never left held (the span lock's discipline, applied to the backend lock).
struct MapGuard<'a> {
    lock: &'a BackendLock,
    map: &'a mut ExtentMap,
}

impl Drop for MapGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.lock.release();
    }
}

impl<P: TopoBackingProvider> ExtentManager<P> {
    /// Reserve a region of `size` bytes aligned to `align` from `provider` for
    /// `arena` and build a back-end over it with a pool of `slot_cap` extent
    /// descriptors from `meta`. The region begins as a single
    /// [`Reserved`](ExtentState::Reserved) free extent; backing is committed on
    /// demand by [`alloc`](Self::alloc). The retain/unmap policy is taken from the
    /// build profile (§20.5, W4-3b).
    ///
    /// Returns the provider's error if the reservation fails, or
    /// [`ExtentError::Exhausted`] if the metadata for the slot pool cannot be
    /// allocated. On any failure the reserved region (if any) is released, so no
    /// reservation is leaked (W4-5).
    pub fn new(
        provider: P,
        meta: &dyn MetadataAlloc,
        arena: ArenaId,
        size: usize,
        align: usize,
        slot_cap: usize,
    ) -> Result<Self, ExtentError> {
        // The managed region must be a whole number of pages so every extent base
        // and split is page-aligned (§18.4).
        let pages = pages_for(size, PAGE_SIZE).ok_or(ExtentError::Overflow)?;
        let region_len = pages.checked_mul(PAGE_SIZE).ok_or(ExtentError::Overflow)?;
        if region_len == 0 || !align.is_power_of_two() {
            return Err(ExtentError::InvalidRequest);
        }
        let region = provider
            .reserve(arena, region_len, align.max(PAGE_SIZE))
            .map_err(ExtentError::Backend)?;
        let region_base = region.base as usize;
        match ExtentMap::new(meta, region_base, region_len, slot_cap) {
            Some(mut map) => {
                map.set_arena(arena);
                Ok(Self {
                    provider,
                    region,
                    arena,
                    retain: RetainPolicy::from_profile(),
                    lock: BackendLock::new(),
                    map: UnsafeCell::new(map),
                    released: false,
                })
            }
            None => {
                // Roll back the reservation so a failed construction leaks nothing.
                let _ = provider.release(arena, region);
                Err(ExtentError::Exhausted)
            }
        }
    }

    /// The active retain/unmap policy (§20.5).
    #[inline]
    pub fn retain_policy(&self) -> RetainPolicy {
        self.retain
    }

    /// Override the retain/unmap policy (tests / the control plane, §20.5).
    #[inline]
    pub fn set_retain_policy(&mut self, policy: RetainPolicy) {
        self.retain = policy;
    }

    /// The backend's name (the provider's), for diagnostics/traces.
    #[inline]
    pub fn backend_name(&self) -> &'static str {
        self.provider.name()
    }

    /// The whole reserved [`Region`] this manager owns (its single provider
    /// reservation). Used to confirm two managers over a custom backing hold
    /// **disjoint** regions (§23.3 no-overlap across separate reservations, W10).
    #[inline]
    pub fn reserved_region(&self) -> Region {
        self.region
    }

    /// Borrow the backing provider — e.g. to read a [`HookProvider`](crate::HookProvider)'s
    /// per-kind hook-failure counts for [`ArenaStats`](crate::ArenaStats) (W10).
    #[inline]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// **Explicitly** return the reserved region to the provider, surfacing the
    /// provider's result instead of discarding it the way `Drop` must (W10 strict
    /// teardown). Idempotent: the region is released **exactly once** across this
    /// call and the eventual `Drop`. A custom backing ([`HookProvider`](crate::HookProvider))
    /// can refuse the return (a failing `dealloc` hook); the arena-destroy path routes
    /// that `Err` into the §36.13 quarantine instead of reporting a clean destroy.
    ///
    /// The release is attempted only once even on failure: a refused return will
    /// refuse again, and the provider's reservation set has already dropped the
    /// region, so a retry from `Drop` would mis-account it.
    pub fn teardown(&mut self) -> Result<(), BackendError> {
        if self.released {
            return Ok(());
        }
        self.released = true;
        self.provider.release(self.arena, self.region)
    }

    /// Acquire the backend lock and expose the guarded map (RAII release).
    #[inline]
    fn lock(&self) -> MapGuard<'_> {
        self.lock.acquire();
        MapGuard {
            lock: &self.lock,
            // SAFETY: the lock is held, granting exclusive access to `map`.
            map: unsafe { &mut *self.map.get() },
        }
    }

    /// The provider offset of `extent_base` within the reserved region.
    #[inline]
    fn sub_offset(&self, extent_base: usize) -> usize {
        extent_base - (self.region.base as usize)
    }

    /// The [`ExtentNotify`] sink that dispatches the bookkeeping's split/merge
    /// events to the provider's §23.2 hooks (plan 06 W10). Borrows the provider for
    /// the duration of one `carve_in`/`free_in`; a no-op on POSIX/seLe4n.
    #[inline]
    fn notifier(&self) -> ProviderNotify<'_, P> {
        ProviderNotify {
            provider: &self.provider,
            region: self.region,
            arena: self.arena,
        }
    }

    /// The [`Region`] (address + length) backing extent `r`, or `None` if stale.
    pub fn region_of(&self, r: ExtentRef) -> Option<Region> {
        let g = self.lock();
        let e = g.map.resolve(r)?;
        Some(Region {
            // The sub-extent pointer derives from the reserved region's base, so it
            // carries the same provenance as the bytes the provider handed us.
            base: self.region.base.wrapping_add(self.sub_offset(e.base)),
            len: e.len,
        })
    }

    /// A snapshot of extent `r`, or `None` if the reference is stale.
    pub fn view(&self, r: ExtentRef) -> Option<Extent> {
        self.lock().map.resolve(r)
    }

    /// Free bytes available across all free extents.
    pub fn free_bytes(&self) -> usize {
        self.lock().map.free_bytes()
    }

    /// Committed bytes across all extents (stats reconciliation, §20 / plan 07).
    pub fn committed_bytes(&self) -> usize {
        self.lock().map.committed_bytes()
    }

    /// Bytes in each [`ExtentState`] (§20.1) — the dirty/muzzy/released breakdown
    /// that reconciles in stats (plan 07, W4-3a). `total()` is the region length;
    /// `committed()` equals [`committed_bytes`](Self::committed_bytes).
    pub fn state_bytes(&self) -> StateBytes {
        self.lock().map.state_bytes()
    }

    /// The §18.2 policy [`flags`](ExtentFlags) on extent `r`, or `None` if stale.
    pub fn extent_flags(&self, r: ExtentRef) -> Option<ExtentFlags> {
        self.lock().map.resolve(r).map(|e| e.flags)
    }

    /// Set the §18.2 policy [`flags`](ExtentFlags) on extent `r` (back-end policy —
    /// e.g. the W11 hugepage filler's per-extent hints). `Stale` if `r` was recycled.
    pub fn set_extent_flags(&self, r: ExtentRef, flags: ExtentFlags) -> Result<(), ExtentError> {
        let g = self.lock();
        if g.map.resolve(r).is_none() {
            return Err(ExtentError::Stale);
        }
        g.map.set_flags(r.id, flags);
        Ok(())
    }

    /// Whether the back-end is well-formed (the W4-5 invariant oracle).
    pub fn check_invariants(&self) -> bool {
        self.lock().map.check_invariants()
    }

    /// Allocate an extent of at least `size` bytes aligned to `align`, committed
    /// and ready to use (M-005: a [`Released`](ExtentState::Released) source extent
    /// is recommitted before it is handed out). `size` is rounded up to a whole
    /// number of pages (overflow-safe, §9.7). Returns a generation-checked
    /// [`ExtentRef`].
    ///
    /// On a provider commit failure the carved extent is returned to the free
    /// index, so the back-end is unchanged (W4-5).
    ///
    /// SPEC-transition: `extent_alloc` + commit (§18.3 / object `* -> Live`, §7.2)
    pub fn alloc(&self, size: usize, align: usize, fit: Fit) -> Result<ExtentRef, ExtentError> {
        self.alloc_z(size, align, fit).map(|(r, _)| r)
    }

    /// [`alloc`](Self::alloc), additionally reporting whether the handed-out extent
    /// is **freshly OS-zeroed** (W15-5 / §26.2/§26.3): `true` iff it was carved from
    /// an **unbacked** source (Reserved or Released — `committed_len == 0`, no
    /// retained data) and then committed by a provider that
    /// [`committed_memory_is_zeroed`](TopoBackingProvider::committed_memory_is_zeroed).
    /// A carve from a retained-dirty/muzzy extent keeps old data ⇒ not zeroed. The
    /// large path uses [`zeroed`](RegionProvenance::zeroed) so `calloc` can skip the
    /// redundant `memset` on a fresh extent, and [`canary`](RegionProvenance::canary)
    /// so a retained, canary-filled extent's reuse is verified (W18-5, §29.6).
    fn alloc_z(
        &self,
        size: usize,
        align: usize,
        fit: Fit,
    ) -> Result<(ExtentRef, RegionProvenance), ExtentError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(ExtentError::InvalidRequest);
        }
        let needed = align_up(size, PAGE_SIZE).ok_or(ExtentError::Overflow)?;
        let notify = self.notifier();
        let g = self.lock();
        // Carve, dispatching each alignment/size-trim split to the §23.2 hook (W10).
        let id = g
            .map
            .carve_in(needed, align, fit, &notify)
            .ok_or(ExtentError::Exhausted)?;
        // Commit the part of the carved extent that is not yet backed (M-005). For
        // a freshly carved extent `committed_len` is `0` (was Reserved/Released) or
        // `len` (was Dirty/Muzzy), so this commits the whole extent or nothing.
        let e = g.map.view(id).expect("just carved");
        // An unbacked source (committed_len == 0) carries no retained data; once
        // committed by a zeroing provider it reads as zero (§26.2). A retained-dirty
        // source (committed_len == len) keeps old data — never zero.
        let from_unbacked = e.committed_len == 0;
        let uncommitted = e.len - e.committed_len;
        if uncommitted > 0 {
            let offset = self.sub_offset(e.base) + e.committed_len;
            if let Err(err) = self.provider.commit(self.region, offset, uncommitted) {
                // Roll back: return the carved extent to the free index in a state
                // consistent with its (unchanged) backing, leaving us well-formed.
                // The coalesce notifies the §23.2 merge hook, so a backing that saw
                // the carve's splits sees the matching re-merge (net zero).
                let recovered = if e.committed_len == e.len {
                    ExtentState::Dirty
                } else {
                    ExtentState::Released
                };
                g.map.free_in(id, recovered, &notify);
                return Err(ExtentError::Backend(err));
            }
            g.map.mark_committed(id);
        }
        debug_assert!(g.map.check_invariants());
        let generation = g.map.view(id).expect("just carved").generation;
        let zeroed = from_unbacked && self.provider.committed_memory_is_zeroed();
        // W18-5: the carved range reads as the verify-on-reuse canary iff it came from
        // a retained, fully-committed, canary-filled source — `mark_committed` above
        // has already cleared the flag if any fresh (non-canary) pages were committed,
        // and `set_state`/`mark_decommitted` cleared it on any muzzy/released source,
        // so the surviving flag is sound. `uncommitted == 0` is belt-and-suspenders.
        let canary = uncommitted == 0 && g.map.extent_canary(id);
        debug_assert!(
            !(zeroed && canary),
            "a region cannot be both zeroed and canary"
        );
        Ok((
            ExtentRef { id, generation },
            RegionProvenance { zeroed, canary },
        ))
    }

    /// The large-allocation path (§18.5, W4-4): page-round `bytes` overflow-safely,
    /// consult the region-cache `hook` for awkward sizes (§18.6), and otherwise
    /// allocate a best-fit extent — **bypassing the small-object slab path
    /// entirely** (this layer never touches size classes, §18.5). Returns the
    /// backing [`Region`] and, when served from the extent manager, the owning
    /// [`ExtentRef`] (a cache-served region is owned by the cache, so its ref is
    /// `None`).
    ///
    /// The third tuple element is the region's [`RegionProvenance`] (W15-5 / W18-5):
    /// `zeroed` for an extent-served region carved fresh from unbacked backing under a
    /// zeroing provider (so `calloc` may skip its `memset`), and `canary` for a
    /// retained, fully-committed, canary-filled extent (so the large path can verify
    /// the use-after-free canary before reuse, §29.6). A **cache-served** region is
    /// conservatively neither (the region cache / hugepage filler may hand back a
    /// reused page whose provenance this layer cannot vouch for) — so `calloc` still
    /// zeroes it and verify-on-reuse is skipped (never a false abort).
    ///
    /// SPEC-transition: `large_allocate` (§18.5)
    pub fn alloc_large(
        &self,
        bytes: usize,
        align: usize,
        hints: Hints,
        hook: &dyn RegionCacheHook,
    ) -> Result<(Region, Option<ExtentRef>, RegionProvenance), ExtentError> {
        if bytes == 0 || !align.is_power_of_two() {
            return Err(ExtentError::InvalidRequest);
        }
        // §9.7 / plan 06 W2-4: round to whole pages without ever wrapping to a
        // smaller region. (The hugepage rounding lands with W11; the hook handles
        // the awkward-size case it exists for.)
        let rounded = align_up(bytes, PAGE_SIZE).ok_or(ExtentError::Overflow)?;
        // §18.6 region cache first refusal for awkward (just-over-a-hugepage) sizes,
        // carrying the placement hints so a hugepage backend can pack by hotness/
        // lifetime (§19.3/§19.5, W11).
        if let Some(region) = hook.try_alloc(rounded, align, hints) {
            // Cache-served: the cache may return a reused page — conservatively
            // not-known-zero and not-known-canary, so `calloc` still zeroes it
            // (correct, just not elided) and verify-on-reuse is skipped.
            return Ok((region, None, RegionProvenance::default()));
        }
        let (r, prov) = self.alloc_z(rounded, align, Fit::Best)?;
        let region = self.region_of(r).expect("just allocated");
        Ok((region, Some(r), prov))
    }

    /// Split the **live** ([`Active`](ExtentState::Active)) extent `r` at
    /// `prefix_len` page-aligned bytes, keeping `[base, base + prefix_len)` in `r`
    /// (its [`ExtentRef`] stays valid — `split` bumps only `split_generation`, not
    /// the recycle `generation`) and returning the tail
    /// `[base + prefix_len, base + len)` as a **fresh, still-`Active`** extent
    /// (§18.3, plan 06 W15-3b — the large/medium in-place shrink).
    ///
    /// The tail is deliberately **not** freed here: the caller owns the
    /// retire-before-free ordering (clear the tail's pagemap entries, shrink the
    /// owning descriptor, *then* [`free`](Self::free) the returned ref), so a reuse
    /// of the eventually-freed tail can never collide with stale pagemap entries —
    /// the same discipline `Allocator::retire_span` follows.
    ///
    /// On **any** failure — `r` stale or not `Active`, `prefix_len` not a nonzero
    /// page multiple strictly inside the extent, or the descriptor-slot pool
    /// exhausted — **nothing is mutated** (`split_in`'s slot pop is the first and
    /// only fallible step) and the error is returned, so the original extent stays
    /// exactly as it was. This is the W4-5 / §25.3 safety guarantee a failed
    /// in-place shrink relies on: it falls back to keeping the allocation whole.
    ///
    /// SPEC-transition: `extent_split` (§18.3 / §33.4 `span_split_preserves_disjointness`)
    pub fn split_tail(&self, r: ExtentRef, prefix_len: usize) -> Result<ExtentRef, ExtentError> {
        let notify = self.notifier();
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        // Only a live allocation can be shrunk in place; a free extent has no owner
        // to keep the prefix for (M-004: never resize a range with no live object).
        if e.state != ExtentState::Active {
            return Err(ExtentError::NotFree);
        }
        // `split_in` validates `prefix_len` (a nonzero page multiple `< len`) and is
        // the sole fallible step — a `None` leaves the back-end untouched (W4-5).
        let tail = g
            .map
            .split_in(r.id, prefix_len, &notify)
            .ok_or(ExtentError::Exhausted)?;
        // The tail inherits the parent's `Active` state and full backing; report a
        // generation-checked ref so the caller's subsequent `free` cannot resolve a
        // stale slot (DD-1 F2).
        let generation = g.map.view(tail).expect("just split").generation;
        debug_assert!(g.map.check_invariants());
        Ok(ExtentRef {
            id: tail,
            generation,
        })
    }

    /// Whether [`grow_in_place`](Self::grow_in_place) could currently extend `r` to
    /// `new_len` — `r` is a live extent and its address-adjacent successor is **free**
    /// and large enough to supply the deficit. A cheap **O(1)** feasibility probe (a
    /// single address-list step) the large path consults to **fail fast** — falling to
    /// a move (`realloc`) or reporting no-grow (`xallocx`) *before* any pagemap work —
    /// so a grow that cannot happen costs nothing (the common "no adjacent free" case).
    /// Advisory and mirrors [`grow_in_place`](Self::grow_in_place)'s precondition: the
    /// grow re-checks under its own lock, so a neighbour that changes between this probe
    /// and the grow is harmless (the grow simply declines).
    pub fn can_grow_in_place(&self, r: ExtentRef, new_len: usize) -> bool {
        let needed = match align_up(new_len, PAGE_SIZE) {
            Some(n) => n,
            None => return false,
        };
        let g = self.lock();
        let e = match g.map.resolve(r) {
            Some(e) => e,
            None => return false,
        };
        if e.state != ExtentState::Active || needed <= e.len {
            return false;
        }
        let additional = needed - e.len;
        match g.map.addr_next(r.id) {
            Some(next_id) => g
                .map
                .view(next_id)
                .is_some_and(|nv| nv.state.is_free() && nv.len >= additional),
            None => false,
        }
    }

    /// Grow the **live** ([`Active`](ExtentState::Active)) extent `r` in place to
    /// `new_len` page-aligned bytes by **absorbing the front of its address-adjacent
    /// free neighbour** (§18.3 grow — the dual of [`split_tail`](Self::split_tail),
    /// and the medium/large in-place grow of §25.2 / W15-3a). `r` keeps its base,
    /// id, and generation (live pointers stay valid); only its length grows.
    ///
    /// Succeeds only when the immediately-following extent is **free** and large
    /// enough to supply the deficit; otherwise the caller must move (`realloc`) or
    /// report no-grow (`xallocx`). On **any** failure — no adjacent free neighbour,
    /// the neighbour too small, a slot-pool-exhausted trim, or a provider commit
    /// failure — **nothing is mutated** (the neighbour trim is rolled back by a
    /// re-coalesce), so the allocation stays exactly as it was (the W4-5 / §25.1
    /// in-place-grow safety guarantee).
    ///
    /// SPEC-transition: `extent_merge` (§18.3 / §33.4 `span_merge_preserves_disjointness`)
    pub fn grow_in_place(&self, r: ExtentRef, new_len: usize) -> Result<(), ExtentError> {
        let needed = align_up(new_len, PAGE_SIZE).ok_or(ExtentError::Overflow)?;
        let notify = self.notifier();
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if e.state != ExtentState::Active {
            return Err(ExtentError::NotFree); // only a live allocation grows in place
        }
        if needed <= e.len {
            return Ok(()); // already large enough (caller pre-checks; defensive no-op)
        }
        let additional = needed - e.len;
        // The deficit must come from the address-adjacent successor, which must be
        // free and cover it. The address list guarantees adjacency (no gaps, §18 tiling).
        let next_id = g.map.addr_next(r.id).ok_or(ExtentError::Exhausted)?;
        let nv = g.map.view(next_id).ok_or(ExtentError::Exhausted)?;
        if !nv.state.is_free() || nv.len < additional {
            return Err(ExtentError::Exhausted);
        }
        debug_assert_eq!(
            e.base + e.len,
            nv.base,
            "addr_next must be adjacent (§18 tiling)"
        );
        // Trim the neighbour down to exactly `additional` (if larger) — the only
        // bookkeeping that needs a slot; a `None` here leaves the back-end untouched
        // (W4-5: the trim's slot pop is its first, only fallible, step).
        if nv.len > additional && g.map.split_in(next_id, additional, &notify).is_none() {
            return Err(ExtentError::Exhausted);
        }
        // Commit the neighbour's uncommitted backing so the grown extent stays fully
        // backed (M-005). On a provider failure, re-coalesce the (still-free) neighbour
        // to undo the trim, leaving the original free geometry — nothing is mutated.
        let ne = g.map.view(next_id).expect("trimmed neighbour");
        let uncommitted = ne.len - ne.committed_len;
        if uncommitted > 0 {
            let offset = self.sub_offset(ne.base) + ne.committed_len;
            if let Err(err) = self.provider.commit(self.region, offset, uncommitted) {
                g.map.coalesce_in(next_id, &notify);
                return Err(ExtentError::Backend(err));
            }
            g.map.mark_committed(next_id);
        }
        // Absorb the now-fully-committed neighbour into the active extent.
        g.map.absorb_next_in(r.id, next_id, &notify);
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// Free extent `r`, applying the retain/unmap policy (§20.5, W4-3b): retain
    /// keeps the backing ([`Dirty`](ExtentState::Dirty)); unmap decommits it
    /// eagerly ([`Released`](ExtentState::Released)). Coalesces with free
    /// neighbours (§18.4). A stale or non-[`Active`](ExtentState::Active) reference
    /// (a double free) is rejected with [`ExtentError::Stale`]/`NotFree` and never
    /// acted on.
    ///
    /// Freeing always succeeds for a live reference: if the eager-decommit provider
    /// call fails under the unmap policy, the extent is simply retained (still
    /// freed, just not decommitted) — well-formed (W4-5).
    ///
    /// SPEC-transition: `extent free` (object `Live -> CentralFree`/`Dirty`, §7.2/§20.1)
    pub fn free(&self, r: ExtentRef) -> Result<(), ExtentError> {
        self.free_canary(r, false)
    }

    /// [`free`](Self::free), recording whether the caller **canary-filled** the
    /// freed bytes (W18-5, §29.6) so a later reuse of these exact retained,
    /// fully-committed bytes can verify the use-after-free canary. The flag only
    /// takes effect on the retain (`Dirty`) path — an eager unmap decommits the
    /// bytes, so there is no canary to preserve.
    pub fn free_canary(&self, r: ExtentRef, canary: bool) -> Result<(), ExtentError> {
        let notify = self.notifier();
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if e.state != ExtentState::Active {
            return Err(ExtentError::NotFree); // double free / not an allocation
        }
        // Each free's coalesce dispatches its §23.2 merge(s) to the hook (W10).
        match self.retain {
            RetainPolicy::Retain => {
                g.map
                    .free_in_canary(r.id, ExtentState::Dirty, &notify, canary);
            }
            RetainPolicy::Unmap => {
                let offset = self.sub_offset(e.base);
                match self.provider.decommit(self.region, offset, e.committed_len) {
                    Ok(()) => {
                        g.map.mark_decommitted(r.id, ExtentState::Active);
                        g.map.free_in(r.id, ExtentState::Released, &notify);
                    }
                    // Decommit failed: retain instead (still a valid free) — the bytes
                    // are still committed, so the canary (if filled) is preserved.
                    Err(_) => {
                        g.map
                            .free_in_canary(r.id, ExtentState::Dirty, &notify, canary);
                    }
                }
            }
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// **Revoke-before-recycle** for arena destroy/drain (§36.6/§36.13, plan 06
    /// W9-6d): revoke the descendant capabilities of `r`'s backing for `arena`,
    /// **then** free (recycle) the extent. §36.6 requires revocation to complete
    /// before backing is returned to a pool that can serve another authority
    /// domain, which is exactly what arena destruction does.
    ///
    /// A revoke failure leaves the extent **allocated and well-formed** and
    /// returns the error *without* freeing it — so the arena-destroy drain
    /// quarantines (§36.13: partial failure ⇒ DRAINING/ERROR_QUARANTINED, never
    /// DESTROYED) rather than recycling backing whose capabilities are still
    /// live. On POSIX `revoke_descendants` is a no-op, so this is exactly
    /// [`free`](Self::free); on the seLe4n capability provider (plan 09) it is
    /// real frame/mapping-capability revocation, with **no change above the seam**.
    ///
    /// SPEC-transition: provider `Unmapped -> Revoked` then recycle (§36.6)
    pub fn free_revoking(&self, r: ExtentRef, arena: ArenaId) -> Result<(), ExtentError> {
        self.free_revoking_canary(r, arena, false)
    }

    /// [`free_revoking`](Self::free_revoking) carrying the W18-5 `canary` provenance
    /// (§29.6) — as [`free_canary`](Self::free_canary), but revoke-before-recycle.
    pub fn free_revoking_canary(
        &self,
        r: ExtentRef,
        arena: ArenaId,
        canary: bool,
    ) -> Result<(), ExtentError> {
        // Resolve the extent's *sub-region* (the granularity revocation acts on),
        // not the whole managed region, so a per-arena extent is revoked precisely.
        let region = self.region_of(r).ok_or(ExtentError::Stale)?;
        self.provider
            .revoke_descendants(arena, region)
            .map_err(ExtentError::Backend)?;
        self.free_canary(r, canary)
    }

    /// Recommit a free extent's backing (M-005): a [`Released`](ExtentState::Released)
    /// extent becomes [`Dirty`](ExtentState::Dirty) again. Idempotent on an
    /// already-backed extent.
    ///
    /// SPEC-transition: `extent_commit` (provider `* -> AllocatorCommitted`, §36.6)
    pub fn commit(&self, r: ExtentRef) -> Result<(), ExtentError> {
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        let uncommitted = e.len - e.committed_len;
        if uncommitted > 0 {
            let offset = self.sub_offset(e.base) + e.committed_len;
            self.provider
                .commit(self.region, offset, uncommitted)
                .map_err(ExtentError::Backend)?;
            g.map.mark_committed(r.id);
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// Decommit a **free** extent's backing (§18.3, M-004): the extent becomes
    /// [`Released`](ExtentState::Released) (recommit before reuse). Rejected with
    /// [`ExtentError::NotFree`] if the extent is still [`Active`](ExtentState::Active)
    /// — the runtime evidence M-004 requires that the range holds no live object.
    ///
    /// SPEC-transition: `extent_decommit` (provider `* -> Unmapped`, §18.3/§36.6)
    pub fn decommit(&self, r: ExtentRef) -> Result<(), ExtentError> {
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if !e.state.is_free() {
            return Err(ExtentError::NotFree); // M-004
        }
        if e.committed_len > 0 {
            let offset = self.sub_offset(e.base);
            self.provider
                .decommit(self.region, offset, e.committed_len)
                .map_err(ExtentError::Backend)?;
            g.map.mark_decommitted(r.id, ExtentState::Released);
        } else {
            g.map.set_state(r.id, ExtentState::Released);
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// W18-4 (§29.5): make the page-aligned sub-range `[addr, addr+len)`
    /// **inaccessible** (a guard page, `accessible == false`) or restore it
    /// read-write, via the provider's [`protect`](TopoBackingProvider::protect).
    /// `addr` must lie in this manager's region. Best-effort: a provider without
    /// page protection no-ops, so a guard is advisory (never load-bearing for
    /// correctness, §2.4).
    pub fn protect_range(
        &self,
        addr: usize,
        len: usize,
        accessible: bool,
    ) -> Result<(), ExtentError> {
        let offset = self.sub_offset(addr);
        self.provider
            .protect(self.region, offset, len, accessible)
            .map_err(ExtentError::Backend)
    }

    /// Lazily purge a **free**, [`Dirty`](ExtentState::Dirty) extent (§20.4): mark
    /// it discardable ([`Muzzy`](ExtentState::Muzzy)); the backing stays mapped and
    /// reuse is still cheap. No-op on a non-dirty extent.
    ///
    /// SPEC-transition: `purge_lazy` (provider `AllocatorDirty -> AllocatorMuzzyOrScrubbed`, §20.4)
    pub fn purge_lazy(&self, r: ExtentRef) -> Result<(), ExtentError> {
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if !e.state.is_free() {
            return Err(ExtentError::NotFree);
        }
        if e.state == ExtentState::Dirty {
            let offset = self.sub_offset(e.base);
            self.provider
                .purge_lazy(self.region, offset, e.len)
                .map_err(ExtentError::Backend)?;
            g.map.set_state(r.id, ExtentState::Muzzy);
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// Forcibly purge a **free, backed** extent's contents now (§20.4): RSS drops
    /// immediately and the extent becomes [`Muzzy`](ExtentState::Muzzy) (still
    /// mapped; a later read faults a fresh page). A **no-op on an unbacked** free
    /// extent ([`Released`](ExtentState::Released)/[`Reserved`](ExtentState::Reserved),
    /// `committed_len == 0`) — nothing is resident to drop, and forcing it to `Muzzy`
    /// would break the state↔backing coupling (mirrors [`purge_lazy`](Self::purge_lazy)'s
    /// no-op off non-`Dirty`). M-004: free extents only.
    ///
    /// SPEC-transition: `purge_forced` (provider `Allocator* -> AllocatorMuzzyOrScrubbed`, §20.4)
    pub fn purge_forced(&self, r: ExtentRef) -> Result<(), ExtentError> {
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if !e.state.is_free() {
            return Err(ExtentError::NotFree);
        }
        // Only a *backed* free extent has resident pages to drop; an unbacked one
        // (Released/Reserved, `committed_len == 0`) is already purged, so forcing it
        // to Muzzy would be a no-op that violates the state↔backing coupling (Muzzy
        // must be backed). Mirror `purge_lazy`, which already no-ops off `Dirty`.
        if e.committed_len > 0 {
            let offset = self.sub_offset(e.base);
            self.provider
                .purge_forced(self.region, offset, e.len)
                .map_err(ExtentError::Backend)?;
            g.map.set_state(r.id, ExtentState::Muzzy);
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }

    /// Release a **free** extent's backing to the OS/provider (§18.3 `extent_release`
    /// / §20.4): revoke descendants (a no-op on POSIX, real capability work on
    /// seLe4n, §36.6) then decommit, leaving the extent
    /// [`Released`](ExtentState::Released) (recommit before reuse, M-005). M-004:
    /// free extents only.
    ///
    /// SPEC-transition: `release` (provider `Unmapped -> Revoked -> RecyclableUntyped`, §36.6/§21)
    pub fn release(&self, r: ExtentRef) -> Result<(), ExtentError> {
        let g = self.lock();
        let e = g.map.resolve(r).ok_or(ExtentError::Stale)?;
        if !e.state.is_free() {
            return Err(ExtentError::NotFree); // M-004
        }
        // Revoke descendants of *this sub-extent* before returning its backing
        // (§36.6: revoke must complete before memory is returned to a pool); a
        // no-op on POSIX, real capability revocation on seLe4n (plan 09 — which is
        // why it gets the sub-extent's region, not the whole one). The whole-region
        // `release` is the provider's unit of return (on `Drop`); a *sub-extent*
        // returns its backing via the page-granular `decommit` while the manager
        // retains the virtual range for recommit (M-005, §20.5 retain).
        let offset = self.sub_offset(e.base);
        let sub = Region {
            base: self.region.base.wrapping_add(offset),
            len: e.len,
        };
        self.provider
            .revoke_descendants(self.arena, sub)
            .map_err(ExtentError::Backend)?;
        if e.committed_len > 0 {
            self.provider
                .decommit(self.region, offset, e.committed_len)
                .map_err(ExtentError::Backend)?;
            g.map.mark_decommitted(r.id, ExtentState::Released);
        } else {
            g.map.set_state(r.id, ExtentState::Released);
        }
        debug_assert!(g.map.check_invariants());
        Ok(())
    }
}

/// A **type-erased view** of an [`ExtentManager`] as a span-extent source (plan 06
/// W10 per-arena hooked regions). The allocator's span path reserves and frees
/// backing extents through this seam so it can route an arena to its **own** backing
/// region (a [`HookProvider`](crate::HookProvider)-backed manager) without being
/// generic over the provider type at the call site: the shared default backend and a
/// per-arena hooked backend are different `ExtentManager<P>` instantiations, but both
/// are `&dyn ExtentBacking`. The seam is exactly the methods the span create/retire
/// and stats paths need; the default backend's behaviour is unchanged (one `dyn`
/// call on the slow span-create path).
pub trait ExtentBacking {
    /// Allocate a committed backing extent of at least `size` bytes at `align`.
    fn alloc(&self, size: usize, align: usize, fit: Fit) -> Result<ExtentRef, ExtentError>;
    /// Free a backing extent (retain/unmap per policy), coalescing.
    fn free(&self, r: ExtentRef) -> Result<(), ExtentError>;
    /// Revoke the extent's descendants for `arena`, **then** free it (§36.6/§36.13).
    fn free_revoking(&self, r: ExtentRef, arena: ArenaId) -> Result<(), ExtentError>;
    /// The backing [`Region`] of `r`, or `None` if stale.
    fn region_of(&self, r: ExtentRef) -> Option<Region>;
    /// The §20.1 physical-state byte breakdown (stats reconciliation, §8.6).
    fn state_bytes(&self) -> StateBytes;
    /// Bytes physically committed across the region.
    fn committed_bytes(&self) -> usize;
    /// Whether the back-end is well-formed (the W4-5 oracle).
    fn check_invariants(&self) -> bool;
}

impl<P: TopoBackingProvider> ExtentBacking for ExtentManager<P> {
    #[inline]
    fn alloc(&self, size: usize, align: usize, fit: Fit) -> Result<ExtentRef, ExtentError> {
        ExtentManager::alloc(self, size, align, fit)
    }
    #[inline]
    fn free(&self, r: ExtentRef) -> Result<(), ExtentError> {
        ExtentManager::free(self, r)
    }
    #[inline]
    fn free_revoking(&self, r: ExtentRef, arena: ArenaId) -> Result<(), ExtentError> {
        ExtentManager::free_revoking(self, r, arena)
    }
    #[inline]
    fn region_of(&self, r: ExtentRef) -> Option<Region> {
        ExtentManager::region_of(self, r)
    }
    #[inline]
    fn state_bytes(&self) -> StateBytes {
        ExtentManager::state_bytes(self)
    }
    #[inline]
    fn committed_bytes(&self) -> usize {
        ExtentManager::committed_bytes(self)
    }
    #[inline]
    fn check_invariants(&self) -> bool {
        ExtentManager::check_invariants(self)
    }
}

impl<P: TopoBackingProvider> Drop for ExtentManager<P> {
    fn drop(&mut self) {
        // Return the whole reserved region to the provider — unless an explicit
        // `teardown` already did (then this is a no-op, so the region is released
        // exactly once). A failure here cannot be reported from `drop`, but providers
        // must leave their state well-formed (§36.6); the metadata-backed slot pool
        // is monotonic and simply goes away with its arena. Callers that must observe
        // a release failure use `teardown` (W10).
        if !self.released {
            let _ = self.provider.release(self.arena, self.region);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use std::alloc::{alloc as host_alloc, dealloc, Layout};
    use std::sync::atomic::{AtomicU32, Ordering as O};
    use std::sync::Mutex;

    const PAGE: usize = PAGE_SIZE;

    /// A leaked heap metadata arena (the `pagemap`/`span` test pattern), valid for
    /// the process so vended slot pools outlive every extent map built over them.
    fn meta(bytes: usize) -> &'static BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
        Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
    }

    /// Build an `ExtentMap` over a synthetic page-aligned region (no real bytes —
    /// the map only does address bookkeeping). 4096 slots is ample for the tests.
    fn map(base: usize, pages: usize) -> ExtentMap {
        ExtentMap::new(meta(1 << 20), base, pages * PAGE, 4096).expect("extent map")
    }

    // --- a host-backed test provider (with failure injection, W4-5) ----------

    /// A real-memory provider for `ExtentManager` tests: `reserve` hands out a host
    /// allocation (already writable), the physical-state ops model the documented
    /// POSIX mapping (decommit/purge zero the range like `MADV_DONTNEED`), and any
    /// op can be made to fail once to exercise the well-formed-on-failure path.
    struct HostProvider {
        owned: Mutex<Vec<(usize, Layout)>>,
        fail_commit: AtomicU32,
        fail_decommit: AtomicU32,
        commits: AtomicU32,
        decommits: AtomicU32,
        revokes: AtomicU32,
    }

    impl HostProvider {
        fn new() -> Self {
            Self {
                owned: Mutex::new(Vec::new()),
                fail_commit: AtomicU32::new(0),
                fail_decommit: AtomicU32::new(0),
                commits: AtomicU32::new(0),
                decommits: AtomicU32::new(0),
                revokes: AtomicU32::new(0),
            }
        }
        fn fail_next_commit(&self) {
            self.fail_commit.store(1, O::Relaxed);
        }
        fn fail_next_decommit(&self) {
            self.fail_decommit.store(1, O::Relaxed);
        }
    }

    impl TopoBackingProvider for HostProvider {
        fn reserve(
            &self,
            _arena: ArenaId,
            size: usize,
            align: usize,
        ) -> Result<Region, BackendError> {
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
            if self.fail_decommit.swap(0, O::Relaxed) == 1 {
                return Err(BackendError::OutOfMemory);
            }
            // Model MADV_DONTNEED: discard the contents now (a later read faults a
            // fresh zero page). Bounds: `offset + len <= region.len`.
            // SAFETY: `[offset, offset+len)` is in bounds of the committed region.
            unsafe { ptr::write_bytes(region.base.add(offset), 0, len) };
            Ok(())
        }
        fn purge_forced(
            &self,
            region: Region,
            offset: usize,
            len: usize,
        ) -> Result<(), BackendError> {
            // SAFETY: in-bounds sub-range of the committed region.
            unsafe { ptr::write_bytes(region.base.add(offset), 0, len) };
            Ok(())
        }
        fn revoke_descendants(&self, _a: ArenaId, _r: Region) -> Result<(), BackendError> {
            self.revokes.fetch_add(1, O::Relaxed);
            Ok(())
        }
        fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
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
            "host-test"
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

    fn manager(pages: usize) -> ExtentManager<HostProvider> {
        let mut m = ExtentManager::new(
            HostProvider::new(),
            meta(1 << 20),
            ArenaId::DEFAULT,
            pages * PAGE,
            PAGE,
            4096,
        )
        .expect("manager");
        // Pin the retain policy so the lifecycle tests are deterministic regardless of
        // the build profile (under `--features low-rss`/`debug`, `from_profile` would
        // otherwise default to Unmap and flip `free` semantics). Profile *selection*
        // is covered separately by `retain_policy_follows_the_build_profile`.
        m.set_retain_policy(RetainPolicy::Retain);
        m
    }

    // === ExtentMap (pure bookkeeping) ====================================

    #[test]
    fn new_tiles_region_as_one_reserved_extent() {
        let m = map(0x4000_0000, 8);
        assert!(m.check_invariants());
        assert_eq!(m.free_bytes(), 8 * PAGE);
        assert_eq!(m.committed_bytes(), 0);
        let (lo, hi) = m.region();
        assert_eq!(hi - lo, 8 * PAGE);
    }

    #[test]
    fn carve_exact_marks_active_and_empties_free() {
        let mut m = map(0x4000_0000, 4);
        let id = m.carve(4 * PAGE, PAGE, Fit::Best).expect("carve all");
        let e = m.view(id).unwrap();
        assert_eq!(e.state, ExtentState::Active);
        assert_eq!(e.len, 4 * PAGE);
        assert_eq!(m.free_bytes(), 0);
        assert!(m.check_invariants());
    }

    #[test]
    fn carve_splits_off_remainder_and_tiles() {
        let mut m = map(0x4000_0000, 8);
        let id = m.carve(3 * PAGE, PAGE, Fit::Best).expect("carve 3");
        let e = m.view(id).unwrap();
        assert_eq!(e.len, 3 * PAGE);
        assert_eq!(e.base, 0x4000_0000);
        // The remainder (5 pages) is still free.
        assert_eq!(m.free_bytes(), 5 * PAGE);
        assert!(m.check_invariants());
    }

    /// `(base, total_len, prefix_len, backed)` of the last reported split.
    type SplitGeom = (usize, usize, usize, bool);
    /// `(left_base, left_len, right_base, right_len, backed)` of the last merge.
    type MergeGeom = (usize, usize, usize, usize, bool);

    /// A counting [`ExtentNotify`] for the W10 notification-threading tests:
    /// records every split/merge the bookkeeping reports, plus the last geometry.
    #[derive(Default)]
    struct CountingNotify {
        splits: AtomicU32,
        merges: AtomicU32,
        last_split: Mutex<Option<SplitGeom>>,
        last_merge: Mutex<Option<MergeGeom>>,
    }
    impl ExtentNotify for CountingNotify {
        fn on_split(&self, base: usize, total_len: usize, prefix_len: usize, backed: bool) {
            self.splits.fetch_add(1, O::Relaxed);
            *self.last_split.lock().unwrap() = Some((base, total_len, prefix_len, backed));
        }
        fn on_merge(&self, lb: usize, ll: usize, rb: usize, rl: usize, backed: bool) {
            self.merges.fetch_add(1, O::Relaxed);
            *self.last_merge.lock().unwrap() = Some((lb, ll, rb, rl, backed));
        }
    }

    #[test]
    fn carve_in_and_free_in_notify_each_split_and_merge() {
        // W10: `carve_in` notifies the §23.2 split hook for the tail trim, and
        // `free_in`'s coalesce notifies the merge hook when the freed extent rejoins
        // its neighbour — with the correct pre-split / pre-merge geometry.
        let n = CountingNotify::default();
        let base = 0x4000_0000;
        let mut m = map(base, 8);
        // Carve 3 pages: trims a 5-page tail ⇒ exactly one split notification.
        let id = m.carve_in(3 * PAGE, PAGE, Fit::Best, &n).expect("carve");
        assert_eq!(n.splits.load(O::Relaxed), 1, "tail trim is one split");
        assert_eq!(
            *n.last_split.lock().unwrap(),
            Some((base, 8 * PAGE, 3 * PAGE, false)),
            "pre-split geometry: the 8-page extent cut at 3 pages, unbacked"
        );
        assert_eq!(n.merges.load(O::Relaxed), 0);
        // Free the 3-page extent: it coalesces back with the 5-page free tail ⇒ one
        // merge notification, with the pre-merge geometry (3-page left, 5-page right).
        m.free_in(id, ExtentState::Released, &n).expect("free");
        assert_eq!(
            n.merges.load(O::Relaxed),
            1,
            "rejoining the tail is one merge"
        );
        assert_eq!(
            *n.last_merge.lock().unwrap(),
            Some((base, 3 * PAGE, base + 3 * PAGE, 5 * PAGE, false))
        );
        assert!(m.check_invariants());
        assert_eq!(m.free_bytes(), 8 * PAGE, "whole region free again");
    }

    #[test]
    fn carve_in_over_alignment_notifies_prefix_and_tail_splits() {
        // W10: an over-aligned carve trims a page-aligned prefix *and* a size tail —
        // two distinct §23.2 split notifications.
        let n = CountingNotify::default();
        let base = PAGE; // region base not 4*PAGE-aligned ⇒ a prefix trim is needed
        let mut m = map(base, 16);
        let _id = m
            .carve_in(2 * PAGE, 4 * PAGE, Fit::Best, &n)
            .expect("aligned carve");
        assert_eq!(
            n.splits.load(O::Relaxed),
            2,
            "prefix trim + tail trim = two splits"
        );
        assert!(m.check_invariants());
    }

    #[test]
    fn carve_with_overalignment_splits_a_page_aligned_prefix() {
        // A 4*PAGE alignment forces an aligned base above the region base, so a
        // page-aligned prefix is split off (W4-2b: every split page-aligned).
        let base = PAGE; // region base not 4*PAGE-aligned
        let mut m = map(base, 16);
        let big_align = 4 * PAGE;
        let id = m
            .carve(2 * PAGE, big_align, Fit::Best)
            .expect("aligned carve");
        let e = m.view(id).unwrap();
        assert_eq!(e.base % big_align, 0, "allocation honors the alignment");
        assert_eq!(e.base % PAGE, 0, "still page-aligned");
        assert_eq!(e.len, 2 * PAGE);
        assert!(m.check_invariants());
    }

    #[test]
    fn split_rejects_unaligned_zero_and_out_of_bounds() {
        let mut m = map(0x4000_0000, 4);
        let id = ExtentId(m.addr_head);
        assert!(m.split(id, 0).is_none(), "zero-length prefix");
        assert!(m.split(id, 4 * PAGE).is_none(), "whole-length (no tail)");
        assert!(m.split(id, PAGE + 1).is_none(), "non-page-aligned");
        // None of the failed splits mutated the map.
        assert!(m.check_invariants());
        assert_eq!(m.free_bytes(), 4 * PAGE);
    }

    #[test]
    fn split_halves_tile_the_parent() {
        // Mirrors Theorems/Span.lean `split_halves_disjoint`: the two halves tile
        // the parent with no gap or overlap.
        let mut m = map(0x4000_0000, 8);
        let left = ExtentId(m.addr_head);
        let parent = m.view(left).unwrap();
        let right = m.split(left, 3 * PAGE).expect("split");
        let l = m.view(left).unwrap();
        let r = m.view(right).unwrap();
        assert_eq!(l.base, parent.base);
        assert_eq!(l.len, 3 * PAGE);
        assert_eq!(r.base, parent.base + 3 * PAGE);
        assert_eq!(l.end(), r.base, "no gap");
        assert_eq!(r.end(), parent.end(), "covers the parent");
        assert!(m.check_invariants());
    }

    #[test]
    fn merge_coalesces_adjacent_free_extents() {
        let mut m = map(0x4000_0000, 8);
        let left = ExtentId(m.addr_head);
        let right = m.split(left, 3 * PAGE).expect("split");
        assert!(m.merge(left, right), "adjacent free extents merge");
        let e = m.view(left).unwrap();
        assert_eq!(e.len, 8 * PAGE, "merged back to the whole region");
        assert!(m.view(right).is_none(), "right slot retired");
        assert!(m.check_invariants());
    }

    #[test]
    fn merge_refuses_non_adjacent_active_or_cross_state() {
        let mut m = map(0x4000_0000, 8);
        let a = ExtentId(m.addr_head);
        let b = m.split(a, 4 * PAGE).expect("split");
        // Make `a` Active (carve it): now merge(a,b) must refuse (a is not free).
        let act = m.carve(4 * PAGE, PAGE, Fit::First).expect("carve a");
        assert!(!m.merge(act, b), "never merge a live (Active) range");
        // Non-adjacent: free `act`, split `b`, and try to merge the two non-touching
        // free extents — refused (adjacency gate).
        assert!(m.check_invariants());
    }

    #[test]
    fn coalesce_absorbs_both_free_neighbours() {
        let mut m = map(0x4000_0000, 9);
        // Carve three adjacent 3-page extents, free the outer two, then free the
        // middle — coalesce must merge all three into one.
        let a = m.carve(3 * PAGE, PAGE, Fit::First).unwrap();
        let b = m.carve(3 * PAGE, PAGE, Fit::First).unwrap();
        let c = m.carve(3 * PAGE, PAGE, Fit::First).unwrap();
        assert_eq!(m.free_bytes(), 0);
        // The pure map never commits (that is the manager's job), so a carved extent
        // is unbacked: free it to the unbacked free state, `Released` (a Dirty free
        // extent would have to be backed). Coalescing across same-backing states is
        // exercised here; the backed (`Dirty`) path is covered by the manager tests.
        m.free(a, ExtentState::Released);
        m.free(c, ExtentState::Released);
        let survivor = m.free(b, ExtentState::Released).unwrap();
        let e = m.view(survivor).unwrap();
        assert_eq!(e.len, 9 * PAGE, "all three coalesced");
        assert_eq!(m.free_bytes(), 9 * PAGE);
        assert!(m.check_invariants());
    }

    #[test]
    fn best_fit_picks_the_smallest_adequate_extent() {
        let mut m = map(0x4000_0000, 16);
        // Carve to leave free extents of 2 and 4 pages (and an active gap between).
        let _a = m.carve(2 * PAGE, PAGE, Fit::First).unwrap(); // [0,2) active
        let hold2 = m.carve(2 * PAGE, PAGE, Fit::First).unwrap(); // [2,4)
        let _b = m.carve(4 * PAGE, PAGE, Fit::First).unwrap(); // [4,8) active
        let hold4 = m.carve(4 * PAGE, PAGE, Fit::First).unwrap(); // [8,12)
                                                                  // Free the 2-page and 4-page holds, leaving free extents of size 2 and 4.
                                                                  // (Unbacked carves free to `Released`; see `coalesce_absorbs_both_free_neighbours`.)
        m.free(hold2, ExtentState::Released);
        m.free(hold4, ExtentState::Released);
        // Best-fit a 2-page request: must pick the 2-page extent, not the 4-page one.
        let got = m.carve(2 * PAGE, PAGE, Fit::Best).unwrap();
        assert_eq!(m.view(got).unwrap().base, 0x4000_0000 + 2 * PAGE);
        assert!(m.check_invariants());
    }

    #[test]
    fn slot_exhaustion_fails_safely() {
        // A tiny pool: after the initial extent, only a couple of splits are possible.
        let mut m = ExtentMap::new(meta(1 << 16), 0x4000_0000, 64 * PAGE, 3).expect("map");
        // Each non-exact carve consumes a slot for the remainder; eventually the pool
        // is exhausted and carve returns None without corrupting the map.
        let mut n = 0;
        while m.carve(PAGE, PAGE, Fit::First).is_some() {
            n += 1;
            assert!(m.check_invariants());
            assert!(n < 100, "should exhaust the 3-slot pool quickly");
        }
        assert!(
            m.check_invariants(),
            "exhaustion leaves the map well-formed"
        );
    }

    #[test]
    fn stale_ref_after_merge_resolves_none() {
        // DD-1 F2: a reference captured before a merge that retires the slot must
        // resolve to None (not to the different range now in the slot).
        let mut m = map(0x4000_0000, 8);
        let left = ExtentId(m.addr_head);
        let right = m.split(left, 4 * PAGE).expect("split");
        let stale = m.extent_ref(right).expect("ref to right");
        assert!(m.resolve(stale).is_some());
        assert!(m.merge(left, right), "merge retires right's slot");
        assert!(m.resolve(stale).is_none(), "captured ref is now stale");
    }

    // === ExtentManager (provider integration, W4-2d/3/4/5) ===============

    #[test]
    fn alloc_commits_writable_memory_and_frees() {
        let mgr = manager(16);
        let r = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc");
        let region = mgr.region_of(r).expect("region");
        assert_eq!(region.len, 3 * PAGE);
        assert_eq!(region.base as usize % PAGE, 0);
        // The committed region is writable end to end.
        // SAFETY: `region` is committed for its whole length.
        unsafe {
            for i in 0..region.len {
                region.base.add(i).write(0x5a);
            }
            assert_eq!(region.base.add(region.len - 1).read(), 0x5a);
        }
        assert_eq!(mgr.committed_bytes(), 3 * PAGE);
        mgr.free(r).expect("free");
        assert!(mgr.check_invariants());
    }

    #[test]
    fn alloc_free_reuses_space_under_retain() {
        let mut mgr = manager(8);
        mgr.set_retain_policy(RetainPolicy::Retain);
        let r1 = mgr.alloc(8 * PAGE, PAGE, Fit::Best).unwrap();
        assert_eq!(mgr.free_bytes(), 0);
        mgr.free(r1).unwrap();
        // Retain keeps the backing: the freed extent is Dirty (still committed).
        assert_eq!(mgr.free_bytes(), 8 * PAGE);
        assert_eq!(mgr.committed_bytes(), 8 * PAGE, "retain keeps backing");
        // A re-alloc reuses the space without a fresh commit.
        let r2 = mgr.alloc(8 * PAGE, PAGE, Fit::Best).unwrap();
        assert_eq!(mgr.region_of(r2).unwrap().len, 8 * PAGE);
        assert!(mgr.check_invariants());
    }

    #[test]
    fn free_under_unmap_policy_decommits_backing() {
        let mut mgr = manager(8);
        mgr.set_retain_policy(RetainPolicy::Unmap);
        let r = mgr.alloc(8 * PAGE, PAGE, Fit::Best).unwrap();
        assert_eq!(mgr.committed_bytes(), 8 * PAGE);
        mgr.free(r).unwrap();
        // Unmap decommits eagerly: the backing is returned (committed bytes drop).
        assert_eq!(mgr.committed_bytes(), 0, "unmap decommits on free");
        assert_eq!(mgr.free_bytes(), 8 * PAGE);
        assert!(mgr.check_invariants());
    }

    #[test]
    fn decommit_and_release_require_a_free_extent_m004() {
        let mgr = manager(8);
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::Best).unwrap();
        // M-004: an Active extent may hold a live object — decommit/release refused.
        assert_eq!(mgr.decommit(r), Err(ExtentError::NotFree));
        assert_eq!(mgr.release(r), Err(ExtentError::NotFree));
        assert_eq!(mgr.purge_lazy(r), Err(ExtentError::NotFree));
        assert!(mgr.check_invariants());
    }

    #[test]
    fn purge_forced_on_unbacked_released_extent_is_a_safe_noop() {
        // Regression: `purge_forced` on an *unbacked* free extent (Released,
        // `committed_len == 0`) must be a no-op. Forcing it to Muzzy would claim it is
        // backed (Muzzy ⇒ backed) while `committed_len == 0`, violating the
        // state↔backing coupling and tripping `check_invariants` — and it has no
        // resident pages to drop anyway. Mirrors `purge_lazy`'s no-op off non-Dirty.
        let mgr = manager(8);
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(b).unwrap();
        mgr.release(b).unwrap(); // b: Released, committed_len 0
        assert_eq!(mgr.view(b).unwrap().state, ExtentState::Released);
        mgr.purge_forced(b).unwrap(); // no-op: unbacked
        assert_eq!(mgr.view(b).unwrap().state, ExtentState::Released);
        assert_eq!(mgr.view(b).unwrap().committed_len, 0);
        assert!(mgr.check_invariants());
        let _ = a;
    }

    #[test]
    fn extent_state_transition_matches_lean() {
        use ExtentState::*;
        const STATES: [ExtentState; 5] = [Reserved, Active, Dirty, Muzzy, Released];
        // The canonical legal §20.1 directed edges (non-reflexive) — the SAME list the
        // Lean `ExtentState.extentEdges` source of truth encodes
        // (lean/TopoMalloc/ExtentState.lean), pinned by the `lake exe check`
        // `extentStateGate`. `can_transition` must be exactly reflexivity ∪ these
        // edges; if the runtime relation and the Lean model drift, one side fails (the
        // §20.1 analogue of `provider_next_matches_the_36_6_chain_exactly`, W4-2d).
        const EDGES: &[(ExtentState, ExtentState)] = &[
            (Reserved, Active),
            (Dirty, Active),
            (Muzzy, Active),
            (Released, Active),
            (Active, Dirty),
            (Active, Released),
            (Reserved, Dirty),
            (Released, Dirty),
            (Dirty, Muzzy),
            (Reserved, Released),
            (Dirty, Released),
            (Muzzy, Released),
        ];
        for &from in &STATES {
            for &to in &STATES {
                let expected = from == to || EDGES.contains(&(from, to));
                assert_eq!(
                    from.can_transition(to),
                    expected,
                    "transition {from:?} -> {to:?} disagrees with the canonical §20.1 relation"
                );
            }
        }
    }

    #[test]
    fn retain_policy_follows_the_build_profile() {
        // W4-3b: retain by default on a 64-bit performance build; unmap more
        // aggressively under the `debug`/`low-rss` profiles or on a 32-bit
        // (address-space-scarce) target. CI runs this both with and without
        // `--features low-rss`, so the `from_profile` *selection* is exercised — not
        // just the manually-set policy the lifecycle tests use.
        let p = RetainPolicy::from_profile();
        if cfg!(any(feature = "debug", feature = "low-rss")) || cfg!(target_pointer_width = "32") {
            assert_eq!(p, RetainPolicy::Unmap, "aggressive-unmap profile");
        } else {
            assert_eq!(p, RetainPolicy::Retain, "64-bit performance default");
        }
        // `ExtentManager::new` adopts the profile policy by default. (The `manager`
        // helper overrides it to Retain for determinism, so build one directly here.)
        let mgr_default = ExtentManager::new(
            HostProvider::new(),
            meta(1 << 20),
            ArenaId::DEFAULT,
            8 * PAGE,
            PAGE,
            4096,
        )
        .expect("manager");
        assert_eq!(mgr_default.retain_policy(), p);
    }

    #[test]
    fn new_rejects_a_region_size_that_would_overflow() {
        // §9.7: the managed region is page-rounded; a size whose page-rounded length
        // would wrap `usize` is rejected (Overflow), never silently truncated to a
        // smaller region. No reservation happens before the check, so nothing leaks.
        let r = ExtentManager::new(
            HostProvider::new(),
            meta(1 << 16),
            ArenaId::DEFAULT,
            usize::MAX,
            PAGE,
            4096,
        );
        assert!(matches!(r, Err(ExtentError::Overflow)));
    }

    #[test]
    fn set_extent_flags_on_a_stale_ref_is_rejected() {
        let mgr = manager(8);
        let r = mgr.alloc(2 * PAGE, PAGE, Fit::First).unwrap();
        // A ref with a mismatched generation is stale by construction (DD-1 F2): the
        // flag write must be refused and have no effect.
        let stale = ExtentRef {
            id: r.id,
            generation: r.generation.wrapping_add(1),
        };
        assert_eq!(
            mgr.set_extent_flags(stale, ExtentFlags(0b1)),
            Err(ExtentError::Stale)
        );
        // The live ref still works; the stale attempt left the flags untouched.
        mgr.set_extent_flags(r, ExtentFlags(0b1)).expect("live ref");
        assert_eq!(mgr.extent_flags(r), Some(ExtentFlags(0b1)));
        assert!(mgr.check_invariants());
    }

    #[test]
    fn alloc_recommits_a_released_extent_before_reuse_m005() {
        let mgr = manager(8);
        // Split off two halves so a freed half can be released, then re-allocated.
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(b).unwrap();
        mgr.release(b).unwrap(); // b's backing returned; state Released, committed 0
        assert_eq!(mgr.view(b).unwrap().state, ExtentState::Released);
        assert_eq!(mgr.committed_bytes(), 4 * PAGE);
        // Re-allocating that 4-page hole must recommit it (M-005) and hand back
        // writable memory.
        let c = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        assert_eq!(
            mgr.committed_bytes(),
            8 * PAGE,
            "released space recommitted"
        );
        let region = mgr.region_of(c).unwrap();
        // SAFETY: committed for its whole length (the recommit faulted it in).
        unsafe {
            region.base.write(0x42);
            assert_eq!(region.base.read(), 0x42);
        }
        let _ = a;
        assert!(mgr.check_invariants());
    }

    #[test]
    fn explicit_release_decommits_a_free_extent_and_recommit_restores() {
        let mgr = manager(8);
        // Carve two halves so one can be freed and explicitly released while a ref to
        // the free half is retained for the release/commit calls.
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(b).unwrap(); // b is now Dirty (retain); ref `b` still resolves
        let committed_before = mgr.committed_bytes();
        assert_eq!(committed_before, 8 * PAGE);
        // Explicitly release b's backing to the OS (decommit + revoke).
        mgr.release(b).unwrap();
        assert_eq!(mgr.committed_bytes(), 4 * PAGE, "b's 4 pages decommitted");
        assert_eq!(mgr.view(b).unwrap().state, ExtentState::Released);
        // Recommit before reuse (M-005).
        mgr.commit(b).unwrap();
        assert_eq!(mgr.committed_bytes(), 8 * PAGE, "recommitted");
        assert_eq!(mgr.view(b).unwrap().state, ExtentState::Dirty);
        let _ = a;
        assert!(mgr.check_invariants());
    }

    #[test]
    fn purge_lazy_then_forced_track_muzzy() {
        let mgr = manager(8);
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(r).unwrap(); // Dirty
        mgr.purge_lazy(r).unwrap();
        assert_eq!(mgr.view(r).unwrap().state, ExtentState::Muzzy);
        // Forced purge on a muzzy extent stays muzzy (contents discarded now).
        mgr.purge_forced(r).unwrap();
        assert_eq!(mgr.view(r).unwrap().state, ExtentState::Muzzy);
        // Still backed (mapped) — committed bytes unchanged by a purge.
        assert_eq!(mgr.committed_bytes(), 4 * PAGE);
        assert!(mgr.check_invariants());
    }

    #[test]
    fn purge_lazy_is_idempotent_on_muzzy_and_rejects_live() {
        let mgr = manager(8);
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        // M-004: purging a *live* (Active) extent is refused and never acted on.
        assert!(matches!(mgr.purge_lazy(r), Err(ExtentError::NotFree)));
        assert!(matches!(mgr.purge_forced(r), Err(ExtentError::NotFree)));
        assert_eq!(mgr.view(r).unwrap().state, ExtentState::Active);
        // Free → Dirty → purge_lazy → Muzzy; a second purge_lazy is the documented
        // idempotent no-op ("No-op on a non-dirty extent"), still Muzzy, still backed.
        mgr.free(r).unwrap();
        mgr.purge_lazy(r).unwrap();
        assert_eq!(mgr.view(r).unwrap().state, ExtentState::Muzzy);
        mgr.purge_lazy(r).unwrap();
        assert_eq!(mgr.view(r).unwrap().state, ExtentState::Muzzy);
        assert_eq!(mgr.committed_bytes(), 4 * PAGE);
        assert!(mgr.check_invariants());
    }

    #[test]
    fn double_free_is_rejected_not_acted_on() {
        let mgr = manager(8);
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(r).unwrap();
        // The second free sees a non-Active (now Dirty/coalesced) extent: rejected.
        assert!(matches!(
            mgr.free(r),
            Err(ExtentError::NotFree) | Err(ExtentError::Stale)
        ));
        assert!(mgr.check_invariants());
    }

    #[test]
    fn alloc_large_rounds_and_bypasses_with_region_cache_hook() {
        let mgr = manager(64);
        // An "awkward" size (just over 2 pages) rounds up to whole pages, no wrap.
        let (region, r, prov) = mgr
            .alloc_large(2 * PAGE + 1, PAGE, Hints::default(), &NoRegionCache)
            .expect("large");
        assert_eq!(region.len, 3 * PAGE, "rounded up to whole pages");
        assert!(r.is_some(), "served from the extent manager (no cache)");
        // The host test provider does not promise zeroed commits, so the extent is
        // conservatively reported not-known-zero (calloc would memset it). A fresh
        // carve from an unbacked source holds no canary either.
        assert!(
            !prov.zeroed,
            "default test provider does not opt into committed_memory_is_zeroed"
        );
        assert!(!prov.canary, "a fresh (never-freed) extent holds no canary");
        assert!(mgr.check_invariants());
    }

    // --- W18-5 verify-on-reuse provenance (the `canary` flag invariant) ---------

    #[test]
    fn retained_canary_extent_reports_canary_provenance() {
        // A canary-filled free-to-Dirty extent, reused, reports `canary` provenance —
        // the signal the large path uses to verify the use-after-free canary (§29.6).
        let mgr = manager(8); // Retain policy: free keeps the extent Dirty
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free_canary(r, true).unwrap();
        let (_r2, prov) = mgr.alloc_z(4 * PAGE, PAGE, Fit::First).unwrap();
        assert!(
            prov.canary,
            "a retained, canary-filled extent is reuse-verifiable"
        );
        assert!(!prov.zeroed, "a canary region is never reported zeroed");
        assert!(mgr.check_invariants());
    }

    #[test]
    fn free_without_canary_is_not_reuse_verifiable() {
        // The default (un-filled) free path never claims the canary, so a reuse is
        // never (falsely) verified — the conservative default that keeps a correct
        // program from being aborted.
        let mgr = manager(8);
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free(r).unwrap(); // canary = false
        let (_r2, prov) = mgr.alloc_z(4 * PAGE, PAGE, Fit::First).unwrap();
        assert!(
            !prov.canary,
            "an un-filled free is not verify-on-reuse eligible"
        );
    }

    #[test]
    fn purge_to_muzzy_clears_the_canary() {
        // A lazy purge (dirty→muzzy, MADV_FREE) may let the kernel reclaim/zero the
        // pages, so the canary is no longer reliable — the flag must clear, or a reuse
        // would false-abort. This is the invalidation that makes a stale bit impossible.
        // Fill the region (A then B) so freeing A coalesces with nothing and its ref
        // stays valid for the purge.
        let mgr = manager(8);
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        let _b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        mgr.free_canary(a, true).unwrap(); // a: Dirty + canary, no free neighbour
        mgr.purge_lazy(a).unwrap(); // dirty → muzzy: clears the canary
        let (_r2, prov) = mgr.alloc_z(4 * PAGE, PAGE, Fit::First).unwrap();
        assert!(
            !prov.canary,
            "a muzzy (purged) extent is not reuse-verifiable"
        );
        assert!(mgr.check_invariants());
    }

    #[test]
    fn merge_with_a_noncanary_neighbour_clears_the_canary() {
        // The AND-join on coalesce: a canary extent merged with a non-canary neighbour
        // yields a non-canary range (the merged region is not uniformly the canary), so
        // verify-on-reuse over the whole survivor is correctly skipped.
        let mgr = manager(16);
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap(); // low
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap(); // adjacent, higher
        mgr.free_canary(a, true).unwrap(); // a: Dirty + canary (b still Active, no merge)
        mgr.free(b).unwrap(); // b: Dirty, no canary → coalesces with a (AND-join → 0)
                              // Reuse the merged 8-page extent: not uniformly canary ⇒ not verifiable.
        let (_r, prov) = mgr.alloc_z(8 * PAGE, PAGE, Fit::First).unwrap();
        assert!(
            !prov.canary,
            "a merge with a non-canary neighbour taints the canary join"
        );
        assert!(mgr.check_invariants());
    }

    #[test]
    fn commit_failure_leaves_state_well_formed_w4_5() {
        let mgr = manager(8);
        // `mod tests` is a child of `extent`, so it reaches the private provider
        // field to arm the one-shot failure injection.
        mgr.provider.fail_next_commit();
        // The first alloc must commit (the region is Reserved); the injected failure
        // makes it fail — and roll the carve back, leaving the back-end untouched.
        let before_free = mgr.free_bytes();
        assert!(matches!(
            mgr.alloc(4 * PAGE, PAGE, Fit::Best),
            Err(ExtentError::Backend(BackendError::OutOfMemory))
        ));
        assert_eq!(mgr.free_bytes(), before_free, "carve rolled back");
        assert_eq!(mgr.committed_bytes(), 0);
        assert!(
            mgr.check_invariants(),
            "failed alloc keeps invariants green"
        );
        // The back-end still works after the injected failure.
        let r = mgr.alloc(4 * PAGE, PAGE, Fit::Best).expect("recovers");
        assert!(mgr.view(r).is_some());
        assert!(mgr.check_invariants());
    }

    #[test]
    fn decommit_failure_leaves_state_well_formed_w4_5() {
        // W4-5: a provider decommit failure must leave the back-end well-formed.
        // Two halves so a freed extent stays distinct (its Active neighbour prevents
        // a coalesce that would stale its ref).
        let mut mgr = manager(8);
        mgr.set_retain_policy(RetainPolicy::Unmap);
        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap();

        // (1) Free under Unmap attempts decommit; the injected failure makes the
        // manager fall back to retaining `b` (still a clean free).
        mgr.provider.fail_next_decommit();
        mgr.free(b)
            .expect("free still succeeds (falls back to retain)");
        assert!(
            mgr.check_invariants(),
            "decommit failure keeps invariants green"
        );
        assert_eq!(
            mgr.committed_bytes(),
            8 * PAGE,
            "decommit failed ⇒ backing retained"
        );

        // (2) `b` is now a free Dirty extent (its left neighbour `a` is Active, so it
        // did not coalesce — its ref is still valid). An explicit decommit that hits
        // the injected failure is surfaced as Backend(..) and changes nothing.
        let committed_before = mgr.committed_bytes();
        mgr.provider.fail_next_decommit();
        assert!(matches!(
            mgr.decommit(b),
            Err(ExtentError::Backend(BackendError::OutOfMemory))
        ));
        assert_eq!(
            mgr.committed_bytes(),
            committed_before,
            "failed decommit changed nothing"
        );
        assert!(mgr.check_invariants());
        let _ = a;
    }

    #[test]
    fn state_bytes_reconcile_with_region_and_committed() {
        // W4-3a "states reconcile in stats": the per-state byte breakdown tiles the
        // region and its committed sum matches committed_bytes(), across a mix of
        // reserved / active / dirty extents.
        let mgr = manager(16);
        let total = 16 * PAGE;
        // All reserved initially (committed 0).
        let sb0 = mgr.state_bytes();
        assert_eq!(sb0.total(), total);
        assert_eq!(sb0.reserved, total);
        assert_eq!(sb0.committed(), 0);

        let a = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap(); // Active, committed
        let b = mgr.alloc(4 * PAGE, PAGE, Fit::First).unwrap(); // Active, committed
        let sb = mgr.state_bytes();
        assert_eq!(sb.total(), total, "state bytes always tile the region");
        assert_eq!(sb.committed(), mgr.committed_bytes(), "committed agrees");
        assert_eq!(sb.active, 8 * PAGE);
        assert_eq!(sb.reserved, 8 * PAGE); // the un-carved remainder

        mgr.free(a).unwrap(); // retain (perf default) → Dirty, still committed
        let sb2 = mgr.state_bytes();
        assert_eq!(sb2.total(), total);
        assert_eq!(sb2.committed(), mgr.committed_bytes());
        assert_eq!(sb2.dirty, 4 * PAGE, "a is now dirty (still backed)");
        assert_eq!(sb2.active, 4 * PAGE, "b stays active");
        let _ = b;
        assert!(mgr.check_invariants());
    }

    #[test]
    fn extent_flags_round_trip() {
        // §18.2 `flags` (C12): the back-end policy bits set/get on a live extent and
        // survive; a split inherits the parent's flags.
        let mgr = manager(16);
        let r = mgr.alloc(8 * PAGE, PAGE, Fit::Best).unwrap();
        assert_eq!(mgr.extent_flags(r), Some(ExtentFlags::NONE));
        mgr.set_extent_flags(r, ExtentFlags(0b1011))
            .expect("set flags");
        assert_eq!(mgr.extent_flags(r), Some(ExtentFlags(0b1011)));
        mgr.free(r).unwrap();
        assert!(mgr.check_invariants());
    }

    #[test]
    fn concurrent_alloc_free_keeps_invariants_and_is_disjoint() {
        // C9: the §27.2 backend lock makes the manager `Sync`. Hammer a shared
        // manager from several threads doing alloc/free; the back-end must stay
        // well-formed and never hand two threads overlapping live extents. Each
        // thread stamps its allocations and checks them, so any aliasing is caught.
        use std::sync::Arc;
        let mgr = Arc::new(manager(512));
        std::thread::scope(|s| {
            for t in 0..6u8 {
                let mgr = Arc::clone(&mgr);
                s.spawn(move || {
                    let mut held: Vec<(ExtentRef, usize)> = Vec::new();
                    for round in 0..400u32 {
                        // Alternate alloc and free to churn split/merge under contention.
                        if round % 3 != 0 || held.is_empty() {
                            let pages = (round % 4 + 1) as usize;
                            if let Ok(r) = mgr.alloc(pages * PAGE, PAGE, Fit::First) {
                                if let Some(region) = mgr.region_of(r) {
                                    let tag = t.wrapping_add(1);
                                    // SAFETY: committed for its whole length; stamp the
                                    // first byte of each page and re-check it later.
                                    unsafe {
                                        for p in (0..region.len).step_by(PAGE) {
                                            region.base.add(p).write(tag);
                                        }
                                    }
                                    held.push((r, region.base as usize));
                                }
                            }
                        } else {
                            let (r, base) = held.swap_remove(held.len() - 1);
                            // SAFETY: still a live allocation we stamped.
                            let seen = unsafe { (base as *const u8).read() };
                            assert_eq!(
                                seen,
                                t.wrapping_add(1),
                                "another thread aliased our extent"
                            );
                            mgr.free(r).expect("free a live extent");
                        }
                    }
                    // Drain.
                    for (r, _) in held {
                        let _ = mgr.free(r);
                    }
                });
            }
        });
        assert!(
            mgr.check_invariants(),
            "concurrent churn left the back-end well-formed"
        );
        // Everything was freed and coalesced back to the whole region.
        assert_eq!(mgr.free_bytes(), 512 * PAGE);
    }
}

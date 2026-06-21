// SPDX-License-Identifier: MIT
//! W18 — security & hardening primitives (§29, §17.3, §36.12, plan 08).
//!
//! This module is the home of TopoMalloc's opt-in hardening machinery. Each
//! protection is its **own Cargo feature** so the `performance` profile pays for
//! none of them and the `hardened`/`debug` profiles *compose* the ones they want
//! (overview principle 8; `profiles/README.md`):
//!
//! | Feature | W18 | SPEC | What it adds |
//! |---|---|---|---|
//! | `junk-fill` | W18-5 | §29.6 | fill-on-alloc / fill-on-free / verify-on-reuse |
//! | `quarantine` | W18-3 | §29.4 | delayed reuse of freed objects, accounted separately |
//! | `guard-pages` | W18-4 | §29.5 | sampled inaccessible pages around an allocation |
//! | `secure-scrub` | W18-6 | §36.12 | scrub dirty memory before cross-label reuse |
//!
//! **Safety before policy (§2.4).** None of these primitives can change an
//! allocation's size, alignment, validity, or free path — they only fill,
//! verify, hold, or zero memory the allocator already owns. A protection that is
//! compiled out is a *true no-op*: its entry point here takes the same arguments
//! the hot path already computed and lowers to nothing, so the `performance`
//! build is byte-for-byte the un-hardened one.
//!
//! ## Junk filling (W18-5, §29.6)
//!
//! Two patterns bracket an object's life:
//!
//! * [`ALLOC_PATTERN`] is written over a freshly handed-out object (when the
//!   caller did **not** ask for zeroing) so a read of uninitialised memory is
//!   conspicuous rather than a lucky zero.
//! * [`FREE_PATTERN`] is written over an object the moment it is freed, which
//!   both scrubs the stale contents and arms a **use-after-free canary**.
//!
//! The canary is *sound* because of TopoMalloc's metadata design: a free small
//! object stores **no** allocator metadata in its user bytes (the free list is an
//! out-of-line bitmap, §16.4 — the W18-1b structural win), so between
//! [`fill_on_free`] and the next allocation nothing but a buggy application can
//! touch those bytes. [`fill_fresh_slab`] establishes the invariant for a newly
//! carved slab, so on every reuse [`verify_free_pattern`] can assert the object
//! still reads as [`FREE_PATTERN`]; a mismatch is a write-after-free.

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{compiler_fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::ids::ArenaId;
use crate::lock::{LockRank, RankedLock};
use crate::sampling::SampleBloom;
use crate::span::SpanDescriptor;

/// Byte written over freshly-allocated user memory in junk-fill builds (§29.6),
/// so reading it before initialisation is obvious. Mirrors jemalloc's `0xa5`
/// intent with a distinct value.
pub const ALLOC_PATTERN: u8 = 0xAB;

/// Byte written over just-freed user memory in junk-fill builds (§29.6): it
/// scrubs the stale contents *and* is the use-after-free canary
/// [`verify_free_pattern`] checks on reuse.
pub const FREE_PATTERN: u8 = 0xDE;

/// Whether junk filling (W18-5, §29.6) is compiled in.
#[inline]
#[must_use]
pub const fn junk_fill_enabled() -> bool {
    cfg!(feature = "junk-fill")
}

/// Whether the delayed-reuse quarantine (W18-3, §29.4) is compiled in.
#[inline]
#[must_use]
pub const fn quarantine_enabled() -> bool {
    cfg!(feature = "quarantine")
}

/// Whether sampled guarded allocations (W18-4, §29.5) are compiled in.
#[inline]
#[must_use]
pub const fn guard_pages_enabled() -> bool {
    cfg!(feature = "guard-pages")
}

/// Whether scrub-before-downgrade (W18-6, §36.12) is compiled in.
#[inline]
#[must_use]
pub const fn secure_scrub_enabled() -> bool {
    cfg!(feature = "secure-scrub")
}

/// Fill a freshly handed-out object with [`ALLOC_PATTERN`] (§29.6, W18-5). The
/// caller invokes this **only when it is not already zeroing** the object (the two
/// are mutually exclusive — `TOPO_ZERO`/`calloc` win). A no-op unless `junk-fill`
/// is compiled in.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a writable region the allocator owns for this
/// freshly-allocated object (its usable size).
#[inline]
pub unsafe fn fill_on_alloc(ptr: *mut u8, len: usize) {
    #[cfg(feature = "junk-fill")]
    // SAFETY: forwarded from this function's contract — the caller guarantees
    // `[ptr, ptr + len)` is a writable, just-allocated region.
    unsafe {
        ptr::write_bytes(ptr, ALLOC_PATTERN, len);
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
    }
}

/// Fill a just-freed object with [`FREE_PATTERN`] (§29.6, W18-5): scrubs the stale
/// contents and arms the use-after-free canary. Called on the free path *before*
/// the object returns to the central free list / backend, while the freeing
/// thread still has the exact object pointer. A no-op unless `junk-fill` is in.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be the writable user region of the object being freed
/// (its usable size), and the caller MUST have established that the object is no
/// longer reachable by the application (it is being freed).
#[inline]
pub unsafe fn fill_on_free(ptr: *mut u8, len: usize) {
    #[cfg(feature = "junk-fill")]
    // SAFETY: forwarded from this function's contract — the caller guarantees
    // `[ptr, ptr + len)` is the writable user region of the object being freed.
    unsafe {
        ptr::write_bytes(ptr, FREE_PATTERN, len);
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
    }
}

/// Establish the [`FREE_PATTERN`] invariant over a **freshly carved slab** (§29.6,
/// W18-5): every object slot of a brand-new span is filled so that *every*
/// central-free object — fresh or recycled — reads as [`FREE_PATTERN`], which is
/// what makes [`verify_free_pattern`] sound on the first reuse of each slot. A
/// no-op unless `junk-fill` is in. (It is the same byte as [`fill_on_free`], named
/// distinctly because it covers the whole slab, including any inter-object
/// padding, at span creation.)
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be the writable backing of a slab this allocator just
/// carved and not yet handed any object out of.
#[inline]
pub unsafe fn fill_fresh_slab(ptr: *mut u8, len: usize) {
    // SAFETY: forwarded from this function's contract; `fill_on_free` writes the
    // same `FREE_PATTERN` over the (here, whole-slab) writable region.
    unsafe { fill_on_free(ptr, len) }
}

/// Report a detected memory-safety violation and **terminate** (W18). A detected
/// corruption — a verify-on-reuse canary mismatch, say — means the heap is already
/// inconsistent, so the §2.4-safe response is to stop *now* rather than hand out a
/// compromised object. In the real artifact this **aborts** (no unwinding back
/// through the allocator, which a `panic!` risks); in `topo-core`'s own test build
/// it `panic!`s so a `#[should_panic]` test can observe the detection. Allocation-
/// free (a `&'static str`); diverges, so it is sound on any path. `#[cold]`/
/// `#[inline(never)]` keeps it off the hot path.
#[cold]
#[inline(never)]
pub fn corruption_abort(msg: &str) -> ! {
    #[cfg(all(feature = "std", not(test)))]
    {
        use std::io::Write as _;
        // Best-effort note to stderr (allocation-free), then an immediate abort.
        let _ = std::io::stderr().write_all(msg.as_bytes());
        std::process::abort()
    }
    #[cfg(not(all(feature = "std", not(test))))]
    {
        panic!("{}", msg)
    }
}

/// Verify that a central-free object still holds [`FREE_PATTERN`] (§29.6, W18-5) —
/// the **verify-on-reuse** check, run on the allocation path *before* the object
/// is overwritten (filled or zeroed). Returns `true` when the canary is intact (or
/// when `junk-fill` is compiled out, so a caller may `debug_assert!` the result
/// unconditionally); `false` means some byte differs — a write-after-free.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a readable region of `len` usable bytes for the
/// object about to be handed out.
#[inline]
#[must_use]
pub unsafe fn verify_free_pattern(ptr: *const u8, len: usize) -> bool {
    #[cfg(feature = "junk-fill")]
    {
        let mut i = 0;
        while i < len {
            // SAFETY: `i < len` and the caller guarantees `len` readable bytes.
            if unsafe { *ptr.add(i) } != FREE_PATTERN {
                return false;
            }
            i += 1;
        }
        true
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
        true
    }
}

/// **Scrub** (zero) a range of memory and fence the write so the compiler cannot
/// sink or elide it (§26.4 "security zeroing"; §36.12 scrub-before-downgrade,
/// W18-6). Unlike the junk-fill helpers this is **always available** (not
/// feature-gated): scrubbing high-domain bytes before they are reused at a lower
/// label is an information-flow *MUST* (§36.12), not a debugging aid, so the
/// primitive must exist in every profile. The `secure-scrub` feature governs only
/// *how aggressively* the allocator chooses to invoke it (see
/// [`must_scrub_for_relabel`]).
///
/// The trailing [`compiler_fence`] keeps the zeroing from being reordered after a
/// subsequent revoke/relabel, so a reader at the new label can never observe the
/// pre-scrub contents through a reordering.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a writable region the allocator owns and that no
/// other thread is concurrently reading at the *old* label (the caller — arena
/// teardown / extent recycle — guarantees quiescence, §22.5/§36.13).
#[inline]
pub unsafe fn scrub(ptr: *mut u8, len: usize) {
    // SAFETY: forwarded from this function's contract — a writable, quiesced region.
    unsafe { ptr::write_bytes(ptr, 0, len) };
    // A compiler fence pins the bulk zeroing *before* the caller's later
    // revoke/relabel so the scrub cannot be sunk past it / elided.
    // SeqCst: a *compiler* fence, not inter-thread atomic ordering — no atomic here, so the §27.3 map does not apply.
    compiler_fence(Ordering::SeqCst);
}

/// Decide whether a range must be scrubbed before its backing is reused under a
/// **different** security label (§36.12, W18-6). The information-flow rule is:
/// dirty memory from one label MUST NOT be observable at a *different* label
/// without scrubbing. This returns `true` exactly when the labels differ — the
/// scrub is then mandatory regardless of profile (it is a §36.12 MUST), and the
/// `secure-scrub` feature additionally lets the `hardened` profile scrub on
/// *every* recycle as defence in depth (see the caller in `allocator.rs`).
///
/// Pinned to the Lean `scrub_before_downgrade` theorem
/// (`lean/TopoMalloc/SeLe4n/Refinement.lean`): a successful downgrade implies the
/// frame was scrubbed (its provider state advanced to `AllocatorMuzzyOrScrubbed`).
#[inline]
#[must_use]
pub fn must_scrub_for_relabel(old: crate::ids::Label, new: crate::ids::Label) -> bool {
    old != new
}

// ---------------------------------------------------------------------------
// W18-3 — quarantine (§29.4)
// ---------------------------------------------------------------------------

/// Ring capacity of the [`Quarantine`] — the hard ceiling on held objects
/// (`max_objects` is clamped to this). A fixed array, so the quarantine never
/// allocates (it *is* the allocator, Appendix F). Sized for a few-tens-of-KiB
/// footprint that exists **only** in builds with the `quarantine` feature (the
/// engine's field is `#[cfg]`-gated, so `performance` pays nothing).
pub const QUARANTINE_CAP: usize = 1024;

/// Most entries a single [`Quarantine::offer`] / [`Quarantine::drain_batch`] can
/// hand back for the caller to really free. One admission evicts ~one object, so a
/// small chunk covers the common case; a budget tightening evicts in bounded chunks
/// that converge over successive offers, and the per-object byte accounting is exact
/// regardless, so a transient over-budget never loses bytes.
pub const QUARANTINE_MAX_BATCH: usize = 8;

/// A freed allocation held out of circulation (§29.4). The engine performs the
/// deferred *real* free (the `insert_batch` / large free) when it is evicted or
/// drained. `Copy` (it is a few words of plain data); the raw pointers reach
/// never-freed span metadata / live user memory.
#[derive(Clone, Copy)]
pub struct QuarantineEntry {
    /// The user pointer — the membership key, and the free pointer for a large.
    pub user_ptr: *mut u8,
    /// The owning span (small object); null for a large allocation.
    pub span: *const SpanDescriptor,
    /// The object index within `span` (small only; ignored for large).
    pub index: u16,
    /// The owning arena (routes the deferred free's accounting).
    pub arena: ArenaId,
    /// Usable bytes held (the separate quarantine byte accounting, §29.4).
    pub bytes: u64,
}

impl QuarantineEntry {
    /// Whether this entry is a small-object hold (vs. a large allocation).
    #[inline]
    #[must_use]
    pub fn is_small(&self) -> bool {
        !self.span.is_null()
    }
}

/// A bounded batch of entries the caller must really free (an offer's evictions
/// or one drain step). Fixed-size so the quarantine never allocates.
pub struct EvictBatch {
    entries: [QuarantineEntry; QUARANTINE_MAX_BATCH],
    len: usize,
}

impl EvictBatch {
    /// A fresh, empty eviction buffer (the caller's stack scratch for
    /// [`Quarantine::offer`] / [`Quarantine::drain_batch`]).
    #[inline]
    #[must_use]
    pub fn new() -> EvictBatch {
        EvictBatch {
            entries: [QuarantineEntry {
                user_ptr: ptr::null_mut(),
                span: ptr::null(),
                index: 0,
                arena: ArenaId::DEFAULT,
                bytes: 0,
            }; QUARANTINE_MAX_BATCH],
            len: 0,
        }
    }

    /// The evicted entries the caller must really free (drain).
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[QuarantineEntry] {
        &self.entries[..self.len]
    }

    /// Whether the batch is full (the offer/drain stopped at the chunk bound).
    #[inline]
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.len == QUARANTINE_MAX_BATCH
    }

    #[inline]
    fn push(&mut self, e: QuarantineEntry) -> bool {
        if self.len < QUARANTINE_MAX_BATCH {
            self.entries[self.len] = e;
            self.len += 1;
            true
        } else {
            false
        }
    }
}

impl Default for EvictBatch {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of offering a freed allocation to the [`Quarantine`]. Any evictions the
/// admission caused are written into the caller's `evicted` out-parameter (so this
/// stays a small, copy-cheap enum — the eviction buffer lives on the caller's stack).
pub enum Offer {
    /// `user_ptr` is already in quarantine → a double free (§29.3 quarantine hit).
    AlreadyQuarantined,
    /// The entry was held; the caller MUST really free (drain) the entries written
    /// into its `evicted` buffer.
    Held,
    /// Policy declined to quarantine this free — the caller frees it immediately.
    Declined,
}

/// The §29.4 quarantine policy knobs. Runtime-configurable (held as atomics inside
/// [`Quarantine`]); the defaults match the §32.3 hardened profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuarantinePolicy {
    /// Total bytes the quarantine may hold before evicting (§29.4 `max_bytes`).
    pub max_bytes: u64,
    /// Total objects the quarantine may hold (≤ [`QUARANTINE_CAP`], §29.4
    /// `max_objects`).
    pub max_objects: u32,
    /// Per-arena byte ceiling (`0` = unlimited; §29.4 `per_arena_limit`): a free
    /// that would push one arena over this is not quarantined (freed immediately),
    /// so one arena cannot monopolise the quarantine.
    pub per_arena_bytes: u64,
    /// Evict a **random** victim rather than the oldest (§29.4 `random_evict`):
    /// defeats an attacker timing reuse off a deterministic FIFO.
    pub random_evict: bool,
    /// Quarantine only a sampled fraction of frees (§29.4 `sampled_only`): one in
    /// `sample_shift` powers of two (so `0` = all frees, `k` = ~1 in 2^k).
    pub sample_shift: u8,
}

impl QuarantinePolicy {
    /// The default hardened policy (§32.3): hold up to 16 MiB / 4096 objects,
    /// no per-arena cap, FIFO eviction, every free quarantined.
    pub const DEFAULT: QuarantinePolicy = QuarantinePolicy {
        max_bytes: 16 * 1024 * 1024,
        max_objects: QUARANTINE_CAP as u32,
        per_arena_bytes: 0,
        random_evict: false,
        sample_shift: 0,
    };
}

/// The mutable ring state, guarded by the quarantine lock.
struct QRing {
    slots: [QuarantineEntry; QUARANTINE_CAP],
    /// Index of the oldest held entry.
    head: usize,
    /// Number of held entries.
    len: usize,
    /// Inserts since the membership filter was last rebuilt (caps its
    /// false-positive rate by triggering a periodic rebuild).
    inserts_since_rebuild: u32,
}

/// The W18-3 delayed-reuse quarantine (§29.4): a bounded FIFO of freed
/// allocations held out of circulation, with byte/object/per-arena budgets, an
/// optional random-eviction and sampling policy, and a drain protocol. Accounted
/// **separately** — held bytes are reported as `quarantine.bytes`, never as live
/// or central-free. A decoupled leaf: an eviction is *returned* and really freed
/// by the caller after the quarantine lock is released (rank [`LockRank::QUARANTINE`]).
pub struct Quarantine {
    lock: RankedLock<{ LockRank::QUARANTINE }>,
    ring: UnsafeCell<QRing>,
    /// Lock-free membership *negative* (a false answer is exact); a positive is
    /// confirmed by an exact ring scan under the lock. Bounds the double-free check
    /// on the free hot path to O(1) for the overwhelmingly common non-member case.
    bloom: SampleBloom,
    /// Held bytes — read lock-free for `quarantine.bytes` in stats (§29.4/§8.6).
    bytes: AtomicU64,
    /// Held objects — read lock-free for stats.
    count: AtomicU32,
    /// Per-arena held bytes (enforces `per_arena_bytes`).
    per_arena: [AtomicU64; crate::arena::MAX_ARENAS],
    /// Runtime master switch (in addition to the compile-time `quarantine` feature).
    enabled: AtomicBool,
    // --- policy (runtime-configurable atomics) ---
    max_bytes: AtomicU64,
    max_objects: AtomicU32,
    per_arena_bytes: AtomicU64,
    random_evict: AtomicBool,
    sample_shift: AtomicU32,
    /// xorshift state for `random_evict` / sampling decisions.
    rng: AtomicU64,
}

// SAFETY: every access to the interior `ring` is serialised by `lock`; the atomics
// are independently synchronised; the raw pointers in entries reach never-freed
// span metadata or live user memory whose lifetime the engine manages. So the
// `Quarantine` is safe to share across threads.
unsafe impl Sync for Quarantine {}
// SAFETY: as above — no thread-affine state.
unsafe impl Send for Quarantine {}

impl Quarantine {
    /// A fresh, **empty** quarantine with the [`DEFAULT`](QuarantinePolicy::DEFAULT)
    /// policy and the runtime switch **off** (opt-in, like the W17 sampler): the
    /// compiled-in machinery costs nothing until an operator enables it via the
    /// control plane / `TOPOMALLOC_QUARANTINE`, since holding objects out of
    /// circulation has an RSS/latency cost the `hardened` build should not impose
    /// unasked. Whether it can hold anything at all is *also* gated by the
    /// compile-time `quarantine` feature (a `performance` build never reaches `offer`).
    #[must_use]
    pub fn new() -> Quarantine {
        const EMPTY: QuarantineEntry = QuarantineEntry {
            user_ptr: ptr::null_mut(),
            span: ptr::null(),
            index: 0,
            arena: ArenaId::DEFAULT,
            bytes: 0,
        };
        let p = QuarantinePolicy::DEFAULT;
        Quarantine {
            lock: RankedLock::new(),
            ring: UnsafeCell::new(QRing {
                slots: [EMPTY; QUARANTINE_CAP],
                head: 0,
                len: 0,
                inserts_since_rebuild: 0,
            }),
            bloom: SampleBloom::new(),
            bytes: AtomicU64::new(0),
            count: AtomicU32::new(0),
            per_arena: [const { AtomicU64::new(0) }; crate::arena::MAX_ARENAS],
            enabled: AtomicBool::new(false),
            max_bytes: AtomicU64::new(p.max_bytes),
            max_objects: AtomicU32::new(p.max_objects),
            per_arena_bytes: AtomicU64::new(p.per_arena_bytes),
            random_evict: AtomicBool::new(p.random_evict),
            sample_shift: AtomicU32::new(p.sample_shift as u32),
            rng: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
        }
    }

    /// Held bytes, read lock-free (§29.4 separate accounting; `quarantine.bytes`).
    #[inline]
    #[must_use]
    pub fn held_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Held object count, read lock-free.
    #[inline]
    #[must_use]
    pub fn held_objects(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    /// Whether the runtime master switch is on (and the feature is compiled in).
    #[inline]
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        quarantine_enabled() && self.enabled.load(Ordering::Relaxed)
    }

    /// Turn the runtime master switch on/off. Turning it off does **not** drain —
    /// the caller drains explicitly so it can really free the held objects.
    #[inline]
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Install a new policy (clamping `max_objects` to [`QUARANTINE_CAP`]). Does not
    /// itself evict; the next [`offer`](Self::offer) (or an explicit drain) brings
    /// the ring within the new budget.
    pub fn set_policy(&self, p: QuarantinePolicy) {
        self.max_bytes.store(p.max_bytes, Ordering::Relaxed);
        self.max_objects
            .store(p.max_objects.min(QUARANTINE_CAP as u32), Ordering::Relaxed);
        self.per_arena_bytes
            .store(p.per_arena_bytes, Ordering::Relaxed);
        self.random_evict.store(p.random_evict, Ordering::Relaxed);
        self.sample_shift
            .store(p.sample_shift as u32, Ordering::Relaxed);
    }

    /// The current policy.
    #[must_use]
    pub fn policy(&self) -> QuarantinePolicy {
        QuarantinePolicy {
            max_bytes: self.max_bytes.load(Ordering::Relaxed),
            max_objects: self.max_objects.load(Ordering::Relaxed),
            per_arena_bytes: self.per_arena_bytes.load(Ordering::Relaxed),
            random_evict: self.random_evict.load(Ordering::Relaxed),
            sample_shift: self.sample_shift.load(Ordering::Relaxed) as u8,
        }
    }

    /// Next xorshift value (for sampling / random eviction).
    #[inline]
    fn next_rng(&self) -> u64 {
        // A relaxed read-modify-write loop is fine: we only need a fast,
        // decorrelated stream, not a strict sequence.
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        x
    }

    /// Whether sampling admits this free (`sample_shift == 0` ⇒ always).
    #[inline]
    fn sampled_in(&self) -> bool {
        let shift = self.sample_shift.load(Ordering::Relaxed);
        if shift == 0 {
            return true;
        }
        let mask = (1u64 << shift.min(63)) - 1;
        self.next_rng() & mask == 0
    }

    /// Exact membership (double-free of a held object), bounded to O(1) on the
    /// common non-member path by the lock-free [`SampleBloom`] negative. Caller
    /// holds the lock.
    fn contains_locked(&self, user_ptr: usize) -> bool {
        if !self.bloom.maybe_contains(user_ptr) {
            return false;
        }
        // SAFETY: the quarantine lock is held ⇒ exclusive ring access.
        let r = unsafe { &*self.ring.get() };
        for k in 0..r.len {
            let i = (r.head + k) % QUARANTINE_CAP;
            if r.slots[i].user_ptr as usize == user_ptr {
                return true;
            }
        }
        false
    }

    /// Offer a freed allocation to the quarantine (§29.4). The caller has already
    /// established `entry` is a *genuine* live→free transition (not already
    /// central-free); this adds the §29.3 "already in quarantine" double-free guard
    /// and the policy decision. Any entries this admission evicts are appended to
    /// `evicted` (a fresh, caller-owned [`EvictBatch`]); the caller must really free
    /// them. Returns:
    /// * [`Offer::AlreadyQuarantined`] — `user_ptr` is held ⇒ a double free;
    /// * [`Offer::Held`] — held; the caller drains `evicted`;
    /// * [`Offer::Declined`] — policy declined; the caller frees `entry` now.
    ///
    /// The held bytes/objects are charged to the separate accounting here; an
    /// eviction's bytes are released when it is moved into `evicted` (the caller
    /// really frees it). So `held_bytes` is always exact.
    pub fn offer(&self, entry: QuarantineEntry, evicted: &mut EvictBatch) -> Offer {
        evicted.len = 0;
        let admit_sampled = self.sampled_in();
        self.lock.acquire();
        if self.contains_locked(entry.user_ptr as usize) {
            self.lock.release();
            return Offer::AlreadyQuarantined;
        }
        let max_bytes = self.max_bytes.load(Ordering::Relaxed);
        let max_objects = self.max_objects.load(Ordering::Relaxed) as usize;
        let bytes = entry.bytes;
        // Decline (free immediately) when: sampling skipped it, the object alone
        // exceeds the whole budget (it could never fit), or it would push its arena
        // past the per-arena ceiling.
        let per_arena_limit = self.per_arena_bytes.load(Ordering::Relaxed);
        let arena_idx = entry.arena.0 as usize;
        let arena_held = self
            .per_arena
            .get(arena_idx)
            .map_or(0, |a| a.load(Ordering::Relaxed));
        let declines = !admit_sampled
            || max_objects == 0
            || bytes > max_bytes
            || (per_arena_limit != 0 && arena_held + bytes > per_arena_limit);
        if declines {
            self.lock.release();
            return Offer::Declined;
        }
        // Evict until the new entry fits within both budgets (bounded per call).
        // SAFETY: the lock is held ⇒ exclusive ring access throughout.
        let r = unsafe { &mut *self.ring.get() };
        while (self.bytes.load(Ordering::Relaxed) + bytes > max_bytes || r.len >= max_objects)
            && r.len > 0
            && !evicted.is_full()
        {
            let victim_off = if self.random_evict.load(Ordering::Relaxed) {
                (self.next_rng() as usize) % r.len
            } else {
                0 // FIFO: the oldest
            };
            let vi = (r.head + victim_off) % QUARANTINE_CAP;
            let v = r.slots[vi];
            // Compact: move the head entry into the victim's slot, advance head.
            r.slots[vi] = r.slots[r.head];
            r.head = (r.head + 1) % QUARANTINE_CAP;
            r.len -= 1;
            self.release_accounting(&v);
            evicted.push(v);
        }
        // Push the new entry at the tail.
        if r.len < QUARANTINE_CAP {
            let ti = (r.head + r.len) % QUARANTINE_CAP;
            r.slots[ti] = entry;
            r.len += 1;
            r.inserts_since_rebuild += 1;
            self.bloom.insert(entry.user_ptr as usize);
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
            self.count.fetch_add(1, Ordering::Relaxed);
            if let Some(a) = self.per_arena.get(arena_idx) {
                a.fetch_add(bytes, Ordering::Relaxed);
            }
            // Periodically rebuild the membership filter so drained entries' stale
            // bits cannot accumulate into a high false-positive rate.
            if r.inserts_since_rebuild as usize >= QUARANTINE_CAP {
                self.rebuild_bloom(r);
            }
        } else {
            // Ring full at capacity even after eviction (max_objects == CAP and the
            // batch bound stopped eviction): decline rather than drop the entry, so
            // the caller frees it now (never leaked). Any evictions already collected
            // are still in `evicted` for the caller to drain.
            self.lock.release();
            return if evicted.len > 0 {
                Offer::Held
            } else {
                Offer::Declined
            };
        }
        self.lock.release();
        Offer::Held
    }

    /// Pop up to [`QUARANTINE_MAX_BATCH`] held entries for the caller to really free
    /// — the **drain protocol** (§29.4). Returns an empty batch when the quarantine
    /// is empty; the caller loops until then (each batch freed outside the lock).
    /// Used at shutdown, on a runtime disable, and before an arena teardown so no
    /// held object dangles into a retired span.
    pub fn drain_batch(&self) -> EvictBatch {
        let mut out = EvictBatch::new();
        self.lock.acquire();
        // SAFETY: the lock is held ⇒ exclusive ring access.
        let r = unsafe { &mut *self.ring.get() };
        while r.len > 0 && !out.is_full() {
            let e = r.slots[r.head];
            r.head = (r.head + 1) % QUARANTINE_CAP;
            r.len -= 1;
            self.release_accounting(&e);
            out.push(e);
        }
        if r.len == 0 {
            // Empty ring ⇒ no stale membership bits can matter; reset the filter.
            self.bloom.reset();
            r.inserts_since_rebuild = 0;
        }
        self.lock.release();
        out
    }

    /// Release an entry's separate accounting as it leaves the quarantine (evicted
    /// or drained). Caller holds the lock.
    #[inline]
    fn release_accounting(&self, e: &QuarantineEntry) {
        self.bytes.fetch_sub(e.bytes, Ordering::Relaxed);
        self.count.fetch_sub(1, Ordering::Relaxed);
        if let Some(a) = self.per_arena.get(e.arena.0 as usize) {
            a.fetch_sub(e.bytes, Ordering::Relaxed);
        }
    }

    /// Rebuild the membership filter from the live ring (caller holds the lock):
    /// reset, then re-insert every held entry, clearing stale (drained) bits.
    fn rebuild_bloom(&self, r: &mut QRing) {
        self.bloom.reset();
        for k in 0..r.len {
            let i = (r.head + k) % QUARANTINE_CAP;
            self.bloom.insert(r.slots[i].user_ptr as usize);
        }
        r.inserts_since_rebuild = 0;
    }
}

impl Default for Quarantine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// W18-4 — guarded-allocation sampler (§29.5)
// ---------------------------------------------------------------------------

/// Decides which allocations receive guard pages (W18-4, §29.5). A `TOPO_GUARDED`
/// request is always honoured by the engine; this governs the *sampled* fraction of
/// ordinary allocations: each allocation is guarded independently with probability
/// ~1/`rate` (rate `0` ⇒ explicit requests only — the default, so the `hardened`
/// build does not pay the ≥3-pages-per-object cost unasked). Lock-free: one relaxed
/// load on the common (rate 0) path.
///
/// Sampling is **randomized**, not a fixed 1-in-`rate` stride: a deterministic stride
/// is predictable (an attacker who learns `rate` can avoid the guarded slots by
/// counting allocations), defeating the probabilistic detection W18-4 is for. An
/// independent per-allocation coin (GWP-ASan style) is unpredictable and still yields
/// the same expected ~1/`rate` density.
pub struct GuardSampler {
    rate: AtomicU64,
    /// xorshift64 state — a fast, decorrelated stream for the per-allocation coin.
    /// Seeded non-zero (xorshift64 never leaves the non-zero orbit), distinct from
    /// the quarantine's seed so the two samplers are uncorrelated.
    rng: AtomicU64,
}

impl GuardSampler {
    /// A fresh sampler with sampling **off** (explicit `TOPO_GUARDED` only).
    #[must_use]
    pub const fn new() -> GuardSampler {
        GuardSampler {
            rate: AtomicU64::new(0),
            rng: AtomicU64::new(0xD1B5_4A32_D192_ED03),
        }
    }

    /// Sample ~1 in `rate` allocations (`0` disables sampling).
    #[inline]
    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// The current sampling rate (`0` = off).
    #[inline]
    #[must_use]
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Next xorshift value — a relaxed read-modify-write (a fast, decorrelated
    /// stream is all sampling needs, not a strict sequence; a lost update under a
    /// race merely re-uses a value, never biasing toward guarding).
    #[inline]
    fn next_rng(&self) -> u64 {
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        x
    }

    /// Whether to guard the next allocation **by sampling** (an explicit request is
    /// decided by the caller). `false` immediately when sampling is off. Each call is
    /// an independent ~1/`rate` coin, so the guarded slots are unpredictable.
    #[inline]
    #[must_use]
    pub fn sampled(&self) -> bool {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 {
            return false;
        }
        if rate == 1 {
            return true; // guard everything (degenerate rate)
        }
        self.next_rng().is_multiple_of(rate)
    }
}

impl Default for GuardSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_distinct_and_nonzero() {
        // The two junk patterns must differ (so a fill-on-free is distinguishable
        // from a fill-on-alloc) and be non-zero (so they are distinguishable from
        // a zeroed/`calloc` region).
        assert_ne!(ALLOC_PATTERN, FREE_PATTERN);
        assert_ne!(ALLOC_PATTERN, 0);
        assert_ne!(FREE_PATTERN, 0);
    }

    #[test]
    fn gating_reflects_features() {
        // Each predicate mirrors its feature exactly (the profile composition is
        // tested by the build matrix; here we pin the const-fn ↔ cfg coupling).
        assert_eq!(junk_fill_enabled(), cfg!(feature = "junk-fill"));
        assert_eq!(quarantine_enabled(), cfg!(feature = "quarantine"));
        assert_eq!(guard_pages_enabled(), cfg!(feature = "guard-pages"));
        assert_eq!(secure_scrub_enabled(), cfg!(feature = "secure-scrub"));
    }

    #[test]
    fn scrub_zeroes_the_whole_range() {
        let mut buf = [0xFFu8; 64];
        // SAFETY: `buf` is a live, exclusively-owned 64-byte region.
        unsafe { scrub(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn must_scrub_only_on_label_change() {
        use crate::ids::Label;
        assert!(!must_scrub_for_relabel(Label::PUBLIC, Label::PUBLIC));
        assert!(must_scrub_for_relabel(Label::PUBLIC, Label(7)));
        assert!(must_scrub_for_relabel(Label(7), Label::PUBLIC));
    }

    #[cfg(feature = "junk-fill")]
    #[test]
    fn junk_fill_lifecycle_round_trips() {
        let mut buf = [0u8; 48];
        // Fresh slab → every byte is FREE_PATTERN, so verify passes.
        // SAFETY: `buf` is a live 48-byte region.
        unsafe { fill_fresh_slab(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == FREE_PATTERN));
        // SAFETY: same region; verify reads it.
        assert!(unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
        // Hand out → fill with ALLOC_PATTERN.
        // SAFETY: same region.
        unsafe { fill_on_alloc(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == ALLOC_PATTERN));
        // Free → back to FREE_PATTERN, verify passes again.
        // SAFETY: same region.
        unsafe { fill_on_free(buf.as_mut_ptr(), buf.len()) };
        // SAFETY: same region; verify reads it.
        assert!(unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
    }

    #[cfg(feature = "junk-fill")]
    #[test]
    fn verify_detects_a_write_after_free() {
        let mut buf = [FREE_PATTERN; 32];
        buf[17] = 0x00; // a stray write-after-free
                        // SAFETY: `buf` is a live 32-byte region.
        assert!(!unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
    }

    // --- W18-3 quarantine (the data structure; exercised with opaque pointers it
    // never dereferences) ---

    fn small_entry(addr: usize, bytes: u64) -> QuarantineEntry {
        // A small-object entry; the quarantine treats `span` as opaque (non-null ⇒
        // small) and `user_ptr` as the membership key — it dereferences neither.
        QuarantineEntry {
            user_ptr: addr as *mut u8,
            span: addr as *const SpanDescriptor,
            index: 0,
            arena: ArenaId::DEFAULT,
            bytes,
        }
    }

    /// Offer an entry, returning the outcome and any evictions (the out-param form
    /// the engine uses, wrapped for the tests).
    fn offer(q: &Quarantine, e: QuarantineEntry) -> (Offer, EvictBatch) {
        let mut ev = EvictBatch::new();
        let o = q.offer(e, &mut ev);
        (o, ev)
    }

    #[test]
    fn quarantine_holds_and_accounts_separately() {
        let q = Quarantine::new();
        assert_eq!(q.held_bytes(), 0);
        for k in 1..=4u64 {
            let (o, _) = offer(&q, small_entry(0x1000 * k as usize, 100));
            assert!(matches!(o, Offer::Held));
        }
        assert_eq!(q.held_objects(), 4);
        assert_eq!(q.held_bytes(), 400);
    }

    #[test]
    fn quarantine_detects_a_double_offer_as_a_hit() {
        let q = Quarantine::new();
        let p = 0xDEAD_0000usize;
        assert!(matches!(offer(&q, small_entry(p, 64)).0, Offer::Held));
        // Offering the same pointer again is a quarantine hit (double free, §29.3).
        assert!(matches!(
            offer(&q, small_entry(p, 64)).0,
            Offer::AlreadyQuarantined
        ));
        // Accounted once, not twice.
        assert_eq!(q.held_objects(), 1);
        assert_eq!(q.held_bytes(), 64);
    }

    #[test]
    fn quarantine_evicts_the_oldest_over_the_byte_budget() {
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: 250,
            max_objects: QUARANTINE_CAP as u32,
            per_arena_bytes: 0,
            random_evict: false,
            sample_shift: 0,
        });
        // Three 100-byte holds: the third pushes total to 300 > 250, evicting the
        // oldest so the held total stays within budget.
        let _ = offer(&q, small_entry(0xA000, 100)); // oldest
        let _ = offer(&q, small_entry(0xB000, 100));
        let (o, ev) = offer(&q, small_entry(0xC000, 100));
        assert!(matches!(o, Offer::Held));
        assert_eq!(ev.as_slice().len(), 1, "one eviction");
        assert_eq!(
            ev.as_slice()[0].user_ptr as usize,
            0xA000,
            "FIFO: oldest out"
        );
        assert_eq!(q.held_bytes(), 200, "two 100-byte holds remain");
        // The evicted one is no longer a member; the kept ones still are.
        assert!(matches!(
            offer(&q, small_entry(0xB000, 100)).0,
            Offer::AlreadyQuarantined
        ));
    }

    #[test]
    fn quarantine_object_budget_is_enforced() {
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: u64::MAX,
            max_objects: 2,
            per_arena_bytes: 0,
            random_evict: false,
            sample_shift: 0,
        });
        let _ = offer(&q, small_entry(0x10, 8));
        let _ = offer(&q, small_entry(0x20, 8));
        let (o, ev) = offer(&q, small_entry(0x30, 8));
        assert!(matches!(o, Offer::Held));
        assert_eq!(ev.as_slice().len(), 1, "evict one to stay at max_objects=2");
        assert_eq!(q.held_objects(), 2);
    }

    #[test]
    fn quarantine_per_arena_cap_declines() {
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: u64::MAX,
            max_objects: QUARANTINE_CAP as u32,
            per_arena_bytes: 150,
            random_evict: false,
            sample_shift: 0,
        });
        assert!(matches!(offer(&q, small_entry(0x1, 100)).0, Offer::Held));
        // 100 + 100 > 150 ⇒ the second is declined (freed immediately), so one
        // arena cannot monopolise the quarantine.
        assert!(matches!(
            offer(&q, small_entry(0x2, 100)).0,
            Offer::Declined
        ));
        assert_eq!(q.held_objects(), 1);
    }

    #[test]
    fn quarantine_object_larger_than_budget_is_declined() {
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: 64,
            ..QuarantinePolicy::DEFAULT
        });
        // A 128-byte object can never fit a 64-byte budget ⇒ declined, never held.
        assert!(matches!(
            offer(&q, small_entry(0x1, 128)).0,
            Offer::Declined
        ));
        assert_eq!(q.held_bytes(), 0);
    }

    #[test]
    fn quarantine_drains_everything() {
        let q = Quarantine::new();
        for k in 0..100usize {
            let _ = offer(&q, small_entry(0x1_0000 + k * 0x100, 16));
        }
        assert_eq!(q.held_objects(), 100);
        let mut drained = 0usize;
        loop {
            let b = q.drain_batch();
            if b.as_slice().is_empty() {
                break;
            }
            drained += b.as_slice().len();
        }
        assert_eq!(drained, 100, "drain returns every held object exactly once");
        assert_eq!(q.held_bytes(), 0);
        assert_eq!(q.held_objects(), 0);
    }

    // --- W18-4 guarded-allocation sampler ---

    #[test]
    fn guard_sampler_off_and_full_are_exact() {
        let s = GuardSampler::new();
        // Off by default: never samples, regardless of how many times asked.
        assert_eq!(s.rate(), 0);
        for _ in 0..1000 {
            assert!(!s.sampled());
        }
        // Rate 1 is the degenerate "guard everything".
        s.set_rate(1);
        for _ in 0..1000 {
            assert!(s.sampled());
        }
    }

    #[test]
    fn guard_sampler_density_is_about_one_in_rate() {
        // Over a large run the *density* of guarded allocations tracks ~1/rate, even
        // though each decision is an independent randomized coin (not a fixed stride).
        let s = GuardSampler::new();
        const RATE: u64 = 16;
        const N: u64 = 200_000;
        s.set_rate(RATE);
        let hits = (0..N).filter(|_| s.sampled()).count() as u64;
        let expected = N / RATE;
        // A generous ±35% band: the point is order-of-magnitude correctness and
        // determinism-of-the-test, not a tight statistical bound (which would flake).
        let lo = expected - expected * 35 / 100;
        let hi = expected + expected * 35 / 100;
        assert!(
            (lo..=hi).contains(&hits),
            "guard density {hits} outside [{lo}, {hi}] for rate {RATE} over {N}"
        );
    }

    #[test]
    fn guard_sampler_is_not_a_fixed_stride() {
        // A deterministic 1-in-rate stride would guard exactly the indices
        // {rate-1, 2*rate-1, …} with a constant gap; the randomized sampler must not.
        // Collect the gaps between consecutive hits and assert they are not all equal.
        let s = GuardSampler::new();
        const RATE: u64 = 8;
        s.set_rate(RATE);
        let mut gaps = Vec::new();
        let mut last_hit: Option<u64> = None;
        for i in 0..4000u64 {
            if s.sampled() {
                if let Some(prev) = last_hit {
                    gaps.push(i - prev);
                }
                last_hit = Some(i);
            }
        }
        assert!(gaps.len() > 20, "expected many hits to compare gaps");
        let first = gaps[0];
        assert!(
            gaps.iter().any(|&g| g != first),
            "randomized sampling must produce varying gaps, not a constant stride"
        );
    }
}

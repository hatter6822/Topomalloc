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

// ---------------------------------------------------------------------------
// Process entropy for the randomized security samplers (§29.4/§29.5)
// ---------------------------------------------------------------------------

/// Per-process entropy the randomized *security* samplers seed from — the W18-4
/// guarded-allocation coin and the W18-3 quarantine's sampling / random eviction.
///
/// **Why this exists.** Both samplers document unpredictability as the property that
/// makes them useful: an attacker who can predict which allocations are guarded simply
/// avoids those slots, and one who can predict the eviction order can force an early
/// reuse. A compile-time constant seed gives the *same* stream in every process of the
/// same binary, so the sequence is public — exactly the weakness the "randomized, not a
/// fixed stride" design exists to remove. The host installs real OS entropy here once at
/// start-up; until it does, the samplers keep their build-time constant (no worse than
/// before, and still correct).
///
/// `topo-core` stays `no_std`, so entropy is **pushed in** rather than read: the hosted
/// shell (`topo-abi`) calls [`set_process_entropy`] from its initializer, and a
/// kernel/seLe4n embedder calls it with whatever its platform provides. Deterministic
/// mode (§30.4) overrides it afterwards through `Allocator::apply_deterministic_seed`,
/// so a seeded replay stays reproducible.
///
/// `0` means "not installed" (the sentinel).
static PROCESS_ENTROPY: AtomicU64 = AtomicU64::new(0);

/// Install this process's entropy for the randomized security samplers. The last writer
/// wins; safe to call before or after the allocator exists (a sampler built later reads
/// it, and a live one is re-seeded by
/// [`Allocator::seed_security_samplers`](crate::Allocator::seed_security_samplers)).
///
/// A `0` value is ignored — it is the "not installed" sentinel.
#[inline]
pub fn set_process_entropy(value: u64) {
    if value != 0 {
        PROCESS_ENTROPY.store(value, Ordering::Relaxed);
    }
}

/// The installed process entropy, or `0` when the host installed none.
#[inline]
#[must_use]
pub fn process_entropy() -> u64 {
    PROCESS_ENTROPY.load(Ordering::Relaxed)
}

/// A non-zero RNG seed for `domain_salt` derived from the installed process entropy, or
/// `None` when none was installed (the caller keeps its build-time seed).
///
/// Uses the same SplitMix64 mix as the deterministic-mode `domain_seed`, so distinct
/// salts give decorrelated streams and the entropy is not recoverable from one stream.
#[inline]
#[must_use]
pub fn entropy_seed(domain_salt: u64) -> Option<u64> {
    let e = process_entropy();
    if e == 0 {
        return None;
    }
    let mixed = crate::deterministic::mix_seed(e, domain_salt);
    Some(if mixed == 0 { 0x1 } else { mixed })
}

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
///
/// ## Accounting model (and its relationship to the §16.4/§17.5 scaffolding)
///
/// This is a **per-object** quarantine. A held object is accounted as **app-freed**
/// (`freed_bytes += usable`, so it leaves `live_bytes = allocated − freed`) and,
/// separately, as `quarantine.bytes`; the owning span's `live_count` stays raised so
/// the slab cannot be recycled while a held object's address is still reserved. Those
/// are two independent ledgers — the app byte stats key off `allocated/freed`, the
/// span `live_count` only governs slab recycling — so there is **no double count**
/// (verified by `tests/tests/stats.rs::reconciliation_holds_with_quarantine_*`). A
/// double free of a held object is caught here by the membership filter
/// ([`offer`](Self::offer) returns [`AlreadyQuarantined`](Offer::AlreadyQuarantined)),
/// O(1) on the common non-member path.
///
/// The SPEC's per-**span** `QUARANTINED` flag (`SpanDescriptor::set_quarantined` →
/// [`PointerClass::Quarantined`](crate::ptr_class::PointerClass) →
/// `InvalidFree::DoubleFreeQuarantined`) is a **different granularity** — whole-span
/// quarantine — that this per-object mechanism deliberately does not drive (flagging a
/// whole span would mis-classify its still-live objects). It remains defined for that
/// distinct use.
///
/// ## Double-free detection is sound through the eviction window (§29.3)
///
/// A double free of a *held* object is caught with **no window**, including the
/// concurrency-sensitive eviction case. The mechanism has two layers:
///
/// * The ring's membership check catches a re-free of an object still held
///   ([`AlreadyQuarantined`](Offer::AlreadyQuarantined)).
/// * An **authoritative per-object quarantined state** — a second per-object bitmap on
///   the [`SpanDescriptor`] for small objects (`SpanGuard::mark_quarantined` /
///   `SpanDescriptor::is_object_quarantined`), and a per-descriptor mark for larges
///   (`LargeAllocator::mark_quarantined`) — covers the gap the ring alone cannot. The
///   bit is set under the span lock when an object is held and stays set after an
///   eviction removes it from the ring; only the **drain** clears it, and the drain
///   sets the central-free bit *before* clearing the quarantined bit (free-bit-first),
///   both under the span lock. So a lock-free `is_object_quarantined(i) ||
///   is_central_free(i)` check on the free path always observes at least one bit set
///   from hold until the object is legitimately re-vended — closing the window without
///   restructuring the §27.2 leaf-lock discipline.
///
/// This is keyed on the *slot*, not the address, so it is immune to address reuse (a
/// re-vended object reads as neither quarantined nor central-free — correctly *not* a
/// double free), unlike an address-keyed scheme. Only compiled in `quarantine` builds,
/// so `performance` pays nothing.
///
/// ## Sampling independence
///
/// `sample_shift` (a fraction of frees are held, to cap quarantine RSS/latency) is an
/// **independent** randomized coin, not tied to the W17-3 heap sampler: profiling
/// state must never gate a *safety* protection (a non-sampled object must still be
/// quarantine-eligible), so the two samplers are deliberately decoupled.
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

    /// Appendix-B invariant check (W18-3): the lock-free accounting atomics agree
    /// with the ring contents — `count` is the ring length, `bytes` is the exact sum
    /// of held entry bytes, the per-arena tallies partition `bytes`, the ring indices
    /// are in range, and the membership filter contains every held entry (so the
    /// O(1) double-free negative is never a false *negative*). Acquires the lock; for
    /// external/test use (the hot paths assert the locked form in debug). Always
    /// `true` is not assumed — a regression in the accounting trips it.
    #[must_use]
    pub fn check_invariants(&self) -> bool {
        self.lock.acquire();
        // SAFETY: the lock is held ⇒ exclusive ring access.
        let ok = self.check_invariants_locked(unsafe { &*self.ring.get() });
        self.lock.release();
        ok
    }

    /// [`check_invariants`](Self::check_invariants) with the lock already held — the
    /// form the hot paths `debug_assert!` before releasing the lock.
    fn check_invariants_locked(&self, r: &QRing) -> bool {
        if r.len > QUARANTINE_CAP || (r.len > 0 && r.head >= QUARANTINE_CAP) {
            return false;
        }
        if self.count.load(Ordering::Relaxed) as usize != r.len {
            return false;
        }
        let mut sum: u64 = 0;
        let mut per_arena = [0u64; crate::arena::MAX_ARENAS];
        for k in 0..r.len {
            let i = (r.head + k) % QUARANTINE_CAP;
            let e = &r.slots[i];
            sum += e.bytes;
            if let Some(slot) = per_arena.get_mut(e.arena.0 as usize) {
                *slot += e.bytes;
            }
            // Every held entry must be in the membership filter (no false negative).
            if !self.bloom.maybe_contains(e.user_ptr as usize) {
                return false;
            }
        }
        if self.bytes.load(Ordering::Relaxed) != sum {
            return false;
        }
        (0..crate::arena::MAX_ARENAS)
            .all(|a| self.per_arena[a].load(Ordering::Relaxed) == per_arena[a])
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

    /// Reseed the sampling / random-eviction RNG (§30.4, W19-3): deterministic
    /// mode seeds this from [`deterministic::domain_seed`](crate::deterministic::domain_seed)
    /// so sampled holds and random evictions are *reproducible* run-to-run. A `0`
    /// seed is coerced to a non-zero state (xorshift64 requires a non-zero orbit).
    #[inline]
    pub fn set_seed(&self, seed: u64) {
        self.rng
            .store(if seed == 0 { 0x1 } else { seed }, Ordering::Relaxed);
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
            debug_assert!(self.check_invariants_locked(r));
            self.lock.release();
            return if evicted.len > 0 {
                Offer::Held
            } else {
                Offer::Declined
            };
        }
        debug_assert!(self.check_invariants_locked(r));
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
        debug_assert!(self.check_invariants_locked(r));
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

    /// **Background convergence** (§29.4, W18-3): pop the oldest held entries (a
    /// bounded batch) until the held set is within the *current* budget, returning
    /// them for the caller to really free (the drain protocol). The [`offer`](Self::offer)
    /// path converges incrementally as new frees arrive, but after a runtime budget
    /// reduction ([`set_policy`](Self::set_policy) with a smaller `max_bytes`/
    /// `max_objects`) a quiescent heap (no allocation traffic) would otherwise hold
    /// the now-excess bytes indefinitely; a host calls this to bring the quarantine
    /// down to budget promptly. Returns an empty batch when already within budget;
    /// loop until empty (each batch freed outside the lock). FIFO (oldest-first), so
    /// convergence is deterministic regardless of the eviction policy.
    pub fn drain_excess(&self) -> EvictBatch {
        let mut out = EvictBatch::new();
        let max_bytes = self.max_bytes.load(Ordering::Relaxed);
        let max_objects = self.max_objects.load(Ordering::Relaxed) as usize;
        self.lock.acquire();
        // SAFETY: the lock is held ⇒ exclusive ring access.
        let r = unsafe { &mut *self.ring.get() };
        while (self.bytes.load(Ordering::Relaxed) > max_bytes || r.len > max_objects)
            && r.len > 0
            && !out.is_full()
        {
            let e = r.slots[r.head];
            r.head = (r.head + 1) % QUARANTINE_CAP;
            r.len -= 1;
            self.release_accounting(&e);
            out.push(e);
        }
        if r.len == 0 {
            self.bloom.reset();
            r.inserts_since_rebuild = 0;
        }
        debug_assert!(self.check_invariants_locked(r));
        self.lock.release();
        out
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
/// independent per-allocation coin (GWP-ASan style) still yields the same expected
/// ~1/`rate` density.
///
/// Randomized is not the same as unpredictable, and the difference is the whole point:
/// the stream is only unpredictable once it is seeded from **per-process entropy**. The
/// `const` seed below is a placeholder — identical in every process of a given binary,
/// so an attacker with the binary can compute the guarded slots exactly. The host must
/// install entropy with [`set_process_entropy`] and call
/// [`Allocator::seed_security_samplers`](crate::Allocator::seed_security_samplers)
/// during start-up (`topo-abi` does this in its initializer); deterministic mode
/// (§30.4) then re-seeds from the explicit global seed so a replay stays reproducible.
pub struct GuardSampler {
    rate: AtomicU64,
    /// xorshift64 state — a fast, decorrelated stream for the per-allocation coin.
    /// Seeded non-zero (xorshift64 never leaves the non-zero orbit), distinct from
    /// the quarantine's seed so the two samplers are uncorrelated. The build-time value
    /// is a **placeholder**: it is the same in every process, so unpredictability only
    /// begins once the host re-seeds from [`process_entropy`] (see the type docs).
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

    /// Reseed the per-allocation coin's RNG (§30.4, W19-3): deterministic mode
    /// seeds this from [`deterministic::domain_seed`](crate::deterministic::domain_seed)
    /// so the guarded slots are *reproducible* run-to-run. A `0` seed is coerced
    /// to a non-zero state (xorshift64 never leaves the non-zero orbit).
    #[inline]
    pub fn set_seed(&self, seed: u64) {
        self.rng
            .store(if seed == 0 { 0x1 } else { seed }, Ordering::Relaxed);
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

    #[test]
    fn quarantine_randomized_offers_and_drains_preserve_every_invariant() {
        // A dependency-free property test (the topo-core supply chain has no proptest):
        // a long random sequence of offers (distinct keys) and drains must keep the
        // Appendix-B invariant, respect both budgets, and conserve bytes — every offered
        // entry is exactly one of held / evicted-now / declined, never lost or
        // double-counted.
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: 4096,
            max_objects: 24,
            random_evict: true,
            ..QuarantinePolicy::DEFAULT
        });
        // A tiny deterministic xorshift so the run is reproducible.
        let mut rng: u64 = 0x1234_5678_9abc_def1;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut key: usize = 0x10_0000;
        // Conservation tallies over the whole run.
        let mut offered_bytes: u64 = 0;
        let mut freed_bytes: u64 = 0; // declined + evicted + drained (caller really frees)
        for _ in 0..20_000 {
            assert!(q.check_invariants(), "invariant holds before each step");
            if next() % 3 == 0 {
                // Drain one batch; the caller frees the returned entries.
                let batch = q.drain_batch();
                for e in batch.as_slice() {
                    freed_bytes += e.bytes;
                }
            } else {
                // Offer a fresh (never-before-seen) key so there is no double-offer.
                key += 0x40;
                let bytes = 16 + (next() % 8) * 16; // 16..=128, a multiple of 16
                offered_bytes += bytes;
                let mut ev = EvictBatch::new();
                match q.offer(small_entry(key, bytes), &mut ev) {
                    Offer::Declined => freed_bytes += bytes, // not held ⇒ caller frees now
                    Offer::Held | Offer::AlreadyQuarantined => {}
                }
                for e in ev.as_slice() {
                    freed_bytes += e.bytes; // evicted ⇒ caller frees now
                }
            }
            // Budgets are never exceeded after a settled step.
            assert!(q.held_bytes() <= 4096, "byte budget respected");
            assert!(q.held_objects() <= 24, "object budget respected");
        }
        // Drain the remainder and check exact conservation: everything offered was
        // either freed along the way or is still held (then drained).
        loop {
            let b = q.drain_batch();
            if b.as_slice().is_empty() {
                break;
            }
            for e in b.as_slice() {
                freed_bytes += e.bytes;
            }
        }
        assert_eq!(q.held_bytes(), 0);
        assert_eq!(q.held_objects(), 0);
        assert_eq!(
            offered_bytes, freed_bytes,
            "byte conservation: every offered entry is freed exactly once"
        );
    }

    #[test]
    fn quarantine_concurrent_offers_and_drains_conserve_and_stay_well_formed() {
        // W18-3 (#20): the quarantine is shared across threads (it is `Sync` behind one
        // ranked lock). Many threads concurrently offer (disjoint key ranges, so no
        // cross-thread double-offer) and drain; the invariant holds throughout, the
        // budgets are respected, and **exact byte conservation** holds across all
        // threads — every offered byte is freed exactly once (held-then-drained,
        // declined, or evicted). Run under TSan (the `hardened` lib pass) this is also
        // the data-race check for the lock + the lock-free stat atomics + the bloom.
        use std::sync::atomic::AtomicU64;
        use std::sync::Arc;

        let q = Arc::new(Quarantine::new());
        q.set_policy(QuarantinePolicy {
            max_bytes: 8192,
            max_objects: 48,
            random_evict: true,
            ..QuarantinePolicy::DEFAULT
        });
        let offered = Arc::new(AtomicU64::new(0));
        let freed = Arc::new(AtomicU64::new(0));
        const THREADS: usize = 4;
        const PER: usize = 4000;
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let q = Arc::clone(&q);
                let offered = Arc::clone(&offered);
                let freed = Arc::clone(&freed);
                scope.spawn(move || {
                    // Disjoint key space per thread ⇒ keys are globally unique.
                    let mut key = 0x100_0000usize + t * 0x40_0000;
                    let mut rng = 0xC0FF_EE00_u64 ^ (t as u64).wrapping_mul(0x9E37_79B9);
                    let mut next = || {
                        rng ^= rng << 13;
                        rng ^= rng >> 7;
                        rng ^= rng << 17;
                        rng
                    };
                    for _ in 0..PER {
                        if next() % 4 == 0 {
                            for e in q.drain_batch().as_slice() {
                                freed.fetch_add(e.bytes, Ordering::Relaxed);
                            }
                        } else {
                            key += 0x40;
                            let bytes = 16 + (next() % 16) * 16;
                            offered.fetch_add(bytes, Ordering::Relaxed);
                            let mut ev = EvictBatch::new();
                            match q.offer(small_entry(key, bytes), &mut ev) {
                                Offer::Declined => {
                                    freed.fetch_add(bytes, Ordering::Relaxed);
                                }
                                Offer::Held | Offer::AlreadyQuarantined => {}
                            }
                            for e in ev.as_slice() {
                                freed.fetch_add(e.bytes, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
        });
        // Quiescent: invariant holds, budgets respected, then drain the remainder.
        assert!(q.check_invariants());
        assert!(q.held_bytes() <= 8192 && q.held_objects() <= 48);
        loop {
            let b = q.drain_batch();
            if b.as_slice().is_empty() {
                break;
            }
            for e in b.as_slice() {
                freed.fetch_add(e.bytes, Ordering::Relaxed);
            }
        }
        assert_eq!(q.held_objects(), 0);
        assert_eq!(
            offered.load(Ordering::Relaxed),
            freed.load(Ordering::Relaxed),
            "byte conservation across all threads"
        );
    }

    #[test]
    fn quarantine_drain_excess_converges_to_a_lowered_budget() {
        // Hold a full quarantine, then lower the budget at runtime: `drain_excess`
        // brings the held set down to the new budget (oldest-first), and never below.
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: 64 * 20,
            max_objects: 20,
            ..QuarantinePolicy::DEFAULT
        });
        for k in 0..20 {
            assert!(matches!(
                offer(&q, small_entry(0x1000 + k * 0x40, 64)).0,
                Offer::Held
            ));
        }
        assert_eq!(q.held_objects(), 20);
        // Lower the object budget to 5; nothing drains until we converge.
        q.set_policy(QuarantinePolicy {
            max_bytes: 64 * 20,
            max_objects: 5,
            ..QuarantinePolicy::DEFAULT
        });
        assert_eq!(q.held_objects(), 20, "set_policy alone does not drain");
        let mut freed = 0;
        loop {
            let b = q.drain_excess();
            if b.as_slice().is_empty() {
                break;
            }
            freed += b.as_slice().len();
            assert!(q.check_invariants());
        }
        assert_eq!(q.held_objects(), 5, "converged exactly to the new budget");
        assert_eq!(freed, 15, "freed exactly the excess");
        // Already within budget ⇒ a further converge is a no-op.
        assert!(q.drain_excess().as_slice().is_empty());
    }

    #[test]
    fn quarantine_check_invariants_tracks_the_ring() {
        // The Appendix-B checker holds across offers, an eviction, and drains — the
        // accounting atomics stay exactly in step with the ring contents.
        let q = Quarantine::new();
        q.set_policy(QuarantinePolicy {
            max_bytes: 10 * 64,
            max_objects: 8,
            ..QuarantinePolicy::DEFAULT
        });
        assert!(q.check_invariants(), "empty quarantine is well-formed");
        for k in 0..6 {
            let _ = offer(&q, small_entry(0x1000 + k * 0x40, 64));
            assert!(q.check_invariants(), "well-formed after each offer");
        }
        // Force an eviction by overflowing the byte budget, then re-check.
        let _ = offer(&q, small_entry(0x9000, 64));
        assert!(q.check_invariants(), "well-formed after an eviction");
        // Drain in batches; the invariant holds at every step down to empty.
        while !q.drain_batch().as_slice().is_empty() {
            assert!(q.check_invariants());
        }
        assert!(q.check_invariants());
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

    #[test]
    fn guard_sampler_is_reproducible_given_a_seed() {
        // §30.4 (W19-3): "disable randomization unless seeded" — two samplers
        // reseeded to the same value produce *identical* guard decisions, and a
        // different seed diverges. This is the determinism the differential runner
        // relies on (W21-2). A pure test — no global state.
        let decisions = |seed: u64| -> Vec<bool> {
            let s = GuardSampler::new();
            s.set_rate(8);
            s.set_seed(seed);
            (0..2000).map(|_| s.sampled()).collect()
        };
        let a = decisions(0xDEAD_BEEF_0000_0001);
        let b = decisions(0xDEAD_BEEF_0000_0001);
        assert_eq!(a, b, "same seed ⇒ identical guard decisions (reproducible)");
        let c = decisions(0x1234_5678_9ABC_DEF0);
        assert_ne!(a, c, "a different seed ⇒ a different decision stream");
        // The reproducible stream is still a real ~1/8 sample (not all-off/all-on).
        let hits = a.iter().filter(|&&h| h).count();
        assert!(
            hits > 0 && hits < a.len(),
            "seeded stream still samples ~1/rate"
        );
    }

    #[test]
    fn quarantine_random_evict_is_reproducible_given_a_seed() {
        // §30.4 (W19-3): the quarantine's random-eviction RNG is likewise seedable,
        // so a hardened deterministic run evicts the same victims each time. Two
        // quarantines seeded alike, driven by the same offers, evict the same
        // objects in the same order; a different seed diverges. Pure — no globals.
        let run = |seed: u64| -> u64 {
            let q = Quarantine::new();
            q.set_policy(QuarantinePolicy {
                max_objects: 4,
                random_evict: true,
                ..QuarantinePolicy::DEFAULT
            });
            q.set_seed(seed);
            let mut fingerprint = 0u64;
            let mut step = 0u64;
            // Offer more than the object budget so random eviction fires repeatedly.
            for i in 1..=32u64 {
                let entry = QuarantineEntry {
                    user_ptr: ((i as usize) * 64) as *mut u8,
                    span: core::ptr::null(),
                    index: 0,
                    arena: crate::ids::ArenaId::DEFAULT,
                    bytes: 64,
                };
                let mut batch = EvictBatch::new();
                let _ = q.offer(entry, &mut batch);
                for e in batch.as_slice() {
                    step += 1;
                    fingerprint = fingerprint.wrapping_add((e.user_ptr as u64).wrapping_mul(step));
                }
            }
            fingerprint
        };
        assert_eq!(
            run(0xABCD_0001),
            run(0xABCD_0001),
            "same seed ⇒ same evictions"
        );
        assert_ne!(
            run(0xABCD_0001),
            run(0x1357_9BDF),
            "a different seed ⇒ a different eviction order"
        );
    }
}

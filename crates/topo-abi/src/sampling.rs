// SPDX-License-Identifier: MIT
//! Live heap/lifetime **sampling glue** (§31.4, plan 07 W17-3) — the platform side of the
//! pure machinery in [`topo_core::sampling`], wired into the allocation path so W14
//! placement profiles ([`topo_core::SiteProfileTable`]) are fed from *real* traffic.
//!
//! The split keeps the core `no_std` and this crate's `std`/`libc` glue minimal:
//!
//! * **Hot-path decision (W17-3a).** [`on_alloc`] / [`on_free`] are called from
//!   [`AnyAllocator`](crate::AnyAllocator)'s `allocate`/`free`/`realloc`, so *every*
//!   public entry point (C ABI, `topo_*x`, [`GlobalAlloc`](core::alloc::GlobalAlloc),
//!   arena API) samples uniformly. When profiling is **off** (the default) each is a
//!   single atomic load — the default artifact's path is unchanged. When on, the
//!   decision is a thread-local [`Sampler`] (no lock, no syscall, no allocation); the
//!   free path's "is this sampled?" test is a lock-free [`SampleBloom`] so the common
//!   (non-sampled) free never takes the sampled-set lock (§31.4, DD-1 *F2*).
//! * **Alloc-free stack capture (W17-3b).** On a fired sample the slow path captures the
//!   call stack with `libc::backtrace` straight into a fixed thread-local [`StackBuf`] —
//!   no allocation, no recursion into the allocator — and folds it to a [`StackId`]. The
//!   one-time glibc unwinder warm-up runs at *enable* time (outside any sampled
//!   allocation), so the capture itself never allocates.
//! * **Sampled-object lifecycle (W17-3c).** A fired alloc records into the global
//!   [`SampledObjects`] set + profile table; a sampled free resolves the object's
//!   lifetime and folds it in; [`fold_censored`] right-censors still-live objects at dump.
//!
//! **Re-entrancy (Appendix F / §31.4).** A thread-local guard makes the slow path
//! non-re-entrant: any allocation attempted while sampling (there are none — the slow
//! path only touches fixed buffers, a lock, the monotonic clock, and `backtrace`) is
//! observed and skipped, so the unwinder can never re-enter the allocator it samples. The
//! `sampler_never_reenters_the_allocator` integration test pins it.

use std::boxed::Box;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use topo_core::{
    classify, predicted_usable_size, PlacementStats, RequestFlags, RequestKind, SampleBloom,
    SampleConfig, SampledObjects, SampledRecord, Sampler, SiteProfileTable, SizeClassDist,
    StackBuf, StackId,
};

/// Distinct allocation sites the profile table holds (§24.4). Bounded and inline; see the
/// module note on capacity vs. the one-time enable-time allocation.
const PROFILE_SITES: usize = 1024;
/// Live sampled objects tracked at once (W17-3c). With the default 1 MiB sample rate this
/// covers a multi-GiB live heap; excess samples are dropped (counted), never evicted.
const LIVE_SAMPLES: usize = 2048;

/// The global sampled state (profiles + live-object set), guarded by one mutex. Touched
/// only by the rare *sampled* slow path, never the hot decision (§31.4).
struct SamplingState {
    profiles: SiteProfileTable<PROFILE_SITES>,
    objects: SampledObjects<LIVE_SAMPLES>,
    /// Monotonic (wrapping) count of fired allocation samples, driving the publish /
    /// bloom-refresh cadences below — so neither runs on every sample.
    fired_samples: u32,
}

impl SamplingState {
    fn new() -> Self {
        SamplingState {
            profiles: SiteProfileTable::new(),
            objects: SampledObjects::new(),
            fired_samples: 0,
        }
    }
}

/// Republish the learned per-bucket placement consensus to the engine every this many fired
/// allocation samples (the live W14 learn → place loop), amortizing its `O(sites)` scan.
const PUBLISH_EVERY: u32 = 32;

/// Rebuild the membership filter every this many fired samples, bounding its false-positive
/// rate over a long run (else the free path's lock-free reject slowly degrades, §31.4 / DD-1).
const BLOOM_REFRESH_EVERY: u32 = 1024;

/// `true` once profiling has been enabled. The hot path reads this first; `false` (the
/// default) short-circuits before touching any other sampling state.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The configured mean sample interval (bytes); `0` ⇒ disabled.
static RATE_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bumped on every rate change so each thread's [`Sampler`] re-syncs lazily.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Lock-free membership filter so the free hot path avoids the sampled-set lock (DD-1 F2).
static BLOOM: SampleBloom = SampleBloom::new();
/// The sampled state, boxed and initialized at first enable (off the hot path), so the
/// default build allocates none of it and the binary carries no large static.
static STATE: OnceLock<Mutex<Box<SamplingState>>> = OnceLock::new();
/// Monotonic process clock origin, for lifetime/rate timestamps (ms).
static CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();
/// Per-thread sampler seed source (each thread gets an independent stream).
static SEED: AtomicU64 = AtomicU64::new(0x5eed_0001);

thread_local! {
    /// This thread's sampling counter, paired with the [`GENERATION`] it was built for so
    /// a runtime rate change re-syncs it lazily.
    static TLS_SAMPLER: RefCell<(u64, Sampler)> =
        RefCell::new((u64::MAX, Sampler::new(SampleConfig::DISABLED, 0)));
    /// This thread's fixed stack-capture buffer (reused per sample; never grows).
    static TLS_CAPTURE: RefCell<StackBuf> = const { RefCell::new(StackBuf::new()) };
    /// Set while this thread is inside the sampled slow path — makes it non-re-entrant.
    static IN_SAMPLER: Cell<bool> = const { Cell::new(false) };
}

/// Milliseconds since the process clock origin (lazily set on first use).
#[inline]
fn now_ms() -> u64 {
    let origin = CLOCK_ORIGIN.get_or_init(Instant::now);
    origin.elapsed().as_millis() as u64
}

/// A fresh, well-separated per-thread sampler seed.
#[inline]
fn next_seed() -> u64 {
    SEED.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
}

/// The global sampled state, initialized on demand (called only from enable / the slow
/// path, both of which run after the engine exists).
#[inline]
fn state() -> &'static Mutex<Box<SamplingState>> {
    STATE.get_or_init(|| Mutex::new(Box::new(SamplingState::new())))
}

/// RAII re-entrancy guard: marks this thread "inside the sampler" so a nested allocation
/// (there should be none) is detected and skipped rather than recursing.
struct SamplerGuard;

impl SamplerGuard {
    /// Enter the sampled region, or `None` if already inside it (caller must skip).
    #[inline]
    fn enter() -> Option<SamplerGuard> {
        IN_SAMPLER.with(|g| {
            if g.get() {
                None
            } else {
                g.set(true);
                Some(SamplerGuard)
            }
        })
    }
}

impl Drop for SamplerGuard {
    #[inline]
    fn drop(&mut self) {
        IN_SAMPLER.with(|g| g.set(false));
    }
}

/// Whether this thread is currently executing the sampler slow path (the re-entrancy
/// guard is set). Exposed for the re-entrancy integration test.
#[inline]
pub fn in_sampler() -> bool {
    IN_SAMPLER.with(|g| g.get())
}

// ---------------------------------------------------------------------------
// Enable / configure (§32.1 env + runtime control; §31.3 "profile.start")
// ---------------------------------------------------------------------------

/// Reset the per-thread sampler **seed base** (§30.4, W19-3 deterministic mode):
/// threads built after this draw their independent sampling streams from the new
/// origin, so a deterministic run produces reproducible sampling. A single-threaded
/// replay (one `next_seed` draw) is then fully reproducible. Existing threads'
/// samplers re-sync lazily on the next rate change; for a fresh deterministic run
/// set the seed before arming sampling.
pub fn set_base_seed(seed: u64) {
    SEED.store(seed, Ordering::Relaxed);
}

/// The current per-thread sampler seed base. Observability for the deterministic-mode
/// transitions (§30.4): it is what distinguishes a seed-derived reproducible stream from
/// an entropy-derived unpredictable one.
pub fn base_seed() -> u64 {
    SEED.load(Ordering::Relaxed)
}

/// Set the mean sample interval in bytes (`0` disables). Enabling warms up the unwinder
/// once (outside any sampled allocation) and initializes the sampled state, so the slow
/// path is allocation-free thereafter. Runtime-safe: a change re-syncs every thread's
/// sampler lazily via a generation counter.
pub fn set_rate(rate_bytes: u64) {
    set_rate_with(rate_bytes, crate::global());
}

/// [`set_rate`] against an explicitly supplied engine. The startup path runs inside
/// `GLOBAL.get_or_init`, where calling the crate's private `global()` would re-enter the
/// still-running `OnceLock` and park the thread on its own initialization — a hang at
/// the process's first allocation. Public entry points pass `crate::global()`.
pub fn set_rate_with(rate_bytes: u64, engine: Option<&crate::AnyAllocator>) {
    if rate_bytes != 0 {
        // Initialize the state and warm the unwinder *before* arming, so the first sample
        // never triggers a first-use allocation inside the guarded slow path.
        let _ = state();
        warm_up_unwinder();
        let _ = now_ms(); // pin the clock origin off the hot path.
    }
    RATE_BYTES.store(rate_bytes, Ordering::Relaxed);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    // Release so a reader that sees ENABLED also sees STATE/RATE (paired Acquire below).
    ENABLED.store(rate_bytes != 0, Ordering::Release);
    if rate_bytes == 0 {
        // Disabling reverts the allocation path to default placement: drop the learned
        // hints so the engine stops applying them (the default path is then byte-for-byte
        // unchanged).
        if let Some(eng) = engine {
            eng.clear_learned_hints();
        }
    }
}

/// Read `$TOPOMALLOC_SAMPLE_RATE` (mean bytes between samples) at startup and enable
/// sampling if it is a non-zero integer (§32.1 env config). Called once during global
/// allocator init (under the bootstrap guard, so any setup allocation is safe).
///
/// Takes the engine by reference (see [`set_rate_with`]): the startup path runs inside
/// `GLOBAL.get_or_init`, so a nested `global()` would deadlock.
pub fn init_from_env(engine: &crate::AnyAllocator) {
    if let Ok(v) = std::env::var("TOPOMALLOC_SAMPLE_RATE") {
        if let Ok(rate) = v.trim().parse::<u64>() {
            set_rate_with(rate, Some(engine));
        }
    }
}

/// Run the platform unwinder once into a throwaway buffer so glibc's first-use `dlopen`
/// (which *does* allocate) happens here — never inside the guarded, must-not-allocate
/// sampled path.
fn warm_up_unwinder() {
    let mut buf = StackBuf::new();
    capture_into(&mut buf);
}

// ---------------------------------------------------------------------------
// Hot-path hooks (called from AnyAllocator::{allocate, free, realloc})
// ---------------------------------------------------------------------------

/// Sample hook for a completed allocation of `size` bytes at `ptr` under `flags`. A
/// no-op (one atomic load) when profiling is off; otherwise a thread-local Poisson
/// decision, with the capture/record work only on a fired sample.
#[inline]
pub fn on_alloc(ptr: *mut u8, size: usize, align: usize, flags: RequestFlags) {
    if ptr.is_null() || !ENABLED.load(Ordering::Acquire) {
        return;
    }
    // Never sample our own slow-path work (defensive; the slow path doesn't allocate).
    if in_sampler() {
        return;
    }
    let fire = TLS_SAMPLER.with(|t| {
        let mut t = t.borrow_mut();
        let cur_gen = GENERATION.load(Ordering::Relaxed);
        if t.0 != cur_gen {
            let rate = RATE_BYTES.load(Ordering::Relaxed);
            t.1 = Sampler::new(
                SampleConfig {
                    sample_rate_bytes: rate,
                },
                next_seed(),
            );
            t.0 = cur_gen;
        }
        t.1.should_sample(size)
    });
    if fire {
        sample_alloc_slow(ptr, size, align, flags);
    }
}

/// The rare fired-sample path: capture the site, resolve a timestamp, and record the
/// allocation into the profile table + live-object set. Allocation-free; runs under the
/// re-entrancy guard.
#[cold]
fn sample_alloc_slow(ptr: *mut u8, size: usize, align: usize, flags: RequestFlags) {
    let Some(_guard) = SamplerGuard::enter() else {
        return;
    };
    let stack_id = capture_stack_id();
    if !stack_id.is_known() {
        return; // un-attributed sample: nothing useful to record.
    }
    let now = now_ms();
    let bucket = bucket_of(size, align, flags);
    let hotness = flags.hints().hotness;
    // The usable size to record for this sample (§31.5 split, W17-4): small objects feed the
    // *sampled* estimate with their real page/slab waste; medium/large record zero waste because
    // the large path counts their page-tail exactly (see [`sampled_usable`]).
    let usable = sampled_usable(bucket, size, align, flags);
    let rec = SampledRecord {
        stack_id,
        bytes: size as u64,
        usable,
        alloc_ms: now,
    };
    let st = state();
    let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
    g.profiles
        .record_alloc(stack_id, bucket, size as u64, hotness, now);
    g.objects.on_alloc(ptr as usize, rec);
    // Publish to the lock-free membership filter *before* releasing the lock, so a concurrent
    // free of this object never sees the bloom bit without the precise record behind it.
    BLOOM.insert(ptr as usize);
    g.fired_samples = g.fired_samples.wrapping_add(1);
    // Refresh the live learn → place loop on a bounded cadence (the consensus scan reads the
    // table under this lock and writes the engine's lock-free hint atomics — no extra lock,
    // no allocation).
    if g.fired_samples.is_multiple_of(PUBLISH_EVERY) {
        if let Some(eng) = crate::global() {
            eng.publish_learned_hints(&g.profiles);
        }
    }
    // Periodically rebuild the membership filter from the live set so its false-positive rate
    // stays bounded. A concurrent sampled free during the brief rebuild window may be missed
    // (the object lingers and is later right-censored) — a bounded, accepted inaccuracy.
    if g.fired_samples.is_multiple_of(BLOOM_REFRESH_EVERY) {
        BLOOM.reset();
        g.objects.for_each_addr(|a| BLOOM.insert(a));
    }
}

/// Sample hook for a free of `ptr`. Lock-free fast reject via the [`SampleBloom`]; only a
/// (rare) maybe-positive consults the sampled set under the lock to resolve the object's
/// lifetime. A no-op when profiling is off.
#[inline]
pub fn on_free(ptr: *mut u8) {
    if ptr.is_null() || !ENABLED.load(Ordering::Acquire) {
        return;
    }
    if !BLOOM.maybe_contains(ptr as usize) || in_sampler() {
        return; // definitely not sampled (the common case): no lock, no work.
    }
    sample_free_slow(ptr);
}

/// The rare sampled-free path: resolve and remove the object's record, folding its
/// completed lifetime into its site profile.
#[cold]
fn sample_free_slow(ptr: *mut u8) {
    let Some(_guard) = SamplerGuard::enter() else {
        return;
    };
    let now = now_ms();
    let st = state();
    let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rec) = g.objects.on_free(ptr as usize) {
        let age = now.saturating_sub(rec.alloc_ms);
        g.profiles.record_free(rec.stack_id, age, rec.bytes, now);
    }
}

/// Remove `ptr`'s sampled record **without** folding a lifetime — for a `realloc` that is
/// about to free or move it. `None` when nothing was sampled (the common case).
///
/// A `realloc` cannot simply call [`on_free`] after the fact. The core frees `ptr` inside
/// the call, so between that free and a post-hoc `on_free(ptr)` another thread can allocate
/// the same address and, if sampled, publish a record for it — `SampledObjects::on_alloc`
/// overwrites the duplicate address, and the post-hoc removal then retires *that thread's
/// still-live* allocation, permanently mis-recording sampled live bytes and feeding the W14
/// learn→place loop from corrupted data. Taking the record **before** the call closes the
/// window: the address is ours until we hand it back.
pub(crate) fn take_for_realloc(ptr: *mut u8) -> Option<SampledRecord> {
    if ptr.is_null() || !ENABLED.load(Ordering::Acquire) {
        return None;
    }
    if !BLOOM.maybe_contains(ptr as usize) || in_sampler() {
        return None; // definitely not sampled: no lock, no work.
    }
    let _guard = SamplerGuard::enter()?;
    let st = state();
    let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
    g.objects.on_free(ptr as usize)
}

/// Fold a [`take_for_realloc`] record's completed lifetime into its site profile — the
/// realloc succeeded, so the original object is genuinely gone.
pub(crate) fn retire_taken(rec: SampledRecord) {
    let Some(_guard) = SamplerGuard::enter() else {
        return;
    };
    let now = now_ms();
    let st = state();
    let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
    let age = now.saturating_sub(rec.alloc_ms);
    g.profiles.record_free(rec.stack_id, age, rec.bytes, now);
}

/// Put a [`take_for_realloc`] record back — the realloc **failed**, so §25.1 leaves the
/// original allocation live and its sample must survive untouched.
pub(crate) fn restore_taken(ptr: *mut u8, rec: SampledRecord) {
    let Some(_guard) = SamplerGuard::enter() else {
        return;
    };
    let st = state();
    let mut g = st.lock().unwrap_or_else(|e| e.into_inner());
    // `on_alloc_restore`, not `on_alloc`: this record was already tracked and its object
    // is still live, so the load-cap drop that is correct for a *new* sample would here
    // erase a live allocation from sampled accounting for good.
    if g.objects.on_alloc_restore(ptr as usize, rec) {
        BLOOM.insert(ptr as usize);
    }
}

/// Map a request to its [`SizeClassDist`] bucket: the small size-class index, or the
/// medium / large sentinel.
#[inline]
fn bucket_of(size: usize, align: usize, flags: RequestFlags) -> u16 {
    match classify(size, align, flags.raw()).map(|r| r.kind) {
        Some(RequestKind::Small { sc, .. }) => sc.index() as u16,
        Some(RequestKind::Medium { .. }) => SizeClassDist::BUCKET_MEDIUM,
        Some(RequestKind::Large { .. }) | None => SizeClassDist::BUCKET_LARGE,
    }
}

/// The `usable` size to record for a sample (§31.5 split, W17-4). For a **small** object it is
/// the true predicted usable (the §10.3 `nallocx` oracle, computed purely from the request — no
/// allocator call-back, so the must-not-allocate sampled path stays alloc-free, §31.4), so the
/// object's slab/page waste `usable − requested` feeds the *sampled* fragmentation estimate. For
/// a **medium/large** object it is `size` (zero waste): that allocation's page-tail is tracked
/// *exactly* by the large path (every live descriptor), so counting it in the sampled estimate
/// too would double-count the same bytes in both the sampled and exact fragmentation fields. The
/// `predicted_usable_size` `None` arm cannot occur (the allocation succeeded) but falls back to
/// `size` (zero waste) for total safety.
#[inline]
fn sampled_usable(bucket: u16, size: usize, align: usize, flags: RequestFlags) -> u64 {
    if bucket < SizeClassDist::BUCKET_MEDIUM {
        predicted_usable_size(size, align, flags).unwrap_or(size) as u64
    } else {
        size as u64
    }
}

/// Capture the current call stack into the thread-local buffer and fold it to a
/// [`StackId`]. Uses the glibc `backtrace` unwinder, which writes return addresses into a
/// caller-provided fixed array — **no allocation** (after the enable-time warm-up). On
/// platforms without it, returns [`StackId::UNKNOWN`] (sampling still counts, but is
/// un-attributed) so the build stays portable.
#[inline]
fn capture_stack_id() -> StackId {
    TLS_CAPTURE.with(|c| {
        let mut buf = c.borrow_mut();
        capture_into(&mut buf);
        buf.stack_id()
    })
}

/// Fill `buf` with the current return-address stack via `libc::backtrace`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn capture_into(buf: &mut StackBuf) {
    buf.clear();
    let frames = buf.frames_mut();
    // SAFETY: `libc::backtrace` writes at most `len` return-address pointers into the
    // caller-provided buffer and returns the count. `frames` is a fixed `[usize;
    // MAX_STACK_FRAMES]`; `usize` and `*mut c_void` are layout-compatible (both
    // pointer-width), so the cast is sound, and the count is clamped by `set_len`.
    let n = unsafe {
        libc::backtrace(
            frames.as_mut_ptr().cast::<*mut core::ffi::c_void>(),
            frames.len() as core::ffi::c_int,
        )
    };
    buf.set_len(if n > 0 { n as usize } else { 0 });
}

/// Portable fallback: no unwinder available, so samples are un-attributed.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn capture_into(buf: &mut StackBuf) {
    buf.clear();
}

// ---------------------------------------------------------------------------
// Observability (the plan-07 tax) + dump (§31.3)
// ---------------------------------------------------------------------------

/// A [`PlacementStats`] snapshot of the learning policy, for the stats snapshot and the
/// `topo.placement.*` control namespace. All-zero when profiling was never enabled.
pub fn placement_stats() -> PlacementStats {
    match STATE.get() {
        Some(m) => {
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            g.profiles.stats()
        }
        None => PlacementStats::default(),
    }
}

/// The **sampled internal-fragmentation** estimate (§31.5, W17-4): the sum over the live
/// sampled objects of `usable − requested`. Computed on demand over the live set (the slow
/// stats path), so it is exact for the *current* live sample by construction — no
/// incremental accumulator to drift across the duplicate-overwrite / capacity-drop /
/// censor-at-dump edges. `0` when profiling was never enabled (no samples ⇒ no estimate).
///
/// This is a *sampled* metric: it is the internal fragmentation of the objects the sampler
/// happens to be tracking, not a scaled estimate of the whole heap (§31.5 "MAY sample it in
/// performance mode"). With sampling off it is `0`, exactly as the rest of the profiler.
pub fn sampled_internal_fragmentation_bytes() -> u64 {
    match STATE.get() {
        Some(m) => {
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            let mut frag: u64 = 0;
            g.objects.for_each(|rec| {
                frag = frag.saturating_add(rec.usable.saturating_sub(rec.bytes));
            });
            frag
        }
        None => 0,
    }
}

/// Render the live heap/lifetime profile as JSON (§31.3 "profiles dumpable"): the policy
/// summary plus one entry per tracked site (`stack_id`, dominant lifetime class, mean
/// hotness, confidence, sampled live bytes, sample counts, and rates). Folds still-live
/// objects in as right-censored lifetimes first, so the dump is not biased toward
/// short-lived objects. Allocates a `String` — a diagnostic call, never the hot path.
pub fn dump_json() -> String {
    use std::fmt::Write as _;
    fold_censored();
    let mut out = String::new();
    let _ = write!(
        out,
        "{{\"enabled\":{},\"rate_bytes\":{},",
        enabled(),
        rate_bytes()
    );
    match STATE.get() {
        None => {
            let _ = write!(out, "\"sites_tracked\":0,\"sites\":[]}}");
        }
        Some(m) => {
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            let s = g.profiles.stats();
            let _ = write!(
                out,
                "\"sites_tracked\":{},\"confident_sites\":{},\"sampled_live_bytes\":{},\"sites\":[",
                s.sites_tracked, s.confident_sites, s.sampled_live_bytes
            );
            let mut first = true;
            g.profiles.for_each_profile(|p| {
                let (allocs, frees) = p.counts();
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = write!(
                    out,
                    "{{\"stack_id\":{},\"lifetime\":\"{}\",\"hotness\":{},\"confidence_bp\":{},\
                     \"dominant_size_bucket\":{},\"sampled_live_bytes\":{},\"alloc_samples\":{},\
                     \"free_samples\":{},\"alloc_rate\":{},\"free_rate\":{}}}",
                    p.stack_id().0,
                    p.dominant_lifetime().as_str(),
                    p.hotness_estimate(),
                    p.confidence_bp(),
                    p.dominant_bucket(),
                    p.sampled_live_bytes(),
                    allocs,
                    frees,
                    p.allocation_rate(),
                    p.free_rate(),
                );
            });
            let _ = write!(out, "]}}");
        }
    }
    out
}

/// Whether profiling is currently enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// The current mean sample interval (bytes); `0` if disabled.
pub fn rate_bytes() -> u64 {
    RATE_BYTES.load(Ordering::Relaxed)
}

/// Fold every still-live sampled object into its site profile as a **right-censored**
/// lifetime (§31.4): its true lifetime is at least its current age. Called before a
/// profile dump so long-lived objects are not under-counted. No-op when disabled.
pub fn fold_censored() {
    if let Some(m) = STATE.get() {
        let now = now_ms();
        let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
        // Collect ages first (the closure borrows `objects`; recording borrows
        // `profiles`), then fold — both live behind the one lock.
        let g = &mut **g;
        let objects = &g.objects;
        let profiles = &mut g.profiles;
        objects.for_each(|rec| {
            let age = now.saturating_sub(rec.alloc_ms);
            profiles.record_censored(rec.stack_id, age);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests drive the pure decision/record surface directly (the cross-crate live
    // tests, which exercise the real malloc/free path, live in `tests/tests/placement.rs`).

    #[test]
    fn disabled_by_default_is_a_noop() {
        // A fresh process state: nothing enabled, every hook a no-op.
        assert!(!enabled());
        on_alloc(0x1000 as *mut u8, 64, 8, RequestFlags::NONE);
        on_free(0x1000 as *mut u8);
        assert_eq!(placement_stats(), PlacementStats::default());
    }

    #[test]
    fn bucket_mapping_is_total() {
        // Small / medium / large all map to a defined bucket; a bad request maps to LARGE
        // (the conservative catch-all) rather than panicking.
        assert!(bucket_of(64, 8, RequestFlags::NONE) < SizeClassDist::BUCKET_MEDIUM);
        assert_eq!(
            bucket_of(usize::MAX, 1, RequestFlags::NONE),
            SizeClassDist::BUCKET_LARGE
        );
    }

    #[test]
    fn sampled_usable_excludes_medium_and_large_from_fragmentation() {
        // PR #21 review (§31.5 split, W17-4): a small sample records its real usable, so its
        // slab/page waste feeds the *sampled* estimate; a medium/large sample records zero waste
        // (`usable == bytes`), because the large path counts that page-tail *exactly* — recording
        // it here too would double-count it in both the sampled and exact fragmentation fields.
        // Small: real usable ≥ size (a 100-byte request rounds up to a slab class with slack).
        let sb = bucket_of(100, 8, RequestFlags::NONE);
        assert!(
            sb < SizeClassDist::BUCKET_MEDIUM,
            "100 bytes is a small class"
        );
        assert!(
            sampled_usable(sb, 100, 8, RequestFlags::NONE) >= 100,
            "a small sample records its true (slack-carrying) usable"
        );
        // Medium: zero waste recorded.
        let medium = 80_000usize;
        let mb = bucket_of(medium, 16, RequestFlags::NONE);
        assert_eq!(mb, SizeClassDist::BUCKET_MEDIUM);
        assert_eq!(
            sampled_usable(mb, medium, 16, RequestFlags::NONE),
            medium as u64,
            "a medium sample records zero waste (tracked exactly by the large path)"
        );
        // Large: zero waste recorded, even with a non-page-multiple request that genuinely wastes.
        let large = 4 * 1024 * 1024 + 123;
        let lb = bucket_of(large, 16, RequestFlags::NONE);
        assert_eq!(lb, SizeClassDist::BUCKET_LARGE);
        assert_eq!(
            sampled_usable(lb, large, 16, RequestFlags::NONE),
            large as u64,
            "a large sample records zero waste (no sampled/exact double-count)"
        );
    }
}

// SPDX-License-Identifier: MIT
//! Deterministic test mode (§30.4, plan 08 W19-3).
//!
//! A pure, `no_std`, process-global control block that makes a run **reproducible**
//! so a captured §33.7 trace replays identically (the W21-2 differential runner's
//! prerequisite, DD-1 failure mode F1). Per §30.4 it lets the allocator:
//!
//! * **disable randomization unless seeded** — every randomized decision (the
//!   W18-4 guard-page sampler, the W18-3 quarantine evictor, the W17-3 heap
//!   sampler) derives its RNG stream from a single global [`seed`] via
//!   [`domain_seed`], instead of an independent fixed/process-unique seed, so two
//!   runs with the same seed make the same choices;
//! * **use a deterministic cache refill order** — the front-end caches are already
//!   strict LIFO (no hashing/address-order), reproducible by construction;
//! * **use reproducible sampling** — the heap sampler's per-thread seed base is
//!   derived from [`domain_seed`] in deterministic mode, so a **single-threaded**
//!   run (the differential runner replays a captured trace single-threaded against
//!   the model — DD-1) draws a reproducible sampling stream. Across *multiple*
//!   concurrent threads the per-thread seeds still depend on thread-creation order,
//!   which is inherently non-deterministic — that is the deliberate boundary, not a
//!   gap: replay is single-threaded;
//! * **optionally force slow paths** — [`force_slow_path`] makes the front-end
//!   refill bypass the transfer cache *and* the central layer skip its empty-span
//!   cache reuse, exercising the backend/activation slow path every time;
//! * **optionally force frequent purging** — [`force_purge`] makes the extent
//!   backend decommit (release) freed backing eagerly instead of retaining it (the
//!   per-free RSS decision; the host-driven W12 release controller takes its decay/
//!   pressure inputs explicitly, preserving its purity, so it is not coupled here);
//! * **expose trace ids for model replay** — [`next_trace_id`] is the canonical
//!   monotonic request-id source for the §33.7 trace grammar; [`reset_trace_ids`]
//!   re-bases it so a fresh run reproduces the same ids.
//!
//! Everything here is a relaxed atomic toggle: the flags are read on cold/slow
//! paths (or behind the sampler's own rate gate), so the `performance` build pays
//! nothing for an idle deterministic mode. Deterministic mode is **off by
//! default** and auto-enabled when the crate is built under the `deterministic-test`
//! profile; it is also runtime-togglable (the `topomalloc_deterministic_*` C
//! surface, W19-3 / W20).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The default deterministic seed — a fixed, non-zero constant, so an enabled
/// deterministic mode is reproducible even before an explicit [`set_seed`].
pub const DEFAULT_SEED: u64 = 0x5EED_C0DE_5EED_C0DE;

/// Whether deterministic mode is active. Initialized to the `deterministic-test`
/// profile (a build under that profile is deterministic out of the box) and
/// runtime-togglable for tests/tools that opt in dynamically.
static ENABLED: AtomicBool = AtomicBool::new(cfg!(feature = "deterministic-test"));
/// The global seed all randomization derives from in deterministic mode.
static SEED: AtomicU64 = AtomicU64::new(DEFAULT_SEED);
/// Force slow paths (bypass fast-path cache reuse) when set.
static FORCE_SLOW_PATH: AtomicBool = AtomicBool::new(false);
/// Force frequent purging (eager release-to-OS of freed backing) when set.
static FORCE_PURGE: AtomicBool = AtomicBool::new(false);
/// Monotonic trace/request-id counter (§33.7). Starts at 1 (0 is reserved as the
/// "no id" sentinel in the grammar's optional fields).
static NEXT_TRACE_ID: AtomicU64 = AtomicU64::new(1);

/// Domain salts so each RNG stream derived from the single global [`seed`] is
/// distinct but reproducible. Distinct primes keep the streams decorrelated.
pub mod salt {
    /// The W18-4 guarded-allocation sampler.
    pub const GUARD: u64 = 0x9E37_79B9_7F4A_7C15;
    /// The W18-3 quarantine evictor.
    pub const QUARANTINE: u64 = 0xC2B2_AE3D_27D4_EB4F;
    /// The W17-3 heap sampler.
    pub const SAMPLER: u64 = 0x1656_67B1_9E37_79F9;
}

/// Whether deterministic mode is active (the `deterministic-test` profile, or a
/// runtime [`set_deterministic`]). Hot paths that consult the force flags or seed
/// can short-circuit on this.
#[inline]
pub fn is_deterministic() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Enable or disable deterministic mode at runtime (§30.4). Enabling does **not**
/// itself reseed live RNG state — the caller (the C control surface) reseeds the
/// global allocator's samplers so randomization becomes seed-derived.
#[inline]
pub fn set_deterministic(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The current global seed.
#[inline]
pub fn seed() -> u64 {
    SEED.load(Ordering::Relaxed)
}

/// Set the global seed (§30.4 "disable randomization unless seeded"). All
/// subsequently-derived [`domain_seed`]s change accordingly; the caller reseeds
/// live samplers to take effect immediately.
#[inline]
pub fn set_seed(s: u64) {
    SEED.store(s, Ordering::Relaxed);
}

/// SplitMix64 finalizer — a strong, allocation-free mix for deriving distinct
/// streams from one seed (no statistical correlation between salts).
#[inline]
const fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Mix a base seed with a domain salt into a decorrelated stream seed. Shared by
/// [`domain_seed`] (deterministic mode's global seed) and
/// [`harden::entropy_seed`](crate::harden::entropy_seed) (the process entropy the
/// randomized security samplers use in ordinary builds), so both derive their
/// per-domain streams the same proven way.
#[inline]
pub(crate) const fn mix_seed(base: u64, domain_salt: u64) -> u64 {
    splitmix64(base ^ splitmix64(domain_salt))
}

/// A reproducible, non-zero seed for an RNG `domain` (see [`salt`]), derived from
/// the global [`seed`]. Distinct salts yield decorrelated streams; the result is
/// never `0` (xorshift/SplitMix64 RNGs require a non-zero state).
#[inline]
pub fn domain_seed(domain_salt: u64) -> u64 {
    let mixed = mix_seed(seed(), domain_salt);
    if mixed == 0 {
        // Vanishingly unlikely, but a zero state would stall an xorshift RNG.
        0x1
    } else {
        mixed
    }
}

/// Whether the slow path is forced (§30.4): the front-end / central layer should
/// bypass its fast-path cache reuse. Off by default.
#[inline]
pub fn force_slow_path() -> bool {
    FORCE_SLOW_PATH.load(Ordering::Relaxed)
}

/// Force (or stop forcing) the slow path.
#[inline]
pub fn set_force_slow_path(on: bool) {
    FORCE_SLOW_PATH.store(on, Ordering::Relaxed);
}

/// Whether frequent purging is forced (§30.4): the extent backend should release
/// freed backing to the OS eagerly instead of retaining it. Off by default.
#[inline]
pub fn force_purge() -> bool {
    FORCE_PURGE.load(Ordering::Relaxed)
}

/// Force (or stop forcing) frequent purging.
#[inline]
pub fn set_force_purge(on: bool) {
    FORCE_PURGE.store(on, Ordering::Relaxed);
}

/// The next trace/request id (§33.7) — a process-global monotonic counter, the
/// canonical id source for trace emission. Lock-free; reproducible in
/// single-threaded deterministic replay (one issuing thread ⇒ a fixed order).
#[inline]
pub fn next_trace_id() -> u64 {
    NEXT_TRACE_ID.fetch_add(1, Ordering::Relaxed)
}

/// The id the next [`next_trace_id`] will return, without consuming it.
#[inline]
pub fn peek_trace_id() -> u64 {
    NEXT_TRACE_ID.load(Ordering::Relaxed)
}

/// Re-base the trace-id counter to 1, so a fresh deterministic run reproduces the
/// same id sequence. For test/replay harnesses only.
#[inline]
pub fn reset_trace_ids() {
    NEXT_TRACE_ID.store(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_seeds_are_reproducible_distinct_and_nonzero() {
        set_seed(0xABCD_1234_5678_9999);
        let g1 = domain_seed(salt::GUARD);
        let q1 = domain_seed(salt::QUARANTINE);
        let s1 = domain_seed(salt::SAMPLER);
        // Distinct streams.
        assert_ne!(g1, q1);
        assert_ne!(q1, s1);
        assert_ne!(g1, s1);
        // Non-zero (a zero state would stall an xorshift RNG).
        assert_ne!(g1, 0);
        assert_ne!(q1, 0);
        assert_ne!(s1, 0);
        // Reproducible for the same seed.
        assert_eq!(domain_seed(salt::GUARD), g1);
        // A different seed gives different streams.
        set_seed(0x1111_2222_3333_4444);
        assert_ne!(domain_seed(salt::GUARD), g1);
        // Restore the default so the global state does not leak across tests.
        set_seed(DEFAULT_SEED);
    }

    // NOTE: the `force_slow_path` / `force_purge` flags are read on allocator hot
    // paths (central empty-cache reuse, extent free), so toggling them here would
    // race the parallel central/extent unit tests in this same process. Their
    // behavioural wiring is tested in the dedicated, serialized integration binary
    // `tests/tests/deterministic.rs`, which runs in its own process.

    #[test]
    fn trace_ids_are_monotonic_and_resettable() {
        reset_trace_ids();
        assert_eq!(peek_trace_id(), 1);
        let a = next_trace_id();
        let b = next_trace_id();
        let c = next_trace_id();
        assert_eq!(a, 1);
        assert!(b > a && c > b);
        reset_trace_ids();
        assert_eq!(next_trace_id(), 1, "reset re-bases the sequence to 1");
    }

    #[test]
    fn deterministic_flag_round_trips() {
        let was = is_deterministic();
        set_deterministic(true);
        assert!(is_deterministic());
        set_deterministic(false);
        assert!(!is_deterministic());
        set_deterministic(was); // restore
    }
}

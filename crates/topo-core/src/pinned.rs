// SPDX-License-Identifier: MIT
//! seLe4n pinned-thread per-core fast path (W7-5; SPEC §36.10 option 1).
//!
//! seLe4n (Raspberry Pi 5 / seL4) is **not** Linux, so it has no `rseq`. §36.10
//! defines an alternative per-core fast-path contract; option 1 — the one this
//! module implements — is **pinned-thread per-core mode**: a thread's CPU
//! affinity is stable while it uses a per-core cache, and the cache is flushed or
//! handed off before affinity changes. Under that contract a thread is the *sole*
//! accessor of its core's slot, so push/pop are lock-free.
//!
//! The same [`FeOutcome`](crate::FeOutcome) front-end contract governs this path
//! as governs the locked baseline and the RSEQ fast path. In particular it keeps
//! the distinct **`Abort`** outcome: the per-core sequence reads the current core
//! from a [`CoreProvider`] (the seLe4n runtime's per-core identity, the analogue
//! of `rseq`'s `cpu_id`) and, if the thread is not on the expected core — a
//! migration or a violated pinning contract — it **aborts with no state change**,
//! committing only the single store when the core is stable across the read. This
//! mirrors the Lean `per_core_cache_abort_no_change` obligation (plan 02 W1-12d,
//! §36.17): an aborted per-core fast path leaves allocator-visible state
//! unchanged.
//!
//! **Soundness boundary.** Under the pinned-thread contract there is no
//! concurrent accessor, so the lock-free read-modify-write is race-free and the
//! core-stability checks are the safety net the SPEC requires for the rare
//! contract violation. A fully migration-atomic commit on a non-pinned thread is
//! §36.10 option 2 (a kernel restartable section); when seLe4n exposes that ABI
//! it slots in behind this same contract. The migration flush/hand-off itself is
//! the caller's responsibility (flush or make the cache unreachable before
//! affinity changes), exercised via [`CpuCache::disable_rseq`](crate::CpuCache)
//! and the cache-drain operations.

use crate::fe::CoreId;

/// The per-core identity oracle for the pinned-thread fast path: the seLe4n
/// runtime's notion of "which core am I on right now". On Linux this would be
/// `sched_getcpu`; on seLe4n it is the pinned-core affinity the runtime tracks.
/// Tests inject a provider that simulates migration to exercise the abort path.
pub trait CoreProvider {
    /// The core the calling thread is currently executing on.
    fn current_core(&self) -> CoreId;
}

/// A provider that always reports the same core — a perfectly-pinned thread (the
/// common §36.10 option-1 case). The fast path never aborts with this provider.
#[derive(Clone, Copy, Debug)]
pub struct FixedCore(pub CoreId);

impl CoreProvider for FixedCore {
    #[inline]
    fn current_core(&self) -> CoreId {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_core_reports_its_core() {
        let p = FixedCore(CoreId(5));
        assert_eq!(p.current_core(), CoreId(5));
        // Stable across calls — models a pinned thread that never migrates.
        assert_eq!(p.current_core(), CoreId(5));
    }
}

// SPDX-License-Identifier: MIT
//! Front-end contract types (W6-4, plan 05).
//!
//! These types define the pop/push contract between the front-end cache and the
//! allocation fast path. [`FeOutcome`] distinguishes the four possible results
//! of a front-end operation: success, an empty cache (needs refill), a full
//! cache (needs flush), or an abort (RSEQ preemption, plan 06). [`CoreId`]
//! identifies a logical CPU for per-CPU cache indexing.
//!
//! The types are intentionally small and self-contained so the front-end
//! contract can be reasoned about in isolation from the cache implementation.

/// Logical CPU/core identifier for per-CPU cache indexing.
///
/// This is distinct from the OS-level CPU id; it is an allocator-internal index
/// into the per-CPU cache array. The mapping from OS CPU to `CoreId` is
/// established at init time (plan 06 RSEQ, W7 topology).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CoreId(pub u32);

impl CoreId {
    /// The default core id (CPU 0), used before topology discovery.
    pub const DEFAULT: CoreId = CoreId(0);

    /// The raw index for array indexing.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Outcome of a front-end pop or push operation (the four-way contract,
/// plan 05 W6-4).
///
/// - `Success(T)` — the operation completed; `T` is the popped address or `()`.
/// - `Empty` — the per-CPU slot is empty; the caller must refill from the
///   transfer cache or central list before retrying.
/// - `Full` — the per-CPU slot is at capacity; the caller must flush to the
///   transfer cache or central list before retrying.
/// - `Abort` — the operation was preempted mid-sequence (RSEQ, plan 06). The
///   caller retries the fast path. Before RSEQ, this variant is never produced.
#[derive(Debug, PartialEq, Eq)]
pub enum FeOutcome<T> {
    /// The operation completed successfully.
    Success(T),
    /// The per-CPU slot was empty (pop) -- needs refill.
    Empty,
    /// The per-CPU slot was full (push) -- needs flush.
    Full,
    /// Preempted mid-RSEQ sequence -- retry the fast path.
    Abort,
}

impl<T> FeOutcome<T> {
    /// Whether the outcome is `Success`.
    #[inline]
    pub fn is_success(&self) -> bool {
        matches!(self, FeOutcome::Success(_))
    }

    /// Whether the outcome is `Empty`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        matches!(self, FeOutcome::Empty)
    }

    /// Whether the outcome is `Full`.
    #[inline]
    pub fn is_full(&self) -> bool {
        matches!(self, FeOutcome::Full)
    }

    /// Whether the outcome is `Abort`.
    #[inline]
    pub fn is_abort(&self) -> bool {
        matches!(self, FeOutcome::Abort)
    }

    /// Extract the success value, panicking if the outcome is not `Success`.
    #[inline]
    pub fn unwrap(self) -> T {
        match self {
            FeOutcome::Success(v) => v,
            FeOutcome::Empty => panic!("called unwrap on FeOutcome::Empty"),
            FeOutcome::Full => panic!("called unwrap on FeOutcome::Full"),
            FeOutcome::Abort => panic!("called unwrap on FeOutcome::Abort"),
        }
    }

    /// Map the success value with `f`, preserving non-success variants.
    #[inline]
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> FeOutcome<U> {
        match self {
            FeOutcome::Success(v) => FeOutcome::Success(f(v)),
            FeOutcome::Empty => FeOutcome::Empty,
            FeOutcome::Full => FeOutcome::Full,
            FeOutcome::Abort => FeOutcome::Abort,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_id_default() {
        assert_eq!(CoreId::DEFAULT, CoreId(0));
        assert_eq!(CoreId::DEFAULT.index(), 0);
    }

    #[test]
    fn core_id_index() {
        assert_eq!(CoreId(7).index(), 7);
        assert_eq!(CoreId(127).index(), 127);
    }

    #[test]
    fn fe_outcome_predicates() {
        let s: FeOutcome<usize> = FeOutcome::Success(42);
        assert!(s.is_success());
        assert!(!s.is_empty());
        assert!(!s.is_full());
        assert!(!s.is_abort());

        let e: FeOutcome<usize> = FeOutcome::Empty;
        assert!(e.is_empty());
        assert!(!e.is_success());

        let f: FeOutcome<usize> = FeOutcome::Full;
        assert!(f.is_full());

        let a: FeOutcome<usize> = FeOutcome::Abort;
        assert!(a.is_abort());
    }

    #[test]
    fn fe_outcome_unwrap_success() {
        let s = FeOutcome::Success(0xDEAD_BEEF_usize);
        assert_eq!(s.unwrap(), 0xDEAD_BEEF);
    }

    #[test]
    #[should_panic(expected = "Empty")]
    fn fe_outcome_unwrap_empty_panics() {
        let e: FeOutcome<usize> = FeOutcome::Empty;
        e.unwrap();
    }

    #[test]
    fn fe_outcome_map() {
        let s = FeOutcome::Success(10usize);
        let mapped = s.map(|v| v * 2);
        assert_eq!(mapped, FeOutcome::Success(20));

        let e: FeOutcome<usize> = FeOutcome::Empty;
        let mapped_e: FeOutcome<u32> = e.map(|_| 0);
        assert!(mapped_e.is_empty());
    }

    #[test]
    fn fe_outcome_eq() {
        assert_eq!(FeOutcome::Success(1u32), FeOutcome::Success(1u32));
        assert_ne!(FeOutcome::Success(1u32), FeOutcome::Success(2u32));
        assert_ne!(FeOutcome::<u32>::Empty, FeOutcome::Full);
        assert_eq!(FeOutcome::<u32>::Abort, FeOutcome::Abort);
    }

    #[test]
    fn fe_outcome_debug_format() {
        let s = FeOutcome::Success(42usize);
        let dbg = format!("{s:?}");
        assert!(dbg.contains("Success"));
        assert!(dbg.contains("42"));
    }
}

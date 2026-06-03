// SPDX-License-Identifier: MIT
//! A tiny executable model that checks well-formedness at trace boundaries
//! (§33.7: "A Lean executable model SHOULD replay traces and check invariants at
//! trace boundaries"). This is the *host-side* seed of that oracle (plan 08
//! W21-2 / plan 02 W1-10): it tracks the live set and rejects the two cardinal
//! ownership violations — two live objects at one address, and freeing a pointer
//! that is not live. The Lean executable model supersedes it as the proof-grade
//! oracle; keeping a host model too lets `tools/trace-replay` run without Lean.

use alloc::collections::BTreeSet;

use crate::trace::TraceRecord;

/// A well-formedness violation found while replaying a trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// Two live objects reported at the same address (live-disjointness, §8.3).
    DoubleAlloc(u64),
    /// A free of a pointer that is not currently live (free-of-not-live, S-009).
    FreeOfUnknown(u64),
}

/// The live-pointer model. `ALLOC` adds a pointer; `FREE` removes it; null is
/// ignored on both sides (`malloc` failure / `free(NULL)`).
#[derive(Default)]
pub struct LiveModel {
    live: BTreeSet<u64>,
}

impl LiveModel {
    /// A fresh, empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one record, checking the invariants. Returns the offending pointer
    /// wrapped in a [`ModelError`] on violation, leaving the model unchanged.
    pub fn apply(&mut self, rec: &TraceRecord) -> Result<(), ModelError> {
        match *rec {
            TraceRecord::Alloc { ptr, .. } => {
                if ptr == 0 {
                    return Ok(()); // allocation failed; nothing becomes live
                }
                if !self.live.insert(ptr) {
                    return Err(ModelError::DoubleAlloc(ptr));
                }
                Ok(())
            }
            TraceRecord::Free { ptr, .. } => {
                if ptr == 0 {
                    return Ok(()); // free(NULL) is a no-op (§9.6)
                }
                if !self.live.remove(&ptr) {
                    return Err(ModelError::FreeOfUnknown(ptr));
                }
                Ok(())
            }
        }
    }

    /// Number of currently live pointers.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc(ptr: u64) -> TraceRecord {
        TraceRecord::Alloc {
            request_id: 0,
            size: 8,
            align: 8,
            arena: 0,
            flags: 0,
            ptr,
            usable_size: 16,
            sc: Some(0),
            span: Some(0),
        }
    }

    fn free(ptr: u64) -> TraceRecord {
        TraceRecord::Free {
            ptr,
            size_hint: 0,
            sc: None,
            span: None,
        }
    }

    #[test]
    fn alloc_then_free_is_wellformed() {
        let mut m = LiveModel::new();
        m.apply(&alloc(0x1000)).unwrap();
        assert_eq!(m.live_count(), 1);
        m.apply(&free(0x1000)).unwrap();
        assert_eq!(m.live_count(), 0);
    }

    #[test]
    fn double_alloc_same_address_is_caught() {
        let mut m = LiveModel::new();
        m.apply(&alloc(0x2000)).unwrap();
        assert_eq!(
            m.apply(&alloc(0x2000)),
            Err(ModelError::DoubleAlloc(0x2000))
        );
    }

    #[test]
    fn free_of_unknown_is_caught() {
        let mut m = LiveModel::new();
        assert_eq!(
            m.apply(&free(0x3000)),
            Err(ModelError::FreeOfUnknown(0x3000))
        );
    }

    #[test]
    fn null_alloc_and_free_are_noops() {
        let mut m = LiveModel::new();
        m.apply(&alloc(0)).unwrap();
        m.apply(&free(0)).unwrap();
        assert_eq!(m.live_count(), 0);
    }
}

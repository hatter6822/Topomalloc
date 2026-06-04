// SPDX-License-Identifier: MIT
//! A tiny executable model that checks well-formedness at trace boundaries
//! (§33.7: "A Lean executable model SHOULD replay traces and check invariants at
//! trace boundaries"). This is the *host-side* seed of that oracle (plan 08
//! W21-2 / plan 02 W1-10): it tracks the live set and rejects the two cardinal
//! ownership violations — two live objects whose `[ptr, ptr+usable_size)` ranges
//! **overlap**, and freeing a pointer that is not live. It is kept in lockstep with
//! the proof-grade Lean oracle (`TopoMalloc.Exec`, whose `apply`/`replay_disjoint`
//! check the same range-disjointness), so the two replayers agree by differential
//! replay; keeping a host model too lets `tools/trace-replay` run without Lean.

use alloc::collections::BTreeMap;

use crate::trace::TraceRecord;

/// A well-formedness violation found while replaying a trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// An allocation whose `[ptr, ptr+usable_size)` range overlaps a live object
    /// (live-disjointness, §8.3) — caught at range granularity, not merely at an
    /// equal base address.
    Overlap(u64),
    /// A free of a pointer that is not currently live (free-of-not-live, S-009).
    FreeOfUnknown(u64),
}

/// Whether `[a, a+a_size)` and `[b, b+b_size)` overlap. Saturating arithmetic keeps
/// the check total even at the very top of the address space.
fn ranges_overlap(a: u64, a_size: u64, b: u64, b_size: u64) -> bool {
    a < b.saturating_add(b_size) && b < a.saturating_add(a_size)
}

/// The live-object model: each live object is the range `[base, base+usable_size)`,
/// keyed by base. `ALLOC` adds a range, rejecting any overlap with a live one; `FREE`
/// removes the range at that base; null is ignored on both sides (`malloc` failure /
/// `free(NULL)`).
#[derive(Default)]
pub struct LiveModel {
    live: BTreeMap<u64, u64>,
}

impl LiveModel {
    /// A fresh, empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one record, checking the invariants. Returns the offending pointer
    /// wrapped in a [`ModelError`] on violation, leaving the model unchanged.
    pub fn apply(&mut self, rec: &TraceRecord) -> Result<(), ModelError> {
        match rec {
            TraceRecord::Alloc {
                ptr, usable_size, ..
            } => {
                let (ptr, size) = (*ptr, *usable_size);
                if ptr == 0 {
                    return Ok(()); // allocation failed; nothing becomes live
                }
                if self
                    .live
                    .iter()
                    .any(|(&b, &s)| ranges_overlap(ptr, size, b, s))
                {
                    return Err(ModelError::Overlap(ptr));
                }
                self.live.insert(ptr, size);
                Ok(())
            }
            TraceRecord::Free { ptr, .. } => {
                let ptr = *ptr;
                if ptr == 0 {
                    return Ok(()); // free(NULL) is a no-op (§9.6)
                }
                if self.live.remove(&ptr).is_none() {
                    return Err(ModelError::FreeOfUnknown(ptr));
                }
                Ok(())
            }
            // Middle/back-end events (REFILL/FLUSH/SPAN_ALLOC/RELEASE) do not
            // change the live-object set; richer conservation checks over them
            // arrive with the cache/central-list model (plan 02/08).
            _ => Ok(()),
        }
    }

    /// Number of currently live objects.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc_sized(ptr: u64, usable_size: u64) -> TraceRecord {
        TraceRecord::Alloc {
            request_id: 0,
            size: 8,
            align: 8,
            arena: 0,
            flags: 0,
            ptr,
            usable_size,
            sc: Some(0),
            span: Some(0),
        }
    }

    fn alloc(ptr: u64) -> TraceRecord {
        alloc_sized(ptr, 16)
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
    fn overlapping_alloc_same_base_is_caught() {
        let mut m = LiveModel::new();
        m.apply(&alloc(0x2000)).unwrap();
        assert_eq!(m.apply(&alloc(0x2000)), Err(ModelError::Overlap(0x2000)));
    }

    #[test]
    fn overlapping_alloc_distinct_base_is_caught() {
        // [0x1000, 0x1020) is live; [0x1010, 0x1020) overlaps it though the bases
        // differ — the range check catches what an address-equality check would miss.
        let mut m = LiveModel::new();
        m.apply(&alloc_sized(0x1000, 32)).unwrap();
        assert_eq!(
            m.apply(&alloc_sized(0x1010, 16)),
            Err(ModelError::Overlap(0x1010))
        );
    }

    #[test]
    fn adjacent_allocs_do_not_overlap() {
        // [0x1000, 0x1010) and [0x1010, 0x1020) touch but do not overlap.
        let mut m = LiveModel::new();
        m.apply(&alloc_sized(0x1000, 16)).unwrap();
        m.apply(&alloc_sized(0x1010, 16)).unwrap();
        assert_eq!(m.live_count(), 2);
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

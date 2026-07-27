// SPDX-License-Identifier: MIT
//! Appendix B invariant checks as runtime code (W19-1, plan 08 DD-2).
//!
//! SPEC Appendix B (§39) enumerates the invariants a debug/hardened build SHOULD
//! check after transitions. This module is the **canonical surface** for them: one
//! callable checker per invariant group, each *total* (terminates on every input)
//! and *side-effect-free* (read-only until the caller's abort decision), exactly
//! as DD-2 requires. They are **first-class runtime code** (overview principle 7),
//! not test-only helpers — debug/hardened builds run them as `debug_assert!`s at
//! transition boundaries, and they back the G-core / G-conc / G-mem / G-arena CI
//! gates.
//!
//! ## Where each checker lives (it lands with the state it checks)
//!
//! The checkers are methods on the types that own the state, so each lands with
//! the workstream that creates it (DD-2). This module gathers them by Appendix-B
//! group and documents the mapping; [`check_b2_cache`] is the one group whose
//! state is not owned by the engine ([`Allocator`](crate::Allocator)) at M1, so it
//! gets a free-function entry point here.
//!
//! ### B.1 Global ([`Allocator::check_invariants`](crate::Allocator::check_invariants))
//! * *every free object reachable from exactly one free structure* —
//!   [`CentralCache::check_invariants`](crate::CentralCache::check_invariants)
//!   (acyclic span lists; `Σ central_free` over the reachable lists equals the bin
//!   aggregate);
//! * *every live object disjoint / every page maps to at most one descriptor* —
//!   the back-end extent map's tiling/disjointness oracle
//!   ([`ExtentManager::check_invariants`](crate::ExtentManager::check_invariants))
//!   for the large path, the slab geometry ([`SlabLayout`](crate::SlabLayout),
//!   verified per span in B.3) for the small path;
//! * *the pagemap agrees with the descriptors* (§30.2 "pagemap agrees with spans")
//!   — every populated pagemap entry names a descriptor whose owned range covers
//!   that page ([`PageMap::check_invariants`](crate::PageMap::check_invariants), the
//!   runtime counterpart to the Lean §17.2 pagemap differential), catching a stale
//!   entry after a descriptor recycle that the descriptor-level oracles above —
//!   which reason about descriptors, not the pagemap structure — cannot see;
//! * *every span/extent belongs to exactly one arena* — the per-arena `used`
//!   accounting and quota gate ([`ArenaTable::check_invariants`]);
//! * *released ranges contain no live objects* — the extent state↔backing coupling
//!   (a `Released`/`Reserved` extent is never backed) in the extent map oracle.
//!
//! ### B.2 Cache ([`check_b2_cache`])
//! * *per-CPU counts ≤ capacity / bytes ≤ hard budget* —
//!   [`CpuCache::check_invariants`](crate::CpuCache::check_invariants);
//! * *transfer batches contain distinct objects / consistent size class* —
//!   [`TransferCache::check_invariants`](crate::TransferCache::check_invariants);
//! * *cached and central-free objects are disjoint* — the W6 residency marker the
//!   lock-free double-free oracle rests on, checked per span under its lock by
//!   [`SpanDescriptor::cached_and_central_free_are_disjoint`](crate::SpanDescriptor::cached_and_central_free_are_disjoint)
//!   and swept by [`Allocator::check_invariants`](crate::Allocator::check_invariants);
//! * *cache flush/refill preserves object count* — asserted at the
//!   [`cache_ops`](crate::cache_ops) flush/refill sites (a transition invariant,
//!   not a state predicate).
//!
//! There is no per-thread term: the §13 thread cache is an *optional alternative*
//! front end for platforms that cannot use per-CPU caches (§13.1), and this tree
//! implements the per-CPU one. See `docs/DECISIONS.md`.
//!
//! ### B.3 Span ([`SpanDescriptor::check_invariants`](crate::SpanDescriptor::check_invariants))
//! * *object ranges fit within span / are disjoint* — verified against the §16.3
//!   [`SlabLayout`](crate::SlabLayout) (disjointness is then structural: `size` is
//!   a multiple of `align`, no inter-object padding);
//! * *size class matches all contained objects* — checked per span (and per listed
//!   span in the central walk);
//! * *free count equals authoritative representation* — `central_free_count ==
//!   popcount(free_bitmap)` (§8.5);
//! * *empty-span detection accounts for local/transfer/central/quarantine* — the
//!   central walk classifies partial vs empty consistently with residency, and the
//!   §16.4 partition bound `live + central_free ≤ object_count` holds (the
//!   implementation's `live_count` already subsumes the cache *and* quarantine
//!   residency — an object removed from central but not yet returned stays counted
//!   as live — so those terms are folded into `live`, with `quarantined ≤ live`);
//! * *generation prevents stale descriptor reuse* — the §17.3 integrity tag (which
//!   covers `generation`) validates under `debug-checks`;
//! * *redzones / free-fill intact* (§30.2 "redzones are intact") — under
//!   `junk-fill`, every central-free object still reads as `harden::FREE_PATTERN`
//!   (the §29.6 invariant), swept on demand by
//!   [`SpanGuard::verify_free_patterns`](crate::SpanGuard::verify_free_patterns) and
//!   per bin by [`CentralCache::verify_free_patterns`](crate::CentralCache::verify_free_patterns)
//!   from [`Allocator::check_invariants`](crate::Allocator::check_invariants) — the
//!   on-demand companion to W18 verify-on-reuse (large objects carry the W18
//!   per-extent canary instead). A no-op when `junk-fill` is compiled out.
//!
//! ### B.4 Hugepage ([`HugePageFiller::check_invariants`](crate::HugePageFiller::check_invariants))
//! * *bins match occupancy / live bytes equal sum / empty has no live / partial
//!   subrelease has no live / coverage from committed state* — H-001..H-005,
//!   modelled in `lean/TopoMalloc/HugePageFiller.lean` and pinned by the
//!   `hugeBinGate`.
//!
//! ### B.5 Arena ([`ArenaTable::check_invariants`])
//! * *state controls allowed operations / reset-destroy drains* — the state gate
//!   and the terminal own-live-bytes-zero check;
//! * *destroyed ids are not reused while stale references can exist* — a
//!   non-`Destroyed` arena carries a nonzero incarnation generation;
//! * *extent hooks installed before hook-owned extents exist* — enforced by
//!   construction (an arena is served from its `HookProvider` region only after the
//!   hooks are installed at create time) and witnessed by the per-arena hooked
//!   backend walk in [`Allocator::check_invariants`](crate::Allocator::check_invariants).
//!
//! [`ArenaTable::check_invariants`]: crate::ArenaTable::check_invariants
//!
//! ## Cadence (DD-2 failure mode F1)
//! Every checker is `O(state)` (the central/span walks are `O(spans ×
//! objects)`), cheap enough to run after every op in unit tests and on a sampled
//! cadence in long stress tests. The `performance` profile compiles none of the
//! `debug_assert!` call sites (they vanish), so it pays nothing
//! ([`runtime_checks_enabled`] reports whether the assertions are live).

use crate::cpu_cache::CpuCache;
use crate::transfer_cache::TransferCache;

/// Whether the Appendix-B runtime invariant checks are wired into runtime
/// assertions in this build — the `debug-checks` feature (implied by the `debug`
/// and `hardened` profiles). The `performance` profile reports `false` and pays
/// nothing. A re-export of [`crate::debug_checks_enabled`] under the Appendix-B
/// name, so callers reading this module need not reach into `profile`.
#[inline]
pub const fn runtime_checks_enabled() -> bool {
    crate::profile::debug_checks_enabled()
}

/// The Appendix B invariant groups (§39), for diagnostics and for naming which
/// group a CI gate or `debug_assert!` is enforcing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    /// B.1 — global (ownership, disjointness, reachability, page↔descriptor).
    Global,
    /// B.2 — cache (per-CPU/thread capacity & budget, transfer distinctness).
    Cache,
    /// B.3 — span (ranges fit/disjoint, sc match, free-count==bitmap, empty).
    Span,
    /// B.4 — hugepage (bins match occupancy, live-bytes sum, subrelease).
    Hugepage,
    /// B.5 — arena (state gate, reset/destroy drain, generation reuse guard).
    Arena,
}

impl Group {
    /// A short, stable name (`"B.1"`..`"B.5"`) for trace/diagnostics.
    pub fn name(self) -> &'static str {
        match self {
            Group::Global => "B.1",
            Group::Cache => "B.2",
            Group::Span => "B.3",
            Group::Hugepage => "B.4",
            Group::Arena => "B.5",
        }
    }

    /// Every group, in order — for iterating the full Appendix-B checklist.
    pub const ALL: [Group; 5] = [
        Group::Global,
        Group::Cache,
        Group::Span,
        Group::Hugepage,
        Group::Arena,
    ];
}

/// Appendix B.2 (cache invariants, W19-1b): the front-end cache layer — per-CPU
/// ([`CpuCache`]) and transfer ([`TransferCache`]) — is well-formed. Total +
/// side-effect-free; the single callable that composes both cache checkers, so a
/// test or the G-conc gate validates the layer in one call.
///
/// The engine's own [`check_invariants`](crate::Allocator::check_invariants) runs
/// these two directly (it owns both structures since W6) *plus* the per-span
/// cached↔central-free disjointness sweep, which needs the span pool this function
/// has no access to. This entry point stays for a caller holding the cache
/// structures without an engine — the `cache_ops` unit tests and the `debug_checks`
/// fuzz target.
pub fn check_b2_cache(cpu: &CpuCache, transfer: &TransferCache) -> bool {
    cpu.check_invariants() && transfer.check_invariants()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::central::{CentralCache, RemoveResult, ANY_PLACE_CLASS};
    use crate::fe::CoreId;
    use crate::ids::{ArenaId, Label, NodeId, SizeClassId, SpanId};
    use crate::pagemap::PageMap;
    use crate::size_class;
    use crate::span::SpanDescriptor;

    const A: ArenaId = ArenaId::DEFAULT;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    #[test]
    fn group_names_are_the_appendix_b_sections() {
        assert_eq!(Group::Global.name(), "B.1");
        assert_eq!(Group::Cache.name(), "B.2");
        assert_eq!(Group::Span.name(), "B.3");
        assert_eq!(Group::Hugepage.name(), "B.4");
        assert_eq!(Group::Arena.name(), "B.5");
        assert_eq!(Group::ALL.len(), 5);
    }

    #[test]
    fn runtime_checks_flag_matches_the_feature() {
        assert_eq!(runtime_checks_enabled(), cfg!(feature = "debug-checks"));
    }

    // --- B.2 cache group: positive + negative ---------------------------------

    #[test]
    fn b2_cache_group_holds_for_a_live_cache_layer() {
        let m = meta(8 * 1024 * 1024);
        let cpu = CpuCache::new();
        cpu.set_active_cpus(2);
        let transfer = TransferCache::new();
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;

        // Exercise both layers, then assert the composed B.2 checker.
        cpu.init_slot(CoreId(0), sc, &m, batch);
        for i in 1..=10usize {
            cpu.fe_push(CoreId(0), A, sc, i, &m);
        }
        transfer.try_push_batch(A, sc, &[100, 200, 300, 400], &m);

        assert!(check_b2_cache(&cpu, &transfer));
        // And each layer individually.
        assert!(cpu.check_invariants());
        assert!(transfer.check_invariants());
    }

    #[test]
    fn b2_transfer_checker_catches_a_duplicate_address() {
        // A duplicate in a transfer bin is a double-listed (double-freed) object —
        // the safety-critical B.2 distinctness clause. The checker must catch it.
        let m = meta(1024 * 1024);
        let transfer = TransferCache::new();
        let sc = SizeClassId::new(0);
        // Push the *same* address twice: a corrupted/double-freed batch.
        transfer.try_push_batch(A, sc, &[0xabc, 0xdef, 0xabc], &m);
        assert!(
            !transfer.check_invariants(),
            "the distinctness check must reject a double-listed object"
        );
    }

    #[test]
    fn b2_transfer_checker_catches_a_null_address() {
        let m = meta(1024 * 1024);
        let transfer = TransferCache::new();
        let sc = SizeClassId::new(0);
        transfer.try_push_batch(A, sc, &[0x111, 0, 0x222], &m);
        assert!(
            !transfer.check_invariants(),
            "a null (0) object address is never valid in a cache"
        );
    }

    #[test]
    fn b2_cpu_checker_catches_an_over_capacity_slot() {
        // B.2 (W19-1b) negative test: the per-CPU capacity bound (`len <=
        // hard_capacity`, §11.5) must be enforced, not just documented. Force a
        // slot past its ceiling and confirm the checker rejects it.
        let m = meta(4 * 1024 * 1024);
        let cpu = CpuCache::new();
        cpu.set_active_cpus(1);
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;
        cpu.init_slot(CoreId(0), sc, &m, batch);
        assert!(cpu.check_invariants());

        let slot = cpu.per_cpu(CoreId(0)).unwrap().slot(sc).unwrap();
        slot.corrupt_len_for_test(slot.hard_capacity() + 1);
        assert!(
            !cpu.check_invariants(),
            "len past the hard capacity must fail the per-CPU checker"
        );
    }

    #[test]
    fn b2_cpu_checker_catches_a_duplicate_entry() {
        // B.2 (W19-1b) negative test: a duplicate in a per-CPU slot is a
        // double-freed object; the distinctness check must reject it.
        let m = meta(4 * 1024 * 1024);
        let cpu = CpuCache::new();
        cpu.set_active_cpus(1);
        let sc = SizeClassId::new(0);
        cpu.init_slot(CoreId(0), sc, &m, size_class::batch(sc) as u32);
        cpu.fe_push(CoreId(0), A, sc, 0x1000, &m);
        cpu.fe_push(CoreId(0), A, sc, 0x1000, &m); // the same object freed twice
        assert!(
            !cpu.check_invariants(),
            "a duplicate per-CPU entry must fail the distinctness check"
        );
    }

    #[test]
    fn b2_residency_checker_catches_a_cached_and_central_free_object() {
        // B.2 (W6, §16.4) negative test: the front-end residency marker and the central
        // free bitmap must name **disjoint** sets. An object in both is the one state
        // that breaks the lock-free double-free oracle — a free observes
        // `is_cached || is_central_free` and reports a double free, while the central
        // list simultaneously believes the object is re-vendable, so it would be handed
        // out to a caller who then "double frees" it. The sweep must reject it.
        let m = meta(4 * 1024 * 1024);
        let span = fresh_span(64, &m);
        // Positive: a fresh span (nothing central-free, nothing cached) is disjoint,
        // and so is one with the two states on *different* objects.
        assert!(span.cached_and_central_free_are_disjoint());
        {
            let g = span.lock();
            assert!(g.central_insert(0));
        }
        assert!(span.try_mark_cached(1));
        assert!(
            span.cached_and_central_free_are_disjoint(),
            "distinct objects in the two states are not a violation"
        );

        // Negative: the *same* object in both sets.
        assert!(span.try_mark_cached(0));
        assert!(
            !span.cached_and_central_free_are_disjoint(),
            "an object that is both cached and central-free must fail the B.2 sweep"
        );
    }

    // --- B.3 span checker: positive + negative --------------------------------

    fn fresh_span(object_count: u32, m: &BumpArena) -> SpanDescriptor {
        // A page-aligned base inside a plausible address window.
        SpanDescriptor::new(
            SpanId(1),
            A,
            SizeClassId::new(0),
            0x4000_0000,
            1,
            object_count,
            0,
            m,
        )
        .expect("bitmap")
    }

    #[test]
    fn b3_span_checker_holds_through_a_normal_lifecycle() {
        let m = meta(256 * 1024);
        let row = size_class::checked_row(SizeClassId::new(0)).unwrap();
        let obj = row.objects_per_slab;
        let span = fresh_span(obj, &m);
        assert!(span.check_invariants());

        // Activate: every object becomes central-free (central_free == count).
        {
            let g = span.lock();
            g.set_live_count(0);
            for i in 0..obj as usize {
                g.central_insert(i);
            }
        }
        assert!(span.check_invariants());
        assert!(span.is_empty_central_only());

        // Hand two objects to the application: live grows, central shrinks.
        {
            let g = span.lock();
            let mut idx = [0u16; 2];
            let n = g.central_remove_batch(&mut idx, 2);
            g.set_live_count(n as u32);
        }
        assert!(span.check_invariants());
    }

    #[test]
    fn b3_span_checker_catches_a_count_over_geometry() {
        let m = meta(256 * 1024);
        let row = size_class::checked_row(SizeClassId::new(0)).unwrap();
        let span = fresh_span(row.objects_per_slab, &m);
        // Force live_count beyond object_count: the §16.4 partition bound breaks.
        {
            let g = span.lock();
            g.set_live_count(row.objects_per_slab + 1);
        }
        assert!(
            !span.check_invariants(),
            "live_count > object_count must fail the partition bound"
        );
    }

    // --- B.1/B.3 central reachability: positive + negative ---------------------

    #[test]
    fn b1_central_checker_reconciles_a_populated_bin() {
        let m = meta(4 * 1024 * 1024);
        let pm = PageMap::new();
        let central = CentralCache::new();
        let sc = SizeClassId::new(3); // 64-byte class (a many-object slab)
        let base = 0x4000_0000usize;
        let oc = size_class::checked_row(sc).unwrap().objects_per_slab;
        // Activation fills the span's bitmap and links it into the partial list.
        let span = SpanDescriptor::new(SpanId(1), A, sc, base, 1, oc, 0, &m).expect("span");
        central.activate_span(&span, &pm, &m).expect("activate");
        assert!(
            central.check_invariants(),
            "an activated span must leave the central bin well-formed and reconciled"
        );
        // Remove a batch: the span stays partial and the accounting still reconciles.
        match central.remove_batch(NodeId::DEFAULT, A, Label::PUBLIC, sc, ANY_PLACE_CLASS, 4) {
            RemoveResult::Ok(b) => assert_eq!(b.len(), 4),
            RemoveResult::NeedSpan => panic!("expected a batch from the activated span"),
        }
        assert!(
            central.check_invariants(),
            "the bin must still reconcile after a partial remove"
        );
    }
}

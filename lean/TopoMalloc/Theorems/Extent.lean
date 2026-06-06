-- SPDX-License-Identifier: MIT
/-
The §18.3 extent physical-state operations (plan 04 W4-2d): the **commit / recommit**
direction, complementing `Theorems/Release.lean`'s decommit / release.

`Release.lean` proves the *decommit/release* side (§21.6, M-004): recording a range
as released preserves every live object, and is well-formed when the released range
holds no live object. This file proves the *commit/recommit* side (M-005): a range
must be **recommitted** — removed from `released` — before a block in it may be made
live again. `recommit` is that transition; the three theorems show it preserves live
objects, actually clears the range from `released`, and preserves well-formedness;
`recommit_then_live_preserves_wellformed` composes it with `setOwner … live` to make
the **M-005 obligation explicit**: a freshly-live block in a recommitted range keeps
clause 10 (`releasedNoLive`).

These are the pre/postconditions the Rust `ExtentManager::commit` (recommit a
`Released` extent before reuse) discharges; `decommit`/`purge` discharge the
`Release.lean` side. The `purge_lazy`/`purge_forced` ops touch no live object and
record nothing — they are the identity on the abstract state, so they preserve every
invariant trivially.
-/
import TopoMalloc.Theorems.Release

namespace TopoMalloc

open State

/-- **`recommit` (§18.3 / M-005).** Remove a range from `released`: the backing is
committed again, so the range may once more serve live allocations. The inverse of
`releaseToOs` at the range level. -/
def recommit (s : State) (r : Range) : State :=
  { s with released := s.released.filter (fun x => decide (x ≠ r)) }

@[simp] theorem recommit_blocks (s : State) (r : Range) : (recommit s r).blocks = s.blocks := rfl
@[simp] theorem recommit_spans (s : State) (r : Range) : (recommit s r).spans = s.spans := rfl

/-- **`recommit_preserves_live_objects` (M-005).** Recommitting a range touches no
block, so every previously-live slot stays live (and at the same address). -/
theorem recommit_preserves_live_objects (s : State) (r : Range) :
    ∀ b, s.IsLive b → (recommit s r).IsLive b := by
  intro b hlive
  -- `recommit` changes only `released`; `ownerOf` reads `blocks`, unchanged.
  simpa [recommit, State.IsLive, State.ownerOf, State.blockById] using hlive

/-- **`recommit_clears_range`.** After recommitting `r`, it is no longer in the
released set — the postcondition a subsequent live allocation in `r` relies on
(M-005: recommit *before* use). -/
theorem recommit_clears_range (s : State) (r : Range) : r ∉ (recommit s r).released := by
  simp [recommit, List.mem_filter]

/-- A recommitted range's released set is a sublist of the original (we only removed
entries), so membership in it implies membership in the original. -/
theorem recommit_released_subset {s : State} {r r' : Range}
    (h : r' ∈ (recommit s r).released) : r' ∈ s.released := by
  have h' : r' ∈ s.released.filter (fun x => decide (x ≠ r)) := h
  exact (List.mem_filter.mp h').1

/-- **`recommit_preserves_wellformed`.** Removing a range from `released` preserves
every well-formedness clause: only clause 10 (`releasedNoLive`) reads `released`, and
it quantifies over a now-smaller set. -/
theorem recommit_preserves_wellformed (s : State) (r : Range) (hwf : WellFormed s) :
    WellFormed (recommit s r) :=
  { rangesDisjoint := hwf.rangesDisjoint
    uniqueOwner := hwf.uniqueOwner
    cachesConsistent := hwf.cachesConsistent
    centralConsistent := hwf.centralConsistent
    objectsFitSpans := hwf.objectsFitSpans
    bitmapsAgree := hwf.bitmapsAgree
    pagemapAgrees := hwf.pagemapAgrees
    hugepagesAgree := hwf.hugepagesAgree
    arenaUnique := hwf.arenaUnique
    releasedNoLive := by
      intro r' hr' blk hblk hlive
      exact hwf.releasedNoLive r' (recommit_released_subset hr') blk hblk hlive
    cacheCapacities := hwf.cacheCapacities
    slabLayout := hwf.slabLayout
    spansDisjoint := hwf.spansDisjoint
    blockSpanClass := hwf.blockSpanClass }

/-- **M-005 made explicit: `recommit_then_live_preserves_wellformed`.** A block may be
made live in a range only *after* it is recommitted. Concretely: if, after recommitting
`r`, block `b`'s range is disjoint from every still-released range, then making `b` live
preserves well-formedness — so the only way to legally re-allocate into a released range
is to recommit it first (clause 10 forbids a live block in any released range). This is
the Lean face of `ExtentManager::alloc` recommitting a `Released` extent before handing
it out. -/
theorem recommit_then_live_preserves_wellformed (s : State) (r : Range) (b : BlockId)
    (hwf : WellFormed s)
    (hb : ∀ blk ∈ s.blocks, blk.id = b →
      ∀ rr ∈ (recommit s r).released, Range.Disjoint rr blk.range) :
    WfReleasedNoLive ((recommit s r).setOwner b Owner.live) := by
  -- After recommit, clause 10 holds (smaller released set); `setOwner_live` then
  -- preserves it given `b` avoids every still-released range (the M-005 precondition).
  have h10 : WfReleasedNoLive (recommit s r) := (recommit_preserves_wellformed s r hwf).releasedNoLive
  exact WfReleasedNoLive.setOwner_live (recommit s r) b h10 (by
    intro blk hblk hid rr hrr
    exact hb blk (by simpa using hblk) hid rr hrr)

end TopoMalloc

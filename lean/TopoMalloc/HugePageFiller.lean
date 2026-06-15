-- SPDX-License-Identifier: MIT
/-
The hugepage filler: bins, occupancy, and partial subrelease (SPEC §19, plan 04 W11).

This module models the **correctness object** of the hugepage backend (DD-2: "bins
are correctness, the score is policy"):

* `classifyBin` — the *total* §19.4 function mapping a hugepage's
  `(used, total, subreleased, hotness)` to its unique `HugeBin`. It is the single
  source of truth the Rust `huge::classify_bin` is pinned to, by the
  `huge_bin_classification_matches_lean` test and the `lake exe check` `hugeBinGate`
  (the §19.4 analogue of the §20.1 `extentStateGate`). H-003 ("a bin assignment MUST
  match occupancy and state") holds **by construction**, and the lemmas below make
  that precise: a bin pins down the occupancy/state facts it implies.

* **H-002** ("occupancy bytes == sum of contained span/large occupancy") — modeled
  as: the occupancy is the sum of the contained allocations' page counts, so the
  occupancy in bytes equals the sum of their byte sizes (`occupancy_eq_sum_bytes`).
  The Rust `live` bitmap *is* that sum (popcount), so the runtime upholds it
  definitionally.

* **H-005** ("partial subrelease must never release a subpage intersecting a live
  object") — modeled over the `Range` geometry of `Types.lean`: under the runtime
  guard (the released sub-range is disjoint from every live range — exactly the Rust
  `subrelease`'s `run_is_clear(live)` refusal), every live range stays disjoint from
  the grown released set, i.e. stays backed (`subrelease_preserves_live_backing`).
  This is the §19.6 conditional-correctness statement, tied to the real disjointness
  geometry the allocator maintains.

The model is `sorry`-free and rests only on the standard axioms (`Range` lemmas are
`omega`-discharged; the bin lemmas are `decide`/`split`-discharged).
-/
import TopoMalloc.Types

namespace TopoMalloc
namespace Huge

/-- A hotness class for a hugepage (§19.3 `hotness_match` / §19.6 coldness). Advisory
placement input; mirrors the Rust `huge::Hotness`. -/
inductive Hotness where
  /-- Cold: idle / explicitly cold — the prime partial-subrelease candidate. -/
  | cold
  /-- Neutral: no hotness signal (the default). -/
  | neutral
  /-- Hot: frequently accessed — pack densely, preserve coverage. -/
  | hot
  deriving Repr, DecidableEq, BEq

/-- The nine §19.4 hugepage bins. Each hugepage is in exactly one (H-003); mirrors the
Rust `huge::HugeBin`. -/
inductive HugeBin where
  /-- `used = 0`, no subrelease: empty, backed, held for reuse (HugeCache). -/
  | emptyBacked
  /-- Low occupancy `(0, 1/8)`. -/
  | nearlyEmpty
  /-- Sparse `[1/8, 3/8)`, not cold. -/
  | sparse
  /-- Moderate `[3/8, 5/8)`. -/
  | medium
  /-- High `[5/8, 1)`, not hot. -/
  | nearlyFull
  /-- `used = total`, not hot. -/
  | full
  /-- Has a subreleased page (§19.6). -/
  | partialSubreleased
  /-- Sparse `[1/8, 3/8)` and cold: the prime subrelease bin. -/
  | coldSparse
  /-- High/full and hot: pack densely, preserve coverage. -/
  | hotDense
  deriving Repr, DecidableEq, BEq

namespace HugeBin

/-- **§19.4 classification (H-003).** The total function mapping a hugepage's
occupancy/state to its unique bin, mirroring the Rust `huge::classify_bin` exactly:

* any subreleased page ⇒ `partialSubreleased` (state override);
* else `used = 0` ⇒ `emptyBacked`;
* else `total ≤ used` ⇒ `hotDense` if hot, else `full`;
* else by occupancy eighths `band = used*8/total` (∈ `{0,…,7}` since `0 < used < total`):
  `0` ⇒ nearlyEmpty; `1,2` ⇒ coldSparse if cold else sparse; `3,4` ⇒ medium; `5,6,7`
  ⇒ hotDense if hot else nearlyFull.

The hotness parameter is named `hotness` (not `hot`) to avoid clashing with the
`Hotness.hot` constructor. -/
def classifyBin (used total subreleased : Nat) (hotness : Hotness) : HugeBin :=
  if subreleased > 0 then
    .partialSubreleased
  else if used = 0 then
    .emptyBacked
  else if total ≤ used then
    match hotness with
    | .hot => .hotDense
    | _ => .full
  else
    let band := used * 8 / total
    if band = 0 then
      .nearlyEmpty
    else if band = 1 ∨ band = 2 then
      match hotness with
      | .cold => .coldSparse
      | _ => .sparse
    else if band = 3 ∨ band = 4 then
      .medium
    else
      match hotness with
      | .hot => .hotDense
      | _ => .nearlyFull

/-- **H-003 (state).** A hugepage is binned `partialSubreleased` **iff** it has a
subreleased page — the §19.6 state the bin records. (The forward direction shows no
*other* branch yields `partialSubreleased`, so the bin records exactly the subrelease
state.) -/
theorem partialSubreleased_iff_subreleased (used total subreleased : Nat) (hotness : Hotness) :
    classifyBin used total subreleased hotness = .partialSubreleased ↔ 0 < subreleased := by
  simp only [classifyBin]
  cases hotness <;> (repeat' split) <;> simp_all

/-- **H-003 (occupancy).** An `emptyBacked` hugepage has zero occupancy and no
subrelease — the bin pins down the occupancy it implies. -/
theorem emptyBacked_implies_empty (used total subreleased : Nat) (hotness : Hotness)
    (h : classifyBin used total subreleased hotness = .emptyBacked) :
    used = 0 ∧ subreleased = 0 := by
  simp only [classifyBin] at h
  cases hotness <;> (repeat' split at h) <;>
    first
      | exact absurd h (by decide)
      | (refine ⟨?_, ?_⟩ <;> omega)

/-- **H-003 (occupancy).** A `full` hugepage is at capacity (`total ≤ used`,
non-empty) with no subrelease — the §19.4 full state. -/
theorem full_implies_at_capacity (used total subreleased : Nat) (hotness : Hotness)
    (h : classifyBin used total subreleased hotness = .full) :
    0 < used ∧ total ≤ used ∧ subreleased = 0 := by
  simp only [classifyBin] at h
  cases hotness <;> (repeat' split at h) <;>
    first
      | exact absurd h (by decide)
      | (refine ⟨?_, ?_, ?_⟩ <;> omega)

/-- The classification is **total**: it returns *some* bin for every input (trivially,
being a function — stated so the "exactly one bin" claim of §19.4 is explicit). -/
theorem classifyBin_total (used total subreleased : Nat) (hotness : Hotness) :
    ∃ b : HugeBin, classifyBin used total subreleased hotness = b :=
  ⟨_, rfl⟩

end HugeBin

-- ---------------------------------------------------------------------------
-- H-002: occupancy is the sum of contained allocations
-- ---------------------------------------------------------------------------

/-- **H-002.** A hugepage's occupancy in bytes equals the sum of its contained
allocations' byte sizes. Modeling the occupancy as the sum of the contained
allocations' page counts (`allocs.sum` — exactly what the Rust `live` bitmap's
popcount is), the occupancy bytes (`allocs.sum * pageSize`) equal the sum of the
per-allocation byte sizes (`allocs.map (· * pageSize)).sum`). -/
theorem occupancy_eq_sum_bytes (allocs : List Nat) (pageSize : Nat) :
    allocs.sum * pageSize = (allocs.map (· * pageSize)).sum := by
  induction allocs with
  | nil => simp
  | cons a t ih => simp only [List.sum_cons, List.map_cons, Nat.add_mul, ih]

-- ---------------------------------------------------------------------------
-- H-005: partial subrelease never intersects a live object
-- ---------------------------------------------------------------------------

open Range

/-- **§19.6 / H-005, over the real `Range` geometry.** Under the runtime guard — the
subreleased range `sub` is disjoint from every live range (the Rust `subrelease`'s
`run_is_clear(live)` refusal) — every live range that was backed (disjoint from the
already-released set `rel`) stays backed after `sub` joins the released set. So a
partial subrelease can never strand a live object's backing: "allocator correctness
assumes the guard," and the guard is exactly what the runtime enforces. -/
theorem subrelease_preserves_live_backing
    {live rel : List Range} {sub : Range}
    (hbacked : ∀ r ∈ live, ∀ q ∈ rel, Disjoint r q)
    (hguard : ∀ r ∈ live, Disjoint r sub) :
    ∀ r ∈ live, ∀ q ∈ sub :: rel, Disjoint r q := by
  intro r hr q hq
  rcases List.mem_cons.mp hq with rfl | hq'
  · exact hguard r hr
  · exact hbacked r hr q hq'

/-- **H-005 corollary.** In particular no live range is ever the released range: the
guard makes `sub` disjoint from each live range, and a valid range is not disjoint
from itself, so the subrelease target is none of the live objects. -/
theorem subrelease_target_is_not_live
    {live : List Range} {sub : Range} (hsub : Valid sub)
    (hguard : ∀ r ∈ live, Disjoint r sub) :
    sub ∉ live := by
  intro hmem
  exact (not_disjoint_self hsub) ((hguard sub hmem).symm)

-- ---------------------------------------------------------------------------
-- The filler as a per-page state machine (H-001 / H-004 / H-005 preservation)
-- ---------------------------------------------------------------------------
--
-- The `Range`-geometry H-005 above is the disjointness form; this section models the
-- filler's place/free/subrelease as transitions of a per-page state machine — the
-- direct analogue of the §20.1 `ExtentState`/§22.3 `ArenaPhase` machines — and proves
-- the H-001/H-004/H-005 invariants are *preserved*. It mirrors the Rust filler's three
-- per-page bitmaps (`live`/`committed`/`released`) collapsed to one state per page.

namespace Filler

/-- The per-page state of a hugepage (the Rust filler's three bitmaps as one state):
`reserved` = never committed; `free` = committed, not live; `live` = committed and
live; `released` = subreleased (was committed). Each page is in exactly one. -/
inductive PageState where
  /-- Virtual page reserved, never committed (¬committed, ¬live, ¬released). -/
  | reserved
  /-- Committed and free (backed, reusable — "dirty"). -/
  | free
  /-- Committed and live (a live object occupies it). -/
  | live
  /-- Subreleased — was committed, now decommitted (¬committed, ¬live). -/
  | released
  deriving Repr, DecidableEq

namespace PageState

/-- Whether the page is physically backed (committed) — the H-001 `committed` side. -/
def committed : PageState → Bool
  | .free => true
  | .live => true
  | _ => false

/-- Whether a live object occupies the page. -/
def isLive : PageState → Bool
  | .live => true
  | _ => false

/-- **H-001 (coupling).** A live page is always committed: the model makes `live` a
committed state distinct from `released`, so a live object is, by construction, in a
committed non-released subrange. -/
theorem live_is_committed (s : PageState) (h : isLive s = true) : committed s = true := by
  cases s <;> simp_all [isLive, committed]

/-- **H-001 (no live in released).** A live page is never the `released` state. -/
theorem live_ne_released (s : PageState) (h : isLive s = true) : s ≠ .released := by
  cases s <;> simp_all [isLive]

end PageState

/-- A hugepage as a map from page index to state (the Rust per-page bitmaps; pages
outside the hugepage are `reserved`). -/
def HugePageState := Nat → PageState

/-- Page `p` lies in the run `[off, off + len)`. -/
def inRun (off len p : Nat) : Prop := off ≤ p ∧ p < off + len

instance (off len p : Nat) : Decidable (inRun off len p) := by unfold inRun; infer_instance

/-- **place**: the run's pages become `live` (the filler's carve + commit, M-005). -/
def place (hp : HugePageState) (off len : Nat) : HugePageState :=
  fun p => if inRun off len p then .live else hp p

/-- **free**: the run's `live` pages become `free` (committed retained — the HugeCache
retain policy). -/
def freeRun (hp : HugePageState) (off len : Nat) : HugePageState :=
  fun p => if inRun off len p ∧ hp p = .live then .free else hp p

/-- **subrelease**: the run's `free` pages become `released`. In this model it touches
**only** `free` pages — never `live` — so H-005 holds *structurally* (by the shape of
this function). The Rust `subrelease` enforces the equivalent runtime guard explicitly
(`run_is_clear(live) ∧ run committed`, i.e. exactly the `free` pages), so the model is a
faithful abstraction of the checked code. -/
def subrelease (hp : HugePageState) (off len : Nat) : HugePageState :=
  fun p => if inRun off len p ∧ hp p = .free then .released else hp p

/-- **H-005 (the headline, page form).** `subrelease` never releases a live page: any
page `live` before is still `live` after, **whatever** the run. The §19.6 "must never
release a subpage intersecting a live object" holds structurally — the operation only
converts `free` pages, so it can never strand a live object's backing. -/
theorem subrelease_preserves_live (hp : HugePageState) (off len p : Nat)
    (h : hp p = .live) : subrelease hp off len p = .live := by
  unfold subrelease
  split
  · rename_i hcond
    rw [h] at hcond
    exact absurd hcond.2 (by decide)
  · exact h

/-- **H-005 (set form).** The live-page predicate is unchanged by `subrelease` — the
set of live pages is invariant, so no live object loses backing. -/
theorem subrelease_preserves_isLive (hp : HugePageState) (off len p : Nat) :
    (subrelease hp off len p).isLive = (hp p).isLive := by
  unfold subrelease
  split
  · rename_i hcond
    rw [hcond.2]
    decide
  · rfl

/-- **H-004.** An empty-hugepage release is the whole-hugepage `subrelease` `[0, total)`;
by `subrelease_preserves_live` it keeps every live page live, so a hugepage holding a
live object never has that object released — "empty hugepages may be released only
after all contained spans are empty and detached." -/
theorem release_preserves_live_anywhere (hp : HugePageState) (total p : Nat)
    (h : hp p = .live) : subrelease hp 0 total p = .live :=
  subrelease_preserves_live hp 0 total p h

/-- **H-001 (place).** After `place`, every page of the run is `live` — hence committed
(`PageState.live_is_committed`), the H-001 "live object in a committed subrange". -/
theorem place_run_is_live (hp : HugePageState) (off len p : Nat)
    (hin : inRun off len p) : place hp off len p = .live := by
  unfold place
  rw [if_pos hin]

/-- **`free` is the inverse of `place` on the run's live pages**: a freed page is no
longer live, so the freed run can be re-placed (the filler's reuse). -/
theorem freeRun_clears_live (hp : HugePageState) (off len p : Nat)
    (hin : inRun off len p) (h : hp p = .live) : (freeRun hp off len p).isLive = false := by
  unfold freeRun
  rw [if_pos ⟨hin, h⟩]
  rfl

/-- **`place` leaves pages outside the run untouched** — so it never disturbs an
existing allocation. The intra-hugepage analogue of "split/merge keep other extents
fixed". -/
theorem place_preserves_outside (hp : HugePageState) (off len p : Nat)
    (hout : ¬ inRun off len p) : place hp off len p = hp p := by
  unfold place
  rw [if_neg hout]

/-- **An existing live page survives `place`.** Under the placement precondition that
the run is *clear of live pages* (the filler carves only clear pages), every
previously-live page stays live: an in-run page was not live (so the case is vacuous),
an out-of-run page is unchanged. So `place` adds the run to the live set without
evicting any existing live object — the placement analogue of `subrelease_preserves_live`. -/
theorem place_preserves_existing_live (hp : HugePageState) (off len p : Nat)
    (hclear : ∀ q, inRun off len q → hp q ≠ .live) (h : hp p = .live) :
    place hp off len p = .live := by
  by_cases hin : inRun off len p
  · exact absurd h (hclear p hin)
  · rw [place_preserves_outside hp off len p hin]; exact h

/-- **Intra-hugepage placement disjointness (the §18.4 / H-001 core for the filler).**
Under the precondition that the run is clear of live pages, the pages live *after*
`place` are exactly `(previously-live) ⊔ (run)`, and those two sets are **disjoint**.
So two allocations placed into one hugepage can never share a page — the runtime
guarantees this structurally by carving each run from the free bitmap (`find_run_aligned`
requires `run_is_clear(live)`), and this is the proof that the structure suffices. -/
theorem place_live_is_disjoint_union (hp : HugePageState) (off len p : Nat)
    (hclear : ∀ q, inRun off len q → hp q ≠ .live) :
    (place hp off len p = .live ↔ (hp p = .live ∨ inRun off len p))
      ∧ ¬ (hp p = .live ∧ inRun off len p) := by
  refine ⟨⟨?_, ?_⟩, ?_⟩
  · intro hlive
    by_cases hin : inRun off len p
    · exact Or.inr hin
    · rw [place_preserves_outside hp off len p hin] at hlive; exact Or.inl hlive
  · rintro (hold | hin)
    · exact place_preserves_existing_live hp off len p hclear hold
    · exact place_run_is_live hp off len p hin
  · rintro ⟨hold, hin⟩
    exact (hclear p hin) hold

/-- The page state machine is **inhabited and non-trivial**: placing then subreleasing
a free page reaches `released`, while a concurrently-live page stays `live`. -/
example :
    let hp : HugePageState := fun p => if p = 0 then .live else .free
    subrelease hp 1 1 0 = .live ∧ subrelease hp 1 1 1 = .released := by
  refine ⟨?_, ?_⟩
  · exact subrelease_preserves_live _ 1 1 0 rfl
  · decide

end Filler

-- ---------------------------------------------------------------------------
-- Non-vacuity: the model is about real subdivisions / classifications
-- ---------------------------------------------------------------------------

/-- A concrete cold-sparse hugepage (16 of 128 pages live, cold, no subrelease) is
binned `coldSparse` — a witness that the sparse/cold branch is reachable. -/
example : HugeBin.classifyBin 16 128 0 .cold = HugeBin.coldSparse := by decide

/-- A concrete partial-subrelease guard is satisfiable: a live object at `[0,16)` is
disjoint from a subrelease target at `[64,80)`, so `subrelease_preserves_live_backing`
is not vacuous. -/
example : ∀ r ∈ [(⟨0, 16⟩ : Range)], Disjoint r (⟨64, 16⟩ : Range) := by
  intro r hr
  simp only [List.mem_singleton] at hr; subst hr
  simp only [Range.Disjoint, Range.stop]; omega

-- ---------------------------------------------------------------------------
-- Executable differential gate (pins the Rust `classify_bin`)
-- ---------------------------------------------------------------------------

/-- The §19.4 hugepage-bin differential gate (W11): `classifyBin` returns the expected
bin for one representative input per bin (with `total = 128`, the tuned
`PAGES_PER_HUGEPAGE`). The Rust test `huge_bin_classification_matches_lean` checks the
**identical** inputs against `huge::classify_bin`, so the runtime classifier and this
model cannot drift (the §19.4 analogue of `extentStateGate`). -/
def hugeBinGate : Bool :=
  (HugeBin.classifyBin 0 128 0 .neutral == .emptyBacked)
    && (HugeBin.classifyBin 1 128 0 .neutral == .nearlyEmpty)
    && (HugeBin.classifyBin 32 128 0 .neutral == .sparse)
    && (HugeBin.classifyBin 32 128 0 .cold == .coldSparse)
    && (HugeBin.classifyBin 64 128 0 .neutral == .medium)
    && (HugeBin.classifyBin 112 128 0 .neutral == .nearlyFull)
    && (HugeBin.classifyBin 112 128 0 .hot == .hotDense)
    && (HugeBin.classifyBin 128 128 0 .neutral == .full)
    && (HugeBin.classifyBin 128 128 0 .hot == .hotDense)
    && (HugeBin.classifyBin 5 128 3 .neutral == .partialSubreleased)

/-- The gate evaluates `true` (proof-checked, not just `#eval`uated), so the example
set is genuinely the bin split the Rust `classify_bin` implements. -/
theorem hugeBinGate_holds : hugeBinGate = true := by decide

end Huge
end TopoMalloc

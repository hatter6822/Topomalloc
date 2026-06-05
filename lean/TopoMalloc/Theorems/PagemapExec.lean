-- SPDX-License-Identifier: MIT
/-
The **executable pagemap model + replay differential** (§17.1/§17.2, §33.7, plan 03
W3-3d; mirrors plan 02 W1-8b `pagemap_lookup_sound`).

This is the proof-grade, *runnable* counterpart of the runtime radix pagemap
(`crates/topo-core/src/pagemap.rs`). A page number is `addr / pageSize`; `install`
maps every page of a (page-aligned) span's range to that span; `lookup` returns the
span owning an address's page (§17.1). Two artefacts:

* `PageMapModel.install_lookup_sound` — the executable analogue of the proved
  `pagemap_lookup_sound`: a lookup that hits names the span whose range contains the
  queried address. Kernel-checked (no `native_decide`), so it is a real proof, not an
  evaluation.

* `replayPagemap` + `samplePagemapTrace` — an install/lookup trace replayed through
  the model, checking each lookup. `lake exe check` evaluates `pagemapGate` on it,
  and the Rust differential test `pagemap_matches_lean_replay_differential`
  (`tests/tests/pagemap.rs`) replays the **same** trace against the real radix and
  asserts the **same** `addr -> span-id` results. A divergence on either side fails
  CI — closing the W3-3d differential loop the way the live-set oracle (`Exec.lean` +
  `trace-replay`) closes the §8.3 one.
-/
import TopoMalloc.Generated.SizeClasses

namespace TopoMalloc

/-- The allocator page number containing `addr` (mirrors `pagemap::page_of`). -/
def pageOf (addr : Nat) : Nat := addr / Generated.pageSize

/-- An executable pagemap as a page-number → span-id map (the logical content of the
runtime radix and of the abstract `State.pagemap`). A function rather than an assoc
list so `install`/`lookup` are decidable if-then-else and the soundness proof is
direct. -/
structure PageMapModel where
  /-- The owning span of each allocator page, if any. -/
  map : Nat → Option Nat

namespace PageMapModel

/-- The empty pagemap — every page non-owned (P-Map-002). -/
def empty : PageMapModel := ⟨fun _ => none⟩

/-- Install span `sp` over `pages` pages starting at the page of `base`: map each
covered page to `sp`, shadowing any prior owner (P-Map-001 — exactly one owner). For
a page-aligned `base` this is exactly the runtime `install_span`'s page range. -/
def install (pm : PageMapModel) (sp base pages : Nat) : PageMapModel :=
  ⟨fun page =>
    if pageOf base ≤ page ∧ page < pageOf base + pages then some sp else pm.map page⟩

/-- Look up the span owning `addr`'s page (the §17.1 query). -/
def lookup (pm : PageMapModel) (addr : Nat) : Option Nat :=
  pm.map (pageOf addr)

/-- **`pagemap_lookup_sound` (executable, §17.2 / §33.4).** After installing span `sp`
over a **page-aligned** range `[base, base + pages * pageSize)`, looking up any address
in that range returns `sp`. The hypothesis *is* "the span's range contains `addr`", so
this is the soundness statement: a lookup hit names a span whose range covers the
query. -/
theorem install_lookup_sound (pm : PageMapModel) (sp base pages addr : Nat)
    (halign : base % Generated.pageSize = 0) (hlo : base ≤ addr)
    (hhi : addr < base + pages * Generated.pageSize) :
    (pm.install sp base pages).lookup addr = some sp := by
  -- `pageSize` is the concrete `16384`, so `omega` can reason about the div/mod.
  simp only [install, lookup, pageOf, Generated.pageSize] at halign hhi ⊢
  rw [if_pos (by omega)]

end PageMapModel

/-- One step of a pagemap replay trace: install a span, or assert a lookup result. -/
inductive PageOp where
  /-- Install span `sp` over `pages` pages starting at `base`. -/
  | install (sp base pages : Nat)
  /-- Assert `lookup addr = span` (the differential check point). -/
  | expect (addr : Nat) (span : Option Nat)
  deriving Repr

/-- Replay a trace against the model, folding installs and checking each `expect`. -/
def replayPagemap : List PageOp → PageMapModel → Bool
  | [], _ => true
  | PageOp.install sp base pages :: rest, pm => replayPagemap rest (pm.install sp base pages)
  | PageOp.expect addr span :: rest, pm => (pm.lookup addr == span) && replayPagemap rest pm

/-- A recorded install/lookup trace. The Rust differential test replays the **same**
trace (same bases, page counts, query addresses, and expected span ids) against the
runtime radix. Span 1 is a 2-page span at `0x4000_0000`; span 2 a 1-page span at
`0x8000_0000`. -/
def samplePagemapTrace : List PageOp :=
  [ .install 1 0x4000_0000 2,
    .install 2 0x8000_0000 1,
    .expect 0x4000_0000 (some 1),  -- first byte of span 1
    .expect 0x4000_0001 (some 1),  -- interior of span 1's first page
    .expect 0x4000_4000 (some 1),  -- span 1's second page (+ one pageSize)
    .expect 0x4000_8000 none,      -- one page past span 1 → non-owned
    .expect 0x8000_0000 (some 2),  -- span 2
    .expect 0x3FFF_FFFF none,      -- one page below span 1 → non-owned
    .expect 0xDEAD_0000 none ]     -- wholly unrelated → non-owned

/-- The W3-3d gate value: the recorded trace replays with every lookup as expected.
`lake exe check` evaluates this; the Rust side checks the identical expectations. -/
def pagemapGate : Bool := replayPagemap samplePagemapTrace PageMapModel.empty

/-- A concrete, **kernel-checked** soundness instance (no `native_decide`): the
mid-span lookup of span 1's second page returns span 1 — discharged by
`install_lookup_sound`, with the page-alignment and range side conditions decided. -/
example : (PageMapModel.empty.install 1 0x4000_0000 2).lookup 0x4000_4000 = some 1 :=
  PageMapModel.install_lookup_sound _ 1 0x4000_0000 2 0x4000_4000 (by decide) (by decide) (by decide)

end TopoMalloc

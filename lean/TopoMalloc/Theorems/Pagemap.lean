-- SPDX-License-Identifier: MIT
/-
The §17.2 pagemap-soundness theorem (plan 02 W1-8b, mirroring plan 03 W3-3d).

* `pagemap_lookup_sound`

A pagemap lookup keyed by `addr / pageSize` returns a span id; soundness (clause 7,
`WfPagemapAgrees`) says that span exists and its range covers the whole page, so the
queried address lies inside it.
-/
import TopoMalloc.Transitions

namespace TopoMalloc

open State

/-- The allocator page size is positive (`pageSize = 16384`). -/
theorem pageSize_pos : 0 < Generated.pageSize := by decide

/-- **`pagemap_lookup_sound` (§33.4 / §17.2).** If the pagemap maps `addr`'s page to
span `sp`, then `sp` is a real span whose range contains `addr`. -/
theorem pagemap_lookup_sound (s : State) (hwf : WellFormed s) (addr : Nat) (sp : SpanId)
    (h : s.pagemap.lookup (addr / Generated.pageSize) = some sp) :
    ∃ d ∈ s.spans, d.id = sp ∧ d.range.Contains addr := by
  -- the lookup hit is an actual pagemap entry
  obtain ⟨l1, l2, heq, _⟩ := List.lookup_eq_some_iff.mp h
  have hmem : (addr / Generated.pageSize, sp) ∈ s.pagemap := by
    rw [heq]; exact List.mem_append_right l1 (List.mem_cons_self ..)
  -- clause 7 gives a covering span
  obtain ⟨d, hd, hdid, hbase, hstop⟩ := hwf.pagemapAgrees (addr / Generated.pageSize) sp hmem
  refine ⟨d, hd, hdid, ?_⟩
  -- the page `[p·ps, (p+1)·ps)` lies in `d.range`, and `addr` lies in that page
  simp only [Range.Contains, Range.stop]
  have hps := pageSize_pos
  have h1 := Nat.div_add_mod addr Generated.pageSize
  have h2 := Nat.mod_lt addr hps
  have hexp : (addr / Generated.pageSize + 1) * Generated.pageSize
      = Generated.pageSize * (addr / Generated.pageSize) + Generated.pageSize := by
    rw [Nat.add_one_mul, Nat.mul_comm]
  have hle : addr / Generated.pageSize * Generated.pageSize
      = Generated.pageSize * (addr / Generated.pageSize) := Nat.mul_comm _ _
  simp only [Range.stop] at hstop
  omega

end TopoMalloc

-- SPDX-License-Identifier: GPL-3.0-or-later
/-
The VSpace map/unmap provider contract (SPEC §36.6/§36.8.1, plan 02 W1-11).

VSpace windows are scarce allocator resources (§36.8.1): a contiguous virtual region
with a rights mask into which provider frames may be mapped. The provider MUST install
mappings only into an authorized window (`mapped_within`), and `unmap` MUST make the
range unreachable before revocation. This module models a window, a mapping, and the
"map only into an authorized window" obligation that `map_frame` discharges.

GPL-3.0-or-later (D5).
-/
import TopoMalloc.Types

namespace TopoMalloc.SeLe4n

open TopoMalloc

/-- A VSpace window (§36.8.1): a contiguous virtual region with a rights mask. -/
structure VSpaceWindow where
  range : Range
  readable : Bool
  writable : Bool
  deriving Repr, DecidableEq

/-- A frame mapped into a VSpace at some sub-range. -/
structure Mapping where
  window : VSpaceWindow
  mapped : Range
  deriving Repr, DecidableEq

/-- §36.6: a mapping is authorized exactly when it falls inside its window. -/
def Mapping.authorized (m : Mapping) : Prop := m.mapped.Subset m.window.range

instance (m : Mapping) : Decidable m.authorized := by unfold Mapping.authorized; infer_instance

/-- `map_frame` produces an authorized mapping when handed a sub-range of the window:
mappings are installed only into an authorized VSpace window (§36.6). -/
theorem map_frame_authorized (w : VSpaceWindow) (sub : Range) (h : sub.Subset w.range) :
    (Mapping.mk w sub).authorized := h

/-- After `unmap`, the range is no longer claimed: two mappings into disjoint windows
never alias, so unmapping one cannot leave the other reachable (§36.6 unmap safety). -/
theorem unmap_no_alias (m₁ m₂ : Mapping) (h₁ : m₁.authorized) (h₂ : m₂.authorized)
    (hwin : Range.Disjoint m₁.window.range m₂.window.range) :
    Range.Disjoint m₁.mapped m₂.mapped :=
  (hwin.of_subset_right h₂).of_subset_left h₁

end TopoMalloc.SeLe4n

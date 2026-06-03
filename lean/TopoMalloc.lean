-- SPDX-License-Identifier: MIT
/-
Root module of the TopoMalloc formal model (plan 02). Importing it brings in the
core types, the size-class predicate, the generated (single-source-of-truth)
table, and the (empty at M0) seLe4n bridge.
-/
import TopoMalloc.Types
import TopoMalloc.SizeClass
import TopoMalloc.Generated.SizeClasses
import TopoMalloc.SeLe4n.Bridge

-- SPDX-License-Identifier: MIT
/-
Umbrella for the §33.4 theorem families (plan 02 W1-4e, W1-6, W1-8, W1-9). Each
submodule proves one family, named **exactly** per §33.4 for traceability (Definition
of Done, addendum 2).
-/
import TopoMalloc.Theorems.SizeClass
import TopoMalloc.Theorems.Malloc
import TopoMalloc.Theorems.Free
import TopoMalloc.Theorems.Cache
import TopoMalloc.Theorems.Central
import TopoMalloc.Theorems.Span
import TopoMalloc.Theorems.Pagemap
import TopoMalloc.Theorems.PagemapExec
import TopoMalloc.Theorems.Release
import TopoMalloc.Theorems.Extent
import TopoMalloc.Theorems.Arena
import TopoMalloc.Theorems.Allocate
import TopoMalloc.Theorems.Demo

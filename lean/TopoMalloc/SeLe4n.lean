-- SPDX-License-Identifier: GPL-3.0-or-later
/-
Umbrella for the seLe4n bridge package (SPEC §36.3.3, plan 02 W1-2/W1-11/W1-12/W1-13).

Importing this brings in the whole bridge: capability-backed arenas (§36.4), the §36.6
backing-provider state machine, the VSpace/CSpace provider contracts (§36.8), the
abstraction relation and `TopoSeLe4nWellFormed` (§36.3.3), the resource-server and
client-runtime families, information-flow non-interference (§36.12), the staged SMP
extensions (V-004), and the preservation/simulation theorems (§36.17).

The whole package is GPL-3.0-or-later (D5) and **builds without seLe4n**: it imports
TopoMalloc's model and seLe4n's public shapes only, never private kernel internals.
-/
import TopoMalloc.SeLe4n.CapBackedArena
import TopoMalloc.SeLe4n.UntypedProvider
import TopoMalloc.SeLe4n.VSpaceProvider
import TopoMalloc.SeLe4n.CSpaceProvider
import TopoMalloc.SeLe4n.Bridge
import TopoMalloc.SeLe4n.ResourceServer
import TopoMalloc.SeLe4n.ClientRuntime
import TopoMalloc.SeLe4n.InformationFlow
import TopoMalloc.SeLe4n.SMP
import TopoMalloc.SeLe4n.Refinement

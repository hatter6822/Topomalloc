<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# `sele4n/` — the seLe4n resource-server component (GPL-3.0-or-later)

The seLe4n-side integration: the `TopoResourceServer` component and the adapters
to seL4-style `allocman`/VKA/VSpace abstractions (§36.3, §36.15, plan 09). This
directory is **separately licensed GPL-3.0-or-later** (`sele4n/LICENSE`), because
it links/models the GPLv3 seLe4n microkernel (decision D5). The default MIT
TopoMalloc artifact does not include it. See [`../NOTICE`](../NOTICE).

It is empty of code at M0 by design: the license boundary is drawn *before* the
first seLe4n byte (W0-12 best practice), so there is never a relicensing event.
The resource server, IPC handlers, CSlot/VSpace accounting, and boot integration
are built in plan 09 (W22) against the pinned upstream ABI (D8).

# Plan 09 — seLe4n Integration

**Workstreams:** W22 · **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §36 (whole), R10–R15; upstream crates `sele4n-abi`/`sele4n-types`/`sele4n-sys`/`sele4n-hal`.
**Upstream deps:** [04](04-backend-hugepages-release.md) (the seam), [06](06-api-realloc-arenas.md) (cap
arenas), [02](02-formal-model.md) (bridge W1-11..14). **Downstream:** Microkernel conformance.
**Milestones:** co-developed against the real ABI from **M1**; runs on the real kernel at **M8**; SMP at **M9**.

> The **required** microkernel-integration conformance (§36). Per D2 it is co-equal from the start: the
> provider compiles against the *real* `sele4n-abi`/`sele4n-types` from M1, a host `Sele4nSim` mirrors the same
> `invoke_syscall` surface for host execution + differential testing, and M8 changes only the *execution
> target* to the real kernel in QEMU. The integration must remain a **user-level/service-level** component —
> never an in-kernel heap (§36.2).

## Three-layer architecture (§36.2)

```text
seLe4n kernel core            capabilities, scheduling, IPC, CSpace, VSpace, retype — NO TopoMalloc heap
   ▲ invoke_syscall (sele4n-abi)
TopoResourceServer (W22-5)    owns delegated untyped/CSlot/VSpace authority; arenas/quota/purge/stats over IPC
   ▲ IPC (MessageInfo/IpcBuffer)
libtopomalloc-sele4n (W22-6)  malloc/free/calloc/realloc + arena APIs; local caches; server only on slow paths
```

## Real-ABI mapping (W22-0 — frozen once, reviewed once)

| TopoBackingProvider op | seLe4n `SyscallId` / `TypeTag` | rights |
|---|---|---|
| `create_frame` | retype untyped → `TypeTag::Frame` (and CNode/etc. as needed) | — |
| `map_frame` | page map into a VSpace window | `AccessRights`/`PagePerms` |
| `unmap_frame` | page unmap | — |
| `revoke_descendants` | CNode revoke | — |
| `recycle` | delete + return untyped to pool | — |

| TopoMalloc error (§36.14) | seLe4n `KernelError` family |
|---|---|
| `TOPO_ERR_INVALID_CAP` / `AUTHORITY_DENIED` | invalid-cap / rights errors |
| `TOPO_ERR_CSPACE_EXHAUSTED` | slot/CNode-full errors |
| `TOPO_ERR_VSPACE_EXHAUSTED` | mapping/VSpace errors |
| `TOPO_ERR_RETYPE_FAILED` / `MAP_FAILED` / `REVOKE_FAILED` | the corresponding kernel op errors |
| `TOPO_ERR_LABEL_VIOLATION` | (TopoMalloc-policy, above the kernel) |

---

## W22 — seLe4n integration profile

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W22-0 | **Bind to the real ABI:** pin `sele4n-abi`/`sele4n-types`/`sele4n-sys` (D8); the op→`SyscallId`/`TypeTag` and error→`KernelError` maps above. | M | | provider + Sim share the real types; maps reviewed; workspace type-checks vs pinned upstream. |
| W22-1a | `Sele4nSim`: model kernel objects on the host (untyped pool, CNode/CSlots, frames, VSpace) typed with `CPtr`/`Slot`/`ObjId`/`TypeTag`/`AccessRights`. | M | | object model uses the real types; deterministic. |
| W22-1b | `Sele4nSim`: implement `invoke_syscall` dispatch for retype/map/unmap/delete/revoke `SyscallId`s; return `KernelResult`/`KernelError`. | L | | each modeled syscall matches the real ABI signature + error space. |
| W22-1c | `Sele4nSim`: enforce the §36.6 provider state machine + provenance (every frame descends from authorized untyped); device/DMA isolation. | M | | illegal transitions rejected as `KernelError`; provenance recorded. |
| W22-1d | `Sele4nSim`: trace emit + replay hook for differential testing vs the Lean bridge (plan 08 W21-2, plan 02 W1-11). | M | ∥ | Sim traces replay against the bridge. |
| W22-2 | Capability authority set (§36.4): `TopoHeapServiceCap/ArenaCap/BackingCap/ControlCap/StatsCap/EmergencyCap`; attenuation. | M | ∥ | authority checks enforced; monotonicity (plan 06 W9-5). |
| W22-3 | CSlot/VSpace accounting (§36.8) + clean exhaustion errors (`TOPO_ERR_CSPACE/VSPACE_EXHAUSTED`). | M | | exhaustion tests (§36.16) fail cleanly, never borrow authority. |
| W22-4 | Backing-state mapping (§36.7): Topo states ↔ seLe4n resource states; `Released` = revoked + recycled. | M | | state-mapping tests; no cross-label reuse without scrub+revoke. |
| W22-5a | TopoResourceServer state (§36.3.1): untyped inventory, CSlot/VSpace accounting, per-arena quota ledger. | M | | state model + accounting consistent. |
| W22-5b | TopoResourceServer IPC handlers (arena create/quota/purge/stats/backing) over `MessageInfo`/`IpcBuffer`; per-cap authorization. | L | | each request authorized by capability; denials explicit. |
| W22-5c | TopoResourceServer boot/init integration (with W22-7) + emergency reserve allocated before clients. | M | | server boots on Sim (S2); reserve independent of the normal heap. |
| W22-5d | TopoResourceServer maintenance scheduling: resumable revoke/scrub chunks per latency classes (§36.11, plan 04 W12-4). | M | ∥ | maintenance never blocks a client critical path unboundedly. |
| W22-6a | libtopomalloc-sele4n: client-side caches + local metadata for mapped objects; the no-IPC fast path (§36.11). | M | ∥ | common-path malloc/free issue no IPC. |
| W22-6b | libtopomalloc-sele4n: slow-path batching to the server; correct on denial (quota/CSlot/VSpace/policy/pressure). | M | | every denial class handled; client never assumes success. |
| W22-6c | libtopomalloc-sele4n: flush/quarantine before arena destroy, revocation, thread migration, or domain transfer. | M | ∥ | caches drained before any such event (ties plan 06 W9-6b). |
| W22-7 | Boot inventory + largest-first retype (§36.5): classify device/DMA/normal; emergency reserve before clients; provenance recorded. | M | | boot-inventory + largest-first tests (§36.16); device memory never in the normal pool. |
| W22-8a | Information-flow: label type + arena label assignment; partition local/transfer/central/backend pools by label (§36.12). | M | | pools never mix labels by construction. |
| W22-8b | Information-flow: cross-label reuse gate — high→low only after scrub+revoke (plan 08 W18-6 / plan 06 W9-6c); enforced at the central/backend boundary. | M | | cross-label reuse blocked without scrub (§36.16). |
| W22-8c | Information-flow: label-scoped stats/profiling (with plan 07 W17-6) + non-interference test (mirrors plan 02 W1-12d). | M | ∥ | low domain cannot infer high-domain patterns. |
| W22-9 | seLe4n API surface (§36.14): bootstrap/create/delegate/mallocx_arena/destroy/snapshot + full error taxonomy. | M | ∥ | every error class reachable + tested; C++ maps selected errors to `bad_alloc`. |
| W22-10 | Adapters (§36.15): VKA-like, VSpace, allocman-like, Rust `GlobalAlloc`, C++ ABI on seLe4n. | M | ∥ | each adapter passes a compatibility test (S7). |
| W22-11 | Deployment profiles (§36.18): static fixed-arena, dynamic-service, kernel-adjacent bootstrap (bump-only). | M | | fixed-arena needs no runtime retype; bootstrap profile has no general free. |
| W22-12a | Feature-flag the backend to select real `invoke_syscall` vs `Sele4nSim`; build `topo-backend-sele4n` against the real kernel ABI. | M | | both targets build; selection is one flag. |
| W22-12b | Boot `TopoResourceServer` in QEMU; serve a minimal client; vertical-slice malloc/free works on the real kernel. | L | | server boots in QEMU; client allocates/frees. |
| W22-12c | Run the §36.16 suite in QEMU; fixed-arena profile needs no runtime retype; dynamic exhaustion clean. | L | | §36.16 green on the real kernel (G-sele4n). |
| W22-13a | §36.16 test list realized as concrete tests on Sim (then reused in QEMU at W22-12c). | M | | every §36.16 bullet has a test. |
| W22-13b | Wire the single-core §36.17 theorem families (plan 02 W1-12) as CI gates for M8. | S | ∥ | G-sele4n requires the families proved. |

> **▸ Decomposition — W22-0 vs W22-1 (bind the types vs implement the behavior).** W22-0 *binds the types*
> (compile against `sele4n-abi`/`sele4n-types`, freeze the op→`SyscallId` and error→`KernelError` maps above);
> W22-1 *implements the behavior* on the host. Separating them means the mapping is reviewed once and frozen,
> and the Sim's behavior (W22-1b, the largest piece) evolves without re-touching the seam. The Sim must enforce
> the same *failures* the real kernel does — provenance (W22-1c), authority, the §36.6 order — or it gives
> false confidence; hence W22-1c is its own unit and the Sim joins differential replay (W22-1d). At M8 only the
> execution target changes (W22-12a, one feature flag); the suite (W22-13a) is reused verbatim.

> **▸ Decomposition — W22-5 (resource server) and W22-6 (client) across the IPC seam.** The server splits into
> *state* (W22-5a), *authorized IPC handlers* (W22-5b, the security-critical surface — every request is gated
> by a capability), *boot+reserve* (W22-5c), and *resumable maintenance* (W22-5d, so revoke/scrub never block a
> client unboundedly — §36.11). The client splits into the *no-IPC fast path* (W22-6a), *slow-path batching
> that is correct on denial* (W22-6b — the client is a *cache of authority*, not authority itself, so a server
> "no" must be handled gracefully), and *pre-event flush* (W22-6c). The boundary rule (§36.8.3): a corrupted
> client must never make the server revoke the wrong cap or cross a label — authorization lives in the server.

> **▸ Decomposition — W22-8 (information flow).** Labels are designed into the *data structures* (W22-8a:
> caches/central/backend partitioned by label) so cross-label mixing is impossible by construction, not by a
> later check. The *reuse gate* (W22-8b) enforces scrub-before-downgrade at the one boundary where backing
> crosses labels. *Stats redaction* (W22-8c) closes the side channel. On POSIX these are single-label no-ops,
> but the structure exists from M1 so seLe4n turns them on without a refactor (R10).

### Integration phases (§36.19 S0–S9) → milestones

| Phase | What | Milestone |
|---|---|---|
| S0 | Fit & boundary accepted | (this plan + overview) |
| S1 | Pure Lean bridge model + single-core preservation | plan 02 M1→M7 |
| S2 | Resource-server prototype boots in simulation | W22-5 @ M4 (Sim) |
| S3 | Client runtime over pre-granted arenas | W22-6 @ M4 |
| S4 | Dynamic backing: retype/map/unmap/revoke + quota/CSlot/VSpace | W22-3/7 @ M4 |
| S5 | Security labels: partition + scrub-before-downgrade + redaction | W22-8 @ M6 |
| S6 | SMP/per-core caches: pinned-thread + migration flush | plan 05 W7-5 @ M3; SMP proofs @ M9 |
| S7 | Rust + C++ integration | W22-10 @ M8 |
| S8 | Refinement hardening (server ops refine seLe4n transitions) | plan 02 W1-12/W1-14 @ M8/M9 |
| S9 | Performance validation on target | plan 10 W23-5b @ M9 |

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · `Sele4nSim` (W22-1) — a host kernel-object model behind the real ABI

**Problem.** Run and differentially test the *entire* capability backend on a host CI machine — no QEMU, no
hardware — while guaranteeing that what passes on the host also passes on the real kernel. A simulator that is
*more permissive* than the kernel gives false confidence.

**Design space.** **A deterministic host model of the kernel objects that implements the *same*
`invoke_syscall` surface as `sele4n-abi`/`sele4n-types`** — chosen. It shares the real `CPtr`/`Slot`/`ObjId`/
`TypeTag`/`AccessRights`/`KernelError` types (W22-0), so swapping to the real kernel (W22-12a) is a feature
flag. The Sim must enforce the *same failures* (provenance, authority, ordering) the kernel does.

**Structures.**
```rust
struct Sim {
  untyped: Vec<UntypedRegion>,         // with provenance + device/normal/DMA class
  cspace: HashMap<Slot, CapEntry>,     // CNode slots → derived caps
  vspace: HashMap<VAddr, Mapping>,     // installed mappings
}
fn invoke_syscall(&mut self, req: SyscallRequest) -> SyscallResponse  // returns KernelResult/KernelError
```

**Work breakdown (refines W22-1a..d).** 1. the host object model typed with the real `sele4n-types` (W22-1a).
2. `invoke_syscall` dispatch for retype/map/unmap/delete/revoke, returning real `KernelError`s (W22-1b — the
largest piece). 3. enforce the §36.6 state machine + provenance (every frame descends from authorized
untyped; device/DMA isolated) (W22-1c). 4. trace emit + replay hook for differential testing vs the Lean
bridge (W22-1d).

**Invariants.** the Sim rejects exactly what the kernel rejects (no extra permissiveness); provenance holds;
the backing state machine matches §36.6.

**Verify.** the M1 vertical slice runs over Sim *identically* to POSIX (G-sim); Sim traces replay against the
Lean bridge `R` (plan 02 W1-11); a "negative" suite asserts the Sim returns the *correct* `KernelError` for
unauthorized retype/map/revoke.

**Failure modes.** *F1* the Sim is more permissive than the kernel → host-green/QEMU-red surprise at M8 →
W22-1c's provenance/authority/order checks + the negative suite. *F2* Sim and provider drift → they share one
type surface (W22-0), so drift is a compile error (R13).

**Sequencing.** **M1**; maintained as a first-class CI backend at every milestone (G-sim).

### DD-2 · Resource server & client across the IPC seam (W22-5 / W22-6)

**Problem.** Split memory *authority* (the server, which owns untyped/CSlots/VSpace) from memory *use* (the
client library), connected only by IPC, such that a corrupted or malicious client can never make the server
revoke the wrong cap, map an unauthorized frame, or cross a label (§36.8.3).

**Design space.** **Authority lives entirely in the server; the client is a *cache of authority*, not
authority itself** — chosen. Every IPC request is authorized against a capability *in the server*; client-side
metadata is a performance cache that the server never trusts for authorization.

**Structures.**
```text
Server:  untyped inventory · CSlot/VSpace ledgers · per-arena quota · IPC handlers (cap-authorized)
Client:  per-(arena,sc,label) caches · local pagemap of mapped objects · slow-path batcher
IPC:     MessageInfo / IpcBuffer  (sele4n-abi)
```

**Work breakdown.** Server: 1 state + ledgers (W22-5a). 2 cap-authorized IPC handlers (W22-5b — the
security-critical surface). 3 boot + emergency reserve (W22-5c). 4 resumable maintenance (W22-5d, so
revoke/scrub never block a client unboundedly — §36.11). Client: 5 no-IPC fast path (W22-6a). 6 slow-path
batching **correct on denial** (W22-6b). 7 flush/quarantine before destroy/revoke/migration (W22-6c).

**Invariants.** every server mutation is authorized by a capability the *caller* holds; the client is correct
even if the server returns `TOPO_ERR_QUOTA/CSPACE/VSPACE_EXHAUSTED`/`AUTHORITY_DENIED`; client caches refine
server-granted authority (plan 02 W1-12a `client_cache_refines_server_authority`).

**Verify.** denial-injection: the server says "no" for each error class and the client recovers cleanly;
a "malicious client" test corrupts client metadata and asserts the server still authorizes correctly;
deterministic trace replay of server operations (S2).

**Failure modes.** *F1* the server trusts client metadata for authorization → privilege escalation → all
authorization re-derived server-side. *F2* unbounded revoke/scrub on a client's critical path → resumable
maintenance chunks (W22-5d) + latency classes (plan 04 W12-4).

**Sequencing.** **M4** (Sim); real-kernel at **M8**.

### DD-3 · Information-flow labels (W22-8) — partition by construction

**Problem.** A low-security domain must not infer a high domain's allocation patterns, and high-domain dirty
memory must not be reused by a low domain without scrubbing (§36.12). Bolting checks on later is error-prone.

**Design space.** **Design the label into the data structures so cross-label mixing is impossible by
construction**, not caught by a later test — chosen. Caches, central lists, and backend pools are keyed by
label; one gate (the central/backend boundary) enforces scrub-before-downgrade.

**Work breakdown (refines W22-8a..c).** 1. label type + arena label; partition local/transfer/central/backend
pools by label (W22-8a). 2. the cross-label reuse gate: high→low only after scrub+revoke (W22-8b → plan 06
W9-6c / plan 08 W18-6). 3. label-scoped + redacted stats (W22-8c → plan 07 W17-6).

**Invariants.** no cache/list/pool mixes labels; backing crosses a label only after scrub+revoke; low-domain
stats are aggregated/redacted so they cannot reveal high-domain patterns.

**Verify.** the label test (§36.16): high-domain dirty memory cannot be reused by a low-domain arena without
scrub; the redaction test; mirror to Lean `label_partition_preserved` + `stats_observation_noninterference`
(plan 02 W1-12c/d). On POSIX these are single-label no-ops — but the *structure* exists from M1, so seLe4n
turns them on without a refactor (R10).

**Failure modes.** *F1* a shared "default" pool silently mixes labels → partitioning is structural, not
optional. *F2* a stats field leaks a high-domain count → redaction + the non-interference test.

**Sequencing.** structure from **M1** (single-label); enforcement + redaction at **M6**.

### DD-4 · Boot inventory & largest-first retype (W22-7)

**Problem.** At boot the root task is handed raw untyped capabilities of varied sizes, addresses, and device
flags. They must be classified exactly once and carved with minimal watermark/alignment waste, with every
heap page provably descended from authorized, non-device untyped (§36.5, R12 of the SPEC).

**Design space.** **Classify (device/DMA/normal/reserved), then split normal untyped largest-first into
backing pools, reserving CSlots before frames and VSpace windows before mapping** — chosen (§36.5): largest-
first reduces watermark loss; reserving slots/windows first avoids a frame with nowhere to store its cap.

**Work breakdown (refines W22-7).** 1. enumerate + classify untyped from boot info. 2. reserve server CSlots +
VSpace metadata windows. 3. allocate the emergency reserve *before* enabling clients. 4. split normal untyped
largest-first into pools, recording provenance.

**Invariants.** each untyped classified exactly once; device/DMA never enters the normal pool; every mapped
heap page descends from authorized untyped (plan 02 W1-12b `backing_descends_from_untyped`).

**Verify.** the boot-inventory test (every root untyped classified once) and the largest-first test (splits
avoid known alignment-waste patterns) (§36.16).

**Failure modes.** *F1* device memory in the normal pool → DMA/MMIO corruption → the classification gate. *F2*
a frame retyped with no CSlot to hold its cap → reserve slots first.

**Sequencing.** **M4** (Sim); real boot at **M8**.

### DD-5 · Real-kernel bring-up in QEMU (W22-12)

**Problem.** Switch the execution target from the host Sim to the real seLe4n kernel without changing the
provider API, and prove the §36.16 suite passes on the real kernel.

**Design space.** **A feature flag selecting real `invoke_syscall` vs Sim** — chosen: the provider compiled
against `sele4n-abi` since M1 (W22-0), so M8 changes only which crate provides the syscall, not the allocator.

**Work breakdown (refines W22-12a..c).** 1. feature-flag the backend; build `topo-backend-sele4n` against the
real kernel ABI (W22-12a). 2. boot `TopoResourceServer` in QEMU; serve a minimal client; vertical-slice
malloc/free (W22-12b). 3. run the §36.16 suite in QEMU; fixed-arena needs no runtime retype; dynamic
exhaustion clean (W22-12c).

**Invariants.** the real-kernel slice passes the *same* suite the Sim passed; the API surface is unchanged.

**Verify.** the suite (W22-13a) runs unmodified in QEMU; G-sele4n gates M8 on it + the single-core §36.17
theorem families (W22-13b).

**Failure modes.** *F1* a behavior the Sim allowed but the kernel forbids → caught here, traced back to a Sim
permissiveness bug (DD-1 F1) and the Sim is tightened. *F2* QEMU timing differs from RPi5 → perf is plan 10
W23-5b, not a correctness gate.

**Sequencing.** **M8**; SMP/per-core at **M9**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M0 | W22-0 (pin + maps) so the seLe4n provider stub type-checks (plan 04 W4-1). |
| M1 | W22-1 (`Sele4nSim`); the M1 vertical slice runs over Sim identically to POSIX (G-sim). |
| M4 | W22-2/3/4 (authority, CSlot/VSpace, state mapping), W22-5/6 (server+client on Sim), W22-7 (boot inventory). |
| M6 | W22-8 (labels, scrub gate, redaction). |
| M8 | W22-9/10/11 (API, adapters, profiles), **W22-12 (real kernel in QEMU)**, W22-13 (suite + theorem gates). |
| M9 | SMP/per-core bridge (plan 02 W1-14); perf (plan 10 W23-5b). |

## Domain risks

- **R2** (co-equal slips) — W22-0/W22-1 land in M1 and are CI-gated (G-sim) every milestone. **R7**
  (cap/CSlot leaks) — W22-3 accounting + plan 06 W9-6 revocation + the theorem. **R8** (IPC cost) — W22-6
  batching + fixed-arena profile (W22-11) + latency classes. **R10** (info-flow) — W22-8. **R13** (ABI drift)
  — D8 pin + Sim mirrors the pinned surface ⇒ drift is a compile error.

## Definition of Done (addendum)

Every W22 WU passes on **`Sele4nSim`** (and, from M8, in QEMU); every IPC handler is capability-authorized;
every backing transition obeys the §36.6 order and is provenance-checked; the bridge theorem it relates to
(plan 02 W1-12) is proved or carries a `V-004` debt.

## Best-practices checklist

- [ ] Never an in-kernel heap; user/service-level only (§36.2).
- [ ] Bind types once (W22-0), implement behavior separately (W22-1); swap to real kernel via one flag.
- [ ] The Sim enforces the *same failures* as the kernel (provenance, authority, order) — no false confidence.
- [ ] Authorization lives in the server; the client is a cache of authority, correct on denial.
- [ ] Labels partition structures by construction; scrub-before-downgrade at the one cross-label boundary.
- [ ] Device/DMA memory never enters the normal pool; largest-first retype reduces watermark waste.

# Plan 02 — Formal Model & seLe4n Bridge

**Workstreams:** W1 · **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §33 (whole), §9.5, §21.6, §22.5/§22.6, §33.5, §36.3.3, §36.7, §36.12, §36.17; V-001..V-005.
**Upstream deps:** [plan 01](01-repository-and-infrastructure.md) (Lean toolchain, `lake`). **Downstream:**
plan 03 (tables), plan 05 (conservation theorems), plan 08 (trace oracle), plan 09 (bridge).
**Milestones:** continuous from **M0**; gates G-table (M1), G-model (M1→), full single-core theorem set (M7),
SMP forms (M9). This is the **Formal track** and is *mostly independent* — it unblocks others via the tables
and the trace oracle.

> Lean is used to define the allocator's abstract state machine and prove its invariants. It is **not** on the
> production hot path (§33.1). Its three deliverables make conformance real: (a) the generated/verified
> tables consumed by the implementation, (b) the trace-replay oracle for differential testing, (c) the proofs.

---

## Lean module layout (the artifact this plan builds)

```text
/lean/TopoMalloc/
  Types.lean          Addr, Bytes, CpuId, ArenaId, SizeClassId, SpanId, HugePageId, Range, Owner
  State.lean          the abstract State; ownership map; span/pagemap/hugepage models
  WellFormed.lean     the WellFormed predicate (§33.3) — see clause set below
  SizeClass.lean      parameterized table builder + the §9.5 proofs (W1-4)
  Transitions.lean    malloc/free/central_batch_*/cache_refill_flush as total functions (W1-5)
  Rseq.lean           RseqPop/RseqPush contract + axioms (W1-7, §33.5)
  Theorems/           one file per §33.4 theorem family (W1-6, W1-8, W1-9)
  Exec.lean           executable model + trace replay (W1-10, §33.7)
/lean/TopoMalloc/SeLe4n/                                            (the bridge package, §36.3.3)
  Bridge.lean  CapBackedArena.lean  UntypedProvider.lean  VSpaceProvider.lean
  CSpaceProvider.lean  ResourceServer.lean  ClientRuntime.lean  InformationFlow.lean
  SMP.lean  Refinement.lean
```

The bridge imports TopoMalloc's model and seLe4n's *public* model interfaces only, and **must build without
seLe4n** (feature-gated), so plan 02 never blocks on plan 09.

---

## W1 — Formal core & Lean model (+ seLe4n bridge)

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W1-1 | `lake` package + CI `lake exe check`; core types (§33.2): `Owner`, `Range`, `State`. | S | | `Owner` includes all SPEC owners incl. `released`/`quarantine`; compiles in CI. |
| W1-2 | **Empty bridge package** scaffolding (§36.3.3). | S | ∥ | `SeLe4n/*.lean` compile (stubs); importable without seLe4n internals. |
| W1-3 | `WellFormed` predicate (§33.3) — see clause set. | M | | predicate total; each clause cross-referenced to a SPEC bullet. |
| W1-4a | Abstract `SizeClass` record + parameterized table-construction function (size, alignment, slab_pages, objects_per_slab, batch_size, max_local_capacity). | M | | table derives from params (D4); no literals. |
| W1-4b | Prove the **spacing-dominated** bound `r(c)=size(c)/size(prev(c)) ≤ 1+W` per §9.4 range. | M | ∥ | per-range spacing lemma discharged. |
| W1-4c | Prove the **alignment-dominated** caveat (`req<q/W` ⇒ waste ≤ `(q-1)/req`); classify each class into its regime. | S | ∥ | regime split machine-checked. |
| W1-4d | Prove the layout lemmas (§9.5): `size(c)` an integer multiple of `alignment(c)`; objects fit the span; ranges pairwise-disjoint. | M | | layout lemmas proved. |
| W1-4e | Prove lookup obligations (total, monotonic, in-bounds, `size≥req`, `align≥req.align`, `batch≤max_local_capacity`); **emit** the table + golden-diff contract for plan 03. | M | | `size_class_table_covers_all_small_requests` proved; emitted table is the single source of truth. |
| W1-5 | Abstract transitions `malloc`/`free`/`central_batch_remove/insert`/`cache_refill/flush` as total functions on `State`. | M | | each is total. |
| W1-6a | `malloc_preserves_wellformed` + `malloc_success_returns_aligned_sufficient_disjoint_object`. | M | | both proved, named per §33.4. |
| W1-6b | `free_preserves_wellformed_for_valid_pointer` + `free_removes_liveness_and_adds_exactly_one_free_owner`. | M | ∥ | both proved. |
| W1-6c | `cache_refill_preserves_ownership_conservation` + `cache_flush_preserves_ownership_conservation`. | M | | both proved; consume W1-7. |
| W1-7 | RSEQ abstraction (§33.5): `RseqPop`/`RseqPush` with **abort / empty(full) / success** + **frame condition**. | M | ∥ | distinct abort vs empty/full; frame condition present. |
| W1-8a | `span_split_preserves_disjointness` + `span_merge_preserves_disjointness`. | M | | both proved; mirror plan 04 W4-2b/c. |
| W1-8b | `pagemap_lookup_sound`. | M | ∥ | proved; mirrors plan 03 W3-3d. |
| W1-8c | `release_to_os_preserves_live_objects` (§21.6 release-safety theorem). | M | | proved; mirrors plan 04 W12-2. |
| W1-9 | `arena_reset_invalidates_only_target_arena` + `arena_destroy_preserves_other_arenas`. | M | | both proved; mirror plan 06 W9-4. |
| W1-10 | Executable model + trace replay (§33.7): consume the grammar, check `WellFormed` at boundaries. | M | | replays a recorded trace; flags an injected violation. |
| W1-11a | Bridge relation `TopoState ↔ SeLe4n SystemState` + abstraction function (§36.3.3, §36.7). | M | | compiles; importable without seLe4n internals. |
| W1-11b | Backing-provider state machine (§36.6 order: `AuthorizedUntyped → … → RecyclableUntyped`). | M | ∥ | transitions match §36.6 exactly. |
| W1-11c | Single-label `TopoSeLe4nWellFormed` composing TopoMalloc `WellFormed` + seLe4n invariants. | M | | predicate total; clauses cross-referenced. |
| W1-12a | Authority/quota family: `arena_cap_authorizes_alloc`, `arena_quota_preserved`, `client_cache_refines_server_authority`. | M | | proved single-core. |
| W1-12b | Provenance/release family: `backing_descends_from_untyped`, `no_live_object_released`. | M | ∥ | proved single-core. |
| W1-12c | Destroy/label/scrub family: `destroy_revokes_descendants`, `label_partition_preserved`, `scrub_before_downgrade`. | M | | proved single-core; mirror plans 06/08. |
| W1-12d | Per-core/stats/composite family: `per_core_cache_abort_no_change`, `stats_observation_noninterference`, `topo_step_preserves_sele4n_invariants`. | M | ∥ | proved single-core; gate M7/M8. |
| W1-13 | Non-interference (§36.12): `topo_step_preserves_low_equivalence` for the cache/stats steps. | M | ∥ | proved for the modeled steps. |
| W1-14 | SMP/per-core bridge extensions (§36.17 SMP forms) — staged per V-004. | L | | proved or recorded as explicit refinement debt by M9. |

### `WellFormed` clause set (W1-3 acceptance detail, §33.3)

Each clause is a named lemma so a transition proof can cite exactly the ones it preserves:

1. live ranges pairwise-disjoint · 2. every block has exactly one `Owner` · 3. caches contain only blocks
they own · 4. central lists contain only blocks they own · 5. span objects fit their span range · 6. span
bitmaps agree with ownership · 7. pagemap agrees with span descriptors · 8. hugepage occupancy agrees with
spans · 9. arena ownership unique · 10. released ranges contain no live block · 11. cache capacities respected.

> **▸ Decomposition — W1-4 (size-class model), the M1 longest pole.** Split so each sub-proof ships
> independently and a generator change re-checks cheaply rather than re-proving the whole table. The two
> regimes (W1-4b spacing, W1-4c alignment) exist because the SPEC (§9.4) proves a flat per-request waste
> target is *unattainable* below the ABI quantum `q` (a 17 B request under a 16 B quantum rounds to 32 B).
> **Obligations, in order:** (1) build the table from params (4a) → (2) per-range spacing bound (4b) ∥ (3)
> small-request alignment caveat (4c) → (4) alignment-multiple + slab-fit + disjointness (4d) → (5) lookup
> totality/coverage + emit (4e). Output is *data* consumed by plan 03 W2-1 and golden-diffed in CI (G-table).
> **Start W1-4a in M0** alongside plan 01; 4d/4e gate M1.

> **▸ Decomposition — W1-7 (RSEQ contract), the load-bearing axiom.** Modeled first as an axiom with a
> precise contract (§33.5). The three outcomes **must be distinct**: `abort` (preempted/migrated → retry,
> state unchanged), `empty`/`full` (genuine underflow/overflow → slow path), `success` (one object changes
> owner). The **frame condition** — *only* the popped/pushed object changes owner — is what lets W1-6c
> (refill/flush conservation) be proved at all; conflating `abort` with `empty` would let the model "prove"
> progress the implementation does not make. Plan 05's two front-ends (RSEQ, pinned-core) each discharge this
> contract; W1-14 may later refine the axiom to verified per-arch assembly (open question 7).

> **▸ Decomposition — W1-11/W1-12 (the bridge), co-developed not deferred.** W1-11 builds the *relation* and
> the §36.6 state machine; W1-12 proves the theorem families, split into four logical clusters (authority/
> quota, provenance/release, destroy/label/scrub, per-core/stats/composite) so each can land with the
> implementation WU it certifies (e.g. W1-12c lands with plan 06 W9-6 revocation and plan 08 W18-6 scrub).
> Single-core forms gate M7/M8; the SMP forms (W1-14) may stage to M9 per §36.17/V-004 — staging applies
> *only* to the SMP extensions, never to the bridge package or the single-core checklist (both required).

---

## Deep dives

> Each deep dive expands one complex task into **Problem · Design space · Structures · Work breakdown (finer
> than the table) · Invariants · Verify · Failure modes · Sequencing.** They are the authoritative
> engineering detail; the tables above are the index.

### DD-1 · Size-class model & generator (W1-4) — the M1 longest pole

**Problem.** The size-class table is the allocator's most safety-critical *generated* artifact: every span's
slab layout, every alignment guarantee, and every `size→class` lookup derive from it. One wrong row breaks
disjointness or alignment *everywhere*. The SPEC (§9.4) further proves a flat per-request waste target is
**unattainable** below the ABI quantum `q` — a 17-byte request under a 16-byte quantum must round to 32 —
so the model cannot promise a single bound; it must prove two regime-specific ones.

**Design space.** (a) hand-written table — *rejected*, Appendix F anti-pattern; (b) Rust generator with its
own separate tests — drift risk between code and proof; (c) **Lean derives the table as data, Rust consumes
it, CI golden-diffs both against the Lean output** — *chosen*: one source, machine-checked, impossible to
drift (R4).

**Structures.**
```lean
structure SizeClass where
  size : Nat            -- usable bytes; a multiple of `align`
  align : Nat           -- power of two
  slabPages : Nat       -- span size in allocator pages
  objects : Nat         -- objects carved per slab = (slabPages*page - hdr) / size
  batch : Nat           -- middle→front transfer batch
  cap : Nat             -- per-CPU hard capacity;  batch ≤ cap
structure Params where  W : Nat;  q : Nat;  page : Nat;  smallMax : Nat   -- D4 build constants
def buildTable (p : Params) : Array SizeClass := …   -- pure function of p; no literals
```

**Work breakdown (refines W1-4a..e).**
1. `buildTable` + `Params` (W1-4a) — pure, parameterized. *S/M.*
2. Spacing lemma `∀ c, size(c) ≤ (1+1/W)·size(prev c)` over the spacing-dominated range (W1-4b). *M, ∥.*
3. Quantum-tail lemma `∀ req < q/W, waste(req) ≤ (q-1)/req`; tag each class's regime (W1-4c). *S, ∥.*
4. Layout lemmas (W1-4d): `align ∣ size`; `objects·size + hdr ≤ slabPages·page`; object ranges pairwise
   disjoint. *M.*
5. Lookup lemmas + emit (W1-4e): `size_class` total + monotonic + in-bounds + `size≥req` + `align≥req.align`;
   `batch ≤ cap`; serialize the table to `tools/size-class-gen`'s golden. *M.*

**Invariants.** `align(c)` power-of-two and divides `size(c)`; classes strictly increasing; every small
request `≤ smallMax` maps to exactly one class with `size ≥ req`; `batch ≤ cap`.

**Verify.** Lean proves steps 2–5. Plan 03 W2-1 emits Rust tables from the *same* serialized data; CI
golden-diffs (G-table). An **exhaustive** small-range differential test (every `req ∈ [0, smallMax]` × a set
of alignments) checks the Rust `size_class` against the Lean lookup.

**Failure modes.** *F1* off-by-one at a class boundary → exhaustive differential catches it. *F2* `align ∤
size` → step 4 fails to compile. *F3* `batch > cap` (front-end overflow) → step 5 fails.

**Sequencing.** W1-4a in **M0**; W1-4b..e in **M1** (gate **G-table**). It blocks plan 03 W2 and therefore
all of M1 — start it first.

### DD-2 · RSEQ contract (W1-7) — the load-bearing axiom

**Problem.** Per-CPU lock-free pop/push (plan 05 W7) is correct only against a *precise* contract for the
restartable hardware sequence. The contract must capture preemption/migration **without overclaiming
progress**: if the model let a pop "succeed" after an abort, the conservation proofs would certify behavior
the implementation never makes.

**Design space.** (a) model RSEQ as a lock — *rejected*: hides the abort case the implementation must
handle; (b) **axiom with three disjoint outcomes + a frame condition** — *chosen* (§33.5); (c) verified
per-arch assembly — deferred to W1-14 / open question 7.

**Structures.**
```lean
inductive RseqPop (s : State) (cpu : CpuId) (sc : SizeClassId)
  | abort                                   -- preempted/migrated; state unchanged; caller retries
  | empty                                   -- genuine underflow; caller takes the slow path
  | success (p : Addr) (s' : State)
axiom rseq_pop_contract : … success p s' →
  OwnerOf s p = .cpuCache cpu sc ∧ OwnerOf s' p = .live ∧ WellFormed s' ∧
  (∀ q, q ≠ p → OwnerOf s' q = OwnerOf s q)              -- FRAME: only p changes owner
```

**Work breakdown (refines W1-7).** 1. `RseqPop`/`RseqPush` inductives. 2. the two contract axioms with the
frame clause. 3. corollaries used by W1-6c (refill/flush conservation).

**Invariants.** `abort` ⇒ state unchanged; `empty`/`full` are *logical* under/overflow, **distinct** from
`abort`; the frame condition holds for `success`.

**Verify.** W1-6c (cache conservation) *consumes* the frame clause — it cannot be proved without it, which
is the proof that the clause is load-bearing. The implementation's empirical counterpart is plan 05 W7-2e/3c:
a forced-migration differential test showing the asm makes exactly the moves the axiom permits.

**Failure modes.** *F1* conflating `abort` with `empty` → the model "proves" progress the implementation
doesn't make → mitigated by keeping the constructors disjoint and asserting it in W1-12d's
`per_core_cache_abort_no_change`.

**Sequencing.** W1-7 in **M2**; exercised by both front-ends (RSEQ + pinned-core) at **M3**.

### DD-3 · The seLe4n bridge (W1-11/W1-12) — co-developed, not bolted on

**Problem.** Relate the allocator's abstract `TopoState` to seLe4n's `SystemState` and prove the §36.17
theorem families, *without* coupling to private kernel internals and *without* blocking plan 02 on plan 09.

**Design space.** (a) re-model seLe4n inside this package — *rejected*: duplication + drift with the real
kernel; (b) **import seLe4n's public model interfaces + define an abstraction relation `R : TopoState →
SystemState → Prop`** — *chosen*; the bridge builds with seLe4n absent (feature-gated stubs).

**Structures.** `R` (the abstraction relation, W1-11a); the §36.6 backing state machine as a Lean transition
system `AuthorizedUntyped → … → RecyclableUntyped` (W1-11b); `TopoSeLe4nWellFormed := WellFormed ∧
seL4Invariants ∧ labelPartition` (W1-11c).

**Work breakdown (refines W1-12) — four clusters, each landing with the impl WU it certifies.**
1. authority/quota (W1-12a) ↔ plan 06 W9-1/W9-5, plan 09 W22-2/W22-6.
2. provenance/release (W1-12b) ↔ plan 09 W22-7, plan 04 W12.
3. destroy/label/scrub (W1-12c) ↔ plan 06 W9-6, plan 08 W18-6, plan 09 W22-8.
4. per-core/stats/composite (W1-12d) ↔ plan 05 W7-5, plan 07 W17-6.

**Invariants.** every heap frame descends from authorized untyped; delegation is authority/quota/label
monotone; caches/lists never mix labels; `Released` ⇒ revoked + recycled.

**Verify.** each cluster is proved single-core and gated at M7/M8 (G-sele4n); `Sele4nSim` traces (plan 09
W22-1d) replay against `R` so the simulator and the model agree.

**Failure modes.** *F1* SMP/per-core forms unproved → **staged** to M9 per V-004 as explicit refinement debt
— staging applies *only* to the SMP extensions, never to the bridge package or the single-core checklist.

**Sequencing.** W1-11 from **M0** (skeleton) → **M4** (state machine); W1-12 single-core across **M4→M7/M8**;
W1-14 SMP at **M9**.

---

## Sequencing & milestone mapping

| Milestone | W1 deliverables |
|---|---|
| M0 | W1-1, W1-2, W1-4a (start); empty bridge compiles. |
| M1 | W1-3, W1-4b..e (G-table), W1-5, W1-6a/b, W1-8b; bridge `WellFormed` skeleton (W1-11a/c single-label). |
| M2 | W1-6c (consumes W1-7), W1-7, W1-10 (trace oracle for plan 08). |
| M3 | RSEQ contract (W1-7) exercised by both front-ends; `per_core_cache_abort_no_change` shape. |
| M4 | W1-9 (arena theorems); W1-11b (provider state machine); W1-12a (authority/quota). |
| M5 | W1-8a/c (split/merge, release safety). |
| M6 | W1-12d (`stats_observation_noninterference`), W1-13. |
| M7 | **full §33.4 set + single-core §36.17 (W1-12a..d)**; trace replay in CI; V-004 debt → 0. |
| M9 | W1-14 (SMP forms) proved or documented as refinement debt. |

## Domain risks

- **R4** (table drift) — owned with plan 03: single generator + Lean check + golden-diff.
- **R9** (proofs lag → "formal theater") — owned here: the G-model gate forces every shipped transition to be
  proved or carry a tracked `V-004` debt; the debt list is reviewed every milestone and burned to zero
  (single-core) at M7. *Best practice:* never let `Theorems/` lag `Transitions.lean` across a release.

## Definition of Done (addendum)

Every W1 WU: (1) `lake exe check` green in CI; (2) theorems named **exactly** per §33.4/§36.17 for
traceability; (3) if it changes a transition, the corresponding `Transitions.lean`/`Exec.lean` and the
differential corpus (plan 08) are updated in lockstep.

## Best-practices checklist

- [ ] Tables are emitted **data**, not Lean literals copied by hand (Appendix F).
- [ ] Each `WellFormed` clause is a named lemma so transition proofs cite precisely what they preserve.
- [ ] RSEQ `abort`/`empty`/`success` stay distinct; the frame condition is present and used.
- [ ] The bridge builds without seLe4n; bridge theorems cluster with the implementation WUs they certify.
- [ ] The executable model (W1-10) stays in sync with the implementation via differential replay (plan 08).

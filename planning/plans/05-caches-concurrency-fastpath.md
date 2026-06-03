# Plan 05 — Caches, Concurrency & Fast Paths

**Workstreams:** W6 (front/middle caches), W7 (RSEQ/restartable + asm), W16 (concurrency/ordering/fork/TLS) ·
**Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §11, §12, §13, §14.2–§14.4, §27, §28, §35.4; B.2, P-001..P-003, S-008/S-010.
**Upstream deps:** [03](03-core-allocator.md) (central list), [02](02-formal-model.md) (RSEQ contract).
**Downstream:** [06](06-api-realloc-arenas.md) (the public fast path), [04](04-backend-hugepages-release.md)
(cache drain into release). **Milestones:** caches + concurrency at **M2**; RSEQ/pinned-core fast paths at
**M3**.

> The performance heart of the allocator — *and* its subtlest correctness. The rule is **correct before
> fast**: the locked per-CPU baseline (W6-4) ships first, then RSEQ (W7) makes it lock-free with *no*
> behavioral difference (proved by equivalence tests). The only hand-written assembly in the project lives
> here, decomposed instruction-class by instruction-class.

## Front-end contract (owned here, consumed by plan 06)

```rust
enum FeOutcome<T> { Success(T), Empty, Full, Abort }   // Abort ≠ Empty/Full (SPEC §33.5)
fn fe_pop(core: CoreId, arena: ArenaId, sc: SizeClassId) -> FeOutcome<Ptr>;
fn fe_push(core: CoreId, arena: ArenaId, sc: SizeClassId, p: Ptr) -> FeOutcome<()>;
```

`Abort` (RSEQ preemption/migration → retry, state unchanged) is **distinct** from `Empty`/`Full` (genuine
underflow/overflow → slow path). Both the locked baseline and the RSEQ/pinned-core fast paths implement this
one contract; the Lean RSEQ axiom (plan 02 W1-7) is its specification.

---

## W6 — Front-end & middle-end caches (portable, correct before RSEQ)

**Depends on:** plan 03 W5. **Enables:** W7, plan 06 (fast path), plan 04 W12 (drain).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W6-1a | Thread cache (§13): per-`(arena,sc,label)` free lists; push/pop/refill/flush. | M | | basic tcache correct under single thread. |
| W6-1b | Thread-cache GC on exit/pressure/budget (§13.3) + arena-reset drain precondition (§13.4). | M | ∥ | thread-exit flush; budget bounded (B.2); reset drains. |
| W6-2 | Transfer cache `(domain, arena, label, sc)` with `Batch` (§14.2); distinct/correct/free guarantee. | M | ∥ | batch invariants tested. |
| W6-3a | Refill (§14.3): transfer batch → central batch (plan 03 W5-4b) → new span; push to cpu/thread cache. **Hand-over-hand:** release the transfer lock before taking the central lock. | M | | never holds two middle-end locks; conservation matches plan 02 W1-6c. |
| W6-3b | Flush (§14.4): pop a batch → transfer cache if capacity else central (W5-4c); same hand-over-hand. | M | ∥ | lock-order checker clean; conservation matches W1-6c. |
| W6-3c | Wire empty-span detection (plan 03 W5-3e) into flush-to-central; debug conservation check (B.2). | S | | a flush that empties a span triggers detection. |
| W6-4 | Per-CPU cache structure + **locked** per-CPU mode (§11.2–§11.5) as the RSEQ-free correct baseline + RSEQ fallback. | M | | hard-capacity invariant (§11.5) holds; ready as fallback. |
| W6-5 | Cache budget controller v1 (§11.5, P-005): adapt to miss/overflow counts; global budget + per-CPU soft/hard. | M | ∥ | budget honored; stats expose miss/overflow. |
| W6-6 | Arena routing (D6, §11.7): bound-arena fast path now; arena-qualified `(cpu,arena,sc)` slots wired for M4. | M | | free always returns to the owning arena's structures; alloc from A returns only A's objects. |
| W6-7 | Idle-CPU/affinity-change flush (§11.6) + a control to release stranded caches. | S | ∥ | flushing an idle CPU moves objects to transfer/central; control hook present (plan 07). |

> **▸ Decomposition — W6-3 (refill/flush), the hand-over-hand rule.** Refill and flush are the only places
> two middle-end locks could be wanted at once; the lock hierarchy (W16-1) forbids holding them together, so
> both are written **hand-over-hand** — release the transfer-cache lock *before* acquiring the central lock.
> Splitting refill (W6-3a), flush (W6-3b), and the empty-detection wiring (W6-3c) lets each be proved against
> the conservation theorems (plan 02 W1-6c) and checked by the lock-order checker independently. **Pitfall:**
> a "fast" refactor that grabs both locks to avoid a re-lookup reintroduces the deadlock the hierarchy
> exists to prevent — the checker (W16-1b) is the gate that catches it.

---

## W7 — RSEQ / restartable fast paths & per-arch assembly

**Depends on:** W6-4, plan 02 W1-7. **Enables:** M3. The only hand-written assembly in the project.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W7-1 | RSEQ registration + per-thread availability detection (§12.3); initial-exec TLS interplay (W16-2). | M | | registered on Linux; clean fallback when absent (P-003). |
| W7-2a | RSEQ critical-section descriptors + abort-handler trampolines in a dedicated section; register the rseq area. | M | | cs table present; abort vector wired. |
| W7-2b | x86-64 `rseq_pop` (load cpu → load head → single committing store); abort ⇒ logical no-op. | M | | pops one or reports empty; abort unchanged. |
| W7-2c | x86-64 `rseq_push` (capacity check → store → commit index); abort ⇒ no-op. | M | ∥ | pushes one or reports full; abort unchanged. |
| W7-2d | Clobber/barrier docs + compiler-fence discipline; audit/lint that no call or faulting ref occurs inside a CS (§12.3). | S | | documented; lint passes. |
| W7-2e | x86-64 equivalence vs locked mode under forced migration (feeds W7-6). | M | | no lost/duplicated object vs locked (G-fast). |
| W7-3a | AArch64 `rseq_pop`/`rseq_push` + abort handler (commit-store model); **shares the arch with the seLe4n RPi5 target**. | L | ∥ | pop/push correct; abort unchanged. |
| W7-3b | AArch64 clobber/barrier docs + no-call/no-fault-in-CS audit. | S | ∥ | documented; lint passes. |
| W7-3c | AArch64 equivalence vs locked under forced migration (QEMU). | M | ∥ | matches locked (G-fast). |
| W7-4 | Non-owner coordination (§27.4): flushing an idle CPU vs the owner's RSEQ (epoch / stop-the-world / per-CPU lock). | M | | concurrent flush-vs-fastpath stress clean. |
| W7-5 | seLe4n pinned-thread per-core mode (§36.10 option 1) behind the same front-end contract; abort/no-change case. | M | | migration flush/hand-off correct; `per_core_cache_abort_no_change` mirrored. |
| W7-6 | RSEQ test battery (§34.5): migration, signal near sequence, preemption, registration failure, compare-vs-locked. | M | | all pass in CI (QEMU where needed). |

> **▸ Decomposition — W7-2/W7-3 (per-arch restartable sequences), the highest-risk code.** Each sequence is
> its own reviewable unit with its own equivalence test. Non-negotiable rules (§12.3): the critical section
> contains **no calls and no possibly-faulting memory reference**; the abort handler restores a logical
> no-op; the **only** state-changing instruction is the single commit store at the end, so an abort before
> commit is invisible. Each sequence is validated *two* ways — the Lean RSEQ contract (plan 02 W1-7) with its
> abort/empty/success + frame condition, and a forced-migration differential test against the locked baseline
> (W6-4) showing identical object movement. **Why pop/push/abort are separate units:** they fail differently
> (pop underflows, push overflows, abort retries) and the SPEC requires these be *distinct* outcomes (§33.5).
> AArch64 (W7-3) is not a "port" afterthought — it is the seLe4n target arch, so it is co-primary with x86-64.

---

## W16 — Concurrency, memory ordering, fork, signal, TLS

**Depends on:** touches all. **Enables:** **M2** (real concurrency).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W16-1a | Encode the lock ranks (table below, §27.2) as a typed/ranked lock wrapper; acquisition records its rank. | M | | every lock has a compile-time rank; refill/flush hold ≤1 middle-end lock by construction. |
| W16-1b | Debug lock-order checker: a per-thread held-rank stack asserts monotonic acquisition; wired as the **G-conc** gate. | S | ∥ | any out-of-order acquire fails in debug CI. |
| W16-2 | **TLS initial-exec model** (§27.6): no `malloc` re-entry on first TLS access; `dlopen` allocation-free bootstrap path. | M | | TLS-recursion test (load via dlopen) does not re-enter the allocator. |
| W16-3 | Atomics-ordering map (§27.3): publication=release, consumption=acquire, transitions=acq-rel, stats=relaxed; documented per atomic. | M | ∥ | each atomic annotated; TSan clean. |
| W16-4 | Global lock (M1) → fine-grained hierarchy (M2) migration without correctness regression. | M | | M1 passes with the global lock; M2 with the hierarchy. |
| W16-5a | Pre-fork + parent-post-fork handlers (§28.1): acquire the fork lock + quiesce background threads pre-fork; release + resume in the parent. | M | | parent unaffected; no leaked held lock. |
| W16-5b | Child-post-fork handler: reset lock states, disable background threads, flush/conservative-mode inconsistent per-CPU state. | M | | fork-in-multithread test: child allocates safely; no inherited held lock. |
| W16-6 | Signal/reentrancy/crash (§28.2–§28.4): document non-async-signal-safety; reentrancy guard; lock-free crash summary. | S | ∥ | reentrancy during init/hooks handled; crash summary needs no lock/alloc. |
| W16-7 | Initialization phases (§35.4) Phase 0–6, each reentrancy-safe; shutdown policy (§35.5). | M | | phased-init test; teardown available for tests, leak-by-default in prod. |

### Lock hierarchy (W16-1, §27.2 — the total order)

```text
Global config (rank 0) → Arena registry (1) → Arena (2) → Transfer cache (3, per-LLC)
  → NUMA central list (4, per-NUMA) → Span (5) → Backend extent (6) → Stats shard (7)
```

Acquire strictly in increasing rank. The refill/flush paths (W6-3) are written hand-over-hand and hold **at
most one** of ranks 3–6 at a time. The checker (W16-1b) enforces monotonicity at runtime in debug.

> **▸ Decomposition — W16-2 (TLS), a top risk (R3).** The allocator's own TLS **must** use the initial-exec
> model (or a static `__thread` slot reached without dynamic TLS allocation): general-dynamic TLS can trigger
> a lazy allocation on first access, re-entering the allocator before its per-thread state exists →
> recursion/deadlock. When loaded via `dlopen` (where initial-exec may be unavailable), an allocation-free
> per-thread bootstrap path is required, consistent with the phased init of W16-7. This is the threading
> analogue of plan 03's bootstrap-metadata rule (S-007) and **must be proven at M2 before the W7 fast paths
> land**, because the fast paths assume per-thread state already exists.

> **▸ Decomposition — W16-5 (fork).** Three distinct execution contexts, hence three sub-units: pre-fork
> (acquire + quiesce), parent-post-fork (release + resume), child-post-fork (reset locks held by vanished
> threads, disable background threads, conservative mode). The child handler is the hard one: locks may be
> held by threads that no longer exist, so it must *reset* rather than *unlock* and enter a conservative mode
> until safe.

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · Refill / flush, hand-over-hand (W6-3)

**Problem.** Refill (front-end empty → pull a batch up the hierarchy) and flush (front-end full → push a batch
down) are the only places two *middle-end* locks could be wanted at once. The lock hierarchy forbids holding
them together, so both must be written to hold **at most one** of {transfer, central} at a time, while still
conserving object count exactly.

**Design space.** **Hand-over-hand (release-before-acquire)** — chosen (§27.2): take the transfer-cache lock,
get/put the batch, *release it*, then take the central lock. The alternative (hold both to avoid a re-lookup)
reintroduces exactly the deadlock the hierarchy prevents.

**Structures.**
```text
refill(core,arena,sc): b = transfer_try_pop(...)            // hold transfer lock only
                       if b.empty { b = central_remove_batch(...) }  // hold central lock only (transfer released)
                       if b.empty { span = backend_new_span(...); central_attach(span); retry }  // §A.2
                       cpu_cache_insert_batch(core, sc, b)
flush(core,sc):        b = cpu_cache_make_space(core, sc)
                       if !transfer_try_push(b) { central_insert_batch(b) }   // triggers empty-detection
```

**Work breakdown (refines W6-3a..c).** 1. refill with the release-before-acquire discipline (W6-3a). 2. flush,
same discipline (W6-3b). 3. wire empty-span detection (plan 03 W5-3e) into flush-to-central + the debug
conservation check (W6-3c).

**Invariants.** object count conserved across refill and flush (plan 02 W1-6c); **≤ 1** middle-end lock held
at any instant; span creation happens *outside* the central lock (§A.2).

**Verify.** the lock-order checker (DD-3) proves the ≤1 property at runtime; plan 02 W1-6c proves
conservation; plan 08 W21-3 stresses concurrent refill/flush under TSan.

**Failure modes.** *F1* a "fast" refactor grabs both locks → deadlock under the hierarchy → the checker
(W16-1b) fails it in debug CI (G-conc). *F2* a flush empties a span but doesn't trigger detection → leak →
W6-3c wires the trigger.

**Sequencing.** **M2**.

### DD-2 · RSEQ restartable sequences (W7-2/W7-3) — the only assembly

**Problem.** Make per-CPU pop/push lock-free using a restartable critical section that the kernel aborts on
preemption/migration. This is the highest-risk code in the project: a single mis-ordered store or a faulting
reference inside the CS corrupts the cache.

**Design space.** **Linux RSEQ on POSIX; a pinned-thread per-core contract on seLe4n (§36.10)** behind one
front-end interface. Both implement the same `FeOutcome` contract and the same Lean axiom (plan 02 W1-7).

**Anatomy of a sequence (the non-negotiable shape, §12.3).**
```text
  start_ip:  load cpu id from the rseq area
             load head/index for (cpu, sc)
             compute next; bounds-check (→ Empty/Full without committing)
  commit_ip: SINGLE store that publishes the new head/index   ← the only state change
  post_commit_ip:
  abort_ip:  (kernel jumps here on preempt/migrate) → return Abort; caller retries
```
Rules: **no calls, no possibly-faulting memory reference** between `start_ip` and `commit_ip`; the abort
handler restores a logical no-op; the commit is one store, so an abort before it is invisible.

**Work breakdown (refines W7-2a..e / W7-3a..c).** 1. cs descriptors + abort trampolines in a dedicated section
+ rseq registration (W7-2a). 2. `rseq_pop` (W7-2b). 3. `rseq_push` (W7-2c). 4. clobber/barrier docs + a lint
that no call/fault occurs in a CS (W7-2d). 5. forced-migration equivalence vs the locked baseline (W7-2e).
6–8. the AArch64 mirror (W7-3a..c) — **co-primary**, since AArch64 is the seLe4n target.

**Invariants.** the only store that changes cache state is the commit; abort ⇒ no change (plan 02 W1-7 frame
condition); `Abort` is distinct from `Empty`/`Full`.

**Verify.** *two* ways, both required: (a) plan 02 W1-7 axiom + W1-12d `per_core_cache_abort_no_change`; (b)
a forced-migration differential test (W7-2e/W7-3c) that pins/repins a thread mid-sequence thousands of times
and asserts object movement identical to the locked baseline (W6-4). Plus the §34.5 battery (W7-6): signal
near the CS, preemption, registration failure.

**Failure modes.** *F1* a load inside the CS faults (e.g., touches an unmapped page) → kernel does *not*
restart a fault, it delivers the signal → keep all CS memory references to already-resident cache metadata.
*F2* compiler reorders across the commit → explicit fences + the no-call rule. *F3* `Abort` treated as
`Empty` → spurious slow-path or lost progress → the front-end contract keeps them distinct.

**Sequencing.** **M3**; pinned-core (W7-5) lands alongside for seLe4n.

### DD-3 · Lock hierarchy & the checker (W16-1)

**Problem.** A fixed global acquisition order is the only practical defense against deadlock across the
front→transfer→central→span→backend→stats layering; it must be cheap to follow and *enforced*, not just
documented.

**Design space.** **A typed/ranked lock wrapper + a debug per-thread held-rank stack** — chosen: ranks are
compile-time constants on the lock type; acquisition pushes the rank and asserts it exceeds the current top.

**Structures.**
```text
rank: config 0 < arena_registry 1 < arena 2 < transfer 3 < central 4 < span 5 < backend 6 < stats 7
RankedLock<const R: u8>;  acquire(): assert(thread.top_rank < R); thread.push(R)
```

**Work breakdown (refines W16-1a/b).** 1. the ranked wrapper; route *every* lock through it (W16-1a). 2. the
debug checker (per-thread rank stack) + wire as the G-conc gate (W16-1b).

**Invariants.** acquisitions are strictly rank-increasing; refill/flush hold ≤1 of ranks 3–6 (DD-1).

**Verify.** the checker fails any out-of-order acquire in debug CI; plan 08 W21-3 runs the full concurrency
suite under it.

**Failure modes.** *F1* a lock not routed through the wrapper escapes the checker → a lint/grep forbids raw
`Mutex` in `topo-core`. *F2* two locks of the same rank → ranks are unique per layer; same-rank needs a
documented tie-break (none currently).

**Sequencing.** **M2** (the global lock of M1 trivially satisfies it; the hierarchy replaces it here).

### DD-4 · TLS bootstrap (W16-2) — a top risk (R3)

**Problem.** The allocator's own thread-local state must be reachable on a thread's first allocation *without*
that access itself allocating — or the allocator re-enters itself before its per-thread state exists →
recursion/deadlock.

**Design space.** **Initial-exec TLS (or a static `__thread` slot reached without dynamic TLS allocation)** —
chosen (§27.6). When loaded via `dlopen`, where initial-exec may be unavailable, fall back to an
**allocation-free per-thread bootstrap path** until TLS is safe, consistent with the phased init (W16-7).

**Work breakdown (refines W16-2).** 1. declare per-thread state initial-exec. 2. a dlopen-safe bootstrap that
serves the first allocations from a thread-agnostic path until the TLS slot is established. 3. the recursion
test.

**Invariants.** first TLS access never calls `malloc`; the bootstrap path is allocation-free.

**Verify.** load the allocator via `dlopen` in a test and assert the first allocation does not re-enter (a
re-entrancy guard counts depth). This is the **threading analogue of S-007** (plan 03 DD-2).

**Failure modes.** *F1* general-dynamic TLS sneaks in via a dependency → a build assertion on the TLS model;
*F2* fast paths assume TLS exists → **W16-2 must be proven before W7 lands** (M2 before M3).

**Sequencing.** **M2**, ahead of the W7 fast paths.

### DD-5 · fork (W16-5)

**Problem.** In a multithreaded process, `fork()` yields a child with one thread but locks possibly held by
threads that no longer exist. The child must not deadlock on the allocator's own locks.

**Design space.** **atfork handlers in three contexts** — chosen (§28.1): pre-fork acquire+quiesce, parent
release+resume, child **reset** (not unlock) + disable background threads + conservative mode.

**Work breakdown (refines W16-5a/b).** 1. pre-fork (acquire the fork lock, quiesce background threads) +
parent-post-fork (release, resume) (W16-5a). 2. child-post-fork (reset all lock states, disable background
threads, flush/mark-conservative inconsistent per-CPU state) (W16-5b).

**Invariants.** after fork: no lock is held by a vanished thread; the child allocates correctly; background
threads stay off until safe.

**Verify.** a fork-in-multithread stress test (threads mid-allocation at fork) asserts the child allocates
without deadlock and stats are consistent.

**Failure modes.** *F1* child *unlocks* a lock held by a dead thread (UB) → it must **reset** state, not
unlock. *F2* a per-CPU cache is mid-RSEQ at fork → the child enters conservative (locked) mode until flushed.

**Sequencing.** **M2**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M1 | (global lock from W16-4; no caches yet — plan 06's M1 path goes straight to central.) |
| M2 | W6-1..W6-7 (caches), W16-1 (hierarchy + checker), **W16-2 (TLS)**, W16-3, W16-4 (global→fine), W16-5 (fork), W16-6, W16-7 (init phases). |
| M3 | W7-1, W7-2 (x86-64), W7-3 (AArch64), W7-4, W7-5 (pinned-core), W7-6. |

## Domain risks

- **R3** (TLS re-entrancy), **R5** (RSEQ correctness), **R6** (lock-order cycles) are all owned here — see the
  W16-2 and W7-2/3 decompositions and the lock-rank table. Each has a CI gate (G-conc, G-fast).

## Definition of Done (addendum)

Every concurrency WU runs under **TSan**; every lock acquisition goes through the ranked wrapper (W16-1a) so
the checker can see it; every fast-path change re-runs the forced-migration equivalence vs the locked
baseline (W7-6).

## Best-practices checklist

- [ ] Correct (locked) before fast (RSEQ); the RSEQ path proves behavioral equivalence, never just "passes."
- [ ] `Abort` is distinct from `Empty`/`Full` everywhere (front-end contract + Lean).
- [ ] Refill/flush are hand-over-hand; never hold two middle-end locks (checker-enforced).
- [ ] TLS is initial-exec / bootstrap-safe and proven before fast paths exist.
- [ ] AArch64 is co-primary with x86-64 (it is the seLe4n target), not a later port.
- [ ] The child fork handler resets (not unlocks) and goes conservative.

# Plan 06 — Public API, Reallocation & Arenas

**Workstreams:** W8 (public API/ABI), W15 (realloc/aligned/calloc), W9 (capability-backed arenas), W10
(extent hooks) · **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §10, §25, §26, §9.6/§9.7, §22, §36.4, §36.13, §23, §35.2/§35.3; F-001..F-010, §15.5.
**Upstream deps:** [03](03-core-allocator.md) (classify, pagemap, central), [04](04-backend-hugepages-release.md)
(provider, extents), [05](05-caches-concurrency-fastpath.md) (front-end). **Downstream:** every consumer;
[09](09-sele4n-integration.md) (arenas become capability resource domains). **Milestones:** API + realloc at
**M1**; arenas/authority/hooks at **M4**.

> This is the user-facing surface (correct C/C++/Rust semantics, errno, alignment, overflow) **and** the
> policy/authority domains (arenas). Per D2, an arena is *both* a jemalloc-style policy domain (§22) *and* a
> capability-controlled resource domain (§36.4) from M1 — trivial/ambient on POSIX, real on seLe4n.

## Public surface (owned here)

```text
C core:     malloc free calloc realloc                                   (§10.1)
aligned:    posix_memalign aligned_alloc memalign malloc_usable_size     (§10, F-003/F-008)
C23:        free_sized free_aligned_sized reallocarray                   (§10.1)
C++:        operator new/new[]/delete/delete[] (+ aligned/sized/nothrow) (§10.2)
extended:   topo_mallocx rallocx xallocx dallocx sdallocx nallocx + flags(§10.3/§10.4)
arena API:  topo_arena_create/configure/reset/destroy/delegate          (§22, §36.14)
Rust:       GlobalAlloc / allocator-api adapter                          (D1)
```

## Arena descriptor (owned here, consumed by all)

```rust
struct Arena {
  id: ArenaId, name: [u8;32], state: ArenaState,        // Initializing|Active|Draining|Resetting|Destroyed
  authority_cap: Cap, label: Label, quota: Quota,        // §36.4 — ambient/trivial on POSIX
  policy: ArenaPolicy, decay: DecayConfig, huge: HugePolicy, numa: NumaPolicy,
  hooks: ExtentHooks, stats: ArenaStats, locks: LockSet,
}
```

---

## W8 — Public API & ABI (C, C++, Rust)

**Depends on:** plan 03 (W2,W3,W5), plan 05 (W6). **Note:** the **M1 path allocates/frees via the central
list under the global lock; it does *not* depend on W6** — W6 is wired in as the fast path at M2 (plan 05
W16-4 owns the transition). **Enables:** every consumer + tests.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W8-1a | C core `malloc/free/calloc/realloc` over the central path (M1). | M | | ABI tests pass single-threaded. |
| W8-1b | errno semantics: `ENOMEM` on failure, `free` preserves `errno`, realloc-failure preserves the original (§10.1). | S | ∥ | errno tests. |
| W8-2 | Aligned/POSIX: `posix_memalign`, `aligned_alloc`, `memalign`, `malloc_usable_size` (F-003/F-008). | M | ∥ | alignment + usable-size tests; never silently ignore alignment (§10.4). |
| W8-3 | C23: `free_sized`, `free_aligned_sized`, `reallocarray` (overflow-checked); sized-delete-style hint use + sample-check mismatch. | S | ∥ | mismatched size sample-checked in hardened/debug (plan 08 W18-2). |
| W8-4 | Zero-size policy (§9.6/F-004): `compat.zero_unique`/`zero_null`, `free(NULL)` no-op; documented + configurable. | S | ∥ | behavior consistent + configurable. |
| W8-5 | C++ operators (§10.2): new/new[]/aligned/sized/nothrow overloads; map errors to `bad_alloc` per API. | M | | C++ link test; sized delete used when valid. |
| W8-6 | Extended C API (§10.3/§10.4): `topo_mallocx/rallocx/xallocx/dallocx/sdallocx/nallocx` + flags; validate flag combos. | M | ∥ | invalid combos fail deterministically; mandatory flags honored. |
| W8-7 | Rust `GlobalAlloc`/allocator-api adapter (D1). | S | ∥ | a Rust program uses TopoMalloc as `#[global_allocator]`. |
| W8-8 | Generated `include/topomalloc.h` + header/ABI compatibility test. | S | ∥ | header compiles under C and C++; ABI test pins struct/opaque-handle layout (§35.3). |

---

## W15 — Reallocation, aligned allocation & calloc zeroing

**Depends on:** plan 03 W2 (size classify) + W3 (pointer classify), plan 04 W4 (extent grow/shrink), W8.
**Enables:** M1 (basic), M5 (in-place via extents).

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W15-1 | realloc semantics (§25.1): `realloc(NULL,n)`, `realloc(p,0)` policy, content preservation, failure keeps the original. | M | | property test: content preserved; failure safety. |
| W15-2 | Move realloc (§25.4): allocate-before-free, copy `min(old_usable,new)`, alignment/arena preserved, profiled as realloc when sampled. | M | | old object preserved on OOM. |
| W15-3a | In-place **grow** (§25.2): same-size-class fast path now; extent-merge growth for medium/large at M5. | M | ∥ | same-class grow is in-place; large via plan 04 extents. |
| W15-3b | In-place **shrink** (§25.3): same-class; large tail-page split/return; no unusable tiny fragments unless policy permits. | M | ∥ | shrink returns tail safely. |
| W15-4 | Aligned-allocation validation (§25.5) + over-aligned routing (§9.3, plan 03 W2-3b). | S | ∥ | power-of-two/min checks; over-aligned never offset-adjusts a shared slab. |
| W15-5 | calloc zeroing (§26.2/§26.3) + overflow+rounding guard (§26.1, with plan 03 W2-4). | M | | zeroed result; zero-state metadata invalidated on reuse; overflow safe. |

> **▸ Decomposition — W15 (realloc).** Split *semantics* (W15-1, the contract incl. failure-preserves-original),
> *move* (W15-2, allocate-before-free so OOM cannot lose the original), and *in-place grow/shrink* (W15-3a/b,
> the optimization that avoids copies). The order matters: ship the always-correct move path (W15-2) first,
> then add in-place as a *fast path* under it — never the reverse, or a failed in-place attempt could corrupt
> the original. calloc's overflow guard (W15-5) must also catch *rounding* overflow after the multiply
> (§26.1/§9.7), not just `n*size`.

---

## W9 — Arena policy & authority domains (capability-backed)

**Depends on:** plan 04 (provider), plan 03 W5 (central per arena), plan 05 W6 (cache routing). **Enables:**
M4, W10, plan 09.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W9-1 | Arena descriptor (above) with `authority_cap`, `label`, `quota` (§36.4); trivial ambient values on POSIX. | M | | POSIX default arena works; fields present for seLe4n. |
| W9-2 | Arena states + lifecycle (§22.3); allocations only in `Active`. | M | | illegal-state ops rejected. |
| W9-3 | Create/configure (§22.4, F-005/F-006): validate policy; metadata from a safe arena; hooks installed before the first extent; publish id only after init. | M | | creation order enforced; default-arena policy (F-006) covers no-extended-API programs. |
| W9-4a | State transitions Active→Resetting/Draining + precondition checks (no active allocators; explicit, not the default arena unless special mode) (§22.5). | M | | illegal reset rejected. |
| W9-4b | Cache drain/invalidate of *every* per-CPU/thread/transfer cache holding the arena's objects (uses W6 routing). | M | | post-drain: no cache holds an arena object (B.5). |
| W9-4c | Return arena extents to backend/retain per policy; reset accounting; bump reset generation. | M | ∥ | §22.5 postconditions met. |
| W9-4d | Destroy = reset + metadata removal + id non-reuse-while-stale (§22.6); isolation preserved (§22.7); mirrors plan 02 W1-9. | M | | `arena_destroy` tests; isolation invariant. |
| W9-5 | **Capability monotonicity** (§36.4): authority/quota/label monotonic on delegation; attenuation-only. | M | | delegation cannot widen rights/quota or downgrade label (§36.16). |
| W9-6a | Revocation: enter DRAINING — reject new allocations + delegations; notify clients (§36.13). | M | | no new alloc/delegation while draining. |
| W9-6b | Revocation: drain local/transfer caches + central lists; quarantine or reject stale frees. | M | | post-drain inventory empty (shares W9-4b). |
| W9-6c | Revocation: unmap client VSpace windows; scrub dirty pages if cross-label reuse is possible (uses plan 08 W18-6). | M | ∥ | unmapped before revoke; scrub recorded. |
| W9-6d | Revocation: revoke derived frame/mapping caps; delete CSlots; recycle untyped (provider `revoke_descendants`/`recycle`). | M | | no live derived cap/mapping remains. |
| W9-6e | Revocation: finalize DESTROYED + generation++; **partial failure ⇒ DRAINING/ERROR_QUARANTINED, never DESTROYED**; emergency allocs never depend on a destroying arena. | M | | revocation test (§36.16); mirrors `destroy_revokes_descendants` (plan 02 W1-12c). |
| W9-7 | NUMA policy modes (§15.5) + binding-failure visibility in stats. | M | ∥ | local/interleave/bind/arena_policy/OS_default; failures surfaced. |

> **▸ Decomposition — W9-6 (arena revocation), the seLe4n-critical lifecycle.** Ordering is the whole game:
> **unmap before revoke before recycle**, because recycling untyped backing while a client mapping or derived
> capability still exists would hand live authority to another security domain. Each step is its own unit so a
> partial failure stops cleanly — the arena lands in DRAINING/ERROR_QUARANTINED, never DESTROYED, never with a
> half-revoked CSpace. The scrub (W9-6c → plan 08 W18-6) makes cross-label reuse safe (§36.12), skippable only
> when the reused-at label is ≥ the old label. **Pitfall:** draining *all* caches (W9-6b, shared with W9-4b)
> is the same hard search as empty-span detection — an object can hide in any cache; bound-arena routing (D6)
> + arena-qualified slots (M4) make it tractable. Mirrors Lean `destroy_revokes_descendants`; on the G-arena
> gate. On POSIX, W9-6c/d collapse to unmap + no-op revoke; the *structure* is identical so seLe4n is a drop-in.

---

## W10 — Extent hooks & custom backing

**Depends on:** plan 04 W4, W9. **Enables:** M4.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W10-1 | Hook interface (§23.2) wired through the provider seam (plan 04). | M | | alloc/dealloc/commit/decommit/purge/split/merge dispatch to user hooks. |
| W10-2 | Hook contracts (§23.3) enforced/validated (alignment, size, no-overlap, subrange-only, no undocumented reentrancy). | M | | violations detected in debug; assumptions documented in Lean (§23.4). |
| W10-3 | Failure-injection tests (§34.8): every hook can fail; the allocator stays well-formed. | M | ∥ | fuzzed hook failures never corrupt state. |

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · realloc, as a state machine (W15)

**Problem.** `realloc` has four standard obligations that interact badly if implemented ad hoc: `realloc(NULL,
n)==malloc(n)`; `realloc(p,0)` follows a configured policy; on success the new object holds
`min(old,new)` bytes of old content; **on failure the old allocation stays valid** (§25.1). The last is the
trap — a clever in-place attempt that fails after mutating the original loses data.

**Design space.** **Decide the path from `(classify_ptr(p), old_class, new_class)`, always allocate-before-free
on the move path, and treat in-place as a fast path layered *under* move** — chosen. Never the reverse.

**State machine.**
```text
realloc(p, n):
  p == NULL            -> malloc(n)
  n == 0               -> policy(free(p); return zero_unique|NULL)
  same size class      -> in-place, return p                                   (W15-3a)
  shrink, large tail   -> split + return tail to backend, return p             (W15-3b)
  grow, adjacent free  -> try extent-merge grow (plan 04 W4-2c); else MOVE     (W15-3a)
  otherwise            -> MOVE: q=malloc(n); copy min(old_usable,n); free(p)   (W15-2)  ← q first, free p last
```

**Work breakdown (refines W15-1..5).** 1. the contract + dispatch (W15-1). 2. the **move** path, q-before-free
(W15-2) — *ship this first; it is always correct*. 3. in-place grow (W15-3a) + shrink (W15-3b) as fast paths
*under* move. 4. aligned validation + over-aligned routing (W15-4). 5. calloc zeroing + overflow (W15-5, see
DD-2).

**Invariants.** failure preserves the original object and its contents; alignment + arena preserved across a
move; content `min(old_usable, new)` preserved.

**Verify.** a property test that interleaves `realloc` with content checks (write a pattern, realloc, verify
the prefix survives) and injects allocation failure to assert the original survives.

**Failure modes.** *F1* in-place grow mutates then fails → **never**: in-place only commits when it cannot
fail; otherwise fall to move. *F2* copying `new` bytes when `new>old_usable` reads OOB → copy `min`.

**Sequencing.** **M1** for move + same-class in-place; extent-merge grow lands with plan 04 **M5**.

### DD-2 · calloc overflow & zeroing (W15-5, with plan 03 W2-4)

**Problem.** `calloc(n,size)` must reject `n*size` overflow *and* any subsequent size-class/page/hugepage
**rounding** overflow (§26.1/§9.7), then return genuinely-zeroed memory cheaply.

**Design space.** check `n != 0 && size > SIZE_MAX/n` → then route the rounded size through the same
overflow-checked rounding as malloc (plan 03 W2-4); zero via OS zero-pages when the span is freshly committed,
else `memset`, tracking a `zeroed` flag invalidated on user write.

**Invariants.** no integer wraps to a small allocation; returned bytes are zero; the `zeroed` flag is
invalidated when user-visible memory may be non-zero.

**Verify.** overflow tests across the boundary (`n*size` near `SIZE_MAX`, and a product that passes but rounds
past `SIZE_MAX`); a zeroing test on both the fresh-commit and reused-span paths.

**Failure modes.** *F1* checking only `n*size`, not rounding → a product that fits but rounds over wraps → DD
routes through W2-4. *F2* trusting a stale `zeroed` flag → invalidate on any user write to the span.

**Sequencing.** **M1**.

### DD-3 · Arena revocation protocol (W9-6) — the seLe4n-critical lifecycle

**Problem.** Destroying an arena in the seLe4n profile is far more serious than a POSIX `free`: backing is
revocable capabilities + mapped frames, and recycling backing while a client mapping or derived capability
still exists would hand **live authority to another security domain** (§36.13). Ordering is the whole game.

**Design space.** **A strictly ordered, step-isolated protocol with a non-DESTROYED failure state** — chosen:
each step is its own unit so a partial failure stops cleanly in `DRAINING`/`ERROR_QUARANTINED`, never
`DESTROYED`, never with a half-revoked CSpace.

**The protocol (ordered; **unmap before revoke before recycle**).**
```text
1 DRAINING: reject new allocations + delegations; notify clients          (W9-6a)
2 drain local/transfer caches + central lists; quarantine stale frees     (W9-6b)  ← same hard search as DD-3 of plan 03
3 unmap client VSpace windows                                             (W9-6c)
4 scrub dirty pages IF cross-label reuse is possible                      (W9-6c → plan 08 W18-6)
5 revoke derived frame/mapping caps; delete CSlots                        (W9-6d)
6 recycle untyped to free pools                                           (W9-6d, provider.recycle)
7 DESTROYED + generation++                                                (W9-6e)
on partial failure at any step → DRAINING | ERROR_QUARANTINED  (never DESTROYED)
```

**Invariants.** no live derived cap/mapping after step 6; a stale pointer never becomes valid for a new arena
(generation guard); emergency allocations never depend on a draining arena.

**Verify.** the revocation test (§36.16): after destroy, assert zero live frame caps, zero client mappings,
zero cache objects for the arena; mirror to Lean `destroy_revokes_descendants` (plan 02 W1-12c). On POSIX,
steps 3–6 collapse to unmap + no-op revoke, so the *structure* is identical and seLe4n drops in.

**Failure modes.** *F1* recycle before revoke → live authority leaks across domains → the fixed order. *F2*
an object hidden in a per-CPU cache survives step 2 → bound-arena routing (D6) + arena-qualified slots (M4)
make the drain exhaustive (the same technique as empty-span detection). *F3* destroy marked complete after a
failed revoke → the non-DESTROYED failure state.

**Sequencing.** **M4** (against the Sim); real-kernel revocation at plan 09 **M8**.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M1 | W8-1..W8-4, W8-7/8, W15-1, W15-2, W15-3a (same-class), W15-4, W15-5; a single default capability-backed arena (W9-1, ambient). |
| M2 | W8-5/6 (C++/extended) as caches arrive; arena-qualified routing prep (W6-6). |
| M4 | W9-2..W9-7 (lifecycle, delegation, revocation), W10 (hooks); W15-3 extent-merge growth lands with plan 04 M5. |

## Domain risks

- **R7** (capability/CSlot leaks on destroy/revoke) — owned here via the W9-6 protocol + generation counters
  + the `destroy_revokes_descendants` theorem. **R12** (scope) — ship the always-correct move-realloc before
  the in-place fast path.

## Definition of Done (addendum)

Every API WU has an ABI test; every realloc WU has a content-preservation + failure-safety property test;
every arena lifecycle WU runs the isolation (§22.7) and B.5 checks; the M1 path is proven correct **before**
the M2 fast path is wired under it.

## Best-practices checklist

- [ ] Alignment is never silently ignored (§10.4).
- [ ] calloc checks multiply **and** rounding overflow (§26.1/§9.7).
- [ ] Move-realloc allocates before freeing; in-place is a fast path *under* it, never instead of it.
- [ ] Arenas carry cap/label/quota from M1; POSIX values are trivial so seLe4n drops in.
- [ ] Revocation is unmap→revoke→recycle, step-isolated, partial-failure-safe.

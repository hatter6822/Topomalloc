# TopoMalloc SPEC.md — Audit and Refinement Report

**Audited document:** `planning/SPEC.md`
**Spec revision before audit:** 0.2 (2026-06-01)
**Spec revision after audit:** 0.3 (2026-06-03)
**Scope of audit:** completeness, internal consistency, mathematical correctness, security
best-practices, and a directed requirement change (the seLe4n/seL4 integration profile is not
optional).

This report lists every finding, its severity, and how it was resolved in revision 0.3. The
repository contains no implementation yet, so "the codebase" under audit is the pseudocode, data
structures, mathematical claims, and Lean fragments described in the specification.

---

## 1. Directed requirement change

### D-1 — seLe4n/seL4 integration profile is not optional *(requirement, applied)*
The profile was described as "optional" in the scope line, the executive summary (item 5), the
source-basis notes, and the Section 36 conclusion.

**Resolution.** Removed every "optional" qualifier on the profile. Added a fifth conformance
class (**Microkernel-integration conformance**) under "Normative language," a normative preamble
to Section 36, and strengthened Goal 11 to a `MUST`. Reframed the Microkit source-basis note so
that *dynamic retype and arena growth* degrade to fixed-arena mode — the profile itself is
required. The user-level/not-in-kernel boundary (3.2, 36.2) is explicitly preserved: requiring
the profile does not permit an in-kernel heap.

---

## 2. Mathematical correctness

### M-1 — Rounding-waste table was unachievable at small sizes *(high, fixed)*
§9.4 stated a flat `<= 33%` worst-case waste target for "17 to 128 bytes." Under a 16-byte ABI
alignment quantum (`alignof(max_align_t)` on LP64), a 17-byte request must round up to 32 bytes
(~88% waste), and no 16-aligned class exists in `(16, 32)`. The target is provably impossible
below `req = q/W` (≈48 B for `q=16`, `W=33%`).

**Resolution.** Reworked §9.4 into two regimes: a *spacing-dominated* regime with the exact
identity `worst-case waste ≈ r(c) − 1` where `r(c)` is the ratio of consecutive class sizes, and
an *alignment-dominated* regime where waste is bounded by the quantum `q`, not by `r`. The table
now expresses achievable per-class spacing-ratio targets with the alignment caveat called out.

### M-2 — Missing `size` ≡ multiple-of-`alignment` invariant *(high, fixed)*
§16.3 lays out objects at `base0 + i·c.size` with `base0` aligned to `c.alignment`. Every object
is then aligned only if `c.size` is an integer multiple of `c.alignment`. §9.3 required only that
`c.size` be a multiple of "the minimum ABI alignment" and additionally allowed "adjusting the
object start" — which breaks the uniform layout, the `objects_per_slab` count, and the
disjointness proof.

**Resolution.** §9.3 now requires `c.size` to be an integer multiple of `c.alignment`, states the
"iff" linking it to §16.3/§9.5, and forbids per-object offset adjustment; over-aligned requests
route to a dedicated aligned class or the medium/large path (§25.5). The §9.4 generator must
assert this per class.

### M-3 — Span object-count conservation was inconsistent *(medium, fixed)*
Three different decompositions of `object_count` appeared: the `Span` descriptor (§16.2) stored
`local_free_count` + `central_free_count`; §16.4 used `cached_count`; §16.5 used
`transfer_free_count`. Several referenced fields were undefined.

**Resolution.** Adopted one canonical five-term partition
(`live + local_cached + transfer_cached + central_free + quarantined = object_count`), documented
which terms are cheaply maintained (`central_free = popcount(free_bitmap)`) versus reconstructed
in debug, and aligned the descriptor, §16.4, and §16.5 to it.

### M-4 — RSEQ Lean contract missing a frame condition *(medium, fixed)*
The §33.5 `rseq_pop` axiom asserted ownership change for the popped pointer but not that *no other
object changes owner*. The cache-conservation theorems in §33.4 cannot be proved without that
frame condition. The axiom also folded genuine cache-empty (underflow) into "abort," erasing
progress.

**Resolution.** Rewrote the axiom in Lean 4 syntax with a three-way result
(`abort | empty | success`), an explicit frame condition `∀ q, q ≠ p → OwnerOf s' q = OwnerOf s q`,
and a symmetric note for `rseq_push` (`abort | full | success`).

---

## 3. Robustness / concurrency

### R-1 — Small-alloc slow path had no OOM handling and an empty-batch race *(high, fixed)*
§A.2 dereferenced `batch.head`/`batch.tail` unconditionally. If `backend_new_span` returned null
(OOM), or another CPU drained the freshly attached span before the re-read, the batch could be
empty.

**Resolution.** Wrapped the body in a retry loop, added explicit `span == null → return null`
(propagating to `errno = ENOMEM` / `std::bad_alloc`), and `continue` after attach so the batch is
re-derived.

### R-2 — Lock hierarchy inverted relative to data flow *(medium, fixed)*
§27.2 ordered the per-NUMA central lock *above* the per-LLC transfer lock, the reverse of the
front→transfer→central layering used by refill/flush.

**Resolution.** Reordered to transfer-before-central, added a release-before-acquire (hand-over-
hand) recommendation, and noted the refill/flush paths hold at most one middle-end lock. The order
remains a documented total order with a debug-mode checker.

### R-3 — Per-CPU cache vs explicit-arena routing was unspecified *(medium, fixed)*
A `(cpu, sc)` slot can receive frees routing to different arenas (arena recovered via pagemap),
but a batch is single-arena and empty spans reclaim only to their owning arena. The refill/flush
pseudocode elided the arena dimension, leaving free-routing under-specified.

**Resolution.** Added §11.7 giving two conforming designs (bound-arena fast path; arena-partitioned
flush), the fully-qualified `[domain][arena][sc]` / `[node][arena][sc]` indexing, and a statement
that correct free-routing is a safety property derived from ownership uniqueness (§8.2).

---

## 4. Security / best practices

### S-1 — Allocator TLS recursion hazard was unaddressed *(high, fixed)*
The spec required bootstrap-safe metadata allocation (S-007) but not a non-allocating TLS model.
General-dynamic TLS can lazily allocate on first access and re-enter `malloc` before per-thread
state exists.

**Resolution.** §27.6 now requires an initial-exec (or static `__thread`) TLS model and an
allocation-free per-thread bootstrap fallback for `dlopen` deployments, framed as the threading
analogue of S-007.

### S-2 — `errno`/failure ABI under-specified *(low, fixed)*
The C API section did not require `errno = ENOMEM` on failure or `free` to preserve `errno`.

**Resolution.** §10.1 now mandates `errno = ENOMEM` on C allocation failure, `realloc` validity
preservation, and POSIX `free` `errno`-preservation.

### S-3 — calloc overflow guard incomplete *(low, fixed)*
The `n·size` guard is correct but does not cover the subsequent size-class/page/hugepage rounding
overflow (§9.7).

**Resolution.** §26.1 now cross-references §9.7 and requires calloc to fail on rounding overflow.

---

## 5. Completeness / consistency

| ID | Finding | Severity | Resolution |
|---|---|---|---|
| C-1 | Executive summary said "four ideas" but listed five. | low | Corrected to "five." |
| C-2 | Table of contents collapsed appendices into one item; body has §38–§45. | low | Expanded the ToC to list Appendices A–G and References. |
| C-3 | §8.6 accounting identity ignored fully-unmapped `Released` memory. | low | Added the unmap caveat and required the convention to be stated. |
| C-4 | C23 `free_sized`/`free_aligned_sized` and `reallocarray` not mentioned. | low | Added as optional APIs in §10.1. |
| C-5 | §6.2 expository fast-path pseudocode could read as a TOCTOU. | low | Added a note that the test-and-update is one restartable transaction. |
| C-6 | No changelog. | low | Added a "Document history" table and bumped the revision to 0.3. |

---

## 6. Items reviewed and found correct (no change)

These were checked and are correct as written; recorded so the audit is auditable.

* `calloc` multiplication guard `n != 0 && size > SIZE_MAX / n` — canonical and correct.
* Move-`realloc` copy bound `min(old_usable_size, new_size)` — safe (never reads past the old
  object nor writes past the new).
* Hugepage `coverage_ratio = live_on_intact / max(live_total, 1)` — well-defined.
* Per-CPU hard-capacity invariant `Σ count·size ≤ hard_capacity_bytes` — sound.
* Memory-ordering defaults (release publish / acquire consume; relaxed for approximate counters)
  and the fork atfork protocol — consistent with best practice.
* Ownership uniqueness, live-disjointness, and the "policy mistake must not become a safety
  mistake" separation — coherent and preserved throughout.

---

## 7. Suggested follow-ups (not applied; tracked for a future revision)

* Generate the size-class table from the Lean model and check both the §9.4 spacing-ratio bounds
  and the `size ≡ 0 (mod alignment)` invariant in CI (already required by the text; needs the
  generator).
* Consider whether safe-linking-style freelist-pointer encoding is cheap enough to enable in the
  default profile rather than only in hardened mode (currently a deliberate, documented trade-off).
* Reconcile the `LivePending` proof state (§12.2) with `Owner.live` (§33.5) by an explicit
  collapse at the linearization point.

# Plan 10 — Deployment & ABI

**Workstreams:** W23 · **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** §35 (whole), §36.19 S9, §34.6, Appendix F (deployment anti-patterns).
**Upstream deps:** [06](06-api-realloc-arenas.md) (the ABI), [05](05-caches-concurrency-fastpath.md) (init/TLS),
[07](07-observability-placement-control.md) (stats), [09](09-sele4n-integration.md) (seLe4n perf baseline).
**Milestones:** GA at **M9**; deployment modes are usable incrementally from M1.

> Ship it: make TopoMalloc deployable as the process allocator (static, dynamic, or interposed), safe against
> mixed-allocator misuse, ABI-stable across a release series, and measured against credible baselines. The
> SPEC flags LD_PRELOAD-style deployment as subtle (§35.1), so it is documented as such, not hidden.

---

## W23 — Deployment, ABI compatibility & release engineering

**Depends on:** plan 06 W8, plan 05 W16, plan 07 W17. **Enables:** M9 / GA.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W23-1a | Deployment: static link + dynamic-as-process-allocator (§35.1). | M | | both modes documented + smoke-tested. |
| W23-1b | Deployment: LD_PRELOAD-style interposition with **documented caveats** (early init, interposition order, mixed allocators) + runtime-integration mode. | M | ∥ | interposition smoke test; caveats in `docs/deployment`. |
| W23-2 | Mixed-allocator detection (§35.2): TopoMalloc-frees-only-TopoMalloc; foreign-pointer detection in hardened/debug (uses plan 03 W3-4b). | M | ∥ | a foreign free fails safely in hardened, not silently corrupts. |
| W23-3 | ABI stability (§35.3): stable C names/struct ABI, opaque handles, additive stats fields; an ABI test pins the surface across a release series. | M | | ABI test in CI (G-abi); breaking changes caught. |
| W23-4 | Packaging + docs site (mdbook): deployment, profiles, tuning, ABI, seLe4n integration guide. | M | ∥ | `xtask docs` builds the site; published artifact. |
| W23-5a | POSIX perf vs jemalloc/TCMalloc: micro-benchmarks + workload replays (§34.6); record in `/bench`. | M | ∥ | reproducible; targets met or documented as open. |
| W23-5b | seLe4n perf (§36.19 S9): RPi5/QEMU vs static pools + an allocman-like baseline. | L | ∥ | reproducible; recorded in `/bench`. |

> **▸ Decomposition — W23-1 (deployment modes).** Split the *low-risk* modes (W23-1a: static + dynamic as the
> process allocator, where init order is controllable) from the *high-risk* interposition mode (W23-1b:
> LD_PRELOAD, where early init, interposition order, and mixed allocators bite — §35.1). The SPEC requires
> this be documented as tricky; W23-1b's deliverable is as much the *caveats document* as the code, and it
> leans on plan 05's phased init (W16-7) and TLS bootstrap (W16-2) to survive being the very first allocator
> loaded.

> **▸ Decomposition — W23-5 (perf validation).** Two independent baselines, hence two sub-WUs: W23-5a is the
> POSIX story (vs jemalloc/TCMalloc — the allocators this design draws from, R1–R7 of the SPEC) and W23-5b is
> the seLe4n story (vs static pools and an allocman-like baseline — the comparison that matters for the
> microkernel target, §36.19 S9). Both record into `/bench` with a committed results schema so runs are
> comparable over time; "targets met or documented as open" keeps perf honest without blocking GA on a number.

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · Interposition / LD_PRELOAD hazards (W23-1b)

**Problem.** Becoming the process allocator via interposition means TopoMalloc may run *before* its own init
completes, may be called from inside the dynamic loader, and may see pointers from another allocator that ran
earlier (§35.1). Each is a documented foot-gun, not a turnkey mode.

**Design space.** **Lean on phased init (plan 05 W16-7) + the TLS bootstrap (plan 05 W16-2) so the allocator
survives being first; document the caveats as a first-class deliverable** — chosen. The SPEC requires
LD_PRELOAD be presented *as tricky*.

**Work breakdown (refines W23-1a/b).** 1. low-risk modes (static link, dynamic-as-process-allocator) where
init order is controllable (W23-1a). 2. interposition (W23-1b): the symbol-export set, early-init survival,
and the **caveats document** (init order, interposition order, mixed allocators) — the doc is as much the
deliverable as the code. 3. mixed-allocator safety (W23-2): foreign-pointer detection in hardened (plan 03
W3-4b) so a pointer from another allocator fails safely, not silently corrupts.

**Invariants.** the allocator answers a `malloc` issued during its own Phase 0–2 (from the bootstrap path,
plan 03 DD-2); a foreign `free` in hardened mode is detected, not acted on.

**Verify.** an interposition smoke test (LD_PRELOAD a small program); a mixed-allocator test frees a
foreign pointer under hardened and asserts safe failure.

**Failure modes.** *F1* a `malloc` arrives before init → the phased-init bootstrap path serves it. *F2* a
loader-internal allocation re-enters → reentrancy guard (plan 05 W16-6). *F3* freeing foreign memory → hardened
foreign-pointer detection (W23-2).

**Sequencing.** **M9** (low-risk static/dynamic usable from M1 for testing).

### DD-2 · ABI stability test (W23-3)

**Problem.** Within a release series the public C names, struct ABI, opaque-handle layout, and stats-JSON
fields must not break downstream consumers (§35.3) — and "must not break" needs to be a *mechanical* check,
not a review habit.

**Design space.** **A committed ABI snapshot + a CI diff** — chosen: capture exported symbols, public struct
layouts, and the stats-JSON field set into a checked-in baseline; CI fails on a non-additive change.

**Work breakdown (refines W23-3).** 1. snapshot exported symbols + `#[repr(C)]` layouts + opaque-handle sizes.
2. snapshot the stats-JSON field set. 3. CI diff: removed/renamed symbols or fields, or changed layouts, fail
(G-abi); *added* stats fields pass (additive rule).

**Invariants.** no public symbol/struct/handle changes incompatibly within a series; stats-JSON fields are
additive-only.

**Verify.** G-abi at M9 and on every release branch; a deliberate breaking change in a test asserts the gate
fires.

**Failure modes.** *F1* an accidental struct reorder → layout snapshot diff fails. *F2* a removed stats field
breaks a dashboard → field-set diff fails (only additions pass).

**Sequencing.** **M9** (release-gating); the snapshot tooling can land earlier to track drift.

---

## Sequencing & milestone mapping

| Milestone | Deliverables |
|---|---|
| M1→ | W23-1a usable early (static/dynamic) for testing; W23-5a micro-benchmarks track regressions continuously. |
| M9 | W23-1b (interposition), W23-2 (mixed-allocator safety), W23-3 (ABI freeze + G-abi), W23-4 (docs site), W23-5a/b (perf vs baselines). |

## Domain risks

- *Local:* interposition surprises (init ordering, mixed allocators). *Mitigation:* document caveats (W23-1b);
  foreign-pointer detection in hardened (W23-2); rely on phased init (plan 05 W16-7).
- **R12** (scope) — perf validation reports numbers but does not *block* GA on a target; "documented as open"
  is an acceptable exit.

## Definition of Done (addendum)

Every deployment mode is smoke-tested in CI; the ABI test (W23-3) is a release-gating check; the docs site
builds via `xtask docs`; perf runs are reproducible from a committed schema.

## Best-practices checklist

- [ ] TopoMalloc frees only TopoMalloc memory; foreign pointers fail safely in hardened (§35.2).
- [ ] LD_PRELOAD is documented as tricky, not presented as turnkey (§35.1).
- [ ] ABI is stable within a release series; stats-JSON fields are additive (§35.3).
- [ ] Perf is measured against credible baselines (jemalloc/TCMalloc on POSIX; static pools/allocman on
      seLe4n) and recorded reproducibly.
- [ ] At exit, the allocator leaks-by-default (OS reclaims) unless teardown is explicitly requested (§35.5).

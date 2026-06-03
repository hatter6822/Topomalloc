<!-- SPDX-License-Identifier: MIT -->
## What & why

<!-- What does this change do, and which WU / milestone / SPEC section does it
implement? Link the plan item (e.g. "plan 03 W2-3a") and SPEC anchors. -->

## Definition of Done

<!-- From planning/plans/README.md §8. Tick every box or explain the exception. -->

- [ ] Builds clean on x86-64 **and** AArch64 (debug + performance); no new lint warnings (`cargo xtask ci`).
- [ ] Unit tests added/updated; **property tests** updated if API behavior changed.
- [ ] Debug **invariant checks** (Appendix B) for touched state pass; new state added its checker.
- [ ] Transition added/changed ⇒ **Lean model + named §33.4/§36.17 obligation** updated and proved, or a tracked `V-004` debt filed (ID: ____).
- [ ] New state ⇒ **stats** expose it and it reconciles (§8.6); **trace grammar** updated.
- [ ] New knob/behavior ⇒ **control plane** + docs updated; default matches §32.3.
- [ ] The **seLe4n vertical slice** still passes over `Sele4nSim` (G-sim) for milestones ≥ M1.
- [ ] SPDX header on every new file; `/sele4n`-integration files are GPL-3.0-or-later (D5).
- [ ] Generated tables not hand-edited — golden updated and `cargo xtask gen` run (G-table).
- [ ] Transitions tagged with their SPEC state-machine name (M-001).

## Appendix-F anti-pattern review

<!-- Confirm this change introduces NONE of these (planning/SPEC.md Appendix F). -->

- [ ] No hidden global lock on the hot path.
- [ ] No unbounded per-thread cache.
- [ ] No eager partial-hugepage release without memory pressure.
- [ ] No hand-maintained table without generated checks.
- [ ] No critical metadata stored only in user-writable memory.
- [ ] No `malloc` from allocator error logging / profiling callbacks (no recursive allocation).
- [ ] RSS is not treated as the only memory metric.
- [ ] No arena reset while local caches still hold arena objects.
- [ ] No silently-ignored alignment request.
- [ ] No unsynchronized pagemap update.
- [ ] No span-descriptor reuse without generation protection.
- [ ] No mixing memory from different allocators without clear ownership.
- [ ] seLe4n capabilities are treated as allocation **authority**, not mere metadata.
- [ ] No ordinary heap memory for seLe4n request-path metadata.
- [ ] No cross-domain reuse without scrub + information-flow check.
- [ ] seLe4n revocation is not conflated with POSIX page release.

## Security review

- [ ] Not applicable, **or** `security-review` was run because this touches
      `/sele4n`, `/arch`, freelist encoding, or metadata protection (close of M4/M7/M8).

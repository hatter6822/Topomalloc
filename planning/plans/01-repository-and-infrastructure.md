# Plan 01 — Repository & Infrastructure

**Workstreams:** W0 · **Status:** rev 2.1 · **Overview:** [README.md](README.md)
**SPEC anchors:** Appendix F (anti-patterns → CI checks), §34 (test categories to scaffold), §35
(deployment/ABI to anticipate), §30.1 (profiles).
**Upstream deps:** none (this is the first work). **Downstream:** *every* other plan depends on M0 here.
**Milestones:** owns **M0**; provides the substrate all later milestones build on.

> This plan is the *first work*. It produces a greenfield repo where any contributor (or web/CI agent) can
> clone and get a green build, tests, lints, and a Lean check on x86-64 **and** AArch64, with the dual-backend
> layout already in place. Nothing else can start until the seams and toolchain here exist.

> **Implementation status — W0 landed; M0 closed.** All of W0-1 … W0-14 are
> implemented and **verified end to end on both arches**: the Cargo workspace +
> `cargo xtask` (D3), pinned toolchains, CI (`.github/workflows/ci.yml`, every job
> calling `xtask`), the lint/SPDX/Lean-style/license-boundary/`cargo-deny` gates,
> the unit/property/differential/fuzz/bench harnesses, the C-ABI compile-link-run
> test, the SessionStart hook + `scripts/setup_lean.sh`, governance and docs, the
> MIT/GPL license split (D5/D8), versioning + ABI policy, and the M0 walking
> skeleton. `cargo xtask ci` is green on a fresh clone. The decisions are ratified
> in [`../../docs/DECISIONS.md`](../../docs/DECISIONS.md). The Lean toolchain is
> pinned to `leanprover/lean4:v4.28.0` (matching seLe4n) and `lake build` +
> `lake exe check` pass; AArch64 builds and runs the full suite under QEMU. The
> five M0 walking-skeleton sub-units (W0-14a..e) all pass — see
> `tests/tests/walking_skeleton.rs` and `tests/tests/dual_backend.rs`.

---

## W0 — Repository, build & developer infrastructure

**Goal:** a self-provisioning repo + the M0 walking skeleton that proves the whole Rust+asm+Lean+dual-backend
toolchain end to end. **Enables:** all.

| WU | Description | Size | ∥ | Acceptance |
|---|---|---|---|---|
| W0-1 | Ratify D3–D8; record outcomes in the overview §4.2. | S | | no `TBD` for D5 before any `/sele4n` file; D8 closed before plan 09 first compiles. |
| W0-2 | Repo layout (below) with a one-paragraph charter `README` per top dir. | S | ∥ | `tree -L 2` matches §“Repository layout”; each dir has a charter. |
| W0-3 | Toolchain pinning (`rust-toolchain.toml`, `lean-toolchain`/`elan`); `nightly` only if a justified unstable feature is recorded. | S | ∥ | fresh clone + `xtask setup` installs exact versions; versions committed. |
| W0-4 | Build orchestration (D3): Cargo workspace + `cargo xtask` driving Lean (`lake`), codegen, cross builds. | M | | see ▸ W0-4 below; `xtask build` builds Rust+Lean; `--target aarch64` cross-builds. |
| W0-5 | CI pipeline (GitHub Actions). | M | | see ▸ W0-5 below; PR cannot merge unless all jobs green; required status checks on the branch. |
| W0-6 | Formatting + lint gates: `rustfmt`, `clippy -D warnings`, `markdownlint` for `/planning`+`/docs`, Lean style. | S | ∥ | CI fails on any lint; `xtask fmt --check` reproduces locally. |
| W0-7 | Test-harness skeletons: unit (per crate), property (`proptest`, D7), differential (trace-replay stub), fuzz stub (`cargo-fuzz`). | M | ∥ | `xtask test` runs all suites; one example test per kind passes. |
| W0-8 | Benchmark-harness skeleton (`criterion` micro + workload-replay placeholder, §34.6). | S | ∥ | `xtask bench` runs and reports; not gating. |
| W0-9 | **SessionStart hook for Claude-on-web** (use the `session-start-hook` skill). | S | ∥ | `.claude/` hook present; a fresh web session can run `xtask ci` without manual setup. |
| W0-10 | Coding standards + invariant conventions (`docs/CONVENTIONS.md`): transition-tagging, `assert!`/`debug_assert!` profile gating, error taxonomy. | S | ∥ | doc exists; review checklist references it. |
| W0-11 | Governance: `CONTRIBUTING.md`, `CODEOWNERS`, PR/issue templates embedding the Appendix-F checklist + DoD, `SECURITY.md`. | S | ∥ | opening a PR shows the checklist; CODEOWNERS routes `/lean`, `/sele4n`, `/arch`. |
| W0-12 | License split (D5): core MIT; `/sele4n/LICENSE` (seLe4n-compatible) + top-level `NOTICE`; SPDX-header policy. | S | ∥ | `NOTICE` present; CI checks SPDX headers on new files. |
| W0-13 | Versioning + ABI-series policy (`docs/ABI.md`, semver, stats-JSON additive rule §35.3); `topomalloc_version` wired to stats. | S | ∥ | series defined; version in stats JSON (Appendix D). |
| W0-14 | **Walking skeleton (M0 exit).** | M | | see ▸ W0-14 below; M0 exit met end to end on both arches. |

### Repository layout (charter per directory)

```text
/Cargo.toml  /rust-toolchain.toml  /lean-toolchain
/xtask/                      build/codegen/CI driver (W0-4)
/crates/
  topo-core/                 classifier, spans, central list, front/middle-end (no_std-capable)  → plans 03,05
  topo-abi/                  C ABI exports + Rust GlobalAlloc adapter                              → plan 06
  topo-backend-posix/        PosixBackingProvider                                                 → plan 04
  topo-backend-sele4n/       Sele4nBackingProvider + Sele4nSim                                    → plan 09
  topo-arch/                 per-arch asm: RSEQ x86-64/aarch64, restartable sections              → plan 05
  topo-stats/                stats/profiling/explain                                              → plan 07
  topo-control/              config + control namespace                                           → plan 07
  topo-test-support/         trace grammar, deterministic harness, property generators            → plan 08
/lean/TopoMalloc/            model: Types, State, WellFormed, SizeClass, Rseq, Theorems           → plan 02
/lean/TopoMalloc/SeLe4n/     bridge: Bridge, CapBackedArena, *Provider, ResourceServer, …         → plans 02,09
/tools/size-class-gen/       THE size-class generator (single source of truth)                    → plans 02,03
/tools/trace-replay/         executable-model replay / differential runner                        → plan 08
/include/                    generated C headers (topomalloc.h, topomalloc_sele4n.h)              → plan 06
/sele4n/                     resource-server component + adapters (allocman/VKA/VSpace), sep. LICENSE → plan 09
/bench/                      workloads, drivers, results schema                                   → plan 08
/tests/                      cross-crate integration, ABI, concurrency, fork, fuzz corpora        → plan 08
/docs/                       CONVENTIONS, ABI, deployment, profiles, mdbook site                  → plan 10
/profiles/                   profile definitions / feature wiring
/ci/   /planning/   /.claude/
```

> **▸ W0-4 (build orchestration) — decomposed.** `cargo xtask` is the single entry point so contributors and
> CI run identical commands. Minimum command surface, each its own sub-WU:
>
> | Sub-WU | Command | Does |
> |---|---|---|
> | W0-4a | `xtask setup` | install pinned Rust (rustup) + Lean (elan) + cross targets; idempotent. |
> | W0-4b | `xtask build [--target …] [--profile …]` | build all crates; invoke `lake build` for `/lean`; build `/tools`. |
> | W0-4c | `xtask gen` | run `size-class-gen`; **fail if output ≠ committed golden** (G-table). |
> | W0-4d | `xtask test [--kind unit/prop/diff/fuzz]` | run the suites; wires plan 08. |
> | W0-4e | `xtask lint` / `xtask fmt [--check]` | rustfmt+clippy+markdownlint+SPDX+Lean style. |
> | W0-4f | `xtask ci` | the exact sequence CI runs, runnable locally — the contract between dev and CI. |
>
> **Pitfall:** Lean and Rust have separate toolchains; `xtask` must orchestrate both so "build" never means
> "Rust only." Cross builds (`--target aarch64-unknown-linux-gnu`) run under QEMU in CI for the asm/RSEQ paths.

> **▸ W0-5 (CI) — decomposed.** Job graph (each a sub-WU; later milestones extend it, not replace it):
>
> | Sub-WU | Job | Matrix | Gate |
> |---|---|---|---|
> | W0-5a | `build` | {x86-64, aarch64-via-QEMU} × {debug, performance} | G-build |
> | W0-5b | `lint` | host | G-build |
> | W0-5c | `lean` (`lake exe check`) | host | G-build → G-model |
> | W0-5d | `test-unit` + `test-prop` | both arches | G-core |
> | W0-5e | `gen-golden-diff` | host | G-table |
> | W0-5f | `docs` (mdbook build) | host | non-gating until plan 10 |
>
> Toolchains are cached; `aarch64` uses cross + `qemu-user`. Required status checks are configured on the
> working branch so a red job blocks merge. **Best practice:** CI calls `xtask ci` verbatim — no logic lives
> only in YAML.

> **▸ W0-9 (SessionStart hook).** A `.claude/` SessionStart hook runs `xtask setup` (fast path: verify
> toolchains) so a web/CI agent session is build-ready with no manual steps. Keep it fast and idempotent;
> emit a one-line readiness summary. Authored via the `session-start-hook` skill.

> **▸ W0-14 (walking skeleton) — the M0 proof, decomposed.** The skeleton exists to exercise *every* tool in
> one thin vertical slice before any real allocator logic:
>
> | Sub-WU | Deliverable | Proves |
> |---|---|---|
> | W0-14a | `topo-core` exports stub `malloc`/`free` that bump-allocate through `TopoBackingProvider` and leak on free. | the C-ABI export + the seam compile and link. |
> | W0-14b | Both `PosixBackingProvider` and `Sele4nSim` satisfy the trait (stub `reserve/commit`); a runtime flag selects one. | dual-backend wiring (D2). |
> | W0-14c | `/lean` `lake` package builds, incl. an **empty** `TopoMalloc/SeLe4n/Bridge.lean`. | Lean toolchain + bridge co-development from day one. |
> | W0-14d | `size-class-gen` emits a (trivial) table; `xtask gen` golden-diffs it; Lean imports it. | the single-source-of-truth pipeline. |
> | W0-14e | A trace-emit call prints one §33.7 grammar line; the replay stub parses it. | the differential-testing spine. |
>
> Closing all five = **M0**.

---

## Deep dives

> Template: **Problem · Design space · Structures · Work breakdown (finer than the table) · Invariants ·
> Verify · Failure modes · Sequencing.**

### DD-1 · The single-source-of-truth codegen pipeline (W0-4c / W0-14d)

**Problem.** The size-class table (and other generated tables) must be **provably identical** in three
places — the Lean proofs (plan 02), the Rust runtime (plan 03), and the C headers (plan 06) — or the
Appendix-F "hand-maintained tables" anti-pattern silently returns through drift.

**Design space.** **One generator emits data; everyone consumes it; CI golden-diffs** — chosen. Lean derives
the table (plan 02 W1-4e) and serializes it; `tools/size-class-gen` reads that serialization and emits Rust +
C; CI re-runs the generator and fails on any diff from the committed golden.

**Pipeline.**
```text
plan 02 Lean buildTable ──serialize──▶ size-classes.json (committed golden)
                                          │
                 tools/size-class-gen ◀────┘──▶ crates/topo-core/tables.rs  +  include/topomalloc_tables.h
xtask gen: regenerate from the golden; `git diff --exit-code` ⇒ G-table fails if anything diverged
```

**Work breakdown.** 1. define the serialization format + commit the golden (W0-14d). 2. `xtask gen` emits Rust
+ C from the golden (W0-4c). 3. CI runs `xtask gen` and fails on a non-empty diff (W0-5e, G-table).

**Invariants.** the Rust table, the C header, and the Lean golden are byte-for-byte consistent; no table value
is ever hand-edited.

**Verify.** G-table in CI; plan 03's exhaustive `size_class` differential vs Lean closes the loop at runtime.

**Failure modes.** *F1* someone edits `tables.rs` by hand → the golden-diff fails. *F2* the Lean table changes
but the golden isn't refreshed → `lake` emits a new serialization, the diff fails until the golden is updated
in the same PR.

**Sequencing.** **M0** (the trivial table) so the pipeline exists *before* any real table — drift becomes
impossible by construction.

### DD-2 · Dual-arch CI (W0-5a) — AArch64 is not optional

**Problem.** The RSEQ/restartable assembly (plan 05 W7) and the seLe4n target (plan 09) are **AArch64**.
CI that runs only x86-64 would not exercise the arch that the microkernel deployment actually ships on, and
would let AArch64-only asm bugs through to M3/M8.

**Design space.** **A build/test matrix {x86-64 native, aarch64 via cross + `qemu-user`} × {debug,
performance}** — chosen. Native x86-64 for speed; QEMU AArch64 so the asm and forced-migration tests run on
the real instruction set before hardware exists.

**Work breakdown.** 1. cross toolchain + `qemu-user` in `xtask setup`/CI (W0-3/W0-5a). 2. the matrix jobs
(W0-5a). 3. mark asm/RSEQ AArch64 jobs retry-once-on-infra-flake but never auto-skip.

**Invariants.** every gate that can differ by arch (G-build, G-fast) runs on both; AArch64 is co-primary, not
a nightly afterthought.

**Verify.** the matrix is a required status check; plan 05 W7-3 (AArch64 RSEQ) and W7-6 (battery) run under
QEMU here.

**Failure modes.** *F1* AArch64 deferred "until later" → asm bugs surface at M8 on hardware → it is required
from M0. *F2* QEMU flake masks a real failure → retry-once + never auto-skip; a persistent failure blocks
merge.

**Sequencing.** **M0**.

## Sequencing & milestone mapping

```text
W0-1 ─▶ W0-2/3 ─▶ W0-4 ─▶ W0-5/6 ─▶ W0-7/8/9/10/11/12/13  (mostly ∥ after W0-4)
                              └────────────────────────▶ W0-14 (closes M0)
```

All of W0 lands in **M0**. W0-1 (D5, D8) must close before plans 09 and any `/sele4n` file. W0-4/W0-5 unblock
every other plan's CI.

## Domain risks

- **R11** (license incompatibility) and **R13** (ABI drift) are owned here: D5 split + `NOTICE` (W0-12), and
  the pinned-dependency + periodic-bump policy (W0-1/D8, executed in plan 09).
- *Local:* CI flakiness on the QEMU AArch64 path. *Mitigation:* pin the QEMU image; mark the asm/RSEQ jobs
  retry-once but never auto-skip.

## Definition of Done (addendum)

Every W0 WU additionally: (1) is exercised by `xtask ci` so the repo self-verifies; (2) updates the relevant
`docs/` file; (3) leaves a fresh clone green with only `xtask setup` + `xtask ci`.

## Best-practices checklist

- [ ] One entry point (`xtask`) — CI and dev run identical commands.
- [ ] Both toolchains (Rust + Lean) orchestrated together; "build" never means Rust-only.
- [ ] Dual-arch from day one (AArch64 is the seLe4n target; do not defer it).
- [ ] Single source of truth wired before any table exists (W0-14d), so drift is impossible by construction.
- [ ] License split resolved before the first `/sele4n` byte (W0-12).
- [ ] The walking skeleton touches every tool; M0 is not "it builds" but "the whole pipeline runs."

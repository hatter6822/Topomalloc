<!-- SPDX-License-Identifier: MIT -->
# `planning/` — specification & implementation plan

The authoritative design documents for TopoMalloc. This directory is the source
of truth for *what* is built and *in what order*; the code under `crates/`,
`lean/`, etc. implements it.

| Document | Charter |
|----------|---------|
| `SPEC.md` | The TopoMalloc specification (rev 0.3): requirements, architecture, the memory state machine, size classes, APIs, the formal model, and the seLe4n integration profile. |
| `plans/README.md` | The implementation-plan overview & index: principles, the central seam, ratified decisions, milestones M0–M9, CI gates, the risk register, the Definition of Done, and traceability. |
| `plans/01..10-*.md` | Ten focused domain plans (24 workstreams) that decompose the SPEC into reviewable work units. |
| `WORKSTREAM_PLAN.md` | A redirect to `plans/README.md` (the former single-file plan). |

Changes to sequencing, seams, or conformance are PRs against `plans/README.md`;
workstream detail changes go to the relevant domain plan. Markdown here is
markdownlinted (`cargo xtask lint`).

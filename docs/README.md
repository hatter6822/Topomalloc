<!-- SPDX-License-Identifier: MIT -->
# `docs/` — policies, decisions, ABI, and mdBook sources

This directory contains contributor- and operator-facing documentation that must
stay aligned with the code. Long-form roadmap material lives in
[`../planning/`](../planning/); generated API surfaces live in [`../include/`](../include/).

| Document | Purpose |
|----------|---------|
| `ABI.md` | SemVer, C ABI policy, exported symbols, additive stats JSON, and version wiring. |
| `CONVENTIONS.md` | Coding standards, generated-file rules, assertion/profile policy, error taxonomy, and `unsafe` discipline. |
| `DECISIONS.md` | Ratified architecture decisions and audit records. Keep it factual; avoid using it as a progress log. |
| `src/` + `book.toml` | mdBook source for the operator/contributor guide. Build with `mdbook build docs`. |

When changing allocator behavior, update the narrowest relevant document first:
ABI changes go in `ABI.md`, engineering rules in `CONVENTIONS.md`, durable design
rationale in `DECISIONS.md`, and user-facing orientation in the mdBook/README.

<!-- SPDX-License-Identifier: MIT -->
# `docs/` — conventions, ABI, decisions, and the mdbook site

Operator- and contributor-facing documentation. The authoritative *design*
documents are the specification and plan under [`../planning/`](../planning/);
this directory holds the standards and policies that govern the code.

| Document | Charter |
|----------|---------|
| `DECISIONS.md` | The ratified record of decisions D3–D8 (W0-1). |
| `CONVENTIONS.md` | Coding standards: transition tagging, `assert!`/`debug_assert!` profile gating, the error taxonomy, `unsafe`/`no_std` discipline (W0-10). |
| `ABI.md` | Versioning + ABI-series policy; the stats-JSON additive rule; how `topomalloc_version` is wired (W0-13). |
| `src/` + `book.toml` | The mdbook site (`mdbook build docs`), built by the non-gating `docs` CI job (W0-5f). |

The deployment guide and profile reference grow here with plan 10.

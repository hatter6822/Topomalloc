<!-- SPDX-License-Identifier: MIT -->
# Security Policy

TopoMalloc is a memory allocator: correctness *is* security. A bug in the
allocator can become a memory-safety vulnerability in every program that links
it. We take that seriously.

## Supported versions

TopoMalloc is pre-1.0 and under active development. During the `0.x` series only
the latest published release (and `main`) receives security fixes. The ABI is
explicitly **not** stable before 1.0 (see [`docs/ABI.md`](docs/ABI.md)).

## Reporting a vulnerability

**Please do not open a public issue for a security vulnerability.**

Report privately via GitHub's "Report a vulnerability" (Security advisories) on
the repository, or email the maintainer at the address in the commit history.
Include: affected version/commit, a description, and a reproducer if possible.

We aim to acknowledge a report within a few days and to coordinate a fix and
disclosure timeline with you.

## Scope

In scope: any way to make TopoMalloc violate its safety invariants (S-001..S-010
in `planning/SPEC.md`) — e.g. double-free not detected where the profile
requires it, returning overlapping live allocations, metadata corruption from
ordinary API use, integer-overflow leading to under-allocation, or an
information-flow leak across security domains (§36.12).

Out of scope (for now): performance regressions, and behavior in build profiles
explicitly documented as unsafe-for-adversarial-input (e.g. `performance`
profile skips sampled checks by design — see §30.1).

## Security-review cadence

Per the plan ([`planning/plans/README.md`](planning/plans/README.md) §6), the
repository's `security-review` is run:

* at the close of milestones **M4, M7, and M8**, and
* on **any change** touching `/sele4n`, `/arch`, freelist encoding, or metadata
  protection.

## Hardening profiles

For adversarial environments, build with the `hardened` profile (§29): metadata
protection, double/invalid-free detection, quarantine, and guarded allocations.
The `debug` profile additionally runs the Appendix-B invariant checklist as
runtime assertions.

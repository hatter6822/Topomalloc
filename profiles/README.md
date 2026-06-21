<!-- SPDX-License-Identifier: MIT -->
# `profiles/` — profile definitions & feature wiring

Profiles are **features, not forks** (overview principle 8): the same code base
ships every profile, selected by Cargo features rather than separate builds.
This directory is the canonical map from a SPEC profile name (§30.1) to the
Cargo feature set that realizes it.

| Profile (§30.1) | Intent | Cargo features (current → planned) |
|-----------------|--------|------------------------------------|
| `performance` | optimized; sampled checks off; debug fills off (the default) | *(none)* → release-tuned defaults |
| `hardened` | metadata protection, double/invalid-free detection, quarantine, guard pages, scrub-before-downgrade (§29/§36.12) | `topo-core/hardened` = `debug-checks` + `junk-fill` + `quarantine` + `guard-pages` + `secure-scrub` (plan 08 W18) |
| `debug` | the Appendix-B invariant checklist as runtime assertions + junk filling (§30.2/§29.6) | `topo-core/debug` = `debug-checks` + `junk-fill` (plan 08 W18/W19) |
| `deterministic_test` | seeded randomness, deterministic refill, force-slow-path, trace IDs (§30.4) | `topo-test-support` deterministic harness → `deterministic_test` (plan 08 W19-3) |
| `low_rss` | aggressive release / unmap (§20.5) | `topo-core/low-rss` → `RetainPolicy::Unmap` via `from_profile` (plan 04 W4-3b) |
| `hugepage_optimized` | hugepage-aware placement tuned up (§19) | `topo-abi/hugepage-optimized` → `topo-core/hugepage-optimized`: live `malloc` over a `HugePageBackend` engine (`build_posix_allocator` + `Allocator::new_with_huge`, plan 04 W11) |

## W18 hardening — granular, profile-composed features (plan 08)

Each §29 protection is its **own opt-in Cargo feature** (overview principle 8), so
the `performance` profile pays for none and the `hardened`/`debug` profiles compose
the ones they want. The machinery lives in `crate::harden`; a feature that is off
compiles every entry point to a no-op (the `performance` build is byte-for-byte the
un-hardened one).

| Feature | W18 | SPEC | What it adds |
|---------|-----|------|--------------|
| `junk-fill` | W18-5 | §29.6 | fill-on-alloc (`0xAB`) / fill-on-free (`0xDE`) / verify-on-reuse (use-after-free canary) |
| `quarantine` | W18-3 | §29.4 | delayed reuse of freed objects; accounted separately as `quarantine.bytes`; byte/object/per-arena budgets, random-evict + sampling, drain protocol |
| `guard-pages` | W18-4 | §29.5 | sampled (or `TOPO_GUARDED`) allocations with inaccessible `mprotect(PROT_NONE)` guard pages either side — overrun/underrun traps with `SIGSEGV` |
| `secure-scrub` | W18-6 | §36.12 | scrub (zero) a high-domain arena's backing before it is recycled to a lower label (defence-in-depth; the non-PUBLIC scrub is unconditional) |

Always-on W18 pieces (no feature, no perf cost): out-of-line large metadata +
generation/integrity tags (W18-1a/b, §29.2/§17.3 — the freelist is an out-of-line
bitmap, so no critical metadata lives in user-writable memory), and double/invalid-
free detection (W18-2, §29.3 — bitmap double-free, interior/foreign/metadata
classification, sized-delete mismatch, quarantine-hit). The hardened **invariant
checks** (§17.3 integrity validation) are gated by `debug-checks`.

Runtime opt-in: the quarantine and guard sampler are **off by default even with the
feature compiled in** (the RSS/latency cost is not imposed unasked) — enabled via the
`topomalloc_quarantine_*` / `topomalloc_guard_*` C control surface or
`$TOPOMALLOC_QUARANTINE` / `$TOPOMALLOC_GUARD_SAMPLE_RATE`.

## Status at M0

The profile *names* and the stats/`topo.profile` plumbing exist (`topo_stats::Profile`),
and `topo-core` exposes the `debug-checks` feature plus the W18 hardening features
above. The remaining feature wiring is filled in as each subsystem lands
(caches/release in plans 04, 05). The build profile is reported in stats JSON
(`profile`) and via `topo.profile`.

The cargo *build* profiles (`dev`/`release`) are orthogonal: `release` is used
for the `performance` build, but debug assertions are gated by **feature**, not
by the cargo profile, so a `release` build can still run hardened checks.

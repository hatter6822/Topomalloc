---
name: Bug report
about: A correctness, safety, or build problem (NOT a security vulnerability)
title: "[bug] "
labels: bug
---

<!-- SPDX-License-Identifier: MIT -->
<!-- For a SECURITY vulnerability, do NOT use this form — see SECURITY.md. -->

## Summary

<!-- One sentence: what is wrong? -->

## Environment

- TopoMalloc commit / version:
- Profile (performance/hardened/debug/...):
- Backend (posix / sele4n-sim):
- Architecture (x86-64 / aarch64):
- Toolchain (`rustc --version`, Lean version if relevant):

## Reproducer

<!-- Minimal steps or code. A failing `cargo xtask test` invocation, a trace that
diverges on replay, or a property-test seed is ideal. -->

## Expected vs actual

## Relevant invariant / SPEC section

<!-- If known, the safety invariant (S-00x) or SPEC section this violates. -->

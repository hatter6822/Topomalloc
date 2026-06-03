# TopoMalloc Workstream & Implementation Plan — moved

> **This single-file plan has been split into focused domain documents.**
> Start at the overview/index: **[`plans/README.md`](plans/README.md)**.

The former monolith (revisions 1.0–1.2) is preserved in git history. As of revision **2.0** the plan is a set
of one overview plus ten domain plans under [`plans/`](plans/), each self-contained, cross-linked, and
further-decomposed around its complex tasks.

| Document | Workstreams | Covers |
|---|---|---|
| [plans/README.md](plans/README.md) | — | overview & index: principles, the central seam, decisions, milestones M0–M9, CI gates, global risk register, Definition of Done, traceability |
| [01 Repository & infrastructure](plans/01-repository-and-infrastructure.md) | W0 | repo layout, toolchains, build (`xtask`), CI, governance, the M0 walking skeleton |
| [02 Formal model & seLe4n bridge](plans/02-formal-model.md) | W1 | Lean model, size-class proofs, RSEQ contract, theorem sets, trace oracle, bridge |
| [03 Core allocator](plans/03-core-allocator.md) | W2, W3, W5 | size classes/classify, metadata/pagemap/bootstrap, spans/slabs/central, empty-span detection |
| [04 Backend, hugepages, release & topology](plans/04-backend-hugepages-release.md) | W4, W11, W12, W13 | `TopoBackingProvider` seam, POSIX backend, hugepage filler, release controller, NUMA |
| [05 Caches, concurrency & fast paths](plans/05-caches-concurrency-fastpath.md) | W6, W7, W16 | thread/per-CPU/transfer caches, RSEQ asm, lock hierarchy, TLS, fork |
| [06 Public API, realloc & arenas](plans/06-api-realloc-arenas.md) | W8, W15, W9, W10 | C/C++/Rust ABI, realloc/aligned/calloc, capability-backed arenas, extent hooks |
| [07 Observability, placement & control](plans/07-observability-placement-control.md) | W17, W14, W20 | stats/JSON/profiling/explain, lifetime placement, config + control plane |
| [08 Security, debug & testing](plans/08-security-debug-testing.md) | W18, W19, W21 | hardening/quarantine/guard pages, debug checks/sanitizers, property/differential/fuzz |
| [09 seLe4n integration](plans/09-sele4n-integration.md) | W22 | `Sele4nSim`, resource server, client runtime, labels, real-kernel bring-up, conformance |
| [10 Deployment & ABI](plans/10-deployment-and-abi.md) | W23 | interposition, mixed-allocator safety, ABI stability, packaging, perf validation |

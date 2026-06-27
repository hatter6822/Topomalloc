<!-- SPDX-License-Identifier: MIT -->
# `bench/` — benchmarks and result schema

Benchmarks are non-gating diagnostics run with `cargo xtask bench`. They measure
allocator-call latency as well as workload-level behavior such as throughput,
producer/consumer handoff, cache footprint, fragmentation, RSS across phase
changes, release behavior, and tail latency.

| Item | Purpose |
|------|---------|
| `results-schema.json` | Stable schema for comparable benchmark output across commits and machines. |
| Criterion benches | Micro-benchmarks colocated with the crates they measure and orchestrated by `xtask`. |
| Workload/replay inputs | Allocation-distribution and trace workloads used for longer-running comparisons. |

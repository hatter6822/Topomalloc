<!-- SPDX-License-Identifier: MIT -->
# `bench/` — benchmark workloads, drivers, and results schema

The benchmark harness (§34.6, plan 08 W21-6). Benchmarks measure more than
allocator-call time: throughput, cross-thread producer/consumer, idle-cache
footprint, fragmentation over long traces, RSS under phase changes, and tail
latency. They are **non-gating** (run and reported, not pass/fail) until the
results schema and baselines are established.

| Item | Charter |
|------|---------|
| `results-schema.json` | The schema every benchmark run emits, so results are comparable across commits and machines. |
| criterion micro-benches | Live next to the code they measure (`crates/topo-abi/benches/`); run via `cargo xtask bench`. |
| workload replay (placeholder) | Real-service allocation-distribution replay drivers arrive with plan 08; this directory will hold the workloads and drivers. |

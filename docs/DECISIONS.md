<!-- SPDX-License-Identifier: MIT -->
# Decision record — D3–D8 (W0-1)

This is the ratified record of the open decisions from
[`planning/plans/README.md`](../planning/plans/README.md) §4.2. Each is closed
here with its outcome and rationale. D1 and D2 are already ratified in the
overview (§4.1) and are not repeated.

| ID | Decision | Outcome | Status |
|----|----------|---------|--------|
| D3 | Build orchestration | Cargo workspace + `cargo xtask` driving `lake` + codegen | **Ratified, implemented** (W0-4) |
| D4 | Allocator page / `small_max` | `16 KiB` page; `small_max` finalized with the tuned table at M1 | **Ratified (page); small_max at M1** |
| D5 | Licensing | core **MIT**; seLe4n integration **GPL-3.0-or-later** + `NOTICE` | **Ratified, implemented** (W0-12) |
| D6 | Arena routing in caches | bound-arena fast path → arena-qualified slots at M4 | **Ratified, deferred to M2** |
| D7 | Property / fuzz stack | `proptest` + `cargo-fuzz` + a custom differential harness | **Ratified, scaffolded** (W0-7) |
| D8 | Consuming seLe4n crates | git dep pinned to a SHA + vendored mirror + periodic-bump WU | **Ratified; pin recorded** |

## D3 — Build orchestration

`cargo xtask` is the single entry point (`xtask setup/build/gen/test/fmt/lint/lean/bench/ci`).
It orchestrates **both** toolchains: `cargo` for Rust and `lake` for Lean, plus
the codegen pipeline and cross builds. CI invokes the same subcommands, so no
build logic lives only in YAML. See [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## D4 — Allocator page and `small_max`

The allocator page is **16 KiB** (server default, Appendix C). `small_max` (the
largest size served by the front-end size-class table) is targeted at **32 KiB**
but is finalized when the Lean model emits the tuned, non-uniform size-class
table at M1 (plan 02 W1-4e). The M0 walking skeleton ships a deliberately
**trivial** table (`page = 16 KiB`, `small_max = 128 B`, uniform 16-byte
spacing) purely to exercise the single-source-of-truth pipeline (DD-1); it is
mathematically sound but is a placeholder, replaced at M1. All values are
generated constants — never hand-edited (Appendix F).

## D5 — Licensing (closed before the first `/sele4n` byte, W0-12)

The standalone allocator core is **MIT**. The seLe4n-integration layer is
**GPL-3.0-or-later**, because upstream seLe4n
([`hatter6822/seLe4n`](https://github.com/hatter6822/seLe4n)) is GPL-3.0-or-later
and the integration links/models its ABI. The boundary is drawn **now**, before
any seLe4n-linked code exists, so there is never a relicensing event:

* MIT: `crates/topo-core`, `topo-abi`, `topo-backend-posix`, `topo-arch`,
  `topo-stats`, `topo-control`, `topo-test-support`, `xtask`, `tools/*`,
  `lean/TopoMalloc/**` (except the bridge), `include/**`.
* GPL-3.0-or-later: `crates/topo-backend-sele4n`, `lean/TopoMalloc/SeLe4n/**`,
  `sele4n/**`.

The default `libtopomalloc` artifact links no GPL code and is MIT. Building with
the `sele4n-sim` feature produces a GPL combined work. SPDX headers are enforced
by CI. See [`NOTICE`](../NOTICE).

## D6 — Arena routing in caches

The front-end caches use a **bound-arena fast path** (one arena per cache slot
class), upgrading to arena-qualified slots at M4 when multiple authority domains
become active. No cache code exists at M0; the decision is recorded so plan 05
(M2) implements the fast path directly.

## D7 — Property / fuzz / differential stack

* **Property testing:** [`proptest`](https://crates.io/crates/proptest) (see
  `tests/tests/property.rs`).
* **Fuzzing:** [`cargo-fuzz`](https://crates.io/crates/cargo-fuzz) +
  `libfuzzer-sys` (see `fuzz/`), nightly-only and opt-in.
* **Differential testing:** a custom harness — the host executable model
  (`topo-test-support::LiveModel`) and `tools/trace-replay` at M0, superseded by
  the Lean executable model (plan 02 W1-10) as the proof-grade oracle.

## D8 — Consuming the seLe4n crates (closed before plan 09 compiles)

The seLe4n Rust ABI is consumed as a **git dependency pinned to an exact SHA**,
with a vendored mirror and a periodic-bump work unit (plan 09). The pin as of
W0:

| Field | Value |
|-------|-------|
| Repository | `https://github.com/hatter6822/seLe4n` |
| Commit (pin) | `57c11054d31b819364d8089268ab38927881ab0b` |
| Crates | `sele4n-abi`, `sele4n-types` (`sele4n-sys`/`sele4n-hal` as needed) |
| Upstream workspace version | `0.31.50` |
| Upstream MSRV | `1.82` |
| Upstream license | GPL-3.0-or-later |

At M0 the dependency is **recorded but not yet linked**: `topo-backend-sele4n`
ships only the host `Sele4nSim` (no upstream dependency), so the M0 build stays
hermetic. M1 (plan 09 W22-0) enables the pinned git dependency behind the
`real-abi` feature and compiles `Sele4nBackingProvider` against it. Because the
simulator mirrors the pinned ABI surface, any upstream drift becomes a compile
error (risk R13). See [`docs/ABI.md`](ABI.md).

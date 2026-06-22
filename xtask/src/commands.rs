// SPDX-License-Identifier: MIT
//! Implementations of the xtask subcommands. Each returns an [`Outcome`]; `ci`
//! composes the others into the exact sequence CI runs.

use std::path::Path;
use std::process::Command;

use crate::util::{have, target_installed, Outcome, Runner};

/// Directories never scanned by the built-in file checks (build outputs, VCS, and
/// the gitignored, separately-licensed seLe4n ABI mirror under `vendor/`, D8).
const SKIP_DIRS: &[&str] = &["target", ".git", ".lake", "book", "node_modules", "vendor"];

/// True if `args` contains the bare `flag`.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// The value following `flag`, if present.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `build`/`ci` use `cargo build` for the host and any target with a linker, but
/// fall back to `cargo check` for a cross target whose linker is absent (so a
/// dev box without a cross toolchain still verifies AArch64 *compiles*; CI with
/// the linker + QEMU does the full build/run — DD-2).
fn compile_verb(target: Option<&str>) -> &'static str {
    match target {
        Some("aarch64-unknown-linux-gnu") if !have("aarch64-linux-gnu-gcc") => "check",
        _ => "build",
    }
}

// ---------------------------------------------------------------------------

/// `setup [--verify]` — install (or, with `--verify`, just check) the pinned
/// toolchains and cross targets. Idempotent and, in `--verify` mode, fast and
/// non-blocking (used by the SessionStart hook, W0-9).
pub fn setup(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    let verify = has_flag(args, "--verify");

    let rust_ok = have("cargo") && have("rustc");
    r.record("rust toolchain present", rust_ok);
    let lake = have("lake");

    if verify {
        r.note(if lake {
            "lean: lake present — Lean steps will run"
        } else {
            "lean: lake not found — Lean steps are skipped locally (CI installs it)"
        });
        println!("\nxtask setup: ready (rust={rust_ok}, lean={lake})");
        return r.finish();
    }

    r.run(
        "rustup targets",
        "rustup",
        &[
            "target",
            "add",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
        ],
    );
    r.run(
        "rustup components",
        "rustup",
        &["component", "add", "rustfmt", "clippy"],
    );

    // Lean toolchain via the direct-download script (robust behind egress
    // gateways where elan's release host is unreachable). Best-effort: the
    // ~260 MB download must not fail `setup`, and a dev box may not want Lean.
    if root.join("scripts/setup_lean.sh").exists() {
        r.run_optional("lean toolchain", "bash", &["scripts/setup_lean.sh"]);
    } else {
        r.note("scripts/setup_lean.sh missing; cannot install Lean");
    }
    r.finish()
}

/// `build [--target T] [--profile debug|performance]` — build all crates and,
/// when `lake` is present, the Lean package.
pub fn build(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    let target = flag_value(args, "--target");
    let performance = flag_value(args, "--profile") == Some("performance");
    let verb = compile_verb(target);

    let mut cargo_args = vec![verb, "--workspace"];
    if performance {
        cargo_args.push("--release");
    }
    if let Some(t) = target {
        cargo_args.push("--target");
        cargo_args.push(t);
    }
    let label = format!(
        "cargo {verb} ({}, {})",
        target.unwrap_or("host"),
        if performance { "performance" } else { "debug" }
    );
    r.run(&label, "cargo", &cargo_args);

    // The GPL seLe4n path must also compile (dual-backend wiring, D2).
    let mut sim_args = vec![verb, "-p", "topo-abi", "--features", "sele4n-sim"];
    if let Some(t) = target {
        sim_args.push("--target");
        sim_args.push(t);
    }
    r.run("cargo build (sele4n-sim feature)", "cargo", &sim_args);

    lean_steps(&mut r, false);
    r.finish()
}

/// `gen [--check]` — run the size-class generator; `--check` is the G-table gate.
pub fn gen(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    let mut a = vec!["run", "-q", "-p", "size-class-gen"];
    if has_flag(args, "--check") {
        a.push("--");
        a.push("--check");
    }
    r.run("size-class-gen", "cargo", &a);
    r.finish()
}

/// `test [--kind unit|prop|diff|fuzz|loom|tsan|asan|msan|rseq] [--target T]` — run the test suites.
///
/// With `--target` (used by the AArch64 CI job), tests are built for that target
/// and run via the `.cargo/config.toml` runner (`qemu-aarch64`). Without it, the
/// host run additionally exercises the `sele4n-sim` vertical slice (G-sim).
pub fn test(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);

    // A cross target runs the standard workspace suite under the configured
    // runner; per-kind selection below is for host development.
    if let Some(t) = flag_value(args, "--target") {
        // Run single-threaded under the qemu-user runner. qemu-user 8.2.x (Ubuntu
        // 24.04's qemu, used by CI) has a thread-safety bug in its /proc/self/maps
        // emulation: `open_self_maps` / `walk_memory_regions` (linux-user/syscall.c)
        // walk the guest memory map *without* taking the mmap lock, so when one guest
        // thread reads /proc/self/maps (Rust's panic/backtrace machinery does) while
        // another is mmap/munmap-ing (the allocator's backing churn), qemu dereferences
        // a torn page-table entry and dies with "QEMU internal SIGSEGV {addr=0x20}" —
        // the *emulator* crashing, not a guest fault. Fixed upstream by qemu commit
        // bbd5630a75e7 ("linux-user: Emulate /proc/self/maps under mmap_lock", in qemu
        // >= 9.0.4); until the CI runner ships that, serialize so the maps read never
        // races a map mutation. A ~30-line pure-C repro (concurrent /proc/self/maps
        // reads + mmap, no TopoMalloc) crashes this qemu 20/20; single-threaded, 0/40.
        //
        // This costs no concurrency coverage: qemu-user on an x86 host executes guest
        // atomics with the host's strong (TSO) ordering, so it cannot faithfully
        // exercise AArch64's weak memory model regardless of thread count. Real
        // concurrency correctness is covered by ThreadSanitizer + the parallel x86 run
        // (data races / the C++ memory model) and the native-arm64 RSEQ job (real
        // hardware). The asm/instruction-set coverage this qemu job exists for is
        // thread-count-independent.
        r.run(
            "workspace tests",
            "cargo",
            &[
                "test",
                "--workspace",
                "--target",
                t,
                "--",
                "--test-threads=1",
            ],
        );
        return r.finish();
    }

    match flag_value(args, "--kind") {
        Some("unit") => {
            r.run(
                "unit tests",
                "cargo",
                &["test", "--workspace", "--lib", "--bins"],
            );
        }
        Some("prop") => {
            r.run(
                "property tests",
                "cargo",
                &["test", "-p", "topo-tests", "--test", "property"],
            );
        }
        Some("diff") => {
            r.run(
                "differential (walking skeleton)",
                "cargo",
                &["test", "-p", "topo-tests", "--test", "walking_skeleton"],
            );
            r.run(
                "differential (trace-replay)",
                "cargo",
                &["test", "-p", "trace-replay"],
            );
        }
        Some("fuzz") => {
            fuzz_steps(&mut r);
        }
        Some("loom") => {
            loom_steps(&mut r);
        }
        Some("tsan") => {
            tsan_steps(&mut r);
        }
        Some("asan") => {
            asan_steps(&mut r);
        }
        Some("msan") => {
            msan_steps(&mut r);
        }
        Some("rseq") => {
            // The W7 RSEQ / pinned-core battery (also part of the default
            // `--workspace` run; this is the focused subset, G-fast).
            // `--features std` so the self-registration path (its `thread_local!`
            // area) is compiled in — otherwise `self_registration_path_works`
            // passes vacuously through its "kernel lacks rseq" fallback.
            r.run(
                "rseq sequences (topo-arch, std)",
                "cargo",
                &[
                    "test",
                    "-p",
                    "topo-arch",
                    "--test",
                    "rseq",
                    "--features",
                    "std",
                ],
            );
            r.run(
                "rseq equivalence (topo-core)",
                "cargo",
                &["test", "-p", "topo-core", "--test", "rseq_equivalence"],
            );
            r.run(
                "pinned-core (topo-core)",
                "cargo",
                &["test", "-p", "topo-core", "--test", "pinned_core"],
            );
        }
        Some(other) => {
            eprintln!(
                "xtask: unknown --kind '{other}' \
                 (use unit|prop|diff|fuzz|loom|tsan|asan|msan|rseq)"
            );
            r.record("unknown test kind", false);
        }
        None => {
            r.run("workspace tests", "cargo", &["test", "--workspace"]);
            // RSEQ self-registration path (W7-1): build `topo-arch` with `std` so
            // its `thread_local!` self-reg area is compiled in (the workspace run
            // above builds it `no_std`, leaving `self_registration_path_works`
            // vacuous on glibc hosts).
            r.run(
                "rseq self-registration (topo-arch, std)",
                "cargo",
                &["test", "-p", "topo-arch", "--features", "std"],
            );
            r.run(
                "dual-backend (G-sim)",
                "cargo",
                &["test", "-p", "topo-tests", "--features", "sele4n-sim"],
            );
            // W4-3b: exercise the low-rss profile so `RetainPolicy::from_profile`
            // actually resolves to `Unmap` (the aggressive-unmap default), not just
            // the manually-set policy the lifecycle tests use.
            r.run(
                "low-rss profile (retain policy)",
                "cargo",
                &["test", "-p", "topo-core", "--features", "low-rss"],
            );
            global_alloc_smoke_step(&mut r);
        }
    }
    r.finish()
}

/// `fmt [--check]` — rustfmt across the workspace.
pub fn fmt(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    let mut a = vec!["fmt", "--all"];
    if has_flag(args, "--check") {
        a.push("--");
        a.push("--check");
    }
    r.run("rustfmt", "cargo", &a);
    r.finish()
}

/// `lint` — clippy, SPDX, Lean style, license boundary, markdownlint, shellcheck, deny.
pub fn lint(root: &Path, _args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    clippy_steps(&mut r);
    r.record("SPDX headers", check_spdx(root));
    r.record("Lean style", check_lean_style(root));
    r.record(
        "obligation citations (V-004)",
        check_obligation_citations(root),
    );
    r.record("RSEQ CS audit (W7-2d)", check_rseq_cs(root));
    r.record(
        "lock hierarchy (G-conc, W16-1b)",
        check_lock_hierarchy(root),
    );
    r.record("atomics ordering (W16-3)", check_atomics_ordering(root));
    r.record("license boundary", check_license_boundary(root));
    markdownlint_step(&mut r);
    shellcheck_step(&mut r, root);
    deny_step(&mut r);
    r.finish()
}

/// `lean [--check]` — build the Lean package and run `lake exe check`.
pub fn lean(root: &Path, _args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    if !have("lake") {
        r.note("lake not found on PATH; skipping Lean build/check.");
        r.note("Install via elan + the pinned lean-toolchain (see docs/DECISIONS.md). CI runs this for real.");
        return r.finish();
    }
    r.run("lake build", "lake", &["build"]);
    r.run("lake exe check", "lake", &["exe", "check"]);
    r.finish()
}

/// `bench` — run the criterion micro-benchmarks (non-gating).
pub fn bench(root: &Path, _args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    r.run("criterion benches", "cargo", &["bench", "--workspace"]);
    r.finish()
}

/// `ci` — the exact sequence CI runs, end to end and locally reproducible.
pub fn ci(root: &Path, _args: &[String]) -> Outcome {
    let mut r = Runner::new(root);

    // Format + generated-table drift (G-table) first: cheap, catches the common
    // "forgot to regenerate / reformat" mistakes immediately.
    r.run(
        "rustfmt --check",
        "cargo",
        &["fmt", "--all", "--", "--check"],
    );
    r.run(
        "gen --check (G-table)",
        "cargo",
        &["run", "-q", "-p", "size-class-gen", "--", "--check"],
    );

    // Lint gates.
    clippy_steps(&mut r);
    r.record("SPDX headers", check_spdx(root));
    r.record("Lean style", check_lean_style(root));
    r.record(
        "obligation citations (V-004)",
        check_obligation_citations(root),
    );
    r.record("RSEQ CS audit (W7-2d)", check_rseq_cs(root));
    r.record(
        "lock hierarchy (G-conc, W16-1b)",
        check_lock_hierarchy(root),
    );
    r.record("atomics ordering (W16-3)", check_atomics_ordering(root));
    r.record("license boundary", check_license_boundary(root));
    markdownlint_step(&mut r);
    shellcheck_step(&mut r, root);
    deny_step(&mut r);

    // Build matrix: host {debug, performance} + AArch64 {debug} (DD-2).
    r.run("build host (debug)", "cargo", &["build", "--workspace"]);
    r.run(
        "build host (performance)",
        "cargo",
        &["build", "--workspace", "--release"],
    );
    let aarch64_verb = compile_verb(Some("aarch64-unknown-linux-gnu"));
    if aarch64_verb == "check" {
        r.note("AArch64: no cross-linker locally — verifying compilation with `cargo check` (CI links + runs under QEMU)");
    }
    r.run(
        "build AArch64 (debug)",
        "cargo",
        &[
            aarch64_verb,
            "--workspace",
            "--target",
            "aarch64-unknown-linux-gnu",
        ],
    );
    // The Darwin (Apple) `madvise` cfg branch (`MADV_FREE_REUSABLE`, §20.4) is
    // metadata-checked here when its target std is installed, so it cannot bit-rot
    // unnoticed; skipped with a note where the target is absent (the GitHub `build`
    // job installs it and runs this check once — CI is otherwise Linux).
    const APPLE_TARGET: &str = "x86_64-apple-darwin";
    if target_installed(APPLE_TARGET) {
        r.run(
            "check Darwin (apple madvise cfg)",
            "cargo",
            &[
                "check",
                "-p",
                "topo-backend-posix",
                "--target",
                APPLE_TARGET,
            ],
        );
    } else {
        r.note(
            "x86_64-apple-darwin not installed; skipping the Apple madvise cfg check \
             (`rustup target add x86_64-apple-darwin` to enable).",
        );
    }

    // Tests, including the seLe4n simulator vertical slice (G-sim).
    r.run("test host", "cargo", &["test", "--workspace"]);
    // The W8 free-path hardening has *release-only* semantics (stale/double
    // frees are silently rejected where debug builds abort); run the core
    // suite in release so those `cfg(not(debug_assertions))` tests execute.
    r.run(
        "test release semantics (topo-core)",
        "cargo",
        &["test", "-p", "topo-core", "--release", "--lib"],
    );
    // Hardened pass (G-core): the `debug-checks` profile compiles in the §17.3 /
    // Appendix-B invariant checks, so the corruption-resistant classification and
    // descriptor-integrity tests actually run.
    r.run(
        "test hardened (debug-checks)",
        "cargo",
        &["test", "-p", "topo-core", "--features", "debug-checks"],
    );
    // W19-1a (G-core): run the cross-crate **integration** suite with the Appendix-B
    // runtime assertions live, so the engine on-demand sweeps
    // (`Allocator::check_invariants` — incl. the W19-1a pagemap↔descriptor and
    // W19-1c redzone checks) are exercised over real end-to-end malloc/free/realloc
    // sequences in CI, not only topo-core's own unit tests ("runs in debug CI").
    r.run(
        "test integration (debug-checks)",
        "cargo",
        &["test", "-p", "topo-tests", "--features", "debug-checks"],
    );
    // W18 hardened profile (plan 08): the full hardening composition — junk-fill +
    // quarantine + guard-pages + secure-scrub on top of debug-checks. Runs the core
    // suite so every protection's wiring + accounting is exercised *together* (the
    // composed profile, not just each feature alone).
    r.run(
        "test hardened profile (W18)",
        "cargo",
        &["test", "-p", "topo-core", "--features", "hardened"],
    );
    // W18 (#26): each hardening unit must build **and test alone**, not only inside the
    // composed `hardened` profile. A feature that secretly leans on a sibling (a symbol
    // or code path only compiled under another feature) would pass the composed run yet
    // break a deployment that opts into just one protection — exactly the "features, not
    // forks" composition `profiles/README.md` (principle 8) promises. Each single-feature
    // run also exercises that protection's own `#[cfg(feature = "…")]` tests in isolation.
    for feat in ["junk-fill", "quarantine", "guard-pages", "secure-scrub"] {
        r.run(
            &format!("test W18 feature alone: {feat}"),
            "cargo",
            &["test", "-p", "topo-core", "--features", feat, "--lib"],
        );
    }
    // The W18 hardening integration tests over the **real POSIX provider**: the
    // guarded-allocation `mprotect` death test (overrun/underrun ⇒ SIGSEGV) and the
    // live quarantine control surface, which the in-crate `HostProvider` cannot.
    r.run(
        "test W18 hardening integration (POSIX)",
        "cargo",
        &["test", "-p", "topo-tests", "--features", "hardened"],
    );
    // Hardened **release** pass (W16-1b / G-conc): in a `--release --features
    // debug-checks` artifact `debug_assertions` is off, so this proves the
    // lock-order checker (and its `assert!`-based trip) is still compiled in and
    // active under the `debug-checks` feature — not silently elided with the
    // `debug_assert!`s. Scoped to `lock::` to stay fast.
    r.run(
        "test hardened-release lock checker (G-conc)",
        "cargo",
        &[
            "test",
            "-p",
            "topo-core",
            "--release",
            "--features",
            "debug-checks",
            "--lib",
            "lock::",
        ],
    );
    r.run(
        "test dual-backend (G-sim)",
        "cargo",
        &["test", "-p", "topo-tests", "--features", "sele4n-sim"],
    );
    // W4-3b: the low-rss profile selects `RetainPolicy::Unmap` via `from_profile`;
    // run the core suite under it so that aggressive-unmap default is exercised.
    r.run(
        "test low-rss profile (W4-3b retain policy)",
        "cargo",
        &["test", "-p", "topo-core", "--features", "low-rss"],
    );
    // W11: the hugepage_optimized profile routes the engine's large path through the
    // hugepage filler. Run the core suite under it (the filler/backend) and the ABI
    // suite (the hugepage-backed global allocator) so the live wiring is exercised.
    r.run(
        "test hugepage_optimized core (W11)",
        "cargo",
        &[
            "test",
            "-p",
            "topo-core",
            "--features",
            "hugepage-optimized",
        ],
    );
    r.run(
        "test hugepage_optimized ABI (W11 live global allocator)",
        "cargo",
        &["test", "-p", "topo-abi", "--features", "hugepage-optimized"],
    );
    // seLe4n `real-abi`: the GPL backend must keep compiling against the pinned,
    // vendored ABI (D8, W4-1) — guards the `vendor/sele4n` wiring against drift.
    r.run(
        "test seLe4n real-abi (vendored pin)",
        "cargo",
        &[
            "test",
            "-p",
            "topo-backend-sele4n",
            "--features",
            "real-abi",
        ],
    );
    global_alloc_smoke_step(&mut r);

    // C ABI compile-link-run (§34.1) + rustdoc intra-doc-link check.
    abi_test_steps(&mut r, root);
    doc_steps(&mut r);

    // Lean: gating in CI (where lake is installed), best-effort locally.
    lean_steps(&mut r, true);

    r.finish()
}

/// `abi-test` — compile a C harness against `include/topomalloc.h`, link the
/// staticlib, and run it (§34.1): proves the hand-written header matches the ABI.
pub fn abi_test(root: &Path) -> Outcome {
    let mut r = Runner::new(root);
    abi_test_steps(&mut r, root);
    r.finish()
}

/// `doc` — build the docs with `-D warnings` to catch broken intra-doc links.
pub fn doc(root: &Path) -> Outcome {
    let mut r = Runner::new(root);
    doc_steps(&mut r);
    r.finish()
}

/// `deny` — supply-chain audit (licenses, advisories, bans) via cargo-deny.
pub fn deny(root: &Path) -> Outcome {
    let mut r = Runner::new(root);
    deny_step(&mut r);
    r.finish()
}

// ---------------------------------------------------------------------------
// Shared step groups.

fn clippy_steps(r: &mut Runner<'_>) {
    r.run(
        "clippy -D warnings",
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );
    r.run(
        "clippy -D warnings (sele4n-sim)",
        "cargo",
        &[
            "clippy",
            "-p",
            "topo-abi",
            "-p",
            "topo-tests",
            "--all-targets",
            "--features",
            "sele4n-sim",
            "--",
            "-D",
            "warnings",
        ],
    );
    // The `hugepage-optimized` feature (W11) gates conditional code on the live
    // path — the `HugePageBackend`-backed engine wiring in `topo-abi` and the
    // filler tests in `topo-core` — that the default workspace clippy never
    // compiles, so it gets its own gate, mirroring the sele4n-sim slice above.
    r.run(
        "clippy -D warnings (hugepage-optimized)",
        "cargo",
        &[
            "clippy",
            "-p",
            "topo-core",
            "-p",
            "topo-abi",
            "--all-targets",
            "--features",
            "hugepage-optimized",
            "--",
            "-D",
            "warnings",
        ],
    );
}

fn markdownlint_step(r: &mut Runner<'_>) {
    if have("markdownlint-cli2") {
        r.run(
            "markdownlint",
            "markdownlint-cli2",
            &["**/*.md", "!**/target/**", "!**/.lake/**"],
        );
    } else if have("markdownlint") {
        r.run(
            "markdownlint",
            "markdownlint",
            &["planning", "docs", "README.md", "CONTRIBUTING.md"],
        );
    } else {
        r.note(
            "markdownlint not found; skipping (CI runs it). Install: npm i -g markdownlint-cli2",
        );
    }
}

/// Build + check the Lean package. `gating` controls whether a *present* `lake`
/// that fails is a hard failure (true in CI). A missing `lake` is always a
/// non-fatal skip so a fresh clone without Lean stays green.
fn lean_steps(r: &mut Runner<'_>, _gating: bool) {
    if !have("lake") {
        r.note("lake not found; skipping Lean build/check (install via elan; CI runs it).");
        return;
    }
    r.run("lake build", "lake", &["build"]);
    r.run("lake exe check", "lake", &["exe", "check"]);
}

fn fuzz_steps(r: &mut Runner<'_>) {
    if have("cargo-fuzz") {
        // cargo-fuzz needs nightly + libFuzzer; building the targets is enough
        // to keep the fuzz harness compiling (W0-7).
        r.run_optional("cargo fuzz build", "cargo", &["+nightly", "fuzz", "build"]);
    } else {
        r.note("cargo-fuzz not found; skipping fuzz build. Install: cargo install cargo-fuzz (nightly).");
    }
}

/// `loom` model-check of the W3/W4 concurrency protocols (the W3-4 seqlock, the
/// W3-3c pagemap publish/read, and the W4 large-free critical section — the
/// lookup-under-the-pool-lock discipline that makes a concurrent double-free safe).
/// Run under `--cfg loom` so loom and its heavy transitive deps stay out of the
/// normal build/audit. Slower than unit tests (exhaustive interleaving), so it is
/// opt-in (`xtask test --kind loom`), not part of the default `ci` sweep.
fn loom_steps(r: &mut Runner<'_>) {
    std::env::set_var("RUSTFLAGS", "--cfg loom");
    r.run(
        "loom protocols (seqlock + publish/read + large-free)",
        "cargo",
        &["test", "-p", "topo-core", "--test", "loom_protocols"],
    );
    std::env::remove_var("RUSTFLAGS");
}

/// Whether a `nightly` toolchain is installed (TSan needs `-Zsanitizer=thread`
/// + `-Zbuild-std`, both nightly-only).
fn nightly_available() -> bool {
    matches!(
        Command::new("rustc").args(["+nightly", "--version"]).output(),
        Ok(o) if o.status.success()
    )
}

/// ThreadSanitizer over the W6/W7 concurrency tests (the DoD addendum: every
/// concurrency WU runs under TSan). Needs the nightly toolchain (opt-in for the
/// allocator, like `cargo-fuzz`); a missing nightly is noted and skipped, not a
/// failure. **Blind spot:** TSan instruments compiler-generated accesses, *not*
/// inline assembly, so the RSEQ sequence interior is invisible to it — the
/// asm-vs-atomic interactions are covered by the forced-migration conservation
/// tests instead. TSan here validates the locked path, every atomic, and the
/// W7-4 lock/fence coordination.
fn tsan_steps(r: &mut Runner<'_>) {
    if !nightly_available() {
        r.note(
            "nightly toolchain not found; skipping TSan. Install: \
             rustup toolchain install nightly && rustup +nightly component add rust-src. CI runs it.",
        );
        return;
    }
    std::env::set_var("RUSTFLAGS", "-Zsanitizer=thread");
    const T: &str = "x86_64-unknown-linux-gnu";
    r.run(
        "tsan: rseq equivalence + W7-4 coordination (topo-core)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--test",
            "rseq_equivalence",
        ],
    );
    r.run(
        "tsan: rseq battery (topo-arch)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-arch",
            "--test",
            "rseq",
        ],
    );
    r.run(
        "tsan: cache concurrency (topo-core lib)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--lib",
        ],
    );
    // W18-3 (#20): race-check the hardening concurrency — the quarantine's ranked lock
    // + its lock-free stat atomics + membership filter under the concurrent
    // offer/drain stress test (`quarantine_concurrent_*`), and the junk-fill/guard
    // paths — by running the lib suite again with the composed `hardened` features on.
    r.run(
        "tsan: hardening concurrency (topo-core lib, hardened)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--features",
            "hardened",
            "--lib",
        ],
    );
    std::env::remove_var("RUSTFLAGS");
}

/// AddressSanitizer over the `topo-core` library (W19-2, §30.3). ASan catches
/// out-of-bounds, use-after-free, and other spatial/temporal memory errors in the
/// crate's intricate `unsafe` metadata/descriptor/bitmap code. Needs the nightly
/// toolchain (`-Zsanitizer=address` + `-Zbuild-std`, both nightly-only); a missing
/// nightly is noted and skipped, not a failure.
///
/// **RSEQ asm:** the hand-written restartable sequences disable themselves under
/// ASan (`build.rs` → `cfg(topo_sanitize_no_asm)` → `topo_arch::rseq::enable`
/// returns `false`), so the locked baseline runs and there are no asm false
/// positives. **Leaks (W19-2 #4):** LeakSanitizer is **on for the real-allocator C
/// ABI pass** (`topo-tests --test abi`, `detect_leaks=1`), where the harness frees
/// everything and the allocator's monotonic metadata is mmap-backed (untracked) —
/// so a genuine leak fails CI, with `lsan-suppressions.txt` the documented
/// mechanism for by-design monotonic metadata. The lib passes keep
/// `detect_leaks=0`: they `Box::leak` their metadata arenas (`meta()` helpers) and
/// many tests never drop their allocators, so the harness leaks by design,
/// pervasively (§30.3 "where practical") — ASan still vets every spatial/temporal
/// access there. The C/global-allocator `malloc` interposition paths are **not**
/// run under ASan: ASan provides its own allocator and would conflict with the
/// `#[global_allocator]` interposition.
fn asan_steps(r: &mut Runner<'_>) {
    if !nightly_available() {
        r.note(
            "nightly toolchain not found; skipping ASan. Install: \
             rustup toolchain install nightly && rustup +nightly component add rust-src. CI runs it.",
        );
        return;
    }
    std::env::set_var("RUSTFLAGS", "-Zsanitizer=address");
    // Lib tests: leaks are **off** — they `Box::leak` their metadata arenas
    // (`meta()` helpers) and many short-lived tests never drop their constructed
    // allocators, so the test harness leaks by design, pervasively and
    // indistinguishably from a real leak. ASan still checks every spatial/temporal
    // access; the leak check rides on the ABI path below (§30.3 "where practical").
    std::env::set_var("ASAN_OPTIONS", "detect_leaks=0");
    const T: &str = "x86_64-unknown-linux-gnu";
    r.run(
        "asan: core memory safety (topo-core lib)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--lib",
        ],
    );
    // The hardening code (junk fill, quarantine, guard pages, scrub) carries the
    // densest `unsafe`; vet it under ASan with the composed profile on.
    r.run(
        "asan: hardening memory safety (topo-core lib, hardened)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--features",
            "hardened",
            "--lib",
        ],
    );
    // W19-2 (#4): the public C ABI (malloc/free/realloc/aligned/calloc) over the
    // **real POSIX backend** (mmap/madvise) — the backend glue the lib tests (which
    // use in-process metadata) never reach. This `abi` integration target installs
    // no `#[global_allocator]`, so ASan's own allocator does not conflict.
    //
    // **LeakSanitizer is ON here** (`detect_leaks=1`): this is the real-allocator C
    // path, where the harness frees everything and the allocator's monotonic
    // metadata is mmap-backed (which LSan does not track), so the pass is clean and
    // a genuine leak — an object the program freed that the allocator lost — fails
    // CI. The suppression file is the documented mechanism for by-design monotonic
    // metadata, should a build ever source it from a tracked allocator.
    const LSAN_SUPP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/lsan-suppressions.txt");
    std::env::set_var("ASAN_OPTIONS", "detect_leaks=1");
    std::env::set_var("LSAN_OPTIONS", format!("suppressions={LSAN_SUPP}"));
    r.run(
        "asan+lsan: C ABI over POSIX (topo-tests abi)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-tests",
            "--test",
            "abi",
        ],
    );
    std::env::remove_var("LSAN_OPTIONS");
    std::env::remove_var("ASAN_OPTIONS");
    std::env::remove_var("RUSTFLAGS");
}

/// MemorySanitizer over the `topo-core` library (W19-2, §30.3). MSan catches reads
/// of uninitialized memory — the strictest sanitizer. It is scoped to the
/// `no_std`-capable core, whose hot paths take **no** libc calls (all OS access is
/// behind the `TopoBackingProvider` seam, mocked
/// with in-process metadata in the lib tests); running MSan over code that calls an
/// **uninstrumented** libc (the POSIX backend's `mmap`/`madvise`) would false-
/// positive, so those crates are intentionally excluded (§30.3 "where practical").
/// `-Zbuild-std` instruments `std`; the RSEQ asm disables itself under MSan exactly
/// as under ASan. Needs nightly; a missing nightly is noted and skipped.
fn msan_steps(r: &mut Runner<'_>) {
    if !nightly_available() {
        r.note(
            "nightly toolchain not found; skipping MSan. Install: \
             rustup toolchain install nightly && rustup +nightly component add rust-src. CI runs it.",
        );
        return;
    }
    // `-Zsanitizer-memory-track-origins` makes a report name the allocation an
    // uninitialized value originated from (worth the extra cost in CI triage).
    std::env::set_var(
        "RUSTFLAGS",
        "-Zsanitizer=memory -Zsanitizer-memory-track-origins",
    );
    const T: &str = "x86_64-unknown-linux-gnu";
    r.run(
        "msan: core uninitialized-read safety (topo-core lib)",
        "cargo",
        &[
            "+nightly",
            "test",
            "-Zbuild-std",
            "--target",
            T,
            "-p",
            "topo-core",
            "--lib",
        ],
    );
    std::env::remove_var("RUSTFLAGS");
}

/// Run the `#[global_allocator]` bootstrap smoke example (the re-entrancy guard,
/// D1): registering `TopoMallocGlobal` as the process allocator must not deadlock
/// when its lazy initializer allocates. Host-only — the bootstrap is arch-neutral.
fn global_alloc_smoke_step(r: &mut Runner<'_>) {
    r.run(
        "global-allocator bootstrap (re-entrancy guard)",
        "cargo",
        &[
            "run",
            "-q",
            "-p",
            "topo-abi",
            "--example",
            "global_allocator",
        ],
    );
}

// ---------------------------------------------------------------------------
// Built-in SPDX header check (W0-12). No external dependency.

/// Whether the first few lines of a file carry an SPDX identifier (pure; tested).
fn has_spdx_header(content: &str) -> bool {
    content
        .lines()
        .take(5)
        .any(|l| l.contains("SPDX-License-Identifier:"))
}

/// Verify every source/config file carries an `SPDX-License-Identifier` header.
fn check_spdx(root: &Path) -> bool {
    const EXTS: &[&str] = &["rs", "lean", "h", "c", "sh", "toml", "yml", "yaml"];
    let mut missing = Vec::new();
    visit_files(root, SKIP_DIRS, &mut |path| {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !EXTS.contains(&ext) {
            return;
        }
        let content = std::fs::read_to_string(path).unwrap_or_default();
        if !has_spdx_header(&content) {
            missing.push(path.display().to_string());
        }
    });
    if missing.is_empty() {
        println!("  · SPDX: every source/config file carries a license header");
        true
    } else {
        for m in &missing {
            eprintln!("  ✗ missing SPDX-License-Identifier header: {m}");
        }
        false
    }
}

/// Lean-style issues in one file's content (pure; tested): hard tabs, trailing
/// whitespace, missing final newline. Returns `(line_or_0, reason)` pairs.
fn lean_style_issues(content: &str) -> Vec<(usize, &'static str)> {
    let mut issues = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.contains('\t') {
            issues.push((i + 1, "hard tab"));
        }
        if line.len() != line.trim_end().len() {
            issues.push((i + 1, "trailing whitespace"));
        }
    }
    if !content.is_empty() && !content.ends_with('\n') {
        issues.push((0, "missing final newline"));
    }
    issues
}

/// Built-in Lean style check (W0-6): no hard tabs, no trailing whitespace, and a
/// trailing newline. Richer semantic Lean lints arrive with plan 02.
fn check_lean_style(root: &Path) -> bool {
    let mut issues = Vec::new();
    visit_files(root, SKIP_DIRS, &mut |path| {
        if path.extension().and_then(|e| e.to_str()) != Some("lean") {
            return;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for (line, reason) in lean_style_issues(&content) {
            issues.push(format!("{}:{}: {reason}", path.display(), line));
        }
    });
    if issues.is_empty() {
        println!("  · Lean style: no tabs / trailing whitespace; files end with a newline");
        true
    } else {
        for it in issues.iter().take(50) {
            eprintln!("  ✗ Lean style: {it}");
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Formal-obligation citation governance (the W15-3b review lesson, V-004).
//
// A claim that a change carries *no* formal-model obligation — "policy, not safety",
// "no Lean obligation", "adds no abstract transition", "composes certified mechanisms"
// — is exactly the kind of assertion that, left unbacked, lets real proof work slip
// by under a plausible-sounding label (it happened to W15-3b before review). So every
// such claim in crate source MUST cite a concrete, auditable artifact **in the same
// comment block**: either a named Lean theorem (the "sequences certified transitions"
// pattern — W12/W15-3b) or a fixed-wall safety test (the "pure policy" pattern —
// W13/W14). This lint makes that a gate, not a convention.

/// Phrases that assert the absence of a formal-model obligation (lowercased match).
const OBLIGATION_CLAIM_PHRASES: &[&str] = &[
    "no lean obligation",
    "no lean theorem",
    "no new abstract transition",
    "adds no abstract transition",
    "no abstract state-machine transition",
    "not a modeled transition",
    "no new §33.4 obligation",
    "no §33.4 obligation",
];

/// Keywords marking a concrete citation: a Lean theorem (`theorem`/`certif`/`proven`/
/// `proved`/`discharg`) or a pinning safety test (`pin`/`fixed wall`/`fixed-wall`).
const OBLIGATION_CITATION_KEYWORDS: &[&str] = &[
    "pin",
    "certif",
    "proven",
    "proved",
    "discharg",
    "theorem",
    "fixed wall",
    "fixed-wall",
];

/// Flag every contiguous comment block that asserts "no formal obligation" without
/// citing a backing artifact in the **same block** (pure; tested). Joining the block
/// first means a phrase that wraps across doc-comment lines is still seen, and the
/// citation window is exactly the block. Returns `(start_line, phrase)` pairs.
fn obligation_citation_issues(content: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut issues = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].trim_start().starts_with("//") {
            i += 1;
            continue;
        }
        let mut block = String::new();
        // `spans[k] = (block_offset, source_line_idx)` for the start of each joined line,
        // so a block byte-offset maps back to a source line. Joining strips the comment
        // marker (`//`, `///`, `//!`) so a phrase that wraps across doc-comment lines
        // reconstructs ("no Lean" + "/// obligation" → "no lean obligation").
        let mut spans: Vec<(usize, usize)> = Vec::new();
        while i < lines.len() && lines[i].trim_start().starts_with("//") {
            spans.push((block.len(), i));
            let text = lines[i]
                .trim_start()
                .trim_start_matches('/')
                .trim_start_matches('!')
                .trim();
            block.push_str(&text.to_lowercase());
            block.push(' ');
            i += 1;
        }
        // The source line (0-based) a block offset came from.
        let line_of = |off: usize| -> usize {
            spans
                .iter()
                .rev()
                .find(|(bo, _)| *bo <= off)
                .map(|(_, ln)| *ln)
                .unwrap_or(0)
        };
        // A claim is "cited" only if a citation keyword sits within `LINE_WINDOW` source
        // lines of it (≈ the same paragraph) — local enough that a stray keyword elsewhere
        // in a long module doc cannot launder an unrelated bare claim, yet forgiving of a
        // genuine claim and its citation spread over adjacent sentences. Lines (not bytes)
        // are the unit, so the multibyte `§`/`—` never distort the distance.
        const LINE_WINDOW: usize = 6;
        for phrase in OBLIGATION_CLAIM_PHRASES {
            let Some(pos) = block.find(phrase) else {
                continue;
            };
            let pline = line_of(pos);
            let cited = OBLIGATION_CITATION_KEYWORDS.iter().any(|c| {
                block.match_indices(c).any(|(cpos, _)| {
                    // Word-boundary on the left so a citation stem matches only as a word
                    // (`pin`→"pinned"/"pins"), never inside another word ("map**pin**g",
                    // "ap**proved**") — an accidental substring must not launder a claim.
                    let at_word_start = cpos == 0
                        || !block[..cpos]
                            .chars()
                            .next_back()
                            .is_some_and(|ch| ch.is_alphanumeric());
                    at_word_start && line_of(cpos).abs_diff(pline) <= LINE_WINDOW
                })
            });
            if !cited {
                issues.push((pline + 1, (*phrase).to_string()));
            }
        }
    }
    issues
}

/// Gate the "no formal obligation" claims (the W15-3b review lesson): every such claim
/// in crate source must cite a backing theorem or fixed-wall safety test in the same
/// comment block, so "no Lean obligation" is never a bare, unverifiable assertion.
/// Scans `crates/**/src/**/*.rs`.
fn check_obligation_citations(root: &Path) -> bool {
    let mut issues = Vec::new();
    visit_files(root, SKIP_DIRS, &mut |path| {
        let s = path.to_string_lossy();
        if !(s.contains("crates") && s.contains("src") && s.ends_with(".rs")) {
            return;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        for (line, phrase) in obligation_citation_issues(&content) {
            issues.push(format!(
                "{}:{}: \"{phrase}\" with no backing theorem / fixed-wall test cited in the block",
                path.display(),
                line
            ));
        }
    });
    if issues.is_empty() {
        println!(
            "  · obligation citations: every \"no formal obligation\" claim cites a theorem or fixed-wall test"
        );
        true
    } else {
        for it in issues.iter().take(50) {
            eprintln!("  ✗ obligation citation: {it}");
        }
        eprintln!(
            "  → a \"policy, not safety / no Lean obligation\" claim MUST cite a backing artifact: \
             a named Lean theorem (the W12/W15-3b 'sequences certified transitions' pattern) or a \
             fixed-wall safety test (the W13/W14 'pure policy' pattern, e.g. \
             `placement_never_breaks_the_allocation_contract`). See docs/CONVENTIONS.md."
        );
        false
    }
}

/// The RSEQ no-call discipline (W7-2d, §12.3): a restartable critical section
/// MUST contain no calls and no branch-with-link, because the kernel does not
/// restart across them. The per-architecture sequences in `topo-arch` are the
/// only hand-written assembly in the project; this scans their `asm!` string
/// literals and flags any forbidden mnemonic. (The companion "no possibly-faulting
/// memory reference" rule is an audit — every reference is to already-resident
/// per-CPU cache metadata — documented in those modules.)
fn rseq_cs_issues(content: &str) -> Vec<(usize, String)> {
    // Calls / branch-with-link / traps the kernel will not restart across.
    const FORBIDDEN: &[&str] = &[
        "call", "callq", "bl", "blr", "blx", "syscall", "svc", "int", "int3", "ud2",
    ];
    let mut issues = Vec::new();
    for (i, raw) in content.lines().enumerate() {
        let line = raw.trim();
        // Only inspect asm string literals (instruction lines start with `"`).
        if !line.starts_with('"') {
            continue;
        }
        let inner = match line[1..].split('"').next() {
            Some(s) => s.trim(),
            None => continue,
        };
        // The mnemonic is the first whitespace-delimited token (skip `.directive`
        // and `label:` lines, which never name an instruction we forbid).
        let mnem = match inner.split_whitespace().next() {
            Some(t) => t.trim_end_matches(',').to_ascii_lowercase(),
            None => continue,
        };
        if FORBIDDEN.contains(&mnem.as_str()) {
            issues.push((
                i + 1,
                format!("forbidden `{mnem}` inside an RSEQ critical sequence (§12.3)"),
            ));
        }
    }
    issues
}

/// W7-2d gate over the per-architecture RSEQ sequence files. Two checks:
///
/// 1. **No call / branch-with-link** in any `asm!` instruction (§12.3). These
///    files contain *only* the pop/push sequences (no helper code that could
///    legitimately call), so scanning the whole file is the conservative,
///    no-false-pass choice — a forbidden mnemonic anywhere is a real bug.
/// 2. **Structural well-formedness:** each sequence must pair a CS descriptor
///    section (`__rseq_cs`) with a signature-prefixed abort handler
///    (`__rseq_failure` + `RSEQ_SIG`). A sequence missing its abort trampoline
///    would be silently non-restartable, which this catches.
fn check_rseq_cs(root: &Path) -> bool {
    let files = [
        root.join("crates/topo-arch/src/rseq/seq_x86_64.rs"),
        root.join("crates/topo-arch/src/rseq/seq_aarch64.rs"),
    ];
    let mut issues = Vec::new();
    let mut scanned = 0usize;
    for f in &files {
        let Ok(content) = std::fs::read_to_string(f) else {
            continue;
        };
        scanned += 1;
        let name = f.display();
        for (line, reason) in rseq_cs_issues(&content) {
            issues.push(format!("{name}:{line}: {reason}"));
        }
        // Each sequence pairs a `.pushsection __rseq_cs` (the descriptor) with a
        // `.pushsection __rseq_failure` (the abort handler).
        let descriptors = content.matches(".pushsection __rseq_cs").count();
        let aborts = content.matches(".pushsection __rseq_failure").count();
        if descriptors == 0 {
            issues.push(format!("{name}: no `__rseq_cs` descriptor section"));
        }
        if descriptors != aborts {
            issues.push(format!(
                "{name}: {descriptors} CS descriptor(s) but {aborts} abort handler(s) — \
                 every sequence needs both"
            ));
        }
        if !content.contains("RSEQ_SIG") {
            issues.push(format!(
                "{name}: abort handlers must be prefixed by `RSEQ_SIG` (kernel-verified)"
            ));
        }
    }
    if issues.is_empty() {
        println!(
            "  · RSEQ critical sections: no calls/branch-with-link + descriptor↔abort paired \
             ({scanned} files, §12.3)"
        );
        true
    } else {
        for it in issues.iter().take(50) {
            eprintln!("  ✗ RSEQ CS: {it}");
        }
        false
    }
}

/// The lock-hierarchy structural gate (W16-1b, the G-conc gate, DD-3 F1): every
/// lock in `topo-core` MUST go through the ranked `RankedLock` wrapper so the
/// debug lock-order checker sees every acquisition. A lock that escapes the
/// wrapper escapes the checker, so this scans the **non-test** portion of
/// `crates/topo-core/src/**/*.rs` (everything before the first `#[cfg(test)]`)
/// and forbids, outside `lock.rs` (the wrapper's sole home):
///
/// * the test-and-set spinlock idiom `compare_exchange(false, true, …)`, and
/// * any blocking lock primitive — `std::sync::Mutex` / `RwLock` / `Condvar` or a
///   `parking_lot` lock — which would be unranked and (in a `no_std`/hot-path
///   crate) wrong besides.
///
/// Matching `false, true` (not bare `compare_exchange`) keeps a legitimate value
/// CAS (the arena quota, the init-phase advance, a state machine) from tripping
/// it. The runtime half of G-conc — the per-thread held-rank assertion — fails any
/// out-of-order acquire in debug CI; this static half guarantees the runtime
/// checker has nothing it cannot see.
fn lock_hierarchy_issues(content: &str) -> Vec<(usize, String)> {
    // Scan only the non-test portion: tests legitimately model spinlocks / use
    // `std::sync::Mutex` (the loom and conservation models). Tests are conventionally
    // a trailing `#[cfg(test)] mod tests`, so truncate at the first such attribute.
    let end = content.find("#[cfg(test)]").unwrap_or(content.len());
    let mut issues = Vec::new();
    for (i, raw) in content[..end].lines().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") || line.starts_with("//!") {
            continue; // a doc/comment mention (e.g. this gate's own description)
        }
        let normalized: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.contains("compare_exchange_weak(false, true")
            || normalized.contains("compare_exchange(false, true")
        {
            issues.push((
                i + 1,
                "hand-rolled spinlock (`compare_exchange(false, true, …)`) — route it through \
                 `lock::RankedLock` so the W16-1b lock-order checker sees it (§27.2)"
                    .to_string(),
            ));
        }
        // A blocking lock primitive: unranked (escapes the checker) and wrong for a
        // `no_std`-capable hot-path crate. (`std::sync::Once`/`OnceLock`/atomics are
        // fine — only the *locks* are forbidden.)
        for prim in [
            "std::sync::Mutex",
            "std::sync::RwLock",
            "std::sync::Condvar",
            "parking_lot::",
        ] {
            if normalized.contains(prim) {
                issues.push((
                    i + 1,
                    format!(
                        "blocking lock primitive `{prim}` in non-test `topo-core` — use a \
                         ranked `lock::RankedLock` (the single lock primitive, §27.2/W16-1)"
                    ),
                ));
            }
        }
    }
    issues
}

/// Gate the lock-hierarchy discipline (W16-1b / G-conc): no hand-rolled spinlock
/// or unranked blocking lock in non-test `topo-core` outside `lock.rs`. Scans
/// `crates/topo-core/src/**/*.rs`.
fn check_lock_hierarchy(root: &Path) -> bool {
    let mut issues = Vec::new();
    let mut scanned = 0usize;
    visit_files(root, SKIP_DIRS, &mut |path| {
        let s = path.to_string_lossy().replace('\\', "/");
        if !(s.contains("crates/topo-core/src") && s.ends_with(".rs")) {
            return;
        }
        // `lock.rs` IS the ranked-lock wrapper: the one place the primitive lives.
        if s.ends_with("/lock.rs") {
            return;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        scanned += 1;
        for (line, reason) in lock_hierarchy_issues(&content) {
            issues.push(format!("{}:{}: {reason}", path.display(), line));
        }
    });
    if issues.is_empty() {
        println!(
            "  · lock hierarchy (G-conc): every `topo-core` lock is a ranked `RankedLock` \
             ({scanned} files, §27.2/W16-1)"
        );
        true
    } else {
        for it in issues.iter().take(50) {
            eprintln!("  ✗ lock hierarchy: {it}");
        }
        eprintln!(
            "  → route the lock through `lock::RankedLock<{{ LockRank::… }}>` (W16-1a) so the \
             debug checker (W16-1b) enforces the §27.2 order. See the `lock` module docs."
        );
        false
    }
}

/// The §27.3 atomics-ordering-map gate (W16-3): the documented policy is
/// publication = `Release`, consumption = `Acquire`, transitions = `AcqRel` (or a
/// `Release`/`Acquire` pair), counters = `Relaxed` — **`SeqCst` is not in the
/// map**. So a `SeqCst` in non-test `topo-core` production code is an undocumented
/// deviation: it must carry an inline `SeqCst:` justification (the reason it needs
/// the global total order), or be removed in favour of a mapped ordering. Scans
/// the non-test portion of `crates/topo-core/src/**/*.rs` (everything before the
/// first `#[cfg(test)]`), so the loom/conservation test models that legitimately
/// use `SeqCst` for simplicity are exempt.
fn atomics_ordering_issues(content: &str) -> Vec<(usize, String)> {
    let end = content.find("#[cfg(test)]").unwrap_or(content.len());
    let lines: Vec<&str> = content[..end].lines().collect();
    let mut issues = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        if line.contains("SeqCst") {
            // Permit a justified use: an inline or adjacent `SeqCst:` rationale,
            // mirroring the V-004 citation discipline (the map is the law; a
            // deviation must say why it needs the global order).
            let justified = (i.saturating_sub(1)..=i + 1)
                .filter_map(|j| lines.get(j))
                .any(|l| l.contains("SeqCst:"));
            if !justified {
                issues.push((
                    i + 1,
                    "`Ordering::SeqCst` is off the §27.3 ordering map — use Release/Acquire/AcqRel/\
                     Relaxed, or justify with an inline `SeqCst: <reason>` comment (W16-3)"
                        .to_string(),
                ));
            }
        }
    }
    issues
}

/// Gate the §27.3 atomics-ordering map (W16-3): no unjustified `SeqCst` in
/// non-test `topo-core`. Scans `crates/topo-core/src/**/*.rs`.
fn check_atomics_ordering(root: &Path) -> bool {
    let mut issues = Vec::new();
    let mut scanned = 0usize;
    visit_files(root, SKIP_DIRS, &mut |path| {
        let s = path.to_string_lossy().replace('\\', "/");
        if !(s.contains("crates/topo-core/src") && s.ends_with(".rs")) {
            return;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        scanned += 1;
        for (line, reason) in atomics_ordering_issues(&content) {
            issues.push(format!("{}:{}: {reason}", path.display(), line));
        }
    });
    if issues.is_empty() {
        println!(
            "  · atomics ordering (W16-3): no off-map `SeqCst` in non-test `topo-core` \
             ({scanned} files, §27.3)"
        );
        true
    } else {
        for it in issues.iter().take(50) {
            eprintln!("  ✗ atomics ordering: {it}");
        }
        false
    }
}

/// Run `shellcheck` over every `.sh` file, if it is installed.
fn shellcheck_step(r: &mut Runner<'_>, root: &Path) {
    if !have("shellcheck") {
        r.note("shellcheck not found; skipping (CI runs it). Install: apt-get install shellcheck");
        return;
    }
    let files = collect_files(root, &["sh"]);
    if files.is_empty() {
        return;
    }
    let strs: Vec<String> = files.iter().map(|p| p.display().to_string()).collect();
    let mut args: Vec<&str> = vec!["--severity=warning"];
    args.extend(strs.iter().map(String::as_str));
    r.run("shellcheck", "shellcheck", &args);
}

/// Collect files with one of `exts` under `root` (sorted, skipping build dirs).
fn collect_files(root: &Path, exts: &[&str]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    visit_files(root, SKIP_DIRS, &mut |path| {
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if exts.contains(&ext) {
                out.push(path.to_path_buf());
            }
        }
    });
    out.sort();
    out
}

/// Enforce the D5 license boundary: the default (MIT) build of `topo-abi` must
/// not link the GPL `topo-backend-sele4n` crate. Uses `cargo tree` on the normal
/// (non-dev) dependency edges with default features.
fn check_license_boundary(root: &Path) -> bool {
    let output = Command::new("cargo")
        .args([
            "tree",
            "-p",
            "topo-abi",
            "--no-default-features",
            "--edges",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(root)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("topo-backend-sele4n") {
                eprintln!("  ✗ license boundary: the MIT default build of topo-abi links GPL topo-backend-sele4n");
                false
            } else {
                println!("  · license boundary: MIT default build does not link the GPL seLe4n crate (D5)");
                true
            }
        }
        Ok(o) => {
            eprintln!(
                "  ✗ license boundary: cargo tree failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            false
        }
        Err(e) => {
            eprintln!("  ✗ license boundary: could not run cargo tree: {e}");
            false
        }
    }
}

/// Build the staticlib, then compile + link + run the C **and** C++ ABI
/// harnesses (§34.1, plan 06 W8-8: the header must compile under both), and
/// cross-check the exported symbol set against the header declarations
/// (§35.3 ABI pinning).
fn abi_test_steps(r: &mut Runner<'_>, root: &Path) {
    let cc = if have("cc") {
        "cc"
    } else if have("gcc") {
        "gcc"
    } else {
        r.note("no C compiler (cc/gcc) found; skipping C ABI test (CI installs one).");
        return;
    };
    if !r.run(
        "build staticlib (topo-abi)",
        "cargo",
        &["build", "-p", "topo-abi"],
    ) {
        return;
    }

    // W8-8: the header and the binary may not drift — every exported
    // `topomalloc_*`/`topo_*` function must be declared, and vice versa.
    r.record(
        "header ↔ symbol cross-check (W8-8)",
        check_abi_symbols(root),
    );

    let out = root.join("target/debug/abi_smoke");
    let out_str = out.to_string_lossy().into_owned();
    let ok = r.run(
        "compile + link C ABI harness",
        cc,
        &[
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "tests/c/abi_smoke.c",
            "-I",
            "include",
            "-o",
            out_str.as_str(),
            "target/debug/libtopo_abi.a",
            "-lpthread",
            "-ldl",
            "-lm",
        ],
    );
    if ok {
        r.run("run C ABI harness", out_str.as_str(), &[]);
    }

    // C++ harness (W8-5 operators + W8-8 "compiles under C++").
    let cxx = if have("c++") {
        "c++"
    } else if have("g++") {
        "g++"
    } else {
        r.note("no C++ compiler (c++/g++) found; skipping C++ ABI test (CI installs one).");
        return;
    };
    let out_cpp = root.join("target/debug/abi_smoke_cpp");
    let out_cpp_str = out_cpp.to_string_lossy().into_owned();
    let ok = r.run(
        "compile + link C++ ABI harness",
        cxx,
        &[
            "-std=c++17",
            "-Wall",
            "-Wextra",
            "-Werror",
            "tests/cpp/abi_smoke.cpp",
            "-I",
            "include",
            "-o",
            out_cpp_str.as_str(),
            "target/debug/libtopo_abi.a",
            "-lpthread",
            "-ldl",
            "-lm",
        ],
    );
    if ok {
        r.run("run C++ ABI harness", out_cpp_str.as_str(), &[]);
    }
}

/// Function names declared in `include/topomalloc.h` (pure; tested): every
/// lowercase `topomalloc_*`/`topo_*` identifier immediately followed by `(`
/// is a declaration; macros are uppercase and typedefs are not followed by a
/// parenthesis, so neither matches.
fn header_function_names(header: &str) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();
    let bytes = header.as_bytes();
    let is_ident = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        // Identifier start (not mid-identifier).
        if bytes[i].is_ascii_lowercase() && (i == 0 || !is_ident(bytes[i - 1])) {
            let start = i;
            while i < bytes.len() && is_ident(bytes[i]) {
                i += 1;
            }
            let ident = &header[start..i];
            if (ident.starts_with("topomalloc_") || ident.starts_with("topo_"))
                && bytes.get(i) == Some(&b'(')
            {
                names.insert(ident.to_string());
            }
        } else {
            i += 1;
        }
    }
    names
}

/// W8-8 ABI pinning: the set of exported `topomalloc_*`/`topo_*` text symbols
/// in the staticlib must equal the set of functions the public header
/// declares — a symbol without a declaration (or vice versa) is ABI drift.
fn check_abi_symbols(root: &Path) -> bool {
    if !have("nm") {
        println!("  · ABI symbols: nm not found; skipping the cross-check (CI runs it)");
        return true;
    }
    let header = match std::fs::read_to_string(root.join("include/topomalloc.h")) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  ✗ ABI symbols: cannot read include/topomalloc.h: {e}");
            return false;
        }
    };
    let declared = header_function_names(&header);

    let output = Command::new("nm")
        .args(["-g", "--defined-only", "target/debug/libtopo_abi.a"])
        .current_dir(root)
        .output();
    let out = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "  ✗ ABI symbols: nm failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return false;
        }
        Err(e) => {
            eprintln!("  ✗ ABI symbols: could not run nm: {e}");
            return false;
        }
    };
    let exported: std::collections::BTreeSet<String> = out
        .lines()
        .filter_map(|l| {
            // `<addr> T <name>` — exported text symbols only.
            let mut parts = l.split_whitespace();
            let _addr = parts.next()?;
            let kind = parts.next()?;
            let name = parts.next()?;
            (kind == "T" && (name.starts_with("topomalloc_") || name.starts_with("topo_")))
                .then(|| name.to_string())
        })
        .collect();

    let undeclared: Vec<_> = exported.difference(&declared).collect();
    let unexported: Vec<_> = declared.difference(&exported).collect();
    if undeclared.is_empty() && unexported.is_empty() {
        println!(
            "  · ABI symbols: {} exported topomalloc_*/topo_* functions all declared in the header",
            exported.len()
        );
        true
    } else {
        for s in undeclared {
            eprintln!("  ✗ exported but not declared in include/topomalloc.h: {s}");
        }
        for s in unexported {
            eprintln!("  ✗ declared in include/topomalloc.h but not exported: {s}");
        }
        false
    }
}

/// Build the docs with broken-link detection (`RUSTDOCFLAGS=-D warnings`).
fn doc_steps(r: &mut Runner<'_>) {
    // Propagates to the child cargo; a broken intra-doc link then fails the build.
    std::env::set_var("RUSTDOCFLAGS", "-D warnings");
    r.run(
        "cargo doc (-D warnings)",
        "cargo",
        &["doc", "--no-deps", "--workspace"],
    );
}

/// Run cargo-deny (licenses + advisories + bans), if it is installed.
fn deny_step(r: &mut Runner<'_>) {
    if !have("cargo-deny") {
        r.note("cargo-deny not found; skipping (CI runs it). Install: cargo install cargo-deny");
        return;
    }
    r.run("cargo deny check", "cargo", &["deny", "check"]);
}

/// Recursively visit files under `dir`, skipping any directory named in `skip`.
fn visit_files(dir: &Path, skip: &[&str], f: &mut impl FnMut(&Path)) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if skip.contains(&name.as_ref()) {
                continue;
            }
            visit_files(&path, skip, f);
        } else {
            f(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flag_parsing() {
        let a = argv(&[
            "build",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--profile",
            "performance",
        ]);
        assert!(has_flag(&a, "--target"));
        assert!(!has_flag(&a, "--nope"));
        assert_eq!(
            flag_value(&a, "--target"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(flag_value(&a, "--profile"), Some("performance"));
        assert_eq!(flag_value(&a, "--missing"), None);
        // A trailing flag with no following token yields None.
        assert_eq!(flag_value(&argv(&["--check"]), "--check"), None);
    }

    #[test]
    fn compile_verb_uses_build_for_host_targets() {
        assert_eq!(compile_verb(None), "build");
        assert_eq!(compile_verb(Some("x86_64-unknown-linux-gnu")), "build");
    }

    #[test]
    fn obligation_citation_lint_requires_a_backing_citation() {
        // A bare "no Lean obligation" claim is flagged.
        let bare = "//! Placement is policy, so there is no Lean obligation.\n";
        assert_eq!(obligation_citation_issues(bare).len(), 1);

        // A claim citing a Lean theorem passes (the W12/W15-3b "sequences certified" pattern).
        let cited_thm =
            "//! Sequences the certified extent split; no Lean obligation\n//! (pinned by the `foo_preserves_bar` theorem).\n";
        assert!(obligation_citation_issues(cited_thm).is_empty());

        // A claim citing a fixed-wall safety test passes (the W13/W14 "pure policy" pattern).
        let cited_test =
            "/// adds no abstract transition — the fixed wall\n/// `x_never_breaks_y` pins it.\n";
        assert!(obligation_citation_issues(cited_test).is_empty());

        // A phrase that wraps across doc-comment lines is still seen (here uncited ⇒ flagged):
        // the block join is what makes this robust to comment wrapping.
        let wrapped = "/// … so it carries no Lean\n/// obligation for the policy.\n";
        assert_eq!(obligation_citation_issues(wrapped).len(), 1);

        // Two separate uncited blocks are flagged independently; a citation in block A does
        // not excuse block B.
        let two =
            "/// no Lean obligation (pinned by `t`).\nfn a() {}\n/// not a modeled transition here.\nfn b() {}\n";
        assert_eq!(obligation_citation_issues(two).len(), 1);

        // A citation FAR from the claim (same block, but beyond the line window) does not
        // excuse it — the window is local, so a stray keyword elsewhere in a long module
        // doc cannot launder an unrelated bare claim.
        let filler = "/// lorem ipsum dolor sit amet.\n".repeat(8); // 8 comment lines > LINE_WINDOW
        let far = format!("/// no Lean obligation.\n{filler}/// (proved by `t`).\n");
        assert_eq!(
            obligation_citation_issues(&far).len(),
            1,
            "a citation beyond the line window must not excuse the claim"
        );

        // An accidental substring is NOT a citation: "map**pin**g" must not satisfy the
        // `pin` keyword (the real bug a profile.rs scan hit). Word-boundary matching flags
        // this claim despite the nearby "mapping".
        let substring = "//! the feature→profile mapping; so there is no Lean obligation.\n";
        assert_eq!(
            obligation_citation_issues(substring).len(),
            1,
            "a citation stem inside another word (mapPINg) must not launder the claim"
        );
    }

    #[test]
    fn spdx_header_detection() {
        assert!(has_spdx_header(
            "// SPDX-License-Identifier: MIT\nfn main() {}\n"
        ));
        assert!(has_spdx_header(
            "#!/bin/sh\n# SPDX-License-Identifier: GPL-3.0-or-later\n"
        ));
        // Beyond the first 5 lines does not count.
        assert!(!has_spdx_header(
            "a\nb\nc\nd\ne\n// SPDX-License-Identifier: MIT\n"
        ));
        assert!(!has_spdx_header(""));
    }

    #[test]
    fn lean_style_detection() {
        assert!(lean_style_issues("def x := 1\n").is_empty());
        assert_eq!(
            lean_style_issues("def x := 1\tfoo\n"),
            vec![(1, "hard tab")]
        );
        assert_eq!(
            lean_style_issues("def x := 1 \n"),
            vec![(1, "trailing whitespace")]
        );
        assert_eq!(
            lean_style_issues("def x := 1"),
            vec![(0, "missing final newline")]
        );
    }

    #[test]
    fn header_function_name_extraction() {
        let header = r#"
            void *topomalloc_malloc(size_t size);
            void  topomalloc_free(void *ptr);
            size_t topo_nallocx(size_t size, topo_flags_t flags);
            typedef uint64_t topo_flags_t;          /* type, not a function */
            typedef uint32_t topo_arena_t;
            #define TOPO_ALIGN_LG(la) ((la) & 0x3f) /* macro: uppercase */
            int topomalloc_posix_memalign(void **memptr, size_t a, size_t s);
        "#;
        let names = header_function_names(header);
        assert!(names.contains("topomalloc_malloc"));
        assert!(names.contains("topomalloc_free"));
        assert!(names.contains("topo_nallocx"));
        assert!(names.contains("topomalloc_posix_memalign"));
        assert!(
            !names.contains("topo_flags_t"),
            "typedefs are not functions"
        );
        assert!(!names.contains("topo_arena_t"));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn rseq_cs_audit_flags_calls_only() {
        // Allowed instructions and directives — no findings.
        let ok = r#"
            "mov {len:e}, [{slot} + 8]",
            "test {len:e}, {len:e}",
            "jz 7f",
            "b 8f",
            "ldarb {t:w}, [{laddr}]",
            "cbnz {t:w}, 6f",
            ".quad 3f, 4f - 3f, 5f",
            "3:",
        "#;
        assert!(rseq_cs_issues(ok).is_empty());

        // A call, a branch-with-link, and a syscall are each flagged.
        assert_eq!(rseq_cs_issues("            \"call {f}\",\n").len(), 1);
        assert_eq!(rseq_cs_issues("            \"bl {f}\",\n").len(), 1);
        assert_eq!(rseq_cs_issues("            \"blr {x}\",\n").len(), 1);
        assert_eq!(rseq_cs_issues("            \"syscall\",\n").len(), 1);
        // A comment mentioning "call" is not an asm string literal — not flagged.
        assert!(rseq_cs_issues("            // never call inside the CS\n").is_empty());
    }

    #[test]
    fn lock_hierarchy_gate_flags_hand_rolled_locks_only() {
        // The test-and-set spinlock idiom is flagged (it escapes the checker).
        let bad = r#"
            self.locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        "#;
        assert_eq!(lock_hierarchy_issues(bad).len(), 1);
        assert_eq!(
            lock_hierarchy_issues("    x.compare_exchange(false, true, AcqRel, Acquire)").len(),
            1
        );
        // A blocking lock primitive is flagged too (unranked, escapes the checker).
        assert_eq!(
            lock_hierarchy_issues("    let m: std::sync::Mutex<()> = todo!();").len(),
            1
        );
        assert_eq!(lock_hierarchy_issues("use std::sync::RwLock;").len(), 1);
        // A *value* CAS (not a bool lock) is fine — e.g. the arena quota / init flag.
        assert!(lock_hierarchy_issues(
            "    used.compare_exchange_weak(cur, next, AcqRel, Acquire)"
        )
        .is_empty());
        assert!(
            lock_hierarchy_issues("    state.compare_exchange(0, 1, Acquire, Acquire)").is_empty()
        );
        // `OnceLock`/`Once`/atomics are not locks — not flagged.
        assert!(lock_hierarchy_issues("use std::sync::OnceLock;").is_empty());
        // A comment describing the forbidden idiom must not trip the gate.
        assert!(
            lock_hierarchy_issues("    // never compare_exchange(false, true) by hand").is_empty()
        );
        // Test code (after `#[cfg(test)]`) may model spinlocks / use Mutex freely.
        let with_test = "fn ok() {}\n#[cfg(test)]\nmod tests {\n  use std::sync::Mutex;\n  x.compare_exchange(false, true, A, B);\n}";
        assert!(lock_hierarchy_issues(with_test).is_empty());
    }

    #[test]
    fn atomics_ordering_gate_flags_unjustified_seqcst() {
        // An off-map SeqCst in production is flagged.
        assert_eq!(
            atomics_ordering_issues("    x.store(1, Ordering::SeqCst);").len(),
            1
        );
        // A justified one (inline `SeqCst:` rationale) passes.
        assert!(atomics_ordering_issues(
            "    // SeqCst: a Dekker gate needs the global total order here.\n    x.store(1, Ordering::SeqCst);"
        )
        .is_empty());
        // Mapped orderings are fine.
        assert!(atomics_ordering_issues("    x.fetch_add(1, Ordering::AcqRel);").is_empty());
        assert!(atomics_ordering_issues("    x.load(Ordering::Acquire);").is_empty());
        // Test code (after `#[cfg(test)]`) may use SeqCst freely.
        assert!(atomics_ordering_issues(
            "fn ok() {}\n#[cfg(test)]\nmod t {\n  x.store(1, Ordering::SeqCst);\n}"
        )
        .is_empty());
    }
}

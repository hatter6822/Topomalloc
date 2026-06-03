// SPDX-License-Identifier: MIT
//! Implementations of the xtask subcommands. Each returns an [`Outcome`]; `ci`
//! composes the others into the exact sequence CI runs.

use std::path::Path;

use crate::util::{have, Outcome, Runner};

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

    if have("elan") {
        let toolchain = std::fs::read_to_string(root.join("lean-toolchain")).unwrap_or_default();
        let toolchain = toolchain.trim();
        if !toolchain.is_empty() {
            // Best-effort: some networks block the Lean release host; CI installs
            // Lean via the official action regardless.
            r.run_optional(
                "lean toolchain (elan)",
                "elan",
                &["toolchain", "install", toolchain],
            );
        }
    } else {
        r.note("elan not found; install it to build /lean locally (https://github.com/leanprover/elan)");
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

/// `test [--kind unit|prop|diff|fuzz] [--target T]` — run the test suites.
///
/// With `--target` (used by the AArch64 CI job), tests are built for that target
/// and run via the `.cargo/config.toml` runner (`qemu-aarch64`). Without it, the
/// host run additionally exercises the `sele4n-sim` vertical slice (G-sim).
pub fn test(root: &Path, args: &[String]) -> Outcome {
    let mut r = Runner::new(root);

    // A cross target runs the standard workspace suite under the configured
    // runner; per-kind selection below is for host development.
    if let Some(t) = flag_value(args, "--target") {
        r.run(
            "workspace tests",
            "cargo",
            &["test", "--workspace", "--target", t],
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
        Some(other) => {
            eprintln!("xtask: unknown --kind '{other}' (use unit|prop|diff|fuzz)");
            r.record("unknown test kind", false);
        }
        None => {
            r.run("workspace tests", "cargo", &["test", "--workspace"]);
            r.run(
                "dual-backend (G-sim)",
                "cargo",
                &["test", "-p", "topo-tests", "--features", "sele4n-sim"],
            );
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

/// `lint` — clippy (`-D warnings`), SPDX headers, markdownlint, Lean style.
pub fn lint(root: &Path, _args: &[String]) -> Outcome {
    let mut r = Runner::new(root);
    clippy_steps(&mut r);
    r.record("SPDX headers", check_spdx(root));
    markdownlint_step(&mut r);
    r.note("Lean style: SPDX checked above; richer Lean lints arrive with plan 02");
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
    markdownlint_step(&mut r);

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

    // Tests, including the seLe4n simulator vertical slice (G-sim).
    r.run("test host", "cargo", &["test", "--workspace"]);
    r.run(
        "test dual-backend (G-sim)",
        "cargo",
        &["test", "-p", "topo-tests", "--features", "sele4n-sim"],
    );

    // Lean: gating in CI (where lake is installed), best-effort locally.
    lean_steps(&mut r, true);

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

// ---------------------------------------------------------------------------
// Built-in SPDX header check (W0-12). No external dependency.

/// Verify every source file carries an `SPDX-License-Identifier` header.
fn check_spdx(root: &Path) -> bool {
    const EXTS: &[&str] = &["rs", "lean", "h", "c", "sh"];
    const SKIP_DIRS: &[&str] = &["target", ".git", ".lake", "book", "node_modules"];
    let mut missing = Vec::new();
    visit_files(root, SKIP_DIRS, &mut |path| {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !EXTS.contains(&ext) {
            return;
        }
        let head = std::fs::read_to_string(path)
            .map(|c| c.lines().take(5).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default();
        if !head.contains("SPDX-License-Identifier:") {
            missing.push(path.display().to_string());
        }
    });
    if missing.is_empty() {
        println!("  · SPDX: every .rs/.lean/.h/.c/.sh file carries a license header");
        true
    } else {
        for m in &missing {
            eprintln!("  ✗ missing SPDX-License-Identifier header: {m}");
        }
        false
    }
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

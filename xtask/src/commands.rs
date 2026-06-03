// SPDX-License-Identifier: MIT
//! Implementations of the xtask subcommands. Each returns an [`Outcome`]; `ci`
//! composes the others into the exact sequence CI runs.

use std::path::Path;
use std::process::Command;

use crate::util::{have, Outcome, Runner};

/// Directories never scanned by the built-in file checks (build outputs, VCS).
const SKIP_DIRS: &[&str] = &["target", ".git", ".lake", "book", "node_modules"];

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

    // Tests, including the seLe4n simulator vertical slice (G-sim).
    r.run("test host", "cargo", &["test", "--workspace"]);
    r.run(
        "test dual-backend (G-sim)",
        "cargo",
        &["test", "-p", "topo-tests", "--features", "sele4n-sim"],
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

/// Build the staticlib, then compile + link + run the C ABI harness (§34.1).
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
    let out = root.join("target/debug/abi_smoke");
    let out_str = out.to_string_lossy().into_owned();
    let ok = r.run(
        "compile + link C ABI harness",
        cc,
        &[
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
}

// SPDX-License-Identifier: MIT
//! `cargo xtask` — the single build/codegen/CI driver (D3, W0-4).
//!
//! CI and developers run identical commands through here, so "build" never
//! means "Rust only": Lean (`lake`), the codegen pipeline, and cross builds are
//! all orchestrated from one place. The crate is intentionally dependency-free
//! so it builds from a bare toolchain before anything else is fetched.
//!
//! Lean is treated as *best-effort locally*: if `lake` is not on PATH the Lean
//! steps print a clear skip notice and succeed, so a fresh clone is green with
//! just `cargo xtask setup && cargo xtask ci`. CI installs Lean and runs the
//! `lake` steps for real (the `lean` job).
#![allow(unreachable_pub)] // binary crate

mod commands;
mod util;

use std::process::ExitCode;

use util::{project_root, Outcome};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            print_help();
            return ExitCode::FAILURE;
        }
    };

    // Every command runs from the repository root so relative paths (the codegen
    // golden, generated outputs, the Lean package) resolve identically.
    let root = project_root();

    let outcome = match cmd {
        "setup" => commands::setup(&root, rest),
        "build" => commands::build(&root, rest),
        "gen" => commands::gen(&root, rest),
        "test" => commands::test(&root, rest),
        "fmt" => commands::fmt(&root, rest),
        "lint" => commands::lint(&root, rest),
        "lean" => commands::lean(&root, rest),
        "bench" => commands::bench(&root, rest),
        "abi-test" => commands::abi_test(&root),
        "doc" => commands::doc(&root),
        "deny" => commands::deny(&root),
        "ci" => commands::ci(&root, rest),
        "-h" | "--help" | "help" => {
            print_help();
            Outcome::Ok
        }
        other => {
            eprintln!("xtask: unknown command '{other}'\n");
            print_help();
            Outcome::Failed
        }
    };

    match outcome {
        Outcome::Ok => ExitCode::SUCCESS,
        Outcome::Failed => ExitCode::FAILURE,
    }
}

fn print_help() {
    eprintln!(
        "\
cargo xtask <command> — TopoMalloc build/CI driver (W0-4)

COMMANDS
  setup [--verify]            install/verify pinned Rust + Lean + cross targets (idempotent)
  build [--target T] [--profile debug|performance]
                              build all crates (+ Lean via lake, if present)
  gen   [--check]             run the size-class generator; --check fails on drift (G-table)
  test  [--kind unit|prop|diff|fuzz]
                              run the test suites (all kinds if omitted)
  fmt   [--check]             rustfmt (--check reproduces the CI gate)
  lint                        clippy -D warnings + SPDX + Lean style + license boundary + markdownlint + shellcheck + deny
  lean  [--check]             build the Lean package and run `lake exe check`
  bench                       run criterion benches (non-gating)
  abi-test                    compile + link + run the C ABI harness (§34.1)
  doc                         build docs with -D warnings (broken-link check)
  deny                        cargo-deny: licenses + advisories + bans
  ci                          the exact sequence CI runs, end to end

EXAMPLES
  cargo xtask ci
  cargo xtask build --target aarch64-unknown-linux-gnu --profile performance
  cargo xtask gen --check"
    );
}

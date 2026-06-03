// SPDX-License-Identifier: MIT
//! Shared helpers for xtask: locating the repo root, probing for tools, running
//! subprocesses with consistent logging, and accumulating step results so a
//! whole `ci` run reports *every* failure rather than stopping at the first.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The result of a command.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every step passed.
    Ok,
    /// At least one step failed.
    Failed,
}

/// The repository root (the parent of the `xtask` crate directory).
pub fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under the repo root")
        .to_path_buf()
}

/// Whether `bin` is runnable on `PATH`.
pub fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Accumulates step results for a command, printing a header per step and a
/// summary at the end.
pub struct Runner<'a> {
    root: &'a Path,
    failures: Vec<String>,
}

impl<'a> Runner<'a> {
    /// Create a runner rooted at the repository root.
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            failures: Vec::new(),
        }
    }

    /// Run `program args`, logging it under `label`. Records and returns whether
    /// it succeeded; never aborts, so a `ci` run can surface all failures.
    pub fn run(&mut self, label: &str, program: &str, args: &[&str]) -> bool {
        println!("\n\x1b[1m━━ {label} ━━\x1b[0m");
        println!("$ {program} {}", args.join(" "));
        let status = Command::new(program)
            .args(args)
            .current_dir(self.root)
            .status();
        let ok = matches!(status, Ok(s) if s.success());
        if !ok {
            match status {
                Ok(s) => eprintln!("xtask: step '{label}' failed (exit {:?})", s.code()),
                Err(e) => eprintln!("xtask: step '{label}' could not start: {e}"),
            }
            self.failures.push(label.to_string());
        }
        ok
    }

    /// Run an *optional* step: log and execute it, but on failure print a note
    /// instead of recording a failure. For best-effort steps (e.g. installing a
    /// toolchain that the network policy may block) that must not fail `ci`.
    pub fn run_optional(&mut self, label: &str, program: &str, args: &[&str]) -> bool {
        println!("\n\x1b[1m━━ {label} (optional) ━━\x1b[0m");
        println!("$ {program} {}", args.join(" "));
        let ok = matches!(
            Command::new(program).args(args).current_dir(self.root).status(),
            Ok(s) if s.success()
        );
        if !ok {
            println!("  · optional step '{label}' did not succeed; continuing");
        }
        ok
    }

    /// Record the result of a step performed inline (not a subprocess).
    pub fn record(&mut self, label: &str, ok: bool) -> bool {
        println!("\n\x1b[1m━━ {label} ━━\x1b[0m");
        if !ok {
            self.failures.push(label.to_string());
        }
        ok
    }

    /// Print an informational note (e.g. a skipped optional step).
    pub fn note(&self, msg: &str) {
        println!("  · {msg}");
    }

    /// Print the summary and return the overall outcome.
    pub fn finish(self) -> Outcome {
        println!();
        if self.failures.is_empty() {
            println!("\x1b[1;32m✓ xtask: all steps passed\x1b[0m");
            Outcome::Ok
        } else {
            eprintln!(
                "\x1b[1;31m✗ xtask: {} step(s) failed: {}\x1b[0m",
                self.failures.len(),
                self.failures.join(", ")
            );
            Outcome::Failed
        }
    }
}

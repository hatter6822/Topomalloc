// SPDX-License-Identifier: MIT
//! `trace-replay` — the executable-model replay / differential runner stub
//! (§33.7, plan 08 W21-2). It reads a trace in the SPEC grammar (a file argument
//! or stdin), parses each line, and replays it against the host executable model
//! (`topo_test_support::LiveModel`), checking well-formedness at every boundary.
//!
//! At M0 this closes the differential-testing spine end to end (W0-14e): a
//! trace emitted by `topo_core::trace` parses and replays here. The Lean
//! executable model (plan 02 W1-10) becomes the proof-grade oracle later; this
//! host runner keeps replay working without a Lean toolchain.
#![allow(unreachable_pub)] // binary crate

use std::io::Read;
use std::process::ExitCode;

use topo_test_support::{parse_trace_line, LiveModel, ModelError, ParseError};

/// Result of replaying a whole trace.
#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    /// Records successfully applied.
    pub records: usize,
    /// Maximum number of simultaneously-live pointers seen.
    pub peak_live: usize,
}

/// Why a replay failed, with the 1-based line number it failed on.
#[derive(Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// A line could not be parsed.
    Parse {
        /// 1-based line number.
        line: usize,
        /// The parse failure.
        kind: ParseError,
    },
    /// A line parsed but violated a model invariant.
    Model {
        /// 1-based line number.
        line: usize,
        /// The invariant violation.
        kind: ModelError,
    },
}

/// Replay every line of `input`. Blank lines and `#` comments are skipped.
pub fn replay_str(input: &str) -> Result<Summary, ReplayError> {
    let mut model = LiveModel::new();
    let mut records = 0usize;
    let mut peak_live = 0usize;
    for (i, raw) in input.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rec =
            parse_trace_line(line).map_err(|kind| ReplayError::Parse { line: i + 1, kind })?;
        model
            .apply(&rec)
            .map_err(|kind| ReplayError::Model { line: i + 1, kind })?;
        records += 1;
        peak_live = peak_live.max(model.live_count());
    }
    Ok(Summary { records, peak_live })
}

fn main() -> ExitCode {
    let input = match std::env::args().nth(1) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("trace-replay: cannot read {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let mut s = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut s) {
                eprintln!("trace-replay: cannot read stdin: {e}");
                return ExitCode::FAILURE;
            }
            s
        }
    };

    match replay_str(&input) {
        Ok(summary) => {
            println!(
                "trace-replay: OK — {} records replayed, peak live {}",
                summary.records, summary.peak_live
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("trace-replay: divergence: {e:?}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_a_wellformed_trace() {
        let trace = "\
            ALLOC 0 24 16 0 0 -> 0x1000 32 1 5\n\
            ALLOC 1 24 16 0 0 -> 0x2000 32 1 5\n\
            FREE 0x1000 32 -> 1 5\n\
            # a comment line\n\
            FREE 0x2000 32 -> 1 5\n";
        let s = replay_str(trace).expect("wellformed");
        assert_eq!(s.records, 4);
        assert_eq!(s.peak_live, 2);
    }

    #[test]
    fn reports_parse_error_with_line() {
        let trace = "ALLOC 0 24 16 0 0 -> 0x1000 32 1 5\nNONSENSE\n";
        assert_eq!(
            replay_str(trace),
            Err(ReplayError::Parse {
                line: 2,
                kind: ParseError::UnknownVerb
            })
        );
    }

    #[test]
    fn reports_model_violation_with_line() {
        let trace = "FREE 0x1000 0 -> - -\n";
        assert_eq!(
            replay_str(trace),
            Err(ReplayError::Model {
                line: 1,
                kind: ModelError::FreeOfUnknown(0x1000)
            })
        );
    }
}

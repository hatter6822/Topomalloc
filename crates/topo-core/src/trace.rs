// SPDX-License-Identifier: MIT
//! Trace emission in the SPEC §33.7 grammar (the differential-testing spine,
//! W0-14e). Emitting writes one line per event into any `core::fmt::Write` sink,
//! so it works in `no_std` (caller supplies a buffer) and in `std` (write into a
//! `String`). Parsing/replay lives in `topo-test-support` and `tools/trace-replay`
//! so the emit and replay sides can diverge in dependencies but not in grammar.

use core::fmt::{self, Write};

/// Render an optional numeric field: a value or `-` for "not applicable".
struct OptNum(Option<u64>);

impl fmt::Display for OptNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(v) => write!(f, "{v}"),
            None => f.write_str("-"),
        }
    }
}

/// Emit an `ALLOC` line:
/// `ALLOC request_id size align arena flags -> ptr usable_size sc span`.
///
/// `sc`/`span` are `None` for large allocations that are not slab-backed.
#[allow(clippy::too_many_arguments)]
pub fn emit_alloc<W: Write>(
    w: &mut W,
    request_id: u64,
    size: usize,
    align: usize,
    arena: u32,
    flags: u32,
    ptr: usize,
    usable_size: usize,
    sc: Option<u64>,
    span: Option<u64>,
) -> fmt::Result {
    writeln!(
        w,
        "ALLOC {request_id} {size} {align} {arena} {flags} -> {ptr:#x} {usable_size} {} {}",
        OptNum(sc),
        OptNum(span),
    )
}

/// Emit a `FREE` line: `FREE ptr size_hint -> sc span`.
pub fn emit_free<W: Write>(
    w: &mut W,
    ptr: usize,
    size_hint: usize,
    sc: Option<u64>,
    span: Option<u64>,
) -> fmt::Result {
    writeln!(
        w,
        "FREE {ptr:#x} {size_hint} -> {} {}",
        OptNum(sc),
        OptNum(span)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_line_matches_grammar() {
        let mut s = std::string::String::new();
        emit_alloc(&mut s, 7, 24, 16, 0, 0, 0xdead_beef, 32, Some(1), Some(5)).unwrap();
        assert_eq!(s, "ALLOC 7 24 16 0 0 -> 0xdeadbeef 32 1 5\n");
    }

    #[test]
    fn large_alloc_uses_dash_for_sc_span() {
        let mut s = std::string::String::new();
        emit_alloc(&mut s, 1, 100000, 16, 0, 0, 0x1000, 114688, None, None).unwrap();
        assert_eq!(s, "ALLOC 1 100000 16 0 0 -> 0x1000 114688 - -\n");
    }

    #[test]
    fn free_line_matches_grammar() {
        let mut s = std::string::String::new();
        emit_free(&mut s, 0x1000, 24, Some(1), Some(5)).unwrap();
        assert_eq!(s, "FREE 0x1000 24 -> 1 5\n");
    }
}

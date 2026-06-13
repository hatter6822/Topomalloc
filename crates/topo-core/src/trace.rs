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

/// Emit a `REFILL` line: `REFILL cpu sc count source` — a front-end cache pulled
/// `count` objects of class `sc` from `source` (e.g. `"central"`/`"transfer"`).
pub fn emit_refill<W: Write>(
    w: &mut W,
    cpu: u64,
    sc: u64,
    count: u64,
    source: &str,
) -> fmt::Result {
    writeln!(w, "REFILL {cpu} {sc} {count} {source}")
}

/// Emit a `FLUSH` line: `FLUSH cpu sc count target` — a front-end cache pushed
/// `count` objects of class `sc` to `target` (e.g. `"central"`/`"transfer"`).
pub fn emit_flush<W: Write>(w: &mut W, cpu: u64, sc: u64, count: u64, target: &str) -> fmt::Result {
    writeln!(w, "FLUSH {cpu} {sc} {count} {target}")
}

/// Emit a `SPAN_ALLOC` line: `SPAN_ALLOC arena sc span pages hugepage`.
///
/// `hugepage` is the backing hugepage id, or `None` for normal-page spans.
pub fn emit_span_alloc<W: Write>(
    w: &mut W,
    arena: u64,
    sc: u64,
    span: u64,
    pages: u64,
    hugepage: Option<u64>,
) -> fmt::Result {
    writeln!(
        w,
        "SPAN_ALLOC {arena} {sc} {span} {pages} {}",
        OptNum(hugepage)
    )
}

/// Emit a `RELEASE` line: `RELEASE base:len state` — the range `[base, base+len)`
/// transitioned to `state` (e.g. `"dirty"`/`"muzzy"`/`"released"`).
pub fn emit_release<W: Write>(w: &mut W, base: usize, len: usize, state: &str) -> fmt::Result {
    writeln!(w, "RELEASE {base:#x}:{len} {state}")
}

/// Emit an `ARENA_CREATE` line (§22.4/§36.4, W9):
/// `ARENA_CREATE arena rights quota` — a new arena `arena` was created with the
/// authority `rights` (the [`CapRights`](crate::CapRights) bit pattern) and a
/// quota ceiling (`-` for an unlimited quota).
pub fn emit_arena_create<W: Write>(
    w: &mut W,
    arena: u64,
    rights: u8,
    quota: Option<u64>,
) -> fmt::Result {
    writeln!(w, "ARENA_CREATE {arena} {rights} {}", OptNum(quota))
}

/// Emit an `ARENA_DELEGATE` line (§36.4, W9):
/// `ARENA_DELEGATE parent child rights quota` — `child` was delegated from
/// `parent` with attenuated `rights` and quota.
pub fn emit_arena_delegate<W: Write>(
    w: &mut W,
    parent: u64,
    child: u64,
    rights: u8,
    quota: Option<u64>,
) -> fmt::Result {
    writeln!(
        w,
        "ARENA_DELEGATE {parent} {child} {rights} {}",
        OptNum(quota)
    )
}

/// Emit an `ARENA_RESET` line (§22.5, W9): `ARENA_RESET arena reset_gen` — arena
/// `arena` was reset, advancing its reset generation to `reset_gen`.
pub fn emit_arena_reset<W: Write>(w: &mut W, arena: u64, reset_gen: u64) -> fmt::Result {
    writeln!(w, "ARENA_RESET {arena} {reset_gen}")
}

/// Emit an `ARENA_DESTROY` line (§22.6/§36.13, W9):
/// `ARENA_DESTROY arena generation` — arena `arena` was destroyed, advancing its
/// incarnation `generation`.
pub fn emit_arena_destroy<W: Write>(w: &mut W, arena: u64, generation: u64) -> fmt::Result {
    writeln!(w, "ARENA_DESTROY {arena} {generation}")
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

    #[test]
    fn middle_and_back_end_lines_match_grammar() {
        let mut s = std::string::String::new();
        emit_refill(&mut s, 3, 1, 32, "central").unwrap();
        emit_flush(&mut s, 3, 1, 16, "transfer").unwrap();
        emit_span_alloc(&mut s, 0, 1, 7, 1, Some(2)).unwrap();
        emit_span_alloc(&mut s, 0, 1, 8, 1, None).unwrap();
        emit_release(&mut s, 0x2000, 4096, "released").unwrap();
        assert_eq!(
            s,
            "REFILL 3 1 32 central\n\
             FLUSH 3 1 16 transfer\n\
             SPAN_ALLOC 0 1 7 1 2\n\
             SPAN_ALLOC 0 1 8 1 -\n\
             RELEASE 0x2000:4096 released\n"
        );
    }

    #[test]
    fn arena_lifecycle_lines_match_grammar() {
        let mut s = std::string::String::new();
        emit_arena_create(&mut s, 1, 0b1111, Some(4096)).unwrap();
        emit_arena_create(&mut s, 2, 0b0001, None).unwrap();
        emit_arena_delegate(&mut s, 1, 3, 0b0011, Some(512)).unwrap();
        emit_arena_reset(&mut s, 1, 2).unwrap();
        emit_arena_destroy(&mut s, 3, 4).unwrap();
        assert_eq!(
            s,
            "ARENA_CREATE 1 15 4096\n\
             ARENA_CREATE 2 1 -\n\
             ARENA_DELEGATE 1 3 3 512\n\
             ARENA_RESET 1 2\n\
             ARENA_DESTROY 3 4\n"
        );
    }
}

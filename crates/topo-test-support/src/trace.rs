// SPDX-License-Identifier: MIT
//! Parser for the SPEC §33.7 trace grammar. The counterpart to the emit side in
//! `topo_core::trace`; round-tripping the two is tested there and here. The full
//! grammar is supported (`ALLOC`/`FREE`/`REFILL`/`FLUSH`/`SPAN_ALLOC`/`RELEASE`)
//! so the replay tool accepts any conforming trace, even though the M0 skeleton
//! only emits `ALLOC`/`FREE`.

use alloc::string::{String, ToString};

/// A parsed trace record covering the whole §33.7 grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TraceRecord {
    /// `ALLOC request_id size align arena flags -> ptr usable_size sc span`.
    Alloc {
        /// Correlation id.
        request_id: u64,
        /// Requested size.
        size: u64,
        /// Requested alignment.
        align: u64,
        /// Arena id.
        arena: u64,
        /// Extended flags.
        flags: u64,
        /// Returned pointer (`0` = allocation failed).
        ptr: u64,
        /// Usable size of the returned object.
        usable_size: u64,
        /// Size class, or `None` for non-slab (large) allocations.
        sc: Option<u64>,
        /// Span id, or `None`.
        span: Option<u64>,
    },
    /// `FREE ptr size_hint -> sc span`.
    Free {
        /// Pointer being freed (`0` = `free(NULL)`).
        ptr: u64,
        /// Caller-provided size hint (sized delete), or `0`.
        size_hint: u64,
        /// Size class, or `None`.
        sc: Option<u64>,
        /// Span id, or `None`.
        span: Option<u64>,
    },
    /// `REFILL cpu sc count source`.
    Refill {
        /// CPU/cache id.
        cpu: u64,
        /// Size class.
        sc: u64,
        /// Objects moved.
        count: u64,
        /// Source of the refill (e.g. `central`/`transfer`).
        source: String,
    },
    /// `FLUSH cpu sc count target`.
    Flush {
        /// CPU/cache id.
        cpu: u64,
        /// Size class.
        sc: u64,
        /// Objects moved.
        count: u64,
        /// Target of the flush (e.g. `central`/`transfer`).
        target: String,
    },
    /// `SPAN_ALLOC arena sc span pages hugepage`.
    SpanAlloc {
        /// Arena id.
        arena: u64,
        /// Size class.
        sc: u64,
        /// Span id.
        span: u64,
        /// Pages backing the span.
        pages: u64,
        /// Backing hugepage id, or `None` for normal-page spans.
        hugepage: Option<u64>,
    },
    /// `RELEASE base:len state`.
    Release {
        /// Range base address.
        base: u64,
        /// Range length in bytes.
        len: u64,
        /// New memory state (e.g. `dirty`/`muzzy`/`released`).
        state: String,
    },
    /// `ARENA_CREATE arena rights quota` (§22.4/§36.4, W9).
    ArenaCreate {
        /// The created arena id.
        arena: u64,
        /// The authority bits (`CapRights`).
        rights: u64,
        /// The quota ceiling, or `None` for unlimited.
        quota: Option<u64>,
    },
    /// `ARENA_DELEGATE parent child rights quota` (§36.4, W9).
    ArenaDelegate {
        /// The delegating parent arena.
        parent: u64,
        /// The delegated child arena.
        child: u64,
        /// The (attenuated) authority bits.
        rights: u64,
        /// The child's quota ceiling, or `None` for unlimited.
        quota: Option<u64>,
    },
    /// `ARENA_RESET arena reset_gen` (§22.5, W9).
    ArenaReset {
        /// The reset arena id.
        arena: u64,
        /// The new reset generation.
        reset_gen: u64,
    },
    /// `ARENA_DESTROY arena generation` (§22.6/§36.13, W9).
    ArenaDestroy {
        /// The destroyed arena id.
        arena: u64,
        /// The new incarnation generation.
        generation: u64,
    },
}

/// Why a trace line failed to parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The line was empty or whitespace only.
    Empty,
    /// The leading verb was not a known §33.7 verb.
    UnknownVerb,
    /// The line had the wrong number of fields for its verb.
    BadArity,
    /// The `->` separator was missing or misplaced.
    MissingArrow,
    /// A field could not be parsed as the expected number.
    BadField,
}

fn parse_u64(tok: &str) -> Result<u64, ParseError> {
    tok.parse::<u64>().map_err(|_| ParseError::BadField)
}

/// Parse an address field: `0x`-prefixed hex (as emitted) or plain decimal.
fn parse_addr(tok: &str) -> Result<u64, ParseError> {
    if let Some(hex) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| ParseError::BadField)
    } else {
        parse_u64(tok)
    }
}

/// Parse an optional numeric field: `-` is `None`, otherwise a decimal number.
fn parse_opt(tok: &str) -> Result<Option<u64>, ParseError> {
    if tok == "-" {
        Ok(None)
    } else {
        parse_u64(tok).map(Some)
    }
}

/// Parse one line of the §33.7 trace grammar.
pub fn parse_trace_line(line: &str) -> Result<TraceRecord, ParseError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut it = line.split_whitespace();
    let verb = it.next().ok_or(ParseError::Empty)?;

    // Helper: the next token, or BadArity if the line is too short.
    let mut next = || it.next().ok_or(ParseError::BadArity);

    match verb {
        "ALLOC" => {
            let request_id = parse_u64(next()?)?;
            let size = parse_u64(next()?)?;
            let align = parse_u64(next()?)?;
            let arena = parse_u64(next()?)?;
            let flags = parse_u64(next()?)?;
            if next()? != "->" {
                return Err(ParseError::MissingArrow);
            }
            let ptr = parse_addr(next()?)?;
            let usable_size = parse_u64(next()?)?;
            let sc = parse_opt(next()?)?;
            let span = parse_opt(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::Alloc {
                request_id,
                size,
                align,
                arena,
                flags,
                ptr,
                usable_size,
                sc,
                span,
            })
        }
        "FREE" => {
            let ptr = parse_addr(next()?)?;
            let size_hint = parse_u64(next()?)?;
            if next()? != "->" {
                return Err(ParseError::MissingArrow);
            }
            let sc = parse_opt(next()?)?;
            let span = parse_opt(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::Free {
                ptr,
                size_hint,
                sc,
                span,
            })
        }
        "REFILL" | "FLUSH" => {
            let cpu = parse_u64(next()?)?;
            let sc = parse_u64(next()?)?;
            let count = parse_u64(next()?)?;
            let label = next()?.to_string();
            expect_end(&mut it)?;
            if verb == "REFILL" {
                Ok(TraceRecord::Refill {
                    cpu,
                    sc,
                    count,
                    source: label,
                })
            } else {
                Ok(TraceRecord::Flush {
                    cpu,
                    sc,
                    count,
                    target: label,
                })
            }
        }
        "SPAN_ALLOC" => {
            let arena = parse_u64(next()?)?;
            let sc = parse_u64(next()?)?;
            let span = parse_u64(next()?)?;
            let pages = parse_u64(next()?)?;
            let hugepage = parse_opt(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::SpanAlloc {
                arena,
                sc,
                span,
                pages,
                hugepage,
            })
        }
        "RELEASE" => {
            // range token is "base:len"; state is a free-form word.
            let range = next()?;
            let (base_tok, len_tok) = range.split_once(':').ok_or(ParseError::BadField)?;
            let base = parse_addr(base_tok)?;
            let len = parse_u64(len_tok)?;
            let state = next()?.to_string();
            expect_end(&mut it)?;
            Ok(TraceRecord::Release { base, len, state })
        }
        "ARENA_CREATE" => {
            let arena = parse_u64(next()?)?;
            let rights = parse_u64(next()?)?;
            let quota = parse_opt(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::ArenaCreate {
                arena,
                rights,
                quota,
            })
        }
        "ARENA_DELEGATE" => {
            let parent = parse_u64(next()?)?;
            let child = parse_u64(next()?)?;
            let rights = parse_u64(next()?)?;
            let quota = parse_opt(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::ArenaDelegate {
                parent,
                child,
                rights,
                quota,
            })
        }
        "ARENA_RESET" => {
            let arena = parse_u64(next()?)?;
            let reset_gen = parse_u64(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::ArenaReset { arena, reset_gen })
        }
        "ARENA_DESTROY" => {
            let arena = parse_u64(next()?)?;
            let generation = parse_u64(next()?)?;
            expect_end(&mut it)?;
            Ok(TraceRecord::ArenaDestroy { arena, generation })
        }
        _ => Err(ParseError::UnknownVerb),
    }
}

/// Error if the iterator has any tokens left (line too long for its verb).
fn expect_end<'a>(it: &mut impl Iterator<Item = &'a str>) -> Result<(), ParseError> {
    if it.next().is_some() {
        Err(ParseError::BadArity)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alloc_with_sc_span() {
        let r = parse_trace_line("ALLOC 7 24 16 0 0 -> 0xdeadbeef 32 1 5").unwrap();
        assert_eq!(
            r,
            TraceRecord::Alloc {
                request_id: 7,
                size: 24,
                align: 16,
                arena: 0,
                flags: 0,
                ptr: 0xdead_beef,
                usable_size: 32,
                sc: Some(1),
                span: Some(5),
            }
        );
    }

    #[test]
    fn parses_alloc_with_dashes() {
        let r = parse_trace_line("ALLOC 1 100000 16 0 0 -> 0x1000 114688 - -").unwrap();
        match r {
            TraceRecord::Alloc {
                sc,
                span,
                usable_size,
                ..
            } => {
                assert_eq!(sc, None);
                assert_eq!(span, None);
                assert_eq!(usable_size, 114688);
            }
            _ => panic!("expected alloc"),
        }
    }

    #[test]
    fn parses_free() {
        let r = parse_trace_line("FREE 0x1000 24 -> 1 5").unwrap();
        assert_eq!(
            r,
            TraceRecord::Free {
                ptr: 0x1000,
                size_hint: 24,
                sc: Some(1),
                span: Some(5)
            }
        );
    }

    #[test]
    fn parses_refill_flush_span_release() {
        assert_eq!(
            parse_trace_line("REFILL 3 1 32 central").unwrap(),
            TraceRecord::Refill {
                cpu: 3,
                sc: 1,
                count: 32,
                source: "central".to_string()
            }
        );
        assert_eq!(
            parse_trace_line("FLUSH 3 1 16 transfer").unwrap(),
            TraceRecord::Flush {
                cpu: 3,
                sc: 1,
                count: 16,
                target: "transfer".to_string()
            }
        );
        assert_eq!(
            parse_trace_line("SPAN_ALLOC 0 1 7 1 2").unwrap(),
            TraceRecord::SpanAlloc {
                arena: 0,
                sc: 1,
                span: 7,
                pages: 1,
                hugepage: Some(2)
            }
        );
        assert_eq!(
            parse_trace_line("SPAN_ALLOC 0 1 8 1 -").unwrap(),
            TraceRecord::SpanAlloc {
                arena: 0,
                sc: 1,
                span: 8,
                pages: 1,
                hugepage: None
            }
        );
        assert_eq!(
            parse_trace_line("RELEASE 0x2000:4096 released").unwrap(),
            TraceRecord::Release {
                base: 0x2000,
                len: 4096,
                state: "released".to_string()
            }
        );
    }

    #[test]
    fn parses_arena_lifecycle() {
        assert_eq!(
            parse_trace_line("ARENA_CREATE 1 15 4096").unwrap(),
            TraceRecord::ArenaCreate {
                arena: 1,
                rights: 15,
                quota: Some(4096)
            }
        );
        assert_eq!(
            parse_trace_line("ARENA_CREATE 2 1 -").unwrap(),
            TraceRecord::ArenaCreate {
                arena: 2,
                rights: 1,
                quota: None
            }
        );
        assert_eq!(
            parse_trace_line("ARENA_DELEGATE 1 3 3 512").unwrap(),
            TraceRecord::ArenaDelegate {
                parent: 1,
                child: 3,
                rights: 3,
                quota: Some(512)
            }
        );
        assert_eq!(
            parse_trace_line("ARENA_RESET 1 2").unwrap(),
            TraceRecord::ArenaReset {
                arena: 1,
                reset_gen: 2
            }
        );
        assert_eq!(
            parse_trace_line("ARENA_DESTROY 3 4").unwrap(),
            TraceRecord::ArenaDestroy {
                arena: 3,
                generation: 4
            }
        );
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_trace_line(""), Err(ParseError::Empty));
        assert_eq!(parse_trace_line("WAT 1 2 3"), Err(ParseError::UnknownVerb));
        assert_eq!(parse_trace_line("ALLOC 1 2"), Err(ParseError::BadArity));
        assert_eq!(
            parse_trace_line("ALLOC 1 2 3 4 5 X 6 7 8 9"),
            Err(ParseError::MissingArrow)
        );
        assert_eq!(
            parse_trace_line("FREE 0xZZ 1 -> - -"),
            Err(ParseError::BadField)
        );
        assert_eq!(
            parse_trace_line("RELEASE 0x2000 dirty"),
            Err(ParseError::BadField)
        );
        assert_eq!(
            parse_trace_line("REFILL 1 2 3 src extra"),
            Err(ParseError::BadArity)
        );
    }
}

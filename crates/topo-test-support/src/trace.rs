// SPDX-License-Identifier: MIT
//! Parser for the SPEC §33.7 trace grammar. The counterpart to the emit side in
//! `topo_core::trace`; round-tripping the two is tested there and here.

/// A parsed trace record (the M0 subset: `ALLOC` and `FREE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    // Collect tokens without allocating a Vec by indexing a small fixed window.
    let mut it = line.split_whitespace();
    let verb = it.next().ok_or(ParseError::Empty)?;
    match verb {
        "ALLOC" => {
            // request_id size align arena flags -> ptr usable_size sc span
            let request_id = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            let size = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            let align = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            let arena = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            let flags = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            if it.next().ok_or(ParseError::MissingArrow)? != "->" {
                return Err(ParseError::MissingArrow);
            }
            let ptr = parse_addr(it.next().ok_or(ParseError::BadArity)?)?;
            let usable_size = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            let sc = parse_opt(it.next().ok_or(ParseError::BadArity)?)?;
            let span = parse_opt(it.next().ok_or(ParseError::BadArity)?)?;
            if it.next().is_some() {
                return Err(ParseError::BadArity);
            }
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
            // ptr size_hint -> sc span
            let ptr = parse_addr(it.next().ok_or(ParseError::BadArity)?)?;
            let size_hint = parse_u64(it.next().ok_or(ParseError::BadArity)?)?;
            if it.next().ok_or(ParseError::MissingArrow)? != "->" {
                return Err(ParseError::MissingArrow);
            }
            let sc = parse_opt(it.next().ok_or(ParseError::BadArity)?)?;
            let span = parse_opt(it.next().ok_or(ParseError::BadArity)?)?;
            if it.next().is_some() {
                return Err(ParseError::BadArity);
            }
            Ok(TraceRecord::Free {
                ptr,
                size_hint,
                sc,
                span,
            })
        }
        _ => Err(ParseError::UnknownVerb),
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
    }
}

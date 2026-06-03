// SPDX-License-Identifier: MIT
//! Test support: the trace grammar parser (§33.7), a deterministic PRNG and op
//! generator (§30.4 deterministic mode), and a tiny executable model that checks
//! well-formedness at trace boundaries (the differential-testing spine, plan 08
//! W21-2). Parsing lives here — separate from the emit side in `topo-core` — so
//! the two can share the grammar without sharing dependencies.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod model;
pub mod rng;
pub mod trace;

pub use model::{LiveModel, ModelError};
pub use rng::{gen_ops, DetRng, Op};
pub use trace::{parse_trace_line, ParseError, TraceRecord};

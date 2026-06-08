// SPDX-License-Identifier: MIT
//! Linux restartable-sequences (RSEQ) support for the per-CPU fast path
//! (W7, plan 05; SPEC §12, §33.5).
//!
//! This module is being built up workstream-unit by workstream-unit. The first
//! landed piece is the **ABI layer** ([`abi`]): the `struct rseq` / `struct
//! rseq_cs` layout and constants shared with the kernel, which is independent of
//! how a thread's area is registered or which architecture runs the sequence.
//!
//! Still to come (tracked by W7): per-thread registration/detection (W7-1),
//! the per-architecture restartable `pop`/`push` sequences (W7-2/W7-3), the
//! non-owner coordination fence (W7-4), and the seLe4n pinned-core mode (W7-5).

mod abi;

pub use abi::{
    Rseq, RseqCs, RSEQ_CPU_ID_REGISTRATION_FAILED, RSEQ_CPU_ID_UNINITIALIZED, RSEQ_FLAG_UNREGISTER,
    RSEQ_SIG,
};

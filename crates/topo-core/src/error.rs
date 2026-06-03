// SPDX-License-Identifier: MIT
//! Error taxonomy (docs/CONVENTIONS.md). The backing provider is fully fallible
//! (§36.6: "Backing-provider failure MUST leave TopoMalloc state well-formed"),
//! so every seam operation returns a `Result` with a precise cause.

/// A failure from the backing provider (the OS/kernel seam, §3 / §36.6).
///
/// The seLe4n backend maps richer `KernelError`s onto this taxonomy at M1
/// (plan 09 W22-0); the variants here are the host-visible subset the M0
/// skeleton can produce.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum BackendError {
    /// The backend could not satisfy a reservation/commit (mmap/retype failure).
    OutOfMemory,
    /// The request was malformed (e.g., zero size, non-power-of-two alignment).
    InvalidRequest,
    /// The operation is not supported by this backend.
    Unsupported,
}

impl core::fmt::Display for BackendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            BackendError::OutOfMemory => "backend out of memory",
            BackendError::InvalidRequest => "invalid backing request",
            BackendError::Unsupported => "operation unsupported by backend",
        };
        f.write_str(s)
    }
}

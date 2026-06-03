// SPDX-License-Identifier: MIT
//! Per-architecture support. M0 provides only architecture identity and the
//! front-end fast-path *mode* selector; the RSEQ/restartable assembly and the
//! per-CPU sequences (§12) arrive with plan 05 (W7), per-arch for x86-64 and
//! AArch64. Keeping this seam present from M0 means the dual-arch CI matrix
//! (DD-2) exercises real code on both architectures from the start.
#![cfg_attr(not(test), no_std)]

/// The architecture this build targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arch {
    /// x86-64.
    X86_64,
    /// AArch64 (the seLe4n / Raspberry Pi 5 target).
    AArch64,
    /// Any other target (the allocator falls back to portable paths).
    Other,
}

impl Arch {
    /// A short, stable name for diagnostics/trace.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::AArch64 => "aarch64",
            Arch::Other => "other",
        }
    }
}

/// The architecture of the current build.
pub const fn current() -> Arch {
    if cfg!(target_arch = "x86_64") {
        Arch::X86_64
    } else if cfg!(target_arch = "aarch64") {
        Arch::AArch64
    } else {
        Arch::Other
    }
}

/// The front-end fast-path mode (§12). Profiles/work that need a faster path
/// extend this; the allocator is always *correct* in `Locked` (§2.4: a milestone
/// may ship a dumb-but-correct policy, never an unsafe fast path).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FastPathMode {
    /// Global/locked baseline — the only mode at M0. Always correct.
    Locked,
}

/// The fast-path mode for this build. Always `Locked` until plan 05 adds RSEQ
/// (x86-64/AArch64) and pinned-core modes behind a proven abort contract.
pub const fn fast_path_mode() -> FastPathMode {
    FastPathMode::Locked
}

/// Whether Linux RSEQ is available. Always `false` at M0; the real probe (and
/// the §36.10 pinned-core alternative for seLe4n) arrive with plan 05.
pub const fn rseq_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_arch_is_known_on_ci_targets() {
        // The CI matrix runs on x86-64 and AArch64 (DD-2); both are named.
        let a = current();
        assert!(matches!(a, Arch::X86_64 | Arch::AArch64 | Arch::Other));
        assert!(!a.name().is_empty());
    }

    #[test]
    fn m0_is_locked_baseline() {
        assert_eq!(fast_path_mode(), FastPathMode::Locked);
        assert!(!rseq_available());
    }
}

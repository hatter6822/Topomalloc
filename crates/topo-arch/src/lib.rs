// SPDX-License-Identifier: MIT
//! Per-architecture support: architecture identity, the front-end fast-path
//! *mode* selector, and — the only hand-written assembly in the project — the
//! Linux RSEQ restartable per-CPU sequences (§12), per-arch for x86-64 and
//! AArch64 (plan 05 W7). The [`rseq`] module owns registration/detection and the
//! `pop`/`push` sequences; the seLe4n pinned-core alternative (§36.10) lives with
//! the cache in `topo-core`. The dual-arch CI matrix (DD-2) exercises the real
//! sequences on both architectures.
#![cfg_attr(not(any(test, feature = "std")), no_std)]

pub mod rseq;

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

/// The front-end fast-path mode (§12). The allocator is always *correct* in
/// `Locked` (§2.4: a milestone may ship a dumb-but-correct policy, never an
/// unsafe fast path); `Rseq` and `PinnedCore` are the W7 acceleration modes,
/// each proven behaviourally equivalent to `Locked` and selected at runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FastPathMode {
    /// Global/locked baseline. Always correct; the fallback when no faster mode
    /// is available (P-003).
    Locked,
    /// Linux RSEQ restartable per-CPU sequences (W7-2/W7-3).
    Rseq,
    /// seLe4n pinned-thread per-core mode (W7-5, §36.10 option 1).
    PinnedCore,
}

/// The **compile-time baseline** fast-path mode. Always `Locked`: which mode is
/// actually *active* is a runtime decision made by the cache
/// (`topo_core::CpuCache::enable_rseq`, gated on [`rseq::enable`]), because RSEQ
/// availability can only be known at run time. This reports the floor the build
/// always supports, not the selected mode.
pub const fn fast_path_mode() -> FastPathMode {
    FastPathMode::Locked
}

/// Whether Linux RSEQ is available **on this machine, right now** (W7-1). Runs
/// the one-time process detection ([`rseq::enable`]): registration model present
/// and the non-owner fence usable. `false` ⇒ the allocator uses the locked
/// baseline (P-003). Not `const`: the answer is a property of the running kernel,
/// not of the build.
pub fn rseq_available() -> bool {
    rseq::enable()
}

/// Whether the hand-written RSEQ assembly is **disabled for a sanitizer build**
/// (W19-2, §30.3): `true` exactly when this crate was compiled under
/// AddressSanitizer or MemorySanitizer (the `build.rs`-set `cfg(topo_sanitize_no_asm)`),
/// in which case [`rseq_available`]/[`rseq::enable`] always report `false` and the
/// always-correct locked baseline runs — those sanitizers cannot instrument inline
/// asm, so the asm is never executed. A runtime reflector so a dependent crate (or
/// CI test) can assert the disable holds without itself needing the per-crate cfg.
/// `false` in a normal build.
pub const fn asm_disabled_for_sanitizer() -> bool {
    cfg!(topo_sanitize_no_asm)
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
    fn locked_is_the_compile_time_baseline() {
        // The build always supports the locked baseline; faster modes are opt-in.
        assert_eq!(fast_path_mode(), FastPathMode::Locked);
    }

    #[test]
    fn rseq_available_is_consistent_with_the_module() {
        // The convenience probe agrees with the rseq module's own detection.
        assert_eq!(rseq_available(), rseq::available());
    }

    #[test]
    fn sanitizer_asm_disable_is_consistent() {
        // W19-2 (§30.3): when this crate is built under ASan/MSan, the asm is
        // disabled and RSEQ is therefore unavailable. Vacuous in a normal build;
        // load-bearing under a sanitizer build (verifying the build.rs cfg + the
        // `enable()` short-circuit actually take effect). The dependent
        // `topo-core` lib suite runs under the sanitizers in CI and re-asserts
        // this through `asm_disabled_for_sanitizer()`.
        if asm_disabled_for_sanitizer() {
            assert!(
                !rseq_available(),
                "asm disabled for the sanitizer ⇒ rseq unavailable"
            );
            assert!(!rseq::available());
            assert!(!rseq::enable());
        }
    }
}

// SPDX-License-Identifier: MIT
//! Build script (W19-2, §30.3): detect an Address- or MemorySanitizer build and
//! expose a `topo_sanitize_no_asm` cfg so the RSEQ restartable sequences — the
//! only hand-written assembly in the project — disable themselves under those
//! sanitizers. ASan/MSan instrument *compiler-generated* memory accesses, not
//! inline `asm!`, so the asm interior is invisible to them and MSan in particular
//! reports false "use of uninitialized value" on asm-written memory. The locked
//! baseline (`FastPathMode::Locked`, always correct, §2.4) is used instead.
//!
//! ThreadSanitizer is deliberately **excluded**: it ignores inline asm (a
//! documented blind spot — see `tsan_steps` in `xtask`), and the RSEQ equivalence
//! battery runs under it on purpose to race-check the lock/fence coordination.
//!
//! The detection reads `CARGO_ENCODED_RUSTFLAGS` (the `0x1f`-separated rustflags
//! Cargo hands to build scripts), so it fires for both `RUSTFLAGS=...` and
//! `[build] rustflags`/`-Zsanitizer=...` invocations.

fn main() {
    // Register the custom cfg so the (warn-by-default) `unexpected_cfgs` lint
    // does not flag it on toolchains that check cfgs.
    println!("cargo::rustc-check-cfg=cfg(topo_sanitize_no_asm)");
    println!("cargo::rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");

    let flags = std::env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    let has = |needle: &str| flags.split('\u{1f}').any(|flag| flag.contains(needle));
    if has("sanitizer=address") || has("sanitizer=memory") {
        println!("cargo::rustc-cfg=topo_sanitize_no_asm");
    }
}

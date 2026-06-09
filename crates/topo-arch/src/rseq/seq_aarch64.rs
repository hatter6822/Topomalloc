// SPDX-License-Identifier: MIT
//! AArch64 restartable `pop`/`push` sequences (W7-3a; SPEC §12.3, §33.5).
//!
//! The AArch64 mirror of [`super::seq_x86_64`] — co-primary, not a port, because
//! AArch64 is the seLe4n / Raspberry Pi 5 target (DD-2). Same non-negotiable
//! shape: a single critical section whose only state change is one committing
//! store of `len`; an abort before it is invisible (plan 02 W1-7 frame
//! condition). For `push`, the object is staged into the logically-free `buf[len]`
//! and published only by the `len` increment.
//!
//! **No calls, no possibly-faulting reference (§12.3).** Every reference is to
//! resident per-CPU cache metadata; there are no calls.
//!
//! **Clobbers / barriers (W7-3b).** Every scratch register is declared as an
//! output. Unlike x86-64's TSO, AArch64 is weakly ordered, so the per-CPU lock
//! byte is read with **load-acquire** (`ldarb`): observing `locked == 0` then
//! synchronises-with the locked path's `Release` unlock, so a slot the locked
//! path initialised (its `buf`/`len`/cap stores) is seen coherently before the
//! sequence dereferences `buf`. The committing `str` of `len` is published the
//! same way the locked path's `Relaxed` store is, and aligned word stores are
//! atomic on AArch64, so a concurrent `Relaxed` reader never tears.

use super::abi::{Rseq, RSEQ_SIG};
use super::{Pop, Push};

/// AArch64 restartable pop (W7-3a). See the module docs for the sequence shape.
///
/// # Safety
/// Identical contract to [`super::seq_x86_64::pop`].
#[inline]
pub(super) unsafe fn pop<const LEN_OFF: usize, const BUF_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
) -> Pop {
    let status: u64;
    let val: usize;
    // SAFETY: see the function contract; resident loads + one committing store,
    // with the abort handler restoring a logical no-op.
    unsafe {
        core::arch::asm!(
            // ---- critical-section descriptor ----
            ".pushsection __rseq_cs, \"aw\"",
            ".balign 32",
            "2:",
            ".long 0, 0",                       // version, flags
            ".quad 3f, 4f - 3f, 5f",            // start_ip, post_commit_offset, abort_ip
            ".popsection",
            // ---- arm the critical section ----
            "adrp {tmp}, 2b",
            "add {tmp}, {tmp}, :lo12:2b",
            "str {tmp}, [{area}, #8]",           // rseq.rseq_cs = &descriptor
            // ---- start_ip ----
            "3:",
            "ldr {cpu:w}, [{area}, #4]",         // cpu = rseq.cpu_id (inside the CS)
            "cmp {cpu}, {maxcpus}",              // bounds-check cpu < MAX_CPUS
            "bhs 6f",                            // out of range → Fallback (memory safety)
            "mul {off}, {cpu}, {stride}",        // off = cpu * stride
            "add {laddr}, {lbase}, {off}",
            "ldarb {t:w}, [{laddr}]",            // load-acquire the per-CPU lock byte
            "cbnz {t:w}, 6f",                    // lock held → Fallback (W7-4)
            "add {slot}, {sbase}, {off}",        // &slot
            "ldr {buf}, [{slot}, {bufoff}]",
            "cbz {buf}, 6f",                     // uninitialised slot → Fallback
            "ldr {len:w}, [{slot}, {lenoff}]",
            "cbz {len:w}, 7f",                   // empty → Empty (no commit)
            "sub {len:w}, {len:w}, #1",
            "ldr {val}, [{buf}, {len}, lsl #3]", // val = buf[len-1]  (read, not commit)
            "str {len:w}, [{slot}, {lenoff}]",   // COMMIT: len ← len-1
            "4:",                                // post_commit_ip
            "mov {status:w}, #0",                // Success
            "b 8f",
            "6:",
            "mov {status:w}, #3",                // Fallback
            "mov {val}, #0",
            "b 8f",
            "7:",
            "mov {status:w}, #1",                // Empty
            "mov {val}, #0",
            "b 8f",
            // ---- abort handler (signature-prefixed, in the failure section) ----
            ".pushsection __rseq_failure, \"ax\"",
            ".long {sig}",                       // the 4 bytes preceding abort_ip
            "5:",                                // abort_ip
            "mov {status:w}, #2",                // Abort
            "mov {val}, #0",
            "b 8f",
            ".popsection",
            "8:",
            area = in(reg) area,
            sbase = in(reg) slot_base,
            lbase = in(reg) locked_base,
            stride = in(reg) stride,
            maxcpus = in(reg) max_cpus,
            lenoff = const LEN_OFF,
            bufoff = const BUF_OFF,
            sig = const RSEQ_SIG,
            tmp = out(reg) _,
            cpu = out(reg) _,
            off = out(reg) _,
            laddr = out(reg) _,
            t = out(reg) _,
            slot = out(reg) _,
            buf = out(reg) _,
            len = out(reg) _,
            val = out(reg) val,
            status = out(reg) status,
            options(nostack),
        );
    }
    match status {
        0 => Pop::Success(val),
        1 => Pop::Empty,
        2 => Pop::Abort,
        _ => Pop::Fallback,
    }
}

/// AArch64 restartable push (W7-3a). See the module docs for the sequence shape.
///
/// # Safety
/// Identical contract to [`super::seq_x86_64::push`].
#[inline]
pub(super) unsafe fn push<const LEN_OFF: usize, const BUF_OFF: usize, const CAP_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
    value: usize,
) -> Push {
    let status: u64;
    // SAFETY: see the function contract; the staging store targets logically-free
    // space and only the `len` increment publishes it.
    unsafe {
        core::arch::asm!(
            ".pushsection __rseq_cs, \"aw\"",
            ".balign 32",
            "2:",
            ".long 0, 0",
            ".quad 3f, 4f - 3f, 5f",
            ".popsection",
            "adrp {tmp}, 2b",
            "add {tmp}, {tmp}, :lo12:2b",
            "str {tmp}, [{area}, #8]",
            "3:",
            "ldr {cpu:w}, [{area}, #4]",
            "cmp {cpu}, {maxcpus}",              // bounds-check cpu < MAX_CPUS
            "bhs 6f",                            // out of range → Fallback (memory safety)
            "mul {off}, {cpu}, {stride}",
            "add {laddr}, {lbase}, {off}",
            "ldarb {t:w}, [{laddr}]",
            "cbnz {t:w}, 6f",                    // locked → Fallback
            "add {slot}, {sbase}, {off}",
            "ldr {buf}, [{slot}, {bufoff}]",
            "cbz {buf}, 6f",                     // uninitialised → Fallback
            "ldr {len:w}, [{slot}, {lenoff}]",
            "ldr {cap:w}, [{slot}, {capoff}]",
            "cmp {len:w}, {cap:w}",
            "bhs 7f",                            // len >= soft cap → Full (no commit)
            "str {val}, [{buf}, {len}, lsl #3]", // stage into the logically-free slot
            "add {len:w}, {len:w}, #1",
            // COMMIT with store-RELEASE: publishes the staged `buf[len]` before
            // the new `len` becomes visible, so any reader that acquire-observes
            // the incremented length also observes the pushed object (the weak
            // memory model would otherwise let `len` be seen with a stale slot).
            // `stlr` needs a bare base register, so form `&len` first (reusing
            // `laddr`, free after the lock-byte check).
            "add {laddr}, {slot}, {lenoff}",
            "stlr {len:w}, [{laddr}]",           // COMMIT (release): len ← len+1
            "4:",
            "mov {status:w}, #0",                // Success
            "b 8f",
            "6:",
            "mov {status:w}, #3",                // Fallback
            "b 8f",
            "7:",
            "mov {status:w}, #1",                // Full
            "b 8f",
            ".pushsection __rseq_failure, \"ax\"",
            ".long {sig}",
            "5:",
            "mov {status:w}, #2",                // Abort
            "b 8f",
            ".popsection",
            "8:",
            area = in(reg) area,
            sbase = in(reg) slot_base,
            lbase = in(reg) locked_base,
            stride = in(reg) stride,
            maxcpus = in(reg) max_cpus,
            val = in(reg) value,
            lenoff = const LEN_OFF,
            bufoff = const BUF_OFF,
            capoff = const CAP_OFF,
            sig = const RSEQ_SIG,
            tmp = out(reg) _,
            cpu = out(reg) _,
            off = out(reg) _,
            laddr = out(reg) _,
            t = out(reg) _,
            slot = out(reg) _,
            buf = out(reg) _,
            len = out(reg) _,
            cap = out(reg) _,
            status = out(reg) status,
            options(nostack),
        );
    }
    match status {
        0 => Push::Success,
        1 => Push::Full,
        2 => Push::Abort,
        _ => Push::Fallback,
    }
}

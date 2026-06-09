// SPDX-License-Identifier: MIT
//! x86-64 restartable `pop`/`push` sequences (W7-2b, W7-2c; SPEC §12.3, §33.5).
//!
//! Each function is a single inline-assembly critical section with the
//! non-negotiable RSEQ shape (DD-2):
//!
//! ```text
//!   arm:        rseq.rseq_cs = &descriptor        (outside the CS proper)
//!   start_ip:   load cpu_id from the rseq area    (so migration after this aborts)
//!               compute the (cpu, sc) slot address
//!               check the per-CPU lock byte        → Fallback (W7-4)
//!               load the buffer pointer             → Fallback if null (uninitialised)
//!               load len; bounds-check              → Empty / Full (no commit)
//!               read/stage the object
//!   commit_ip:  ONE store that publishes the new len   ← the only state change
//!   post_commit_ip:
//!   abort_ip:   (kernel jumps here on preempt/migrate/signal) → Abort
//! ```
//!
//! **The single committing store** is the `mov [slot+len_off], new_len`. Every
//! earlier instruction is a load or a register op, so an abort before the commit
//! is invisible (the [`crate::rseq::Pop`]/[`crate::rseq::Push`] frame condition,
//! plan 02 W1-7). For `push`, the object is staged into `buf[len]` — space that
//! is *logically free* until `len` is incremented — so that staging store is also
//! invisible on abort; only the `len` increment publishes it.
//!
//! **No calls, no possibly-faulting reference (§12.3).** Every memory reference
//! is to already-resident per-CPU cache metadata: the rseq area, the per-CPU lock
//! byte, the slot's `len`/`buf` fields, and `buf[i]` for an in-bounds `i`. There
//! are no calls. `options(nostack)` keeps the sequence off the stack.
//!
//! **Clobbers / barriers (W7-2d).** The asm declares every scratch register as an
//! output (`out(reg) _`); the compiler must not assume any register survives. No
//! explicit compiler fence is needed *inside* the block because a single `asm!`
//! is an atomic unit to the optimiser — it cannot reorder instructions across the
//! commit, and the surrounding `Acquire`/`Release` on the per-CPU lock and the
//! atomically-published slot fields order the sequence against the locked path.
//! On x86-64 every aligned load is acquire and every aligned store is release
//! (TSO), so the lock-byte load needs no explicit acquire fence.

use super::abi::{Rseq, RSEQ_SIG};
use super::{Pop, Push};

/// x86-64 restartable pop (W7-2b). See the module docs for the sequence shape.
///
/// # Safety
/// `area` must point at this thread's registered [`Rseq`] area. `slot_base` is
/// `&cpus[0] + slots_offset + sc * slot_stride`, `locked_base` is `&cpus[0]`
/// (the per-CPU lock byte at offset 0), and `stride` is `size_of::<PerCpu>()`,
/// describing a live per-CPU cache whose slots have `len` at `LEN_OFF` and the
/// buffer pointer at `BUF_OFF`. The buffer holds at least `len` `usize` entries.
#[inline]
pub(super) unsafe fn pop<const LEN_OFF: usize, const BUF_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
) -> Pop {
    let status: u32;
    let val: usize;
    // SAFETY: see the function contract; the sequence performs only resident
    // loads and a single committing store, with the abort handler restoring a
    // logical no-op.
    unsafe {
        core::arch::asm!(
            // ---- critical-section descriptor (start_ip, len, abort_ip) ----
            ".pushsection __rseq_cs, \"aw\"",
            ".balign 32",
            "2:",
            ".long 0, 0",                       // version, flags
            ".quad 3f, 4f - 3f, 5f",            // start_ip, post_commit_offset, abort_ip
            ".popsection",
            // ---- arm the critical section ----
            "lea {tmp}, [rip + 2b]",
            "mov [{area} + 8], {tmp}",          // rseq.rseq_cs = &descriptor
            // ---- start_ip ----
            "3:",
            "mov {cpu:e}, [{area} + 4]",        // cpu = rseq.cpu_id  (inside the CS)
            "cmp {cpu}, {maxcpus}",             // bounds-check cpu < MAX_CPUS
            "jae 6f",                           // out of range → Fallback (memory safety)
            "imul {cpu}, {stride}",             // cpu *= stride  → byte offset
            "cmp byte ptr [{lbase} + {cpu}], 0",
            "jne 6f",                           // per-CPU lock held → Fallback (W7-4)
            "lea {slot}, [{sbase} + {cpu}]",    // &slot = slot_base + offset
            "mov {buf}, [{slot} + {bufoff}]",
            "test {buf}, {buf}",
            "jz 6f",                            // uninitialised slot → Fallback
            "mov {len:e}, [{slot} + {lenoff}]",
            "test {len:e}, {len:e}",
            "jz 7f",                            // empty → Empty (no commit)
            "dec {len}",
            "mov {val}, [{buf} + {len} * 8]",   // val = buf[len-1]  (read, not commit)
            "mov [{slot} + {lenoff}], {len:e}", // COMMIT: len ← len-1
            "4:",                               // post_commit_ip
            "mov {status:e}, 0",                // Success
            "jmp 8f",
            "6:",
            "mov {status:e}, 3",                // Fallback
            "xor {val:e}, {val:e}",
            "jmp 8f",
            "7:",
            "mov {status:e}, 1",                // Empty
            "xor {val:e}, {val:e}",
            "jmp 8f",
            // ---- abort handler (signature-prefixed, in the failure section) ----
            ".pushsection __rseq_failure, \"ax\"",
            ".byte 0x0f, 0xb9, 0x3d",           // ud1: the next 4 bytes are the signature
            ".long {sig}",
            "5:",                               // abort_ip
            "mov {status:e}, 2",                // Abort
            "xor {val:e}, {val:e}",
            "jmp 8f",
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

/// x86-64 restartable push (W7-2c). See the module docs for the sequence shape.
///
/// # Safety
/// As [`pop`], plus the slot's soft-capacity field at `CAP_OFF`. The buffer holds
/// at least `cap` `usize` entries, so staging into `buf[len]` for `len < cap` is
/// in bounds.
#[inline]
pub(super) unsafe fn push<const LEN_OFF: usize, const BUF_OFF: usize, const CAP_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
    value: usize,
) -> Push {
    let status: u32;
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
            "lea {tmp}, [rip + 2b]",
            "mov [{area} + 8], {tmp}",
            "3:",
            "mov {cpu:e}, [{area} + 4]",
            "cmp {cpu}, {maxcpus}",             // bounds-check cpu < MAX_CPUS
            "jae 6f",                           // out of range → Fallback (memory safety)
            "imul {cpu}, {stride}",
            "cmp byte ptr [{lbase} + {cpu}], 0",
            "jne 6f",                           // locked → Fallback
            "lea {slot}, [{sbase} + {cpu}]",
            "mov {buf}, [{slot} + {bufoff}]",
            "test {buf}, {buf}",
            "jz 6f",                            // uninitialised → Fallback
            "mov {len:e}, [{slot} + {lenoff}]",
            "mov {cap:e}, [{slot} + {capoff}]",
            "cmp {len:e}, {cap:e}",
            "jae 7f",                           // len >= soft cap → Full (no commit)
            "mov [{buf} + {len} * 8], {val}",   // stage into the (logically free) slot
            "add {len:e}, 1",
            "mov [{slot} + {lenoff}], {len:e}", // COMMIT: len ← len+1
            "4:",
            "mov {status:e}, 0",                // Success
            "jmp 8f",
            "6:",
            "mov {status:e}, 3",                // Fallback
            "jmp 8f",
            "7:",
            "mov {status:e}, 1",                // Full
            "jmp 8f",
            ".pushsection __rseq_failure, \"ax\"",
            ".byte 0x0f, 0xb9, 0x3d",
            ".long {sig}",
            "5:",
            "mov {status:e}, 2",                // Abort
            "jmp 8f",
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

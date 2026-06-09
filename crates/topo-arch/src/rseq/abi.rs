// SPDX-License-Identifier: MIT
//! The Linux restartable-sequences (RSEQ) ABI types and constants (W7-2a, §12.3).
//!
//! These mirror `<linux/rseq.h>` exactly. The kernel and userspace share two
//! 32-byte, 32-byte-aligned structures:
//!
//! - [`Rseq`] — the per-thread area registered with the kernel via the `rseq(2)`
//!   syscall (or auto-registered by glibc ≥ 2.35). The kernel publishes the
//!   current CPU into `cpu_id`; the fast path points `rseq_cs` at the active
//!   [`RseqCs`] descriptor before entering a critical section.
//! - [`RseqCs`] — a critical-section descriptor: `start_ip`, the length of the
//!   sequence (`post_commit_offset`), and the `abort_ip` the kernel jumps to on
//!   preemption/migration/signal before the commit instruction.
//!
//! The field offsets are load-bearing: the per-architecture assembly
//! ([`super::seq_x86_64`], [`super::seq_aarch64`]) reads `cpu_id` at offset 4
//! and writes `rseq_cs` at offset 8. Both are asserted by `const` checks and by
//! unit tests, so a layout change cannot silently desynchronize the asm.

/// The RSEQ signature placed immediately before every abort handler (§12.3).
///
/// The kernel verifies that the four bytes preceding a sequence's `abort_ip`
/// equal the signature **registered for the thread**, defending against jumps
/// into the middle of a sequence. The value is **architecture-specific** — it is
/// the encoding of a trap instruction for that arch — and MUST match what glibc
/// registered with (since [`super::enable`] prefers the glibc-registered area):
///
/// * x86-64: `0x53053053` (a `ud1` variant; glibc `bits/rseq.h`).
/// * AArch64: `0xd428bc00` (`BRK #0x45E0`; glibc aarch64 `bits/rseq.h`).
///
/// A mismatch is fatal: on a real preempt/migrate/signal the kernel would find
/// the wrong bytes before `abort_ip` and deliver `SIGSEGV` instead of jumping to
/// the abort handler. For self-registration we pass this same value to `rseq(2)`,
/// so both modes stay consistent.
#[cfg(target_arch = "x86_64")]
pub const RSEQ_SIG: u32 = 0x5305_3053;
/// See [`RSEQ_SIG`] (x86-64). AArch64 value: `BRK #0x45E0`.
#[cfg(target_arch = "aarch64")]
pub const RSEQ_SIG: u32 = 0xd428_bc00;
/// See [`RSEQ_SIG`]. Other architectures have no RSEQ fast path; the value is
/// unused (kept defined so the shared code compiles).
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub const RSEQ_SIG: u32 = 0x5305_3053;

/// `cpu_id` value before the kernel has updated the area (`RSEQ_CPU_ID_UNINITIALIZED`).
pub const RSEQ_CPU_ID_UNINITIALIZED: i32 = -1;

/// `cpu_id` value when registration failed (`RSEQ_CPU_ID_REGISTRATION_FAILED`).
pub const RSEQ_CPU_ID_REGISTRATION_FAILED: i32 = -2;

/// Flag passed to `rseq(2)` to *unregister* a previously registered area.
pub const RSEQ_FLAG_UNREGISTER: i32 = 1;

/// The per-thread RSEQ area (`struct rseq`, `<linux/rseq.h>`).
///
/// 32-byte aligned and (at least) 32 bytes, matching the kernel ABI. Only
/// `cpu_id` (read) and `rseq_cs` (written by the fast path, cleared by the
/// kernel) are touched by this crate; the remaining fields are present so the
/// structure is the exact size and alignment the kernel expects on registration.
#[repr(C, align(32))]
#[derive(Debug)]
pub struct Rseq {
    /// A snapshot of `cpu_id` the kernel keeps in sync; unused here.
    pub cpu_id_start: u32, // offset 0
    /// The current CPU, published by the kernel. Read by the fast path.
    pub cpu_id: u32, // offset 4
    /// Pointer to the active [`RseqCs`] descriptor (set by the fast path,
    /// cleared by the kernel on abort/commit). `0` when no sequence is active.
    pub rseq_cs: u64, // offset 8
    /// Per-thread flags (unused; the modern kernel ignores the deprecated
    /// per-thread restart-inhibit flags).
    pub flags: u32, // offset 16
    /// The current NUMA node, published by the kernel (newer kernels).
    pub node_id: u32, // offset 20
    /// The current memory-map concurrency id, published by the kernel (newer
    /// kernels). Padding to the 32-byte ABI size on older ones.
    pub mm_cid: u32, // offset 24
    /// Explicit tail padding to the 32-byte ABI size.
    pub padding: u32, // offset 28
}

impl Rseq {
    /// A zeroed area (the registration-time initial value; `cpu_id_start`/
    /// `cpu_id` are overwritten by the kernel on the first preemption).
    pub const fn zeroed() -> Self {
        Self {
            cpu_id_start: 0,
            cpu_id: RSEQ_CPU_ID_UNINITIALIZED as u32,
            rseq_cs: 0,
            flags: 0,
            node_id: 0,
            mm_cid: 0,
            padding: 0,
        }
    }

    /// The byte offset of `cpu_id` (read by the asm). Must be 4 (kernel ABI).
    pub const CPU_ID_OFFSET: usize = 4;
    /// The byte offset of `rseq_cs` (written by the asm). Must be 8 (kernel ABI).
    pub const RSEQ_CS_OFFSET: usize = 8;
}

/// A critical-section descriptor (`struct rseq_cs`, `<linux/rseq.h>`).
///
/// One per sequence body, emitted by the asm into a dedicated section. The
/// kernel reads it via the area's `rseq_cs` pointer: if a preemption/migration/
/// signal occurs while the instruction pointer is in
/// `[start_ip, start_ip + post_commit_offset)`, it restarts the thread at
/// `abort_ip`. 32-byte aligned and 32 bytes, matching the kernel ABI.
#[repr(C, align(32))]
#[derive(Debug)]
pub struct RseqCs {
    /// Descriptor version (always 0).
    pub version: u32, // offset 0
    /// Descriptor flags (always 0; the modern kernel restarts on all of
    /// preempt/signal/migrate).
    pub flags: u32, // offset 4
    /// The address of the first instruction of the sequence.
    pub start_ip: u64, // offset 8
    /// The length of the sequence in bytes (`post_commit_ip - start_ip`).
    pub post_commit_offset: u64, // offset 16
    /// The address the kernel restarts the thread at on abort.
    pub abort_ip: u64, // offset 24
}

// Compile-time guards: the kernel ABI fixes these offsets and sizes. A field
// reorder or padding change that breaks the asm fails the build here.
const _: () = {
    assert!(core::mem::size_of::<Rseq>() == 32);
    assert!(core::mem::align_of::<Rseq>() == 32);
    assert!(core::mem::size_of::<RseqCs>() == 32);
    assert!(core::mem::align_of::<RseqCs>() == 32);
    assert!(Rseq::CPU_ID_OFFSET == 4);
    assert!(Rseq::RSEQ_CS_OFFSET == 8);
    // `cpu_id` and `rseq_cs` must sit where the asm hard-codes them.
    assert!(core::mem::offset_of!(Rseq, cpu_id) == Rseq::CPU_ID_OFFSET);
    assert!(core::mem::offset_of!(Rseq, rseq_cs) == Rseq::RSEQ_CS_OFFSET);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rseq_layout_matches_kernel_abi() {
        assert_eq!(core::mem::size_of::<Rseq>(), 32);
        assert_eq!(core::mem::align_of::<Rseq>(), 32);
        assert_eq!(core::mem::offset_of!(Rseq, cpu_id_start), 0);
        assert_eq!(core::mem::offset_of!(Rseq, cpu_id), 4);
        assert_eq!(core::mem::offset_of!(Rseq, rseq_cs), 8);
        assert_eq!(core::mem::offset_of!(Rseq, flags), 16);
    }

    #[test]
    fn rseq_cs_layout_matches_kernel_abi() {
        assert_eq!(core::mem::size_of::<RseqCs>(), 32);
        assert_eq!(core::mem::align_of::<RseqCs>(), 32);
        assert_eq!(core::mem::offset_of!(RseqCs, version), 0);
        assert_eq!(core::mem::offset_of!(RseqCs, flags), 4);
        assert_eq!(core::mem::offset_of!(RseqCs, start_ip), 8);
        assert_eq!(core::mem::offset_of!(RseqCs, post_commit_offset), 16);
        assert_eq!(core::mem::offset_of!(RseqCs, abort_ip), 24);
    }

    #[test]
    fn signature_matches_the_arch_glibc_value() {
        // Interop with glibc's registration depends on the *architecture-specific*
        // value; a wrong value would make a real abort SIGSEGV the process.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(RSEQ_SIG, 0x5305_3053);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(RSEQ_SIG, 0xd428_bc00);
    }

    #[test]
    fn zeroed_area_is_uninitialized() {
        let area = Rseq::zeroed();
        assert_eq!(area.cpu_id as i32, RSEQ_CPU_ID_UNINITIALIZED);
        assert_eq!(area.rseq_cs, 0);
    }
}

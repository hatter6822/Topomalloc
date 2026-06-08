// SPDX-License-Identifier: MIT
//! Linux restartable-sequences (RSEQ) support for the per-CPU fast path
//! (W7, plan 05; SPEC §12, §27.4, §33.5).
//!
//! RSEQ makes per-CPU cache `pop`/`push` lock-free and atomic-free in the common
//! case: a short critical section the kernel **restarts** if the thread is
//! preempted or migrated before its single committing store. It is an *optional
//! acceleration* (§12.1) — when it is unavailable the caller uses the locked
//! per-CPU baseline (W6-4), which this module never replaces, only fronts (P-003).
//!
//! ## Layers
//!
//! - the ABI layer ([`Rseq`] / [`RseqCs`]) — the kernel-shared `struct rseq` /
//!   `struct rseq_cs` layout (W7-2a).
//! - registration & detection ([`enable`], [`register_current_thread`],
//!   [`current_cpu`], [`current_area`]) — glibc-area and self-registration
//!   models (W7-1), Linux-only; every other target reports unavailable.
//! - the restartable sequences ([`pop`], [`push`]) — per-architecture inline
//!   assembly for x86-64 (W7-2b/c) and AArch64 (W7-3a), behind one signature.
//! - the non-owner fence ([`fence_rseq`], W7-4).
//!
//! ## The four-way outcome (front-end contract, §33.5)
//!
//! [`Pop`]/[`Push`] mirror the SPEC's distinct outcomes. **`Abort`** (preempted/
//! migrated — retry, state unchanged) is deliberately separate from `Empty`/`Full`
//! (genuine underflow/overflow — slow path). [`Pop::Fallback`]/[`Push::Fallback`]
//! is an implementation outcome meaning "this sequence cannot run right now — use
//! the locked path" (the per-CPU lock is held by a non-owner, W7-4, or the slot
//! is not yet initialised); it is never surfaced as a front-end result.

mod abi;

pub use abi::{
    Rseq, RseqCs, RSEQ_CPU_ID_REGISTRATION_FAILED, RSEQ_CPU_ID_UNINITIALIZED, RSEQ_FLAG_UNREGISTER,
    RSEQ_SIG,
};

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod imp_linux;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod seq_aarch64;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod seq_x86_64;

/// The outcome of a restartable [`pop`] (the §33.5 distinct outcomes plus the
/// implementation-internal [`Fallback`](Pop::Fallback)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pop {
    /// Popped `addr`; the committing store published the decremented length.
    Success(usize),
    /// The per-CPU slot was genuinely empty (no commit) — caller refills.
    Empty,
    /// Preempted/migrated before the commit; state unchanged — caller retries.
    Abort,
    /// The sequence cannot run now (per-CPU lock held, or slot uninitialised) —
    /// caller takes the locked path. Never a front-end result.
    Fallback,
}

/// The outcome of a restartable [`push`]. See [`Pop`] for the variants' meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// Pushed the object; the committing store published the incremented length.
    Success,
    /// The per-CPU slot was at soft capacity (no commit) — caller flushes.
    Full,
    /// Preempted/migrated before the commit; state unchanged — caller retries.
    Abort,
    /// The sequence cannot run now — caller takes the locked path.
    Fallback,
}

/// The process-global RSEQ registration model selected by [`enable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// RSEQ is not in use (no kernel/glibc support, or the membarrier fence is
    /// unavailable). The caller uses the locked baseline (P-003).
    Unavailable,
    /// glibc auto-registered the per-thread area; we read it at `TP + offset`.
    GlibcArea,
    /// We registered our own per-thread area (the `std` self-registration path).
    SelfRegistered,
}

/// Enable RSEQ mode for the process (idempotent, W7-1). Detects the registration
/// model and the non-owner fence (W7-4). Returns whether the fast path is usable;
/// when `false`, the caller stays on the locked baseline (P-003).
#[inline]
pub fn enable() -> bool {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::enable()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        false
    }
}

/// Whether the RSEQ fast path is usable (after [`enable`]).
#[inline]
pub fn available() -> bool {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::available()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        false
    }
}

/// The selected registration model (after [`enable`]).
#[inline]
pub fn mode() -> Mode {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::mode()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        Mode::Unavailable
    }
}

/// Register the calling thread for the RSEQ fast path (§27.6, W7-1). Must be
/// called at thread start before the fast path is used in self-registration
/// mode; a no-op beyond a presence check in glibc mode. Returns whether this
/// thread can use the fast path.
#[inline]
pub fn register_current_thread() -> bool {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::register_current_thread()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        false
    }
}

/// Best-effort unregister of the calling thread's self-registered area (for
/// tests and thread teardown). A no-op in glibc mode.
#[inline]
pub fn unregister_current_thread() {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::unregister_current_thread();
    }
}

/// This thread's rseq area pointer, or null if RSEQ is unavailable/unregistered.
#[inline]
pub fn current_area() -> *mut Rseq {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::current_area()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        core::ptr::null_mut()
    }
}

/// The current CPU from this thread's rseq area, or `-1` if RSEQ is unavailable
/// or this thread is not registered.
#[inline]
pub fn current_cpu() -> i32 {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::current_cpu()
    }
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        -1
    }
}

/// The W7-4 non-owner fence: abort any in-flight rseq critical section on other
/// CPUs (§27.4). The caller must already hold the per-CPU lock for the slots it
/// will touch, so that after this returns no sequence can commit to them.
#[inline]
pub fn fence_rseq() {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        imp_linux::fence_rseq();
    }
}

/// Restartable per-CPU pop (W7-2b/W7-3a). Reads the current CPU inside the
/// critical section, locates `(cpu, sc)`'s slot, and pops one object with a
/// single committing store; an abort before the commit leaves all state
/// unchanged (the §33.5 frame condition).
///
/// `LEN_OFF`/`BUF_OFF` are the byte offsets of the slot's length and buffer
/// pointer; `slot_base = &cpus[0] + slots_offset + sc * slot_stride`;
/// `locked_base = &cpus[0]` (the per-CPU lock byte, offset 0); `stride =
/// size_of::<PerCpu>()`; `max_cpus` is the length of the per-CPU array — the
/// sequence bounds-checks the kernel-reported CPU against it (memory safety on
/// machines with more cores than the array holds).
///
/// # Safety
/// `area` must be this thread's registered area and the layout arguments must
/// describe a live per-CPU cache of `max_cpus` entries (see [`seq_x86_64::pop`](self) docs).
#[inline]
pub unsafe fn pop<const LEN_OFF: usize, const BUF_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
) -> Pop {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    // SAFETY: forwarded contract.
    return unsafe {
        seq_x86_64::pop::<LEN_OFF, BUF_OFF>(area, slot_base, locked_base, stride, max_cpus)
    };
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    // SAFETY: forwarded contract.
    return unsafe {
        seq_aarch64::pop::<LEN_OFF, BUF_OFF>(area, slot_base, locked_base, stride, max_cpus)
    };
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        let _ = (area, slot_base, locked_base, stride, max_cpus);
        Pop::Fallback
    }
}

/// Restartable per-CPU push (W7-2c/W7-3a). Stages the object into the
/// logically-free slot at `buf[len]` and publishes it with a single committing
/// store of the incremented length; an abort before the commit is invisible.
///
/// `CAP_OFF` is the byte offset of the slot's soft-capacity field; the other
/// arguments are as in [`pop`].
///
/// # Safety
/// As [`pop`], plus the soft-capacity field at `CAP_OFF` and a buffer of at
/// least `cap` entries.
#[inline]
pub unsafe fn push<const LEN_OFF: usize, const BUF_OFF: usize, const CAP_OFF: usize>(
    area: *mut Rseq,
    slot_base: *const u8,
    locked_base: *const u8,
    stride: usize,
    max_cpus: usize,
    value: usize,
) -> Push {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    // SAFETY: forwarded contract.
    return unsafe {
        seq_x86_64::push::<LEN_OFF, BUF_OFF, CAP_OFF>(
            area,
            slot_base,
            locked_base,
            stride,
            max_cpus,
            value,
        )
    };
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    // SAFETY: forwarded contract.
    return unsafe {
        seq_aarch64::push::<LEN_OFF, BUF_OFF, CAP_OFF>(
            area,
            slot_base,
            locked_base,
            stride,
            max_cpus,
            value,
        )
    };
    #[cfg(not(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )))]
    {
        let _ = (area, slot_base, locked_base, stride, max_cpus, value);
        Push::Fallback
    }
}

// SPDX-License-Identifier: MIT
//! Linux OS interface for RSEQ (W7-1, W7-4): raw `rseq(2)`/`membarrier(2)`
//! syscalls, glibc-area detection, per-thread self-registration, and the
//! non-owner coordination fence. Gated to Linux on x86-64/AArch64; every other
//! target uses the [`super`] fallback that reports RSEQ unavailable (P-003).
//!
//! ## Registration models (W7-1, "support both")
//!
//! 1. **glibc area** (`target_env = "gnu"`, glibc ≥ 2.35): glibc auto-registers
//!    an `rseq` area per thread and exports `__rseq_offset`/`__rseq_size`. We use
//!    that area at `thread_pointer + __rseq_offset` — a direct register read, no
//!    TLS of our own, no allocation (the DD-4-correct path on the supported
//!    targets). This is the only path active on the `-gnu` CI targets.
//! 2. **self-registration** (`feature = "std"`, e.g. musl or glibc < 2.35): we
//!    register our own area, stored in a `const`-initialised `thread_local!`
//!    (stable, Local-Exec in an executable — measured). Registration is
//!    **explicit at thread start** ([`register_current_thread`]), never lazily on
//!    the malloc path, so the fallback never re-enters the allocator (DD-4).
//!
//! ## Non-owner coordination (W7-4)
//!
//! RSEQ mode is enabled only when `membarrier(MEMBARRIER_CMD_*_RSEQ)` is
//! available, because a non-owner draining an idle CPU's slots relies on it: it
//! takes the per-CPU lock (so new sequences see the lock byte and divert) and
//! then [`fence_rseq`] forces any *in-flight* sequence on another CPU to abort
//! before it commits. Lock-byte-in-CS + fence together give the non-owner
//! exclusive access (§27.4). Requiring the fence keeps the coordination sound on
//! every kernel where RSEQ mode runs at all.

use core::sync::atomic::{AtomicU8, Ordering};

use super::abi::{Rseq, RSEQ_SIG};
use super::Mode;

// ---------------------------------------------------------------------------
// Syscall numbers (asm-generic for AArch64; x86-64 table).

#[cfg(target_arch = "x86_64")]
const NR_RSEQ: usize = 334;
#[cfg(target_arch = "x86_64")]
const NR_MEMBARRIER: usize = 324;
#[cfg(target_arch = "aarch64")]
const NR_RSEQ: usize = 293;
#[cfg(target_arch = "aarch64")]
const NR_MEMBARRIER: usize = 283;

/// `MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ` (`1 << 7`): force in-flight rseq
/// critical sections on other CPUs to abort, with full memory ordering.
const MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ: usize = 1 << 7;
/// `MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ` (`1 << 8`): register the
/// process's intent to use the RSEQ-expedited fence (required first; Linux ≥ 5.10).
const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ: usize = 1 << 8;

/// `EBUSY`: an rseq area is already registered for this thread (e.g. by glibc).
const EBUSY: isize = 16;

// ---------------------------------------------------------------------------
// Raw syscalls (no libc dependency; keeps the crate `no_std`).

/// A 4-argument syscall. Returns the kernel result (`-errno` on error).
///
/// # Safety
/// The caller must uphold the syscall's own contract (valid pointers, etc.).
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    // SAFETY: `syscall` with the System V Linux syscall ABI; rcx/r11 are clobbered
    // by the instruction and declared as such.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// A 4-argument syscall. Returns the kernel result (`-errno` on error).
///
/// # Safety
/// The caller must uphold the syscall's own contract (valid pointers, etc.).
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    // SAFETY: `svc #0` with the AArch64 Linux syscall ABI; the kernel preserves
    // all but x0-x7.
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") nr,
            inlateout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// Register `area` with the kernel as this thread's rseq area. `0` on success.
unsafe fn sys_rseq_register(area: *mut Rseq) -> isize {
    // SAFETY: `area` is a valid 32-byte rseq area for the calling thread.
    unsafe {
        syscall4(
            NR_RSEQ,
            area as usize,
            core::mem::size_of::<Rseq>(),
            0,
            RSEQ_SIG as usize,
        )
    }
}

/// Best-effort unregister of `area` (`RSEQ_FLAG_UNREGISTER`). `0` on success.
unsafe fn sys_rseq_unregister(area: *mut Rseq) -> isize {
    // SAFETY: `area` is the area previously registered by this thread.
    unsafe {
        syscall4(
            NR_RSEQ,
            area as usize,
            core::mem::size_of::<Rseq>(),
            super::abi::RSEQ_FLAG_UNREGISTER as usize,
            RSEQ_SIG as usize,
        )
    }
}

/// `membarrier(cmd, 0, 0)`. `0` on success, `-errno` otherwise.
fn sys_membarrier(cmd: usize) -> isize {
    // SAFETY: membarrier takes scalar arguments only; no memory is dereferenced.
    unsafe { syscall4(NR_MEMBARRIER, cmd, 0, 0, 0) }
}

// ---------------------------------------------------------------------------
// Thread pointer + glibc area.

/// Read the thread pointer (the base the rseq area offset is relative to).
#[cfg(target_arch = "x86_64")]
#[inline]
fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: `%fs:0` holds the x86-64 TCB self-pointer (= the thread pointer).
    unsafe {
        core::arch::asm!("mov {}, fs:0", out(reg) tp, options(nostack, preserves_flags, readonly));
    }
    tp
}

/// Read the thread pointer (the base the rseq area offset is relative to).
#[cfg(target_arch = "aarch64")]
#[inline]
fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: `tpidr_el0` is the AArch64 user-space thread pointer.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp, options(nostack, nomem, preserves_flags));
    }
    tp
}

// glibc ≥ 2.35 exports these; the rseq area lives at `thread_pointer + __rseq_offset`
// when `__rseq_size != 0`. They are referenced only on `target_env = "gnu"`.
#[cfg(target_env = "gnu")]
extern "C" {
    static __rseq_offset: isize;
    static __rseq_size: u32;
}

/// The size glibc registered its rseq area with (0 ⇒ glibc did not register).
#[cfg(target_env = "gnu")]
fn glibc_rseq_size() -> u32 {
    // SAFETY: a plain read of a glibc-exported `unsigned int`.
    unsafe { __rseq_size }
}
#[cfg(not(target_env = "gnu"))]
fn glibc_rseq_size() -> u32 {
    0
}

/// glibc's rseq area for the current thread, or null if glibc did not register.
#[cfg(target_env = "gnu")]
fn glibc_area() -> *mut Rseq {
    if glibc_rseq_size() < 20 {
        return core::ptr::null_mut();
    }
    // SAFETY: a plain read of a glibc-exported `ptrdiff_t`.
    let off = unsafe { __rseq_offset };
    (thread_pointer() as isize + off) as *mut Rseq
}
#[cfg(not(target_env = "gnu"))]
fn glibc_area() -> *mut Rseq {
    core::ptr::null_mut()
}

// ---------------------------------------------------------------------------
// Self-registration storage (std only; explicit-init, const-initialised TLS).

#[cfg(any(test, feature = "std"))]
mod selfreg {
    use super::Rseq;
    use core::cell::UnsafeCell;

    // `const {}` init keeps this a Local-Exec slot in an executable (no lazy
    // guard, no destructor allocation — measured). In a `cdylib` it is General
    // Dynamic, which is why registration is explicit at thread start (DD-4).
    std::thread_local! {
        static AREA: UnsafeCell<Rseq> = const { UnsafeCell::new(Rseq::zeroed()) };
    }

    /// This thread's self-registration area pointer (stable for the thread).
    pub(super) fn area_ptr() -> *mut Rseq {
        AREA.with(|a| a.get())
    }
}

/// This thread's self-registration area, or null when self-registration storage
/// is unavailable (no `std`).
fn selfreg_area() -> *mut Rseq {
    #[cfg(any(test, feature = "std"))]
    {
        selfreg::area_ptr()
    }
    #[cfg(not(any(test, feature = "std")))]
    {
        core::ptr::null_mut()
    }
}

// ---------------------------------------------------------------------------
// Mode state machine.

const MODE_UNKNOWN: u8 = 0;
const MODE_UNAVAILABLE: u8 = 1;
const MODE_GLIBC: u8 = 2;
const MODE_SELFREG: u8 = 3;

static MODE: AtomicU8 = AtomicU8::new(MODE_UNKNOWN);

fn mode_to_enum(m: u8) -> Mode {
    match m {
        MODE_GLIBC => Mode::GlibcArea,
        MODE_SELFREG => Mode::SelfRegistered,
        _ => Mode::Unavailable,
    }
}

/// The process-global RSEQ mode after [`enable`].
pub(super) fn mode() -> Mode {
    mode_to_enum(MODE.load(Ordering::Acquire))
}

/// Whether the RSEQ fast path is usable (after [`enable`]).
pub(super) fn available() -> bool {
    matches!(MODE.load(Ordering::Acquire), MODE_GLIBC | MODE_SELFREG)
}

/// Enable RSEQ mode for the process (idempotent, W7-1). Detects the registration
/// model and registers the membarrier intent the non-owner fence needs (W7-4).
/// Returns whether the fast path is usable.
pub(super) fn enable() -> bool {
    let cur = MODE.load(Ordering::Acquire);
    if cur != MODE_UNKNOWN {
        return matches!(cur, MODE_GLIBC | MODE_SELFREG);
    }

    let decided = decide_mode();
    // Publish the decision; if another thread won the race, adopt its value.
    let winner =
        match MODE.compare_exchange(MODE_UNKNOWN, decided, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => decided,
            Err(other) => other,
        };
    matches!(winner, MODE_GLIBC | MODE_SELFREG)
}

/// Decide the registration model. The membarrier-RSEQ intent is required (W7-4):
/// without it the non-owner coordination is unsound, so we report unavailable.
fn decide_mode() -> u8 {
    // The fence must be registerable AND actually work for the non-owner
    // coordination to be sound (W7-4). Validate both here so a runtime fence is
    // reliable (a failure post-registration would be a kernel anomaly).
    if sys_membarrier(MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ) < 0 {
        return MODE_UNAVAILABLE;
    }
    if sys_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ) < 0 {
        return MODE_UNAVAILABLE; // registered but the fence itself fails
    }
    // glibc already registered every thread's area: use it.
    if !glibc_area().is_null() {
        return MODE_GLIBC;
    }
    // Otherwise self-register the calling thread as the availability probe.
    let area = selfreg_area();
    if area.is_null() {
        return MODE_UNAVAILABLE; // no self-registration storage (no std)
    }
    // SAFETY: `area` is this thread's fresh, stable self-registration area.
    let r = unsafe { sys_rseq_register(area) };
    if r == 0 || r == -EBUSY {
        MODE_SELFREG
    } else {
        MODE_UNAVAILABLE
    }
}

/// Register the calling thread for the RSEQ fast path (§27.6, W7-1). Idempotent.
/// In glibc mode the area is already registered; in self-registration mode this
/// performs the per-thread `rseq(2)`. Returns whether this thread can use the
/// fast path.
pub(super) fn register_current_thread() -> bool {
    match MODE.load(Ordering::Acquire) {
        MODE_GLIBC => !glibc_area().is_null(),
        MODE_SELFREG => {
            let area = selfreg_area();
            if area.is_null() {
                return false;
            }
            // SAFETY: `area` is this thread's stable self-registration area;
            // EBUSY means it was already registered (idempotent).
            let r = unsafe { sys_rseq_register(area) };
            r == 0 || r == -EBUSY
        }
        _ => false,
    }
}

/// Best-effort unregister of the calling thread's self-registered area (tests).
/// A no-op in glibc mode (glibc owns the area).
pub(super) fn unregister_current_thread() {
    if MODE.load(Ordering::Acquire) == MODE_SELFREG {
        let area = selfreg_area();
        if !area.is_null() {
            // SAFETY: unregistering this thread's own area.
            unsafe {
                let _ = sys_rseq_unregister(area);
            }
        }
    }
}

/// This thread's rseq area pointer, or null if RSEQ is unavailable.
pub(super) fn current_area() -> *mut Rseq {
    match MODE.load(Ordering::Acquire) {
        MODE_GLIBC => glibc_area(),
        MODE_SELFREG => selfreg_area(),
        _ => core::ptr::null_mut(),
    }
}

/// The current CPU read from an already-resolved `area`, or `-1` if `area` is
/// null. Lets the hot path resolve the area once and avoid a second thread-pointer
/// read (`current_cpu` would recompute it).
#[inline]
pub(super) fn cpu_of(area: *mut Rseq) -> i32 {
    if area.is_null() {
        return -1;
    }
    // The kernel writes `cpu_id` concurrently, so read it as an atomic (Relaxed):
    // a plain read would be a data race. The field is an aligned `u32`.
    // SAFETY: `area` is a valid rseq area, so `&(*area).cpu_id` is a valid,
    // aligned `u32` the kernel keeps current; reinterpreting it as `AtomicU32`
    // for a relaxed load is sound (same layout, no tearing on aligned `u32`).
    unsafe {
        let cpu_ptr = core::ptr::addr_of!((*area).cpu_id).cast::<core::sync::atomic::AtomicU32>();
        (*cpu_ptr).load(core::sync::atomic::Ordering::Relaxed) as i32
    }
}

/// The current CPU from this thread's rseq area, or `-1` if unavailable or the
/// area is not yet kernel-initialised (e.g. an unregistered thread).
#[inline]
pub(super) fn current_cpu() -> i32 {
    cpu_of(current_area())
}

/// The W7-4 non-owner fence: abort any in-flight rseq critical section on other
/// CPUs. The caller must already hold the per-CPU lock for the slots it will
/// touch, so that after this returns no sequence can commit to them. Returns
/// whether the fence succeeded — it is validated at [`enable`] time, so `false`
/// here would be a kernel anomaly the caller should treat as fatal in debug.
pub(super) fn fence_rseq() -> bool {
    sys_membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ) >= 0
}

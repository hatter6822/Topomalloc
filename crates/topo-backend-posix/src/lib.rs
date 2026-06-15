// SPDX-License-Identifier: MIT
//! `PosixBackingProvider` — the degenerate single ambient-authority,
//! single-label case of the §36.6 backing-provider contract (overview §3, D2).
//!
//! On **unix** this is the *real* provider: `mmap` reserves virtual memory,
//! `madvise(MADV_DONTNEED/MADV_FREE)` and `mprotect` realise the §20.1
//! dirty/muzzy/released states, and `munmap` returns memory to the OS — exactly
//! the §18.1/§20.4 responsibilities. On a non-unix target it falls back to a
//! host-allocator-backed store with the same observable behaviour, so the crate
//! still builds and tests run everywhere.
//!
//! ## Physical-state mapping (§20.4, W4-3a) — documented per platform
//!
//! TopoMalloc's memory states (§20.1) map to OS primitives as follows. On unix the
//! provider issues the real syscall; the table is the contract:
//!
//! | TopoMalloc op (§20.4) | TopoMalloc state | Linux / other unix | Darwin (macOS/iOS) |
//! |---|---|---|---|
//! | `reserve` | reserved | `mmap(PROT_READ\|WRITE)` (or `PROT_NONE` in guard mode) | `mmap` |
//! | `commit` | committed | no-op (already mapped RW); `mprotect(PROT_READ\|WRITE)` in guard mode | `mprotect` |
//! | `decommit` | released | `madvise(MADV_DONTNEED)` (+ `mprotect(PROT_NONE)` in guard mode) | `madvise(MADV_FREE_REUSABLE)` |
//! | `purge_lazy` | muzzy | `madvise(MADV_FREE)` (lazy; old data may persist) | `madvise(MADV_FREE)` |
//! | `purge_forced` | muzzy (scrubbed) | `madvise(MADV_DONTNEED)` | `madvise(MADV_FREE_REUSABLE)` |
//! | `release` (whole region) | released/recycled | `munmap` | `munmap` |
//! | `revoke_descendants` | — | n/a (single ambient authority) | n/a |
//!
//! On Linux `MADV_DONTNEED` drops the pages immediately (RSS falls now; a later
//! read faults a fresh **zero** page), whereas `MADV_FREE` is lazy (the kernel
//! reclaims under pressure and the old contents survive until then) — exactly the
//! §20.1 dirty-vs-muzzy distinction. On Darwin the discard-now ops (`decommit`,
//! `purge_forced`) use `MADV_FREE_REUSABLE` instead, because Darwin's `MADV_DONTNEED`
//! is advisory and does not reclaim (the `MADV_DISCARD_NOW` split below). Correctness
//! does not depend on the post-discard read value: M-005 requires a recommit before
//! any reuse. The Apple branch is compiled but not exercised by CI (which runs Linux).
//!
//! ## Guard mode (§20.5, W4-3b — catch use-after-free)
//!
//! Constructed with [`PosixBackingProvider::with_guard_pages`], the provider
//! reserves `PROT_NONE` and `mprotect`s ranges `PROT_READ|WRITE` on `commit` /
//! `PROT_NONE` on `decommit`. A stray access to a decommitted (released) range
//! then **faults** rather than silently reading a stale/zero page — the §20.5
//! use-after-free trap the unmap-aggressive (`debug`/`low-rss`) profiles want. The
//! default ([`new`](PosixBackingProvider::new)) is the performance mode:
//! `mmap(PROT_READ|WRITE)`, `commit` a no-op, `decommit` a plain `madvise`.
//!
//! The byte-level dirty/muzzy/released accounting that reconciles in stats (plan
//! 07) lives in the back-end's [`ExtentManager`](topo_core::ExtentManager)
//! (`committed_bytes`/`state_bytes`); per-op counters here let a test confirm the
//! §20.4 mapping is exercised.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use topo_core::{ArenaId, BackendError, Region, TopoBackingProvider, Topology};

/// A live reservation owned by the provider: its base, the bytes mapped (for
/// `munmap`/free — `>=` the requested size, rounded to the OS page), and the
/// requested alignment (for the host fallback's `Layout`).
#[derive(Clone, Copy)]
struct Owned {
    base: usize,
    mapped_len: usize,
    align: usize,
}

/// POSIX backing provider. On unix it owns `mmap` reservations; on other targets a
/// host-allocator store. The §20.4 physical-state operations realise the
/// documented `madvise`/`mprotect` mapping (see the module docs); guard mode
/// (§20.5) traps use-after-free.
pub struct PosixBackingProvider {
    owned: Mutex<Vec<Owned>>,
    /// Guard mode: `mprotect` on commit/decommit so a released range faults (§20.5).
    guard: bool,
    commit_calls: AtomicU64,
    decommit_calls: AtomicU64,
    purge_lazy_calls: AtomicU64,
    purge_forced_calls: AtomicU64,
    bind_node_calls: AtomicU64,
}

impl Default for PosixBackingProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PosixBackingProvider {
    /// Create a provider in the **performance** mode (§20.5 retain): `mmap`
    /// read-write, `commit` is a no-op, `decommit` is a plain `madvise(DONTNEED)`.
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Create a provider in **guard mode** (§20.5, W4-3b): committed ranges are
    /// `mprotect`ed read-write and decommitted ranges `PROT_NONE`, so a
    /// use-after-free **faults** instead of reading stale memory. Pairs with the
    /// extent manager's `RetainPolicy::Unmap` for the `debug`/`low-rss` profiles.
    pub fn with_guard_pages() -> Self {
        Self::build(true)
    }

    fn build(guard: bool) -> Self {
        Self {
            owned: Mutex::new(Vec::new()),
            guard,
            commit_calls: AtomicU64::new(0),
            decommit_calls: AtomicU64::new(0),
            purge_lazy_calls: AtomicU64::new(0),
            purge_forced_calls: AtomicU64::new(0),
            bind_node_calls: AtomicU64::new(0),
        }
    }

    /// Whether the provider is in guard mode (§20.5).
    pub fn guards_use_after_free(&self) -> bool {
        self.guard
    }

    /// Number of `commit` calls (observability — confirms the §20.4 mapping runs).
    pub fn commit_calls(&self) -> u64 {
        self.commit_calls.load(Ordering::Relaxed)
    }
    /// Number of `decommit` calls.
    pub fn decommit_calls(&self) -> u64 {
        self.decommit_calls.load(Ordering::Relaxed)
    }
    /// Number of `purge_lazy` calls.
    pub fn purge_lazy_calls(&self) -> u64 {
        self.purge_lazy_calls.load(Ordering::Relaxed)
    }
    /// Number of `purge_forced` calls.
    pub fn purge_forced_calls(&self) -> u64 {
        self.purge_forced_calls.load(Ordering::Relaxed)
    }
    /// Number of `bind_node` (NUMA `mbind`) calls — observability for the §15.5 path.
    pub fn bind_node_calls(&self) -> u64 {
        self.bind_node_calls.load(Ordering::Relaxed)
    }

    /// Validate that `[offset, offset+len)` lies within `region` and return the
    /// sub-range's start address. A request outside the region is a caller bug (the
    /// extent manager always passes an in-range sub-extent); reject it rather than
    /// computing an out-of-bounds pointer.
    fn checked_subrange(region: Region, offset: usize, len: usize) -> Result<usize, BackendError> {
        let end = offset
            .checked_add(len)
            .ok_or(BackendError::InvalidRequest)?;
        if end > region.len {
            return Err(BackendError::InvalidRequest);
        }
        Ok((region.base as usize).wrapping_add(offset))
    }
}

impl TopoBackingProvider for PosixBackingProvider {
    fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(BackendError::InvalidRequest);
        }
        // SAFETY: `size > 0` and `align` is a power of two (checked); `sys::reserve`
        // upholds its own contract and returns a valid base + mapped length or an
        // error, leaking nothing on failure.
        let (base, mapped_len) = unsafe { sys::reserve(size, align, self.guard)? };
        self.owned
            .lock()
            .expect("provider mutex poisoned")
            .push(Owned {
                base,
                mapped_len,
                align,
            });
        Ok(Region {
            base: base as *mut u8,
            len: size,
        })
    }

    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 commit / recommit (M-005). Performance mode: a no-op (the mapping is
        // already RW). Guard mode: `mprotect(PROT_READ|WRITE)` so the range is usable
        // again after a guarded decommit.
        let addr = Self::checked_subrange(region, offset, len)?;
        self.commit_calls.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `[offset, offset+len)` is in bounds of `region`'s mapping; `addr`
        // is OS-page-aligned (extent offsets/lengths are PAGE_SIZE multiples).
        unsafe { sys::commit(addr, len, self.guard) }
    }

    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 decommit ≈ MADV_DONTNEED: drop the pages now (RSS falls; a later read
        // faults a fresh zero page). Guard mode also `mprotect(PROT_NONE)`s so a
        // use-after-free faults (§20.5).
        let addr = Self::checked_subrange(region, offset, len)?;
        self.decommit_calls.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: in-bounds, OS-page-aligned sub-range of `region`'s mapping.
        unsafe { sys::decommit(addr, len, self.guard) }
    }

    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_lazy ≈ MADV_FREE: lazy discard; old contents may persist until
        // the kernel reclaims under pressure (a valid §20.1 muzzy outcome).
        let addr = Self::checked_subrange(region, offset, len)?;
        self.purge_lazy_calls.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: in-bounds, OS-page-aligned sub-range of `region`'s mapping.
        unsafe { sys::purge_lazy(addr, len) }
    }

    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        // §20.4 purge_forced ≈ MADV_DONTNEED: discard contents promptly.
        let addr = Self::checked_subrange(region, offset, len)?;
        self.purge_forced_calls.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            return Ok(());
        }
        // SAFETY: in-bounds, OS-page-aligned sub-range of `region`'s mapping.
        unsafe { sys::purge_forced(addr, len) }
    }

    fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let mut owned = self.owned.lock().expect("provider mutex poisoned");
        let base = region.base as usize;
        let idx = owned
            .iter()
            .position(|o| o.base == base)
            .ok_or(BackendError::InvalidRequest)?;
        let o = owned.swap_remove(idx);
        // SAFETY: `o.base`/`o.mapped_len`/`o.align` are exactly the reservation from
        // the matching `reserve`, removed from the set so it is freed exactly once.
        unsafe { sys::release(o.base, o.mapped_len, o.align) }
    }

    // `revoke_descendants` uses the default no-op (single ambient authority — there
    // are no descendant capabilities to revoke on POSIX, §36.6).

    fn bind_node(&self, region: Region, os_node: u32) -> Result<(), BackendError> {
        // §15.5 best-effort NUMA bind: prefer `os_node` for `region`'s future faults
        // (Linux `mbind(MPOL_PREFERRED)`). Applied to the not-yet-faulted reservation, so
        // it only steers placement; a failure is surfaced to the caller (the router
        // records it) and never corrupts state. No-op on non-Linux.
        self.bind_node_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `region` is a live reservation of this provider; binding the NUMA policy
        // of its page-aligned VA range performs no read/write through the pointer.
        unsafe { sys::bind_node(region.base as usize, region.len, os_node) }
    }

    fn name(&self) -> &'static str {
        "posix"
    }
}

impl Drop for PosixBackingProvider {
    fn drop(&mut self) {
        // Reclaim anything not explicitly released so tests/sanitizers see no leak.
        for o in self
            .owned
            .get_mut()
            .expect("provider mutex poisoned")
            .drain(..)
        {
            // SAFETY: same invariant as `release` — each entry pairs a live
            // reservation with its mapping, freed exactly once here.
            let _ = unsafe { sys::release(o.base, o.mapped_len, o.align) };
        }
    }
}

// ===========================================================================
// Platform layer: real mmap/madvise/mprotect on unix; a host-backed fallback
// elsewhere. Both expose the same `unsafe` helper surface.
// ===========================================================================

#[cfg(unix)]
mod sys {
    use std::io::Error as IoError;

    use topo_core::BackendError;

    /// `madvise` advice for **discard the backing now** (RSS drops immediately; reuse
    /// needs a recommit, M-005). Linux and the other BSDs use `MADV_DONTNEED` — a
    /// later read then faults a fresh **zero** page. Darwin's `MADV_DONTNEED` is
    /// advisory and does *not* reclaim, so Apple targets use `MADV_FREE_REUSABLE`,
    /// the eager-reclaim equivalent there (the §20.4 per-platform table). The
    /// allocator's contract is unchanged either way: a released range is never read
    /// without a recommit first (M-005), so whether the discarded page reads back as
    /// zero is not relied upon. (The Apple branch compiles against `libc` but is not
    /// exercised by CI, which runs on Linux.)
    #[cfg(target_vendor = "apple")]
    const MADV_DISCARD_NOW: libc::c_int = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_vendor = "apple"))]
    const MADV_DISCARD_NOW: libc::c_int = libc::MADV_DONTNEED;

    /// The OS page size (`sysconf(_SC_PAGESIZE)`), the granularity `mmap`/`madvise`/
    /// `mprotect` operate at. The allocator's `PAGE_SIZE` (16 KiB) is a multiple of
    /// it, so every extent offset/length passed here is OS-page-aligned.
    fn os_page() -> usize {
        // SAFETY: `sysconf` with a valid name is always safe; it returns the page
        // size (a positive power of two on every supported platform) or -1.
        let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if v <= 0 {
            4096 // a safe, conventional fallback if sysconf is unavailable
        } else {
            v as usize
        }
    }

    /// Round `n` up to a multiple of the (power-of-two) OS page, or `None` on
    /// overflow (§9.7 — never wrap to a smaller size).
    fn round_to_page(n: usize) -> Option<usize> {
        let page = os_page();
        let mask = page - 1;
        Some(n.checked_add(mask)? & !mask)
    }

    /// Reserve `size` bytes aligned to `align` (a power of two). Returns
    /// `(base, mapped_len)`; `mapped_len >= size` is the page-rounded length to
    /// `munmap` on release. `guard` reserves `PROT_NONE` (commit will `mprotect` it
    /// usable); otherwise `PROT_READ|WRITE`.
    ///
    /// # Safety
    /// `size > 0` and `align` is a power of two. The returned region is a fresh,
    /// exclusively-owned mapping; the caller frees it via [`release`].
    pub(super) unsafe fn reserve(
        size: usize,
        align: usize,
        guard: bool,
    ) -> Result<(usize, usize), BackendError> {
        let page = os_page();
        let prot = if guard {
            libc::PROT_NONE
        } else {
            libc::PROT_READ | libc::PROT_WRITE
        };
        let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        let msize = round_to_page(size).ok_or(BackendError::InvalidRequest)?;

        if align <= page {
            // A page-granular `mmap` is already `align`-aligned (align ≤ page).
            // SAFETY: `msize > 0`; an anonymous `MAP_PRIVATE` mapping with a null
            // hint lets the kernel choose the address.
            let p = unsafe { libc::mmap(core::ptr::null_mut(), msize, prot, flags, -1, 0) };
            if p == libc::MAP_FAILED {
                return Err(BackendError::OutOfMemory);
            }
            return Ok((p as usize, msize));
        }

        // Over-allocate and trim to an `align`-aligned subrange (`align > page`).
        let over = msize
            .checked_add(align)
            .ok_or(BackendError::InvalidRequest)?;
        // SAFETY: `over > 0`; anonymous mapping as above.
        let raw = unsafe { libc::mmap(core::ptr::null_mut(), over, prot, flags, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(BackendError::OutOfMemory);
        }
        let raw = raw as usize;
        let aligned = (raw + align - 1) & !(align - 1); // align is a power of two
        let front = aligned - raw; // raw and aligned are both page-aligned ⇒ page multiple
        let tail = over - front - msize; // all page multiples ⇒ page multiple
        if front > 0 {
            // SAFETY: `[raw, raw+front)` is the head slack of our own mapping,
            // page-aligned with a page-multiple length; unmapping it is sound.
            unsafe { libc::munmap(raw as *mut libc::c_void, front) };
        }
        if tail > 0 {
            // SAFETY: `[aligned+msize, …)` is the tail slack of our own mapping.
            unsafe { libc::munmap((aligned + msize) as *mut libc::c_void, tail) };
        }
        Ok((aligned, msize))
    }

    /// Make `[addr, addr+len)` usable. Performance mode: a no-op. Guard mode:
    /// `mprotect(PROT_READ|WRITE)`.
    ///
    /// # Safety
    /// `[addr, addr+len)` is an OS-page-aligned sub-range of a live reservation.
    pub(super) unsafe fn commit(addr: usize, len: usize, guard: bool) -> Result<(), BackendError> {
        if !guard {
            return Ok(()); // already mapped read-write
        }
        // SAFETY: page-aligned in-bounds range of our mapping.
        let rc = unsafe {
            libc::mprotect(
                addr as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::OutOfMemory)
        }
    }

    /// Drop the backing of `[addr, addr+len)` (MADV_DONTNEED). Guard mode also
    /// `mprotect(PROT_NONE)`s it so a use-after-free faults.
    ///
    /// # Safety
    /// `[addr, addr+len)` is an OS-page-aligned sub-range of a live reservation.
    pub(super) unsafe fn decommit(
        addr: usize,
        len: usize,
        guard: bool,
    ) -> Result<(), BackendError> {
        // SAFETY: page-aligned in-bounds range; the discard-now advice on anonymous
        // private memory drops the pages (zero-fill on next fault on Linux; eager
        // reclaim on Darwin — see `MADV_DISCARD_NOW`).
        let rc = unsafe { libc::madvise(addr as *mut libc::c_void, len, MADV_DISCARD_NOW) };
        if rc != 0 {
            return Err(BackendError::OutOfMemory);
        }
        if guard {
            // SAFETY: same in-bounds page-aligned range; revoke access so a stray
            // use faults (§20.5). A later `commit` restores RW.
            let rc = unsafe { libc::mprotect(addr as *mut libc::c_void, len, libc::PROT_NONE) };
            if rc != 0 {
                return Err(BackendError::OutOfMemory);
            }
        }
        Ok(())
    }

    /// Lazily discard `[addr, addr+len)` (MADV_FREE). Treats `EINVAL` (the kernel
    /// lacks `MADV_FREE`) as a successful no-op — the contents simply persist, a
    /// valid lazy outcome (§20.1).
    ///
    /// # Safety
    /// `[addr, addr+len)` is an OS-page-aligned sub-range of a live reservation.
    pub(super) unsafe fn purge_lazy(addr: usize, len: usize) -> Result<(), BackendError> {
        // SAFETY: page-aligned in-bounds range.
        let rc = unsafe { libc::madvise(addr as *mut libc::c_void, len, libc::MADV_FREE) };
        if rc == 0 {
            return Ok(());
        }
        match IoError::last_os_error().raw_os_error() {
            Some(libc::EINVAL) => Ok(()), // MADV_FREE unsupported ⇒ retained (lazy)
            _ => Err(BackendError::OutOfMemory),
        }
    }

    /// Forcibly discard `[addr, addr+len)` now (the discard-now advice; see
    /// [`MADV_DISCARD_NOW`]).
    ///
    /// # Safety
    /// `[addr, addr+len)` is an OS-page-aligned sub-range of a live reservation.
    pub(super) unsafe fn purge_forced(addr: usize, len: usize) -> Result<(), BackendError> {
        // SAFETY: page-aligned in-bounds range.
        let rc = unsafe { libc::madvise(addr as *mut libc::c_void, len, MADV_DISCARD_NOW) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::OutOfMemory)
        }
    }

    /// Return a whole reservation to the OS (`munmap`).
    ///
    /// # Safety
    /// `(base, mapped_len)` is exactly a reservation from [`reserve`], freed once.
    pub(super) unsafe fn release(
        base: usize,
        mapped_len: usize,
        _align: usize,
    ) -> Result<(), BackendError> {
        // SAFETY: `base`/`mapped_len` are the page-aligned base and length of our
        // own mapping; `munmap` of the whole reservation is sound and idempotent
        // only once (we removed it from the owned set first).
        let rc = unsafe { libc::munmap(base as *mut libc::c_void, mapped_len) };
        if rc == 0 {
            Ok(())
        } else {
            Err(BackendError::InvalidRequest)
        }
    }

    /// Bind `[addr, addr+len)`'s NUMA policy to prefer node `os_node` (§15.5, W13). On
    /// Linux this is `mbind(MPOL_PREFERRED)` via the raw syscall (`mbind` has no portable
    /// libc wrapper); on other unix (Darwin) NUMA binding is unavailable, so it is a no-op.
    ///
    /// # Safety
    /// `[addr, addr+len)` is a page-aligned, not-yet-faulted range of a live reservation.
    #[cfg(target_os = "linux")]
    pub(super) unsafe fn bind_node(
        addr: usize,
        len: usize,
        os_node: u32,
    ) -> Result<(), BackendError> {
        if len == 0 {
            return Ok(());
        }
        // The OS node id fits one mask word (discovery bounds OS ids to < MAX_NODES ≤ 64).
        if os_node >= 64 {
            return Err(BackendError::InvalidRequest);
        }
        // MPOL_PREFERRED: *prefer* `os_node`, falling back to other nodes if it is full,
        // so a bind never fails the eventual allocation — best-effort placement (§2.4).
        const MPOL_PREFERRED: libc::c_long = 1;
        let nodemask: libc::c_ulong = 1u64 << os_node;
        // SAFETY: `mbind` only sets the policy of our own page-aligned VA range (no access
        // through `addr`); it reads exactly one valid mask word at `&nodemask`.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_mbind,
                addr as *mut libc::c_void,
                len as libc::c_ulong,
                MPOL_PREFERRED,
                &nodemask as *const libc::c_ulong,
                64 as libc::c_ulong, // bits available in the mask
                0 as libc::c_uint,   // flags
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            // Kernel without NUMA, a node that does not exist, etc. — best-effort, so the
            // caller records the failure and proceeds (locality lost, never correctness).
            Err(BackendError::OutOfMemory)
        }
    }

    /// NUMA binding is unavailable on non-Linux unix (e.g. Darwin) — a no-op.
    ///
    /// # Safety
    /// As the Linux variant.
    #[cfg(not(target_os = "linux"))]
    pub(super) unsafe fn bind_node(
        _addr: usize,
        _len: usize,
        _os_node: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(not(unix))]
mod sys {
    //! Host-backed fallback for non-unix targets: the global allocator stands in
    //! for `mmap`, and zeroing models `MADV_DONTNEED`. Same observable behaviour as
    //! the real provider so the crate builds and tests run everywhere.
    use std::alloc::{alloc, dealloc, Layout};

    use topo_core::BackendError;

    fn layout(size: usize, align: usize) -> Result<Layout, BackendError> {
        Layout::from_size_align(size.max(1), align.max(1)).map_err(|_| BackendError::InvalidRequest)
    }

    /// # Safety
    /// `size > 0` and `align` is a power of two.
    pub(super) unsafe fn reserve(
        size: usize,
        align: usize,
        _guard: bool,
    ) -> Result<(usize, usize), BackendError> {
        let l = layout(size, align)?;
        // SAFETY: `l` has nonzero size and a valid power-of-two alignment.
        let p = unsafe { alloc(l) };
        if p.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        Ok((p as usize, size))
    }

    /// # Safety
    /// In-bounds sub-range of a live reservation.
    pub(super) unsafe fn commit(
        _addr: usize,
        _len: usize,
        _guard: bool,
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// # Safety
    /// In-bounds sub-range of a live reservation.
    pub(super) unsafe fn decommit(
        addr: usize,
        len: usize,
        _guard: bool,
    ) -> Result<(), BackendError> {
        // SAFETY: in-bounds range; model MADV_DONTNEED by zeroing (fresh-zero read).
        unsafe { core::ptr::write_bytes(addr as *mut u8, 0, len) };
        Ok(())
    }

    /// # Safety
    /// In-bounds sub-range of a live reservation.
    pub(super) unsafe fn purge_lazy(_addr: usize, _len: usize) -> Result<(), BackendError> {
        Ok(()) // lazy: contents retained on the host (a valid MADV_FREE outcome)
    }

    /// # Safety
    /// In-bounds sub-range of a live reservation.
    pub(super) unsafe fn purge_forced(addr: usize, len: usize) -> Result<(), BackendError> {
        // SAFETY: in-bounds range; model MADV_DONTNEED by zeroing.
        unsafe { core::ptr::write_bytes(addr as *mut u8, 0, len) };
        Ok(())
    }

    /// # Safety
    /// `(base, mapped_len, align)` is exactly a reservation from [`reserve`].
    pub(super) unsafe fn release(
        base: usize,
        mapped_len: usize,
        align: usize,
    ) -> Result<(), BackendError> {
        let l = layout(mapped_len, align)?;
        // SAFETY: exactly the pointer and layout from the matching `reserve`.
        unsafe { dealloc(base as *mut u8, l) };
        Ok(())
    }

    /// No NUMA on the host-allocator fallback — a no-op.
    ///
    /// # Safety
    /// As the unix variant.
    pub(super) unsafe fn bind_node(
        _addr: usize,
        _len: usize,
        _os_node: u32,
    ) -> Result<(), BackendError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Topology discovery (§15.2, plan 04 W13-1/W13-4)
// ---------------------------------------------------------------------------

/// **Discover the §15.2 CPU/LLC/NUMA topology** from the platform, falling back to the
/// conservative single-domain model on any missing or unreadable data (§15.2 — the
/// allocator MUST tolerate inconsistent topology). On Linux this reads sysfs
/// (`/sys/devices/system/node/*` for NUMA membership, `physical_package_id` as the LLC
/// proxy, `node*/distance` for the §15.4 distance matrix); elsewhere it reports one
/// domain over the online CPU count. The host calls this at startup and on a hotplug /
/// periodic-mismatch refresh ([`Topology::detect_mismatch`](topo_core::Topology::detect_mismatch)).
pub fn discover_topology() -> Topology {
    let n = online_cpus();
    #[cfg(target_os = "linux")]
    {
        linux_topology::discover(n).unwrap_or_else(|| Topology::single_domain(n))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Topology::single_domain(n)
    }
}

/// The number of online CPUs (`sysconf(_SC_NPROCESSORS_ONLN)`), at least 1.
fn online_cpus() -> u32 {
    // SAFETY: `sysconf` with a valid name is always safe; `-1`/`0` ⇒ the 1-CPU floor.
    let v = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if v >= 1 {
        (v as u64).min(topo_core::topology::MAX_CPUS as u64) as u32
    } else {
        1
    }
}

#[cfg(target_os = "linux")]
mod linux_topology {
    use super::Topology;
    use topo_core::topology::MAX_NODES;
    use topo_core::TopologyBuilder;

    /// Parse a Linux "cpulist" string (`"0-3,8,10-11"`), invoking `f` for each CPU it
    /// names. Tolerant: malformed fragments are skipped.
    fn for_each_in_list(list: &str, mut f: impl FnMut(u32)) {
        for part in list.trim().split(',') {
            if part.is_empty() {
                continue;
            }
            if let Some((a, b)) = part.split_once('-') {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    for c in a..=b {
                        f(c);
                    }
                }
            } else if let Ok(c) = part.trim().parse::<u32>() {
                f(c);
            }
        }
    }

    /// `cpu → node` from `/sys/devices/system/node/node{K}/cpulist`, plus a parallel
    /// `seen[c]` mask recording which CPUs were actually named by *some* node's cpulist.
    /// `None` if the node hierarchy is absent (→ caller falls back to a single domain).
    ///
    /// A CPU absent from every cpulist (a partial/sparse sysfs, or a node id beyond the
    /// bounded `0..MAX_NODES` scan) keeps its placeholder node `0` **but** `seen[c]` stays
    /// `false`: the caller must treat that as missing data, not as genuine node-0
    /// membership (§15.2 — a never-observed CPU must not be fabricated onto node 0).
    fn read_cpu_nodes(n_cpus: u32) -> Option<(std::vec::Vec<u32>, std::vec::Vec<bool>)> {
        let mut cpu_node = std::vec![0u32; n_cpus as usize];
        let mut seen = std::vec![false; n_cpus as usize];
        let mut any = false;
        for node in 0..MAX_NODES as u32 {
            let path = std::format!("/sys/devices/system/node/node{node}/cpulist");
            let Ok(list) = std::fs::read_to_string(&path) else {
                continue;
            };
            any = true;
            for_each_in_list(&list, |c| {
                if c < n_cpus {
                    cpu_node[c as usize] = node;
                    seen[c as usize] = true;
                }
            });
        }
        any.then_some((cpu_node, seen))
    }

    /// `cpu → LLC` from each CPU's `physical_package_id` (the socket — a robust LLC
    /// proxy: LLC is per-socket on mainstream x86/ARM). Distinct package ids are
    /// renumbered to a dense `0..n_llc`. `None` if any CPU's id is unreadable.
    fn read_cpu_llc(n_cpus: u32) -> Option<std::vec::Vec<u32>> {
        let mut llc = std::vec![0u32; n_cpus as usize];
        let mut seen: std::vec::Vec<u16> = std::vec::Vec::new();
        for c in 0..n_cpus {
            let path = std::format!("/sys/devices/system/cpu/cpu{c}/topology/physical_package_id");
            let id: u16 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
            let dense = match seen.iter().position(|&p| p == id) {
                Some(i) => i as u32,
                None => {
                    seen.push(id);
                    (seen.len() - 1) as u32
                }
            };
            llc[c as usize] = dense;
        }
        Some(llc)
    }

    /// Record the parsed `cpu → (node, LLC)` mapping into `b`, calling
    /// [`set_cpu`](TopologyBuilder::set_cpu) **only for CPUs actually observed** in some
    /// `node*/cpulist` (`seen[c]`). A CPU never seen is deliberately left *unset* so the
    /// builder's own gap check (a CPU never recorded ⇒ inconsistent) fires the
    /// conservative single-domain fallback, rather than the placeholder node `0` being
    /// passed off as real membership (§15.2). Pure (no sysfs) so it is unit-testable.
    pub(super) fn fill_builder(
        b: &mut TopologyBuilder,
        n_cpus: u32,
        cpu_node: &[u32],
        seen: &[bool],
        cpu_llc: &[u32],
    ) {
        for c in 0..n_cpus as usize {
            if seen[c] {
                b.set_cpu(c as u32, cpu_node[c], cpu_llc[c]);
            }
            // Else: leave CPU `c` unset → the builder reports a gap and falls back.
        }
    }

    /// Build the §15.2 snapshot from sysfs, or `None` to fall back. The
    /// [`TopologyBuilder`] itself re-checks consistency, so even a partial read that
    /// gets this far yields a *safe* snapshot.
    pub(super) fn discover(n_cpus: u32) -> Option<Topology> {
        let (cpu_node, seen) = read_cpu_nodes(n_cpus)?;
        let cpu_llc = read_cpu_llc(n_cpus)?;
        let mut b = TopologyBuilder::new(n_cpus);
        fill_builder(&mut b, n_cpus, &cpu_node, &seen, &cpu_llc);
        // §15.4 node distances (best-effort; the default remote distance stands if a
        // row is unreadable).
        for a in 0..MAX_NODES as u32 {
            let path = std::format!("/sys/devices/system/node/node{a}/distance");
            let Ok(row) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (b_idx, d) in row.split_whitespace().enumerate() {
                if let Ok(dist) = d.parse::<u8>() {
                    b.set_distance(a, b_idx as u32, dist);
                }
            }
        }
        Some(b.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: usize = 16384; // the allocator page; a multiple of any OS page.

    #[test]
    fn discover_topology_yields_a_valid_snapshot() {
        // §15.2: discovery returns a usable snapshot whether it reads real sysfs or
        // falls back to a single domain — never empty, never panicking.
        let t = discover_topology();
        assert!(t.cpu_count() >= 1, "at least one CPU");
        assert!(t.node_count() >= 1, "at least one node");
        assert!(t.llc_count() >= 1, "at least one LLC domain");
        // Every query is total: an out-of-range CPU reads as the default domain.
        assert_eq!(t.node_of_cpu(u32::MAX), topo_core::NodeId::DEFAULT);
        // The discovered node count never exceeds the bounded model.
        assert!(t.node_count() as usize <= topo_core::topology::MAX_NODES);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_cpu_node_coverage_falls_back_to_single_domain() {
        // §15.2 regression: if some online CPU is never named by any `node*/cpulist`
        // (a partial/sparse sysfs, or a node id beyond the bounded scan), the parsed
        // node array still carries a *placeholder* 0 for that CPU — but `seen[c]` is
        // false. Discovery must NOT pass that off as genuine node-0 membership: the
        // unseen CPU is left unset, so the builder's gap check fires the conservative
        // single-domain fallback (NOT "everything steered to node 0").
        use topo_core::TopologyBuilder;

        let n_cpus = 4u32;
        // CPUs 0,1 observed on node 0; 2,3 observed on node 1 — but CPU 3 was never
        // seen in any cpulist (its node entry is the placeholder 0, seen[3] == false).
        let cpu_node = [0u32, 0, 1, 0];
        let seen = [true, true, true, false];
        let cpu_llc = [0u32, 0, 1, 1];

        let mut b = TopologyBuilder::new(n_cpus);
        linux_topology::fill_builder(&mut b, n_cpus, &cpu_node, &seen, &cpu_llc);
        let t = b.build();

        assert!(
            t.is_single_domain(),
            "an unseen CPU must trigger the single-domain fallback, not node-0 fabrication"
        );
        assert_eq!(
            t.cpu_count(),
            n_cpus,
            "the fallback keeps the online CPU count"
        );
        // Every CPU reads as the one conservative node — none is steered onto node 0 of
        // a (wrongly) multi-node snapshot.
        for c in 0..n_cpus {
            assert_eq!(t.node_of_cpu(c), topo_core::NodeId(0));
        }

        // Sanity: with full coverage the same inputs DO build the real two-node map,
        // so the fallback is driven by the missing CPU, not by the data shape.
        let seen_full = [true, true, true, true];
        let cpu_node_full = [0u32, 0, 1, 1];
        let mut b2 = TopologyBuilder::new(n_cpus);
        linux_topology::fill_builder(&mut b2, n_cpus, &cpu_node_full, &seen_full, &cpu_llc);
        let t2 = b2.build();
        assert!(
            !t2.is_single_domain(),
            "full coverage builds the real topology"
        );
        assert_eq!(t2.node_count(), 2);
        assert_eq!(t2.node_of_cpu(3), topo_core::NodeId(1));
    }

    #[test]
    fn reserve_commit_write_release_roundtrip() {
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        assert_eq!(r.len, PAGE);
        assert_eq!(r.base as usize % PAGE, 0);
        // SAFETY: the region is committed for its full length; writing within
        // `[0, len)` is in bounds.
        unsafe {
            for i in 0..r.len {
                r.base.add(i).write(0xab);
            }
            assert_eq!(r.base.add(10).read(), 0xab);
        }
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn over_aligned_reservation_honors_alignment() {
        let p = PosixBackingProvider::new();
        let big = 256 * 1024; // 256 KiB alignment, well over any OS page
        let r = p.reserve(ArenaId::DEFAULT, PAGE, big).expect("reserve");
        assert_eq!(r.base as usize % big, 0, "alignment honored");
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed for its full length.
        unsafe { r.base.write(0x5a) };
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn rejects_zero_size_and_bad_align() {
        let p = PosixBackingProvider::new();
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 0, 16),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 16, 24),
            Err(BackendError::InvalidRequest)
        ));
    }

    #[test]
    fn decommit_then_recommit_restores_writable_memory_m005() {
        // §20.4 / M-005 on the production path: after a decommit (MADV_DONTNEED) the
        // range must be recommittable and writable again — a fresh, zeroed mapping —
        // so a reused released extent is sound. (The guard-mode test cycles this with
        // PROT_NONE; this covers the default performance path, which nothing else did.)
        let p = PosixBackingProvider::new();
        let r = p
            .reserve(ArenaId::DEFAULT, 4 * PAGE, PAGE)
            .expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed for its full length.
        unsafe { std::ptr::write_bytes(r.base, 0xaa, r.len) };
        p.decommit(r, 0, r.len).expect("decommit"); // discards contents now
        p.commit(r, 0, r.len).expect("recommit"); // M-005 recommit before reuse
                                                  // SAFETY: recommitted for its full length.
        unsafe {
            assert_eq!(
                r.base.read(),
                0,
                "recommitted range faults in zeroed (DONTNEED)"
            );
            std::ptr::write_bytes(r.base, 0xbb, r.len);
            assert_eq!(
                r.base.add(r.len - 1).read(),
                0xbb,
                "writable after recommit"
            );
        }
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn decommit_discards_contents_like_madv_dontneed() {
        // §20.4 / W4-3a: decommit ≈ MADV_DONTNEED — a later read sees a fresh zero
        // page. With the real provider this exercises the actual madvise.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed for its whole length.
        unsafe {
            std::ptr::write_bytes(r.base, 0xcd, r.len);
            assert_eq!(r.base.add(100).read(), 0xcd);
        }
        p.decommit(r, 0, r.len).expect("decommit");
        // SAFETY: still mapped; the dropped pages fresh-fault to zero.
        unsafe {
            for i in 0..r.len {
                assert_eq!(r.base.add(i).read(), 0, "decommit must discard contents");
            }
        }
        assert_eq!(p.decommit_calls(), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn purge_lazy_keeps_range_usable_purge_forced_discards() {
        // purge_lazy ≈ MADV_FREE: the range stays usable (a write after it is kept;
        // we do not assert the *old* contents, since MADV_FREE may or may not have
        // reclaimed). purge_forced ≈ MADV_DONTNEED: contents discarded now.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed region.
        unsafe { std::ptr::write_bytes(r.base, 0x77, r.len) };
        p.purge_lazy(r, 0, r.len).expect("purge_lazy");
        // SAFETY: a write cancels MADV_FREE; the range is usable.
        unsafe {
            r.base.write(0x88);
            assert_eq!(r.base.read(), 0x88, "range usable after lazy purge");
        }
        p.purge_forced(r, 0, r.len).expect("purge_forced");
        // SAFETY: in bounds; forced purge discarded → fresh zero.
        unsafe { assert_eq!(r.base.read(), 0, "forced purge discards (MADV_DONTNEED)") };
        assert_eq!(p.purge_lazy_calls(), 1);
        assert_eq!(p.purge_forced_calls(), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn physical_ops_reject_out_of_range_subranges() {
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        assert!(matches!(
            p.decommit(r, PAGE, 1),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            p.commit(r, 0, usize::MAX),
            Err(BackendError::InvalidRequest)
        ));
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn bind_node_is_best_effort_and_counted() {
        // §15.5 (W13): the NUMA bind is best-effort — it either succeeds or fails cleanly
        // (never panics, never corrupts state), and the attempt is counted. We do NOT
        // assert success: NUMA support varies by environment (a single-node container, a
        // kernel without NUMA, a seccomp filter), and the contract is "lose locality, not
        // correctness". Binding an unfaulted reservation only sets a policy.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        let _ = p.bind_node(r, 0); // node 0 (or a no-op on non-Linux) — must not panic
        assert_eq!(p.bind_node_calls(), 1, "the bind attempt is counted");
        // A bind to a likely-absent node is also handled gracefully (Ok or Err, no panic).
        let _ = p.bind_node(r, 3);
        assert_eq!(p.bind_node_calls(), 2);
        // The region is still usable after the bind attempts (commit + write).
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed for its whole length.
        unsafe { r.base.write(0x5a) };
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn revoke_descendants_is_a_noop_on_posix() {
        // §36.6: POSIX has a single ambient authority — nothing to revoke.
        let p = PosixBackingProvider::new();
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        assert!(p.revoke_descendants(ArenaId::DEFAULT, r).is_ok());
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn typed_seam_default_composes_the_collapsed_ops() {
        // W4-1 full typed surface: the granular §36.6 methods (default-composed on
        // POSIX) walk reserve_window → create_frame → map_frame → (use) →
        // unmap_frame → recycle, and the mapped range is writable.
        use topo_core::Rights;
        let p = PosixBackingProvider::new();
        let win = p
            .reserve_window(ArenaId::DEFAULT, PAGE, PAGE, Rights::READ_WRITE)
            .expect("reserve_window");
        let frame = p
            .create_frame(
                ArenaId::DEFAULT,
                PAGE.trailing_zeros(),
                topo_core::Label::PUBLIC,
            )
            .expect("create_frame");
        assert_eq!(frame.size, PAGE);
        let mapped = p
            .map_frame(
                ArenaId::DEFAULT,
                frame,
                win,
                Rights::READ_WRITE,
                Default::default(),
            )
            .expect("map_frame");
        // SAFETY: the mapped range is committed for its whole length.
        unsafe {
            mapped.region.base.write(0x42);
            assert_eq!(mapped.region.base.read(), 0x42);
        }
        let _frame = p
            .unmap_frame(ArenaId::DEFAULT, mapped)
            .expect("unmap_frame");
        p.recycle(ArenaId::DEFAULT, mapped).expect("recycle");
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn guard_mode_traps_use_after_free() {
        // §20.5 / C14: in guard mode a decommitted (released) range is PROT_NONE, so
        // a stray access faults. Verify in a forked child (the access kills it) so
        // the parent test survives; recommit restores access.
        let p = PosixBackingProvider::with_guard_pages();
        assert!(p.guards_use_after_free());
        let r = p.reserve(ArenaId::DEFAULT, PAGE, PAGE).expect("reserve");
        p.commit(r, 0, r.len).expect("commit");
        // SAFETY: committed RW in guard mode (mprotect).
        unsafe { r.base.write(0x11) };
        p.decommit(r, 0, r.len).expect("decommit"); // now PROT_NONE

        // SAFETY: `fork` in a test with no other threads touching shared state; the
        // child only reads one address and exits/aborts.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child: touching the guarded page must fault (SIGSEGV/SIGBUS).
            // SAFETY: deliberate faulting access; the child never returns normally.
            let v = unsafe { core::ptr::read_volatile(r.base) };
            // Unreachable if the guard works; exit 0 would signal a *missing* trap.
            std::process::exit(v as i32 & 1);
        }
        // Parent: the child must have died from a signal, not exited normally.
        let mut status: libc::c_int = 0;
        // SAFETY: valid pid and status pointer.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let signalled = libc::WIFSIGNALED(status);
        assert!(signalled, "guarded use-after-free did not fault");

        // Recommit restores access (M-005), proving the guard is reversible.
        p.commit(r, 0, r.len).expect("recommit");
        // SAFETY: re-committed RW.
        unsafe {
            r.base.write(0x22);
            assert_eq!(r.base.read(), 0x22);
        }
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[cfg(all(unix, not(miri)))]
    #[test]
    fn unmap_policy_with_guard_pages_faults_on_use_after_free() {
        // §20.5 / W4-3b end-to-end: the guard-mode provider *driven by the extent
        // manager under `RetainPolicy::Unmap`* must trap a use-after-free. Freeing an
        // extent decommits it AND mprotects the range PROT_NONE, so a later access to
        // the freed extent faults — the integration the provider-level guard test does
        // not cover (it calls the provider directly). Verified in a forked child.
        use topo_core::bootstrap::BumpArena;
        use topo_core::{ExtentManager, Fit, RetainPolicy};

        // Leaked metadata arena (live for the process), the slot-pool test pattern.
        let meta: &'static BumpArena = {
            let buf = vec![0u8; 1 << 20].into_boxed_slice();
            let len = buf.len();
            let ptr = Box::into_raw(buf).cast::<u8>();
            // SAFETY: the leaked buffer is valid for the process.
            Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
        };
        let mut mgr = ExtentManager::new(
            PosixBackingProvider::with_guard_pages(),
            meta,
            ArenaId::DEFAULT,
            4 * PAGE,
            PAGE,
            4096,
        )
        .expect("manager");
        mgr.set_retain_policy(RetainPolicy::Unmap);

        let r = mgr.alloc(2 * PAGE, PAGE, Fit::First).expect("alloc");
        let region = mgr.region_of(r).expect("region");
        // SAFETY: guard mode mprotect'd the extent RW on the commit `alloc` performed.
        unsafe { region.base.write(0x11) };
        mgr.free(r).expect("free"); // Unmap: decommit + mprotect(PROT_NONE)

        // SAFETY: `fork` in a test with no other threads touching shared state; the
        // child only reads one address and never returns normally. The parent must
        // NOT touch `region.base` (it is PROT_NONE in this process too).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child: the freed extent's range is PROT_NONE — this read must fault.
            // SAFETY: deliberate faulting access; the child never returns normally.
            let v = unsafe { core::ptr::read_volatile(region.base) };
            std::process::exit(v as i32 & 1); // exit 0 would signal a *missing* trap
        }
        let mut status: libc::c_int = 0;
        // SAFETY: valid pid and status pointer.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFSIGNALED(status),
            "use-after-free of an Unmap-freed extent did not fault under guard pages"
        );
    }

    #[test]
    fn map_frame_rejects_a_frame_smaller_than_its_window() {
        // Audit: the collapsed seam maps a window 1:1 to a frame. A sub-window frame
        // is rejected (otherwise `recycle` → `release` would unmap the *whole*
        // reservation while the `MappedRange` describes only the frame, leaving the
        // remainder untracked). A frame that fills the window maps fine.
        use topo_core::Rights;
        let p = PosixBackingProvider::new();
        let win = p
            .reserve_window(ArenaId::DEFAULT, 4 * PAGE, PAGE, Rights::READ_WRITE)
            .expect("reserve_window");
        let small = p
            .create_frame(
                ArenaId::DEFAULT,
                PAGE.trailing_zeros(),
                topo_core::Label::PUBLIC,
            )
            .expect("create_frame");
        assert_eq!(small.size, PAGE);
        assert!(
            matches!(
                p.map_frame(
                    ArenaId::DEFAULT,
                    small,
                    win,
                    Rights::READ_WRITE,
                    Default::default()
                ),
                Err(BackendError::InvalidRequest)
            ),
            "a frame smaller than its window is rejected"
        );
        // The reserved window is released by the provider's Drop.
    }

    #[test]
    fn name_is_posix() {
        assert_eq!(PosixBackingProvider::new().name(), "posix");
    }
}

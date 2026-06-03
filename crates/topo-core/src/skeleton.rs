// SPDX-License-Identifier: MIT
//! The M0 walking-skeleton allocator (W0-14a).
//!
//! A bump allocator over a single committed region obtained through the
//! `TopoBackingProvider` seam. `malloc` classifies, aligns, and bumps a shared
//! atomic cursor with fully checked arithmetic (never wraps, §9.7); `free`
//! leaks. Its only jobs are to prove that the C-ABI export and the backing-seam
//! *compile, link, and run* over either backend — not to be a good allocator.
//! Real front/middle/back ends replace it from M1 (plans 03–05).

use core::ptr;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::backend::{Region, TopoBackingProvider};
use crate::classify::{classify, RequestKind};
use crate::error::BackendError;
use crate::ids::ArenaId;
use crate::overflow::align_up;

/// Minimum alignment guaranteed for every allocation (`alignof(max_align_t)` on
/// LP64). The backing region is reserved at least this aligned.
pub const MIN_ALIGN: usize = 16;

/// A bump allocator over one committed region. Thread-safe: concurrent `malloc`s
/// race only on the atomic cursor via a compare-exchange loop.
pub struct SkeletonAllocator<P: TopoBackingProvider> {
    provider: P,
    region: Region,
    arena: ArenaId,
    /// Next free offset within `region`.
    cursor: AtomicUsize,
    /// Monotonic request id for trace correlation.
    next_request_id: AtomicU64,
}

impl<P: TopoBackingProvider> SkeletonAllocator<P> {
    /// Create an allocator backed by `bytes` of memory from `provider`.
    pub fn new(provider: P, bytes: usize) -> Result<Self, BackendError> {
        let arena = ArenaId::DEFAULT;
        let region = provider.reserve(arena, bytes, MIN_ALIGN)?;
        provider.commit(region, 0, region.len)?;
        Ok(Self {
            provider,
            region,
            arena,
            cursor: AtomicUsize::new(0),
            next_request_id: AtomicU64::new(0),
        })
    }

    /// Allocate `size` bytes aligned to `align`. Returns a null pointer on
    /// out-of-memory or on any size/alignment overflow (§9.7: never wrap,
    /// never under-allocate). `align` must be a power of two; otherwise null.
    pub fn malloc(&self, size: usize, align: usize) -> *mut u8 {
        self.try_malloc(size, align).unwrap_or(ptr::null_mut())
    }

    /// The fallible core of `malloc`. `None` means "return null".
    fn try_malloc(&self, size: usize, align: usize) -> Option<*mut u8> {
        let req = classify(size, align, 0)?;
        let need = match req.kind {
            RequestKind::Small { usable, .. } => usable,
            RequestKind::Large { bytes } => bytes,
        };
        let align = req.align.max(MIN_ALIGN);
        let base_addr = self.region.base as usize;

        loop {
            let cur = self.cursor.load(Ordering::Relaxed);
            // Align the *absolute* address, with every step checked.
            let start_abs = base_addr.checked_add(cur)?;
            let aligned_abs = align_up(start_abs, align)?;
            let pad = aligned_abs - start_abs; // cannot underflow: aligned >= start
            let end = cur.checked_add(pad)?.checked_add(need)?;
            if end > self.region.len {
                return None; // genuine OOM — no wrap, no under-allocation
            }
            if self
                .cursor
                .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.next_request_id.fetch_add(1, Ordering::Relaxed);
                let offset = cur + pad;
                // SAFETY: `offset + need <= region.len` (checked above) and the
                // region is committed for its whole length in `new`, so the
                // resulting pointer is within the single committed allocation.
                return Some(unsafe { self.region.base.add(offset) });
            }
            // CAS lost the race; retry with the updated cursor.
        }
    }

    /// Free a pointer. M0 leaks every allocation (W0-14a); `free(NULL)` is a
    /// no-op (§9.6). Real reclamation arrives with the central list (plan 03).
    pub fn free(&self, _ptr: *mut u8) {}

    /// Bytes handed out so far (for tests/stats wiring).
    pub fn used_bytes(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }

    /// Total backing capacity.
    pub fn capacity(&self) -> usize {
        self.region.len
    }

    /// The backend's name (`"posix"` / `"sele4n-sim"`), proving which provider
    /// is wired (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        self.provider.name()
    }
}

impl<P: TopoBackingProvider> Drop for SkeletonAllocator<P> {
    fn drop(&mut self) {
        // Return the region to the backend; a failure here cannot be reported
        // from `drop`, but providers must leave their state well-formed (§36.6).
        let _ = self.provider.release(self.arena, self.region);
    }
}

// SAFETY: all shared mutable state is the atomic `cursor`/`next_request_id`;
// `Region` is `Send`/`Sync` (an address+len). With a `Send + Sync` provider the
// allocator is safe to share across threads, which the concurrency tests rely
// on.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for SkeletonAllocator<P> {}

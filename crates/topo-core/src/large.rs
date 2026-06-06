// SPDX-License-Identifier: MIT
//! The large-allocation path (§18.5, plan 04 W4-4) wired to pointer
//! classification (§17, W3-6).
//!
//! A large allocation (`>= HUGE_THRESHOLD`, §9.2) bypasses the small-object slab
//! path entirely: it takes a page-rounded region from the back-end
//! [`ExtentManager`], installs a
//! [`LargeDescriptor`] for it in the [`PageMap`] (so an unsized `free`/`realloc`/
//! `usable_size` over an arbitrary pointer can recover the allocation, §17.1), and
//! on free retires that pagemap entry and returns the extent. This is the
//! extent↔pagemap tie W4-2b/§18.5 require, routed through the single W3-6 mutator
//! (`install_large`/`retire_large`).
//!
//! **No metadata leak.** Large descriptors live in a fixed-capacity,
//! metadata-backed pool and are **recycled in place** with a generation bump
//! (§16.6/§27.5, the [`LargeDescriptor::recycle`] path) — so a long-running
//! workload of large alloc/free does not leak a descriptor per allocation.
//!
//! **Region-cache lifecycle (§18.6, C13).** A [`RegionCacheHook`] gets first
//! refusal for awkward (just-over-a-hugepage) sizes; an allocation it serves is
//! recorded as *cache-owned* (no backing [`ExtentRef`]) and, on free, offered back
//! to the cache rather than to the extent manager. The default hook
//! ([`NoRegionCache`]) declines, so every allocation flows through the extent
//! manager until the hugepage backend supplies a real cache (W11-3).
//!
//! **Concurrency.** The descriptor pool is guarded by a backend-class spinlock
//! (§27.2); the extent manager and the pagemap carry their own synchronization.
//! `free` resolves the descriptor and retires its pagemap entry **under that lock,
//! before** the slot can be recycled, so (a) a classifier never resolves a stale
//! address to a recycled descriptor, and (b) two threads racing to free the **same**
//! pointer resolve to exactly one successful free — the loser re-reads an
//! already-retired entry and rejects, never double-releasing the slot or
//! double-freeing the extent (M-004; checked by a `loom` model and a stress test).

use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::backend::{Region, TopoBackingProvider};
use crate::bootstrap::MetadataAlloc;
use crate::extent::{
    BackendLock, ExtentError, ExtentId, ExtentManager, ExtentRef, NoRegionCache, RegionCacheHook,
};
use crate::ids::{ArenaId, LargeId};
use crate::pagemap::PageMap;
use crate::span::LargeDescriptor;

/// Sentinel "no slot" index for the pool free list.
const NIL: u32 = u32::MAX;

/// One descriptor-pool slot: a [`LargeDescriptor`] plus the backing it owns. All
/// non-descriptor fields are integers, so a zeroed slot is valid (the descriptor's
/// atomics are valid at zero and are overwritten by `new`/`recycle` before any read).
/// `#[repr(C)]` with `desc` first, so a `*const LargeDescriptor` from the pagemap is
/// exactly the slot pointer — letting `free` map it back to a slot index.
#[repr(C)]
struct LargeSlot {
    /// The descriptor the pagemap points at (must be the first field).
    desc: LargeDescriptor,
    /// Backing extent id/generation when `has_extent != 0` (else cache-served).
    backing_id: u32,
    backing_gen: u32,
    /// `1` ⇒ backed by an [`ExtentRef`] (`backing_id`/`backing_gen`); `0` ⇒
    /// served by the region cache (freed back to the cache, not the extent manager).
    has_extent: u8,
    /// Unused-slot stack link (valid when the slot is on the free list).
    free_next: u32,
}

/// The metadata-backed, recycling descriptor pool.
struct LargePool {
    slots: NonNull<LargeSlot>,
    cap: u32,
    /// Free-slot stack head (previously-used slots, ready to recycle).
    free_head: u32,
    /// Next never-used slot (initialised with `LargeDescriptor::new`).
    high_water: u32,
    /// Monotonic id for freshly minted descriptors.
    next_id: u32,
    /// Live descriptor count (for stats/tests).
    live: u32,
}

impl LargePool {
    /// Allocate and zero a pool of `cap` slots from `meta` (zeroed slots are valid;
    /// see [`LargeSlot`]). `None` on metadata exhaustion or a zero/oversized cap.
    fn new(meta: &dyn MetadataAlloc, cap: usize) -> Option<LargePool> {
        if cap == 0 || cap > NIL as usize {
            return None;
        }
        let bytes = cap.checked_mul(core::mem::size_of::<LargeSlot>())?;
        let mem = meta.alloc(bytes, core::mem::align_of::<LargeSlot>())?;
        // SAFETY: `mem` is a fresh, exclusively-owned, aligned region of `bytes`
        // bytes; zeroing yields `cap` valid `LargeSlot`s (integer fields zeroed, and
        // a zeroed `LargeDescriptor` is a valid value whose atomics are 0 — it is
        // overwritten by `new`/`recycle` before any meaningful read).
        unsafe { ptr::write_bytes(mem.as_ptr(), 0, bytes) };
        Some(LargePool {
            slots: mem.cast::<LargeSlot>(),
            cap: cap as u32,
            free_head: NIL,
            high_water: 0,
            next_id: 0,
            live: 0,
        })
    }

    /// A raw pointer to slot `i` (`i < cap`). The pool memory is never freed
    /// (monotonic metadata), so the pointer is valid for the process.
    #[inline]
    fn slot_ptr(&self, i: u32) -> *mut LargeSlot {
        debug_assert!(i < self.cap);
        // SAFETY: `i < cap`, so the offset is in bounds of the pool allocation.
        unsafe { self.slots.as_ptr().add(i as usize) }
    }

    /// Acquire a slot, returning `(idx, fresh)` — `fresh` means never-initialised
    /// (use [`LargeDescriptor::new`]); otherwise recycle it. `None` if the pool is full.
    fn acquire(&mut self) -> Option<(u32, bool)> {
        if self.free_head != NIL {
            let i = self.free_head;
            // SAFETY: `i < cap` (it was a valid slot index pushed by `release`).
            self.free_head = unsafe { (*self.slot_ptr(i)).free_next };
            self.live += 1;
            Some((i, false))
        } else if self.high_water < self.cap {
            let i = self.high_water;
            self.high_water += 1;
            self.live += 1;
            Some((i, true))
        } else {
            None
        }
    }

    /// Return slot `i` to the free stack.
    fn release(&mut self, i: u32) {
        // SAFETY: `i < cap`, a slot we handed out.
        unsafe { (*self.slot_ptr(i)).free_next = self.free_head };
        self.free_head = i;
        self.live -= 1;
    }

    /// Map a `*const LargeDescriptor` from the pagemap back to its slot index, or
    /// `None` if it does not point at a slot in this pool (a foreign/again-mismatched
    /// pointer — defense-in-depth).
    fn index_of(&self, desc: *const LargeDescriptor) -> Option<u32> {
        let base = self.slots.as_ptr() as usize;
        let p = desc as usize;
        if p < base {
            return None;
        }
        let stride = core::mem::size_of::<LargeSlot>();
        let off = p - base;
        if !off.is_multiple_of(stride) {
            return None;
        }
        let idx = off / stride;
        if idx < self.cap as usize {
            Some(idx as u32)
        } else {
            None
        }
    }
}

/// Sizing for a [`LargeAllocator`]: the reserved region, its alignment, and the
/// extent- and large-descriptor pool capacities. Grouped so construction stays a
/// few clear arguments.
#[derive(Clone, Copy, Debug)]
pub struct LargeConfig {
    /// Bytes of virtual region to reserve for large allocations.
    pub region_bytes: usize,
    /// Reservation alignment (at least a page).
    pub region_align: usize,
    /// Extent-descriptor pool capacity (back-end split/merge bookkeeping).
    pub extent_slots: usize,
    /// Large-descriptor pool capacity (max concurrent live large allocations).
    pub large_slots: usize,
}

/// The large-allocation allocator (§18.5): a back-end [`ExtentManager`] plus a
/// recycling [`LargeDescriptor`] pool, tied to a [`PageMap`] for classification.
/// `'a` is the lifetime of the borrowed pagemap and metadata source (both
/// process-lived in production, leaked in tests).
pub struct LargeAllocator<'a, P: TopoBackingProvider> {
    extents: ExtentManager<P>,
    pagemap: &'a PageMap,
    meta: &'a dyn MetadataAlloc,
    arena: ArenaId,
    lock: BackendLock,
    pool: UnsafeCell<LargePool>,
}

// SAFETY: all access to `pool` goes through `lock`; `extents`/`pagemap` carry their
// own synchronization; `meta` is `Sync`; `arena` is immutable. So concurrent
// `&self` use is data-race-free.
unsafe impl<P: TopoBackingProvider + Send + Sync> Sync for LargeAllocator<'_, P> {}
// SAFETY: the allocator owns its pool (metadata-backed, never aliased) and a `Send`
// extent manager; the borrowed `pagemap`/`meta` are `Sync`.
unsafe impl<P: TopoBackingProvider + Send> Send for LargeAllocator<'_, P> {}

impl<'a, P: TopoBackingProvider> LargeAllocator<'a, P> {
    /// Build a large allocator over a provider-reserved region (sized by `cfg`),
    /// classifying through `pagemap` and drawing metadata from `meta`.
    pub fn new(
        provider: P,
        meta: &'a dyn MetadataAlloc,
        pagemap: &'a PageMap,
        arena: ArenaId,
        cfg: LargeConfig,
    ) -> Result<Self, ExtentError> {
        let extents = ExtentManager::new(
            provider,
            meta,
            arena,
            cfg.region_bytes,
            cfg.region_align,
            cfg.extent_slots,
        )?;
        let pool = LargePool::new(meta, cfg.large_slots).ok_or(ExtentError::Exhausted)?;
        Ok(Self {
            extents,
            pagemap,
            meta,
            arena,
            lock: BackendLock::new(),
            pool: UnsafeCell::new(pool),
        })
    }

    /// The number of large allocations currently live.
    pub fn live_count(&self) -> usize {
        self.lock.acquire();
        // SAFETY: lock held ⇒ exclusive access to the pool.
        let n = unsafe { (*self.pool.get()).live } as usize;
        self.lock.release();
        n
    }

    /// Whether the back-end is well-formed (delegates to the extent manager).
    pub fn check_invariants(&self) -> bool {
        self.extents.check_invariants()
    }

    /// The backend name (the provider's).
    pub fn backend_name(&self) -> &'static str {
        self.extents.backend_name()
    }

    /// Allocate a large region of at least `bytes` aligned to `align`, install its
    /// [`LargeDescriptor`] in the pagemap, and return the base pointer (null on
    /// failure). Consults `hook` (the §18.6 region cache) first. **Bypasses the
    /// small-object path** (this layer never touches size classes, §18.5).
    ///
    /// On any failure the partial work is rolled back — the extent is freed (or the
    /// cache region returned), the descriptor slot released — so the allocator is
    /// well-formed (W4-5) and nothing leaks.
    ///
    /// SPEC-transition: `large_allocate` (§18.5) + pagemap publish (§17.2 P-Map-006)
    pub fn allocate_with(&self, bytes: usize, align: usize, hook: &dyn RegionCacheHook) -> *mut u8 {
        let (region, backing) = match self.extents.alloc_large(bytes, align, hook) {
            Ok(rb) => rb,
            Err(_) => return ptr::null_mut(),
        };

        self.lock.acquire();
        // SAFETY: lock held ⇒ exclusive access to the pool.
        let pool = unsafe { &mut *self.pool.get() };
        let acquired = pool.acquire();
        let (idx, fresh) = match acquired {
            Some(v) => v,
            None => {
                // Pool full: undo the extent/cache allocation and fail.
                self.lock.release();
                self.return_backing(backing, region, hook);
                return ptr::null_mut();
            }
        };
        let id = LargeId(pool.next_id);
        pool.next_id = pool.next_id.wrapping_add(1);
        let slot = pool.slot_ptr(idx);
        let usable = region.len;
        let base = region.base as usize;
        // Initialise the descriptor in place (install-before-publish, F1): fully
        // formed before it goes into the pagemap.
        // SAFETY: `slot` is a valid pool slot; a fresh slot's descriptor is zeroed
        // (no Drop) and overwritten, a reused one is recycled in place.
        unsafe {
            if fresh {
                (*slot).desc = LargeDescriptor::new(id, self.arena, base, usable, align);
            } else {
                (*slot).desc.recycle(self.arena, base, usable, align);
            }
            match backing {
                Some(ext) => {
                    (*slot).backing_id = ext.id.0;
                    (*slot).backing_gen = ext.generation;
                    (*slot).has_extent = 1;
                }
                None => (*slot).has_extent = 0,
            }
        }
        // Publish into the pagemap (the W3-6 mutator). On metadata exhaustion, roll
        // back fully.
        // SAFETY: `slot` is live and initialised; the descriptor lives in
        // never-freed metadata, so the pagemap pointer stays valid.
        let installed = unsafe { self.pagemap.install_large(self.meta, &(*slot).desc) };
        if installed.is_err() {
            pool.release(idx);
            self.lock.release();
            self.return_backing(backing, region, hook);
            return ptr::null_mut();
        }
        self.lock.release();
        region.base
    }

    /// Allocate with the default (no-op) region cache.
    pub fn allocate(&self, bytes: usize, align: usize) -> *mut u8 {
        self.allocate_with(bytes, align, &NoRegionCache)
    }

    /// Free a large allocation by base pointer (§17.5: `free` requires a base
    /// pointer). Returns `true` if `ptr` was a live large allocation of this
    /// allocator (and is now freed), `false` otherwise (null, foreign, or not a
    /// large pointer) — never acting on a non-owned pointer.
    ///
    /// SPEC-transition: `large free` (pagemap clear §17.2 + extent free §18.3)
    pub fn free_with(&self, ptr: *mut u8, hook: &dyn RegionCacheHook) -> bool {
        if ptr.is_null() {
            return false;
        }
        self.lock.acquire();
        // Resolve the descriptor *under the lock*. The first thread to free `ptr`
        // retires its pagemap entry (below) before releasing the lock, so a second
        // thread racing to free the SAME pointer re-reads `None` here and rejects —
        // it can never reach `index_of`/`release` on an already-retired slot. Doing
        // the lookup outside the lock would let both threads resolve the same slot
        // and double-release it (free-stack corruption → later double-vend), the
        // classic large double-free (§17.5, M-004).
        let desc_ptr = match self.pagemap.lookup(ptr as usize).large_ptr() {
            Some(p) => p,
            None => {
                self.lock.release();
                return false; // not live (foreign / small / released / already freed)
            }
        };
        // SAFETY: lock held ⇒ exclusive access to the pool.
        let pool = unsafe { &mut *self.pool.get() };
        let idx = match pool.index_of(desc_ptr) {
            Some(i) => i,
            None => {
                self.lock.release();
                return false; // a Large pointer not from this pool — not ours
            }
        };
        let slot = pool.slot_ptr(idx);
        // Capture the backing and the region before retiring/recycling.
        // SAFETY: `slot` is a live pool slot (it is in the pagemap).
        let (backing, region) = unsafe {
            let region = Region {
                base: (*slot).desc.base() as *mut u8,
                len: (*slot).desc.usable_size(),
            };
            let backing = if (*slot).has_extent != 0 {
                Some(ExtentRef {
                    id: ExtentId((*slot).backing_id),
                    generation: (*slot).backing_gen,
                })
            } else {
                None
            };
            // Retire the pagemap entry for the *old* address BEFORE the slot can be
            // recycled to a new address (so a classifier never resolves a stale
            // address to a recycled descriptor, §17.2 P-Map-006 / DD-1 F2).
            self.pagemap.retire_large(&(*slot).desc);
            (backing, region)
        };
        pool.release(idx);
        self.lock.release();

        // Return the backing outside the pool lock (the provider call is the slow,
        // §27.2-lowest step). A failed extent free still leaves us well-formed.
        self.return_backing(backing, region, hook);
        true
    }

    /// Free with the default region cache.
    pub fn free(&self, ptr: *mut u8) -> bool {
        self.free_with(ptr, &NoRegionCache)
    }

    /// The usable size of the large allocation at base `ptr`, or `None` if `ptr` is
    /// not a live large allocation of this allocator (§25.4 `usable_size`).
    pub fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        if ptr.is_null() {
            return None;
        }
        self.lock.acquire();
        // Resolve under the lock (as in `free_with`): a concurrent free retires the
        // pagemap entry and recycles the slot while holding this lock, so reading the
        // descriptor outside it could race a retire/recycle and observe a stale or
        // reused slot. Under the lock, `lookup` and the slot read are atomic w.r.t.
        // the free path.
        let res = match self.pagemap.lookup(ptr as usize).large_ptr() {
            Some(desc_ptr) => {
                // SAFETY: lock held ⇒ exclusive pool access.
                let pool = unsafe { &mut *self.pool.get() };
                pool.index_of(desc_ptr).map(|idx| {
                    // SAFETY: `idx` is a live slot under the lock; reading its
                    // usable size is sound.
                    unsafe { (*pool.slot_ptr(idx)).desc.usable_size() }
                })
            }
            None => None,
        };
        self.lock.release();
        res
    }

    /// Return an allocation's backing to wherever it came from: a cache-served
    /// region (`None` backing) is offered back to the region cache; an
    /// extent-served region is freed through the extent manager.
    fn return_backing(
        &self,
        backing: Option<ExtentRef>,
        region: Region,
        hook: &dyn RegionCacheHook,
    ) {
        match backing {
            Some(ext) => {
                let _ = self.extents.free(ext); // a failed free still leaves us well-formed
            }
            None => {
                // Cache-served (§18.6): offer it back; if the cache declines, the
                // region is simply dropped (the cache owns its lifecycle).
                let _ = hook.try_cache(region);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::generated::tables::PAGE_SIZE;
    use topo_backend_posix_in_test::PosixBackingProvider;

    const PAGE: usize = PAGE_SIZE;

    fn meta(bytes: usize) -> &'static BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: leaked buffer, live for the process.
        Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
    }

    fn pagemap() -> &'static PageMap {
        Box::leak(Box::new(PageMap::new()))
    }

    fn large(
        region_pages: usize,
        large_slots: usize,
    ) -> LargeAllocator<'static, PosixBackingProvider> {
        LargeAllocator::new(
            PosixBackingProvider::new(),
            meta(1 << 20),
            pagemap(),
            ArenaId::DEFAULT,
            LargeConfig {
                region_bytes: region_pages * PAGE,
                region_align: PAGE,
                extent_slots: 4096,
                large_slots,
            },
        )
        .expect("large allocator")
    }

    // A host-backed provider local to this test (topo-core cannot depend on
    // topo-backend-posix — that crate depends on topo-core). Mirrors its behaviour.
    mod topo_backend_posix_in_test {
        use crate::backend::{Region, TopoBackingProvider};
        use crate::ids::ArenaId;
        use crate::BackendError;
        use std::alloc::{alloc, dealloc, Layout};
        use std::sync::Mutex;

        #[derive(Default)]
        pub(super) struct PosixBackingProvider {
            owned: Mutex<Vec<(usize, Layout)>>,
        }
        impl PosixBackingProvider {
            pub(super) fn new() -> Self {
                Self::default()
            }
        }
        impl TopoBackingProvider for PosixBackingProvider {
            fn reserve(
                &self,
                _a: ArenaId,
                size: usize,
                align: usize,
            ) -> Result<Region, BackendError> {
                if size == 0 || !align.is_power_of_two() {
                    return Err(BackendError::InvalidRequest);
                }
                let l = Layout::from_size_align(size, align)
                    .map_err(|_| BackendError::InvalidRequest)?;
                // SAFETY: nonzero size + power-of-two align.
                let base = unsafe { alloc(l) };
                if base.is_null() {
                    return Err(BackendError::OutOfMemory);
                }
                self.owned.lock().unwrap().push((base as usize, l));
                Ok(Region { base, len: size })
            }
            fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
                Ok(())
            }
            fn release(&self, _a: ArenaId, region: Region) -> Result<(), BackendError> {
                let mut o = self.owned.lock().unwrap();
                let base = region.base as usize;
                let i = o
                    .iter()
                    .position(|&(b, _)| b == base)
                    .ok_or(BackendError::InvalidRequest)?;
                let (_, l) = o.swap_remove(i);
                // SAFETY: exactly the pointer/layout from `reserve`.
                unsafe { dealloc(base as *mut u8, l) };
                Ok(())
            }
            fn name(&self) -> &'static str {
                "posix-test"
            }
        }
        impl Drop for PosixBackingProvider {
            fn drop(&mut self) {
                for (b, l) in self.owned.get_mut().unwrap().drain(..) {
                    // SAFETY: as `release`.
                    unsafe { dealloc(b as *mut u8, l) };
                }
            }
        }
    }

    #[test]
    fn allocate_is_classifiable_and_freeable() {
        let la = large(64, 16);
        let p = la.allocate(3 * PAGE, PAGE);
        assert!(!p.is_null());
        assert_eq!(p as usize % PAGE, 0);
        // Classifiable: an interior pointer resolves to the allocation.
        assert_eq!(la.usable_size(p), Some(3 * PAGE));
        assert_eq!(la.live_count(), 1);
        // SAFETY: committed for its whole length.
        unsafe {
            p.write(0xab);
            assert_eq!(p.read(), 0xab);
        }
        assert!(la.free(p), "free of a live large allocation succeeds");
        assert_eq!(la.live_count(), 0);
        // After free the pointer no longer classifies as a live large allocation.
        assert_eq!(la.usable_size(p), None);
        assert!(la.check_invariants());
    }

    #[test]
    fn free_of_foreign_or_null_pointer_is_rejected() {
        let la = large(16, 8);
        assert!(!la.free(ptr::null_mut()), "free(NULL) is a no-op");
        let mut x = 0u8;
        assert!(!la.free(&mut x as *mut u8), "foreign pointer is not freed");
        let p = la.allocate(2 * PAGE, PAGE);
        assert!(la.free(p));
        // Double free: the pointer no longer classifies as large.
        assert!(!la.free(p), "double free is rejected");
        assert!(la.check_invariants());
    }

    #[test]
    fn descriptors_recycle_without_leaking_metadata() {
        // Many alloc/free cycles with a tiny pool must succeed (the descriptor pool
        // recycles in place — no per-allocation metadata leak).
        let la = large(64, 4);
        for _ in 0..200 {
            let p = la.allocate(2 * PAGE, PAGE);
            assert!(!p.is_null(), "recycled descriptor + extent reused");
            assert!(la.free(p));
        }
        assert_eq!(la.live_count(), 0);
        assert!(la.check_invariants());
    }

    #[test]
    fn pool_exhaustion_fails_safely_and_recovers() {
        // With 2 descriptor slots, a 3rd concurrent live allocation fails cleanly and
        // rolls back its extent (no leak), then succeeds again after a free.
        let la = large(64, 2);
        let a = la.allocate(2 * PAGE, PAGE);
        let b = la.allocate(2 * PAGE, PAGE);
        assert!(!a.is_null() && !b.is_null());
        let c = la.allocate(2 * PAGE, PAGE);
        assert!(c.is_null(), "3rd live allocation exceeds the 2-slot pool");
        assert!(
            la.check_invariants(),
            "rolled-back allocation left us well-formed"
        );
        assert!(la.free(a));
        let d = la.allocate(2 * PAGE, PAGE);
        assert!(!d.is_null(), "a freed slot is reusable");
        assert!(la.free(b) && la.free(d));
        assert!(la.check_invariants());
    }

    #[test]
    fn concurrent_large_alloc_free_stays_consistent() {
        use std::sync::Arc;
        let la = Arc::new(large(1024, 256));
        std::thread::scope(|s| {
            for _ in 0..6 {
                let la = Arc::clone(&la);
                s.spawn(move || {
                    for _ in 0..150 {
                        let p = la.allocate(2 * PAGE, PAGE);
                        if !p.is_null() {
                            // SAFETY: committed for its whole length.
                            unsafe { p.write(0x5a) };
                            assert!(la.free(p));
                        }
                    }
                });
            }
        });
        assert_eq!(la.live_count(), 0);
        assert!(la.check_invariants());
    }

    #[test]
    fn concurrent_double_free_of_same_pointer_is_safe() {
        // Regression (audit Finding #2): the descriptor lookup must happen *under*
        // the pool lock. Otherwise two threads racing to free the SAME pointer both
        // resolve its slot before either retires it, then both `pool.release(idx)` —
        // double-releasing the slot (free-stack self-cycle → later double-vend) and
        // double-freeing the extent. With the lookup under the lock, the first freer
        // retires the pagemap entry before unlocking, so every racing freer re-reads
        // `None` and rejects. Contract: exactly ONE free wins per pointer, the pool
        // stays well-formed, and slots are cleanly reusable (no double-vend).
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        const PTRS: usize = 64;
        const FREERS: usize = 4;

        let la = Arc::new(large(1024, 256));
        // A batch of live allocations whose bases are shared across every thread.
        let ptrs: Vec<usize> = (0..PTRS)
            .map(|_| {
                let p = la.allocate(2 * PAGE, PAGE);
                assert!(!p.is_null());
                p as usize
            })
            .collect();
        assert_eq!(la.live_count(), PTRS);

        let ptrs = Arc::new(ptrs);
        let wins = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(FREERS));
        std::thread::scope(|s| {
            for _ in 0..FREERS {
                let la = Arc::clone(&la);
                let ptrs = Arc::clone(&ptrs);
                let wins = Arc::clone(&wins);
                let barrier = Arc::clone(&barrier);
                s.spawn(move || {
                    barrier.wait(); // release all freers together to widen the race
                    for &p in ptrs.iter() {
                        if la.free(p as *mut u8) {
                            wins.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });

        // Never zero (a lost free) and never more than one (a double free) per ptr.
        assert_eq!(
            wins.load(Ordering::Relaxed),
            PTRS,
            "exactly one free won per pointer"
        );
        assert_eq!(la.live_count(), 0);
        assert!(
            la.check_invariants(),
            "pool well-formed after the double-free race"
        );

        // Slots must be cleanly reusable: a double-vend would corrupt the free stack
        // and either fail to allocate or hand back an aliasing pointer.
        let mut reuse = Vec::with_capacity(PTRS);
        for _ in 0..PTRS {
            let p = la.allocate(2 * PAGE, PAGE);
            assert!(
                !p.is_null(),
                "slots reusable after the race (no double-vend)"
            );
            reuse.push(p as usize);
        }
        let mut distinct = reuse.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), PTRS, "no two reused slots alias");
        for p in reuse {
            assert!(la.free(p as *mut u8));
        }
        assert!(la.check_invariants());
    }

    #[test]
    fn cache_served_allocation_bypasses_extents_and_returns_to_the_cache() {
        // §18.6 region cache (C13): a hook may serve an "awkward"-sized region itself
        // (`backing == None`). Such an allocation must still classify and be writable,
        // and on free be offered back to the cache via `try_cache` — never routed to
        // the extent manager. This is the cache-served path no other test exercises
        // (all others use `NoRegionCache`, whose defaults decline both calls).
        use std::alloc::{alloc, dealloc, Layout};
        use std::cell::Cell;

        struct OneShotCache {
            base: *mut u8,
            len: usize,
            layout: Layout,
            served: Cell<bool>,
            returned: Cell<bool>,
        }
        impl OneShotCache {
            fn new(len: usize) -> Self {
                let layout = Layout::from_size_align(len, PAGE).unwrap();
                // SAFETY: nonzero, page-aligned layout.
                let base = unsafe { alloc(layout) };
                assert!(!base.is_null());
                Self {
                    base,
                    len,
                    layout,
                    served: Cell::new(false),
                    returned: Cell::new(false),
                }
            }
        }
        impl RegionCacheHook for OneShotCache {
            fn try_alloc(&self, bytes: usize, align: usize) -> Option<Region> {
                if !self.served.get() && bytes <= self.len && align <= PAGE {
                    self.served.set(true);
                    Some(Region {
                        base: self.base,
                        len: self.len,
                    })
                } else {
                    None
                }
            }
            fn try_cache(&self, region: Region) -> bool {
                assert_eq!(region.base, self.base, "freed region offered back to cache");
                self.returned.set(true);
                true // the cache takes ownership
            }
        }
        impl Drop for OneShotCache {
            fn drop(&mut self) {
                // SAFETY: exactly the pointer/layout from `new`.
                unsafe { dealloc(self.base, self.layout) };
            }
        }

        let la = large(64, 16);
        let cache = OneShotCache::new(3 * PAGE);
        // Alloc: the cache serves it (`backing == None`), bypassing the extent manager.
        let p = la.allocate_with(3 * PAGE, PAGE, &cache);
        assert!(!p.is_null());
        assert_eq!(p, cache.base, "served from the cache region");
        assert!(cache.served.get());
        // Classifiable + writable through the normal pagemap path.
        assert_eq!(la.usable_size(p), Some(3 * PAGE));
        assert_eq!(la.live_count(), 1);
        // SAFETY: the cache region is committed host memory of `3 * PAGE` bytes.
        unsafe {
            p.write(0x77);
            assert_eq!(p.read(), 0x77);
        }
        // Free: offered back to the cache (`try_cache`), not freed through the extents.
        assert!(la.free_with(p, &cache));
        assert!(
            cache.returned.get(),
            "freed cache-served region returned to the cache"
        );
        assert_eq!(la.live_count(), 0);
        assert_eq!(la.usable_size(p), None);
        assert!(la.check_invariants());
    }
}

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

/// Byte size of one large-descriptor slot, exposed so sizing helpers can pin
/// their per-slot bound at compile time (W8; see `extent::EXTENT_SLOT_BYTES`).
pub(crate) const LARGE_SLOT_BYTES: usize = core::mem::size_of::<LargeSlot>();

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

    /// The §20.1 physical-state byte breakdown of the large region (delegates
    /// to the extent manager) — the W8 stats-reconciliation input (§8.6).
    pub fn state_bytes(&self) -> crate::extent::StateBytes {
        self.extents.state_bytes()
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
        self.allocate_with_in(self.arena, bytes, align, hook)
    }

    /// As [`allocate_with`](Self::allocate_with), but tags the resulting
    /// [`LargeDescriptor`] with the **requesting** arena (plan 06 W9), so the
    /// large allocation retains its arena identity for isolation (§22.7) and for
    /// per-arena reset/destroy ([`free_arena`](Self::free_arena)). The shared
    /// region is still reserved under the manager's region arena; only the
    /// per-allocation descriptor carries `arena`.
    pub fn allocate_with_in(
        &self,
        arena: ArenaId,
        bytes: usize,
        align: usize,
        hook: &dyn RegionCacheHook,
    ) -> *mut u8 {
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
                self.return_backing(backing, region, hook, None);
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
                (*slot).desc = LargeDescriptor::new(id, arena, base, usable, align);
            } else {
                (*slot).desc.recycle(arena, base, usable, align);
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
            self.return_backing(backing, region, hook, None);
            return ptr::null_mut();
        }
        self.lock.release();
        region.base
    }

    /// Allocate with the default (no-op) region cache.
    pub fn allocate(&self, bytes: usize, align: usize) -> *mut u8 {
        self.allocate_with(bytes, align, &NoRegionCache)
    }

    /// Allocate from arena `arena` with the default region cache (plan 06 W9).
    pub fn allocate_in(&self, arena: ArenaId, bytes: usize, align: usize) -> *mut u8 {
        self.allocate_with_in(arena, bytes, align, &NoRegionCache)
    }

    /// The owning arena of the live large allocation based at `ptr`, or `None`
    /// if `ptr` is not a live large allocation of this allocator. Resolved under
    /// the pool lock (as [`usable_size`](Self::usable_size)) so it never races a
    /// concurrent free/recycle. Used by the free path to credit the right arena's
    /// quota (plan 06 W9).
    pub fn arena_of(&self, ptr: *mut u8) -> Option<ArenaId> {
        if ptr.is_null() {
            return None;
        }
        self.lock.acquire();
        let res = match self.pagemap.lookup(ptr as usize).large_ptr() {
            Some(desc_ptr) => {
                // SAFETY: lock held ⇒ exclusive pool access.
                let pool = unsafe { &mut *self.pool.get() };
                pool.index_of(desc_ptr).map(|idx| {
                    // SAFETY: `idx` is a live slot under the lock.
                    unsafe { (*pool.slot_ptr(idx)).desc.arena() }
                })
            }
            None => None,
        };
        self.lock.release();
        res
    }

    /// Free **every** live large allocation belonging to `arena` (plan 06 W9-4c
    /// / W9-6: the large-side of arena reset/destroy). Returns `(count, bytes,
    /// fully_drained)` — the number freed, their total usable bytes (for the
    /// caller's global `freed_bytes` accounting, since this frees at the
    /// large-allocator level below the engine's counters), and whether the arena
    /// was fully drained (no live large could not be freed). A `false`
    /// `fully_drained` is the §36.13 partial-failure signal the caller turns into
    /// a quarantine.
    ///
    /// Each iteration finds one live large of `arena` (its pagemap entry still
    /// points at its descriptor) under the pool lock, then frees it via
    /// [`free_revoking`](Self::free_revoking) — which retires the pagemap entry
    /// (so the next scan cannot re-find it, guaranteeing progress) and revokes the
    /// backing's descendants before recycling (§36.6/§36.13). The scan reads
    /// descriptor fields only under the pool lock, so it never races a concurrent
    /// recycle of *another* arena's slot; the SPEC §22.5 precondition (the arena
    /// being drained is quiesced) means no concurrent mutation of *this* arena's
    /// allocations. Every object is retired (so the arena's live bytes go to
    /// zero); `fully_drained` is `false` iff some backing revoke failed — those
    /// extents stay allocated and well-formed, and the caller quarantines.
    ///
    /// # Safety
    ///
    /// The caller guarantees `arena` is quiesced (no thread holds or is freeing
    /// its large allocations) — the §22.5/§36.13 reset/destroy precondition. Each
    /// freed pointer is a live base pointer this allocator handed out, so the
    /// per-free [`free`](Self::free) contract is met.
    pub unsafe fn free_arena(&self, arena: ArenaId) -> (usize, usize, bool) {
        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut all_revoked = true;
        loop {
            // Find one live large of `arena` under the pool lock.
            self.lock.acquire();
            // SAFETY: lock held ⇒ exclusive pool access; slots are never freed.
            let found = unsafe {
                let pool = &mut *self.pool.get();
                let hw = pool.high_water;
                let mut found: Option<(usize, usize)> = None;
                let mut idx = 0u32;
                while idx < hw {
                    let slot = pool.slot_ptr(idx);
                    let desc_ptr = &(*slot).desc as *const LargeDescriptor;
                    let base = (*slot).desc.base();
                    // Live iff the pagemap still resolves its base to this very
                    // descriptor (a freed slot's entry was retired). This also
                    // confirms `base` is the descriptor's current base.
                    let live = self.pagemap.lookup(base).large_ptr() == Some(desc_ptr);
                    if live && (*slot).desc.arena() == arena {
                        found = Some((base, (*slot).desc.usable_size()));
                        break;
                    }
                    idx += 1;
                }
                found
            };
            self.lock.release();

            match found {
                Some((base, usable)) => {
                    // SAFETY: `base` is the current base pointer of a live large
                    // allocation of this allocator (the pagemap confirmed it under
                    // the lock); freeing it meets the `free` contract. Revoke the
                    // backing's descendants before recycling (§36.6/§36.13).
                    let (retired, revoked) = unsafe { self.free_revoking(base as *mut u8, arena) };
                    if retired {
                        // The object is gone regardless of revoke; count its bytes.
                        count += 1;
                        bytes += usable;
                        all_revoked &= revoked;
                    } else {
                        // A live large we somehow could not retire: stop so the
                        // caller quarantines rather than spinning (defensive).
                        return (count, bytes, false);
                    }
                }
                // All of this arena's larges are retired; the arena is fully
                // drained iff every backing revoke also succeeded (§36.13).
                None => return (count, bytes, all_revoked),
            }
        }
    }

    /// Free a large allocation by base pointer (§17.5: `free` requires a base
    /// pointer). Returns `true` if `ptr` was a live large allocation of this
    /// allocator (and is now freed), `false` otherwise (null, foreign, or not a
    /// large pointer) — never acting on a non-owned pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be null, a live allocation of this allocator that the caller
    /// still owns, or memory never returned by it (rejected harmlessly). What
    /// the pagemap lookup cannot detect — and what this contract excludes — is
    /// a *stale* pointer whose address has been recycled into a different live
    /// large allocation: freeing it would release another owner's object
    /// (the same contract as [`Allocator::free`](crate::Allocator::free), W8).
    ///
    /// SPEC-transition: `large free` (pagemap clear §17.2 + extent free §18.3)
    pub unsafe fn free_with(&self, ptr: *mut u8, hook: &dyn RegionCacheHook) -> bool {
        // SAFETY: identical contract, forwarded; `None` = no capability revoke.
        unsafe { self.free_inner(ptr, hook, None) }.0
    }

    /// Free a live large of `arena`, **revoking its backing's descendants before
    /// recycling** (§36.6/§36.13, plan 06 W9-6d). Returns `(retired,
    /// backing_reclaimed)`: `retired` is whether `ptr` was a live large (now
    /// retired — its object is gone either way), `backing_reclaimed` is whether
    /// the backing was revoked and recycled (`false` ⇒ a revoke failure left the
    /// extent allocated, the §36.13 partial-failure signal). Used by
    /// [`free_arena`](Self::free_arena).
    ///
    /// # Safety
    ///
    /// As [`free_with`](Self::free_with).
    pub unsafe fn free_revoking(&self, ptr: *mut u8, arena: ArenaId) -> (bool, bool) {
        // SAFETY: identical contract, forwarded; `Some(arena)` = revoke first.
        unsafe { self.free_inner(ptr, &NoRegionCache, Some(arena)) }
    }

    /// The shared body of the large-free paths. `revoke` selects whether the
    /// backing is recycled via [`ExtentManager::free_revoking`] (capability
    /// revoke first) or plain [`free`](ExtentManager::free). Returns `(retired,
    /// backing_reclaimed)` — see [`free_revoking`](Self::free_revoking).
    ///
    /// # Safety
    ///
    /// As [`free_with`](Self::free_with).
    unsafe fn free_inner(
        &self,
        ptr: *mut u8,
        hook: &dyn RegionCacheHook,
        revoke: Option<ArenaId>,
    ) -> (bool, bool) {
        if ptr.is_null() {
            return (false, false);
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
                return (false, false); // not live (foreign / small / released / already freed)
            }
        };
        // SAFETY: lock held ⇒ exclusive access to the pool.
        let pool = unsafe { &mut *self.pool.get() };
        let idx = match pool.index_of(desc_ptr) {
            Some(i) => i,
            None => {
                self.lock.release();
                return (false, false); // a Large pointer not from this pool — not ours
            }
        };
        let slot = pool.slot_ptr(idx);
        // §17.5: `free` requires the allocation's *base* pointer. Every page the large
        // allocation covers maps to the same descriptor in the pagemap, so an interior
        // pointer (`base + k`) also resolves here — but it is NOT a valid free. Reject
        // it (returning `false`, like `ptr_class::classify_in_large`'s `Interior`)
        // rather than retiring/releasing the whole allocation out from under live use.
        // SAFETY: `slot` is a live pool slot (it is in the pagemap).
        if ptr as usize != unsafe { (*slot).desc.base() } {
            self.lock.release();
            return (false, false);
        }
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
        // §27.2-lowest step). A failed extent free still leaves us well-formed;
        // a failed *revoke* (drain path) is reported so the caller quarantines.
        let reclaimed = self.return_backing(backing, region, hook, revoke);
        (true, reclaimed)
    }

    /// Free with the default region cache.
    ///
    /// # Safety
    ///
    /// As [`free_with`](Self::free_with).
    pub unsafe fn free(&self, ptr: *mut u8) -> bool {
        // SAFETY: identical contract, forwarded.
        unsafe { self.free_with(ptr, &NoRegionCache) }
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
    ///
    /// When `revoke` is `Some(arena)` (the arena destroy/drain path, plan 06
    /// W9-6d) an extent-backed region is reclaimed through
    /// [`ExtentManager::free_revoking`] — its descendant capabilities are revoked
    /// before it is recycled (§36.6/§36.13). Returns `true` if the backing was
    /// fully reclaimed (no revoke requested, revoke succeeded, or cache-served);
    /// `false` only when a requested revoke failed (the extent then stays
    /// allocated and well-formed, the §36.13 partial-failure signal).
    fn return_backing(
        &self,
        backing: Option<ExtentRef>,
        region: Region,
        hook: &dyn RegionCacheHook,
        revoke: Option<ArenaId>,
    ) -> bool {
        match backing {
            Some(ext) => match revoke {
                Some(arena) => self.extents.free_revoking(ext, arena).is_ok(),
                // a failed free still leaves us well-formed (W4-5)
                None => {
                    let _ = self.extents.free(ext);
                    true
                }
            },
            None => {
                // Cache-served (§18.6): offer it back; if the cache declines, the
                // region is simply dropped (the cache owns its lifecycle).
                let _ = hook.try_cache(region);
                true
            }
        }
    }
}

/// A **type-erased view** of a [`LargeAllocator`] for per-arena hooked regions
/// (plan 06 W10). Like [`ExtentBacking`](crate::ExtentBacking) for the span path,
/// this lets the allocator route a large allocation to an arena's **own**
/// [`HookProvider`](crate::HookProvider)-backed large allocator without being
/// generic over the provider at the call site — the shared default large allocator
/// and a per-arena hooked one are different `LargeAllocator<P>` instantiations, but
/// both are `&dyn LargeBacking`. Each method resolves `ptr` against the backend's
/// **own** descriptor pool, so a query on a backend that does not own `ptr` returns
/// `None`/`false`/`0` — which is exactly how the free path finds the owner (the one
/// backend whose [`arena_of`](Self::arena_of) is `Some`).
pub trait LargeBacking {
    /// Allocate a large region for `arena`; null on failure (W9 arena tagging).
    fn allocate_in(&self, arena: ArenaId, bytes: usize, align: usize) -> *mut u8;
    /// The usable size of the live large allocation at `ptr` *in this backend*.
    fn usable_size(&self, ptr: *mut u8) -> Option<usize>;
    /// The owning arena of the live large allocation at `ptr` *in this backend*.
    fn arena_of(&self, ptr: *mut u8) -> Option<ArenaId>;
    /// Free the large allocation at `ptr` *if it belongs to this backend*.
    ///
    /// # Safety
    /// `ptr` is a base pointer this allocator handed out (or null/foreign — those
    /// are rejected); the caller upholds the [`LargeAllocator::free`] contract.
    unsafe fn free(&self, ptr: *mut u8) -> bool;
    /// Free every live large of `arena` (reset/destroy); see
    /// [`LargeAllocator::free_arena`].
    ///
    /// # Safety
    /// `arena` is quiesced (the §22.5/§36.13 precondition).
    unsafe fn free_arena(&self, arena: ArenaId) -> (usize, usize, bool);
    /// Number of large allocations currently live in this backend.
    fn live_count(&self) -> usize;
    /// The §20.1 physical-state byte breakdown of this backend's large region.
    fn state_bytes(&self) -> crate::extent::StateBytes;
    /// Whether this backend's back-end is well-formed.
    fn check_invariants(&self) -> bool;
}

impl<P: TopoBackingProvider> LargeBacking for LargeAllocator<'_, P> {
    #[inline]
    fn allocate_in(&self, arena: ArenaId, bytes: usize, align: usize) -> *mut u8 {
        LargeAllocator::allocate_in(self, arena, bytes, align)
    }
    #[inline]
    fn usable_size(&self, ptr: *mut u8) -> Option<usize> {
        LargeAllocator::usable_size(self, ptr)
    }
    #[inline]
    fn arena_of(&self, ptr: *mut u8) -> Option<ArenaId> {
        LargeAllocator::arena_of(self, ptr)
    }
    #[inline]
    unsafe fn free(&self, ptr: *mut u8) -> bool {
        // SAFETY: forwarded unchanged from the trait's `free` contract.
        unsafe { LargeAllocator::free(self, ptr) }
    }
    #[inline]
    unsafe fn free_arena(&self, arena: ArenaId) -> (usize, usize, bool) {
        // SAFETY: forwarded unchanged from the trait's `free_arena` contract.
        unsafe { LargeAllocator::free_arena(self, arena) }
    }
    #[inline]
    fn live_count(&self) -> usize {
        LargeAllocator::live_count(self)
    }
    #[inline]
    fn state_bytes(&self) -> crate::extent::StateBytes {
        LargeAllocator::state_bytes(self)
    }
    #[inline]
    fn check_invariants(&self) -> bool {
        LargeAllocator::check_invariants(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::generated::tables::PAGE_SIZE;
    use topo_backend_posix_in_test::PosixBackingProvider;

    /// Test wrappers: every pointer the tests free here is one they just
    /// received and still own, null, or a never-owned probe the entry point is
    /// specified to reject — exactly the `free`/`free_with` safety contracts.
    fn tfree<P: TopoBackingProvider>(la: &LargeAllocator<'_, P>, p: *mut u8) -> bool {
        // SAFETY: the caller-ownership contract above.
        unsafe { la.free(p) }
    }
    fn tfree_with<P: TopoBackingProvider>(
        la: &LargeAllocator<'_, P>,
        p: *mut u8,
        hook: &dyn RegionCacheHook,
    ) -> bool {
        // SAFETY: as `tfree`.
        unsafe { la.free_with(p, hook) }
    }

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

    // A region-cache hook (§18.6, C13) that serves exactly its first matching request
    // from one owned region and declines the rest, counting how many freed regions it
    // accepts back. Shared by the cache-routing tests.
    use std::alloc::{alloc, dealloc, Layout};
    use std::cell::Cell;

    struct OneShotCache {
        base: *mut u8,
        len: usize,
        layout: Layout,
        served: Cell<bool>,
        returns: Cell<usize>,
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
                returns: Cell::new(0),
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
            assert_eq!(
                region.base, self.base,
                "only the cache-served region returns here"
            );
            self.returns.set(self.returns.get() + 1);
            true // the cache takes ownership
        }
    }
    impl Drop for OneShotCache {
        fn drop(&mut self) {
            // SAFETY: exactly the pointer/layout from `new`.
            unsafe { dealloc(self.base, self.layout) };
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
        assert!(tfree(&la, p), "free of a live large allocation succeeds");
        assert_eq!(la.live_count(), 0);
        // After free the pointer no longer classifies as a live large allocation.
        assert_eq!(la.usable_size(p), None);
        assert!(la.check_invariants());
    }

    #[test]
    fn free_of_foreign_or_null_pointer_is_rejected() {
        let la = large(16, 8);
        assert!(!tfree(&la, ptr::null_mut()), "free(NULL) is a no-op");
        let mut x = 0u8;
        assert!(
            !tfree(&la, &mut x as *mut u8),
            "foreign pointer is not freed"
        );
        let p = la.allocate(2 * PAGE, PAGE);
        assert!(tfree(&la, p));
        // Double free: the pointer no longer classifies as large.
        assert!(!tfree(&la, p), "double free is rejected");
        assert!(la.check_invariants());
    }

    #[test]
    fn free_of_interior_pointer_is_rejected() {
        // §17.5 / audit: `free` requires the *base* pointer. An interior pointer into
        // a large allocation (which resolves to the same descriptor via the pagemap)
        // must NOT free it — that would retire/release the whole allocation out from
        // under live use. It returns `false` and leaves the allocation intact.
        let la = large(64, 16);
        let p = la.allocate(3 * PAGE, PAGE);
        assert!(!p.is_null());
        // SAFETY: `p .. p + 3*PAGE` is the live allocation; `p + PAGE` is interior.
        let interior = unsafe { p.add(PAGE) };
        assert!(!tfree(&la, interior), "interior free is rejected");
        assert_eq!(
            la.live_count(),
            1,
            "allocation still live after interior free"
        );
        assert_eq!(la.usable_size(p), Some(3 * PAGE), "allocation intact");
        // The base pointer still frees correctly.
        assert!(tfree(&la, p), "base free succeeds");
        assert_eq!(la.live_count(), 0);
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
            assert!(tfree(&la, p));
        }
        assert_eq!(la.live_count(), 0);
        assert!(la.check_invariants());
    }

    #[test]
    fn new_fails_cleanly_on_metadata_exhaustion() {
        // W4-5 safe failure: a metadata arena too small for the slot pools makes `new`
        // return an error rather than panicking or half-initialising. (The extent
        // manager releases its reservation on this path, so nothing leaks.)
        let r = LargeAllocator::new(
            PosixBackingProvider::new(),
            meta(64), // far too small for the 4096-slot extent pool below
            pagemap(),
            ArenaId::DEFAULT,
            LargeConfig {
                region_bytes: 64 * PAGE,
                region_align: PAGE,
                extent_slots: 4096,
                large_slots: 16,
            },
        );
        assert!(r.is_err(), "construction over an exhausted arena must fail");
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
        assert!(tfree(&la, a));
        let d = la.allocate(2 * PAGE, PAGE);
        assert!(!d.is_null(), "a freed slot is reusable");
        assert!(tfree(&la, b) && tfree(&la, d));
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
                            assert!(tfree(&la, p));
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
                        if tfree(&la, p as *mut u8) {
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
            assert!(tfree(&la, p as *mut u8));
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
        assert!(tfree_with(&la, p, &cache));
        assert_eq!(
            cache.returns.get(),
            1,
            "freed cache-served region returned to the cache"
        );
        assert_eq!(la.live_count(), 0);
        assert_eq!(la.usable_size(p), None);
        assert!(la.check_invariants());
    }

    #[test]
    fn mixed_cache_served_and_extent_backed_route_correctly_on_free() {
        // §18.6: within one allocator a hook may serve SOME allocations
        // (`backing == None`) and decline others (extent-backed). On free each must go
        // back to its own owner — the cache-served one to `try_cache`, the
        // extent-backed one to the extent manager — driven by the descriptor's
        // `has_extent` flag. Covers the mixed routing the single-allocation test does
        // not.
        let la = large(64, 16);
        let cache = OneShotCache::new(3 * PAGE); // serves exactly the first request

        let served = la.allocate_with(3 * PAGE, PAGE, &cache);
        assert!(!served.is_null());
        assert_eq!(served, cache.base, "first alloc served from the cache");
        let backed = la.allocate_with(3 * PAGE, PAGE, &cache); // cache now declines
        assert!(!backed.is_null());
        assert_ne!(
            backed, cache.base,
            "second alloc came from the extent manager"
        );
        assert_eq!(la.live_count(), 2);

        // Free the extent-backed one first: routed to the extents, never to the cache.
        assert!(tfree_with(&la, backed, &cache));
        assert_eq!(
            cache.returns.get(),
            0,
            "an extent-backed free must not touch the cache"
        );
        // Free the cache-served one: routed back to the cache exactly once.
        assert!(tfree_with(&la, served, &cache));
        assert_eq!(
            cache.returns.get(),
            1,
            "cache-served free returns to the cache"
        );
        assert_eq!(la.live_count(), 0);
        assert!(la.check_invariants());
    }
}

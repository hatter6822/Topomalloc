// SPDX-License-Identifier: MIT
//! The C extent-hook ABI (§23.2, plan 06 W10): `topo_extent_hooks_t` and
//! `topo_arena_create_hooked`, so a C/C++ application can serve an arena's
//! span/large allocations from its **own** custom backing (a memory source / OS
//! policy) — the C-struct surface §23.2 presents, over the Rust [`ExtentHooks`]
//! mechanism and the per-arena hooked regions ([`Allocator::arena_create_hooked`](
//! topo_core::Allocator::arena_create_hooked)).
//!
//! The C `bool` returns follow the jemalloc convention §23.2 references: a hook
//! returns `true` on **failure**. `alloc`/`dealloc` are required; a NULL
//! `commit`/`decommit`/`purge_*`/`split`/`merge` pointer means "use the default"
//! (a no-op — a backing that hands out committed, never-reclaimed memory and does
//! not track sub-extent boundaries). The §23.3 output contracts are still enforced
//! by the underlying `HookProvider` (a misaligned/undersized/out-of-range result is
//! rejected, never trusted).

use core::ffi::c_void;
use std::sync::Mutex;

use topo_core::{
    AllocatorConfig, ArenaPolicy, BackendError, ExtentHooks, Region, MAX_HOOK_BACKENDS,
};

use crate::arena_api::arena_errno;
use crate::errno_shim::{set_errno, EINVAL};
use crate::global;

/// Tracks each live hooked arena's `CHooks` adapter (`arena_id → box address`), so
/// it is **reclaimed** on `topo_arena_destroy` (and on a failed create) rather than
/// leaked. Bounded: one entry per live hooked arena (pushed on create, removed on
/// destroy), so a create/destroy loop does not grow the heap. Addresses are stored
/// as `usize` (a raw `*mut CHooks` is not `Send`); the `Mutex` makes it thread-safe.
static CHOOKS_REGISTRY: Mutex<Vec<(u32, usize)>> = Mutex::new(Vec::new());

/// Reclaim the `CHooks` adapter tracked for arena `id`, if any — called after a
/// **successful** `topo_arena_destroy`. By then the allocator has unregistered the
/// arena's hooked backend (dropped its `HookProvider<&CHooks>`), so no live
/// reference to the adapter remains and freeing it is sound. A no-op for a
/// non-hooked arena (or a quarantined destroy, which keeps its resources).
pub(crate) fn reclaim_chooks(id: u32) {
    let addr = {
        let mut reg = CHOOKS_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .position(|&(a, _)| a == id)
            .map(|i| reg.swap_remove(i).1)
    };
    if let Some(addr) = addr {
        // SAFETY: `addr` is exactly a `Box::into_raw(CHooks)` from a successful
        // create; the arena is now destroyed and its backend unregistered, so
        // nothing references it. Reconstituting + dropping the box frees it once.
        drop(unsafe { Box::from_raw(addr as *mut CHooks) });
    }
}

/// The §23.2 C extent-hook vtable. Every field is a nullable function pointer
/// (`Option<extern fn>` has the null-pointer layout of the bare pointer, so this
/// matches the C `struct topo_extent_hooks_s` exactly). `ctx` is passed separately
/// to [`topo_arena_create_hooked`] (jemalloc passes the hooks struct itself as the
/// context; we keep an explicit `ctx` so the same vtable can back many arenas).
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct topo_extent_hooks_t {
    /// `void* alloc(ctx, size, alignment, bool* zero, bool* commit)` — **required**.
    pub alloc: Option<
        unsafe extern "C" fn(*mut c_void, usize, usize, *mut bool, *mut bool) -> *mut c_void,
    >,
    /// `bool dealloc(ctx, addr, size, committed)` — **required** (true ⇒ failure).
    pub dealloc: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, bool) -> bool>,
    /// `bool commit(ctx, addr, size, offset, length)` (NULL ⇒ no-op default).
    pub commit: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize) -> bool>,
    /// `bool decommit(ctx, addr, size, offset, length)` (NULL ⇒ no-op default).
    pub decommit:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize) -> bool>,
    /// `bool purge_lazy(ctx, addr, size, offset, length)` (NULL ⇒ no-op default).
    pub purge_lazy:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize) -> bool>,
    /// `bool purge_forced(ctx, addr, size, offset, length)` (NULL ⇒ no-op default).
    pub purge_forced:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize) -> bool>,
    /// `bool split(ctx, addr, size, size_a, size_b, committed)` (NULL ⇒ no-op).
    pub split:
        Option<unsafe extern "C" fn(*mut c_void, *mut c_void, usize, usize, usize, bool) -> bool>,
    /// `bool merge(ctx, addr_a, size_a, addr_b, size_b, committed)` (NULL ⇒ no-op).
    pub merge: Option<
        unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, usize, bool) -> bool,
    >,
}

/// Adapts a C [`topo_extent_hooks_t`] vtable + `ctx` to the Rust [`ExtentHooks`]
/// trait. Heap-owned by [`topo_arena_create_hooked`] with a `'static` borrow handed
/// to the process-global allocator (so it outlives it, exactly as the metadata/
/// pagemap do); tracked in `CHOOKS_REGISTRY` and **reclaimed on destroy** (freed on a
/// failed create), so a create/destroy loop stays bounded rather than leaking.
struct CHooks {
    vt: topo_extent_hooks_t,
    ctx: *mut c_void,
}

// SAFETY: the C caller is responsible (§23.3) for the hooks + `ctx` being safe to
// invoke from whichever threads use the allocator; we treat the opaque `ctx` as a
// shared token, never dereferencing it ourselves.
unsafe impl Send for CHooks {}
// SAFETY: see the `Send` impl above.
unsafe impl Sync for CHooks {}

impl CHooks {
    /// Map a C `bool` result (jemalloc convention: `true` ⇒ failure) to a `Result`.
    #[inline]
    fn ok(failed: bool) -> Result<(), BackendError> {
        if failed {
            Err(BackendError::Unsupported)
        } else {
            Ok(())
        }
    }
}

impl ExtentHooks for CHooks {
    fn alloc(
        &self,
        size: usize,
        align: usize,
        zero: &mut bool,
        commit: &mut bool,
    ) -> Result<Region, BackendError> {
        let f = self.vt.alloc.ok_or(BackendError::Unsupported)?;
        // SAFETY: the C caller's `alloc`; `size > 0` and `align` is a power of two
        // (the `HookProvider` checked), and `zero`/`commit` are valid out-pointers.
        let p = unsafe {
            f(
                self.ctx,
                size,
                align,
                zero as *mut bool,
                commit as *mut bool,
            )
        };
        if p.is_null() {
            Err(BackendError::OutOfMemory)
        } else {
            Ok(Region {
                base: p.cast::<u8>(),
                len: size,
            })
        }
    }

    fn dealloc(&self, region: Region, committed: bool) -> Result<(), BackendError> {
        let f = self.vt.dealloc.ok_or(BackendError::Unsupported)?;
        // SAFETY: the C caller's `dealloc`; `region` is a reservation it handed out.
        Self::ok(unsafe {
            f(
                self.ctx,
                region.base.cast::<c_void>(),
                region.len,
                committed,
            )
        })
    }

    fn commit(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        match self.vt.commit {
            // SAFETY: the C caller's `commit`; `[offset, offset+length)` is in-range
            // (the `HookProvider` checked).
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    region.base.cast::<c_void>(),
                    region.len,
                    offset,
                    length,
                )
            }),
            None => Ok(()),
        }
    }

    fn decommit(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        match self.vt.decommit {
            // SAFETY: as `commit`.
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    region.base.cast::<c_void>(),
                    region.len,
                    offset,
                    length,
                )
            }),
            None => Ok(()),
        }
    }

    fn purge_lazy(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        match self.vt.purge_lazy {
            // SAFETY: as `commit`.
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    region.base.cast::<c_void>(),
                    region.len,
                    offset,
                    length,
                )
            }),
            None => Ok(()),
        }
    }

    fn purge_forced(
        &self,
        region: Region,
        offset: usize,
        length: usize,
    ) -> Result<(), BackendError> {
        match self.vt.purge_forced {
            // SAFETY: as `commit`.
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    region.base.cast::<c_void>(),
                    region.len,
                    offset,
                    length,
                )
            }),
            None => Ok(()),
        }
    }

    fn split(
        &self,
        region: Region,
        size_a: usize,
        size_b: usize,
        committed: bool,
    ) -> Result<(), BackendError> {
        match self.vt.split {
            // SAFETY: the C caller's `split`; `region` is a live extent it tracks.
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    region.base.cast::<c_void>(),
                    region.len,
                    size_a,
                    size_b,
                    committed,
                )
            }),
            None => Ok(()),
        }
    }

    fn merge(&self, left: Region, right: Region, committed: bool) -> Result<(), BackendError> {
        match self.vt.merge {
            // SAFETY: the C caller's `merge`; `left`/`right` are adjacent live extents.
            Some(f) => Self::ok(unsafe {
                f(
                    self.ctx,
                    left.base.cast::<c_void>(),
                    left.len,
                    right.base.cast::<c_void>(),
                    right.len,
                    committed,
                )
            }),
            None => Ok(()),
        }
    }

    fn name(&self) -> &'static str {
        "c-hooks"
    }
}

/// Default span/large region sizes for a hooked arena when the caller passes `0`
/// (16 MiB span / 32 MiB large — the [`AllocatorConfig::small`] regions), with that
/// config's descriptor-pool capacities.
fn hooked_cfg(span_region_bytes: usize, large_region_bytes: usize) -> AllocatorConfig {
    let base = AllocatorConfig::small();
    AllocatorConfig {
        span_region_bytes: if span_region_bytes == 0 {
            base.span_region_bytes
        } else {
            span_region_bytes
        },
        large_region_bytes: if large_region_bytes == 0 {
            base.large_region_bytes
        } else {
            large_region_bytes
        },
        ..base
    }
}

/// `uint32_t topo_arena_create_hooked(const topo_extent_hooks_t* hooks, void* ctx,
/// size_t span_region_bytes, size_t large_region_bytes)` (§22.2/§22.4/§23.2, plan
/// 06 W10): create an explicit arena whose span/large allocations are served from
/// the custom backing `*hooks` (with opaque `ctx`). `span_region_bytes` /
/// `large_region_bytes` size the arena's backing (`0` ⇒ a sensible default). The
/// hooks' function pointers are **copied**, so the caller's `topo_extent_hooks_t`
/// need not persist (but `ctx` and any memory the hooks manage must).
///
/// Returns the new arena id (`>= 1`), routable through `TOPO_ARENA(id)`, or `0` on
/// failure (`errno = EINVAL` for a NULL/incomplete vtable; `ENOMEM` if the
/// hooked-backend registry is full or the hooks cannot reserve the backing).
///
/// # Safety
/// `hooks` is NULL or points to a valid `topo_extent_hooks_t`; its `alloc`/`dealloc`
/// (and any non-NULL op) uphold the §23.3 contracts, and `ctx` stays valid for the
/// arena's lifetime.
#[no_mangle]
pub unsafe extern "C" fn topo_arena_create_hooked(
    hooks: *const topo_extent_hooks_t,
    ctx: *mut c_void,
    span_region_bytes: usize,
    large_region_bytes: usize,
) -> u32 {
    if hooks.is_null() {
        set_errno(EINVAL);
        return 0;
    }
    // SAFETY: `hooks` is non-null and points to a valid vtable (caller contract).
    let vt = unsafe { *hooks };
    // `alloc` and `dealloc` are mandatory — a backing must be able to obtain and
    // return memory.
    if vt.alloc.is_none() || vt.dealloc.is_none() {
        set_errno(EINVAL);
        return 0;
    }
    let Some(a) = global() else {
        set_errno(EINVAL);
        return 0;
    };
    // The adapter must outlive the process-global (`'static`) allocator, so it is
    // heap-owned via a raw box. It is **not** leaked: it is tracked in
    // `CHOOKS_REGISTRY` and reclaimed on `topo_arena_destroy` (or freed right here
    // on a failed create), so a create/destroy loop never grows the heap.
    let raw: *mut CHooks = Box::into_raw(Box::new(CHooks { vt, ctx }));
    // SAFETY: `raw` is a fresh, exclusively-owned `CHooks`; the `'static` borrow is
    // upheld by keeping the box alive until destroy (the success path tracks it) or
    // freeing it on the failure path below — never by an actual leak. `chooks` is
    // not used after the call, so the failure-path free cannot alias it.
    let chooks: &'static CHooks = unsafe { &*raw };
    let cfg = hooked_cfg(span_region_bytes, large_region_bytes);
    match a.arena_create_hooked(&ArenaPolicy::explicit(), chooks, cfg) {
        Ok(id) => {
            CHOOKS_REGISTRY
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((id.0, raw as usize));
            id.0
        }
        Err(e) => {
            set_errno(arena_errno(e));
            // SAFETY: the create failed ⇒ the adapter was never registered with the
            // allocator ⇒ no live reference remains; reclaim the box.
            drop(unsafe { Box::from_raw(raw) });
            0
        }
    }
}

/// `size_t topo_max_hook_backends(void)`: the maximum number of arenas that can
/// carry their own extent hooks at once ([`MAX_HOOK_BACKENDS`]).
#[no_mangle]
pub extern "C" fn topo_max_hook_backends() -> usize {
    MAX_HOOK_BACKENDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c_api::topomalloc_free;
    use crate::extended::{topo_arena, topo_mallocx};
    use std::alloc::{alloc as host_alloc, dealloc as host_dealloc, Layout};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A minimal C-style backing: host allocations, with global op counters so the
    // test can confirm the C vtable was actually driven.
    static C_ALLOCS: AtomicUsize = AtomicUsize::new(0);
    static C_DEALLOCS: AtomicUsize = AtomicUsize::new(0);

    // We must remember each allocation's Layout to free it; stash (base,size,align)
    // in a small global table (the test makes few allocations).
    struct Rec {
        base: usize,
        size: usize,
        align: usize,
    }
    static REC: std::sync::Mutex<Vec<Rec>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn c_alloc(
        _ctx: *mut c_void,
        size: usize,
        align: usize,
        zero: *mut bool,
        commit: *mut bool,
    ) -> *mut c_void {
        C_ALLOCS.fetch_add(1, Ordering::Relaxed);
        let layout = match Layout::from_size_align(size, align) {
            Ok(l) => l,
            Err(_) => return core::ptr::null_mut(),
        };
        // SAFETY: nonzero size + valid alignment.
        let p = unsafe { host_alloc(layout) };
        if !p.is_null() {
            REC.lock().unwrap().push(Rec {
                base: p as usize,
                size,
                align,
            });
            // SAFETY: valid out-pointers from the HookProvider.
            unsafe {
                *zero = false;
                *commit = true;
            }
        }
        p.cast::<c_void>()
    }

    unsafe extern "C" fn c_dealloc(
        _ctx: *mut c_void,
        addr: *mut c_void,
        _size: usize,
        _committed: bool,
    ) -> bool {
        C_DEALLOCS.fetch_add(1, Ordering::Relaxed);
        let mut rec = REC.lock().unwrap();
        if let Some(i) = rec.iter().position(|r| r.base == addr as usize) {
            let r = rec.swap_remove(i);
            if let Ok(layout) = Layout::from_size_align(r.size, r.align) {
                // SAFETY: exactly the pointer/layout from `c_alloc`.
                unsafe { host_dealloc(addr.cast::<u8>(), layout) };
            }
        }
        false // success
    }

    fn c_vtable() -> topo_extent_hooks_t {
        topo_extent_hooks_t {
            alloc: Some(c_alloc),
            dealloc: Some(c_dealloc),
            commit: None,
            decommit: None,
            purge_lazy: None,
            purge_forced: None,
            split: None,
            merge: None,
        }
    }

    #[test]
    fn create_hooked_arena_through_the_c_abi() {
        let before = C_ALLOCS.load(Ordering::Relaxed);
        let vt = c_vtable();
        // SAFETY: `vt` is a valid vtable with the required alloc/dealloc.
        let id = unsafe { topo_arena_create_hooked(&vt, core::ptr::null_mut(), 4 << 20, 8 << 20) };
        assert!(id >= 1, "hooked arena created");
        // The C `alloc` hook reserved the arena's regions (span + large).
        assert!(C_ALLOCS.load(Ordering::Relaxed) >= before + 2);

        // Allocate from it through the flag-routed path; the object is from the
        // C backing.
        let p = topo_mallocx(128, topo_arena(id));
        assert!(!p.is_null());
        let base = p as usize;
        assert!(
            REC.lock()
                .unwrap()
                .iter()
                .any(|r| base >= r.base && base < r.base + r.size),
            "allocation served from the C hooks' region"
        );
        // SAFETY: `p` is a live 128-byte allocation owned by this test.
        unsafe { p.cast::<u8>().write_bytes(0xCD, 128) };
        // SAFETY: `p` was returned by the engine and is owned here.
        unsafe { topomalloc_free(p) };

        // Destroy returns the regions to the C `dealloc` hook.
        let deallocs_before = C_DEALLOCS.load(Ordering::Relaxed);
        // SAFETY: quiesced; no live pointers into the arena remain.
        assert_eq!(unsafe { crate::topo_arena_destroy(id) }, 0);
        assert!(C_DEALLOCS.load(Ordering::Relaxed) > deallocs_before);
    }

    #[test]
    fn null_or_incomplete_vtable_is_rejected() {
        // SAFETY: passing a null `hooks` is explicitly handled (rejected, EINVAL).
        let null_result =
            unsafe { topo_arena_create_hooked(core::ptr::null(), core::ptr::null_mut(), 0, 0) };
        assert_eq!(null_result, 0);
        // Missing `alloc` → EINVAL (a backing must obtain memory).
        let mut vt = c_vtable();
        vt.alloc = None;
        // SAFETY: `vt` is valid storage; the missing-alloc case is rejected.
        let no_alloc_result = unsafe { topo_arena_create_hooked(&vt, core::ptr::null_mut(), 0, 0) };
        assert_eq!(no_alloc_result, 0);
    }

    #[test]
    fn max_hook_backends_is_exposed() {
        assert_eq!(topo_max_hook_backends(), MAX_HOOK_BACKENDS);
    }

    /// Whether arena `id`'s `CHooks` adapter is currently tracked (keyed by the
    /// unique arena id, so concurrency-robust without exact-length assertions).
    fn chooks_tracked(id: u32) -> bool {
        CHOOKS_REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|&(a, _)| a == id)
    }

    #[test]
    fn destroy_reclaims_the_hook_adapter_no_leak() {
        // A successful create tracks its adapter; destroy reclaims it (so a
        // create/destroy loop does not grow the heap — the P2 review fix).
        let vt = c_vtable();
        // SAFETY: a valid vtable with the required alloc/dealloc.
        let id = unsafe { topo_arena_create_hooked(&vt, core::ptr::null_mut(), 2 << 20, 4 << 20) };
        assert!(id >= 1);
        assert!(
            chooks_tracked(id),
            "a live hooked arena's adapter is tracked"
        );
        // SAFETY: quiesced; no live pointers into the arena.
        assert_eq!(unsafe { crate::topo_arena_destroy(id) }, 0);
        assert!(
            !chooks_tracked(id),
            "destroy reclaims the adapter (no unbounded leak)"
        );
    }

    /// A backing whose `alloc` always fails, so the create fails after the adapter
    /// box exists — exercising the failure-path reclaim.
    unsafe extern "C" fn c_alloc_fail(
        _ctx: *mut c_void,
        _size: usize,
        _align: usize,
        _zero: *mut bool,
        _commit: *mut bool,
    ) -> *mut c_void {
        core::ptr::null_mut()
    }

    #[test]
    fn failed_hooked_create_reclaims_its_adapter() {
        // A create that fails *after* building the adapter (the hook cannot reserve)
        // must free the adapter box on the failure path, not leak it (the P2 review
        // fix: failed/retry creates do not grow the heap). A failed create returns
        // id 0 and never tracks an entry (the registry is keyed by the granted id,
        // which only exists on success), so retrying never accumulates registry
        // entries; the box itself is freed on the spot by the `Box::from_raw(raw)` on
        // `topo_arena_create_hooked`'s error path — exactly one free per failed create,
        // structurally (Rust ownership), since that is the only path on failure.
        let mut vt = c_vtable();
        vt.alloc = Some(c_alloc_fail);
        for _ in 0..4 {
            // SAFETY: a valid vtable; each create is expected to fail cleanly.
            let id = unsafe { topo_arena_create_hooked(&vt, core::ptr::null_mut(), 0, 0) };
            assert_eq!(
                id, 0,
                "create fails (no leak/track) when the hook cannot allocate"
            );
        }
    }

    /// A backing whose `dealloc` always reports failure (jemalloc convention:
    /// `true`), modeling a custom backing that refuses to take its region back.
    unsafe extern "C" fn c_dealloc_fail(
        _ctx: *mut c_void,
        _addr: *mut c_void,
        _size: usize,
        _committed: bool,
    ) -> bool {
        true // failure
    }

    #[test]
    fn destroy_surfaces_a_backing_release_failure() {
        // W10 strict teardown (PR #14, thread #3) at the C ABI: when the custom
        // backing's `dealloc` hook fails to take the arena's region back on destroy,
        // `topo_arena_destroy` must NOT report success — it returns -1 and the arena
        // is §36.13-quarantined (never cleanly destroyed). The adapter is retained
        // (reclaim is gated on a clean `Ok` destroy — a conservative, safe rule that
        // never frees a possibly-borrowed adapter): a bounded terminal-failure
        // retention, never a use-after-free.
        let mut vt = c_vtable();
        vt.dealloc = Some(c_dealloc_fail);
        // SAFETY: a valid vtable with the required alloc/dealloc.
        let id = unsafe { topo_arena_create_hooked(&vt, core::ptr::null_mut(), 2 << 20, 4 << 20) };
        assert!(id >= 1);
        assert!(chooks_tracked(id));
        // A live object so the drain does real work before the (failing) region return.
        let p = topo_mallocx(128, topo_arena(id));
        assert!(!p.is_null());
        // SAFETY: `p` is a live allocation owned here.
        unsafe { topomalloc_free(p) };
        // Destroy: the C `dealloc` refuses both region returns ⇒ teardown fails ⇒
        // quarantine ⇒ -1, not a clean success.
        // SAFETY: quiesced; no live pointers into the arena remain.
        let rc = unsafe { crate::topo_arena_destroy(id) };
        assert_eq!(
            rc, -1,
            "a backing refusing its region return fails the destroy"
        );
        assert!(
            chooks_tracked(id),
            "the adapter is retained on a quarantined destroy (reclaim is Ok-only)"
        );
    }
}

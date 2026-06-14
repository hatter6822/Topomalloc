// SPDX-License-Identifier: MIT
//! The C arena API (§22.4/§36.14, plan 06 W9): create, delegate, reset, and
//! destroy explicit arenas, plus the rights vocabulary. Allocation *from* an
//! arena is the existing `topo_mallocx(size, TOPO_ARENA(id))` path (the id this
//! API returns); these entry points manage the arena lifecycle and authority.
//!
//! This is the POSIX surface of the capability-explicit ABI §36.14 sketches with
//! `topo_sele4n_*` names: an arena is created with attenuable [rights](CapRights),
//! a `quota`, and (on seLe4n) a label; delegation attenuates a parent's
//! authority. On POSIX the values are ambient (single authority, the `PUBLIC`
//! label), but the *shape* is identical so the seLe4n runtime drops in (D2).
//!
//! **Status convention.** Lifecycle calls return `0` on success and `-1` on
//! failure with `errno` set (`EINVAL` for a bad/illegal request); `create`/
//! `delegate` return the new arena id (`>= 1`) or `0` on failure (`0` is the
//! default arena, which creation never yields, so it is an unambiguous error
//! sentinel).

use core::ffi::c_void;
use core::ptr;

use topo_core::{
    ArenaConfig, ArenaError, ArenaId, ArenaPolicy, CapRights, DecayConfig, Delegation, MIN_ALIGN,
    QUOTA_UNLIMITED,
};

use crate::errno_shim::{alloc_protocol, set_errno, EACCES, EBUSY, EINVAL, ENOMEM};
use crate::extended::decode_flags;
use crate::global;
use crate::policy::{zero_size_policy, ZeroSizePolicy};

/// Map an [`ArenaError`] onto the POSIX `errno` taxonomy — the projection of the
/// §36.14 `TOPO_ERR_*` classes a C caller can read after a failed arena call:
/// authority denials are `EACCES`, a draining/inactive arena is `EBUSY`, a quota
/// overrun is `ENOMEM`, and every malformed/illegal request is `EINVAL`.
pub(crate) fn arena_errno(e: ArenaError) -> i32 {
    match e {
        ArenaError::AuthorityDenied => EACCES,
        ArenaError::NotActive => EBUSY,
        ArenaError::QuotaExceeded | ArenaError::Exhausted => ENOMEM,
        ArenaError::NotFound
        | ArenaError::Attenuation
        | ArenaError::InvalidPolicy
        | ArenaError::IllegalTransition
        | ArenaError::IsDefault => EINVAL,
    }
}

/// Pack a generation-checked arena **handle** (§36.13/§36.14): `(generation <<
/// 32) | id`. `0` is never a valid handle.
fn make_handle(a: &crate::AnyAllocator, id: ArenaId) -> u64 {
    a.arena_handle(id).unwrap_or(0)
}

/// Authority to allocate from the arena (§36.4). Mirrors [`CapRights::ALLOC`].
pub const TOPO_RIGHT_ALLOC: u64 = 1;
/// Authority to free the arena's allocations. Mirrors [`CapRights::FREE`].
pub const TOPO_RIGHT_FREE: u64 = 2;
/// Authority to observe the arena's statistics. Mirrors [`CapRights::STATS`].
pub const TOPO_RIGHT_STATS: u64 = 4;
/// Authority to reset/destroy the arena. Mirrors [`CapRights::DESTROY`].
pub const TOPO_RIGHT_DESTROY: u64 = 8;
/// Every right — the ambient default authority.
pub const TOPO_RIGHTS_ALL: u64 = 0xF;

/// A quota of `0` requests an unlimited quota (the ambient default).
const QUOTA_FROM_C_UNLIMITED: usize = 0;

/// Decode a public `topo_rights_t` word into [`CapRights`], rejecting unknown
/// bits (a forged rights word can never widen authority, §36.4).
fn decode_rights(rights: u64) -> Option<CapRights> {
    if rights & !TOPO_RIGHTS_ALL != 0 {
        return None;
    }
    CapRights::from_bits((rights & TOPO_RIGHTS_ALL) as u8)
}

/// Translate a C quota argument (`0` ⇒ unlimited) into a quota ceiling.
fn quota_from_c(quota_bytes: usize) -> u64 {
    if quota_bytes == QUOTA_FROM_C_UNLIMITED {
        QUOTA_UNLIMITED
    } else {
        quota_bytes as u64
    }
}

/// `uint32_t topo_arena_create_ex(size_t quota_bytes, uint64_t rights)`
/// (§22.4, W9-3): create an explicit arena conferring `rights` with a quota of
/// `quota_bytes` (`0` ⇒ unlimited). Returns the new arena id (`>= 1`), routable
/// through `TOPO_ARENA(id)`, or `0` on failure (`errno = EINVAL` for a bad rights
/// word; the table being full also yields `0`).
#[no_mangle]
pub extern "C" fn topo_arena_create_ex(quota_bytes: usize, rights: u64) -> u32 {
    let Some(rights) = decode_rights(rights) else {
        set_errno(EINVAL);
        return 0;
    };
    let Some(a) = global() else {
        set_errno(EINVAL);
        return 0;
    };
    let policy = ArenaPolicy::explicit()
        .with_rights(rights)
        .with_quota(quota_from_c(quota_bytes));
    match a.arena_create(&policy) {
        Ok(id) => id.0,
        Err(e) => {
            set_errno(arena_errno(e));
            0
        }
    }
}

/// `uint32_t topo_arena_create(void)`: create an explicit arena with full
/// (ambient) authority and an unlimited quota — the common case. Returns the
/// new arena id or `0` on failure.
#[no_mangle]
pub extern "C" fn topo_arena_create() -> u32 {
    topo_arena_create_ex(QUOTA_FROM_C_UNLIMITED, TOPO_RIGHTS_ALL)
}

/// `uint32_t topo_arena_delegate(uint32_t parent, size_t quota_bytes,
/// uint64_t rights)` (§36.4/§36.14, W9-5): delegate an attenuated child arena.
/// The delegation MUST narrow the parent's authority — `rights` must be a subset
/// of the parent's, `quota_bytes` must not exceed the parent's remaining quota,
/// and the label is preserved (§36.16). Returns the child id or `0` on failure
/// (`errno = EINVAL` on any attenuation violation).
#[no_mangle]
pub extern "C" fn topo_arena_delegate(parent: u32, quota_bytes: usize, rights: u64) -> u32 {
    let Some(rights) = decode_rights(rights) else {
        set_errno(EINVAL);
        return 0;
    };
    let Some(a) = global() else {
        set_errno(EINVAL);
        return 0;
    };
    let Some(pstats) = a.arena_stats(ArenaId(parent)) else {
        set_errno(EINVAL);
        return 0;
    };
    let del = Delegation::inheriting(&pstats, quota_from_c(quota_bytes), "").with_rights(rights);
    match a.arena_delegate(ArenaId(parent), &del) {
        Ok(id) => id.0,
        Err(e) => {
            set_errno(arena_errno(e));
            0
        }
    }
}

/// `int topo_arena_reset(uint32_t id)` (§22.5, W9-4): discard every allocation in
/// arena `id`, return its backing, bump its reset generation, and leave it
/// `Active`. Returns `0` on success, `-1` with `errno = EINVAL` on failure (e.g.
/// the default arena, or an illegal lifecycle state).
///
/// # Safety
///
/// The caller MUST ensure the arena is quiesced — no thread is allocating from
/// or freeing into it — and accepts that **every outstanding pointer into the
/// arena becomes invalid** (§22.5). Using such a pointer afterward is undefined
/// behavior, exactly as using a pointer after `free`.
#[no_mangle]
pub unsafe extern "C" fn topo_arena_reset(id: u32) -> i32 {
    let Some(a) = global() else {
        set_errno(EINVAL);
        return -1;
    };
    // SAFETY: the caller upholds this function's quiescence contract.
    match unsafe { a.arena_reset(ArenaId(id)) } {
        Ok(_) => 0,
        Err(e) => {
            set_errno(arena_errno(e));
            -1
        }
    }
}

/// `int topo_arena_destroy(uint32_t id)` (§22.6/§36.13, W9-4d/W9-6): reset arena
/// `id`, then remove it and retire its id behind a generation bump. Returns `0`
/// on success, `-1` with `errno = EINVAL` on failure.
///
/// # Safety
///
/// As [`topo_arena_reset`]: the arena must be quiesced and the caller accepts
/// that its outstanding pointers become invalid.
#[no_mangle]
pub unsafe extern "C" fn topo_arena_destroy(id: u32) -> i32 {
    let Some(a) = global() else {
        set_errno(EINVAL);
        return -1;
    };
    // SAFETY: the caller upholds this function's quiescence contract.
    let r = unsafe { a.arena_destroy(ArenaId(id)) };
    // Reclaim the C hook adapter exactly when the allocator no longer references it
    // (W10). A clean destroy *and* a teardown-failure quarantine both drop the
    // arena's backend — and with it the `HookProvider<&CHooks>` borrow — leaving no
    // hooked backing for `id`, so freeing the adapter is sound and bounds the
    // per-create heap. A drain-failure quarantine KEEPS the backend registered (its
    // `HookProvider` still borrows the adapter), so the adapter is retained — freeing
    // it would be a use-after-free. A no-op for a non-hooked arena or a destroy that
    // was rejected outright (the arena, and any adapter, survive).
    if !a.arena_has_hook_backend(ArenaId(id)) {
        crate::hooks_api::reclaim_chooks(id);
    }
    match r {
        Ok(_) => 0,
        Err(e) => {
            set_errno(arena_errno(e));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Generation-checked handles (§36.13/§36.14): the capability-routing surface.
// ---------------------------------------------------------------------------

/// `topo_arena_handle_t topo_arena_handle(uint32_t id)` (§36.13/§36.14): mint a
/// generation-checked **handle** for arena `id`'s current incarnation. The
/// handle packs `(incarnation generation << 32) | id`; unlike a raw id it
/// detects a destroyed-then-recreated arena as stale. Returns `0` (never a valid
/// handle) for an unregistered id.
#[no_mangle]
pub extern "C" fn topo_arena_handle(id: u32) -> u64 {
    match global() {
        Some(a) => make_handle(a, ArenaId(id)),
        None => 0,
    }
}

/// `uint32_t topo_arena_id(topo_arena_handle_t handle)`: the arena id a handle
/// names (its low 32 bits), for use with `TOPO_ARENA(id)`. Does not validate the
/// handle's generation — use [`topo_mallocx_arena`] for generation-checked
/// routing.
#[no_mangle]
pub extern "C" fn topo_arena_id(handle: u64) -> u32 {
    (handle & 0xffff_ffff) as u32
}

/// `void *topo_mallocx_arena(topo_arena_handle_t handle, size_t size,
/// topo_flags_t flags)` (§36.14): allocate from the arena a **handle** names,
/// with generation checking. A stale handle (its arena was destroyed, possibly
/// recreated at the same id) is `NULL` + `EINVAL` — the §36.13 guarantee a raw
/// `TOPO_ARENA(id)` flag cannot give. The flag word's own arena field is ignored
/// (the handle wins); its alignment/zero/etc. hints apply. Allocation failure is
/// mapped through the arena taxonomy: `EACCES` (no alloc right), `EBUSY`
/// (draining), `ENOMEM` (quota/OOM).
#[no_mangle]
pub extern "C" fn topo_mallocx_arena(handle: u64, size: usize, flags: u64) -> *mut c_void {
    let Some((align, f)) = decode_flags(flags) else {
        set_errno(EINVAL);
        return ptr::null_mut();
    };
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    // Resolve the handle (stale ⇒ EINVAL, §36.13).
    let Some(arena) = a.arena_resolve_handle(handle) else {
        set_errno(EINVAL);
        return ptr::null_mut();
    };
    // Honor the configurable zero-size policy uniformly with `topo_mallocx`
    // (§9.6): under `zero_null`, a zero-size request is NULL with errno
    // untouched.
    if size == 0 && zero_size_policy() == ZeroSizePolicy::Null {
        return ptr::null_mut();
    }
    // Pre-check the gate so a failure carries the precise taxonomy errno, not a
    // bare ENOMEM (§36.14 distinguishes authority/quota/draining).
    if let Some(s) = a.arena_stats(arena) {
        if !s.state.is_active() {
            set_errno(EBUSY);
            return ptr::null_mut();
        }
        if !s.rights.contains(topo_core::CapRights::ALLOC) {
            set_errno(EACCES);
            return ptr::null_mut();
        }
    }
    alloc_protocol(|| a.allocate_in(arena, size, align.max(MIN_ALIGN), f)).cast::<c_void>()
}

/// `int topo_arena_configure(uint32_t id, uint64_t dirty_decay_ms,
/// uint64_t muzzy_decay_ms)` (§22.4 *configure*, F-005): update arena `id`'s
/// decay timing (the headline per-arena tunable, §22.1). The authority and quota
/// are immutable here (a configure cannot widen authority). Returns `0`, or `-1`
/// with a mapped `errno` on failure. The fuller policy surface (NUMA, hugepage,
/// cache budget) is the Rust [`ArenaConfig`] API and the plan-07 control plane.
#[no_mangle]
pub extern "C" fn topo_arena_configure(id: u32, dirty_decay_ms: u64, muzzy_decay_ms: u64) -> i32 {
    let Some(a) = global() else {
        set_errno(EINVAL);
        return -1;
    };
    let Some(s) = a.arena_stats(ArenaId(id)) else {
        set_errno(EINVAL);
        return -1;
    };
    let cfg = ArenaConfig::from_stats(&s).with_decay(DecayConfig {
        dirty_decay_ms,
        muzzy_decay_ms,
    });
    match a.arena_configure(ArenaId(id), &cfg) {
        Ok(()) => 0,
        Err(e) => {
            set_errno(arena_errno(e));
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c_api::topomalloc_free;
    use crate::extended::{topo_arena, topo_mallocx, topo_nallocx};

    #[test]
    fn create_allocate_and_destroy_an_arena_end_to_end() {
        // Create an explicit arena, allocate from it through the flag-routed
        // path, then destroy it — the §37.4 "explicit arenas usable by
        // applications" demo, from C.
        let id = topo_arena_create();
        assert!(id >= 1, "create yields a non-default id");
        let p = topo_mallocx(128, topo_arena(id));
        assert!(!p.is_null(), "allocation from the explicit arena succeeds");
        // SAFETY: `p` is a live 128-byte allocation owned by this test.
        unsafe { p.cast::<u8>().write_bytes(0xAB, 128) };
        // SAFETY: `p` was returned by the engine and is owned here.
        unsafe { topomalloc_free(p) };
        // SAFETY: the test owns the arena and holds no live pointers into it.
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0, "destroy succeeds");
        // The destroyed id is no longer routable.
        assert!(topo_mallocx(64, topo_arena(id)).is_null());
    }

    #[test]
    fn quota_capped_arena_refuses_oversized_demand() {
        let id = topo_arena_create_ex(256, TOPO_RIGHTS_ALL);
        assert!(id >= 1);
        let p = topo_mallocx(200, topo_arena(id));
        assert!(!p.is_null());
        // The quota (256) admits one 200-byte (224-class) object, not two.
        assert!(topo_mallocx(200, topo_arena(id)).is_null());
        // SAFETY: `p` is live and test-owned.
        unsafe { topomalloc_free(p) };
        // SAFETY: quiesced; no live pointers into the arena remain.
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
    }

    #[test]
    fn delegation_is_attenuation_only_through_the_c_abi() {
        let parent = topo_arena_create_ex(4096, TOPO_RIGHTS_ALL);
        assert!(parent >= 1);
        // A faithful attenuation (alloc+free, smaller quota) succeeds.
        let child = topo_arena_delegate(parent, 512, TOPO_RIGHT_ALLOC | TOPO_RIGHT_FREE);
        assert!(child >= 1);
        // Widening the quota beyond the parent's remaining budget is rejected.
        assert_eq!(topo_arena_delegate(parent, 1 << 40, TOPO_RIGHTS_ALL), 0);
        // An unknown rights bit is rejected.
        assert_eq!(topo_arena_create_ex(0, 1 << 40), 0);
        // SAFETY: quiesced; both arenas are unused.
        unsafe {
            assert_eq!(topo_arena_destroy(child), 0);
            assert_eq!(topo_arena_destroy(parent), 0);
        }
    }

    #[test]
    fn reset_and_destroy_reject_the_default_arena() {
        // SAFETY: the default arena is never quiesced, but reset/destroy reject
        // it before touching anything (§22.5), so the call is a safe no-op.
        unsafe {
            assert_eq!(topo_arena_reset(0), -1);
            assert_eq!(topo_arena_destroy(0), -1);
        }
    }

    #[test]
    fn handle_routes_and_detects_staleness() {
        use crate::c_api::topomalloc_malloc_usable_size;
        use crate::errno_shim::{get_errno, EINVAL};

        let id = topo_arena_create();
        assert!(id >= 1);
        let h = topo_arena_handle(id);
        assert_ne!(h, 0, "a live arena has a nonzero handle");
        assert_eq!(topo_arena_id(h), id, "the handle carries its id");

        // Handle-routed allocation works and survives a reset of the arena.
        let p = topo_mallocx_arena(h, 100, 0);
        assert!(!p.is_null());
        assert!(topomalloc_malloc_usable_size(p) >= 100);
        // SAFETY: `p` is live and test-owned.
        unsafe { topomalloc_free(p) };
        // SAFETY: quiesced (p freed); reset keeps the same incarnation.
        assert_eq!(unsafe { topo_arena_reset(id) }, 0);
        let q = topo_mallocx_arena(h, 64, 0);
        assert!(!q.is_null(), "a handle survives a reset of its own arena");
        // SAFETY: `q` is live and test-owned.
        unsafe { topomalloc_free(q) };

        // Destroy invalidates the handle: a stale handle is EINVAL, not a wrong
        // allocation (§36.13).
        // SAFETY: quiesced.
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
        set_errno(0);
        assert!(topo_mallocx_arena(h, 64, 0).is_null());
        assert_eq!(get_errno(), EINVAL, "a stale handle is rejected");
    }

    #[test]
    fn configure_updates_decay_through_the_c_abi() {
        let id = topo_arena_create();
        assert!(id >= 1);
        // Reconfigure the decay timing; it must take effect and not disturb the
        // arena's usability.
        assert_eq!(topo_arena_configure(id, 1234, 5678), 0);
        let p = topo_mallocx(64, topo_arena(id));
        assert!(!p.is_null());
        // SAFETY: live, test-owned.
        unsafe { topomalloc_free(p) };
        // Configuring the default arena's decay is allowed (it is not a lifecycle
        // op); configuring a nonexistent arena fails.
        assert_eq!(topo_arena_configure(0, 1, 1), 0);
        assert_eq!(topo_arena_configure(200, 1, 1), -1);
        // SAFETY: quiesced.
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
    }

    #[test]
    fn mallocx_arena_honors_the_zero_size_policy() {
        // The handle path must apply the §9.6 zero-size policy uniformly with
        // `topo_mallocx`. NOTE: process-global policy — this test toggles it and
        // restores `Unique`; no other test in this binary does a size-0 malloc.
        use crate::policy::{set_zero_size_policy, ZeroSizePolicy};
        let id = topo_arena_create();
        let h = topo_arena_handle(id);
        // Default (zero_unique): size 0 → a unique, freeable pointer.
        let p = topo_mallocx_arena(h, 0, 0);
        assert!(!p.is_null());
        // SAFETY: `p` is a live zero-size allocation owned by this test.
        unsafe { topomalloc_free(p) };
        // zero_null: size 0 → NULL (matching topo_mallocx).
        set_zero_size_policy(ZeroSizePolicy::Null);
        assert!(topo_mallocx_arena(h, 0, 0).is_null());
        set_zero_size_policy(ZeroSizePolicy::Unique); // restore the default
                                                      // SAFETY: quiesced.
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
    }

    #[test]
    fn no_alloc_right_arena_is_eacces_through_the_handle() {
        use crate::errno_shim::{get_errno, EACCES};
        // An arena with STATS but not ALLOC refuses allocation with EACCES — the
        // §36.4 authority-denied taxonomy, surfaced through the handle path.
        let id = topo_arena_create_ex(0, TOPO_RIGHT_STATS);
        assert!(id >= 1);
        let h = topo_arena_handle(id);
        set_errno(0);
        assert!(topo_mallocx_arena(h, 64, 0).is_null());
        assert_eq!(get_errno(), EACCES, "no ALLOC right ⇒ EACCES (§36.4)");
        // SAFETY: quiesced (nothing was allocated).
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
    }

    #[test]
    fn no_alloc_right_arena_is_eacces_through_the_flag_path() {
        use crate::errno_shim::{get_errno, EACCES};
        // The *flag-routed* front door must report the same §36.4 taxonomy as the
        // handle path (PR #13 review): an arena that exists and is Active but
        // whose cap lacks ALLOC is an authority denial — EACCES, not the
        // misleading ENOMEM of the bare engine gate. `topo_nallocx` likewise must
        // predict 0 (it could never allocate there), staying pure.
        let id = topo_arena_create_ex(0, TOPO_RIGHT_STATS);
        assert!(id >= 1);
        set_errno(0);
        assert!(topo_mallocx(64, topo_arena(id)).is_null());
        assert_eq!(
            get_errno(),
            EACCES,
            "no ALLOC right ⇒ EACCES via the flag path"
        );
        set_errno(0);
        assert_eq!(
            topo_nallocx(64, topo_arena(id)),
            0,
            "nallocx predicts failure"
        );
        assert_eq!(get_errno(), 0, "nallocx stays pure (errno untouched)");
        // SAFETY: quiesced (nothing was allocated).
        assert_eq!(unsafe { topo_arena_destroy(id) }, 0);
    }
}

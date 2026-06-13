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

use topo_core::{ArenaId, ArenaPolicy, CapRights, Delegation, QUOTA_UNLIMITED};

use crate::errno_shim::{set_errno, EINVAL};
use crate::global;

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
        Err(_) => {
            set_errno(EINVAL);
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
        Err(_) => {
            set_errno(EINVAL);
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
        Err(_) => {
            set_errno(EINVAL);
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
    match unsafe { a.arena_destroy(ArenaId(id)) } {
        Ok(_) => 0,
        Err(_) => {
            set_errno(EINVAL);
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c_api::topomalloc_free;
    use crate::extended::{topo_arena, topo_mallocx};

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
}

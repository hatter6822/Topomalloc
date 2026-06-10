// SPDX-License-Identifier: MIT
//! The C arena API (plan 06 W9; §22, §36.4, §36.14): explicit arena creation,
//! configuration, reset, destruction, and attenuation-only delegation, plus
//! the explicit-arena allocation entry point (`topo_mallocx_arena`).
//!
//! **Conventions.** Arena ids are `uint32_t` (`topo_arena_t`); they are
//! allocated monotonically and **never reused** after destroy (B.5), so a
//! stale id can never alias a new arena — every operation on it is one
//! deterministic `EINVAL`. The lifecycle functions return `0` on success or a
//! positive errno value (the `posix_memalign` convention), and also set
//! `errno` to the same value on failure for uniformity with the rest of the
//! `topo_*` surface; `topo_mallocx_arena` returns null + `errno` like every
//! other allocation entry point.
//!
//! **Error mapping** (the §36.14 classes onto POSIX errno):
//!
//! | [`ArenaError`] | errno | §36.14 class |
//! |---|---|---|
//! | `NoSuchArena`, `ConfigInvalid` | `EINVAL` | `TOPO_ERR_INVALID_CAP` |
//! | `AuthorityDenied`, `LabelViolation`, `DefaultArenaProtected` | `EPERM` | `TOPO_ERR_AUTHORITY_DENIED` / `TOPO_ERR_LABEL_VIOLATION` |
//! | `QuotaExceeded` | `EDQUOT` | `TOPO_ERR_QUOTA_EXCEEDED` |
//! | `InvalidState` | `EBUSY` | `TOPO_ERR_ARENA_DRAINING` |
//! | `WouldBlock` | `EAGAIN` | `TOPO_ERR_WOULD_BLOCK` |
//! | `CapacityExhausted`, `Exhausted`, `Backend(_)` | `ENOMEM` | `TOPO_ERR_CSPACE/VSPACE_EXHAUSTED` |
//!
//! The capability semantics collapse on POSIX exactly as D2 prescribes: the
//! arena's own rights word is the capability this layer enforces (per-caller
//! capability handles arrive with the seLe4n IPC surface, plan 09), the label
//! is `PUBLIC`, and a zero `rights` field in [`TopoArenaConfig`] means "all
//! rights" for creation (ambient) and "the parent's rights" for delegation
//! (maximal legal attenuation).

use core::ffi::{c_char, c_void};
use core::ptr;

use topo_core::{
    known_numa_nodes, ArenaConfig, ArenaError, ArenaId, ArenaName, CapRights, DelegationSpec,
    Label, NodeId, NumaPolicy,
};

use crate::errno_shim::{alloc_protocol, set_errno, EAGAIN, EBUSY, EDQUOT, EINVAL, ENOMEM, EPERM};
use crate::global;

// ---------------------------------------------------------------------------
// Public constants (mirrored in include/topomalloc.h)
// ---------------------------------------------------------------------------

/// `TOPO_ARENA_RIGHT_ALLOC`: the arena capability admits allocation (§36.4).
pub const TOPO_ARENA_RIGHT_ALLOC: u32 = 1 << 0;
/// `TOPO_ARENA_RIGHT_FREE`: the capability admits frees.
pub const TOPO_ARENA_RIGHT_FREE: u32 = 1 << 1;
/// `TOPO_ARENA_RIGHT_STATS`: the capability admits statistics observation.
pub const TOPO_ARENA_RIGHT_STATS: u32 = 1 << 2;
/// `TOPO_ARENA_RIGHT_PURGE`: the capability admits purge/policy tuning.
pub const TOPO_ARENA_RIGHT_PURGE: u32 = 1 << 3;
/// `TOPO_ARENA_RIGHT_DESTROY`: the capability admits reset/destroy.
pub const TOPO_ARENA_RIGHT_DESTROY: u32 = 1 << 4;
/// `TOPO_ARENA_RIGHT_DELEGATE`: the capability admits further delegation.
pub const TOPO_ARENA_RIGHT_DELEGATE: u32 = 1 << 5;
/// All six rights — the ambient authority of a root arena on POSIX.
pub const TOPO_ARENA_RIGHTS_ALL: u32 = 0b11_1111;

/// `TOPO_NUMA_OS_DEFAULT`: do not override OS placement (§15.5).
pub const TOPO_NUMA_OS_DEFAULT: u32 = 0;
/// `TOPO_NUMA_LOCAL`: prefer the current NUMA node.
pub const TOPO_NUMA_LOCAL: u32 = 1;
/// `TOPO_NUMA_INTERLEAVE`: distribute across nodes.
pub const TOPO_NUMA_INTERLEAVE: u32 = 2;
/// `TOPO_NUMA_BIND`: prefer/require the node in `numa_node`.
pub const TOPO_NUMA_BIND: u32 = 3;
/// `TOPO_NUMA_ARENA_POLICY`: use arena-specific placement.
pub const TOPO_NUMA_ARENA_POLICY: u32 = 4;

/// `topo_arena_config_t` (§36.14-style configuration for create/delegate).
/// Layout pinned by the C harness's `_Static_assert`s; zero-initialization
/// (`{0}`) is the ambient default: unnamed, unlimited quota, all rights (or
/// the parent's, for delegation), `PUBLIC` label, OS-default NUMA placement.
#[repr(C)]
pub struct TopoArenaConfig {
    /// Optional NUL-terminated diagnostic name (≤ 32 bytes used); may be null.
    pub name: *const c_char,
    /// Quota limit in bytes; `0` = unlimited.
    pub quota_bytes: u64,
    /// `TOPO_ARENA_RIGHT_*` bits; `0` = all rights (create) / the parent's
    /// rights (delegate).
    pub rights: u32,
    /// `TOPO_NUMA_*` placement mode.
    pub numa_mode: u32,
    /// The target node for `TOPO_NUMA_BIND` (ignored otherwise).
    pub numa_node: u32,
    /// Information-flow label (§36.12); `0` = `PUBLIC`. Delegation requires
    /// the child label to equal the parent's (§36.4 — no silent downgrade).
    pub label: u32,
}

// ---------------------------------------------------------------------------
// Internal mapping helpers
// ---------------------------------------------------------------------------

/// Map an [`ArenaError`] to its errno (the table in the module docs).
fn errno_of(e: ArenaError) -> i32 {
    match e {
        ArenaError::NoSuchArena | ArenaError::ConfigInvalid => EINVAL,
        ArenaError::AuthorityDenied
        | ArenaError::LabelViolation
        | ArenaError::DefaultArenaProtected => EPERM,
        ArenaError::QuotaExceeded => EDQUOT,
        ArenaError::InvalidState => EBUSY,
        ArenaError::WouldBlock => EAGAIN,
        ArenaError::CapacityExhausted | ArenaError::Exhausted | ArenaError::Backend(_) => ENOMEM,
    }
}

/// Finish a lifecycle entry point: set errno and return the code.
fn finish(r: Result<(), ArenaError>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            let code = errno_of(e);
            set_errno(code);
            code
        }
    }
}

/// Read the optional C name (≤ 32 bytes up to the NUL). `Err(())` if longer.
///
/// # Safety
///
/// `name` is null or a valid NUL-terminated C string (the caller's C
/// contract).
unsafe fn read_name(name: *const c_char) -> Result<ArenaName, ()> {
    if name.is_null() {
        return Ok(ArenaName::default());
    }
    let mut buf = [0u8; 32];
    for (i, slot) in buf.iter_mut().enumerate() {
        // SAFETY: within the caller-guaranteed NUL-terminated string — every
        // byte up to and including the NUL is readable, and we stop at it.
        let b = unsafe { *name.add(i) } as u8;
        if b == 0 {
            // SAFETY: `buf[..i]` holds bytes we just wrote.
            return ArenaName::new(core::str::from_utf8(&buf[..i]).map_err(|_| ())?).ok_or(());
        }
        *slot = b;
    }
    // 32 bytes without a NUL: exactly 32 is representable iff byte 33 is NUL.
    // SAFETY: as above; reading the terminator position only.
    if unsafe { *name.add(32) } as u8 == 0 {
        return ArenaName::new(core::str::from_utf8(&buf).map_err(|_| ())?).ok_or(());
    }
    Err(()) // longer than the §22.2 32-byte field
}

/// Decode the C config into core types. `rights == 0` maps to `default_rights`
/// (ambient for create, the parent's for delegate).
///
/// # Safety
///
/// As [`read_name`]: `cfg.name` is null or a valid NUL-terminated C string.
unsafe fn decode_config(
    cfg: &TopoArenaConfig,
    default_rights: CapRights,
) -> Result<(ArenaName, Label, u64, CapRights, NumaPolicy), ()> {
    // SAFETY: forwarded caller contract.
    let name = unsafe { read_name(cfg.name) }?;
    let quota = if cfg.quota_bytes == 0 {
        u64::MAX
    } else {
        cfg.quota_bytes
    };
    if cfg.rights & !TOPO_ARENA_RIGHTS_ALL != 0 {
        return Err(()); // reserved rights bits MUST be zero
    }
    let rights = if cfg.rights == 0 {
        default_rights
    } else {
        let mut r = CapRights::NONE;
        for (bit, right) in [
            (TOPO_ARENA_RIGHT_ALLOC, CapRights::ALLOC),
            (TOPO_ARENA_RIGHT_FREE, CapRights::FREE),
            (TOPO_ARENA_RIGHT_STATS, CapRights::STATS),
            (TOPO_ARENA_RIGHT_PURGE, CapRights::PURGE),
            (TOPO_ARENA_RIGHT_DESTROY, CapRights::DESTROY),
            (TOPO_ARENA_RIGHT_DELEGATE, CapRights::DELEGATE),
        ] {
            if cfg.rights & bit != 0 {
                r = r.union(right);
            }
        }
        r
    };
    let numa = decode_numa(cfg.numa_mode, cfg.numa_node).ok_or(())?;
    Ok((name, Label(cfg.label), quota, rights, numa))
}

/// Decode a `TOPO_NUMA_*` mode + node pair.
fn decode_numa(mode: u32, node: u32) -> Option<NumaPolicy> {
    Some(match mode {
        TOPO_NUMA_OS_DEFAULT => NumaPolicy::OsDefault,
        TOPO_NUMA_LOCAL => NumaPolicy::Local,
        TOPO_NUMA_INTERLEAVE => NumaPolicy::Interleave,
        TOPO_NUMA_BIND => {
            if node >= known_numa_nodes() {
                return None; // §22.4 "validate policy"
            }
            NumaPolicy::Bind(NodeId(node))
        }
        TOPO_NUMA_ARENA_POLICY => NumaPolicy::ArenaPolicy,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `int topo_arena_create(const topo_arena_config_t* cfg, topo_arena_t*
/// out_id)` (§22.4, F-005): create an explicit arena and write its id to
/// `out_id` only on success. `cfg` may be null for the all-default
/// (ambient-rights, unlimited-quota) arena. Returns `0` or a positive errno
/// (also set as `errno`).
///
/// # Safety
///
/// `cfg` is null or points to a valid [`TopoArenaConfig`] whose `name` is null
/// or a valid NUL-terminated C string; `out_id` is null (the id is discarded —
/// useless but harmless) or a valid `uint32_t` out-pointer.
#[no_mangle]
pub unsafe extern "C" fn topo_arena_create(cfg: *const TopoArenaConfig, out_id: *mut u32) -> i32 {
    let default_cfg = TopoArenaConfig {
        name: ptr::null(),
        quota_bytes: 0,
        rights: 0,
        numa_mode: TOPO_NUMA_OS_DEFAULT,
        numa_node: 0,
        label: 0,
    };
    let c = if cfg.is_null() {
        &default_cfg
    } else {
        // SAFETY: non-null `cfg` is valid per this fn's contract.
        unsafe { &*cfg }
    };
    // SAFETY: `c.name`'s validity is this fn's contract, forwarded.
    let Ok((name, label, quota, rights, numa)) = (unsafe { decode_config(c, CapRights::ALL) })
    else {
        set_errno(EINVAL);
        return EINVAL;
    };
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ENOMEM;
    };
    let core_cfg = ArenaConfig {
        name,
        label,
        quota_limit: quota,
        rights,
        numa,
        ..ArenaConfig::default()
    };
    match a.arena_create(&core_cfg) {
        Ok(id) => {
            if !out_id.is_null() {
                // SAFETY: `out_id` is a valid out-pointer per the contract.
                unsafe { *out_id = id.0 };
            }
            0
        }
        Err(e) => {
            let code = errno_of(e);
            set_errno(code);
            code
        }
    }
}

/// `int topo_arena_delegate(topo_arena_t parent, const topo_arena_config_t*
/// cfg, topo_arena_t* out_id)` (§36.4, W9-5): mint an attenuated child arena.
/// The child's rights must be a subset of the parent's (`rights == 0` ⇒
/// exactly the parent's), its quota (`0` is **invalid** here — a child without
/// a bound would defeat quota monotonicity) is reserved from the parent's
/// remaining quota, and its label must equal the parent's.
///
/// # Safety
///
/// As [`topo_arena_create`], with `cfg` required non-null.
#[no_mangle]
pub unsafe extern "C" fn topo_arena_delegate(
    parent: u32,
    cfg: *const TopoArenaConfig,
    out_id: *mut u32,
) -> i32 {
    if cfg.is_null() {
        set_errno(EINVAL);
        return EINVAL;
    }
    // SAFETY: non-null `cfg` is valid per this fn's contract.
    let c = unsafe { &*cfg };
    if c.quota_bytes == 0 {
        set_errno(EINVAL); // an unbounded child defeats §36.4 quota monotonicity
        return EINVAL;
    }
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ENOMEM;
    };
    // `rights == 0` ⇒ the parent's own rights (maximal legal attenuation).
    let parent_rights = match a.arena_rights(ArenaId(parent)) {
        Some(r) => r,
        None => {
            set_errno(EINVAL);
            return EINVAL;
        }
    };
    // SAFETY: `c.name`'s validity is this fn's contract, forwarded.
    let Ok((name, label, quota, rights, _numa)) = (unsafe { decode_config(c, parent_rights) })
    else {
        set_errno(EINVAL);
        return EINVAL;
    };
    let spec = DelegationSpec {
        name,
        rights,
        quota_limit: quota,
        label,
        allocator: ArenaConfig::default().allocator,
    };
    match a.arena_delegate(ArenaId(parent), &spec) {
        Ok(id) => {
            if !out_id.is_null() {
                // SAFETY: `out_id` is a valid out-pointer per the contract.
                unsafe { *out_id = id.0 };
            }
            0
        }
        Err(e) => {
            let code = errno_of(e);
            set_errno(code);
            code
        }
    }
}

/// `int topo_arena_configure(topo_arena_t id, uint32_t numa_mode, uint32_t
/// numa_node)` (F-005 "configure", §15.5): update the arena's NUMA placement
/// mode — the mutable policy subset; identity/authority fields are immutable
/// after creation (capabilities only attenuate, via delegation). Requires the
/// arena's `PURGE` right.
#[no_mangle]
pub extern "C" fn topo_arena_configure(id: u32, numa_mode: u32, numa_node: u32) -> i32 {
    let Some(numa) = decode_numa(numa_mode, numa_node) else {
        set_errno(EINVAL);
        return EINVAL;
    };
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ENOMEM;
    };
    finish(a.arena_configure(ArenaId(id), numa))
}

/// `int topo_arena_reset(topo_arena_t id)` (§22.5, F-005): discard **every**
/// outstanding allocation of the arena — all its pointers become invalid —
/// returning/retaining backing per policy; the arena remains active. The
/// default arena (id 0) is protected (`EPERM`). Requires the arena's
/// `DESTROY` right.
#[no_mangle]
pub extern "C" fn topo_arena_reset(id: u32) -> i32 {
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ENOMEM;
    };
    finish(a.arena_reset(ArenaId(id)))
}

/// `int topo_arena_destroy(topo_arena_t id)` (§22.6/§36.13, F-005): run the
/// ordered revocation protocol (drain → unmap → scrub-if-needed → revoke →
/// recycle) and retire the id forever. Live delegated children are destroyed
/// first (revocation of derived authority). On a partial failure the arena is
/// quarantined — never reported destroyed — and a later `topo_arena_destroy`
/// retries the protocol. The default arena is protected (`EPERM`). Requires
/// the arena's `DESTROY` right.
#[no_mangle]
pub extern "C" fn topo_arena_destroy(id: u32) -> i32 {
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ENOMEM;
    };
    finish(a.arena_destroy(ArenaId(id)))
}

/// `void* topo_mallocx_arena(topo_arena_t arena, size_t size, topo_flags_t
/// flags)` (§36.14, §10.3): allocate from an explicit arena — the entry point
/// for arena ids beyond the flag word's `TOPO_ARENA` field. The flag word may
/// not name a *different* arena (`EINVAL`). Unknown/inactive arena ⇒ `EINVAL`/
/// `EBUSY`; missing `ALLOC` right ⇒ `EPERM`; quota exceeded or OOM ⇒ null +
/// `ENOMEM`; `size == 0` follows the zero-size policy (§9.6).
#[no_mangle]
pub extern "C" fn topo_mallocx_arena(arena: u32, size: usize, flags: u64) -> *mut c_void {
    let Some((align, f)) = crate::extended::decode_public_flags(flags) else {
        set_errno(EINVAL);
        return ptr::null_mut();
    };
    if let Some(explicit) = f.explicit_arena() {
        if explicit != ArenaId(arena) {
            set_errno(EINVAL); // conflicting routing request (§10.4)
            return ptr::null_mut();
        }
    }
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    // Deterministic error split before the attempt (the W8 audit rule): an
    // id that names no live arena is EINVAL; a live-but-inactive arena is
    // EBUSY; a live arena without the ALLOC right is EPERM. The allocation
    // itself then reports ENOMEM (quota or OOM). The registry's gate
    // re-validates all three under its own ordering — this split only
    // chooses the errno.
    match a.arena_state(ArenaId(arena)) {
        None => {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        Some(s) if !s.allows_alloc() => {
            set_errno(EBUSY);
            return ptr::null_mut();
        }
        Some(_) => {}
    }
    if let Some(rights) = a.arena_rights(ArenaId(arena)) {
        if !rights.contains(CapRights::ALLOC) {
            set_errno(EPERM);
            return ptr::null_mut();
        }
    }
    if size == 0 && crate::policy::zero_size_policy() == crate::policy::ZeroSizePolicy::Null {
        return ptr::null_mut();
    }
    alloc_protocol(|| a.allocate_in(ArenaId(arena), size, align.max(topo_core::MIN_ALIGN), f))
        .cast::<c_void>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errno_shim::get_errno;
    use crate::{topo_dallocx, topo_mallocx};

    /// Build a C config on the stack (no name).
    fn ccfg(quota: u64, rights: u32) -> TopoArenaConfig {
        TopoArenaConfig {
            name: ptr::null(),
            quota_bytes: quota,
            rights,
            numa_mode: TOPO_NUMA_OS_DEFAULT,
            numa_node: 0,
            label: 0,
        }
    }

    /// Test wrapper: every config/out-pointer below is a valid local.
    fn create(cfg: &TopoArenaConfig) -> (i32, u32) {
        let mut id = u32::MAX;
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_create(cfg, &mut id) };
        (rc, id)
    }

    #[test]
    fn create_allocate_reset_destroy_roundtrip() {
        let (rc, id) = create(&ccfg(0, 0));
        assert_eq!(rc, 0);
        assert_ne!(id, 0, "explicit arenas never get the default id");

        // Allocate via the handle entry point and via the flag word.
        let p = topo_mallocx_arena(id, 256, 0);
        assert!(!p.is_null());
        let q = if id <= 254 {
            let q = topo_mallocx(64, crate::extended::topo_arena(id));
            assert!(!q.is_null());
            q
        } else {
            core::ptr::null_mut()
        };

        // Reset: outstanding pointers become invalid (the §22.5 contract —
        // they are deliberately abandoned here); the arena stays usable.
        assert_eq!(topo_arena_reset(id), 0);
        let r = topo_mallocx_arena(id, 64, 0);
        assert!(!r.is_null());
        // SAFETY: `r` was just allocated and is owned here.
        unsafe { topo_dallocx(r, 0) };

        // Destroy retires the id forever.
        assert_eq!(topo_arena_destroy(id), 0);
        assert!(topo_mallocx_arena(id, 64, 0).is_null());
        assert_eq!(
            get_errno(),
            EINVAL,
            "stale ids are one deterministic EINVAL"
        );
        assert_eq!(topo_arena_destroy(id), EINVAL);
        let _ = (p, q); // abandoned to the reset (never freed)
    }

    #[test]
    fn lifecycle_errors_map_to_the_documented_errnos() {
        // The default arena is protected.
        assert_eq!(topo_arena_reset(0), EPERM);
        assert_eq!(topo_arena_destroy(0), EPERM);
        // Unknown ids are EINVAL.
        assert_eq!(topo_arena_reset(0xDEAD_BEEF), EINVAL);
        assert_eq!(topo_arena_destroy(0xDEAD_BEEF), EINVAL);
        assert_eq!(
            topo_arena_configure(0xDEAD_BEEF, TOPO_NUMA_LOCAL, 0),
            EINVAL
        );
        // Invalid NUMA configs are EINVAL before any lookup.
        assert_eq!(topo_arena_configure(0, 99, 0), EINVAL);
        assert_eq!(topo_arena_configure(0, TOPO_NUMA_BIND, 7), EINVAL);
    }

    #[test]
    fn quota_and_rights_are_enforced_at_the_c_surface() {
        // A 64 KiB quota: the second 48 KiB medium must fail with ENOMEM.
        let (rc, id) = create(&ccfg(64 * 1024, 0));
        assert_eq!(rc, 0);
        let p = topo_mallocx_arena(id, 48 * 1024, 0);
        assert!(!p.is_null());
        let q = topo_mallocx_arena(id, 48 * 1024, 0);
        assert!(q.is_null());
        assert_eq!(get_errno(), ENOMEM);
        // SAFETY: `p` was just allocated and is owned here.
        unsafe { topo_dallocx(p, 0) };
        assert_eq!(topo_arena_destroy(id), 0);

        // An arena without ALLOC: EPERM on the allocation attempt.
        let (rc, id) = create(&ccfg(0, TOPO_ARENA_RIGHT_FREE | TOPO_ARENA_RIGHT_DESTROY));
        assert_eq!(rc, 0);
        assert!(topo_mallocx_arena(id, 64, 0).is_null());
        assert_eq!(get_errno(), EPERM);
        // An arena without DESTROY can never be reset/destroyed: minted
        // without the authority, it cannot be conjured later (§36.4).
        let (rc, undying) = create(&ccfg(0, TOPO_ARENA_RIGHT_ALLOC));
        assert_eq!(rc, 0);
        assert_eq!(topo_arena_reset(undying), EPERM);
        assert_eq!(topo_arena_destroy(undying), EPERM);
        assert_eq!(topo_arena_destroy(id), 0);
    }

    #[test]
    fn delegation_attenuates_at_the_c_surface() {
        let (rc, parent) = create(&ccfg(1024 * 1024, 0));
        assert_eq!(rc, 0);

        // An unbounded child is invalid (quota monotonicity needs a bound).
        let mut child = 0u32;
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_delegate(parent, &ccfg(0, 0), &mut child) };
        assert_eq!(rc, EINVAL);

        // A child over the parent's remaining quota: EDQUOT.
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_delegate(parent, &ccfg(2 * 1024 * 1024, 0), &mut child) };
        assert_eq!(rc, EDQUOT);

        // A proper attenuation: alloc/free only, half the parent's quota.
        let spec = ccfg(512 * 1024, TOPO_ARENA_RIGHT_ALLOC | TOPO_ARENA_RIGHT_FREE);
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_delegate(parent, &spec, &mut child) };
        assert_eq!(rc, 0);
        let p = topo_mallocx_arena(child, 128, 0);
        assert!(!p.is_null());
        // SAFETY: `p` was just allocated and is owned here.
        unsafe { topo_dallocx(p, 0) };
        // The child cannot delegate (no DELEGATE right): EPERM.
        let mut grand = 0u32;
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_delegate(child, &ccfg(1024, 0), &mut grand) };
        assert_eq!(rc, EPERM);
        // A label change is EPERM (no silent downgrade, §36.4).
        let mut bad = ccfg(1024, 0);
        bad.label = 9;
        // SAFETY: valid config + out-pointer.
        let rc = unsafe { topo_arena_delegate(parent, &bad, &mut grand) };
        assert_eq!(rc, EPERM);

        // Destroying the parent revokes the child too.
        assert_eq!(topo_arena_destroy(parent), 0);
        assert!(topo_mallocx_arena(child, 64, 0).is_null());
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn names_are_bounded_and_read_safely() {
        let name = c"transaction-pool";
        let cfg = TopoArenaConfig {
            name: name.as_ptr(),
            ..ccfg(0, 0)
        };
        let (rc, id) = create(&cfg);
        assert_eq!(rc, 0);
        assert_eq!(topo_arena_destroy(id), 0);

        // A 33-byte name is rejected with EINVAL.
        let long = c"a-name-that-is-decidedly-too-long";
        assert_eq!(long.to_bytes().len(), 33);
        let cfg = TopoArenaConfig {
            name: long.as_ptr(),
            ..ccfg(0, 0)
        };
        let (rc, _) = create(&cfg);
        assert_eq!(rc, EINVAL);
    }
}

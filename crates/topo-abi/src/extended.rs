// SPDX-License-Identifier: MIT
//! The extended C API (§10.3/§10.4, plan 06 W8-6): `topo_mallocx` /
//! `topo_rallocx` / `topo_xallocx` / `topo_dallocx` / `topo_sdallocx` /
//! `topo_nallocx` over a validated 64-bit flag word.
//!
//! # The public `topo_flags_t` layout (frozen at 1.0, documented now)
//!
//! | Bits | Field | Macro (header) |
//! |---|---|---|
//! | 0–5 | `lg(alignment)` (0 ⇒ natural alignment) | `TOPO_ALIGN_LG(la)` |
//! | 6 | zero the returned memory | `TOPO_ZERO` |
//! | 7 | bypass local caches | `TOPO_TCACHE_NONE` |
//! | 8 | guard allocation | `TOPO_GUARDED` |
//! | 9 | avoid hugepages | `TOPO_NO_HUGEPAGE` |
//! | 10 | prefer hugepages | `TOPO_PREFER_HUGEPAGE` |
//! | 11–12 | lifetime (0 none / 1 short / 2 medium / 3 long) | `TOPO_LIFETIME_*` |
//! | 13–20 | hotness `0..=255` | `TOPO_HOT(h)` / `TOPO_COLD` |
//! | 21–52 | arena id + 1 (0 ⇒ default arena) | `TOPO_ARENA(id)` |
//! | 53–63 | reserved — MUST be zero | — |
//!
//! This public word is mapped onto the allocator's internal
//! [`RequestFlags`] at this boundary (the internal layout is free to evolve;
//! `flags.rs` documents that split). **Validation is total (§10.4):** any
//! reserved bit, an out-of-range `lg(alignment)`, the contradictory
//! hugepage pair, or an arena that does not exist fails *deterministically*
//! — null/0 result with `errno = EINVAL`, never a silently degraded
//! allocation. The mandatory flag — alignment — is extracted into the
//! dedicated `align` argument of [`classify`], so it can never be dropped
//! as an "advisory bit" (§10.4's "MUST NOT be silently ignored").
//!
//! Advisory flags accepted today and *acted on*: `TOPO_ALIGN_LG`, `TOPO_ZERO`,
//! `TOPO_ARENA(0)`, `TOPO_GUARDED`, `TOPO_TCACHE_NONE` (W6/W7 — a bypassing
//! request is served from, and returned to, the central free list rather than
//! the running core's per-CPU slot; honoured on `topo_mallocx` *and* on
//! `topo_dallocx`/`topo_sdallocx`), and the hugepage/lifetime/hotness
//! placement hints. They validate and thread
//! through to the classifier ([`Request::flags`]) where their subsystems
//! consume them — they are accepted because they
//! are *advisory* (§10.4 allows documented advisory flags to be no-ops, not
//! mandatory ones).
//!
//! [`classify`]: topo_core::classify
//! [`Request::flags`]: topo_core::Request

use core::ffi::c_void;
use core::ptr;

use topo_core::flags::Lifetime;
use topo_core::{
    predicted_usable_size, ArenaId, CapRights, HugepagePolicy, RequestFlags, MIN_ALIGN,
};

use crate::c_api;
use crate::errno_shim::{alloc_protocol, preserving_errno, set_errno, EACCES, EINVAL, ENOMEM};
use crate::global;
use crate::policy::{zero_size_policy, ZeroSizePolicy};

// ---------------------------------------------------------------------------
// Public flag constants (the Rust mirror of the header macros)
// ---------------------------------------------------------------------------

/// Mask of the `lg(alignment)` field (bits 0–5): `TOPO_ALIGN_LG(la)`.
pub const TOPO_ALIGN_LG_MASK: u64 = 0x3f;
/// Zero the returned memory (`TOPO_ZERO`).
pub const TOPO_ZERO: u64 = 1 << 6;
/// Bypass the local caches for this request (`TOPO_TCACHE_NONE`).
pub const TOPO_TCACHE_NONE: u64 = 1 << 7;
/// Sampled/forced guard allocation (`TOPO_GUARDED`).
pub const TOPO_GUARDED: u64 = 1 << 8;
/// Avoid hugepage placement (`TOPO_NO_HUGEPAGE`).
pub const TOPO_NO_HUGEPAGE: u64 = 1 << 9;
/// Prefer hugepage placement (`TOPO_PREFER_HUGEPAGE`).
pub const TOPO_PREFER_HUGEPAGE: u64 = 1 << 10;
/// Expected short lifetime (`TOPO_LIFETIME_SHORT`).
pub const TOPO_LIFETIME_SHORT: u64 = 1 << 11;
/// Expected medium lifetime (`TOPO_LIFETIME_MEDIUM`).
pub const TOPO_LIFETIME_MEDIUM: u64 = 2 << 11;
/// Expected long lifetime (`TOPO_LIFETIME_LONG`).
pub const TOPO_LIFETIME_LONG: u64 = 3 << 11;

const LIFETIME_SHIFT: u32 = 11;
const LIFETIME_MASK: u64 = 0b11 << LIFETIME_SHIFT;
const HOTNESS_SHIFT: u32 = 13;
const HOTNESS_MASK: u64 = 0xff << HOTNESS_SHIFT;
const ARENA_SHIFT: u32 = 21;
const ARENA_MASK: u64 = 0xffff_ffff << ARENA_SHIFT;
const KNOWN_MASK: u64 = TOPO_ALIGN_LG_MASK
    | TOPO_ZERO
    | TOPO_TCACHE_NONE
    | TOPO_GUARDED
    | TOPO_NO_HUGEPAGE
    | TOPO_PREFER_HUGEPAGE
    | LIFETIME_MASK
    | HOTNESS_MASK
    | ARENA_MASK;
/// Bits 53–63: reserved, MUST be zero (§10.4 "invalid flags MUST fail").
const RESERVED_MASK: u64 = !KNOWN_MASK;

/// `TOPO_ALIGN_LG(la)`: request `1 << la` alignment (`la == 0` ⇒ natural).
///
/// An out-of-range `la` (≥ 64) encodes to a **reserved-bit word**, so every
/// entry point rejects it deterministically with `EINVAL` — alignment is
/// mandatory and must never degrade silently (§10.4). Masking it to a
/// different (smaller) alignment, as a naive 6-bit truncation would, was a
/// PR #11 review finding. (`la` in `usize::BITS..64` on narrower targets is
/// caught by the decoder's representability check instead.)
pub const fn topo_align_lg(la: u32) -> u64 {
    if la < 64 {
        la as u64
    } else {
        1 << 63 // reserved bit ⇒ deterministic EINVAL at every entry point
    }
}

/// `TOPO_HOT(h)`: hotness hint `0..=255` (`TOPO_COLD` ≡ `TOPO_HOT(0)`).
pub const fn topo_hot(h: u8) -> u64 {
    (h as u64) << HOTNESS_SHIFT
}

/// `TOPO_ARENA(id)`: allocate from the explicit arena `id` (the field stores
/// `id + 1`; an absent field means the default arena).
pub const fn topo_arena(id: u32) -> u64 {
    ((id as u64) + 1) << ARENA_SHIFT
}

/// Decode and validate a public flag word into `(alignment, internal flags)`.
/// `None` is the deterministic §10.4 failure: reserved bits, an unrepresentable
/// alignment, the contradictory hugepage pair, or an arena id the internal
/// encoding cannot carry. `pub(crate)` so the handle-routed `topo_mallocx_arena`
/// (arena_api) shares the identical flag validation.
pub(crate) fn decode_flags(flags: u64) -> Option<(usize, RequestFlags)> {
    if flags & RESERVED_MASK != 0 {
        return None;
    }
    let la = (flags & TOPO_ALIGN_LG_MASK) as u32;
    if la >= usize::BITS {
        return None; // 1 << la would not be a representable alignment
    }
    let align = 1usize << la;

    let mut f = RequestFlags::NONE;
    if flags & TOPO_ZERO != 0 {
        f = f.with_zero();
    }
    if flags & TOPO_TCACHE_NONE != 0 {
        f = f.with_cache_bypass();
    }
    if flags & TOPO_GUARDED != 0 {
        f = f.with_guarded();
    }
    match (
        flags & TOPO_NO_HUGEPAGE != 0,
        flags & TOPO_PREFER_HUGEPAGE != 0,
    ) {
        (true, true) => return None, // contradictory (§10.4)
        (true, false) => f = f.with_hugepage(HugepagePolicy::Avoid),
        (false, true) => f = f.with_hugepage(HugepagePolicy::Prefer),
        (false, false) => {}
    }
    f = f.with_lifetime(match (flags & LIFETIME_MASK) >> LIFETIME_SHIFT {
        1 => Lifetime::Short,
        2 => Lifetime::Medium,
        3 => Lifetime::Long,
        _ => Lifetime::Unspecified,
    });
    f = f.with_hotness(((flags & HOTNESS_MASK) >> HOTNESS_SHIFT) as u8);

    let arena_field = ((flags & ARENA_MASK) >> ARENA_SHIFT) as u32;
    if arena_field != 0 {
        // The field stores id + 1. Two rejection layers, one error code:
        // an id beyond the flag-encodable range fails here (decode), and an id
        // naming an arena that does not *exist* fails the entry point's
        // existence check ([`arena_routable`]) — both are EINVAL, so "no such
        // arena" is one deterministic failure regardless of the id (§10.4).
        f = f.with_arena(arena_field.checked_sub(1)?)?;
    }
    Some((align, f))
}

/// Whether `arena` can be *named* right now: the default arena always, or an
/// explicit arena that is registered and `Active` in the global engine (§22.3,
/// plan 06 W9). A request naming an unregistered, draining, or destroyed arena is
/// a §10.4 *invalid argument* — reported as `EINVAL`, distinct from the `ENOMEM`
/// of a genuine allocation failure.
///
/// This is existence/liveness only — the right check for the paths that merely
/// *name* an arena without allocating *from* it (`topo_rallocx` preserves the
/// original allocation's arena across a move, §25.4; `topo_xallocx` resizes in
/// place). The paths that allocate from the flag arena use [`alloc_routing`],
/// which additionally honors the §36.4 `ALLOC` authority.
fn arena_routable(arena: ArenaId) -> bool {
    arena == ArenaId::DEFAULT || global().is_some_and(|a| a.arena_is_active(arena))
}

/// The §36.4 authority disposition of routing a **new** allocation to `arena`
/// (the `topo_mallocx` / `topo_nallocx` front door). It separates the two ways a
/// flag-routed allocation can be refused *before* the engine even tries, so each
/// carries the same taxonomy the generation-checked handle path
/// (`topo_mallocx_arena`, §36.14) reports:
///
/// * [`NoSuchArena`](AllocRouting::NoSuchArena) — unregistered / draining /
///   destroyed: a §10.4 invalid argument (`EINVAL`).
/// * [`Denied`](AllocRouting::Denied) — registered and `Active`, but the arena's
///   cap lacks [`CapRights::ALLOC`] (§36.4): an *authority* denial (`EACCES`),
///   **not** the `ENOMEM` a quota/storage failure would raise. Collapsing this to
///   `ENOMEM` (the engine gate's bare-null result) would misreport "you may not
///   allocate here" as "out of memory" and let `topo_nallocx` predict a nonzero
///   size for an arena it can never serve (a PR #13 review finding).
enum AllocRouting {
    Ok,
    NoSuchArena,
    Denied,
}

fn alloc_routing(arena: ArenaId) -> AllocRouting {
    if arena == ArenaId::DEFAULT {
        return AllocRouting::Ok; // ambient authority: always allocatable
    }
    match global().and_then(|a| a.arena_stats(arena)) {
        Some(s) if s.state.is_active() => {
            if s.rights.contains(CapRights::ALLOC) {
                AllocRouting::Ok
            } else {
                AllocRouting::Denied
            }
        }
        _ => AllocRouting::NoSuchArena,
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `void* topo_mallocx(size_t size, topo_flags_t flags)` (§10.3): allocate
/// with extended flags. Invalid flags fail deterministically with `NULL` +
/// `errno = EINVAL` (§10.4); a flag naming an arena whose cap lacks
/// [`CapRights::ALLOC`] is an authority denial — `NULL` + `EACCES` (§36.4),
/// matching the handle path; allocation failure (quota/storage) is `NULL` +
/// `ENOMEM`; `size == 0` follows the zero-size policy (§9.6).
#[no_mangle]
pub extern "C" fn topo_mallocx(size: usize, flags: u64) -> *mut c_void {
    let Some((align, f)) = decode_flags(flags) else {
        set_errno(EINVAL);
        return ptr::null_mut();
    };
    match alloc_routing(f.arena()) {
        AllocRouting::Ok => {}
        // No such (active) arena: a deterministic invalid argument (§10.4),
        // distinct from the ENOMEM of a real allocation failure.
        AllocRouting::NoSuchArena => {
            set_errno(EINVAL);
            return ptr::null_mut();
        }
        // The arena exists and is Active but its cap forbids allocation: an
        // authority denial (§36.4), reported as the handle path reports it.
        AllocRouting::Denied => {
            set_errno(EACCES);
            return ptr::null_mut();
        }
    }
    if size == 0 && zero_size_policy() == ZeroSizePolicy::Null {
        return ptr::null_mut();
    }
    alloc_protocol(|| {
        global().map_or(ptr::null_mut(), |a| {
            a.allocate(size, align.max(MIN_ALIGN), f)
        })
    })
    .cast::<c_void>()
}

/// `void* topo_rallocx(void* ptr, size_t size, topo_flags_t flags)` (§10.3):
/// reallocate under the §25 contract — on failure (`NULL`) the original
/// allocation is untouched. `TOPO_ALIGN_LG` is honored on the result;
/// `TOPO_ZERO` zeroes any bytes beyond the preserved prefix. A null `ptr`
/// behaves as `topo_mallocx`; `size == 0` follows the public
/// `realloc(p, 0)` policy (free and return `NULL`, errno untouched); an
/// invalid `ptr` or flag word is `NULL` + `EINVAL` with no state change.
///
/// # Safety
///
/// As [`crate::topomalloc_realloc`]: `ptr` must be null, an owned live
/// allocation, or never-owned memory (rejected with `EINVAL`).
#[no_mangle]
pub unsafe extern "C" fn topo_rallocx(ptr: *mut c_void, size: usize, flags: u64) -> *mut c_void {
    let Some((align, f)) = decode_flags(flags) else {
        set_errno(EINVAL);
        return ptr::null_mut();
    };
    if !arena_routable(f.arena()) {
        // A flag naming a nonexistent arena fails deterministically, leaving the
        // original untouched (§10.4 / §25.1). The engine's realloc preserves the
        // original allocation's arena across a move (§25.4); the validated flag
        // arena is not used to migrate it.
        set_errno(EINVAL);
        return ptr::null_mut();
    }
    if ptr.is_null() {
        return topo_mallocx(size, flags);
    }
    if size == 0 {
        // The public realloc(p != NULL, 0) policy (§25.1 / docs/ABI.md):
        // free and return NULL with errno untouched — not the engine's
        // internal zero-as-one rule (PR #11 review). The flag word was
        // already validated above, so an invalid word never frees.
        // SAFETY: this entry point's contract covers the free of `ptr`.
        unsafe { c_api::topomalloc_free(ptr) };
        return ptr::null_mut();
    }
    let Some(a) = global() else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    if !a.owns(ptr.cast::<u8>()) {
        set_errno(EINVAL);
        return ptr::null_mut();
    }
    // SAFETY: `ptr` is owned-and-live per this entry point's contract (the
    // never-owned case was rejected by `owns` above).
    alloc_protocol(|| unsafe { a.realloc(ptr.cast::<u8>(), size, align.max(MIN_ALIGN), f) })
        .cast::<c_void>()
}

/// `size_t topo_xallocx(void* ptr, size_t size, size_t extra, topo_flags_t
/// flags)` (§10.3): resize `ptr` **in place only**, to at least `size` bytes
/// (best effort toward `size + extra`), and return the allocation's real
/// (usable) size — the caller checks `result >= size` for success, as with
/// jemalloc's `xallocx`. Never moves, never frees, never fails destructively.
///
/// It performs a real in-place resize (W15-3a/b): a medium/large allocation
/// **shrinks** (returning the tail pages to the backend, §25.3) or **grows** by
/// absorbing the adjacent free extent (§25.2) — no copy, the base unchanged — and
/// reports the achieved usable size; a small object's fixed slab slot cannot
/// resize, so its usable size is reported unchanged (success iff it already
/// satisfies `size`). A grow the backend cannot satisfy in place (no adjacent free
/// extent) leaves the allocation as-is and reports the old size (`< size` ⇒ the
/// caller sees the documented in-place failure and may `rallocx`). Deterministic
/// `0`+`EINVAL` failures — invalid flags, a flag naming a nonexistent/inactive
/// arena (§10.4, as the sibling entry points), a pointer this allocator does not
/// own, or a `TOPO_ALIGN_LG` the existing base cannot satisfy (in-place resizing
/// can never change an address) — the standard declares these undefined; we
/// document them instead. `extra` is best-effort headroom that is accepted but not
/// pursued (the resize targets `size`).
///
/// # Safety
///
/// As [`topo_rallocx`] — `ptr` is a resize target; the same ownership
/// contract applies (and will be exercised once in-place growth lands).
#[no_mangle]
pub unsafe extern "C" fn topo_xallocx(
    ptr: *mut c_void,
    size: usize,
    extra: usize,
    flags: u64,
) -> usize {
    // `extra` is best-effort headroom we do not pursue: the resize targets `size`
    // (which always satisfies the `result >= size` contract), so a non-zero `extra`
    // is accepted but not chased (jemalloc permits "inability to allocate the extra
    // by itself is not a failure"). `size` now drives a real in-place resize (W15-3a
    // grow / W15-3b shrink).
    let _ = extra;
    let Some((align, f)) = decode_flags(flags) else {
        set_errno(EINVAL);
        return 0;
    };
    // A flag naming a nonexistent/inactive arena is a §10.4 invalid argument,
    // exactly as in `topo_mallocx`/`topo_rallocx`/`topo_nallocx` — without this,
    // `xallocx` alone would accept `TOPO_ARENA(uncreated_id)` and report a usable
    // size instead of the documented `0 + EINVAL` (a PR #13 review finding). The
    // existence check is all that applies: like `rallocx`, `xallocx` does not
    // allocate *from* the flag arena (it resizes in place), so the arena's ALLOC
    // authority is not consulted here.
    if !arena_routable(f.arena()) {
        set_errno(EINVAL);
        return 0;
    }
    preserving_errno(|| {
        let a = global()?;
        // Resize **in place only** toward `size`: a medium/large allocation shrinks
        // (returning the tail, W15-3b) or grows (absorbing the adjacent free extent,
        // W15-3a); a small object's fixed slot reports its usable size unchanged.
        // `None` (non-owned/interior pointer, or a base that cannot satisfy `align`
        // in place — in-place can never re-base) is the deterministic invalid-request.
        // The returned usable size is the caller's `result >= size` success signal.
        // SAFETY: xallocx's contract — `ptr` is a live resize target the caller owns
        // and does not concurrently free/realloc.
        unsafe { a.resize_in_place(ptr.cast::<u8>(), size, align, f) }
    })
    .unwrap_or_else(|| {
        set_errno(EINVAL);
        0
    })
}

/// `void topo_dallocx(void* ptr, topo_flags_t flags)` (§10.3): free with
/// flags. The flag word is validated; on the deallocation path every flag is
/// advisory (alignment/tcache routing hints), so an *invalid* word is
/// reported in debug builds but the free still proceeds — never leaking over
/// a hint — and `errno` is never modified.
///
/// # Safety
///
/// As [`crate::topomalloc_free`].
#[no_mangle]
pub unsafe extern "C" fn topo_dallocx(ptr: *mut c_void, flags: u64) {
    debug_assert!(
        decode_flags(flags).is_some(),
        "topo_dallocx: invalid flag word {flags:#x}"
    );
    // `TOPO_TCACHE_NONE` is honoured here: the object goes straight to the central free
    // list instead of the running core's front-end slot. Both are correct (the front end
    // only delays the trip to central), so this is a locality/residency choice — but it
    // is the one the flag promises, and a decoded-then-discarded flag is a lie.
    // SAFETY: identical contract, forwarded through the shared `topomalloc_free` body,
    // so `errno` preservation and the non-`Freed`-outcome policy are the same.
    unsafe { c_api::free_with_flag(ptr, flags & TOPO_TCACHE_NONE != 0) };
}

/// `void topo_sdallocx(void* ptr, size_t size, topo_flags_t flags)` (§10.3):
/// sized free with flags — the extended-API counterpart of C23
/// `free_sized`/`free_aligned_sized` (W8-3). The size (and any
/// `TOPO_ALIGN_LG`) hint is cross-checked in debug/hardened builds and never
/// trusted for the actual free, so a wrong hint cannot corrupt state.
///
/// # Safety
///
/// As [`crate::topomalloc_free_sized`].
#[no_mangle]
pub unsafe extern "C" fn topo_sdallocx(ptr: *mut c_void, size: usize, flags: u64) {
    let decoded = decode_flags(flags);
    debug_assert!(
        decoded.is_some(),
        "topo_sdallocx: invalid flag word {flags:#x}"
    );
    let bypass = flags & TOPO_TCACHE_NONE != 0;
    let align = match decoded {
        Some((a, _)) if a > 1 => a,
        _ => 1,
    };
    // SAFETY: identical contract, forwarded; `TOPO_TCACHE_NONE` routed as in
    // [`topo_dallocx`].
    unsafe { c_api::free_sized_with(ptr, align, size, bypass) }
}

/// `size_t topo_nallocx(size_t size, topo_flags_t flags)` (§10.3): the usable
/// size `topo_mallocx(size, flags)` would return, without allocating. `0` for
/// an invalid flag word, an unsatisfiable request (overflow), or a zero size
/// under the `zero_null` policy. Pure: never touches `errno` or any state.
///
/// Computed by [`predicted_usable_size`] — the same formula the allocation
/// path uses — so the prediction holds for over-aligned requests too, where
/// the carved length (size-rounded) is smaller than classify's
/// `max(size, align)` span (PR #11 review).
#[no_mangle]
pub extern "C" fn topo_nallocx(size: usize, flags: u64) -> usize {
    let Some((align, f)) = decode_flags(flags) else {
        return 0;
    };
    // Mirror `topo_mallocx`: a request it would refuse — no such arena (EINVAL)
    // or an arena whose cap lacks ALLOC (EACCES) — predicts a 0 usable size,
    // since `topo_mallocx` would return null. (Pure: errno is left untouched.)
    if !matches!(alloc_routing(f.arena()), AllocRouting::Ok) {
        return 0;
    }
    if size == 0 && zero_size_policy() == ZeroSizePolicy::Null {
        return 0;
    }
    predicted_usable_size(size, align.max(MIN_ALIGN), f).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c_api::{topomalloc_free, topomalloc_malloc_usable_size};
    use crate::errno_shim::get_errno;

    /// Test wrappers: pointers passed here are test-owned (or never-owned
    /// probes the entry points reject) — the entry points' safety contracts.
    fn tdallocx(p: *mut c_void, flags: u64) {
        // SAFETY: test-owned pointer; see above.
        unsafe { topo_dallocx(p, flags) }
    }
    fn tsdallocx(p: *mut c_void, size: usize, flags: u64) {
        // SAFETY: as `tdallocx`.
        unsafe { topo_sdallocx(p, size, flags) }
    }
    fn trallocx(p: *mut c_void, size: usize, flags: u64) -> *mut c_void {
        // SAFETY: as `tdallocx`.
        unsafe { topo_rallocx(p, size, flags) }
    }
    fn txallocx(p: *mut c_void, size: usize, extra: usize, flags: u64) -> usize {
        // SAFETY: as `tdallocx`.
        unsafe { topo_xallocx(p, size, extra, flags) }
    }
    fn tfree(p: *mut c_void) {
        // SAFETY: as `tdallocx`.
        unsafe { topomalloc_free(p) }
    }

    #[test]
    fn public_flag_layout_is_pinned() {
        // The numeric layout is ABI (mirrored by the header macros); pin it.
        assert_eq!(TOPO_ALIGN_LG_MASK, 0x3f);
        assert_eq!(TOPO_ZERO, 0x40);
        assert_eq!(TOPO_TCACHE_NONE, 0x80);
        assert_eq!(TOPO_GUARDED, 0x100);
        assert_eq!(TOPO_NO_HUGEPAGE, 0x200);
        assert_eq!(TOPO_PREFER_HUGEPAGE, 0x400);
        assert_eq!(TOPO_LIFETIME_SHORT, 0x800);
        assert_eq!(TOPO_LIFETIME_MEDIUM, 0x1000);
        assert_eq!(TOPO_LIFETIME_LONG, 0x1800);
        assert_eq!(topo_align_lg(12), 12);
        // Out-of-range lg-alignment encodes a reserved bit, never a different
        // alignment (PR #11 review) — every entry point then EINVALs.
        assert_eq!(topo_align_lg(64), 1 << 63);
        assert_eq!(topo_align_lg(u32::MAX), 1 << 63);
        assert!(decode_flags(topo_align_lg(64)).is_none());
        assert_eq!(topo_hot(255), 255 << 13);
        assert_eq!(topo_arena(0), 1 << 21);
        assert_eq!(topo_arena(41), 42 << 21);
    }

    #[test]
    fn decode_validates_deterministically() {
        // Valid combinations decode.
        assert!(decode_flags(0).is_some());
        assert!(decode_flags(TOPO_ZERO | topo_align_lg(6) | TOPO_LIFETIME_LONG).is_some());
        // Reserved bits fail (§10.4).
        assert!(decode_flags(1 << 53).is_none());
        assert!(decode_flags(u64::MAX).is_none());
        // Contradictory hugepage pair fails.
        assert!(decode_flags(TOPO_NO_HUGEPAGE | TOPO_PREFER_HUGEPAGE).is_none());
        // Every encodable lg-alignment below the pointer width decodes to
        // exactly 1 << la; anything at/above it is rejected (the decoder
        // guard — load-bearing on 32-bit targets, exhaustive here).
        for la in 0..64u32 {
            let decoded = decode_flags(la as u64);
            if la < usize::BITS {
                assert_eq!(decoded.unwrap().0, 1usize << la);
            } else {
                assert!(decoded.is_none(), "la={la} must be rejected");
            }
        }
        // The decoded alignment and hints are faithful.
        let (align, f) = decode_flags(topo_align_lg(9) | TOPO_ZERO).unwrap();
        assert_eq!(align, 512);
        assert!(f.hints().zero);
        // Any flag-encodable arena id (0..=MAX_ARENA_ID) decodes; whether the
        // arena *exists* is an entry-point check, not a decode concern (plan 06
        // W9). An id beyond the encodable range is rejected at decode — it
        // collapses to the same EINVAL as the entry point's "no such arena".
        assert!(decode_flags(topo_arena(0)).is_some());
        assert!(decode_flags(topo_arena(1)).is_some());
        assert!(decode_flags(topo_arena(254)).is_some());
        assert!(decode_flags(topo_arena(255)).is_none());
        assert!(decode_flags(topo_arena(u32::MAX)).is_none());
    }

    #[test]
    fn mallocx_honors_align_and_zero_and_validates() {
        // Invalid flags: deterministic EINVAL, no allocation.
        set_errno(0);
        assert!(topo_mallocx(64, 1 << 60).is_null());
        assert_eq!(get_errno(), EINVAL);

        // Aligned + zeroed allocation.
        let p = topo_mallocx(300, topo_align_lg(8) | TOPO_ZERO);
        assert!(!p.is_null());
        assert_eq!(p as usize % 256, 0, "TOPO_ALIGN_LG must be honored (§10.4)");
        let usable = topomalloc_malloc_usable_size(p);
        // SAFETY: `usable` readable bytes, zeroed per TOPO_ZERO.
        unsafe {
            for i in 0..usable {
                assert_eq!(p.cast::<u8>().add(i).read(), 0);
            }
        }
        tdallocx(p, topo_align_lg(8));

        // A nonexistent arena is an invalid argument — EINVAL regardless of
        // the id's magnitude (an uncreated id and an encoding-overflow id
        // alike). Ids ≥ 200 are never created by the create tests (which
        // assign sequentially from 1), so they stay reliably "nonexistent" in
        // this shared-process test.
        set_errno(0);
        assert!(topo_mallocx(64, topo_arena(200)).is_null());
        assert_eq!(get_errno(), EINVAL, "arena 200 was never created");
        set_errno(0);
        assert!(topo_mallocx(64, topo_arena(300)).is_null());
        assert_eq!(get_errno(), EINVAL, "same failure for unencodable ids");
        assert_eq!(topo_nallocx(64, topo_arena(200)), 0, "nallocx mirrors it");
        // rallocx with a bad arena leaves the original untouched.
        let keep = topo_mallocx(16, 0);
        // SAFETY: `keep` is live and test-owned.
        unsafe { keep.cast::<u8>().write(0x5A) };
        set_errno(0);
        assert!(trallocx(keep, 64, topo_arena(201)).is_null());
        assert_eq!(get_errno(), EINVAL);
        // SAFETY: `keep` is still live with its content.
        unsafe { assert_eq!(keep.cast::<u8>().read(), 0x5A) };
        tdallocx(keep, 0);
        // The default arena, named explicitly, works.
        let q = topo_mallocx(64, topo_arena(0));
        assert!(!q.is_null());
        tdallocx(q, 0);
    }

    #[test]
    fn rallocx_zero_size_follows_the_public_realloc_policy() {
        // rallocx(p != NULL, 0) frees and returns NULL with errno untouched
        // (PR #11 review) — the same §25.1 policy as topomalloc_realloc, not
        // the engine's internal zero-as-one rule.
        let p = topo_mallocx(64, 0);
        assert!(!p.is_null());
        set_errno(5);
        assert!(trallocx(p, 0, 0).is_null());
        assert_eq!(get_errno(), 5, "freeing via rallocx(p, 0) is not an error");
        // The object was genuinely freed (not leaked): its slot is vended again. Every
        // front-end layer is *shared* — a per-CPU slot serves whichever threads run on that
        // core, and the transfer cache is process-wide — so a parallel test thread can
        // transiently take the freed object. Confirm recycling by membership in a bounded
        // batch (held to drain toward the freed slot, then freed), not by asserting the
        // exact next call.
        let mut batch = Vec::with_capacity(64);
        let mut recycled = false;
        for _ in 0..64 {
            let x = topo_mallocx(64, 0);
            recycled |= x == p;
            batch.push(x);
        }
        for x in batch {
            tdallocx(x, 0);
        }
        assert!(
            recycled,
            "rallocx(p, 0) must actually free (p recycled within a batch)"
        );

        // An *invalid flag word* with size 0 must NOT free: validation comes
        // first, deterministically (§10.4).
        let r = topo_mallocx(64, 0);
        set_errno(0);
        assert!(trallocx(r, 0, 1 << 60).is_null());
        assert_eq!(get_errno(), EINVAL);
        // SAFETY: `r` is still live (the invalid word freed nothing).
        unsafe { r.cast::<u8>().write(0x7E) };
        tdallocx(r, 0);
    }

    #[test]
    fn rallocx_preserves_original_on_failure() {
        let p = topo_mallocx(100, 0);
        assert!(!p.is_null());
        // SAFETY: 100 usable bytes.
        unsafe { p.cast::<u8>().write_bytes(0xAB, 100) };

        // Invalid flags: EINVAL, original untouched.
        set_errno(0);
        assert!(trallocx(p, 200, 1 << 63).is_null());
        assert_eq!(get_errno(), EINVAL);
        // SAFETY: p is still live.
        unsafe { assert_eq!(p.cast::<u8>().read(), 0xAB) };

        // Unsatisfiable growth: ENOMEM, original untouched (§25.4).
        set_errno(0);
        assert!(trallocx(p, usize::MAX - 63, 0).is_null());
        assert_eq!(get_errno(), ENOMEM);
        // SAFETY: p is still live.
        unsafe { assert_eq!(p.cast::<u8>().add(99).read(), 0xAB) };

        // A real move with raised alignment + zero: prefix kept, tail zeroed.
        let q = trallocx(p, 4096, topo_align_lg(9) | TOPO_ZERO);
        assert!(!q.is_null());
        assert_eq!(q as usize % 512, 0);
        // SAFETY: 4096 usable bytes at q.
        unsafe {
            assert_eq!(q.cast::<u8>().read(), 0xAB);
            assert_eq!(q.cast::<u8>().add(99).read(), 0xAB);
            // Bytes beyond the old usable size (112-byte class) are zero.
            assert_eq!(q.cast::<u8>().add(112).read(), 0);
            assert_eq!(q.cast::<u8>().add(4095).read(), 0);
        }
        tsdallocx(q, 4096, topo_align_lg(9));
    }

    #[test]
    fn xallocx_succeeds_only_within_current_usable() {
        let p = topo_mallocx(100, 0); // 112-byte class
        let usable = topomalloc_malloc_usable_size(p);
        assert_eq!(txallocx(p, 100, 0, 0), usable, "already satisfied");
        assert_eq!(txallocx(p, usable, 64, 0), usable, "exactly satisfied");
        // Cannot grow in place at M1: the unchanged real size signals failure.
        let r = txallocx(p, usable + 1, 0, 0);
        assert_eq!(r, usable);
        assert!(r < usable + 1, "caller-visible failure: result < size");

        // A flag naming a nonexistent arena is a §10.4 invalid argument even for
        // this live, owned pointer — xallocx now validates it like its siblings
        // instead of silently reporting a usable size (PR #13 review). Arena 200
        // is never created in these tests.
        set_errno(0);
        assert_eq!(txallocx(p, 1, 0, topo_arena(200)), 0);
        assert_eq!(get_errno(), EINVAL, "xallocx rejects a nonexistent arena");
        tdallocx(p, 0);

        // Invalid flags / foreign pointer: 0 + EINVAL.
        set_errno(0);
        assert_eq!(txallocx(p, 1, 0, 1 << 60), 0);
        assert_eq!(get_errno(), EINVAL);
        let mut local = 0u8;
        set_errno(0);
        assert_eq!(txallocx((&mut local as *mut u8).cast(), 1, 0, 0), 0);
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn nallocx_predicts_mallocx_usable_size() {
        for (size, flags) in [
            (1usize, 0u64),
            (100, 0),
            (100, topo_align_lg(8)),
            (40_000, 0),
            (3 * 1024 * 1024, 0),
            // Over-page alignments: classify's span-rounding diverges from
            // the carved length here — the PR #11 P1 case.
            (1, topo_align_lg(16)),
            (100, topo_align_lg(18)),
            (200_000, topo_align_lg(16)),
        ] {
            let predicted = topo_nallocx(size, flags);
            assert!(predicted >= size.max(1));
            let p = topo_mallocx(size, flags);
            assert!(!p.is_null());
            assert_eq!(
                topomalloc_malloc_usable_size(p),
                predicted,
                "nallocx({size}, {flags:#x}) must equal mallocx's usable size"
            );
            tfree(p);
        }
        // Errors: 0, errno untouched.
        set_errno(0);
        assert_eq!(topo_nallocx(usize::MAX - 1, 0), 0);
        assert_eq!(topo_nallocx(64, u64::MAX), 0);
        assert_eq!(get_errno(), 0, "nallocx is pure");
    }

    #[test]
    fn sdallocx_routes_through_the_sized_checks() {
        let p = topo_mallocx(100, 0);
        tsdallocx(p, 100, 0); // matching hint: freed

        let q = topo_mallocx(512, topo_align_lg(8));
        tsdallocx(q, 512, topo_align_lg(8)); // aligned + sized: freed

        tsdallocx(ptr::null_mut(), 7, 0); // NULL: no-op
    }
}

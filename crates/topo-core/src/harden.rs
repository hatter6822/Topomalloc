// SPDX-License-Identifier: MIT
//! W18 — security & hardening primitives (§29, §17.3, §36.12, plan 08).
//!
//! This module is the home of TopoMalloc's opt-in hardening machinery. Each
//! protection is its **own Cargo feature** so the `performance` profile pays for
//! none of them and the `hardened`/`debug` profiles *compose* the ones they want
//! (overview principle 8; `profiles/README.md`):
//!
//! | Feature | W18 | SPEC | What it adds |
//! |---|---|---|---|
//! | `junk-fill` | W18-5 | §29.6 | fill-on-alloc / fill-on-free / verify-on-reuse |
//! | `quarantine` | W18-3 | §29.4 | delayed reuse of freed objects, accounted separately |
//! | `guard-pages` | W18-4 | §29.5 | sampled inaccessible pages around an allocation |
//! | `secure-scrub` | W18-6 | §36.12 | scrub dirty memory before cross-label reuse |
//!
//! **Safety before policy (§2.4).** None of these primitives can change an
//! allocation's size, alignment, validity, or free path — they only fill,
//! verify, hold, or zero memory the allocator already owns. A protection that is
//! compiled out is a *true no-op*: its entry point here takes the same arguments
//! the hot path already computed and lowers to nothing, so the `performance`
//! build is byte-for-byte the un-hardened one.
//!
//! ## Junk filling (W18-5, §29.6)
//!
//! Two patterns bracket an object's life:
//!
//! * [`ALLOC_PATTERN`] is written over a freshly handed-out object (when the
//!   caller did **not** ask for zeroing) so a read of uninitialised memory is
//!   conspicuous rather than a lucky zero.
//! * [`FREE_PATTERN`] is written over an object the moment it is freed, which
//!   both scrubs the stale contents and arms a **use-after-free canary**.
//!
//! The canary is *sound* because of TopoMalloc's metadata design: a free small
//! object stores **no** allocator metadata in its user bytes (the free list is an
//! out-of-line bitmap, §16.4 — the W18-1b structural win), so between
//! [`fill_on_free`] and the next allocation nothing but a buggy application can
//! touch those bytes. [`fill_fresh_slab`] establishes the invariant for a newly
//! carved slab, so on every reuse [`verify_free_pattern`] can assert the object
//! still reads as [`FREE_PATTERN`]; a mismatch is a write-after-free.

use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

/// Byte written over freshly-allocated user memory in junk-fill builds (§29.6),
/// so reading it before initialisation is obvious. Mirrors jemalloc's `0xa5`
/// intent with a distinct value.
pub const ALLOC_PATTERN: u8 = 0xAB;

/// Byte written over just-freed user memory in junk-fill builds (§29.6): it
/// scrubs the stale contents *and* is the use-after-free canary
/// [`verify_free_pattern`] checks on reuse.
pub const FREE_PATTERN: u8 = 0xDE;

/// Whether junk filling (W18-5, §29.6) is compiled in.
#[inline]
#[must_use]
pub const fn junk_fill_enabled() -> bool {
    cfg!(feature = "junk-fill")
}

/// Whether the delayed-reuse quarantine (W18-3, §29.4) is compiled in.
#[inline]
#[must_use]
pub const fn quarantine_enabled() -> bool {
    cfg!(feature = "quarantine")
}

/// Whether sampled guarded allocations (W18-4, §29.5) are compiled in.
#[inline]
#[must_use]
pub const fn guard_pages_enabled() -> bool {
    cfg!(feature = "guard-pages")
}

/// Whether scrub-before-downgrade (W18-6, §36.12) is compiled in.
#[inline]
#[must_use]
pub const fn secure_scrub_enabled() -> bool {
    cfg!(feature = "secure-scrub")
}

/// Fill a freshly handed-out object with [`ALLOC_PATTERN`] (§29.6, W18-5). The
/// caller invokes this **only when it is not already zeroing** the object (the two
/// are mutually exclusive — `TOPO_ZERO`/`calloc` win). A no-op unless `junk-fill`
/// is compiled in.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a writable region the allocator owns for this
/// freshly-allocated object (its usable size).
#[inline]
pub unsafe fn fill_on_alloc(ptr: *mut u8, len: usize) {
    #[cfg(feature = "junk-fill")]
    // SAFETY: forwarded from this function's contract — the caller guarantees
    // `[ptr, ptr + len)` is a writable, just-allocated region.
    unsafe {
        ptr::write_bytes(ptr, ALLOC_PATTERN, len);
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
    }
}

/// Fill a just-freed object with [`FREE_PATTERN`] (§29.6, W18-5): scrubs the stale
/// contents and arms the use-after-free canary. Called on the free path *before*
/// the object returns to the central free list / backend, while the freeing
/// thread still has the exact object pointer. A no-op unless `junk-fill` is in.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be the writable user region of the object being freed
/// (its usable size), and the caller MUST have established that the object is no
/// longer reachable by the application (it is being freed).
#[inline]
pub unsafe fn fill_on_free(ptr: *mut u8, len: usize) {
    #[cfg(feature = "junk-fill")]
    // SAFETY: forwarded from this function's contract — the caller guarantees
    // `[ptr, ptr + len)` is the writable user region of the object being freed.
    unsafe {
        ptr::write_bytes(ptr, FREE_PATTERN, len);
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
    }
}

/// Establish the [`FREE_PATTERN`] invariant over a **freshly carved slab** (§29.6,
/// W18-5): every object slot of a brand-new span is filled so that *every*
/// central-free object — fresh or recycled — reads as [`FREE_PATTERN`], which is
/// what makes [`verify_free_pattern`] sound on the first reuse of each slot. A
/// no-op unless `junk-fill` is in. (It is the same byte as [`fill_on_free`], named
/// distinctly because it covers the whole slab, including any inter-object
/// padding, at span creation.)
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be the writable backing of a slab this allocator just
/// carved and not yet handed any object out of.
#[inline]
pub unsafe fn fill_fresh_slab(ptr: *mut u8, len: usize) {
    // SAFETY: forwarded from this function's contract; `fill_on_free` writes the
    // same `FREE_PATTERN` over the (here, whole-slab) writable region.
    unsafe { fill_on_free(ptr, len) }
}

/// Verify that a central-free object still holds [`FREE_PATTERN`] (§29.6, W18-5) —
/// the **verify-on-reuse** check, run on the allocation path *before* the object
/// is overwritten (filled or zeroed). Returns `true` when the canary is intact (or
/// when `junk-fill` is compiled out, so a caller may `debug_assert!` the result
/// unconditionally); `false` means some byte differs — a write-after-free.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a readable region of `len` usable bytes for the
/// object about to be handed out.
#[inline]
#[must_use]
pub unsafe fn verify_free_pattern(ptr: *const u8, len: usize) -> bool {
    #[cfg(feature = "junk-fill")]
    {
        let mut i = 0;
        while i < len {
            // SAFETY: `i < len` and the caller guarantees `len` readable bytes.
            if unsafe { *ptr.add(i) } != FREE_PATTERN {
                return false;
            }
            i += 1;
        }
        true
    }
    #[cfg(not(feature = "junk-fill"))]
    {
        let _ = (ptr, len);
        true
    }
}

/// **Scrub** (zero) a range of memory and fence the write so the compiler cannot
/// sink or elide it (§26.4 "security zeroing"; §36.12 scrub-before-downgrade,
/// W18-6). Unlike the junk-fill helpers this is **always available** (not
/// feature-gated): scrubbing high-domain bytes before they are reused at a lower
/// label is an information-flow *MUST* (§36.12), not a debugging aid, so the
/// primitive must exist in every profile. The `secure-scrub` feature governs only
/// *how aggressively* the allocator chooses to invoke it (see
/// [`scrub_before_downgrade`]).
///
/// The trailing [`compiler_fence`] keeps the zeroing from being reordered after a
/// subsequent revoke/relabel, so a reader at the new label can never observe the
/// pre-scrub contents through a reordering.
///
/// # Safety
///
/// `[ptr, ptr + len)` MUST be a writable region the allocator owns and that no
/// other thread is concurrently reading at the *old* label (the caller — arena
/// teardown / extent recycle — guarantees quiescence, §22.5/§36.13).
#[inline]
pub unsafe fn scrub(ptr: *mut u8, len: usize) {
    // SAFETY: forwarded from this function's contract — a writable, quiesced region.
    unsafe { ptr::write_bytes(ptr, 0, len) };
    // Prevent the zeroing from being reordered past the caller's later
    // revoke/relabel (it is observable by the next, lower-label, reader).
    compiler_fence(Ordering::SeqCst);
}

/// Decide whether a range must be scrubbed before its backing is reused under a
/// **different** security label (§36.12, W18-6). The information-flow rule is:
/// dirty memory from one label MUST NOT be observable at a *different* label
/// without scrubbing. This returns `true` exactly when the labels differ — the
/// scrub is then mandatory regardless of profile (it is a §36.12 MUST), and the
/// `secure-scrub` feature additionally lets the `hardened` profile scrub on
/// *every* recycle as defence in depth (see the caller in `allocator.rs`).
///
/// Pinned to the Lean `scrub_before_downgrade` theorem
/// (`lean/TopoMalloc/SeLe4n/Refinement.lean`): a successful downgrade implies the
/// frame was scrubbed (its provider state advanced to `AllocatorMuzzyOrScrubbed`).
#[inline]
#[must_use]
pub fn must_scrub_for_relabel(old: crate::ids::Label, new: crate::ids::Label) -> bool {
    old != new
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_distinct_and_nonzero() {
        // The two junk patterns must differ (so a fill-on-free is distinguishable
        // from a fill-on-alloc) and be non-zero (so they are distinguishable from
        // a zeroed/`calloc` region).
        assert_ne!(ALLOC_PATTERN, FREE_PATTERN);
        assert_ne!(ALLOC_PATTERN, 0);
        assert_ne!(FREE_PATTERN, 0);
    }

    #[test]
    fn gating_reflects_features() {
        // Each predicate mirrors its feature exactly (the profile composition is
        // tested by the build matrix; here we pin the const-fn ↔ cfg coupling).
        assert_eq!(junk_fill_enabled(), cfg!(feature = "junk-fill"));
        assert_eq!(quarantine_enabled(), cfg!(feature = "quarantine"));
        assert_eq!(guard_pages_enabled(), cfg!(feature = "guard-pages"));
        assert_eq!(secure_scrub_enabled(), cfg!(feature = "secure-scrub"));
    }

    #[test]
    fn scrub_zeroes_the_whole_range() {
        let mut buf = [0xFFu8; 64];
        // SAFETY: `buf` is a live, exclusively-owned 64-byte region.
        unsafe { scrub(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn must_scrub_only_on_label_change() {
        use crate::ids::Label;
        assert!(!must_scrub_for_relabel(Label::PUBLIC, Label::PUBLIC));
        assert!(must_scrub_for_relabel(Label::PUBLIC, Label(7)));
        assert!(must_scrub_for_relabel(Label(7), Label::PUBLIC));
    }

    #[cfg(feature = "junk-fill")]
    #[test]
    fn junk_fill_lifecycle_round_trips() {
        let mut buf = [0u8; 48];
        // Fresh slab → every byte is FREE_PATTERN, so verify passes.
        // SAFETY: `buf` is a live 48-byte region.
        unsafe { fill_fresh_slab(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == FREE_PATTERN));
        // SAFETY: same region; verify reads it.
        assert!(unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
        // Hand out → fill with ALLOC_PATTERN.
        // SAFETY: same region.
        unsafe { fill_on_alloc(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == ALLOC_PATTERN));
        // Free → back to FREE_PATTERN, verify passes again.
        // SAFETY: same region.
        unsafe { fill_on_free(buf.as_mut_ptr(), buf.len()) };
        // SAFETY: same region; verify reads it.
        assert!(unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
    }

    #[cfg(feature = "junk-fill")]
    #[test]
    fn verify_detects_a_write_after_free() {
        let mut buf = [FREE_PATTERN; 32];
        buf[17] = 0x00; // a stray write-after-free
                        // SAFETY: `buf` is a live 32-byte region.
        assert!(!unsafe { verify_free_pattern(buf.as_ptr(), buf.len()) });
    }
}

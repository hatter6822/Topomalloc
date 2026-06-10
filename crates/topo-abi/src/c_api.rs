// SPDX-License-Identifier: MIT
//! The standard C entry points (§10.1, plan 06 W8-1/2/3/4).
//!
//! Every function here is a prefixed `topomalloc_*` symbol over the M1
//! central-path allocator, with:
//!
//! * **errno semantics (W8-1b):** allocation failure sets `ENOMEM`; argument
//!   validation failure sets `EINVAL`; success restores the caller's
//!   `errno`; the `free` family never modifies `errno` ([`errno_shim`]).
//! * **zero-size policy (W8-4):** `malloc(0)`-style requests follow the
//!   configurable [`policy`] (`unique` by default, `null` opt-in);
//!   `free(NULL)` is always a no-op; `realloc(p != NULL, 0)` always frees
//!   and returns `NULL` (the dominant-platform C17 behavior).
//! * **fail-safe invalid frees (§35.2):** foreign/interior/metadata/released
//!   pointers (and double frees) are detected by classification and ignored
//!   without touching any state. Loud rejection policies (abort, quarantine)
//!   are the hardened/debug profiles' job (plan 08 W18).
//! * **sized-free cross-checks (W8-3):** the C23 `free_sized` /
//!   `free_aligned_sized` size (and alignment) hints are *verified* against
//!   the classifier in debug/hardened builds — a mismatch aborts loudly —
//!   and ignored for the actual free in performance builds, so a wrong hint
//!   can never corrupt state. Plan 08 W18-2 turns the full check into
//!   production sampling.
//!
//! [`errno_shim`]: crate::errno_shim
//! [`policy`]: crate::policy

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use topo_core::generated::tables::PAGE_SIZE;
use topo_core::overflow::{align_up, array_bytes};
use topo_core::{classify, RequestFlags, RequestKind, MIN_ALIGN};

use crate::errno_shim::{alloc_protocol, preserving_errno, set_errno, EINVAL, ENOMEM};
use crate::policy::{zero_size_policy, ZeroSizePolicy};
use crate::{global, AnyAllocator};

/// Whether the sized-free cross-checks run: always in debug builds, and in
/// hardened builds of the core (`debug-checks`). Performance builds trust the
/// hint — but never *act* on it, so a wrong hint is harmless there.
#[inline]
pub(crate) fn sized_checks_enabled() -> bool {
    cfg!(debug_assertions) || topo_core::debug_checks_enabled()
}

/// Resolve the global allocator or fail the allocation path (null + ENOMEM
/// is applied by the caller's [`alloc_protocol`] wrapper).
#[inline]
fn engine() -> Option<&'static AnyAllocator> {
    global()
}

/// The zero-size gate (§9.6/W8-4): `Some(NULL)` when the policy says a
/// zero-size request returns `NULL` (with `errno` untouched — not a failure);
/// `None` when the request should proceed (non-zero size, or `unique` policy).
#[inline]
fn zero_size_gate(size: usize) -> Option<*mut c_void> {
    if size == 0 && zero_size_policy() == ZeroSizePolicy::Null {
        Some(ptr::null_mut())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Core (W8-1a): malloc / free / calloc / realloc
// ---------------------------------------------------------------------------

/// `void* topomalloc_malloc(size_t size)` (§10.1).
///
/// Null + `errno = ENOMEM` on out-of-memory or size overflow (§9.7).
/// `malloc(0)` follows the zero-size policy (§9.6).
#[no_mangle]
pub extern "C" fn topomalloc_malloc(size: usize) -> *mut c_void {
    if let Some(p) = zero_size_gate(size) {
        return p;
    }
    alloc_protocol(|| {
        engine().map_or(ptr::null_mut(), |a| {
            a.allocate(size, MIN_ALIGN, RequestFlags::NONE)
        })
    })
    .cast::<c_void>()
}

/// `void topomalloc_free(void* ptr)` (§10.1). `free(NULL)` is a no-op (§9.6);
/// `free` never modifies `errno` (POSIX); invalid pointers are detected and
/// ignored without state change (§35.2).
///
/// # Safety
///
/// `ptr` must be null, a live `topomalloc_*` allocation the caller still
/// owns, or memory never returned by this allocator (detected and ignored).
/// A *stale* pointer aliasing a recycled live allocation is the classic
/// double free — undefined behavior, like `free` in C.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_free(ptr: *mut c_void) {
    preserving_errno(|| {
        if let Some(a) = engine() {
            // Every non-Freed outcome is a safe no-op at this boundary; the
            // hardened profile escalates instead (plan 08 W18).
            // SAFETY: this entry point's contract is the engine's contract.
            let _ = unsafe { a.free(ptr.cast::<u8>()) };
        }
    });
}

/// `void* topomalloc_calloc(size_t n, size_t size)` (§26). Overflow-checked in
/// both §26.1 clauses: (1) the `n * size` product via [`array_bytes`]; (2) the
/// subsequent size-class/page rounding inside the classifier (§9.7), which
/// fails null rather than wrapping. The result is zeroed through the whole
/// usable size (§26.2).
#[no_mangle]
pub extern "C" fn topomalloc_calloc(n: usize, size: usize) -> *mut c_void {
    // §26.1 clause 1: reject an overflowing product before allocating.
    let Some(total) = array_bytes(n, size) else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    if let Some(p) = zero_size_gate(total) {
        return p;
    }
    alloc_protocol(|| {
        engine().map_or(ptr::null_mut(), |a| {
            // §26.1 clause 2 happens inside classify (checked rounding); the
            // ZERO hint zeroes the entire usable size (§26.2).
            a.allocate(total, MIN_ALIGN, RequestFlags::NONE.with_zero())
        })
    })
    .cast::<c_void>()
}

/// `void* topomalloc_realloc(void* ptr, size_t size)` (§25.1):
///
/// * `realloc(NULL, size)` ≡ `malloc(size)`;
/// * `realloc(ptr, 0)` frees `ptr` and returns `NULL` with `errno` untouched
///   (the dominant-platform behavior; see [`crate::zero_size_policy`]);
/// * on success the result holds `min(old_usable, size)` bytes of the old
///   content; **on failure the original allocation remains valid** and
///   `errno` is `ENOMEM` (§10.1);
/// * an invalid `ptr` (foreign/interior/already freed) returns `NULL` with
///   `errno = EINVAL` and no state change (§35.2).
///
/// # Safety
///
/// As [`topomalloc_free`]: `ptr` must be null, an owned live allocation, or
/// never-owned memory (rejected with `EINVAL`).
#[no_mangle]
pub unsafe extern "C" fn topomalloc_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        return topomalloc_malloc(size);
    }
    if size == 0 {
        // Free-and-null, errno untouched (not a failure).
        // SAFETY: this entry point's contract covers the free of `ptr`.
        unsafe { topomalloc_free(ptr) };
        return ptr::null_mut();
    }
    let Some(a) = engine() else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    // Distinguish "invalid pointer" (EINVAL, nothing touched) from "genuine
    // allocation failure" (ENOMEM, original still valid).
    if !a.owns(ptr.cast::<u8>()) {
        set_errno(EINVAL);
        return ptr::null_mut();
    }
    // SAFETY: `ptr` is owned-and-live per this entry point's contract (and
    // verified `owns` above for the never-owned case).
    alloc_protocol(|| unsafe { a.realloc(ptr.cast::<u8>(), size, MIN_ALIGN, RequestFlags::NONE) })
        .cast::<c_void>()
}

/// `void* topomalloc_reallocarray(void* ptr, size_t n, size_t size)`
/// (BSD/glibc, §10.1): `realloc(ptr, n * size)` with the multiplication
/// overflow-checked (§26.1). On overflow: `NULL`, `errno = ENOMEM`, and the
/// original allocation is untouched.
///
/// # Safety
///
/// As [`topomalloc_realloc`].
#[no_mangle]
pub unsafe extern "C" fn topomalloc_reallocarray(
    ptr: *mut c_void,
    n: usize,
    size: usize,
) -> *mut c_void {
    let Some(total) = array_bytes(n, size) else {
        set_errno(ENOMEM);
        return ptr::null_mut();
    };
    // SAFETY: identical contract, forwarded.
    unsafe { topomalloc_realloc(ptr, total) }
}

// ---------------------------------------------------------------------------
// Aligned / POSIX (W8-2)
// ---------------------------------------------------------------------------

/// `void* topomalloc_aligned_alloc(size_t alignment, size_t size)` (§25.5).
///
/// `alignment` must be a power of two and `size` an integer multiple of it
/// (the C17 rule, kept per SPEC §25.5); otherwise `NULL` + `errno = EINVAL`.
/// Alignment is never silently ignored (§10.4).
#[no_mangle]
pub extern "C" fn topomalloc_aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    // §25.5: validate the power-of-two alignment *and* the size-multiple
    // constraint. `alignment` is a power of two here (hence nonzero), so
    // `is_multiple_of` is well-defined.
    if !alignment.is_power_of_two() || !size.is_multiple_of(alignment) {
        set_errno(EINVAL);
        return ptr::null_mut();
    }
    if let Some(p) = zero_size_gate(size) {
        return p;
    }
    alloc_protocol(|| {
        engine().map_or(ptr::null_mut(), |a| {
            a.allocate(size, alignment.max(MIN_ALIGN), RequestFlags::NONE)
        })
    })
    .cast::<c_void>()
}

/// `int topomalloc_posix_memalign(void** memptr, size_t alignment, size_t
/// size)` (POSIX): `alignment` must be a power of two and a multiple of
/// `sizeof(void*)`. Returns `0` on success, `EINVAL`/`ENOMEM` on failure —
/// `errno` is never modified (the return value carries the error), and
/// `*memptr` is only written on success. A null `memptr` is rejected with
/// `EINVAL` (defensive; POSIX leaves it undefined).
/// # Safety
///
/// `memptr` must be null (rejected with `EINVAL`) or a valid pointer to
/// writable `void*` storage; it is written only on success.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_posix_memalign(
    memptr: *mut *mut c_void,
    alignment: usize,
    size: usize,
) -> c_int {
    preserving_errno(|| {
        if memptr.is_null() {
            return EINVAL;
        }
        if !alignment.is_power_of_two()
            || !alignment.is_multiple_of(core::mem::size_of::<*mut c_void>())
        {
            return EINVAL;
        }
        if size == 0 && zero_size_policy() == ZeroSizePolicy::Null {
            // POSIX: size 0 ⇒ either NULL or a unique pointer, returning 0.
            // SAFETY: `memptr` is non-null and the caller hands a writable slot.
            unsafe { *memptr = ptr::null_mut() };
            return 0;
        }
        let p = engine().map_or(ptr::null_mut(), |a| {
            a.allocate(size, alignment.max(MIN_ALIGN), RequestFlags::NONE)
        });
        if p.is_null() {
            return ENOMEM;
        }
        // SAFETY: `memptr` is non-null and the caller hands a writable slot.
        unsafe { *memptr = p.cast::<c_void>() };
        0
    })
}

/// `void* topomalloc_memalign(size_t alignment, size_t size)` (obsolete, kept
/// for compatibility, §10.1): power-of-two alignment, no size-multiple
/// constraint. `NULL` + `EINVAL` on a bad alignment.
#[no_mangle]
pub extern "C" fn topomalloc_memalign(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() {
        set_errno(EINVAL);
        return ptr::null_mut();
    }
    if let Some(p) = zero_size_gate(size) {
        return p;
    }
    alloc_protocol(|| {
        engine().map_or(ptr::null_mut(), |a| {
            a.allocate(size, alignment.max(MIN_ALIGN), RequestFlags::NONE)
        })
    })
    .cast::<c_void>()
}

/// `size_t topomalloc_malloc_usable_size(void* ptr)` (§10.1): the number of
/// usable bytes in the allocation at `ptr` (≥ the requested size). `0` for
/// `NULL` and — defensively — for any pointer this allocator does not own
/// (glibc leaves that undefined). Never modifies `errno`.
#[no_mangle]
pub extern "C" fn topomalloc_malloc_usable_size(ptr: *mut c_void) -> usize {
    preserving_errno(|| {
        engine()
            .and_then(|a| a.usable_size(ptr.cast::<u8>()))
            .unwrap_or(0)
    })
}

// ---------------------------------------------------------------------------
// C23 sized free (W8-3)
// ---------------------------------------------------------------------------

/// Whether `size` is a plausible original request size for the allocation at
/// `ptr` (the C23 `free_sized` contract): it must classify to the same
/// storage the allocation actually has — exactly for small (same-class
/// in-place only), by fit for medium/large (in-place shrinks keep the
/// extent). Used only as a cross-check — the free itself never trusts the
/// hint.
fn sized_hint_matches(a: &AnyAllocator, ptr: *mut u8, size: usize, align: usize) -> bool {
    let Some(usable) = a.usable_size(ptr) else {
        // Not a live allocation of ours: the plain free path will reject it;
        // the size hint is not the bug to report here.
        return true;
    };
    match classify(size, align, 0).map(|r| r.kind) {
        Some(RequestKind::Small {
            usable: class_usable,
            ..
        }) => class_usable == usable,
        Some(RequestKind::Medium { .. }) | Some(RequestKind::Large { .. }) => {
            // Fit, not equality: an in-place shrink (`Allocator::realloc`)
            // keeps the original extent, so the truthful (last-requested)
            // size can round to *less* than the usable size. A false accept
            // merely weakens a heuristic; a false reject would abort a
            // correct program (found by the W8 self-audit; pinned by
            // `free_sized_accepts_truthful_size_after_inplace_shrink`).
            align_up(size.max(1), PAGE_SIZE).is_some_and(|rounded| rounded <= usable)
        }
        // A size so large it cannot classify can never have been allocated.
        None => false,
    }
}

/// Shared body of the C23 sized frees: cross-check the hint in debug/hardened
/// builds (W8-3 / plan 08 W18-2), then perform the normal classified free —
/// the hint is **never** used to select the free path at M1, so a wrong hint
/// cannot corrupt state in any profile (it is UB the checker catches).
unsafe fn free_sized_common(ptr: *mut c_void, align: usize, size: usize) {
    preserving_errno(|| {
        if ptr.is_null() {
            return; // C23: free_sized(NULL, n) is a no-op like free(NULL)
        }
        let Some(a) = engine() else { return };
        if sized_checks_enabled() {
            assert!(
                sized_hint_matches(a, ptr.cast::<u8>(), size, align),
                "free_sized: size {size} (align {align}) does not match the \
                 allocation at {ptr:p} — heap corruption or a caller bug (C23 UB)",
            );
            if align > 1 {
                assert!(
                    (ptr as usize).is_multiple_of(align),
                    "free_aligned_sized: pointer {ptr:p} is not {align}-aligned",
                );
            }
        }
        // SAFETY: the sized-free entry points carry the free contract.
        let _ = unsafe { a.free(ptr.cast::<u8>()) };
    });
}

/// `void topomalloc_free_sized(void* ptr, size_t size)` (C23, §10.1): `size`
/// must equal the size the allocation was made with (after any realloc); a
/// mismatch is UB that debug/hardened builds abort on (W8-3). Never modifies
/// `errno`; `free_sized(NULL, n)` is a no-op.
/// # Safety
///
/// As [`topomalloc_free`]; additionally, a `size` that does not match the
/// allocation is C23 undefined behavior (debug/hardened builds abort on it;
/// performance builds ignore the hint, so it cannot corrupt state).
#[no_mangle]
pub unsafe extern "C" fn topomalloc_free_sized(ptr: *mut c_void, size: usize) {
    // SAFETY: identical contract, forwarded.
    unsafe { free_sized_common(ptr, 1, size) };
}

/// `void topomalloc_free_aligned_sized(void* ptr, size_t alignment, size_t
/// size)` (C23, §10.1): the aligned-allocation counterpart of
/// [`topomalloc_free_sized`]; `alignment` and `size` must match the original
/// `aligned_alloc`-family request.
/// # Safety
///
/// As [`topomalloc_free_sized`], with `alignment` part of the checked hint.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_free_aligned_sized(
    ptr: *mut c_void,
    alignment: usize,
    size: usize,
) {
    // A non-power-of-two alignment can never have been allocated; flag it in
    // debug, ignore the hint otherwise.
    if sized_checks_enabled() && !ptr.is_null() {
        assert!(
            alignment.is_power_of_two(),
            "free_aligned_sized: alignment {alignment} is not a power of two",
        );
    }
    let align = if alignment.is_power_of_two() {
        alignment
    } else {
        1
    };
    // SAFETY: identical contract, forwarded.
    unsafe { free_sized_common(ptr, align, size) };
}

// ---------------------------------------------------------------------------
// Identification
// ---------------------------------------------------------------------------

/// `const char* topomalloc_version(void)` — the NUL-terminated version string.
#[no_mangle]
pub extern "C" fn topomalloc_version() -> *const c_char {
    // Build a `&CStr` at compile time from the crate version plus a NUL, so the
    // returned pointer is a valid C string for the process lifetime.
    const V: &core::ffi::CStr = match core::ffi::CStr::from_bytes_with_nul(
        concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes(),
    ) {
        Ok(c) => c,
        Err(_) => panic!("version string contains an interior NUL"),
    };
    V.as_ptr()
}

/// `const char* topomalloc_backend(void)` — the active backend name (W0-14b).
#[no_mangle]
pub extern "C" fn topomalloc_backend() -> *const c_char {
    match global().map_or("posix", |a| a.backend_name()) {
        "sele4n-sim" => c"sele4n-sim".as_ptr(),
        _ => c"posix".as_ptr(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errno_shim::get_errno;

    /// Test wrappers: every pointer the tests free/realloc here is either one
    /// they just received and still own, null, or a local probe that was
    /// never returned by the allocator — exactly the entry points' safety
    /// contracts.
    fn tfree(p: *mut c_void) {
        // SAFETY: test-owned (or never-owned, rejected) pointer; see above.
        unsafe { topomalloc_free(p) }
    }
    fn trealloc(p: *mut c_void, n: usize) -> *mut c_void {
        // SAFETY: as `tfree`.
        unsafe { topomalloc_realloc(p, n) }
    }

    #[test]
    fn c_abi_malloc_is_aligned_writable_and_freed_for_reuse() {
        let p = topomalloc_malloc(40);
        assert!(!p.is_null());
        assert_eq!(p as usize % MIN_ALIGN, 0);
        // SAFETY: p points to at least 40 usable bytes.
        unsafe {
            ptr::write_bytes(p.cast::<u8>(), 0x5a, 40);
            assert_eq!(p.cast::<u8>().read(), 0x5a);
        }
        tfree(p);
        // The engine recycles: same class ⇒ the hole is vended again.
        let q = topomalloc_malloc(40);
        assert_eq!(q, p, "free must actually recycle (no more M0 leak)");
        tfree(q);
    }

    #[test]
    fn malloc_failure_sets_enomem_and_success_preserves_errno() {
        set_errno(0);
        // A size that cannot be served (rounding overflow, §9.7).
        assert!(topomalloc_malloc(usize::MAX - 7).is_null());
        assert_eq!(get_errno(), ENOMEM, "failed malloc must set ENOMEM");

        set_errno(41);
        let p = topomalloc_malloc(64);
        assert!(!p.is_null());
        assert_eq!(get_errno(), 41, "successful malloc must preserve errno");
        tfree(p);
    }

    #[test]
    fn free_preserves_errno_even_for_invalid_pointers() {
        set_errno(17);
        tfree(ptr::null_mut());
        assert_eq!(get_errno(), 17);

        // A live pointer: the free path may issue backend syscalls; errno
        // must still be preserved (W8-1b).
        let p = topomalloc_malloc(100);
        set_errno(23);
        tfree(p);
        assert_eq!(get_errno(), 23);

        // An obviously foreign pointer is ignored, errno untouched.
        let mut local = 0u64;
        set_errno(29);
        tfree((&mut local as *mut u64).cast::<c_void>());
        assert_eq!(get_errno(), 29);
        assert_eq!(local, 0, "foreign memory untouched");
    }

    #[test]
    fn calloc_zeroes_whole_usable_size_and_checks_overflow() {
        let p = topomalloc_calloc(4, 16);
        assert!(!p.is_null());
        let usable = topomalloc_malloc_usable_size(p);
        assert!(usable >= 64);
        // SAFETY: `usable` zeroed bytes are available (§26.2).
        unsafe {
            for i in 0..usable {
                assert_eq!(p.cast::<u8>().add(i).read(), 0);
            }
        }
        tfree(p);

        // Overflowing product must return null + ENOMEM, never wrap (§26.1).
        set_errno(0);
        assert!(topomalloc_calloc(usize::MAX, 2).is_null());
        assert_eq!(get_errno(), ENOMEM);
        // Product fits but the page rounding would overflow (§26.1 clause 2).
        set_errno(0);
        assert!(topomalloc_calloc(1, usize::MAX - 100).is_null());
        assert_eq!(get_errno(), ENOMEM);
    }

    #[test]
    fn realloc_contract_at_the_c_boundary() {
        // realloc(NULL, n) == malloc(n).
        let p = trealloc(ptr::null_mut(), 100);
        assert!(!p.is_null());
        // SAFETY: 100 usable bytes.
        unsafe {
            for i in 0..100u8 {
                p.cast::<u8>().add(i as usize).write(i);
            }
        }

        // Growth preserves content.
        let q = trealloc(p, 8192);
        assert!(!q.is_null());
        // SAFETY: 8192 usable bytes; prefix preserved.
        unsafe {
            for i in 0..100u8 {
                assert_eq!(q.cast::<u8>().add(i as usize).read(), i);
            }
        }

        // Failure preserves the original and sets ENOMEM.
        set_errno(0);
        let huge = trealloc(q, usize::MAX - 15);
        assert!(huge.is_null());
        assert_eq!(get_errno(), ENOMEM);
        // SAFETY: q is still live with its content (§25.1).
        unsafe { assert_eq!(q.cast::<u8>().add(99).read(), 99) };

        // realloc(p, 0) frees and returns NULL with errno untouched.
        set_errno(5);
        assert!(trealloc(q, 0).is_null());
        assert_eq!(get_errno(), 5);

        // realloc of an invalid pointer: EINVAL, nothing touched.
        let mut local = 0u32;
        set_errno(0);
        assert!(trealloc((&mut local as *mut u32).cast::<c_void>(), 64).is_null());
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn reallocarray_checks_overflow_and_preserves_original() {
        let p = topomalloc_malloc(32);
        assert!(!p.is_null());
        // SAFETY: 32 usable bytes.
        unsafe { p.cast::<u8>().write(0xCD) };
        set_errno(0);
        // SAFETY: `p` is a live test-owned allocation; overflow rejects it untouched.
        assert!(unsafe { topomalloc_reallocarray(p, usize::MAX / 2, 4) }.is_null());
        assert_eq!(get_errno(), ENOMEM);
        // SAFETY: p is still live, untouched by the failed reallocarray.
        unsafe { assert_eq!(p.cast::<u8>().read(), 0xCD) };
        // SAFETY: `p` is live and test-owned.
        let q = unsafe { topomalloc_reallocarray(p, 16, 8) };
        assert!(!q.is_null());
        tfree(q);
    }

    #[test]
    fn aligned_alloc_validates_and_sets_einval() {
        set_errno(0);
        assert!(topomalloc_aligned_alloc(24, 64).is_null()); // not a power of two
        assert_eq!(get_errno(), EINVAL);
        set_errno(0);
        assert!(topomalloc_aligned_alloc(256, 100).is_null()); // size not a multiple (§25.5)
        assert_eq!(get_errno(), EINVAL);

        let p = topomalloc_aligned_alloc(4096, 8192);
        assert!(!p.is_null());
        assert_eq!(p as usize % 4096, 0);
        tfree(p);
    }

    #[test]
    fn posix_memalign_returns_codes_and_never_touches_errno() {
        let mut out: *mut c_void = ptr::null_mut();
        set_errno(77);
        // SAFETY: `out` is a valid writable slot for every call below.
        unsafe {
            assert_eq!(topomalloc_posix_memalign(&mut out, 3, 64), EINVAL);
            assert_eq!(
                topomalloc_posix_memalign(&mut out, core::mem::size_of::<*mut c_void>() / 2, 64),
                EINVAL,
                "alignment must be a multiple of sizeof(void*)"
            );
            assert!(out.is_null(), "memptr untouched on failure");
            assert_eq!(topomalloc_posix_memalign(ptr::null_mut(), 64, 64), EINVAL);

            assert_eq!(topomalloc_posix_memalign(&mut out, 64, 200), 0);
        }
        assert!(!out.is_null());
        assert_eq!(out as usize % 64, 0);
        assert_eq!(get_errno(), 77, "posix_memalign never modifies errno");
        tfree(out);
    }

    #[test]
    fn memalign_honors_alignment_without_size_multiple() {
        let p = topomalloc_memalign(512, 100); // 100 is not a multiple of 512
        assert!(!p.is_null());
        assert_eq!(p as usize % 512, 0);
        tfree(p);
        set_errno(0);
        assert!(topomalloc_memalign(7, 100).is_null());
        assert_eq!(get_errno(), EINVAL);
    }

    #[test]
    fn malloc_usable_size_reports_class_or_zero() {
        assert_eq!(topomalloc_malloc_usable_size(ptr::null_mut()), 0);
        let p = topomalloc_malloc(100);
        let usable = topomalloc_malloc_usable_size(p);
        assert!(usable >= 100);
        // Writing the whole usable size is legal.
        // SAFETY: `usable` writable bytes per the contract just asserted.
        unsafe { ptr::write_bytes(p.cast::<u8>(), 1, usable) };
        tfree(p);

        let mut local = 0u8;
        assert_eq!(
            topomalloc_malloc_usable_size((&mut local as *mut u8).cast::<c_void>()),
            0,
            "foreign pointers defensively report 0"
        );
    }

    #[test]
    fn free_sized_accepts_matching_hints() {
        // SAFETY: every pointer below is test-owned and freed exactly once;
        // NULL is the documented no-op.
        unsafe {
            let p = topomalloc_malloc(100);
            topomalloc_free_sized(p, 100); // exact original size: accepted

            let q = topomalloc_aligned_alloc(256, 512);
            topomalloc_free_aligned_sized(q, 256, 512);

            // After realloc, the *new* size is the valid hint (C23).
            let r = topomalloc_malloc(50);
            let r = topomalloc_realloc(r, 3000);
            topomalloc_free_sized(r, 3000);

            // NULL is a no-op regardless of size.
            topomalloc_free_sized(ptr::null_mut(), 12345);
        }
    }

    /// Regression (W8 self-audit): after an in-place medium shrink the
    /// truthful (last-requested) size rounds to *less* than the usable size;
    /// the debug cross-check must accept it, not abort a correct program.
    #[test]
    fn free_sized_accepts_truthful_size_after_inplace_shrink() {
        // SAFETY: pointers are test-owned; each is freed exactly once with
        // its truthful last-requested size.
        unsafe {
            let p = topomalloc_malloc(80_000); // 5 pages: usable 81920
            assert_eq!(topomalloc_malloc_usable_size(p), 81_920);
            let q = topomalloc_realloc(p, 40_000); // rounds to 49152 <= 81920
            assert_eq!(q, p, "page-rounded shrink stays in place");
            assert_eq!(topomalloc_malloc_usable_size(q), 81_920);
            topomalloc_free_sized(q, 40_000); // C23: the one valid hint
        }
    }

    /// The mismatch check itself. The test targets the inner (Rust-ABI)
    /// `free_sized_common` rather than the `extern "C"` symbol: a panic
    /// crossing an `extern "C"` boundary aborts the process (which is the
    /// intended production behavior for a corrupted-heap signal), but
    /// `#[should_panic]` needs a catchable unwind.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "does not match the")]
    fn free_sized_mismatch_aborts_in_debug() {
        let p = topomalloc_malloc(100);
        // 100 classifies to the 112-byte class; 4000 is a different class:
        // a C23 UB the debug build catches loudly (W8-3).
        // SAFETY: `p` is live and test-owned; the mismatched size hint is
        // exactly the checked condition under test (the free never happens —
        // the assert fires first).
        unsafe { free_sized_common(p, 1, 4000) };
    }

    #[test]
    fn zero_size_default_unique_yields_distinct_freeable_pointers() {
        // Default policy (zero_unique, §9.6) only — flipping the process-wide
        // policy races sibling tests, so the full unique/null matrix runs in
        // its own process: tests/tests/zero_size_policy.rs (W8-4).
        let a = topomalloc_malloc(0);
        let b = topomalloc_malloc(0);
        assert!(!a.is_null() && !b.is_null());
        assert_ne!(a, b, "zero_unique pointers must be distinct (§9.6)");
        tfree(a);
        tfree(b);
        let c = topomalloc_calloc(0, 8);
        assert!(!c.is_null());
        tfree(c);
        let mut out: *mut c_void = ptr::null_mut();
        // SAFETY: `out` is a valid writable slot.
        unsafe { assert_eq!(topomalloc_posix_memalign(&mut out, 64, 0), 0) };
        assert!(!out.is_null());
        tfree(out);
    }

    #[test]
    fn version_and_backend_strings() {
        // SAFETY: both pointers are valid static NUL-terminated strings.
        unsafe {
            let v = core::ffi::CStr::from_ptr(topomalloc_version());
            assert_eq!(v.to_str().unwrap(), topo_core::VERSION);
            let b = core::ffi::CStr::from_ptr(topomalloc_backend());
            assert_eq!(b.to_str().unwrap(), "posix");
        }
    }
}

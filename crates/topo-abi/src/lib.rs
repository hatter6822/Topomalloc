// SPDX-License-Identifier: MIT
//! Public ABI surface: the C entry points (`topomalloc_*`), the Rust
//! [`GlobalAlloc`] adapter, and the runtime backend selector (W0-14a/b).
//!
//! **Default build = MIT, POSIX-only.** Enabling the optional `sele4n-sim`
//! feature additionally links the GPL seLe4n backend and produces a
//! GPL-3.0-or-later artifact (`libtopomalloc-sele4n`, §36.3.2). The default
//! artifact never links GPL code (D5, see NOTICE).
//!
//! The exported C symbols are **prefixed** (`topomalloc_malloc`, …), so linking
//! this crate never hijacks the process's `malloc`. The interposition/override
//! deployment that replaces system `malloc` (§35.1) is a deliberate, separately
//! gated step in plan 10 — not something a test or dependent crate gets by
//! accident.

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_void};
use core::ptr;
use std::sync::OnceLock;

use topo_backend_posix::PosixBackingProvider;
use topo_core::overflow::array_bytes;
use topo_core::{SkeletonAllocator, MIN_ALIGN};

/// Backing size of the M0 skeleton heap. The skeleton leaks on free, so this is
/// the total a process can hand out before OOM — ample for tests and demos.
const SKELETON_HEAP_BYTES: usize = 16 * 1024 * 1024;

/// A skeleton allocator over whichever backend was selected at runtime. Enum
/// dispatch (not `dyn`) keeps the default build free of the GPL backend type.
pub enum AnyAllocator {
    /// POSIX backend (default).
    Posix(SkeletonAllocator<PosixBackingProvider>),
    /// seLe4n host simulator (only when built with `sele4n-sim`).
    #[cfg(feature = "sele4n-sim")]
    Sim(SkeletonAllocator<topo_backend_sele4n::Sele4nSim>),
}

impl AnyAllocator {
    /// Allocate `size` bytes with `align`. Null on OOM/overflow.
    pub fn malloc(&self, size: usize, align: usize) -> *mut u8 {
        match self {
            AnyAllocator::Posix(a) => a.malloc(size, align),
            #[cfg(feature = "sele4n-sim")]
            AnyAllocator::Sim(a) => a.malloc(size, align),
        }
    }

    /// Free a pointer (M0: leaks; `free(NULL)` is a no-op).
    pub fn free(&self, ptr: *mut u8) {
        match self {
            AnyAllocator::Posix(a) => a.free(ptr),
            #[cfg(feature = "sele4n-sim")]
            AnyAllocator::Sim(a) => a.free(ptr),
        }
    }

    /// The active backend's name (`"posix"` / `"sele4n-sim"`), proving which
    /// provider the runtime flag selected (W0-14b).
    pub fn backend_name(&self) -> &'static str {
        match self {
            AnyAllocator::Posix(a) => a.backend_name(),
            #[cfg(feature = "sele4n-sim")]
            AnyAllocator::Sim(a) => a.backend_name(),
        }
    }
}

/// Build an allocator for the named backend (the runtime flag, W0-14b).
///
/// Returns `None` for an unknown name, or if a backend with that name is not
/// compiled into this build (e.g. `"sele4n-sim"` without the feature).
pub fn new_allocator_named(name: &str, heap_bytes: usize) -> Option<AnyAllocator> {
    match name {
        "posix" => {
            let a = SkeletonAllocator::new(PosixBackingProvider::new(), heap_bytes).ok()?;
            Some(AnyAllocator::Posix(a))
        }
        #[cfg(feature = "sele4n-sim")]
        "sele4n-sim" => {
            let sim = topo_backend_sele4n::Sele4nSim::new(heap_bytes);
            let a = SkeletonAllocator::new(sim, heap_bytes).ok()?;
            Some(AnyAllocator::Sim(a))
        }
        _ => None,
    }
}

/// The process-wide skeleton allocator, initialized once from the environment.
/// `None` records that initialization failed (e.g. the host could not reserve
/// the skeleton heap); the C ABI then reports OOM as a null result instead of
/// aborting the process across the `extern "C"` boundary.
static GLOBAL: OnceLock<Option<AnyAllocator>> = OnceLock::new();

/// The selected backend name: `$TOPOMALLOC_BACKEND` or `"posix"`. An unknown or
/// unavailable name falls back to POSIX so the default artifact is always usable.
fn selected_backend_name() -> String {
    std::env::var("TOPOMALLOC_BACKEND").unwrap_or_else(|_| "posix".into())
}

/// The lazily-initialized global allocator, or `None` if the skeleton heap could
/// not be reserved. The result is memoized: a process that cannot reserve the
/// heap once keeps reporting OOM (a null `malloc`) rather than retrying.
fn global() -> Option<&'static AnyAllocator> {
    GLOBAL
        .get_or_init(|| {
            let name = selected_backend_name();
            new_allocator_named(&name, SKELETON_HEAP_BYTES)
                .or_else(|| new_allocator_named("posix", SKELETON_HEAP_BYTES))
        })
        .as_ref()
}

// ---------------------------------------------------------------------------
// C ABI (§10.1). Prefixed symbols only; overriding system malloc is plan 10.
// ---------------------------------------------------------------------------

/// `void* topomalloc_malloc(size_t size)`.
#[no_mangle]
pub extern "C" fn topomalloc_malloc(size: usize) -> *mut c_void {
    global().map_or(ptr::null_mut(), |a| {
        a.malloc(size, MIN_ALIGN).cast::<c_void>()
    })
}

/// `void topomalloc_free(void* ptr)`. `free(NULL)` is a no-op (§9.6).
#[no_mangle]
pub extern "C" fn topomalloc_free(ptr: *mut c_void) {
    if !ptr.is_null() {
        if let Some(a) = global() {
            a.free(ptr.cast::<u8>());
        }
    }
}

/// `void* topomalloc_calloc(size_t n, size_t size)`. Overflow-checked (§26.1):
/// an overflowing `n * size` returns null rather than wrapping.
#[no_mangle]
pub extern "C" fn topomalloc_calloc(n: usize, size: usize) -> *mut c_void {
    let Some(total) = array_bytes(n, size) else {
        return ptr::null_mut();
    };
    let Some(a) = global() else {
        return ptr::null_mut();
    };
    let p = a.malloc(total, MIN_ALIGN);
    if !p.is_null() && total > 0 {
        // SAFETY: `malloc` returned a non-null pointer to at least `total`
        // committed bytes; zeroing the whole region is in bounds.
        unsafe { ptr::write_bytes(p, 0, total) };
    }
    p.cast::<c_void>()
}

/// `void* topomalloc_aligned_alloc(size_t alignment, size_t size)` (§25.5).
/// `alignment` must be a power of two; otherwise null.
#[no_mangle]
pub extern "C" fn topomalloc_aligned_alloc(alignment: usize, size: usize) -> *mut c_void {
    if !alignment.is_power_of_two() {
        return ptr::null_mut();
    }
    global().map_or(ptr::null_mut(), |a| {
        a.malloc(size, alignment).cast::<c_void>()
    })
}

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

/// The Rust [`GlobalAlloc`] adapter (D1). Provided for opt-in use as
/// `#[global_allocator]`; it is intentionally **not** registered here so it
/// never replaces a test or dependent crate's allocator.
pub struct TopoMallocGlobal;

// SAFETY: `alloc` returns either null or a pointer to at least `layout.size()`
// bytes aligned to `layout.align()` (the skeleton honors both); `dealloc`
// accepts any pointer `alloc` returned (it leaks in M0, which is sound — leaking
// is always memory-safe). These satisfy the `GlobalAlloc` contract.
unsafe impl GlobalAlloc for TopoMallocGlobal {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        global().map_or(ptr::null_mut(), |a| a.malloc(layout.size(), layout.align()))
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if let Some(a) = global() {
            a.free(ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_abi_malloc_is_aligned_and_writable() {
        let p = topomalloc_malloc(40);
        assert!(!p.is_null());
        assert_eq!(p as usize % MIN_ALIGN, 0);
        // SAFETY: p points to at least 40 usable bytes.
        unsafe {
            ptr::write_bytes(p.cast::<u8>(), 0x5a, 40);
            assert_eq!(p.cast::<u8>().read(), 0x5a);
        }
        topomalloc_free(p);
    }

    #[test]
    fn calloc_zeroes_and_checks_overflow() {
        let p = topomalloc_calloc(4, 16);
        assert!(!p.is_null());
        // SAFETY: 64 zeroed bytes are available.
        unsafe {
            for i in 0..64 {
                assert_eq!(p.cast::<u8>().add(i).read(), 0);
            }
        }
        topomalloc_free(p);
        // Overflowing request must return null, not wrap (§26.1).
        assert!(topomalloc_calloc(usize::MAX, 2).is_null());
    }

    #[test]
    fn aligned_alloc_honors_alignment() {
        let p = topomalloc_aligned_alloc(4096, 100);
        assert!(!p.is_null());
        assert_eq!(p as usize % 4096, 0);
        topomalloc_free(p);
        assert!(topomalloc_aligned_alloc(3, 100).is_null()); // not power of two
    }

    #[test]
    fn version_string_is_nul_terminated() {
        let p = topomalloc_version();
        // SAFETY: the version pointer is a valid static NUL-terminated string.
        let s = unsafe { core::ffi::CStr::from_ptr(p) };
        assert_eq!(s.to_str().unwrap(), topo_core::VERSION);
    }

    #[test]
    fn global_alloc_adapter_roundtrips() {
        let g = TopoMallocGlobal;
        let layout = Layout::from_size_align(128, 16).unwrap();
        // SAFETY: valid non-zero layout.
        let p = unsafe { g.alloc(layout) };
        assert!(!p.is_null());
        // SAFETY: p has 128 bytes; dealloc accepts it (leaks in M0).
        unsafe {
            ptr::write_bytes(p, 1, 128);
            g.dealloc(p, layout);
        }
    }

    #[test]
    fn named_selector_builds_posix() {
        let a = new_allocator_named("posix", 4096).expect("posix");
        assert_eq!(a.backend_name(), "posix");
        assert!(new_allocator_named("nope", 4096).is_none());
    }
}

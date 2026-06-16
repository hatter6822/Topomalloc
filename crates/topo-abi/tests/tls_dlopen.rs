// SPDX-License-Identifier: MIT
//! TLS bootstrap / re-entrancy under `dlopen` (W16-2, plan 05; §27.6, DD-4).
//!
//! Top risk **R3**: the allocator's own thread-local state must be reachable on a
//! thread's first allocation *without that access itself allocating* — otherwise
//! the allocator re-enters itself before its per-thread state exists →
//! recursion/deadlock. The danger is acute under `dlopen`, where the
//! initial-exec TLS model may be unavailable and `__thread` slots can fall back
//! to general-dynamic TLS (a lazy allocation on first access).
//!
//! This test loads the freshly-built `libtopo_abi` **via `dlopen`** (a fresh,
//! `RTLD_LOCAL` instance with its own statics and TLS) and drives its
//! `topomalloc_malloc`/`free` from **fresh threads**, exercising the per-thread
//! first-TLS-access path. If that path re-entered the allocator unboundedly it
//! would deadlock or stack-overflow; a watchdog aborts the process if the work
//! does not finish, so a regression fails loudly instead of hanging
//! (acceptance: "load via dlopen does not re-enter the allocator").
//!
//! The cdylib is produced by `cargo build`; CI builds the workspace before
//! testing. Run locally with `cargo build -p topo-abi` first (or `cargo xtask
//! ci`). If the artifact is absent the test fails with a clear hint rather than
//! silently passing.
#![cfg(all(unix, not(miri)))]

use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

type MallocFn = unsafe extern "C" fn(usize) -> *mut c_void;
type FreeFn = unsafe extern "C" fn(*mut c_void);

/// Locate the freshly-built `libtopo_abi.{so,dylib}` next to the test binary.
fn cdylib_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/<test-bin>  →  the profile dir is two parents up.
    let deps = exe.parent()?; // .../deps
    let profile = deps.parent()?; // .../<profile>
    let name = if cfg!(target_os = "macos") {
        "libtopo_abi.dylib"
    } else {
        "libtopo_abi.so"
    };
    for dir in [profile, deps] {
        let cand = dir.join(name);
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

/// A `dlopen` handle that `dlclose`s on drop.
struct Lib(*mut c_void);
// SAFETY: the handle is a plain library handle; we never hand out interior
// pointers that outlive it, and the symbols are used only while it is alive.
unsafe impl Send for Lib {}
// SAFETY: as `Send` — the handle is only used to `dlclose` on drop and is shared
// read-only (`Arc`) across worker threads that merely keep the library alive.
unsafe impl Sync for Lib {}
impl Drop for Lib {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `dlopen`.
            unsafe { libc::dlclose(self.0) };
        }
    }
}

fn dlsym(handle: *mut c_void, name: &str) -> *mut c_void {
    let c = CString::new(name).unwrap();
    // SAFETY: `handle` is a live dlopen handle; `c` is a valid C string.
    unsafe { libc::dlsym(handle, c.as_ptr()) }
}

#[test]
fn dlopen_first_alloc_on_fresh_threads_does_not_reenter() {
    let Some(path) = cdylib_path() else {
        panic!(
            "libtopo_abi cdylib not found next to the test binary — build it first \
             (`cargo build -p topo-abi`, or run via `cargo xtask ci`)."
        );
    };
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    // RTLD_NOW | RTLD_LOCAL: resolve all symbols now, and keep the instance
    // isolated (its own statics/TLS), so this is a genuine fresh load — the
    // dlopen deployment W16-2 targets.
    // SAFETY: `c_path` is a valid path to our just-built cdylib.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    assert!(!handle.is_null(), "dlopen({}) failed", path.display());
    let lib = Arc::new(Lib(handle));

    let malloc_sym = dlsym(handle, "topomalloc_malloc");
    let free_sym = dlsym(handle, "topomalloc_free");
    assert!(
        !malloc_sym.is_null() && !free_sym.is_null(),
        "dlsym of topomalloc_malloc/free failed"
    );
    // SAFETY: the symbol resolved to `topomalloc_malloc`, which has this signature.
    let malloc: MallocFn = unsafe { std::mem::transmute(malloc_sym) };
    // SAFETY: the symbol resolved to `topomalloc_free`, which has this signature.
    let free: FreeFn = unsafe { std::mem::transmute(free_sym) };

    // Watchdog: a TLS re-entrancy deadlock would hang a worker's first call. If
    // the whole exercise is not done within the deadline, abort with a message so
    // the test fails loudly instead of wedging the run.
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let done = done.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                if done.load(Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            eprintln!(
                "tls_dlopen watchdog: fresh-thread first allocation did not complete in 10s — \
                 the dlopen'd allocator likely re-entered itself on first TLS access (W16-2 regression)"
            );
            std::process::abort();
        })
    };

    // Each fresh thread's FIRST call into the dlopen'd allocator exercises the
    // per-thread first-TLS-access path. Several threads, each round-tripping a few
    // allocations, with the very first allocation per thread being the critical one.
    let mut handles = Vec::new();
    for t in 0..8 {
        let lib = lib.clone();
        handles.push(std::thread::spawn(move || {
            // Keep the library alive for the thread's lifetime.
            let _keep = &lib;
            for i in 0..64usize {
                let size = 24 + ((t * 7 + i) % 1024);
                // SAFETY: `malloc` is the dlopen'd `topomalloc_malloc`.
                let p = unsafe { malloc(size) };
                assert!(
                    !p.is_null(),
                    "first/early allocation returned null on thread {t}"
                );
                // SAFETY: `p` has `size` writable bytes; write the whole block.
                unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0x5a, size) };
                // SAFETY: `p` was returned by the same dlopen'd allocator.
                unsafe { free(p) };
            }
        }));
    }
    for h in handles {
        h.join()
            .expect("a dlopen worker thread panicked (re-entry/deadlock?)");
    }
    done.store(true, Ordering::Release);
    watchdog.join().unwrap();
}

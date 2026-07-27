// SPDX-License-Identifier: MIT
//! Startup-configuration regression tests (§32.1): every `TOPOMALLOC_*` environment
//! variable the initializer honours must leave the process **usable**.
//!
//! The initializer runs inside `GLOBAL.get_or_init`, where the `OnceLock` has not
//! published yet. A hook that called `global()` there re-entered the still-running
//! `Once` and parked the thread on its own initialization — so merely exporting
//! `TOPOMALLOC_QUARANTINE=1` (a documented arming mechanism) hung the process at its
//! very first `malloc`, before `main` did any work. Nothing in the tree set these
//! variables, so no test noticed.
//!
//! Each case re-execs *this* test binary with one variable set and a marker that selects
//! the child body, then asserts the child exits promptly and successfully. A regression
//! hangs the child; the parent's bounded wait turns that into a failure rather than a
//! stalled CI job.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Selects the child body. Absent ⇒ this process is the parent driver.
const MARKER: &str = "TOPOMALLOC_ENV_STARTUP_CHILD";

/// How long a child gets to allocate. Initialization is milliseconds; a hang is
/// unbounded, so any generous bound separates them.
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Exercise the allocator enough to force the lazy global initialization and a few
/// real allocations through every size regime.
fn child_body() {
    let mut live = Vec::new();
    for size in [1usize, 64, 4096, 100_000, 3_000_000] {
        let p = topo_abi::topomalloc_malloc(size);
        assert!(!p.is_null(), "allocation of {size} failed");
        // SAFETY: `p` has at least `size` writable bytes.
        unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0x5A, size) };
        live.push(p);
    }
    for p in live {
        // SAFETY: each pointer came from `topomalloc_malloc` above and is freed once.
        unsafe { topo_abi::topomalloc_free(p) };
    }
}

/// Run this binary again with `var=value` set and the child marker, and wait — bounded
/// — for it to finish. Returns the child's success flag.
fn run_child_with(var: &str, value: &str) -> bool {
    let exe = std::env::current_exe().expect("current exe");
    let mut child = Command::new(exe)
        .env(MARKER, "1")
        .env(var, value)
        // Run only the child-body test, single-threaded, so the child's own harness
        // does not re-enter this driver.
        .args(["--test-threads=1", "child_allocates_under_env_config"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    let deadline = Instant::now() + CHILD_TIMEOUT;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => return status.success(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child hung with {var}={value}: the startup hook deadlocked");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The child body, selected by the marker. In the parent it is a no-op, so the same
/// binary serves both roles.
#[test]
fn child_allocates_under_env_config() {
    if std::env::var(MARKER).is_err() {
        return; // parent role: driven by the test below
    }
    child_body();
}

/// Every startup-honoured environment variable must leave the allocator usable.
#[test]
fn every_startup_env_var_leaves_the_allocator_usable() {
    if std::env::var(MARKER).is_ok() {
        return; // we are the child; the body above is the work
    }
    // The full documented set (§32.1). `SAMPLE_RATE=0` is included because the
    // "disable" arm took a different path from the "enable" arm.
    for (var, value) in [
        ("TOPOMALLOC_QUARANTINE", "1"),
        ("TOPOMALLOC_QUARANTINE", "4194304"),
        ("TOPOMALLOC_GUARD_SAMPLE_RATE", "64"),
        ("TOPOMALLOC_DETERMINISTIC_SEED", "7"),
        ("TOPOMALLOC_SAMPLE_RATE", "0"),
        ("TOPOMALLOC_SAMPLE_RATE", "4096"),
        ("TOPOMALLOC_ZERO_SIZE", "null"),
        ("TOPOMALLOC_BACKEND", "posix"),
    ] {
        assert!(
            run_child_with(var, value),
            "the child failed with {var}={value}"
        );
    }
}

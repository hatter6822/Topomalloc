// SPDX-License-Identifier: MIT
//! Fork-in-multithread safety (W16-5, plan 05; §28.1, DD-5).
//!
//! The hard case `pthread_atfork` exists for: in a multithreaded process,
//! `fork()` yields a child with one thread but with allocator locks possibly held
//! by threads that no longer exist. The child must **not** deadlock on the
//! allocator's own spinlocks. TopoMalloc installs atfork handlers (W16-5) that
//! quiesce the allocator pre-fork (so no lock is held at `fork()`) and reset the
//! gate in the child.
//!
//! These tests are unix-only (`fork` is POSIX) and use a watchdog: a child that
//! deadlocked on an inherited lock would hang, so the parent kills it after a
//! deadline and fails rather than wedging the whole test run.
#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use topo_abi::{topomalloc_free, topomalloc_malloc};

/// Allocate, touch, and free `n` blocks through the C path. Returns `false` on the
/// first null (OOM) so a caller can turn it into a non-zero exit code.
fn hammer(n: usize) -> bool {
    for i in 0..n {
        let size = 16 + (i % 4096);
        let p = topomalloc_malloc(size);
        if p.is_null() {
            return false;
        }
        // SAFETY: `p` has `size` writable bytes; write one byte to fault the page.
        unsafe {
            *p.cast::<u8>() = 0xa5;
            topomalloc_free(p);
        }
    }
    true
}

/// `fork()` in a loop while worker threads hammer the allocator; the child
/// allocates and `_exit(0)`s. A child that inherited a held allocator lock would
/// deadlock — caught by the watchdog (W16-5b acceptance: "child allocates safely;
/// no inherited held lock").
#[test]
fn child_allocates_after_fork_under_concurrent_load() {
    // Bring the allocator (and its atfork handlers) up *before* forking, so a fork
    // never races first-time initialization.
    assert!(hammer(8), "allocator must be usable before the test");

    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // Ignore OOM here; the point is to keep allocator locks churning
                    // so a fork has a live chance of racing a held lock.
                    let _ = hammer(32);
                }
            })
        })
        .collect();

    // Several fork rounds, each racing the busy workers.
    for round in 0..40 {
        // SAFETY: `fork` is called from this (the main) thread; the child only runs
        // async-signal-safe-ish work (our quiesced allocator) and then `_exit`s.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed in round {round}");
        if pid == 0 {
            // -- child -- one thread; the atfork child handler has reset the gate.
            // Allocate through TopoMalloc: if the child inherited a held lock this
            // would deadlock (the watchdog in the parent then kills us).
            let ok = hammer(200);
            // Use `_exit` (not `exit`) so no atexit/destructor runs in the child.
            // SAFETY: `_exit` terminates the child immediately; always sound to call.
            unsafe { libc::_exit(if ok { 0 } else { 2 }) };
        }
        // -- parent -- wait for the child with a watchdog so a deadlocked child
        // cannot wedge the test run.
        wait_with_watchdog(pid, Duration::from_secs(10), round);
    }

    stop.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().unwrap();
    }
}

/// A single fork with a worker thread deliberately parked *inside* an allocator
/// operation is covered by the gate-drain semantics; this complements the busy
/// loop above with a quiescent fork (the common, simplest case) to pin the basic
/// contract.
#[test]
fn child_allocates_after_quiescent_fork() {
    assert!(hammer(8));
    // SAFETY: forking from the main thread; the child quickly `_exit`s.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        let ok = hammer(500);
        // SAFETY: `_exit` terminates the child immediately; always sound to call.
        unsafe { libc::_exit(if ok { 0 } else { 2 }) };
    }
    wait_with_watchdog(pid, Duration::from_secs(10), 0);
}

/// The **parent** must be unaffected by a fork (W16-5a): a long-lived allocation
/// it holds *survives* every fork intact (still mapped, still the right size, still
/// freeable), and the parent stays fully usable. (We check survival via
/// `usable_size`, not the process-global `live_bytes`, since libtest runs tests in
/// parallel and other tests perturb that shared counter — DD-5 "stats consistent"
/// from the parent's side, made robust to concurrency.)
#[test]
fn parent_state_is_consistent_across_fork() {
    assert!(hammer(64));
    let probe = topo_abi::topomalloc_malloc(4096);
    assert!(!probe.is_null());
    // SAFETY: `probe` has 4096 writable bytes; mark the whole block.
    unsafe { std::ptr::write_bytes(probe.cast::<u8>(), 0xc3, 4096) };
    let usable_before = topo_abi::topomalloc_malloc_usable_size(probe);
    assert!(usable_before >= 4096);
    // The crash summary (W16-6) is callable and well-formed while the process runs.
    assert!(
        summary_live_bytes() > 0,
        "a live probe ⇒ nonzero live_bytes"
    );

    for _ in 0..20 {
        // SAFETY: fork from the main thread; the child immediately `_exit`s.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Child: prove it can allocate, then leave without disturbing the parent.
            let ok = hammer(50);
            // SAFETY: terminate the child immediately.
            unsafe { libc::_exit(if ok { 0 } else { 2 }) };
        }
        wait_with_watchdog(pid, Duration::from_secs(10), 0);
        // Parent: the long-lived probe is unchanged across the fork, and the parent
        // is still fully usable (allocate + free a fresh block).
        assert_eq!(
            topo_abi::topomalloc_malloc_usable_size(probe),
            usable_before,
            "parent's long-lived allocation must survive the fork unchanged"
        );
        // SAFETY: `probe`'s bytes are still mapped; the pattern must persist.
        unsafe {
            assert_eq!(*probe.cast::<u8>(), 0xc3);
            assert_eq!(*probe.cast::<u8>().add(4095), 0xc3);
        }
        let p = topomalloc_malloc(1234);
        assert!(!p.is_null(), "parent allocation failed after fork");
        // SAFETY: `p` has 1234 writable bytes; freed immediately.
        unsafe {
            *p.cast::<u8>() = 1;
            topomalloc_free(p);
        }
    }

    // The probe is still a valid live allocation and frees cleanly.
    // SAFETY: `probe` is still the live allocation from before the loop.
    unsafe { topomalloc_free(probe) };
}

/// Two threads call `fork()` concurrently while workers hammer the allocator. The
/// `FORK_LOCK` serializes the prepare→child/parent handlers, so neither child
/// deadlocks (W16-5: concurrent forkers).
#[test]
fn concurrent_forks_from_two_threads() {
    assert!(hammer(8));
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = hammer(16);
                }
            })
        })
        .collect();

    let forker = || {
        std::thread::spawn(|| {
            for round in 0..15 {
                // SAFETY: fork from this thread; the child immediately `_exit`s.
                let pid = unsafe { libc::fork() };
                assert!(pid >= 0, "fork failed");
                if pid == 0 {
                    let ok = hammer(64);
                    // SAFETY: terminate the child immediately.
                    unsafe { libc::_exit(if ok { 0 } else { 2 }) };
                }
                wait_with_watchdog(pid, Duration::from_secs(10), round);
            }
        })
    };
    let a = forker();
    let b = forker();
    a.join().unwrap();
    b.join().unwrap();

    stop.store(true, Ordering::Relaxed);
    for w in workers {
        w.join().unwrap();
    }
}

/// The lock-free crash summary's `live_bytes=` field (read from the C entry point,
/// no allocation), used by the parent-consistency check.
fn summary_live_bytes() -> u64 {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable region of its length.
    let n = unsafe {
        topo_abi::topomalloc_crash_summary(buf.as_mut_ptr().cast::<core::ffi::c_char>(), buf.len())
    };
    let text = core::str::from_utf8(&buf[..n]).unwrap();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("live_bytes=") {
            return v.parse().unwrap();
        }
    }
    panic!("crash summary had no live_bytes field: {text:?}");
}

/// Wait for `pid`, failing if it does not exit 0 within `timeout` (a deadlocked
/// child is `SIGKILL`ed and reported, so the test fails fast instead of hanging).
fn wait_with_watchdog(pid: libc::pid_t, timeout: Duration, round: i32) {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out-pointer; `WNOHANG` makes this non-blocking.
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            // Child reaped. It must have exited normally with code 0.
            let exited = libc::WIFEXITED(status);
            let code = libc::WEXITSTATUS(status);
            assert!(
                exited && code == 0,
                "child (round {round}) exited abnormally: WIFEXITED={exited} code={code} \
                 (non-zero ⇒ allocation failed; a hang ⇒ inherited a held lock)"
            );
            return;
        }
        assert!(r >= 0, "waitpid failed for child {pid} (round {round})");
        if Instant::now() >= deadline {
            // The child is wedged — almost certainly deadlocked on an inherited
            // allocator lock. Kill it and fail loudly.
            // SAFETY: `pid` is our child; SIGKILL is always safe to send.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let mut s = 0;
            // SAFETY: reap the killed child so it does not linger as a zombie.
            unsafe { libc::waitpid(pid, &mut s, 0) };
            panic!(
                "child (round {round}) did not exit within {timeout:?} — \
                 likely deadlocked on an inherited allocator lock (W16-5 regression)"
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

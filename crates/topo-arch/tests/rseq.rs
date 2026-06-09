// SPDX-License-Identifier: MIT
//! Integration tests for the RSEQ fast path (W7-2/W7-3/W7-6) against a synthetic
//! per-CPU layout that mirrors what `topo-core`'s `CpuCache` supplies: a
//! `#[repr(C)]` per-CPU record (a lock byte then per-size-class slots), each slot
//! holding `{ len, cap, buf }`. The tests exercise the *production* assembly via
//! the public [`topo_arch::rseq`] API.
//!
//! These run on the `-gnu` CI targets. On x86-64 RSEQ is active (glibc registers
//! it), so pop/push and the forced-migration conservation test exercise the real
//! kernel sequence. Where RSEQ is unavailable (some sandboxes, qemu-user), the
//! API reports it and the tests assert the clean fallback instead of skipping.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use topo_arch::rseq::{self, Pop, Push, Rseq};

const NSC: usize = 4;
const MAX_CPUS: usize = 128;
const CAP: usize = 256;

#[repr(C)]
struct Slot {
    len: AtomicU32,   // offset 0
    cap: AtomicU32,   // offset 4
    buf: AtomicUsize, // offset 8
}
impl Slot {
    const fn new() -> Self {
        Self {
            len: AtomicU32::new(0),
            cap: AtomicU32::new(0),
            buf: AtomicUsize::new(0),
        }
    }
}

#[repr(C)]
struct PerCpu {
    locked: AtomicU8, // offset 0
    slots: [Slot; NSC],
}
impl PerCpu {
    fn new() -> Self {
        Self {
            locked: AtomicU8::new(0),
            slots: [const { Slot::new() }; NSC],
        }
    }
}

const LEN_OFF: usize = core::mem::offset_of!(Slot, len);
const CAP_OFF: usize = core::mem::offset_of!(Slot, cap);
const BUF_OFF: usize = core::mem::offset_of!(Slot, buf);

/// The synthetic cache: a contiguous per-CPU array plus the backing buffers.
struct Cache {
    cpus: Box<[PerCpu]>,
    // One buffer per (cpu, sc); kept alive for the cache's lifetime.
    _bufs: Vec<Box<[UnsafeCell<usize>]>>,
}
// SAFETY: all shared state is atomic or only mutated through the rseq sequence /
// raw-pointer accessors below; the buffers live as long as the cache.
unsafe impl Sync for Cache {}
// SAFETY: as `Sync` above — the cache owns its buffers and shares only atomics.
unsafe impl Send for Cache {}

impl Cache {
    fn new() -> Self {
        let mut cpus: Vec<PerCpu> = (0..MAX_CPUS).map(|_| PerCpu::new()).collect();
        let mut bufs = Vec::new();
        for cpu in cpus.iter_mut() {
            for slot in cpu.slots.iter_mut() {
                let buf: Box<[UnsafeCell<usize>]> = (0..CAP).map(|_| UnsafeCell::new(0)).collect();
                slot.buf.store(buf.as_ptr() as usize, Ordering::Release);
                slot.cap.store(CAP as u32, Ordering::Release);
                slot.len.store(0, Ordering::Release);
                bufs.push(buf);
            }
        }
        Self {
            cpus: cpus.into_boxed_slice(),
            _bufs: bufs,
        }
    }

    fn base(&self) -> *const u8 {
        self.cpus.as_ptr().cast::<u8>()
    }
    fn stride(&self) -> usize {
        core::mem::size_of::<PerCpu>()
    }
    fn slot_base(&self, sc: usize) -> *const u8 {
        // &cpus[0] + offset_of(slots) + sc * size_of::<Slot>()
        (self.base() as usize
            + core::mem::offset_of!(PerCpu, slots)
            + sc * core::mem::size_of::<Slot>()) as *const u8
    }

    // Safe wrappers: the `Cache` invariant (a contiguous, fully-initialised
    // per-CPU layout sized to `MAX_CPUS`, buffers of `CAP` entries) makes the
    // layout arguments to the sequence valid; the caller registered the thread.
    fn pop(&self, sc: usize) -> Pop {
        let area = rseq::current_area();
        // SAFETY: `area` is this thread's area and the layout arguments describe
        // this live synthetic cache (see the `Cache` invariant above).
        unsafe {
            rseq::pop::<LEN_OFF, BUF_OFF>(
                area,
                self.slot_base(sc),
                self.base(),
                self.stride(),
                MAX_CPUS,
            )
        }
    }
    fn push(&self, sc: usize, v: usize) -> Push {
        let area = rseq::current_area();
        // SAFETY: as `pop`, plus the soft-capacity field and a `CAP`-entry buffer.
        unsafe {
            rseq::push::<LEN_OFF, BUF_OFF, CAP_OFF>(
                area,
                self.slot_base(sc),
                self.base(),
                self.stride(),
                MAX_CPUS,
                v,
            )
        }
    }

    /// Run the pop sequence against an explicit `area` (whose `cpu_id` the caller
    /// controls), exercising the per-arch asm even where the kernel does not
    /// provide RSEQ (e.g. qemu-user). Single-threaded use only.
    fn pop_with(&self, sc: usize, area: *mut Rseq) -> Pop {
        // SAFETY: `area` is a valid (if unregistered) area and the layout
        // arguments describe this live cache; no concurrent access.
        unsafe {
            rseq::pop::<LEN_OFF, BUF_OFF>(
                area,
                self.slot_base(sc),
                self.base(),
                self.stride(),
                MAX_CPUS,
            )
        }
    }
    fn push_with(&self, sc: usize, v: usize, area: *mut Rseq) -> Push {
        // SAFETY: as `pop_with`, plus the soft-capacity field and `CAP` buffer.
        unsafe {
            rseq::push::<LEN_OFF, BUF_OFF, CAP_OFF>(
                area,
                self.slot_base(sc),
                self.base(),
                self.stride(),
                MAX_CPUS,
                v,
            )
        }
    }

    /// Direct (non-rseq) read of a slot's length, for assertions.
    fn slot_len(&self, cpu: usize, sc: usize) -> u32 {
        self.cpus[cpu].slots[sc].len.load(Ordering::Acquire)
    }
    /// Seed `tokens` into `(cpu, sc)`'s buffer and set its length.
    fn seed(&self, cpu: usize, sc: usize, tokens: &[usize]) {
        let slot = &self.cpus[cpu].slots[sc];
        let buf = slot.buf.load(Ordering::Acquire) as *mut usize;
        for (i, &t) in tokens.iter().enumerate() {
            // SAFETY: i < tokens.len() <= CAP; single-threaded seeding.
            unsafe { *buf.add(i) = t };
        }
        slot.len.store(tokens.len() as u32, Ordering::Release);
    }
    /// Collect every token currently held across all slots of `sc`.
    fn collect(&self, sc: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for cpu in 0..MAX_CPUS {
            let slot = &self.cpus[cpu].slots[sc];
            let buf = slot.buf.load(Ordering::Acquire) as *const usize;
            let len = slot.len.load(Ordering::Acquire) as usize;
            for i in 0..len {
                // SAFETY: i < len <= CAP.
                out.push(unsafe { *buf.add(i) });
            }
        }
        out
    }
}

/// Pin the calling thread to `cpu`. Returns whether it succeeded.
fn pin_to(cpu: usize) -> bool {
    #[repr(C)]
    struct CpuSet {
        bits: [u64; 16],
    }
    extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const CpuSet) -> i32;
    }
    let mut set = CpuSet { bits: [0; 16] };
    set.bits[cpu / 64] |= 1u64 << (cpu % 64);
    // SAFETY: `set` is a valid cpu_set_t of the given size.
    unsafe { sched_setaffinity(0, core::mem::size_of::<CpuSet>(), &set) == 0 }
}

fn ncpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_CPUS)
}

#[test]
fn asm_sequence_logic_runs_with_explicit_area() {
    // Validate the per-architecture sequence *logic* (the cpu/slot addressing,
    // the bounds branches, and the single committing store) by driving an
    // explicit, unregistered area with `cpu_id = 0` against CPU 0's slot. This
    // runs the real assembly even where the kernel does not provide RSEQ (e.g.
    // qemu-user for AArch64), so the AArch64 encoding/logic is exercised in CI;
    // the *restart* property additionally needs a registered area + real
    // preemption (covered natively on x86-64 and on AArch64 hardware).
    let mut area = Rseq::zeroed();
    area.cpu_id = 0;
    let area_ptr = &mut area as *mut Rseq;
    let cache = Cache::new();
    let sc = 0;

    assert_eq!(cache.pop_with(sc, area_ptr), Pop::Empty);
    for i in 0..5 {
        assert_eq!(cache.push_with(sc, 0xB000 + i, area_ptr), Push::Success);
    }
    assert_eq!(cache.slot_len(0, sc), 5);
    for i in (0..5).rev() {
        assert_eq!(cache.pop_with(sc, area_ptr), Pop::Success(0xB000 + i));
    }
    assert_eq!(cache.pop_with(sc, area_ptr), Pop::Empty);

    // Fill to capacity, then Full; and the lock byte still diverts to Fallback.
    for i in 0..CAP {
        assert_eq!(cache.push_with(sc, i, area_ptr), Push::Success);
    }
    assert_eq!(cache.push_with(sc, 0, area_ptr), Push::Full);
    cache.cpus[0].locked.store(1, Ordering::Release);
    assert_eq!(cache.pop_with(sc, area_ptr), Pop::Fallback);
    cache.cpus[0].locked.store(0, Ordering::Release);
}

/// W7-1 (the self-registration model): with glibc's auto-registration disabled
/// (`GLIBC_TUNABLES=glibc.pthread.rseq=0`), RSEQ — if the kernel supports it at
/// all — must come through *our own* `rseq(2)` registration, not glibc's area.
/// We validate it by re-exec'ing this very test with the tunable set (gated to
/// x86-64 native, where re-exec is reliable; the self-reg logic is arch-neutral).
#[cfg(target_arch = "x86_64")]
#[test]
fn self_registration_path_works() {
    const MARKER: &str = "TOPO_RSEQ_SELFREG_CHILD";
    if std::env::var(MARKER).is_ok() {
        // Child: glibc rseq is disabled, so a present RSEQ must be self-registered.
        let on = rseq::enable();
        assert_ne!(
            rseq::mode(),
            rseq::Mode::GlibcArea,
            "glibc rseq should be disabled"
        );
        if !on {
            return; // kernel lacks rseq (sandbox): clean fallback, nothing to prove
        }
        assert_eq!(rseq::mode(), rseq::Mode::SelfRegistered);
        assert!(rseq::register_current_thread());
        let cpu = rseq::current_cpu();
        assert!(cpu >= 0 && (cpu as usize) < MAX_CPUS);
        // A round-trip through the self-registered area exercises the real asm.
        if pin_to(cpu as usize) {
            let cache = Cache::new();
            let sc = 0;
            assert_eq!(cache.pop(sc), Pop::Empty);
            assert_eq!(cache.push(sc, 0x5E1F), Push::Success);
            assert_eq!(cache.pop(sc), Pop::Success(0x5E1F));
        }
        return;
    }
    // Parent: re-run this test in a fresh process with glibc rseq disabled.
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .args(["--exact", "self_registration_path_works"])
        .env("GLIBC_TUNABLES", "glibc.pthread.rseq=0")
        .env(MARKER, "1")
        .status()
        .expect("spawn self-registration child");
    assert!(status.success(), "self-registration child failed");
}

#[test]
fn out_of_range_cpu_diverts_to_fallback() {
    // Memory-safety guard: a kernel-reported CPU id >= MAX_CPUS must NOT index
    // past the per-CPU array; the sequence bounds-checks it and returns Fallback.
    // Driven via an explicit area so it runs on every target (incl. qemu).
    let cache = Cache::new(); // exactly MAX_CPUS entries
    let sc = 0;
    let mut area = Rseq::zeroed();
    // cpu_id one past the last valid index — cpus[MAX_CPUS] is out of bounds.
    area.cpu_id = MAX_CPUS as u32;
    let ap = &mut area as *mut Rseq;
    assert_eq!(cache.pop_with(sc, ap), Pop::Fallback);
    assert_eq!(cache.push_with(sc, 1, ap), Push::Fallback);
    // A far-out-of-range id is also safely diverted.
    area.cpu_id = u32::MAX;
    let ap = &mut area as *mut Rseq;
    assert_eq!(cache.pop_with(sc, ap), Pop::Fallback);
    assert_eq!(cache.push_with(sc, 1, ap), Push::Fallback);
    // No slot was touched.
    assert_eq!(cache.slot_len(0, sc), 0);
}

#[test]
fn registration_and_cpu_reads_are_sane() {
    let enabled = rseq::enable();
    if !enabled {
        // Clean fallback (P-003): no area, cpu reads as -1.
        assert!(!rseq::available());
        assert_eq!(rseq::current_cpu(), -1);
        assert!(rseq::current_area().is_null());
        return;
    }
    assert!(rseq::available());
    assert!(rseq::register_current_thread());
    let cpu = rseq::current_cpu();
    assert!(cpu >= 0, "registered thread must report a valid CPU");
    assert!((cpu as usize) < MAX_CPUS);
    assert!(!rseq::current_area().is_null());
}

#[test]
fn single_threaded_pop_push_lifo() {
    if !rseq::enable() {
        return; // covered by registration_and_cpu_reads_are_sane
    }
    assert!(rseq::register_current_thread());
    // Pin so the current CPU is stable across the sequence.
    let cpu = rseq::current_cpu();
    if cpu < 0 || !pin_to(cpu as usize) {
        return;
    }
    let cpu = rseq::current_cpu();
    assert!(cpu >= 0);
    let cpu = cpu as usize;
    let cache = Cache::new();
    let sc = 1;

    // Empty pop.
    assert_eq!(cache.pop(sc), Pop::Empty);

    // Push then pop LIFO.
    for i in 0..10 {
        assert_eq!(cache.push(sc, 0xA000 + i), Push::Success, "push {i}");
    }
    assert_eq!(cache.slot_len(cpu, sc), 10);
    for i in (0..10).rev() {
        match cache.pop(sc) {
            Pop::Success(v) => assert_eq!(v, 0xA000 + i, "LIFO order"),
            other => panic!("expected Success, got {other:?}"),
        }
    }
    assert_eq!(cache.pop(sc), Pop::Empty);

    // Fill to capacity, then Full.
    for i in 0..CAP {
        assert_eq!(cache.push(sc, i), Push::Success, "fill {i}");
    }
    assert_eq!(cache.push(sc, 0xDEAD), Push::Full);
    assert_eq!(cache.slot_len(cpu, sc), CAP as u32);
}

#[test]
fn locked_byte_diverts_to_fallback() {
    // W7-4: when the per-CPU lock byte is set, the sequence must not commit.
    if !rseq::enable() {
        return;
    }
    assert!(rseq::register_current_thread());
    let cpu = rseq::current_cpu();
    if cpu < 0 || !pin_to(cpu as usize) {
        return;
    }
    let cpu = rseq::current_cpu() as usize;
    let cache = Cache::new();
    let sc = 0;

    // Set the per-CPU lock byte for the current CPU.
    cache.cpus[cpu].locked.store(1, Ordering::Release);
    assert_eq!(cache.pop(sc), Pop::Fallback);
    assert_eq!(cache.push(sc, 1), Push::Fallback);
    assert_eq!(cache.slot_len(cpu, sc), 0, "no commit while locked");

    // Clear it: the sequence proceeds again.
    cache.cpus[cpu].locked.store(0, Ordering::Release);
    assert_eq!(cache.push(sc, 7), Push::Success);
    assert_eq!(cache.pop(sc), Pop::Success(7));
}

#[test]
fn uninitialised_buffer_diverts_to_fallback() {
    if !rseq::enable() {
        return;
    }
    assert!(rseq::register_current_thread());
    let cpu = rseq::current_cpu();
    if cpu < 0 || !pin_to(cpu as usize) {
        return;
    }
    let cpu = rseq::current_cpu() as usize;
    let cache = Cache::new();
    let sc = 2;
    // Null out the buffer pointer to simulate an uninitialised slot.
    cache.cpus[cpu].slots[sc].buf.store(0, Ordering::Release);
    assert_eq!(cache.pop(sc), Pop::Fallback);
    assert_eq!(cache.push(sc, 1), Push::Fallback);
}

/// W7-6 / §34.5: signal delivery near the critical sequence. A worker pinned to
/// one CPU round-trips tokens through its slot while another thread bombards it
/// with a thread-directed signal. Each signal delivered while the worker is in
/// the critical section aborts it (the kernel jumps to `abort_ip` before running
/// the handler), so the worker simply retries — and the token multiset is
/// conserved exactly.
#[test]
fn signal_near_sequence_conserves() {
    if !rseq::enable() {
        return;
    }
    assert!(rseq::register_current_thread());
    let cpu0 = rseq::current_cpu();
    if cpu0 < 0 || !pin_to(cpu0 as usize) {
        return;
    }

    // Install a no-op SIGUSR1 handler so the directed signals don't terminate.
    extern "C" fn noop_handler(_sig: i32) {}
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
        fn pthread_self() -> u64;
        fn pthread_kill(thread: u64, sig: i32) -> i32;
    }
    const SIGUSR1: i32 = 10;
    // SAFETY: installing a trivial async-signal-safe handler for SIGUSR1.
    unsafe { signal(SIGUSR1, noop_handler as *const () as usize) };

    let cache = Arc::new(Cache::new());
    let sc = 1;
    const NTOK: usize = 100;
    let cpu = rseq::current_cpu() as usize;
    let tokens: Vec<usize> = (0..NTOK).map(|i| 0xE00000 + i).collect();
    cache.seed(cpu, sc, &tokens);

    let worker_tid = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let signals = Arc::new(AtomicU64::new(0));

    std::thread::scope(|s| {
        // Worker: pin to the seeded CPU and round-trip tokens until told to stop.
        {
            let cache = cache.clone();
            let worker_tid = worker_tid.clone();
            let stop = stop.clone();
            s.spawn(move || {
                rseq::register_current_thread();
                pin_to(cpu);
                // SAFETY: pthread_self is a no-argument query of the current thread.
                worker_tid.store(unsafe { pthread_self() }, Ordering::Release);
                let mut hand: Option<usize> = None;
                while !stop.load(Ordering::Relaxed) {
                    match hand {
                        None => match cache.pop(sc) {
                            Pop::Success(v) => hand = Some(v),
                            Pop::Empty => {}
                            Pop::Abort | Pop::Fallback => {}
                        },
                        Some(tok) => match cache.push(sc, tok) {
                            Push::Success => hand = None,
                            Push::Full => {}
                            Push::Abort | Push::Fallback => {}
                        },
                    }
                }
                // Return any token still in hand.
                while let Some(tok) = hand {
                    if let Push::Success = cache.push(sc, tok) {
                        hand = None;
                    }
                }
            });
        }
        // Signaller: bombard the worker with thread-directed signals, then stop.
        {
            let worker_tid = worker_tid.clone();
            let stop = stop.clone();
            let signals = signals.clone();
            s.spawn(move || {
                let tid = loop {
                    let t = worker_tid.load(Ordering::Acquire);
                    if t != 0 {
                        break t;
                    }
                    std::hint::spin_loop();
                };
                for _ in 0..200_000 {
                    // SAFETY: `tid` is the live worker thread; SIGUSR1 has a handler.
                    unsafe { pthread_kill(tid, SIGUSR1) };
                    signals.fetch_add(1, Ordering::Relaxed);
                }
                // Stop only after the last signal, so no signal races thread exit.
                stop.store(true, Ordering::Release);
            });
        }
    });

    let recovered = cache.collect(sc);
    let mut recovered = recovered;
    recovered.sort_unstable();
    let mut expected = tokens.clone();
    expected.sort_unstable();
    assert_eq!(
        recovered,
        expected,
        "token multiset conserved under {} signals near the CS",
        signals.load(Ordering::Relaxed)
    );
}

#[test]
fn forced_migration_conserves_tokens() {
    // W7-2e/W7-6: under affinity churn that forces migration mid-sequence, the
    // token multiset is conserved exactly — no loss, no duplication — and aborts
    // are observed and handled. (Equivalent to the locked baseline's behaviour:
    // both conserve objects.)
    if !rseq::enable() {
        return;
    }
    let ncpu = ncpus();
    if ncpu < 2 {
        return; // need >1 CPU to force migration
    }
    let cache = Arc::new(Cache::new());
    let sc = 0;
    const NTOK: usize = 200;
    let tokens: Vec<usize> = (0..NTOK).map(|i| 0x700000 + i).collect();
    cache.seed(0, sc, &tokens);

    let aborts = Arc::new(AtomicU64::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let nthreads = ncpu.max(2);
    let iters = 300_000u64;

    std::thread::scope(|s| {
        for t in 0..nthreads {
            let cache = cache.clone();
            let aborts = aborts.clone();
            let in_flight = in_flight.clone();
            s.spawn(move || {
                assert!(rseq::register_current_thread());
                let mut rng =
                    0x9E37_79B9_7F4A_7C15u64 ^ (t as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
                let mut hand: Option<usize> = None;
                for i in 0..iters {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    if i.is_multiple_of(2048) {
                        pin_to((rng as usize) % ncpu);
                    }
                    match hand {
                        None => loop {
                            match cache.pop(sc) {
                                Pop::Abort => {
                                    aborts.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                Pop::Fallback => continue,
                                Pop::Empty => break,
                                Pop::Success(v) => {
                                    hand = Some(v);
                                    in_flight.fetch_add(1, Ordering::Relaxed);
                                    break;
                                }
                            }
                        },
                        Some(tok) => loop {
                            match cache.push(sc, tok) {
                                Push::Abort => {
                                    aborts.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                Push::Fallback => continue,
                                Push::Full => break,
                                Push::Success => {
                                    hand = None;
                                    in_flight.fetch_sub(1, Ordering::Relaxed);
                                    break;
                                }
                            }
                        },
                    }
                }
                // Return any token still in hand so the final count is exact.
                while let Some(tok) = hand {
                    match cache.push(sc, tok) {
                        Push::Success => {
                            in_flight.fetch_sub(1, Ordering::Relaxed);
                            hand = None;
                        }
                        Push::Abort | Push::Fallback => {}
                        Push::Full => std::thread::yield_now(),
                    }
                }
            });
        }
    });

    assert_eq!(in_flight.load(Ordering::Relaxed), 0, "all tokens returned");
    let mut recovered = cache.collect(sc);
    recovered.sort_unstable();
    let mut expected = tokens.clone();
    expected.sort_unstable();
    assert_eq!(
        recovered.len(),
        expected.len(),
        "no tokens lost or duplicated (aborts={})",
        aborts.load(Ordering::Relaxed)
    );
    assert_eq!(
        recovered, expected,
        "token multiset conserved under migration"
    );
}

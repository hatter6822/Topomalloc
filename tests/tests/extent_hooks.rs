// SPDX-License-Identifier: MIT
//! Extent-hook & custom-backing integration tests (§23, plan 06 W10). The in-crate
//! `topo_core::hooks` tests cover the `HookProvider` dispatch in isolation; this
//! file adds the **cross-crate** properties:
//!
//! * the back-end extent manager and the full central-path allocator running over a
//!   user-supplied *custom backing* (a `HookProvider`) — proving the §23.2 interface
//!   is "wired through the provider seam" end to end (W10-1);
//! * every §23.2 op (`alloc`/`dealloc`/`commit`/`decommit`/`purge`/`split`/`merge`)
//!   dispatches to the user hook through real carve/coalesce/commit traffic;
//! * the §23.3 output contracts are enforced — a hook that returns a misaligned or
//!   undersized range is rejected, never handed out (W10-2);
//! * the §34.8 failure-injection property — *every* hook can fail and the back-end
//!   stays well-formed after every step (W10-3).

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use proptest::prelude::*;

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{ArenaId, BackendError, ExtentHooks, ExtentManager, Fit, HookProvider, Region};

const PAGE: usize = PAGE_SIZE;

/// A leaked heap metadata arena, valid for the process (the shared test pattern) so
/// vended slot pools outlive every map/allocator built over them.
fn meta(bytes: usize) -> &'static BumpArena {
    let buf = vec![0u8; bytes].into_boxed_slice();
    let len = buf.len();
    let ptr = Box::into_raw(buf).cast::<u8>();
    // SAFETY: the leaked buffer is live for the process; `len` bytes are valid.
    Box::leak(Box::new(unsafe { BumpArena::new(ptr, len) }))
}

// ===========================================================================
// A host-backed custom backing (the §23.1 "custom memory source") with op
// counters and per-op failure injection (W10-3). Counters/flags live behind an
// `Arc` so a handle survives the move of the hooks into the `HookProvider`.
// ===========================================================================

#[derive(Default)]
struct HookStats {
    allocs: AtomicU32,
    deallocs: AtomicU32,
    commits: AtomicU32,
    decommits: AtomicU32,
    purge_lazy: AtomicU32,
    purge_forced: AtomicU32,
    splits: AtomicU32,
    merges: AtomicU32,
    /// Per-op failure period: fail the op every Nth call (`0` ⇒ never). The §34.8
    /// fault injection — each maps to a real recovery path the allocator must keep
    /// well-formed through (alloc/commit gate; decommit retains; split/merge are
    /// advisory).
    fail_alloc: AtomicU32,
    fail_dealloc: AtomicU32,
    fail_commit: AtomicU32,
    fail_decommit: AtomicU32,
    fail_split: AtomicU32,
    fail_merge: AtomicU32,
    // call counters backing the period check (separate from the success counters).
    alloc_calls: AtomicU64,
    dealloc_calls: AtomicU64,
    commit_calls: AtomicU64,
    decommit_calls: AtomicU64,
    split_calls: AtomicU64,
    merge_calls: AtomicU64,
}

impl HookStats {
    /// Whether the op whose `period`/`calls` these are should fail on this call.
    fn trips(period: &AtomicU32, calls: &AtomicU64) -> bool {
        let p = period.load(Ordering::Relaxed) as u64;
        if p == 0 {
            return false;
        }
        let n = calls.fetch_add(1, Ordering::Relaxed) + 1;
        n.is_multiple_of(p)
    }
}

/// A custom backing: host allocations as the memory source, recording every op and
/// honouring the injected failures. Self-checks the §23.3 stateful contracts
/// (no-overlap on alloc, dealloc pairs a live reservation).
struct HostHooks {
    stats: Arc<HookStats>,
    live: Mutex<Vec<(usize, Layout)>>,
}

impl HostHooks {
    fn new(stats: Arc<HookStats>) -> Self {
        Self {
            stats,
            live: Mutex::new(Vec::new()),
        }
    }
    /// Whether `p` falls within one of the regions this backing handed out — i.e.
    /// the allocation was served from these hooks (per-arena routing check, W10).
    fn contains(&self, p: *mut u8) -> bool {
        let a = p as usize;
        self.live
            .lock()
            .unwrap()
            .iter()
            .any(|&(b, l)| a >= b && a < b + l.size())
    }
}

impl Drop for HostHooks {
    fn drop(&mut self) {
        for (base, layout) in self.live.get_mut().unwrap().drain(..) {
            // SAFETY: each entry is exactly a pointer/layout from `alloc`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
    }
}

impl ExtentHooks for HostHooks {
    fn alloc(
        &self,
        size: usize,
        align: usize,
        zero: &mut bool,
        commit: &mut bool,
    ) -> Result<Region, BackendError> {
        self.stats.allocs.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_alloc, &self.stats.alloc_calls) {
            return Err(BackendError::OutOfMemory);
        }
        let layout =
            Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
        // SAFETY: nonzero size + power-of-two alignment (HookProvider checked).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        let mut live = self.live.lock().unwrap();
        let b = base as usize;
        for &(lb, ll) in live.iter() {
            assert!(
                b + size <= lb || lb + ll.size() <= b,
                "§23.3 violated: hook alloc overlaps a live reservation"
            );
        }
        live.push((b, layout));
        *zero = false;
        *commit = true; // host memory is immediately usable
        Ok(Region { base, len: size })
    }

    fn dealloc(&self, region: Region, _committed: bool) -> Result<(), BackendError> {
        self.stats.deallocs.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_dealloc, &self.stats.dealloc_calls) {
            // Refuse to take the region back, keeping it live so this backing's own
            // `Drop` still reclaims it (the test leaks nothing). Models a backing
            // whose whole-region release fails on arena teardown (W10 strict teardown).
            return Err(BackendError::OutOfMemory);
        }
        let mut live = self.live.lock().unwrap();
        let base = region.base as usize;
        let idx = live
            .iter()
            .position(|&(b, _)| b == base)
            .expect("§23.3 violated: dealloc of an unknown region");
        let (_, layout) = live.swap_remove(idx);
        // SAFETY: exactly the pointer/layout from the matching `alloc`.
        unsafe { dealloc(base as *mut u8, layout) };
        Ok(())
    }

    fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
        self.stats.commits.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_commit, &self.stats.commit_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }

    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        self.stats.decommits.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_decommit, &self.stats.decommit_calls) {
            return Err(BackendError::OutOfMemory);
        }
        // Model MADV_DONTNEED: discard now so a later read faults a fresh page.
        // SAFETY: `check_subrange` in `HookProvider` confirmed `[offset, offset+len)`
        // is in bounds of `region`, which is a live host reservation.
        unsafe { std::ptr::write_bytes(region.base.add(offset), 0, len) };
        Ok(())
    }

    fn purge_lazy(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
        self.stats.purge_lazy.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        self.stats.purge_forced.fetch_add(1, Ordering::Relaxed);
        // SAFETY: in-bounds sub-range (validated by `HookProvider`).
        unsafe { std::ptr::write_bytes(region.base.add(offset), 0, len) };
        Ok(())
    }

    fn split(
        &self,
        _region: Region,
        _a: usize,
        _b: usize,
        _committed: bool,
    ) -> Result<(), BackendError> {
        self.stats.splits.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_split, &self.stats.split_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }

    fn merge(&self, _l: Region, _r: Region, _committed: bool) -> Result<(), BackendError> {
        self.stats.merges.fetch_add(1, Ordering::Relaxed);
        if HookStats::trips(&self.stats.fail_merge, &self.stats.merge_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "host-hooks"
    }
}

/// Build an extent manager over a `HookProvider<HostHooks>`, returning the manager
/// and a handle to the shared hook stats.
fn hook_manager(
    pages: usize,
    slots: usize,
) -> (ExtentManager<HookProvider<HostHooks>>, Arc<HookStats>) {
    let stats = Arc::new(HookStats::default());
    let provider = HookProvider::new(HostHooks::new(stats.clone()));
    let mgr = ExtentManager::new(
        provider,
        meta(1 << 20),
        ArenaId::DEFAULT,
        pages * PAGE,
        PAGE,
        slots,
    )
    .expect("manager over hook backing");
    (mgr, stats)
}

// ===========================================================================
// W10-1: end-to-end dispatch through the provider seam.
// ===========================================================================

#[test]
fn extent_manager_over_hooks_dispatches_every_op() {
    // Building the manager reserves the region via the `alloc` hook (custom memory
    // source); allocation commits via the `commit` hook; a tail-trimming carve
    // dispatches the `split` hook; coalescing two adjacent backed free extents
    // dispatches the `merge` hook. Every §23.2 op flows to the user hook.
    let (mgr, stats) = hook_manager(64, 4096);
    assert_eq!(
        stats.allocs.load(Ordering::Relaxed),
        1,
        "region reserve = one alloc"
    );
    assert_eq!(mgr.backend_name(), "host-hooks");

    // Two adjacent carves, each trimming a tail ⇒ ≥ 2 `split` dispatches; each
    // commits its extent ⇒ `commit` dispatches.
    let a = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc a");
    let b = mgr.alloc(3 * PAGE, PAGE, Fit::Best).expect("alloc b");
    assert!(
        stats.splits.load(Ordering::Relaxed) >= 2,
        "tail trims dispatched splits"
    );
    assert!(
        stats.commits.load(Ordering::Relaxed) >= 2,
        "commits dispatched"
    );
    let region = mgr.region_of(a).expect("region");
    // SAFETY: the carved extent is committed for its whole length.
    unsafe {
        region.base.write(0xa5);
        assert_eq!(region.base.read(), 0xa5);
    }

    // Free `a` (its right neighbour `b` is still live ⇒ no merge yet), then free `b`:
    // the two adjacent, backing-compatible (both committed) free extents coalesce ⇒
    // a `merge` dispatch.
    mgr.free(a).expect("free a");
    mgr.free(b).expect("free b");
    assert!(
        stats.merges.load(Ordering::Relaxed) >= 1,
        "coalesce dispatched merge"
    );
    assert!(
        mgr.check_invariants(),
        "back-end well-formed over the hook backing"
    );

    drop(mgr);
    // Tearing the manager down returns the region via the `dealloc` hook.
    assert_eq!(
        stats.deallocs.load(Ordering::Relaxed),
        1,
        "region release = one dealloc"
    );
}

#[test]
fn explicit_release_and_purge_dispatch_to_hooks() {
    // The page-granular release / purge_lazy / purge_forced manager ops each
    // dispatch to the corresponding §23.2 hook.
    let (mgr, stats) = hook_manager(32, 4096);
    let r = mgr.alloc(4 * PAGE, PAGE, Fit::Best).expect("alloc");
    mgr.free(r).expect("free"); // Dirty
    mgr.purge_lazy(r).expect("purge_lazy"); // Dirty -> Muzzy via the purge_lazy hook
    assert_eq!(stats.purge_lazy.load(Ordering::Relaxed), 1);
    // `release` decommits the freed extent's backing through the decommit hook.
    mgr.release(r).expect("release");
    assert!(
        stats.decommits.load(Ordering::Relaxed) >= 1,
        "release dispatched decommit"
    );
    assert!(mgr.check_invariants());
}

// ===========================================================================
// W10-1: the full central-path allocator over a custom backing.
// ===========================================================================

#[test]
fn allocator_runs_the_full_central_path_over_a_hook_backing() {
    use topo_core::{Allocator, AllocatorConfig, FreeOutcome, PageMap, RequestFlags};

    let span_stats = Arc::new(HookStats::default());
    let large_stats = Arc::new(HookStats::default());
    let m = meta(16 * 1024 * 1024);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    let a = Allocator::new(
        HookProvider::new(HostHooks::new(span_stats.clone())),
        HookProvider::new(HostHooks::new(large_stats.clone())),
        m,
        m,
        pm,
        ArenaId::DEFAULT,
        AllocatorConfig::small(),
    )
    .expect("allocator over hook backing");
    assert_eq!(a.backend_name(), "host-hooks");

    // Small allocations (span path) are aligned, writable, and distinct.
    let mut ptrs = Vec::new();
    for i in 0..32usize {
        let p = a.malloc(48 + i);
        assert!(!p.is_null(), "malloc over hook backing");
        // SAFETY: `p` is a live allocation of at least `48 + i` bytes.
        unsafe { std::ptr::write_bytes(p, (i as u8) | 1, 48 + i) };
        ptrs.push((p, (i as u8) | 1, 48 + i));
    }
    // Every span byte still carries its own tag (no aliasing).
    for &(p, tag, n) in &ptrs {
        // SAFETY: `p` is still live for `n` bytes.
        unsafe {
            assert_eq!(p.read(), tag);
            assert_eq!(p.add(n - 1).read(), tag);
        }
    }
    // The span backing went through the hook (reserve + commit).
    assert!(span_stats.allocs.load(Ordering::Relaxed) >= 1);
    assert!(span_stats.commits.load(Ordering::Relaxed) >= 1);

    // A large allocation routes to the large region's hook backing.
    let big = a.malloc(4 * 1024 * 1024); // >= HUGE_THRESHOLD ⇒ large path
    assert!(!big.is_null());
    // SAFETY: a live 4 MiB allocation.
    unsafe {
        big.write(0x5a);
        assert_eq!(big.read(), 0x5a);
    }
    assert!(
        large_stats.allocs.load(Ordering::Relaxed) >= 1,
        "large path used the hook backing"
    );

    // realloc preserves content across a move over the hook backing.
    let (p0, _, _) = ptrs[0];
    // SAFETY: `p0` is a live allocation from this allocator.
    let grown = unsafe { a.realloc(p0, 4096, 16, RequestFlags::NONE) };
    assert!(!grown.is_null());
    // SAFETY: `grown` holds the preserved prefix.
    unsafe { assert_eq!(grown.read(), ptrs[0].1, "realloc preserved the prefix") };
    ptrs[0] = (grown, ptrs[0].1, 4096);

    // Free everything; nothing double-frees or corrupts.
    for &(p, _, _) in &ptrs {
        // SAFETY: each `p` is a live allocation of `a` (the realloc'd one updated).
        assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
    }
    // SAFETY: `big` is live.
    assert_eq!(unsafe { a.free(big) }, FreeOutcome::Freed);

    // The §8.6 application-side identity holds over the custom backing.
    let stats = a.stats();
    assert_eq!(
        stats.live_bytes,
        stats.allocated_bytes_total - stats.freed_bytes_total
    );
}

// ===========================================================================
// W10-2: §23.3 contract enforcement.
// ===========================================================================

/// A backing whose `alloc` deliberately violates the §23.3 output contract: it
/// returns a range that is either undersized or misaligned (selectable), to prove
/// `HookProvider` rejects it rather than handing out unsound memory. `deallocs`
/// counts `dealloc` calls so a test can confirm a *rejected* reserve hands the
/// (real) backing block back — never leaks it (W10 / §2.4 safety-before-policy).
struct BadHooks {
    misalign: bool,
    backing: Mutex<Option<(usize, Layout)>>,
    deallocs: Arc<AtomicU32>,
}

impl BadHooks {
    fn new(misalign: bool) -> Self {
        Self {
            misalign,
            backing: Mutex::new(None),
            deallocs: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl ExtentHooks for BadHooks {
    fn alloc(
        &self,
        size: usize,
        align: usize,
        _zero: &mut bool,
        _commit: &mut bool,
    ) -> Result<Region, BackendError> {
        // Allocate a real, generous host block so the returned pointer is valid
        // memory — the violation is purely in the *reported* geometry.
        let layout = Layout::from_size_align(size + align + PAGE, align).unwrap();
        // SAFETY: nonzero size, valid alignment.
        let base = unsafe { alloc(layout) };
        assert!(!base.is_null());
        *self.backing.lock().unwrap() = Some((base as usize, layout));
        if self.misalign {
            // Offset by one byte: now misaligned to `align` (≥ 2).
            // SAFETY: offset 1 is within the over-sized (`size + align + PAGE`) block.
            let misaligned = unsafe { base.add(1) };
            Ok(Region {
                base: misaligned,
                len: size,
            })
        } else {
            // Report a length one byte short of the request (undersized).
            Ok(Region {
                base,
                len: size.saturating_sub(1),
            })
        }
    }
    fn dealloc(&self, _region: Region, _committed: bool) -> Result<(), BackendError> {
        self.deallocs.fetch_add(1, Ordering::Relaxed);
        if let Some((base, layout)) = self.backing.lock().unwrap().take() {
            // SAFETY: exactly the block from `alloc`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
        Ok(())
    }
}

impl Drop for BadHooks {
    fn drop(&mut self) {
        if let Some((base, layout)) = self.backing.get_mut().unwrap().take() {
            // SAFETY: exactly the block from `alloc`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn reserve_rejects_an_undersized_hook_result() {
    use topo_core::TopoBackingProvider;
    // §23.3 "at least requested size" / §2.4 safety-before-policy: a hook returning
    // a too-small range is rejected, never handed out. (Release-only: in debug the
    // contract violation aborts via `debug_assert!`, which is the intended loud
    // failure — see the companion `#[should_panic]` test.)
    let p = HookProvider::new(BadHooks::new(false));
    assert!(matches!(
        p.reserve(ArenaId::DEFAULT, 4096, 64),
        Err(BackendError::InvalidRequest)
    ));
}

#[cfg(not(debug_assertions))]
#[test]
fn reserve_rejects_a_misaligned_hook_result() {
    use topo_core::TopoBackingProvider;
    let p = HookProvider::new(BadHooks::new(true));
    assert!(matches!(
        p.reserve(ArenaId::DEFAULT, 4096, 64),
        Err(BackendError::InvalidRequest)
    ));
}

#[cfg(not(debug_assertions))]
#[test]
fn a_rejected_reserve_hands_the_backing_back() {
    use topo_core::TopoBackingProvider;
    // §23.3 / §2.4 (PR-review comment 4): a hook result that fails the geometry
    // contract is rejected — and the real backing block the hook allocated for it is
    // **returned to the hook** (`dealloc`) on the reject path, never leaked. The hook
    // allocated one block in `alloc`; assert the rejected reserve called `dealloc`
    // exactly once (so the block went back), and that the slot is empty afterwards.
    // (Release-only: in debug the contract violation aborts via `debug_assert!`.)
    let bad = BadHooks::new(false); // undersized result → rejected
    let deallocs = bad.deallocs.clone();
    let p = HookProvider::new(bad);
    assert!(matches!(
        p.reserve(ArenaId::DEFAULT, 4096, 64),
        Err(BackendError::InvalidRequest)
    ));
    assert_eq!(
        deallocs.load(Ordering::Relaxed),
        1,
        "a rejected reserve returns the hook's backing block (no leak)"
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "§23.3")]
fn reserve_debug_aborts_on_a_contract_violation() {
    use topo_core::TopoBackingProvider;
    // The debug counterpart: a §23.3 contract violation aborts loudly (W10-2
    // "violations detected in debug"). CI runs tests in debug, so this is the
    // profile that actually exercises the abort.
    let p = HookProvider::new(BadHooks::new(true));
    let _ = p.reserve(ArenaId::DEFAULT, 4096, 64);
}

// ===========================================================================
// W10-3: failure-injection property (§34.8).
// ===========================================================================

#[derive(Clone, Copy, Debug)]
enum HookOp {
    Alloc(u8),
    Free(u16),
}

proptest! {
    /// §34.8 / W10-3: under random failures injected into every fallible *runtime*
    /// hook (commit / decommit / split / merge), each back-end operation either
    /// succeeds or fails cleanly, and the back-end stays well-formed after every
    /// step — a failing hook never corrupts allocator state (§23.3). The recovery
    /// paths exercised: a **commit** failure rolls the carve back; a **decommit**
    /// failure (under the unmap policy) retains the extent; **split**/**merge**
    /// failures are advisory (recorded in the provider, never acted on). All keep
    /// `check_invariants` green.
    ///
    /// (`alloc`/`dealloc` are construction/teardown ops — the manager reserves one
    /// region and subdivides it internally — so they are exercised by the
    /// `HookProvider` unit tests, not this runtime property.)
    #[test]
    fn fuzzed_hook_failures_keep_the_backend_wellformed(
        periods in (1u32..=5, 1u32..=5, 1u32..=3, 1u32..=3),
        ops in prop::collection::vec(
            prop_oneof![
                (1u8..=4).prop_map(HookOp::Alloc),
                any::<u16>().prop_map(HookOp::Free),
            ],
            0..120,
        ),
    ) {
        let (fc, fd, fs, fm) = periods;
        let stats = Arc::new(HookStats::default());
        let provider = HookProvider::new(HostHooks::new(stats.clone()));
        // Construct cleanly (the region reserve must succeed), use the unmap policy
        // so decommit-hook failures are exercised on free, then arm the runtime
        // hooks to fail periodically.
        let mut mgr = ExtentManager::new(
            provider, meta(1 << 20), ArenaId::DEFAULT, 128 * PAGE, PAGE, 4096,
        ).expect("manager over hook backing");
        mgr.set_retain_policy(topo_core::RetainPolicy::Unmap);
        stats.fail_commit.store(fc, Ordering::Relaxed);
        stats.fail_decommit.store(fd, Ordering::Relaxed);
        stats.fail_split.store(fs, Ordering::Relaxed);
        stats.fail_merge.store(fm, Ordering::Relaxed);

        let mut live: Vec<topo_core::ExtentRef> = Vec::new();
        for op in ops {
            match op {
                HookOp::Alloc(pages) => {
                    if let Ok(r) = mgr.alloc(pages as usize * PAGE, PAGE, Fit::Best) {
                        live.push(r);
                    }
                }
                HookOp::Free(idx) => {
                    if !live.is_empty() {
                        let k = (idx as usize) % live.len();
                        let r = live.swap_remove(k);
                        let _ = mgr.free(r);
                    }
                }
            }
            prop_assert!(
                mgr.check_invariants(),
                "an injected hook failure left the back-end ill-formed (W10-3)"
            );
        }
    }
}

// ===========================================================================
// W10-3: the FULL allocator (malloc/free/realloc) under hook failures, and
// hook-vs-baseline behavioural equivalence.
// ===========================================================================

use topo_core::{Allocator, AllocatorConfig, FreeOutcome, PageMap, RequestFlags};

/// A small config whose span/large regions the host hooks back eagerly — modest so
/// the per-iteration host allocation stays cheap.
fn hook_cfg() -> AllocatorConfig {
    AllocatorConfig {
        span_region_bytes: 8 * 1024 * 1024,
        span_extent_slots: 256,
        span_slots: 256,
        large_region_bytes: 16 * 1024 * 1024,
        large_extent_slots: 128,
        large_slots: 128,
    }
}

/// Build a scoped (non-leaking) allocator over two host-hook backings, returning it
/// plus handles to the span/large hook stats (for failure injection / inspection).
fn hook_allocator<'a>(
    arena: &'a BumpArena,
    pm: &'a PageMap,
) -> (
    Allocator<'a, HookProvider<HostHooks>>,
    Arc<HookStats>,
    Arc<HookStats>,
) {
    let span_stats = Arc::new(HookStats::default());
    let large_stats = Arc::new(HookStats::default());
    let a = Allocator::new(
        HookProvider::new(HostHooks::new(span_stats.clone())),
        HookProvider::new(HostHooks::new(large_stats.clone())),
        arena,
        arena,
        pm,
        ArenaId::DEFAULT,
        hook_cfg(),
    )
    .expect("allocator over hook backing");
    (a, span_stats, large_stats)
}

#[derive(Clone, Copy, Debug)]
enum AllocOp {
    Malloc(u16),
    Realloc(u16, u16),
    Free(u16),
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// §34.8 / W10-3 over the **full central-path allocator**: under random failures
    /// injected into the runtime hooks (commit/decommit/split/merge) of *both* the
    /// span and large backings, malloc/free/realloc stay sound — every result is
    /// null or a valid, writable, non-aliasing object; every free of a live pointer
    /// succeeds; and the §8.6 identity `live == allocated - freed` holds throughout.
    /// A failing hook never corrupts the allocator (not just the extent manager).
    #[test]
    fn full_allocator_stays_sound_under_hook_failures(
        periods in (1u32..=4, 1u32..=4, 1u32..=3, 1u32..=3),
        ops in prop::collection::vec(
            prop_oneof![
                (1u16..=4096).prop_map(AllocOp::Malloc),
                (1u16..=4096, 0u16..).prop_map(|(n, i)| AllocOp::Realloc(n, i)),
                any::<u16>().prop_map(AllocOp::Free),
            ],
            0..80,
        ),
    ) {
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        // SAFETY: `buf` outlives the allocator/arena (dropped at function end).
        let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
        let pm = PageMap::new();
        let (a, span_stats, large_stats) = hook_allocator(&arena, &pm);
        for s in [&span_stats, &large_stats] {
            let (fc, fd, fs, fm) = periods;
            s.fail_commit.store(fc, Ordering::Relaxed);
            s.fail_decommit.store(fd, Ordering::Relaxed);
            s.fail_split.store(fs, Ordering::Relaxed);
            s.fail_merge.store(fm, Ordering::Relaxed);
        }

        // (ptr, tag, usable_lower_bound) for each live allocation.
        let mut live: Vec<(*mut u8, u8, usize)> = Vec::new();
        let mut tag: u8 = 1;
        let check = |live: &Vec<(*mut u8, u8, usize)>| {
            for &(p, t, n) in live {
                // SAFETY: `p` is a live allocation of at least `n` bytes.
                let got = unsafe { p.read() };
                prop_assert_eq!(got, t, "a live object's first byte was clobbered");
                if n > 1 {
                    // SAFETY: at least `n` usable bytes.
                    prop_assert_eq!(unsafe { p.add(n - 1).read() }, t, "tail clobbered");
                }
            }
            Ok(())
        };

        for op in ops {
            match op {
                AllocOp::Malloc(n) => {
                    let n = n as usize;
                    let p = a.malloc(n);
                    if !p.is_null() {
                        // SAFETY: a fresh allocation of >= n bytes; stamp it.
                        unsafe { std::ptr::write_bytes(p, tag, n) };
                        live.push((p, tag, n));
                        tag = tag.wrapping_add(1).max(1);
                    }
                }
                AllocOp::Realloc(n, idx) => {
                    if !live.is_empty() {
                        let k = (idx as usize) % live.len();
                        let (p, t, _) = live[k];
                        let n = n as usize;
                        // SAFETY: `p` is a live allocation of this allocator.
                        let q = unsafe { a.realloc(p, n, 16, RequestFlags::NONE) };
                        if q.is_null() {
                            // Failure preserves the original (still live at `live[k]`).
                            // SAFETY: original still valid.
                            prop_assert_eq!(unsafe { p.read() }, t, "realloc-fail lost original");
                        } else {
                            // The moved/grown object keeps its first byte; re-stamp.
                            // SAFETY: `q` is a live allocation of >= n bytes.
                            prop_assert_eq!(unsafe { q.read() }, t, "realloc dropped prefix");
                            // SAFETY: `q` owns at least `n` usable bytes; stamp them.
                            unsafe { std::ptr::write_bytes(q, t, n) };
                            live[k] = (q, t, n);
                        }
                    }
                }
                AllocOp::Free(idx) => {
                    if !live.is_empty() {
                        let k = (idx as usize) % live.len();
                        let (p, _, _) = live.swap_remove(k);
                        // SAFETY: `p` is a live allocation we still own.
                        prop_assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed, "live free failed");
                    }
                }
            }
            check(&live)?;
            // §8.6 identity holds regardless of injected failures.
            let st = a.stats();
            prop_assert_eq!(st.live_bytes, st.allocated_bytes_total - st.freed_bytes_total);
        }

        // Drain everything; each free of a still-live pointer succeeds.
        for (p, _, _) in live {
            // SAFETY: `p` is a live allocation we still own.
            prop_assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
        }
        let st = a.stats();
        prop_assert_eq!(st.live_bytes, 0, "everything freed");
    }
}

#[test]
fn hook_backing_is_behaviourally_coequal_with_posix() {
    use topo_backend_posix::PosixBackingProvider;

    // The same malloc/free/usable-size op stream produces the same *abstract*
    // outcomes (success + usable size, address-independent) over the hook backing
    // and the real POSIX backend — the hook custom backing is behaviourally
    // co-equal, just like POSIX vs. the seLe4n sim (D2).
    fn run_hooks(ops: &[(bool, usize)]) -> Vec<(bool, usize)> {
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        // SAFETY: `buf` outlives the allocator.
        let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
        let pm = PageMap::new();
        let (a, _s, _l) = hook_allocator(&arena, &pm);
        drive(&a, ops)
    }
    fn run_posix(ops: &[(bool, usize)]) -> Vec<(bool, usize)> {
        let arena = meta(8 * 1024 * 1024);
        let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
        let a = Allocator::new(
            PosixBackingProvider::new(),
            PosixBackingProvider::new(),
            arena,
            arena,
            pm,
            ArenaId::DEFAULT,
            hook_cfg(),
        )
        .expect("posix allocator");
        drive(&a, ops)
    }
    fn drive<P: topo_core::TopoBackingProvider>(
        a: &Allocator<'_, P>,
        ops: &[(bool, usize)],
    ) -> Vec<(bool, usize)> {
        let mut out = Vec::new();
        let mut live: Vec<*mut u8> = Vec::new();
        for &(is_alloc, n) in ops {
            if is_alloc {
                let p = a.malloc(n);
                if p.is_null() {
                    out.push((false, 0));
                } else {
                    let usable = a.usable_size(p).unwrap_or(0);
                    out.push((true, usable));
                    live.push(p);
                }
            } else if let Some(p) = live.pop() {
                // SAFETY: `p` was returned by `a` and is still owned here.
                assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
                out.push((true, 0));
            } else {
                out.push((true, 0));
            }
        }
        for p in live {
            // SAFETY: still-owned live pointer.
            assert_eq!(unsafe { a.free(p) }, FreeOutcome::Freed);
        }
        out
    }

    // A deterministic mix of small (slab) and large (extent) sizes + frees.
    let ops: Vec<(bool, usize)> = (0..200usize)
        .map(|i| (!i.is_multiple_of(4), (i * 37) % 9000 + 1))
        .collect();
    assert_eq!(
        run_hooks(&ops),
        run_posix(&ops),
        "the hook backing diverged from POSIX in abstract outcomes"
    );
}

// ===========================================================================
// W10 / §22.2–§22.4: per-arena hooked backing regions.
// ===========================================================================

use topo_backend_posix::PosixBackingProvider;
use topo_core::{ArenaPolicy, ArenaState, MAX_HOOK_BACKENDS};

/// Leak a host-hook backing as `&'static dyn ExtentHooks` (the caller-owned,
/// allocator-outliving lifetime the per-arena API requires), returning the hooks
/// reference and a handle to its stats.
fn leak_hooks() -> (&'static HostHooks, Arc<HookStats>) {
    let stats = Arc::new(HookStats::default());
    let hooks: &'static HostHooks = Box::leak(Box::new(HostHooks::new(stats.clone())));
    (hooks, stats)
}

/// A POSIX-backed allocator (the shared/default backend) over leaked metadata +
/// pagemap, so an arena can be given its *own* hooked region alongside it.
fn posix_allocator() -> Allocator<'static, PosixBackingProvider> {
    let arena = meta(16 * 1024 * 1024);
    let pm: &'static PageMap = Box::leak(Box::new(PageMap::new()));
    Allocator::new(
        PosixBackingProvider::new(),
        PosixBackingProvider::new(),
        arena,
        arena,
        pm,
        ArenaId::DEFAULT,
        hook_cfg(),
    )
    .expect("posix allocator")
}

#[test]
fn hooked_arena_serves_from_its_own_region_and_isolates() {
    let a = posix_allocator();
    let (hooks, stats) = leak_hooks();
    let arena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("create hooked arena");
    // The backing reserved its span + large regions via the hooks (§22.4: hooks
    // installed before first extent).
    assert!(
        stats.allocs.load(Ordering::Relaxed) >= 2,
        "hooked arena reserved its own regions via the hooks"
    );

    // A small allocation from the hooked arena is served from the hooks' region.
    let small = a.allocate_in(arena, 100, 16, RequestFlags::NONE);
    assert!(!small.is_null());
    assert!(
        hooks.contains(small),
        "small object is in the hooked region"
    );
    // SAFETY: `small` is a live allocation of >= 100 bytes.
    unsafe { std::ptr::write_bytes(small, 0xab, 100) };

    // A large allocation from the hooked arena, likewise.
    let large = a.allocate_in(arena, 4 * 1024 * 1024, 16, RequestFlags::NONE);
    assert!(!large.is_null());
    assert!(
        hooks.contains(large),
        "large object is in the hooked region"
    );
    // SAFETY: live 4 MiB allocation.
    unsafe { large.write(0x5a) };
    // Aggregate stats include the hooked region: the large is counted, and the
    // large-region physical-state breakdown covers more than the shared region.
    let st_live = a.stats();
    assert!(
        st_live.live_large >= 1,
        "a hooked-arena large allocation is counted in live_large (W10 stats aggregation)"
    );

    // §22.7 isolation: a *default-arena* allocation is NOT in the hooked region
    // (it comes from the shared POSIX backend) — the two arenas' memory is disjoint.
    let shared = a.malloc(100);
    assert!(!shared.is_null());
    assert!(
        !hooks.contains(shared),
        "default-arena object must not fall in the hooked arena's region (§22.7)"
    );

    // Introspection + realloc of a hooked-arena *large* object must route to the
    // OWNING (hooked) backend, not the shared one (regression: the shared large pool
    // cannot resolve a hooked-arena descriptor, so usable_size/realloc would
    // spuriously return None/NULL).
    assert!(
        a.usable_size(large).is_some_and(|u| u >= 4 * 1024 * 1024),
        "usable_size of a hooked-arena large object resolves via its owner"
    );
    // SAFETY: `large` is a live allocation of this allocator; grow it.
    let grown = unsafe { a.realloc(large, 6 * 1024 * 1024, 16, RequestFlags::NONE) };
    assert!(
        !grown.is_null(),
        "realloc of a hooked-arena large object succeeds"
    );
    assert!(
        hooks.contains(grown),
        "the grown object stays in the hooked region"
    );
    // SAFETY: `grown` preserves the prefix and owns >= 6 MiB.
    unsafe { assert_eq!(grown.read(), 0x5a, "realloc preserved the prefix") };
    let large = grown;

    // Frees route back to the owning backend; the §8.6 identity holds.
    // SAFETY: all three are live allocations of `a`.
    unsafe {
        assert_eq!(a.free(small), FreeOutcome::Freed);
        assert_eq!(a.free(large), FreeOutcome::Freed);
        assert_eq!(a.free(shared), FreeOutcome::Freed);
    }
    assert!(a.check_invariants(), "all backends well-formed");
    let st = a.stats();
    assert_eq!(st.live_bytes, 0);
}

#[test]
fn per_arena_hook_failures_surface_in_stats() {
    // W10 observability: a hooked arena's custom-backing failures are reachable
    // through `ArenaStats::hooks` (per-arena) and the global `AllocatorStats`.
    let a = posix_allocator();
    let (hooks, stats) = leak_hooks();
    let arena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("create hooked arena");

    // The shared/default backend has no hooks to fail ⇒ no hook stats.
    assert!(
        a.arena_stats(ArenaId::DEFAULT).unwrap().hooks.is_none(),
        "a non-hooked arena carries no hook-failure stats"
    );
    // A fresh hooked arena has hook stats, all zero.
    let hs0 = a
        .arena_stats(arena)
        .unwrap()
        .hooks
        .expect("a hooked arena carries hook-failure stats");
    assert_eq!(hs0, topo_core::HookFailureStats::default());

    // Arm the commit hook to fail every call, then allocate: the span's commit fails,
    // the allocation fails cleanly (no fallback for a hooked arena), and the failure
    // is COUNTED and observable per-arena — the gap W10's audit closed.
    stats.fail_commit.store(1, Ordering::Relaxed);
    let p = a.allocate_in(arena, 100, 16, RequestFlags::NONE);
    assert!(
        p.is_null(),
        "a backing that cannot commit cannot satisfy the request"
    );
    stats.fail_commit.store(0, Ordering::Relaxed); // disarm

    let hs = a.arena_stats(arena).unwrap().hooks.expect("hook stats");
    assert!(
        hs.commit >= 1,
        "the commit-hook failure is observable per-arena (W10): {hs:?}"
    );
    // …and rolls up into the global cumulative total (live backend summed in).
    assert!(
        a.stats().hook_failures.commit >= 1,
        "the commit-hook failure is observable in the global stats too"
    );
    assert!(
        a.check_invariants(),
        "the failed commit left every backend well-formed"
    );
}

#[test]
fn hooked_arena_destroy_returns_the_region_to_the_hooks() {
    let a = posix_allocator();
    let (hooks, stats) = leak_hooks();
    let arena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("create hooked arena");
    let reserved = stats.allocs.load(Ordering::Relaxed);
    assert!(reserved >= 2);

    // Allocate, then destroy with the objects still live (§22.5: destroy discards
    // outstanding allocations); the drain retires them and the teardown returns the
    // hooked regions to the backing via `dealloc`.
    let p = a.allocate_in(arena, 200, 16, RequestFlags::NONE);
    assert!(!p.is_null() && hooks.contains(p));
    let _big = a.allocate_in(arena, 5 * 1024 * 1024, 16, RequestFlags::NONE);

    // SAFETY: the arena is quiesced (single-threaded test, no outstanding ops).
    unsafe { a.arena_destroy(arena) }.expect("destroy hooked arena");

    // Every region the hooks handed out was returned (dealloc count == alloc count).
    assert_eq!(
        stats.deallocs.load(Ordering::Relaxed),
        stats.allocs.load(Ordering::Relaxed),
        "destroy returned every hooked region to the backing"
    );
    assert!(a.check_invariants());

    // A second hooked arena can now reuse the freed registry slot.
    let (hooks2, _s2) = leak_hooks();
    let arena2 = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks2, hook_cfg())
        .expect("registry slot reused after destroy");
    let q = a.allocate_in(arena2, 64, 16, RequestFlags::NONE);
    assert!(!q.is_null() && hooks2.contains(q));
    // SAFETY: live allocation.
    unsafe { assert_eq!(a.free(q), FreeOutcome::Freed) };
}

#[test]
fn hooked_arena_destroy_quarantines_when_the_backing_refuses_its_region() {
    // §36.13 partial-failure path for W10 (PR #14 review, thread #3 — strict
    // teardown): destroying a hooked arena whose custom backing refuses to take its
    // span/large region back (`dealloc` returns Err) must NOT report a clean
    // success. The drain still retires every object, then the fallible teardown
    // surfaces the backing failure, landing the arena in ErrorQuarantined — never
    // Destroyed (the formal counterpart is
    // `ArenaLifecycle.destroy_backing_release_failure_quarantines`).
    let a = posix_allocator();
    let (hooks, stats) = leak_hooks();
    let arena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("create hooked arena");
    // Real drain work: a small span object and a large (>= huge-threshold) object.
    let p = a.allocate_in(arena, 200, 16, RequestFlags::NONE);
    let big = a.allocate_in(arena, 5 * 1024 * 1024, 16, RequestFlags::NONE);
    assert!(!p.is_null() && !big.is_null() && hooks.contains(p) && hooks.contains(big));

    // Arm the backing to refuse every region return, then destroy. `dealloc` is only
    // called on whole-region teardown (never during normal alloc/free), so this bites
    // exactly the span and large region returns.
    stats.fail_dealloc.store(1, Ordering::Relaxed);
    // SAFETY: the arena is quiesced (single-threaded test; no outstanding ops).
    let outcome = unsafe { a.arena_destroy(arena) };

    assert!(
        outcome.is_err(),
        "a backing that refuses its region return must not report a clean destroy"
    );
    assert_eq!(
        a.arena_stats(arena).unwrap().state,
        ArenaState::ErrorQuarantined,
        "§36.13: a teardown dealloc failure ⇒ ERROR_QUARANTINED, never DESTROYED"
    );
    // Every object was still retired on drain (only the backing region leaked, in the
    // user's hooks), so no live bytes remain and the §8.6/§36.17 reconciliation holds.
    assert_eq!(
        a.arena_stats(arena).unwrap().used,
        0,
        "objects retired on drain"
    );
    assert!(
        !a.owns(p) && !a.owns(big),
        "the arena's objects are invalid"
    );
    // Both region returns were attempted (span + large) and both refused.
    assert_eq!(
        stats.deallocs.load(Ordering::Relaxed),
        2,
        "teardown attempted both the span and large region returns"
    );
    // The shared backend and registry stay well-formed after the quarantine.
    assert!(a.check_invariants());
}

#[test]
fn hooked_arena_reset_keeps_the_region_and_reuses_it() {
    let a = posix_allocator();
    let (hooks, stats) = leak_hooks();
    let arena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("create hooked arena");
    let reserved = stats.allocs.load(Ordering::Relaxed);

    let _p = a.allocate_in(arena, 300, 16, RequestFlags::NONE);
    // SAFETY: quiesced single-threaded reset.
    unsafe { a.arena_reset(arena) }.expect("reset hooked arena");
    // Reset keeps the region (no new reserve, no dealloc of the region).
    assert_eq!(
        stats.allocs.load(Ordering::Relaxed),
        reserved,
        "reset reserved nothing new"
    );
    assert_eq!(
        stats.deallocs.load(Ordering::Relaxed),
        0,
        "reset kept the region"
    );

    // The arena is still usable and still served from the same hooked region.
    let r = a.allocate_in(arena, 128, 16, RequestFlags::NONE);
    assert!(!r.is_null() && hooks.contains(r));
    // SAFETY: live allocation.
    unsafe { assert_eq!(a.free(r), FreeOutcome::Freed) };
    assert!(a.check_invariants());
}

#[test]
fn hooked_arena_registry_full_fails_cleanly() {
    let a = posix_allocator();
    // Fill the registry.
    for _ in 0..MAX_HOOK_BACKENDS {
        let (hooks, _s) = leak_hooks();
        a.arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
            .expect("fill the hooked-backend registry");
    }
    // One more must fail cleanly (Exhausted) — and must not have reserved a region.
    let (extra, extra_stats) = leak_hooks();
    let r = a.arena_create_hooked(&ArenaPolicy::explicit(), extra, hook_cfg());
    assert!(matches!(r, Err(topo_core::ArenaError::Exhausted)));
    // The over-limit attempt left no orphaned reservation (build-then-insert: the
    // region it built was returned when no slot was free).
    assert_eq!(
        extra_stats.allocs.load(Ordering::Relaxed),
        extra_stats.deallocs.load(Ordering::Relaxed),
        "a rejected create leaks no region"
    );
    assert!(a.check_invariants());
}

#[test]
fn concurrent_hooked_and_default_arena_allocation_is_sound() {
    // Stresses the per-arena registry under concurrency: worker threads hammer a
    // hooked arena AND the default arena (so they hold registry references for the
    // hooked one), while another thread creates + destroys *other* hooked arenas —
    // clearing *different* registry slots. This is exactly the cross-arena scenario
    // the per-element (not whole-array) registry access keeps sound; it must never
    // corrupt state, and every backend stays well-formed.
    let a = posix_allocator();
    let (hooks, _stats) = leak_hooks();
    let harena = a
        .arena_create_hooked(&ArenaPolicy::explicit(), hooks, hook_cfg())
        .expect("hooked arena");
    let ar = &a;
    std::thread::scope(|s| {
        for t in 0..4u64 {
            s.spawn(move || {
                for i in 0..3000u64 {
                    let arena = if (i + t).is_multiple_of(2) {
                        harena
                    } else {
                        ArenaId::DEFAULT
                    };
                    let sz = 16 + (i as usize % 600);
                    let p = ar.allocate_in(arena, sz, 16, RequestFlags::NONE);
                    if !p.is_null() {
                        // SAFETY: `p` is a live allocation of >= `sz` bytes we own.
                        unsafe {
                            std::ptr::write_bytes(p, 0xA5, sz);
                            assert_eq!(p.read(), 0xA5);
                            assert_eq!(ar.free(p), FreeOutcome::Freed);
                        }
                    }
                }
            });
        }
        // Churn other hooked arenas (no worker allocates from them, so destroy is
        // quiescent for them) — clearing their slots while workers hold a reference
        // into the registry for `harena`.
        s.spawn(move || {
            for _ in 0..16 {
                let (h2, _s2) = leak_hooks();
                if let Ok(id2) = ar.arena_create_hooked(&ArenaPolicy::explicit(), h2, hook_cfg()) {
                    // SAFETY: no other thread allocates from or frees into `id2`.
                    let _ = unsafe { ar.arena_destroy(id2) };
                }
            }
        });
    });
    assert!(
        a.check_invariants(),
        "all backends well-formed after the storm"
    );
    assert_eq!(a.stats().live_bytes, 0, "no leaks: everything freed");
}

// ===========================================================================
// W10 / §23.3: no-overlap ACROSS a hooked arena's two reservations.
// ===========================================================================

/// A custom backing that returns **overlapping** span and large regions: a hooked
/// arena reserves its span region (the first `alloc`) and its large region (the
/// second `alloc`) through two *separate* `HookProvider`s, so neither provider's own
/// tracker sees the other's range. This backing hands both reservations a range that
/// starts at the **same** base — each is page-aligned and ≥ its requested size (so it
/// passes `validate_alloc` and each provider's single-reservation tracker), yet the
/// two ranges alias. Only the cross-region disjointness check in
/// `build_and_register_hook_backend` can catch it (PR-review comment 2): without that
/// check the arena's small-object spans and large allocations would alias.
struct OverlapBacking {
    /// The one shared host block both reservations alias (freed exactly once).
    host: Mutex<Option<(usize, Layout)>>,
    /// Block size — every reservation must fit (asserted in `alloc`).
    block: usize,
}

impl OverlapBacking {
    fn new(block: usize) -> Self {
        let layout = Layout::from_size_align(block, PAGE).unwrap();
        // SAFETY: nonzero size, power-of-two (page) alignment.
        let base = unsafe { alloc(layout) };
        assert!(!base.is_null());
        Self {
            host: Mutex::new(Some((base as usize, layout))),
            block,
        }
    }
    fn base(&self) -> usize {
        self.host
            .lock()
            .unwrap()
            .as_ref()
            .map(|&(b, _)| b)
            .unwrap_or(0)
    }
}

impl ExtentHooks for OverlapBacking {
    fn alloc(
        &self,
        size: usize,
        _align: usize,
        zero: &mut bool,
        commit: &mut bool,
    ) -> Result<Region, BackendError> {
        // Both reserves return ranges starting at the SAME (page-aligned) base, so
        // they OVERLAP — yet each is ≥ `size` and aligned, so `validate_alloc` and
        // each provider's own tracker accept it. The block fits every reservation.
        assert!(size <= self.block, "overlap backing: block too small");
        let base = self.base();
        assert!(base != 0, "overlap backing already released");
        *zero = false;
        *commit = true; // host memory is immediately usable
        Ok(Region {
            base: base as *mut u8,
            len: size,
        })
    }
    fn dealloc(&self, _region: Region, _committed: bool) -> Result<(), BackendError> {
        // Idempotent: the span and large regions ALIAS the one shared block, so free
        // it exactly once — both providers' `Drop` (on the rejected create) land here.
        if let Some((base, layout)) = self.host.lock().unwrap().take() {
            // SAFETY: exactly the block from `new`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
        Ok(())
    }
}

impl Drop for OverlapBacking {
    fn drop(&mut self) {
        if let Some((base, layout)) = self.host.get_mut().unwrap().take() {
            // SAFETY: exactly the block from `new`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
    }
}

/// A modest config for the overlap tests: the test only *creates* the arena (which
/// reserves the two regions) and expects rejection — it never allocates from it — so
/// the regions stay small (well above the 2 MiB huge threshold). Keeps the host
/// backing block tiny, which also lightens this path under the qemu-aarch64 CI run.
fn overlap_cfg() -> AllocatorConfig {
    AllocatorConfig {
        span_region_bytes: 4 * 1024 * 1024,
        span_extent_slots: 64,
        span_slots: 64,
        large_region_bytes: 4 * 1024 * 1024,
        large_extent_slots: 32,
        large_slots: 32,
    }
}

/// Block size for the overlap backing: comfortably covers either 4 MiB reservation.
const OVERLAP_BLOCK: usize = 6 * 1024 * 1024;

#[cfg(not(debug_assertions))]
#[test]
fn hooked_arena_with_overlapping_span_and_large_regions_is_rejected() {
    // §23.3 no-overlap ACROSS the two per-arena reservations (PR-review comment 2):
    // a hook returning overlapping span/large regions would let the arena's small
    // spans and large allocations alias. The cross-region disjointness check rejects
    // it, and both built managers drop (returning the shared block to the hook).
    // (Release: in debug the check `debug_assert!`-aborts — see the companion test.)
    let a = posix_allocator();
    let backing: &'static OverlapBacking = Box::leak(Box::new(OverlapBacking::new(OVERLAP_BLOCK)));
    let r = a.arena_create_hooked(&ArenaPolicy::explicit(), backing, overlap_cfg());
    assert!(
        matches!(r, Err(topo_core::ArenaError::Exhausted)),
        "overlapping span/large regions must be rejected, got {r:?}"
    );
    // The rejected create left the default arena intact and leaked no reservation
    // (the shared block was handed back when the two managers dropped).
    assert!(a.check_invariants());
    assert!(
        backing.base() == 0,
        "the rejected create returned the hook's backing block"
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "span and large regions overlap")]
fn hooked_arena_with_overlapping_regions_debug_aborts() {
    // The debug counterpart of the release reject above: overlapping span/large
    // regions trip the cross-region `debug_assert!` (W10 / comment 2). CI runs tests
    // in debug, so this is the profile that actually exercises the abort. The two
    // built managers drop during unwinding, returning the shared block to the hook.
    let a = posix_allocator();
    let backing: &'static OverlapBacking = Box::leak(Box::new(OverlapBacking::new(OVERLAP_BLOCK)));
    let _ = a.arena_create_hooked(&ArenaPolicy::explicit(), backing, overlap_cfg());
}

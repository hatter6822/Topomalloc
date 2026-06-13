// SPDX-License-Identifier: MIT
//! Extent hooks & custom backing memory (§23, plan 06 W10).
//!
//! Extent hooks let an application supply a **custom memory source** or **custom
//! OS policies** (§23.1) — jemalloc exposes per-arena extent hooks for allocation,
//! deallocation, commit, decommit, purge, split, and merge (SPEC §23.1 ref R4);
//! TopoMalloc adopts the idea but with **explicit contracts** (§23.3) and a single,
//! sound wiring.
//!
//! # The wiring: hooks *are* a backing provider (W10-1)
//!
//! The allocator already routes every OS/kernel interaction through the
//! [`TopoBackingProvider`] seam (§3, §36.6) — that seam **is** the custom-backing
//! abstraction. So [`ExtentHooks`] (the §23.2 interface, in Rust idiom) is adapted
//! to a provider by [`HookProvider`]; an [`ExtentManager`](crate::ExtentManager) or
//! [`Allocator`](crate::Allocator) built over a `HookProvider` then runs the entire
//! proven central path over the custom backing, **with no change above the seam**.
//! The eight §23.2 operations dispatch as:
//!
//! | §23.2 hook | seam method | dispatch |
//! |---|---|---|
//! | `alloc` | [`reserve`](TopoBackingProvider::reserve) | [`ExtentHooks::alloc`] (+ §23.3 validation) |
//! | `dealloc` | [`release`](TopoBackingProvider::release) | [`ExtentHooks::dealloc`] |
//! | `commit` | [`commit`](TopoBackingProvider::commit) | [`ExtentHooks::commit`] |
//! | `decommit` | [`decommit`](TopoBackingProvider::decommit) | [`ExtentHooks::decommit`] |
//! | `purge_lazy` | [`purge_lazy`](TopoBackingProvider::purge_lazy) | [`ExtentHooks::purge_lazy`] |
//! | `purge_forced` | [`purge_forced`](TopoBackingProvider::purge_forced) | [`ExtentHooks::purge_forced`] |
//! | `split` | [`split`](TopoBackingProvider::split) | [`ExtentHooks::split`] |
//! | `merge` | [`merge`](TopoBackingProvider::merge) | [`ExtentHooks::merge`] |
//!
//! The first six are **gating**: they are the allocator's only route to backing,
//! so a hook failure becomes a [`BackendError`] the extent manager already handles
//! while staying well-formed (W4-5). `split`/`merge` are **advisory** notifications
//! (the allocator's [`ExtentMap`](crate::ExtentMap) owns sub-extent geometry — it
//! subdivides the region it `reserve`d without otherwise involving the backing): a
//! failure is *recorded* ([`HookProvider::split_hook_failures`]) but never alters
//! the bookkeeping, so the back-end stays well-formed regardless (W10-3, §23.3
//! "hook failures are reported without corrupting allocator state").
//!
//! # Contracts (§23.3) and conditional correctness (§23.4)
//!
//! [`HookProvider`] enforces the **load-bearing, allocation-free** contracts on the
//! hook's *output* itself, in every build: an [`alloc`](ExtentHooks::alloc) result
//! must be aligned to the request and at least the requested size, and a
//! commit/decommit/purge target must be a sub-range of the reservation (§23.3
//! "returned ranges are aligned / at least requested size", "operations affect only
//! requested subranges"). A violation is rejected with
//! [`BackendError::InvalidRequest`] (so even a buggy hook cannot make the allocator
//! hand out memory that fails the request — *safety before policy*, §2.4) **and**
//! debug-aborts to surface the hook bug loudly. The remaining, *stateful* contracts
//! (no-overlap with live ranges, dealloc pairs a live reservation) are demonstrated
//! by the validating test backings (plan 08 / the W10 integration tests).
//!
//! Per §23.4 the model treats hooks as **abstract operations with contracts**:
//! allocator correctness is *conditional* on hook correctness, proven in Lean
//! (`lean/TopoMalloc/ExtentHooks.lean`). The reentrancy contract (§23.3 "hooks do
//! not call TopoMalloc recursively unless documented reentrant-safe") is inherited
//! from the seam: every hook runs while the back-end extent lock is held (as the
//! existing `commit`/`decommit` provider calls do), so a re-entrant allocator call
//! from inside a hook would deadlock on that non-re-entrant lock — the documented,
//! enforced boundary (explicit recursion detection is a hardened-profile concern,
//! plan 08).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::backend::{Region, TopoBackingProvider};
use crate::error::BackendError;
use crate::ids::ArenaId;

/// The §23.2 extent-hook interface — the Rust idiom of the C `topo_extent_hooks_t`
/// (the C `bool` "true on failure" convention becomes a [`Result`]; the `void* ctx`
/// becomes the hook object's own `&self`). A type implementing it is a **custom
/// backing**: a memory source ([`alloc`](Self::alloc) / [`dealloc`](Self::dealloc))
/// and, optionally, custom OS policies (commit / decommit / purge) and
/// boundary-tracking ([`split`](Self::split) / [`merge`](Self::merge)).
///
/// # Contracts (§23.3) — the implementer's obligation
///
/// * [`alloc`](Self::alloc) returns a range **aligned** to `align` and **at least**
///   `size` bytes, that **does not overlap** any range the allocator already holds
///   (unless deliberately re-handing the same extent);
/// * `commit` / `decommit` / `purge_*` affect **only** the requested sub-range;
/// * [`split`](Self::split) / [`merge`](Self::merge) keep the backing's notion of
///   boundaries matching the allocator's metadata update;
/// * a hook **does not call TopoMalloc recursively** (it runs under the back-end
///   lock — a re-entrant allocator call would deadlock);
/// * a hook **failure is reported** (returned `Err`) **without corrupting** its own
///   state, so the allocator can stay well-formed.
///
/// [`HookProvider`] enforces the cheap, allocation-free output contracts (alignment,
/// size, sub-range) and rejects a violation rather than trusting it (§2.4); the rest
/// are assumed (§23.4, proven conditionally in Lean).
///
/// Only [`alloc`](Self::alloc) and [`dealloc`](Self::dealloc) are required: a
/// minimal custom memory source that hands out committed memory and never reclaims
/// sub-ranges gets correct default `commit`/`decommit`/`purge`/`split`/`merge`.
pub trait ExtentHooks {
    /// Obtain a range of at least `size` bytes aligned to `align` (a power of two),
    /// the §23.2 `alloc` hook. `zero`/`commit` are in/out hints: on input the
    /// allocator's preference (the [`HookProvider`] requests neither — it commits
    /// lazily before use, M-005); on output, whether the hook delivered zeroed /
    /// committed memory. Returns the range, or a [`BackendError`] on failure (the
    /// allocator then fails the allocation cleanly — no memory is fabricated).
    fn alloc(
        &self,
        size: usize,
        align: usize,
        zero: &mut bool,
        commit: &mut bool,
    ) -> Result<Region, BackendError>;

    /// Return `region` (a whole reservation from a prior [`alloc`](Self::alloc)) to
    /// the backing, the §23.2 `dealloc` hook. `committed` is a conservative hint
    /// that the reservation may hold committed pages (the allocator may have
    /// committed and/or decommitted sub-ranges of it); the hook frees the whole
    /// reservation regardless.
    fn dealloc(&self, region: Region, committed: bool) -> Result<(), BackendError>;

    /// Make `[offset, offset+length)` within `region` usable (backed), the §23.2
    /// `commit` hook. Must be idempotent (also the M-005 recommit before reuse).
    /// Default: a no-op (a backing whose [`alloc`](Self::alloc) returns committed,
    /// never-reclaimed memory).
    fn commit(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        let _ = (region, offset, length);
        Ok(())
    }

    /// Drop the physical backing of `[offset, offset+length)`, keeping the range
    /// reserved (the §23.2 `decommit` hook; reuse needs a [`commit`](Self::commit),
    /// M-005). Default: a no-op (the backing never reclaims).
    fn decommit(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        let _ = (region, offset, length);
        Ok(())
    }

    /// Mark `[offset, offset+length)` lazily discardable (the §23.2 `purge_lazy`
    /// hook; the OS may reclaim under pressure). Default: a no-op.
    fn purge_lazy(&self, region: Region, offset: usize, length: usize) -> Result<(), BackendError> {
        let _ = (region, offset, length);
        Ok(())
    }

    /// Discard the contents of `[offset, offset+length)` promptly (the §23.2
    /// `purge_forced` hook; RSS drops now). Default: a no-op.
    fn purge_forced(
        &self,
        region: Region,
        offset: usize,
        length: usize,
    ) -> Result<(), BackendError> {
        let _ = (region, offset, length);
        Ok(())
    }

    /// Note that the extent occupying `region` was split into a prefix of `size_a`
    /// bytes and a suffix of `size_b` bytes (the §23.2 `split` hook; `size_a +
    /// size_b == region.len`). `committed` is whether the extent was fully backed.
    /// **Advisory** (§23.4): the allocator's metadata is authoritative, so a failure
    /// is recorded, not acted on. Default: a no-op (the backing tracks no
    /// sub-extent boundaries).
    fn split(
        &self,
        region: Region,
        size_a: usize,
        size_b: usize,
        committed: bool,
    ) -> Result<(), BackendError> {
        let _ = (region, size_a, size_b, committed);
        Ok(())
    }

    /// Note that the address-adjacent extents `left` and `right` (with
    /// `left.end() == right.base`) were merged into one (the §23.2 `merge` hook).
    /// `committed` is whether both were fully backed. **Advisory** — see
    /// [`split`](Self::split). Default: a no-op.
    fn merge(&self, left: Region, right: Region, committed: bool) -> Result<(), BackendError> {
        let _ = (left, right, committed);
        Ok(())
    }

    /// A short, stable name for diagnostics/traces (the provider [`name`](
    /// TopoBackingProvider::name)). Default: `"hooks"`.
    fn name(&self) -> &'static str {
        "hooks"
    }
}

/// Adapts user-supplied [`ExtentHooks`] to the [`TopoBackingProvider`] seam (W10-1),
/// so the proven allocator core runs unchanged over a custom backing. Construct it
/// with [`new`](Self::new) and pass it to
/// [`ExtentManager::new`](crate::ExtentManager::new) or
/// [`Allocator::new`](crate::Allocator::new), exactly like the POSIX or seLe4n
/// provider.
///
/// It enforces the cheap §23.3 output contracts on every call (rejecting *and*
/// debug-aborting on a violation — §2.4) and counts the advisory `split`/`merge`
/// hook failures for observability ([`split_hook_failures`](Self::split_hook_failures)
/// / [`merge_hook_failures`](Self::merge_hook_failures)).
pub struct HookProvider<H: ExtentHooks> {
    hooks: H,
    /// Count of advisory §23.2 `split`-hook failures (W10-3 observability). These
    /// never alter the bookkeeping — the split already committed in the
    /// allocator's authoritative metadata — but are surfaced so a backing fault is
    /// visible (§23.3 "reported").
    split_hook_failures: AtomicU64,
    /// Count of advisory §23.2 `merge`-hook failures (see above).
    merge_hook_failures: AtomicU64,
}

impl<H: ExtentHooks> HookProvider<H> {
    /// Wrap `hooks` as a backing provider.
    pub const fn new(hooks: H) -> Self {
        Self {
            hooks,
            split_hook_failures: AtomicU64::new(0),
            merge_hook_failures: AtomicU64::new(0),
        }
    }

    /// Borrow the wrapped hooks (e.g. to read a custom backing's own stats).
    #[inline]
    pub fn hooks(&self) -> &H {
        &self.hooks
    }

    /// Number of advisory §23.2 `split`-hook failures recorded (W10-3).
    #[inline]
    pub fn split_hook_failures(&self) -> u64 {
        self.split_hook_failures.load(Ordering::Relaxed)
    }

    /// Number of advisory §23.2 `merge`-hook failures recorded (W10-3).
    #[inline]
    pub fn merge_hook_failures(&self) -> u64 {
        self.merge_hook_failures.load(Ordering::Relaxed)
    }

    /// Validate an [`alloc`](ExtentHooks::alloc) result against the §23.3 output
    /// contracts that are **load-bearing for safety**: the range must be non-null,
    /// aligned to `align`, and at least `size` bytes. A violation is rejected (so a
    /// buggy hook cannot make the allocator hand out memory that does not satisfy
    /// the request — §2.4) and debug-aborts to surface the hook bug. `align` is a
    /// power of two (the caller checked).
    #[inline]
    fn validate_alloc(region: Region, size: usize, align: usize) -> Result<Region, BackendError> {
        let base = region.base as usize;
        let aligned = base & (align - 1) == 0; // align is a power of two
        let big_enough = region.len >= size;
        let non_null = !region.base.is_null();
        debug_assert!(
            non_null,
            "§23.3: extent-hook alloc returned a null range (W10)"
        );
        debug_assert!(
            aligned,
            "§23.3: extent-hook alloc returned a misaligned range (W10)"
        );
        debug_assert!(
            big_enough,
            "§23.3: extent-hook alloc returned a range smaller than requested (W10)"
        );
        if !non_null || !aligned || !big_enough {
            return Err(BackendError::InvalidRequest);
        }
        Ok(region)
    }

    /// Validate that `[offset, offset+len)` is a sub-range of `region` (§23.3
    /// "operations affect only requested subranges") before a commit / decommit /
    /// purge dispatch — the same defense the POSIX/sim providers apply, so a bad
    /// sub-range is rejected rather than passed through (and debug-aborts).
    #[inline]
    fn check_subrange(region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        let ok = offset
            .checked_add(len)
            .map(|end| end <= region.len)
            .unwrap_or(false);
        debug_assert!(ok, "§23.3: extent-hook op outside its range (W10)");
        if ok {
            Ok(())
        } else {
            Err(BackendError::InvalidRequest)
        }
    }
}

impl<H: ExtentHooks> TopoBackingProvider for HookProvider<H> {
    fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(BackendError::InvalidRequest);
        }
        // The extent manager commits before use (M-005), so reserve asks the hook
        // for neither committed nor zeroed memory; a hook is free to deliver either
        // (the out-flags), but the allocator does not depend on it.
        let mut zero = false;
        let mut commit = false;
        let region = self.hooks.alloc(size, align, &mut zero, &mut commit)?;
        // §23.3 / §2.4: never trust the hook's geometry — validate or reject.
        Self::validate_alloc(region, size, align)
    }

    fn commit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        Self::check_subrange(region, offset, len)?;
        self.hooks.commit(region, offset, len)
    }

    fn decommit(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        Self::check_subrange(region, offset, len)?;
        self.hooks.decommit(region, offset, len)
    }

    fn purge_lazy(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        Self::check_subrange(region, offset, len)?;
        self.hooks.purge_lazy(region, offset, len)
    }

    fn purge_forced(&self, region: Region, offset: usize, len: usize) -> Result<(), BackendError> {
        Self::check_subrange(region, offset, len)?;
        self.hooks.purge_forced(region, offset, len)
    }

    fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
        // `release` returns a whole reservation (the manager's region, on Drop);
        // `committed` is the conservative hint that it may hold committed pages.
        self.hooks.dealloc(region, true)
    }

    fn split(
        &self,
        _arena: ArenaId,
        region: Region,
        size_a: usize,
        size_b: usize,
        committed: bool,
    ) -> Result<(), BackendError> {
        // Advisory (§23.4): dispatch and record a failure, but do not propagate it
        // as a fault that could roll back the (already-committed) bookkeeping.
        match self.hooks.split(region, size_a, size_b, committed) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.split_hook_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    fn merge(
        &self,
        _arena: ArenaId,
        left: Region,
        right: Region,
        committed: bool,
    ) -> Result<(), BackendError> {
        match self.hooks.merge(left, right, committed) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.merge_hook_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    fn name(&self) -> &'static str {
        self.hooks.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc as host_alloc, dealloc, Layout};
    use std::sync::atomic::AtomicU32;
    use std::sync::Mutex;

    /// A minimal custom backing for tests: it hands out host allocations (a "custom
    /// memory source", §23.1), records every op so a test can confirm dispatch, and
    /// can be told to fail any specific hook on its next call (W10-3 failure
    /// injection). It self-checks the §23.3 stateful contracts (no-overlap on alloc,
    /// dealloc pairs a live reservation).
    struct TestHooks {
        live: Mutex<Vec<(usize, Layout)>>,
        // op counters (dispatch evidence)
        allocs: AtomicU32,
        deallocs: AtomicU32,
        commits: AtomicU32,
        decommits: AtomicU32,
        purge_lazy: AtomicU32,
        purge_forced: AtomicU32,
        splits: AtomicU32,
        merges: AtomicU32,
        // one-shot failure injection (set the op to fail on its next call)
        fail_alloc: AtomicU32,
        fail_commit: AtomicU32,
        fail_decommit: AtomicU32,
        fail_split: AtomicU32,
        fail_merge: AtomicU32,
    }

    impl TestHooks {
        fn new() -> Self {
            Self {
                live: Mutex::new(Vec::new()),
                allocs: AtomicU32::new(0),
                deallocs: AtomicU32::new(0),
                commits: AtomicU32::new(0),
                decommits: AtomicU32::new(0),
                purge_lazy: AtomicU32::new(0),
                purge_forced: AtomicU32::new(0),
                splits: AtomicU32::new(0),
                merges: AtomicU32::new(0),
                fail_alloc: AtomicU32::new(0),
                fail_commit: AtomicU32::new(0),
                fail_decommit: AtomicU32::new(0),
                fail_split: AtomicU32::new(0),
                fail_merge: AtomicU32::new(0),
            }
        }
        fn armed(slot: &AtomicU32) -> bool {
            slot.swap(0, Ordering::Relaxed) == 1
        }
    }

    impl Drop for TestHooks {
        fn drop(&mut self) {
            for (base, layout) in self.live.get_mut().unwrap().drain(..) {
                // SAFETY: each entry is exactly a pointer/layout from `alloc`.
                unsafe { dealloc(base as *mut u8, layout) };
            }
        }
    }

    impl ExtentHooks for TestHooks {
        fn alloc(
            &self,
            size: usize,
            align: usize,
            zero: &mut bool,
            commit: &mut bool,
        ) -> Result<Region, BackendError> {
            self.allocs.fetch_add(1, Ordering::Relaxed);
            if Self::armed(&self.fail_alloc) {
                return Err(BackendError::OutOfMemory);
            }
            let layout =
                Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
            // SAFETY: nonzero size + valid power-of-two alignment (HookProvider checked).
            let base = unsafe { host_alloc(layout) };
            if base.is_null() {
                return Err(BackendError::OutOfMemory);
            }
            // §23.3 no-overlap self-check: the fresh allocation must not overlap a
            // live reservation. (Host `alloc` guarantees this; we assert it anyway.)
            let mut live = self.live.lock().unwrap();
            let b = base as usize;
            for &(lb, ll) in live.iter() {
                assert!(
                    b + size <= lb || lb + ll.size() <= b,
                    "§23.3 violated: hook alloc overlaps a live reservation"
                );
            }
            live.push((b, layout));
            *zero = false; // host_alloc does not zero
            *commit = true; // host memory is immediately usable
            Ok(Region { base, len: size })
        }
        fn dealloc(&self, region: Region, _committed: bool) -> Result<(), BackendError> {
            self.deallocs.fetch_add(1, Ordering::Relaxed);
            let mut live = self.live.lock().unwrap();
            let base = region.base as usize;
            // §23.3 self-check: dealloc must name a live reservation.
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
            self.commits.fetch_add(1, Ordering::Relaxed);
            if Self::armed(&self.fail_commit) {
                return Err(BackendError::OutOfMemory);
            }
            Ok(())
        }
        fn decommit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            self.decommits.fetch_add(1, Ordering::Relaxed);
            if Self::armed(&self.fail_decommit) {
                return Err(BackendError::OutOfMemory);
            }
            Ok(())
        }
        fn purge_lazy(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            self.purge_lazy.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn purge_forced(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            self.purge_forced.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn split(
            &self,
            _region: Region,
            _a: usize,
            _b: usize,
            _committed: bool,
        ) -> Result<(), BackendError> {
            self.splits.fetch_add(1, Ordering::Relaxed);
            if Self::armed(&self.fail_split) {
                return Err(BackendError::OutOfMemory);
            }
            Ok(())
        }
        fn merge(&self, _l: Region, _r: Region, _committed: bool) -> Result<(), BackendError> {
            self.merges.fetch_add(1, Ordering::Relaxed);
            if Self::armed(&self.fail_merge) {
                return Err(BackendError::OutOfMemory);
            }
            Ok(())
        }
        fn name(&self) -> &'static str {
            "test-hooks"
        }
    }

    #[test]
    fn reserve_dispatches_to_alloc_and_release_to_dealloc() {
        let p = HookProvider::new(TestHooks::new());
        let r = p.reserve(ArenaId::DEFAULT, 4096, 64).expect("reserve");
        assert_eq!(r.len, 4096);
        assert_eq!(r.base as usize % 64, 0);
        assert_eq!(p.hooks().allocs.load(Ordering::Relaxed), 1);
        // commit/decommit/purge dispatch to the hooks.
        p.commit(r, 0, 4096).expect("commit");
        p.decommit(r, 0, 4096).expect("decommit");
        p.purge_lazy(r, 0, 4096).expect("purge_lazy");
        p.purge_forced(r, 0, 4096).expect("purge_forced");
        assert_eq!(p.hooks().commits.load(Ordering::Relaxed), 1);
        assert_eq!(p.hooks().decommits.load(Ordering::Relaxed), 1);
        assert_eq!(p.hooks().purge_lazy.load(Ordering::Relaxed), 1);
        assert_eq!(p.hooks().purge_forced.load(Ordering::Relaxed), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
        assert_eq!(p.hooks().deallocs.load(Ordering::Relaxed), 1);
        assert_eq!(p.name(), "test-hooks");
    }

    #[test]
    fn split_merge_dispatch_and_count_failures() {
        let p = HookProvider::new(TestHooks::new());
        let r = p.reserve(ArenaId::DEFAULT, 8192, 64).expect("reserve");
        let left = Region {
            base: r.base,
            len: 4096,
        };
        // SAFETY: offset 4096 is in bounds of the 8192-byte reservation `r`.
        let right_base = unsafe { r.base.add(4096) };
        let right = Region {
            base: right_base,
            len: 4096,
        };
        // Successful dispatch.
        p.split(ArenaId::DEFAULT, r, 4096, 4096, true)
            .expect("split");
        p.merge(ArenaId::DEFAULT, left, right, true).expect("merge");
        assert_eq!(p.hooks().splits.load(Ordering::Relaxed), 1);
        assert_eq!(p.hooks().merges.load(Ordering::Relaxed), 1);
        assert_eq!(p.split_hook_failures(), 0);
        assert_eq!(p.merge_hook_failures(), 0);
        // An injected failure is reported AND counted (advisory; §23.3).
        p.hooks().fail_split.store(1, Ordering::Relaxed);
        assert!(p.split(ArenaId::DEFAULT, r, 4096, 4096, true).is_err());
        assert_eq!(p.split_hook_failures(), 1);
        p.hooks().fail_merge.store(1, Ordering::Relaxed);
        assert!(p.merge(ArenaId::DEFAULT, left, right, true).is_err());
        assert_eq!(p.merge_hook_failures(), 1);
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn reserve_rejects_zero_size_and_bad_align() {
        let p = HookProvider::new(TestHooks::new());
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 0, 64),
            Err(BackendError::InvalidRequest)
        ));
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 64, 24),
            Err(BackendError::InvalidRequest)
        ));
        // The hook was never called for the malformed requests.
        assert_eq!(p.hooks().allocs.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn alloc_hook_failure_propagates_as_backend_error() {
        let p = HookProvider::new(TestHooks::new());
        p.hooks().fail_alloc.store(1, Ordering::Relaxed);
        assert!(matches!(
            p.reserve(ArenaId::DEFAULT, 4096, 64),
            Err(BackendError::OutOfMemory)
        ));
        // A later reserve succeeds (one-shot failure).
        let r = p.reserve(ArenaId::DEFAULT, 4096, 64).expect("reserve");
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn commit_hook_failure_propagates() {
        let p = HookProvider::new(TestHooks::new());
        let r = p.reserve(ArenaId::DEFAULT, 4096, 64).expect("reserve");
        p.hooks().fail_commit.store(1, Ordering::Relaxed);
        assert_eq!(
            p.commit(r, 0, 4096),
            Err(BackendError::OutOfMemory),
            "a failing commit hook surfaces as a BackendError (gated, W10-3)"
        );
        p.release(ArenaId::DEFAULT, r).expect("release");
    }

    #[test]
    fn subrange_ops_reject_out_of_range_targets() {
        let p = HookProvider::new(TestHooks::new());
        let r = p.reserve(ArenaId::DEFAULT, 4096, 64).expect("reserve");
        // §23.3 subrange-only: an out-of-range commit/decommit/purge is rejected
        // before the hook is reached. (Run only in release: in debug the contract
        // violation aborts via `debug_assert!`, which is the point — caught loudly.)
        #[cfg(not(debug_assertions))]
        {
            assert_eq!(p.commit(r, 4096, 1), Err(BackendError::InvalidRequest));
            assert_eq!(
                p.decommit(r, 0, usize::MAX),
                Err(BackendError::InvalidRequest)
            );
            // The hook was not reached for the rejected ops.
            assert_eq!(p.hooks().commits.load(Ordering::Relaxed), 0);
        }
        p.release(ArenaId::DEFAULT, r).expect("release");
    }
}

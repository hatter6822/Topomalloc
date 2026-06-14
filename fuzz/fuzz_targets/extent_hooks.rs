// SPDX-License-Identifier: MIT
//! Fuzz target for extent hooks & custom backing (plan 06 W10, §23, §34.8). The
//! property: under **any** sequence of carve/free operations over a `HookProvider`
//! whose user hooks fail arbitrarily (commit / decommit / split / merge), the
//! back-end extent manager stays well-formed — `check_invariants` holds after every
//! step (W10-3: "fuzzed hook failures never corrupt state"). The §23.3 "hook
//! failures are reported without corrupting allocator state" property, fuzzed.
//!
//! The first bytes of the input choose the per-hook failure periods; the rest drive
//! the carve/free op stream. A single counterexample (a torn carve rollback, a bad
//! coalesce, an advisory split/merge failure that leaked into the bookkeeping)
//! trips `check_invariants` and the fuzzer reports it.
#![no_main]

use std::alloc::{alloc, dealloc, Layout};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;

use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{
    ArenaId, BackendError, ExtentHooks, ExtentManager, ExtentRef, Fit, HookProvider, Region,
    RetainPolicy,
};

const PAGE: usize = PAGE_SIZE;
const REGION_PAGES: usize = 128;
const SLOTS: usize = 1024;

/// Shared failure controls + a live-reservation set for a host-backed custom
/// backing. A hook fails every `period`-th call (`0` ⇒ never).
#[derive(Default)]
struct FuzzCtl {
    fail_commit: AtomicU32,
    fail_decommit: AtomicU32,
    fail_split: AtomicU32,
    fail_merge: AtomicU32,
    commit_calls: AtomicU64,
    decommit_calls: AtomicU64,
    split_calls: AtomicU64,
    merge_calls: AtomicU64,
}

impl FuzzCtl {
    fn trips(period: &AtomicU32, calls: &AtomicU64) -> bool {
        let p = period.load(Ordering::Relaxed) as u64;
        if p == 0 {
            return false;
        }
        (calls.fetch_add(1, Ordering::Relaxed) + 1).is_multiple_of(p)
    }
}

struct FuzzHooks {
    ctl: Arc<FuzzCtl>,
    live: Mutex<Vec<(usize, Layout)>>,
}

impl Drop for FuzzHooks {
    fn drop(&mut self) {
        for (base, layout) in self.live.get_mut().unwrap().drain(..) {
            // SAFETY: each entry is exactly a pointer/layout from `alloc`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
    }
}

impl ExtentHooks for FuzzHooks {
    fn alloc(
        &self,
        size: usize,
        align: usize,
        _zero: &mut bool,
        commit: &mut bool,
    ) -> Result<Region, BackendError> {
        let layout =
            Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
        // SAFETY: nonzero size + power-of-two alignment (HookProvider checked).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        self.live.lock().unwrap().push((base as usize, layout));
        *commit = true;
        Ok(Region { base, len: size })
    }
    fn dealloc(&self, region: Region, _committed: bool) -> Result<(), BackendError> {
        let mut live = self.live.lock().unwrap();
        let base = region.base as usize;
        if let Some(idx) = live.iter().position(|&(b, _)| b == base) {
            let (_, layout) = live.swap_remove(idx);
            // SAFETY: exactly the pointer/layout from the matching `alloc`.
            unsafe { dealloc(base as *mut u8, layout) };
        }
        Ok(())
    }
    fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
        if FuzzCtl::trips(&self.ctl.fail_commit, &self.ctl.commit_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }
    fn decommit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
        if FuzzCtl::trips(&self.ctl.fail_decommit, &self.ctl.decommit_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }
    fn split(&self, _r: Region, _a: usize, _b: usize, _c: bool) -> Result<(), BackendError> {
        if FuzzCtl::trips(&self.ctl.fail_split, &self.ctl.split_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }
    fn merge(&self, _l: Region, _r: Region, _c: bool) -> Result<(), BackendError> {
        if FuzzCtl::trips(&self.ctl.fail_merge, &self.ctl.merge_calls) {
            return Err(BackendError::OutOfMemory);
        }
        Ok(())
    }
    fn name(&self) -> &'static str {
        "fuzz-hooks"
    }
}

fuzz_target!(|data: &[u8]| {
    // A fresh metadata arena per input (freed when this input returns).
    let mut buf = vec![0u8; 1 << 18];
    // SAFETY: `buf` outlives `arena`/`mgr` (dropped at the end of this closure) and
    // is not moved or aliased elsewhere.
    let arena = unsafe { topo_core::bootstrap::BumpArena::new(buf.as_mut_ptr(), buf.len()) };

    let ctl = Arc::new(FuzzCtl::default());
    let provider = HookProvider::new(FuzzHooks {
        ctl: ctl.clone(),
        live: Mutex::new(Vec::new()),
    });
    // Construct cleanly (the region reserve must succeed), then arm the runtime
    // hooks from the first input bytes (period in 0..=4; 0 disables that hook).
    let mut mgr = match ExtentManager::new(
        provider,
        &arena,
        ArenaId::DEFAULT,
        REGION_PAGES * PAGE,
        PAGE,
        SLOTS,
    ) {
        Ok(m) => m,
        Err(_) => return,
    };
    mgr.set_retain_policy(RetainPolicy::Unmap);
    let p = |i: usize| -> u32 { data.get(i).map(|b| (b % 5) as u32).unwrap_or(0) };
    ctl.fail_commit.store(p(0), Ordering::Relaxed);
    ctl.fail_decommit.store(p(1), Ordering::Relaxed);
    ctl.fail_split.store(p(2), Ordering::Relaxed);
    ctl.fail_merge.store(p(3), Ordering::Relaxed);

    let mut live: Vec<ExtentRef> = Vec::new();
    for &b in data.iter().skip(4) {
        match b % 3 {
            0 | 1 => {
                let pages = (b / 3) as usize % 8 + 1;
                if let Ok(r) = mgr.alloc(pages * PAGE, PAGE, Fit::Best) {
                    live.push(r);
                }
            }
            _ => {
                if !live.is_empty() {
                    let i = (b as usize) % live.len();
                    let r = live.swap_remove(i);
                    let _ = mgr.free(r);
                }
            }
        }
        // The W10-3 invariant must hold after *every* operation, whatever failed.
        assert!(mgr.check_invariants(), "hook-failure corrupted the back-end");
    }
});

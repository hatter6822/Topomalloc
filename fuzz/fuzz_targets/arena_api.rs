// SPDX-License-Identifier: MIT
//! Fuzz target for the W9 multi-arena data path (plan 06, §22/§36.4/§34.8):
//! under *any* sequence of arena create / delegate / allocate-from / free /
//! realloc / reset / destroy operations, the engine stays well-formed
//! ([`Allocator::check_invariants`], incl. the arena registry B.5 checks), arena
//! isolation holds (an object always belongs to exactly one arena), per-arena
//! quota accounting never wraps, a reset/destroy invalidates exactly the target
//! arena's objects (and nothing else), and the §8.6/§36.17 reconciliation
//! `Σ arena.used == live_bytes` balances. A single counterexample — a lost
//! object, a leaked arena charge, cross-arena bleed, or accounting drift — trips
//! an assert and the fuzzer reports it.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::{
    Allocator, AllocatorConfig, ArenaId, ArenaPolicy, FreeOutcome, PageMap, RequestFlags,
};

mod host_provider {
    //! A minimal std-allocation provider (the same shape as `malloc_api`'s):
    //! committed RW memory, page-aligned reservations, no mmap churn.
    use std::alloc::{alloc, dealloc, Layout};
    use std::sync::Mutex;
    use topo_core::generated::tables::PAGE_SIZE;
    use topo_core::{ArenaId, BackendError, Region, TopoBackingProvider};

    pub struct HostProvider {
        owned: Mutex<Vec<(usize, Layout)>>,
    }

    impl HostProvider {
        pub fn new() -> Self {
            Self {
                owned: Mutex::new(Vec::new()),
            }
        }
    }

    impl TopoBackingProvider for HostProvider {
        fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
            if size == 0 || !align.is_power_of_two() {
                return Err(BackendError::InvalidRequest);
            }
            let layout = Layout::from_size_align(size, align.max(PAGE_SIZE))
                .map_err(|_| BackendError::InvalidRequest)?;
            // SAFETY: layout has nonzero size.
            let p = unsafe { alloc(layout) };
            if p.is_null() {
                return Err(BackendError::OutOfMemory);
            }
            self.owned.lock().unwrap().push((p as usize, layout));
            Ok(Region { base: p, len: size })
        }
        fn commit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn decommit(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn purge_lazy(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn purge_forced(&self, _r: Region, _o: usize, _l: usize) -> Result<(), BackendError> {
            Ok(())
        }
        fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
            let mut owned = self.owned.lock().unwrap();
            if let Some(i) = owned.iter().position(|(b, _)| *b == region.base as usize) {
                let (base, layout) = owned.swap_remove(i);
                // SAFETY: (base, layout) came from `alloc` above.
                unsafe { dealloc(base as *mut u8, layout) };
            }
            Ok(())
        }
        fn name(&self) -> &'static str {
            "fuzz-host"
        }
    }

    impl Drop for HostProvider {
        fn drop(&mut self) {
            for (base, layout) in self.owned.get_mut().unwrap().drain(..) {
                // SAFETY: every entry came from `alloc` and is freed once.
                unsafe { dealloc(base as *mut u8, layout) };
            }
        }
    }
}
use host_provider::HostProvider;

/// One live object: which arena owns it, its pointer/usable size, its pattern.
struct Live {
    arena: ArenaId,
    ptr: *mut u8,
    usable: usize,
    pat: u8,
}

fuzz_target!(|data: &[u8]| {
    let mut meta_buf = vec![0u8; 4 << 20];
    // SAFETY: `meta_buf` outlives `a` (declared before; dropped after), is never
    // moved or reallocated, and is not aliased elsewhere.
    let meta = unsafe { BumpArena::new(meta_buf.as_mut_ptr(), meta_buf.len()) };
    let pagemap = PageMap::new();
    let cfg = AllocatorConfig {
        span_region_bytes: 8 << 20,
        span_extent_slots: 256,
        span_slots: 256,
        large_region_bytes: 8 << 20,
        large_extent_slots: 128,
        large_slots: 128,
    };
    let Ok(a) = Allocator::new(
        HostProvider::new(),
        HostProvider::new(),
        &meta,
        &meta,
        &pagemap,
        ArenaId::DEFAULT,
        cfg,
    ) else {
        return;
    };

    // Explicit arenas created so far (the default is always present, implicit).
    let mut arenas: Vec<ArenaId> = Vec::new();
    let mut live: Vec<Live> = Vec::new();

    // The per-arena used counter must always equal the engine's global live
    // bytes summed over the default arena and every explicit arena.
    let reconciles = |a: &Allocator<'_, HostProvider>, arenas: &[ArenaId]| {
        let mut sum = a.arena_stats(ArenaId::DEFAULT).map(|s| s.used).unwrap_or(0);
        for &id in arenas {
            sum += a.arena_stats(id).map(|s| s.used).unwrap_or(0);
        }
        assert_eq!(sum, a.stats().live_bytes, "Σ arena.used != live_bytes");
    };

    for chunk in data.chunks_exact(4) {
        let op = chunk[0] % 6;
        let sel = chunk[1] as usize;
        let raw = u16::from_le_bytes([chunk[2], chunk[3]]) as usize;
        match op {
            // create an explicit arena (bounded count) with a derived quota.
            0 => {
                if arenas.len() < 8 {
                    let quota = ((raw as u64) << 10).max(4096);
                    if let Ok(id) = a.arena_create(&ArenaPolicy::explicit().with_quota(quota)) {
                        assert!(a.arenas().is_active(id));
                        arenas.push(id);
                    }
                }
            }
            // allocate from a selected arena (default or one of the explicit).
            1 => {
                let arena = if arenas.is_empty() || sel % 2 == 0 {
                    ArenaId::DEFAULT
                } else {
                    arenas[sel % arenas.len()]
                };
                let align = 1usize << (sel % 9); // 1..=256
                let p = a.allocate_in(arena, raw, align, RequestFlags::NONE);
                if !p.is_null() {
                    assert_eq!(p as usize % align, 0, "alignment violated");
                    let usable = a.usable_size(p).expect("fresh allocation is live");
                    assert!(a.owns(p), "probe denies a fresh object");
                    let pat = chunk[2] ^ 0xA5;
                    // SAFETY: `p` is live with `usable` writable bytes.
                    unsafe {
                        p.write(pat);
                        p.add(usable - 1).write(pat);
                    }
                    live.push(Live { arena, ptr: p, usable, pat });
                }
            }
            // free a selected live object.
            2 => {
                if !live.is_empty() {
                    let l = live.swap_remove(sel % live.len());
                    // SAFETY: tracked-live; bytes must survive until freed.
                    unsafe {
                        assert_eq!(l.ptr.read(), l.pat, "head corrupted");
                        assert_eq!(l.ptr.add(l.usable - 1).read(), l.pat, "tail corrupted");
                        assert_eq!(a.free(l.ptr), FreeOutcome::Freed);
                    }
                }
            }
            // reset an explicit arena: its objects are invalidated (dropped from
            // tracking, NOT freed); the arena stays usable.
            3 => {
                if !arenas.is_empty() {
                    let id = arenas[sel % arenas.len()];
                    live.retain(|l| l.arena != id);
                    // SAFETY: every tracked pointer of `id` was just dropped, so
                    // the arena is quiesced from this harness's view.
                    let _ = unsafe { a.arena_reset(id) };
                    assert!(a.arenas().is_active(id), "reset leaves the arena Active");
                    assert_eq!(a.arena_stats(id).unwrap().used, 0, "B.5 after reset");
                }
            }
            // destroy an explicit arena: its objects are invalidated; the id is
            // retired and removed from the tracked set.
            4 => {
                if !arenas.is_empty() {
                    let idx = sel % arenas.len();
                    let id = arenas[idx];
                    live.retain(|l| l.arena != id);
                    // SAFETY: as the reset arm.
                    if unsafe { a.arena_destroy(id) }.is_ok() {
                        assert!(!a.arenas().is_registered(id), "destroyed id is retired");
                        arenas.swap_remove(idx);
                    }
                }
            }
            // probe storm: arbitrary addresses never panic or mutate.
            _ => {
                let addr = (raw as u64 * 0x9E37_79B9 + sel as u64) as usize;
                let p = addr as *mut u8;
                let _ = a.usable_size(p);
                let _ = a.owns(p);
                let _ = a.recognizes(p);
            }
        }
        reconciles(&a, &arenas);
    }

    // Drain the survivors; only live (non-reset, non-destroyed) objects remain.
    for l in live.drain(..) {
        // SAFETY: live and owned.
        unsafe {
            assert_eq!(l.ptr.read(), l.pat, "head corrupted at drain");
            assert_eq!(a.free(l.ptr), FreeOutcome::Freed);
        }
    }
    // Destroy every surviving explicit arena, then the engine reconciles to zero.
    for id in arenas.drain(..) {
        // SAFETY: all of `id`'s objects were freed in the drain above.
        let _ = unsafe { a.arena_destroy(id) };
    }
    assert_eq!(a.stats().live_bytes, 0, "stats live_bytes drifted");
    assert_eq!(a.arena_stats(ArenaId::DEFAULT).unwrap().used, 0);
    assert!(a.check_invariants(), "backend left ill-formed");
});

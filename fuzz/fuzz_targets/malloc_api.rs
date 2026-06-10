// SPDX-License-Identifier: MIT
//! Fuzz target for the W8 M1 allocator (plan 06, §34.8): under *any* sequence
//! of allocate / free / realloc / usable-size operations — with adversarial
//! sizes, alignments, and flag hints — the engine stays well-formed
//! ([`Allocator::check_invariants`]), every handed-out object is writable for
//! its full usable size, content survives realloc per §25.1, frees recycle
//! exactly once, the liveness probes agree with the model's ownership, and
//! the stats identities (§8.6) balance at the end. A single counterexample
//! (lost object, torn span retirement, accounting drift, probe lying) trips
//! an assert and the fuzzer reports it.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::{Allocator, AllocatorConfig, ArenaId, FreeOutcome, PageMap, RequestFlags};

mod host_provider {
    //! A minimal std-allocation provider (the same shape as the engine's own
    //! unit-test provider): committed RW memory, page-aligned reservations —
    //! and no mmap churn, so the fuzzer's iteration rate stays high.
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
        fn reserve(
            &self,
            _arena: ArenaId,
            size: usize,
            align: usize,
        ) -> Result<Region, BackendError> {
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

/// One live object the model tracks: its pointer, its usable size, and the
/// pattern byte its first/last bytes carry.
struct Live {
    ptr: *mut u8,
    usable: usize,
    pat: u8,
}

fn stamp(p: *mut u8, usable: usize, pat: u8) {
    // SAFETY: `p` is live with `usable` writable bytes (asserted by callers).
    unsafe {
        p.write(pat);
        p.add(usable - 1).write(pat);
    }
}

fn check(l: &Live) {
    // SAFETY: tracked-live object; its stamped bytes must survive.
    unsafe {
        assert_eq!(l.ptr.read(), l.pat, "head byte corrupted");
        assert_eq!(l.ptr.add(l.usable - 1).read(), l.pat, "tail byte corrupted");
    }
}

fuzz_target!(|data: &[u8]| {
    // A fresh, fully scoped engine per input: `MetaArena`-style ownership is
    // modeled by keeping the metadata buffer alive past the allocator (drop
    // order below), and the providers reclaim on drop, so a long campaign
    // does not leak.
    let mut meta_buf = vec![0u8; 4 << 20];
    // SAFETY: `meta_buf` outlives `a` (declared before it; dropped after),
    // is never moved or reallocated, and is not aliased elsewhere.
    let meta = unsafe { BumpArena::new(meta_buf.as_mut_ptr(), meta_buf.len()) };
    let pagemap = PageMap::new();
    let cfg = AllocatorConfig {
        span_region_bytes: 4 << 20,
        span_extent_slots: 128,
        span_slots: 128,
        large_region_bytes: 8 << 20,
        large_extent_slots: 64,
        large_slots: 64,
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

    let mut live: Vec<Live> = Vec::new();
    let mut expected_live_bytes = 0u64;
    let mut it = data.chunks_exact(4);
    for chunk in &mut it {
        let op = chunk[0] % 4;
        let sel = chunk[1] as usize;
        let raw = u16::from_le_bytes([chunk[2], chunk[3]]) as usize;
        match op {
            // allocate: sizes 0..=65535 cross zero-size, every small class,
            // and the medium boundary; alignment from the selector's low bits.
            0 => {
                let align = 1usize << (sel % 13); // 1..=4096
                let p = a.allocate(raw, align, RequestFlags::NONE);
                if !p.is_null() {
                    assert_eq!(p as usize % align, 0, "alignment violated");
                    let usable = a.usable_size(p).expect("fresh allocation is live");
                    assert!(usable >= raw.max(1), "usable below request");
                    assert!(a.owns(p) && a.recognizes(p), "probes deny a fresh object");
                    let pat = chunk[2] ^ 0xA5;
                    stamp(p, usable, pat);
                    expected_live_bytes += usable as u64;
                    live.push(Live { ptr: p, usable, pat });
                }
            }
            // free: content intact, recycled exactly once, probes flip.
            1 => {
                if !live.is_empty() {
                    let l = live.swap_remove(sel % live.len());
                    check(&l);
                    // SAFETY: `l.ptr` is live and owned by this harness.
                    assert_eq!(unsafe { a.free(l.ptr) }, FreeOutcome::Freed);
                    expected_live_bytes -= l.usable as u64;
                    assert!(!a.owns(l.ptr), "freed object still owned");
                    assert_eq!(a.usable_size(l.ptr), None, "freed object has usable");
                }
            }
            // realloc: §25.1 — content preserved to min(old_usable, new),
            // failure preserves the original.
            2 => {
                if !live.is_empty() {
                    let i = sel % live.len();
                    let old = &live[i];
                    check(old);
                    let (old_ptr, old_usable, pat) = (old.ptr, old.usable, old.pat);
                    // SAFETY: `old_ptr` is live and owned; ownership moves to
                    // the result on success, stays on failure (§25.1).
                    let q = unsafe { a.realloc(old_ptr, raw, 16, RequestFlags::NONE) };
                    if q.is_null() {
                        check(&live[i]); // original intact after failure
                    } else {
                        let usable = a.usable_size(q).expect("realloc result is live");
                        assert!(usable >= raw.max(1));
                        if raw.max(1) >= 1 && old_usable >= 1 {
                            // The head byte is within min(old_usable, new).
                            // SAFETY: `q` is live with >= 1 usable byte.
                            unsafe { assert_eq!(q.read(), pat, "realloc lost content") };
                        }
                        expected_live_bytes -= old_usable as u64;
                        expected_live_bytes += usable as u64;
                        let pat = pat ^ 0x3C;
                        stamp(q, usable, pat);
                        live[i] = Live { ptr: q, usable, pat };
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
    }

    // Drain: every tracked object is intact and frees exactly once.
    for l in live.drain(..) {
        check(&l);
        // SAFETY: live and owned.
        assert_eq!(unsafe { a.free(l.ptr) }, FreeOutcome::Freed);
        expected_live_bytes -= l.usable as u64;
    }
    assert_eq!(expected_live_bytes, 0);

    // §8.6: the engine's own accounting reconciles with the model.
    let s = a.stats();
    assert_eq!(s.live_bytes, 0, "stats live_bytes drifted");
    assert_eq!(s.allocated_bytes_total, s.freed_bytes_total);
    assert!(a.check_invariants(), "backend left ill-formed");
});

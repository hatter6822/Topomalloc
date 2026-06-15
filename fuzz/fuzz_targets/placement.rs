// SPDX-License-Identifier: MIT
//! Fuzz target for the lifetime/hotness placement policy + the W17-3 sampling machinery
//! (§24, plan 07 W14). Two intertwined properties hold under *any* operation stream:
//!
//! 1. **The §24.5 safety boundary (the fixed wall).** A [`SiteProfileTable`] fed arbitrary
//!    (adversarial) observations distils to a [`PlaceHints`]; feeding that learned hint
//!    into the [`HugePageFiller`] never changes a placed run's geometry — a run is always
//!    exactly the requested page count, page-aligned, in-bounds — and the filler stays
//!    well-formed (`check_invariants`). A learned profile can hurt fragmentation, never
//!    safety.
//! 2. **Totality / bounded state.** Every table / sampled-set / sampler operation is total
//!    (never panics), the profile table never exceeds its capacity, and the sampled-object
//!    set's accounting stays consistent across alloc/free churn.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_core::bootstrap::BumpArena;
use topo_core::generated::tables::PAGE_SIZE;
use topo_core::{
    HugePageFiller, SampleConfig, SampledObjects, SampledRecord, Sampler, SiteProfileTable, StackId,
    HUGEPAGE_SIZE, PAGES_PER_HUGEPAGE,
};

const PAGE: usize = PAGE_SIZE;
const CAPACITY: usize = 4; // hugepages in the filler
const REGION_BASE: usize = HUGEPAGE_SIZE * 16;
const PROFILE_CAP: usize = 16; // power of two
const SAMPLE_CAP: usize = 16; // power of two

fuzz_target!(|data: &[u8]| {
    let mut buf = vec![0u8; 1 << 18];
    // SAFETY: `buf` outlives `arena`/`filler` (both drop at closure end, filler first) and
    // is neither moved nor aliased while they borrow it.
    let arena = unsafe { BumpArena::new(buf.as_mut_ptr(), buf.len()) };
    let mut filler = match HugePageFiller::new(&arena, REGION_BASE, CAPACITY) {
        Some(f) => f,
        None => return,
    };

    let mut table: SiteProfileTable<PROFILE_CAP> = SiteProfileTable::new();
    let mut objects: SampledObjects<SAMPLE_CAP> = SampledObjects::new();
    let mut sampler = Sampler::new(
        SampleConfig {
            sample_rate_bytes: 4096,
        },
        0xF1F0,
    );

    // A monotone-ish logical clock from the byte stream.
    let mut now: u64 = 0;
    // Live filler runs so frees target real allocations.
    let mut live: Vec<(usize, usize)> = Vec::new();
    // Live sampled object addresses (synthetic, page-spaced) for the sampled set.
    let mut sampled: Vec<usize> = Vec::new();

    let mut it = data.iter().copied();
    while let Some(b) = it.next() {
        now = now.wrapping_add((b as u64 & 0x0f) + 1);
        let site = StackId((b as u64 & 0x07) + 1); // a small set of sites ⇒ real churn/eviction
        match b % 8 {
            // Record a sampled allocation observation.
            0 => {
                let bucket = (b as u16) % 80;
                let bytes = ((b as u64) << 4) + 1;
                table.record_alloc(site, bucket, bytes, b, now);
            }
            // Record a completed free (a measured lifetime).
            1 => {
                let age = (b as u64) * 37;
                table.record_free(site, age, ((b as u64) << 4) + 1, now);
            }
            // Record a right-censored (still-live) lifetime.
            2 => {
                table.record_censored(site, (b as u64) * 101);
            }
            // The fixed wall: a *learned* hint must not change placed-run geometry.
            3 | 4 => {
                let hints = table.place_hints(site);
                let pages = (b as usize % PAGES_PER_HUGEPAGE) + 1;
                let align = PAGE << ((b >> 6) & 0b1); // PAGE or 2*PAGE
                if let Some(p) = filler.place(pages, align, hints) {
                    assert_eq!(p.pages, pages, "learned hint changed the run length");
                    assert_eq!(p.base % align, 0, "learned hint changed alignment");
                    let off = p.base % HUGEPAGE_SIZE;
                    assert!(
                        off + pages * PAGE <= HUGEPAGE_SIZE,
                        "run escaped its hugepage"
                    );
                    filler.mark_committed(&p);
                    live.push((p.base, p.pages));
                }
            }
            // Free a live filler run.
            5 => {
                if !live.is_empty() {
                    let i = (b as usize) % live.len();
                    let (base, pages) = live.swap_remove(i);
                    assert!(filler.free(base, pages).valid);
                }
            }
            // Drive the lock-free sampling decision + the sampled-object set.
            6 => {
                let _ = sampler.should_sample(b as usize * 64);
                // A synthetic, never-zero address.
                let addr = REGION_BASE + (sampled.len() + 1) * PAGE;
                if objects.on_alloc(
                    addr,
                    SampledRecord {
                        stack_id: site,
                        bytes: (b as u64) + 1,
                        alloc_ms: now,
                    },
                ) {
                    sampled.push(addr);
                }
            }
            // Free a sampled object (resolves a lifetime into the table).
            _ => {
                if !sampled.is_empty() {
                    let i = (b as usize) % sampled.len();
                    let addr = sampled.swap_remove(i);
                    if let Some(rec) = objects.on_free(addr) {
                        let age = now.saturating_sub(rec.alloc_ms);
                        table.record_free(rec.stack_id, age, rec.bytes, now);
                    }
                }
            }
        }

        // Invariants after every op: the filler stays well-formed, the profile table never
        // exceeds capacity, and place_hints is total for any key.
        assert!(filler.check_invariants(), "filler invariant violated");
        assert!(table.stats().sites_tracked as usize <= PROFILE_CAP);
        let _ = table.place_hints(StackId(b as u64)); // total on any key
        assert!(objects.len() <= SAMPLE_CAP);
    }

    // Draining the live filler runs keeps it well-formed and leaks nothing.
    for (base, pages) in live {
        assert!(filler.free(base, pages).valid);
        assert!(filler.check_invariants());
    }
});

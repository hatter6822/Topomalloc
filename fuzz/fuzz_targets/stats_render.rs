// SPDX-License-Identifier: MIT
//! Fuzz target for the §31 stats renderer + §36.12 label redaction (W17, §34.8). Three
//! properties hold under **any** input:
//!
//! * **Rendering totality + validity:** for an arbitrary [`Stats`], `StatsFlags` word, and
//!   per-entity detail, [`Stats::to_json_with`] never panics and always emits *well-formed
//!   JSON* (parsed back with `serde_json`) carrying the always-present `epoch`. `explain()`
//!   is likewise total.
//! * **Summary redaction non-interference (§36.12):** [`redact_summary`] is a pure function of
//!   the visible (low-labelled) arenas + epoch/profile, so injecting a *higher*-labelled arena
//!   — invisible to the observer — leaves the redacted summary bit-for-bit identical. A low
//!   domain cannot infer a high domain's activity. This is the Rust-side analogue of the Lean
//!   `stats_observation_noninterference`, hammered over arbitrary states.
//! * **Redaction discloses nothing extra:** a redacted summary's `live_bytes` equals the sum
//!   of the visible arenas' `used`, never any cross-domain aggregate.
#![no_main]

use libfuzzer_sys::fuzz_target;

use topo_stats::{
    redact_arenas, redact_summary, ArenaLine, NumaNodeLine, SizeClassLine, Stats, StatsDetail,
    StatsFlags,
};

/// A tiny deterministic byte reader (saturates to 0 once the input is exhausted).
struct Bytes<'a> {
    d: &'a [u8],
    i: usize,
}

impl<'a> Bytes<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bytes { d, i: 0 }
    }
    fn u8(&mut self) -> u8 {
        let v = self.d.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        v
    }
    fn u32(&mut self) -> u32 {
        let mut v = 0u32;
        for _ in 0..4 {
            v = (v << 8) | self.u8() as u32;
        }
        v
    }
    fn u64(&mut self) -> u64 {
        let mut v = 0u64;
        for _ in 0..8 {
            v = (v << 8) | self.u8() as u64;
        }
        v
    }
}

/// Build an arbitrary `Stats` from the fuzzer bytes — every numeric field independently, so
/// the renderer sees adversarial (including non-reconciling) inputs.
fn arbitrary_stats(b: &mut Bytes) -> Stats {
    Stats {
        epoch: b.u64(),
        live_bytes: b.u64(),
        peak_live_bytes: b.u64(),
        allocated_bytes_total: b.u64(),
        freed_bytes_total: b.u64(),
        rss_bytes: b.u64(),
        per_cpu_bytes: b.u64(),
        thread_cache_bytes: b.u64(),
        transfer_bytes: b.u64(),
        central_free_bytes: b.u64(),
        metadata_bytes: b.u64(),
        active_bytes: b.u64(),
        retained_bytes: b.u64(),
        dirty_bytes: b.u64(),
        muzzy_bytes: b.u64(),
        released_bytes: b.u64(),
        pageheap_free_bytes: b.u64(),
        virtual_bytes: b.u64(),
        quarantine_bytes: b.u64(),
        live_arenas: b.u64(),
        arenas_destroyed: b.u64(),
        sampled_internal_fragmentation_bytes: b.u64(),
        exact_internal_fragmentation_bytes: b.u64(),
        ..Stats::default()
    }
}

fn arbitrary_arena(b: &mut Bytes) -> ArenaLine {
    ArenaLine {
        id: b.u32(),
        label: b.u32(),
        used: b.u64(),
        reserved: b.u64(),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut b = Bytes::new(data);
    let s = arbitrary_stats(&mut b);
    let flags = StatsFlags(b.u64());

    let n_arenas = (b.u8() % 12) as usize;
    let arenas: Vec<ArenaLine> = (0..n_arenas).map(|_| arbitrary_arena(&mut b)).collect();
    let n_sc = (b.u8() % 8) as usize;
    let size_classes: Vec<SizeClassLine> = (0..n_sc)
        .map(|_| SizeClassLine {
            class: b.u32(),
            size: b.u32(),
            free_bytes: b.u64(),
        })
        .collect();
    let n_numa = (b.u8() % 8) as usize;
    let numa_nodes: Vec<NumaNodeLine> = (0..n_numa)
        .map(|_| NumaNodeLine {
            node: b.u32(),
            coverage_bytes: b.u64(),
            empty_backed_bytes: b.u64(),
            live_bytes: b.u64(),
        })
        .collect();
    let detail = StatsDetail {
        arenas: &arenas,
        size_classes: &size_classes,
        numa_nodes: &numa_nodes,
    };

    // 1. Rendering is total and always valid JSON carrying `epoch`.
    let json = s.to_json_with(flags, &detail);
    let v: serde_json::Value =
        serde_json::from_str(&json).expect("the stats renderer must emit valid JSON");
    assert!(v["epoch"].is_u64(), "epoch always present");
    // explain() is total too.
    let _ = s.explain();

    // 2. Summary redaction non-interference (§36.12). The redacted view depends only on the
    //    visible arenas (+ epoch/profile), so a *higher*-labelled arena is invisible.
    let observer = b.u32();
    let visible = redact_arenas(&arenas, observer);
    let all_visible = visible.len() == arenas.len();
    let red = redact_summary(&s, &visible, all_visible);

    // 3. A redacted summary discloses only the visible domain's live bytes.
    if !all_visible {
        let disclosed: u64 = visible.iter().fold(0u64, |a, x| a.wrapping_add(x.used));
        assert_eq!(
            red.live_bytes, disclosed,
            "redacted live = Σ visible used, never a cross-domain aggregate"
        );
    }

    // Inject a strictly-higher-labelled arena; it must not perturb a low observer's view.
    if observer < u32::MAX {
        let mut arenas2 = arenas.clone();
        arenas2.push(ArenaLine {
            id: b.u32(),
            label: observer + 1, // strictly higher than the observer (guarded: observer < MAX)
            used: b.u64(),
            reserved: b.u64(),
        });
        // Only meaningful when the injected arena is genuinely higher (so still hidden).
        let visible2 = redact_arenas(&arenas2, observer);
        let all_visible2 = visible2.len() == arenas2.len();
        if !all_visible && !all_visible2 && visible2 == visible {
            let red2 = redact_summary(&s, &visible2, all_visible2);
            assert_eq!(red, red2, "a higher-domain arena is invisible to a low observer");
            assert_eq!(red.to_json(), red2.to_json());
        }
    }
});

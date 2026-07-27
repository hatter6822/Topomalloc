// SPDX-License-Identifier: MIT
//! C statistics / observability surface (§31.2 / §31.5 / §31.6, plan 07 W17-1b/2/4/5/6).
//!
//! This is the platform side of the pure renderer in [`topo_stats`]. It composes a **live,
//! epoch-stamped snapshot** from the running engine — the central-path allocator's byte
//! classes, the heap sampler's profiling estimates, and (under the `hugepage-optimized`
//! profile) the live NUMA router's §15 counters and §19.7 hugepage coverage — and exposes
//! it three ways (§31.2):
//!
//! * [`topomalloc_stats_json`] — the machine-readable Appendix-D JSON, flag-selectable.
//! * [`topomalloc_stats_print`] — a human-readable dump to a `FILE*`.
//! * [`topomalloc_stats_snapshot`] — a fixed `#[repr(C)]` struct of the core byte classes.
//!
//! plus [`topomalloc_explain_memory`] (§31.6, the RSS attribution string) and
//! [`topomalloc_stats_json_for_label`] (§36.12 label-scoped redaction, W17-6).
//!
//! **Epoch / consistency (§8.6, W17-1b).** Every composed snapshot stamps a fresh monotonic
//! [`STATS_EPOCH`], so a reader can order snapshots and tell a stale view from a fresh one
//! (the §8.6 "MUST include an epoch or sequence number"). The cumulative byte counters are
//! relaxed per-shard atomics, so a snapshot taken *during* concurrent traffic is per-field
//! accurate but not globally instantaneous; the documented §8.6 convention bounds the skew by
//! the bytes in flight during the read, and the reconciliation identities
//! (`virtual == active + pageheap_free`, `pageheap_free == retained + dirty + muzzy +
//! released`) hold by construction at any quiescent point. `CONSISTENT_SNAPSHOT` requests the
//! skew-minimizing read (the engine already computes `live` with a saturating
//! `allocated − freed`, so it never underflows).
//!
//! Stats are **derived observability**, not an abstract state-machine transition: composing a
//! snapshot changes no allocator state, so there is no §33.4 obligation. The redaction is the
//! Rust analogue of the proven Lean `stats_observation_noninterference` theorem
//! (`lean/TopoMalloc/SeLe4n/InformationFlow.lean`), pinned by `redaction_is_label_noninterference`.

use core::ffi::{c_char, c_int};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use std::string::String;
use std::vec::Vec;

use topo_core::{ArenaId, ArenaState};
use topo_stats::{ArenaLine, NumaNodeLine, Profile, SizeClassLine, Stats, StatsDetail, StatsFlags};

use crate::{global, AnyAllocator};

/// Monotonic stats-snapshot sequence (§8.6 "MUST include an epoch or sequence number",
/// W17-1b). Bumped once per composed snapshot. Process-global; relaxed — a stats counter
/// (§27.3 ordering map), the same discipline as the other cumulative counters.
static STATS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// The next snapshot epoch (post-increment: the first snapshot is epoch `1`, so `0` reads as
/// "never snapshotted").
#[inline]
fn next_epoch() -> u64 {
    STATS_EPOCH.fetch_add(1, Ordering::Relaxed) + 1
}

/// Validate a raw flag word (§10.4 strict-validation discipline, as the `topo_mallocx` flags):
/// reject any bit outside the defined `TOPOMALLOC_STATS_*` set, so a typo'd flag fails loudly
/// rather than silently doing nothing. `None` ⇒ invalid (the caller returns its error form).
#[inline]
fn checked_flags(flags: u64) -> Option<StatsFlags> {
    let f = StatsFlags(flags);
    if f.has_unknown_bits() {
        None
    } else {
        Some(f)
    }
}

// ---------------------------------------------------------------------------
// Live snapshot composition
// ---------------------------------------------------------------------------

/// Compose a live, epoch-stamped [`Stats`] from the running engine plus the requested
/// `BY_*` detail (`arenas` / `size_classes`, empty unless their flag is set). When
/// `observer_label` is given and it does **not** dominate every arena, the result is
/// **redacted** for that low domain (§36.12, W17-6): the cross-domain summary aggregates are
/// zeroed and only the visible arenas survive. The single place a snapshot is taken, so the
/// epoch is bumped exactly once per public stats call.
type Composed = (Stats, Vec<ArenaLine>, Vec<SizeClassLine>, Vec<NumaNodeLine>);

fn compose(flags: StatsFlags, observer_label: Option<u32>, delivering: bool) -> Composed {
    let mut s = Stats {
        epoch: next_epoch(),
        profile: Profile::active(),
        ..Stats::default()
    };
    let consistent = flags.contains(StatsFlags::CONSISTENT_SNAPSHOT);
    if let Some(eng) = global() {
        s.record_allocator(&allocator_stats(eng, consistent));
    }
    // Heap-sampler estimates (off-by-default; all-zero unless sampling is enabled).
    s.record_placement(crate::sampling::placement_stats());
    s.record_fragmentation(crate::sampling::sampled_internal_fragmentation_bytes());
    // The actual resident-set size from the OS (§31.6, W17-5); `0` if unavailable.
    s.record_rss(read_rss_bytes());
    // Live NUMA router: §15.4/§15.5 counters + §19.7 hugepage coverage (no-op when no router
    // is wired — the default extent build or a single-node host).
    if let Some(r) = crate::numa_api::router() {
        s.record_node_router(r.stats());
        s.record_huge(r.coverage());
    }

    // --- per-arena detail + summary redaction (§36.12, W17-6) ----------------------------
    let need_arenas = flags.contains(StatsFlags::BY_ARENA) || observer_label.is_some();
    let all_arenas = if need_arenas {
        all_arena_lines()
    } else {
        Vec::new()
    };
    let (summary, visible_arenas, redacting) = match observer_label {
        Some(low) => {
            let visible = topo_stats::redact_arenas(&all_arenas, low);
            // Redaction is keyed on the **observer's** label alone, never on which arenas
            // happen to exist: gating it on "the observer sees every live arena" would let
            // a high domain flip the low view by creating and destroying a labelled arena
            // (a covert channel), and would disclose every cross-domain aggregate whenever
            // no high arena existed. Only the lattice-top observer reads the raw summary.
            let raw = low == topo_stats::OBSERVER_LABEL_TOP;
            (topo_stats::redact_summary(&s, &visible, low), visible, !raw)
        }
        None => (s, all_arenas, false),
    };
    // §31.2 RESET_PEAKS: the snapshot above already captured the peak; clear the engine gauge so
    // the next reader sees the high-water *since now* — but only when we are actually
    // **delivering** this snapshot (a `buf == NULL` / `cap == 0` length-query sizing call must
    // not clear the gauge the caller's data call then reads), and only for a reader **authorized**
    // to see the global peak (an unscoped reader, or a label observer that dominates every arena —
    // `!redacting`). A redacted/low observer's peak is itself redacted to 0, so resetting the real
    // process-wide gauge would be a cross-domain write — letting a low domain destroy a high
    // domain's high-water for later authorized readers (§36.12). On POSIX (single PUBLIC label)
    // this is always the identity case, so RESET_PEAKS behaves exactly as before.
    if flags.contains(StatsFlags::RESET_PEAKS) && delivering && !redacting {
        if let Some(eng) = global() {
            eng.reset_peak_live();
        }
    }
    let arenas = if flags.contains(StatsFlags::BY_ARENA) {
        visible_arenas
    } else {
        Vec::new()
    };
    // The per-size-class central lists are shared/cross-domain, so a redacted low observer
    // gets none; an authorized reader gets the real breakdown.
    let size_classes = if flags.contains(StatsFlags::BY_SIZE_CLASS) && !redacting {
        size_class_lines()
    } else {
        Vec::new()
    };
    // Per-node NUMA coverage (cross-domain infrastructure ⇒ omitted for a redacted observer).
    let numa_nodes = if flags.contains(StatsFlags::BY_NUMA) && !redacting {
        numa_node_lines()
    } else {
        Vec::new()
    };
    (summary, arenas, size_classes, numa_nodes)
}

/// The §31.2 `BY_NUMA` per-node lines — each node's §19.7 hugepage coverage, from the live
/// router (empty in the default build / single-node host, where there is no router).
fn numa_node_lines() -> Vec<NumaNodeLine> {
    let mut v = Vec::new();
    if let Some(r) = crate::numa_api::router() {
        let nodes = r.stats().nodes as usize;
        let per = r.node_coverage();
        for (i, c) in per.iter().enumerate().take(nodes) {
            v.push(NumaNodeLine {
                node: i as u32,
                coverage_bytes: c.coverage_bytes,
                empty_backed_bytes: c.empty_backed_bytes,
                live_bytes: c.live_total_bytes,
            });
        }
    }
    v
}

/// Take an [`AllocatorStats`] snapshot, optionally in **consistent** mode (§8.6 / §31.2
/// `CONSISTENT_SNAPSHOT`): read the engine until **two successive reads agree on every field** —
/// not merely the cumulative `(allocated, freed)` totals, but the backend-state, central-list,
/// arena, and fragmentation fields too — meaning no concurrent allocation/free perturbed any of
/// them across the read window, so the snapshot is coherent. (An allocation can mutate backend or
/// central state *before* bumping the cumulative counters, so comparing the totals alone could
/// accept a torn read; full-struct equality closes that window.) This tightens but does not
/// replace true snapshot isolation — the documented seqlock deferral: the struct is still
/// assembled from many relaxed reads, so an equal pair is a strong coherence signal, not an
/// atomic capture. Bounded retries (opt-in operational debugging); the last read is returned
/// regardless, so it always terminates even under continuous load. Plain mode is a single read.
fn allocator_stats(eng: &AnyAllocator, consistent: bool) -> topo_core::AllocatorStats {
    let mut snap = eng.stats();
    if consistent {
        for _ in 0..8 {
            let again = eng.stats();
            // Full-struct equality (`AllocatorStats: PartialEq`): every byte class stable.
            if again == snap {
                return again;
            }
            snap = again;
        }
    }
    snap
}

/// The process resident-set size in bytes (§31.6, W17-5), from `/proc/self/statm` on Linux
/// (`0` elsewhere or on any read/parse failure). The slow stats surface, so a `String` read
/// is immaterial.
#[cfg(target_os = "linux")]
fn read_rss_bytes() -> u64 {
    // `/proc/self/statm`: "size resident shared text lib data dt", all in pages. Field index 1
    // (resident) × the page size.
    let Ok(statm) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let Some(resident_pages) = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return 0;
    };
    // SAFETY: `sysconf` is a pure parameter query with no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = if page > 0 { page as u64 } else { 4096 };
    resident_pages.saturating_mul(page)
}

/// Portable fallback: RSS unavailable (the `explain` endpoint then omits the figure).
#[cfg(not(target_os = "linux"))]
fn read_rss_bytes() -> u64 {
    0
}

/// Every **registered** (`!= Destroyed`) arena as an [`ArenaLine`] — the unredacted set, from
/// which [`compose`] derives the observer-visible view. A never-created slot reads as
/// `Destroyed` (its empty-state sentinel), so this filter makes the line count reconcile with
/// the summary `arenas.count` (= `live_count`). `MAX_ARENAS` is small — a bounded diagnostic.
fn all_arena_lines() -> Vec<ArenaLine> {
    let mut v = Vec::new();
    if let Some(eng) = global() {
        for id in 0..topo_core::MAX_ARENAS as u32 {
            if let Some(st) = eng.arena_stats(ArenaId(id)) {
                if st.state != ArenaState::Destroyed {
                    v.push(ArenaLine {
                        id: st.id.0,
                        label: st.label.0,
                        used: st.used,
                        reserved: st.reserved,
                    });
                }
            }
        }
    }
    v
}

/// The §31.2 `BY_SIZE_CLASS` per-class lines — only the classes with central-resident free
/// bytes (the interesting ones), the per-class decomposition of `central.free_bytes`.
fn size_class_lines() -> Vec<SizeClassLine> {
    let mut v = Vec::new();
    if let Some(eng) = global() {
        eng.for_each_size_class_central_free(|class, size, free_bytes| {
            if free_bytes > 0 {
                v.push(SizeClassLine {
                    class: class as u32,
                    size,
                    free_bytes,
                });
            }
        });
    }
    v
}

/// Compose and render the snapshot as JSON honoring `flags`, redacting arena detail for
/// `observer_label` if given. `delivering` is false for a length-query (`buf == NULL`) sizing
/// call, so a `RESET_PEAKS` request does not clear the gauge before the caller's data call reads.
fn render_json(flags: StatsFlags, observer_label: Option<u32>, delivering: bool) -> String {
    let (s, arenas, size_classes, numa_nodes) = compose(flags, observer_label, delivering);
    s.to_json_with(
        flags,
        &StatsDetail {
            arenas: &arenas,
            size_classes: &size_classes,
            numa_nodes: &numa_nodes,
        },
    )
}

/// Copy `s` into the caller's NUL-terminated buffer (truncated to `cap`), returning the
/// **full** length in bytes excluding the NUL — so a caller can size a buffer and re-call.
/// `buf == NULL` / `cap == 0` queries the length only. The established `topomalloc_*_dump`
/// convention (shared with `topomalloc_profile_dump_json`).
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
unsafe fn write_to_c_buf(s: &str, buf: *mut c_char, cap: usize) -> usize {
    let bytes = s.as_bytes();
    let full = bytes.len();
    if !buf.is_null() && cap > 0 {
        let n = full.min(cap - 1);
        // SAFETY: `bytes` is a live slice of `full >= n` bytes; the caller guarantees `buf`
        // has `cap > n` writable bytes, disjoint from `s`'s owned heap buffer.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), n);
            *buf.add(n) = 0;
        }
    }
    full
}

// ---------------------------------------------------------------------------
// The fixed `#[repr(C)]` snapshot struct (§31.2)
// ---------------------------------------------------------------------------

/// The fixed-layout core byte-class snapshot (§31.2 `topo_stats_t`), for operators who want
/// the numbers without parsing JSON. All `u64` (stable ABI); the field set is **additive**
/// across the 0.x series (new fields append, never reorder — pinned by the ABI smoke tests).
/// The §8.6 reconciliation identities hold over these fields:
/// `virtual_bytes == active_bytes + pageheap_free_bytes` and
/// `pageheap_free_bytes == retained_bytes + dirty_bytes + muzzy_bytes + released_bytes`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
#[allow(non_camel_case_types)]
pub struct topomalloc_stats_t {
    /// Monotonic snapshot sequence number (§8.6); `>= 1` for a real snapshot.
    pub epoch: u64,
    /// `application.live_bytes`.
    pub live_bytes: u64,
    /// `application.allocated_bytes_total`.
    pub allocated_bytes_total: u64,
    /// `application.freed_bytes_total`.
    pub freed_bytes_total: u64,
    /// Front-end cache bytes (per-CPU + thread + transfer; `0` until M2).
    pub cache_bytes: u64,
    /// `central.free_bytes`.
    pub central_free_bytes: u64,
    /// `backend.active_bytes` (§20.1 *active*).
    pub active_bytes: u64,
    /// `backend.retained_bytes` (§20.1 *Retained*).
    pub retained_bytes: u64,
    /// `backend.dirty_bytes`.
    pub dirty_bytes: u64,
    /// `backend.muzzy_bytes`.
    pub muzzy_bytes: u64,
    /// `backend.released_bytes`.
    pub released_bytes: u64,
    /// `backend.pageheap_free_bytes`.
    pub pageheap_free_bytes: u64,
    /// `backend.virtual_bytes` — total allocator-managed virtual memory.
    pub virtual_bytes: u64,
    /// `metadata.bytes`.
    pub metadata_bytes: u64,
    /// `quarantine.bytes` (§17.5; `0` until plan 08 quarantine accounting).
    pub quarantine_bytes: u64,
    /// `hugepage.coverage_bytes` (§19.7).
    pub hugepage_coverage_bytes: u64,
    /// `fragmentation.internal_sampled_bytes` (§31.5; `0` unless sampling is on).
    pub fragmentation_internal_sampled_bytes: u64,
    /// `fragmentation.external_bytes` (§31.5; dirty + muzzy).
    pub fragmentation_external_bytes: u64,
    /// `arenas.count`.
    pub arena_count: u64,
    /// `arenas.destroyed` (§31.1 cumulative).
    pub arena_destroyed: u64,
    // --- appended in the W17 completion pass (additive, §35.3) ---
    /// `application.peak_live_bytes` (§31.3 peak heap high-water).
    pub peak_live_bytes: u64,
    /// `application.rss_bytes` — the OS resident-set size (§31.6); `0` if unavailable.
    pub rss_bytes: u64,
    /// `backend.total_managed_vm_bytes` (= `virtual_bytes + metadata_bytes`, §8.6).
    pub total_managed_vm_bytes: u64,
    /// `fragmentation.internal_exact_bytes` — exact medium/large internal fragmentation (§31.5).
    pub fragmentation_internal_exact_bytes: u64,
    /// `hugepage.coverage_ratio_bp` (§19.7, basis points `0..=10000`).
    pub hugepage_coverage_ratio_bp: u64,
    /// `placement.sampled_live_bytes` (§31.3; `0` unless sampling is on).
    pub sampled_live_bytes: u64,
    /// `topology.numa_nodes` — nodes the router places across (`0` when no router).
    pub numa_nodes: u64,
}

impl topomalloc_stats_t {
    /// Project a composed [`Stats`] summary onto the fixed C struct.
    fn from_stats(s: &Stats) -> topomalloc_stats_t {
        let cache = s
            .per_cpu_bytes
            .saturating_add(s.thread_cache_bytes)
            .saturating_add(s.transfer_bytes);
        topomalloc_stats_t {
            epoch: s.epoch,
            live_bytes: s.live_bytes,
            allocated_bytes_total: s.allocated_bytes_total,
            freed_bytes_total: s.freed_bytes_total,
            cache_bytes: cache,
            central_free_bytes: s.central_free_bytes,
            active_bytes: s.active_bytes,
            retained_bytes: s.retained_bytes,
            dirty_bytes: s.dirty_bytes,
            muzzy_bytes: s.muzzy_bytes,
            released_bytes: s.released_bytes,
            pageheap_free_bytes: s.pageheap_free_bytes,
            virtual_bytes: s.virtual_bytes,
            metadata_bytes: s.metadata_bytes,
            quarantine_bytes: s.quarantine_bytes,
            hugepage_coverage_bytes: s.hugepage.coverage_bytes,
            fragmentation_internal_sampled_bytes: s.sampled_internal_fragmentation_bytes,
            fragmentation_external_bytes: s.dirty_bytes.saturating_add(s.muzzy_bytes),
            arena_count: s.live_arenas,
            arena_destroyed: s.arenas_destroyed,
            peak_live_bytes: s.peak_live_bytes,
            rss_bytes: s.rss_bytes,
            total_managed_vm_bytes: s.virtual_bytes.saturating_add(s.metadata_bytes),
            fragmentation_internal_exact_bytes: s.exact_internal_fragmentation_bytes,
            hugepage_coverage_ratio_bp: s.hugepage.coverage_ratio_bp() as u64,
            sampled_live_bytes: s.placement.sampled_live_bytes,
            numa_nodes: s.numa_nodes as u64,
        }
    }
}

// ---------------------------------------------------------------------------
// C entry points (§31.2)
// ---------------------------------------------------------------------------

/// `size_t topomalloc_stats_json(char* buf, size_t cap, uint64_t flags)` (§31.2): render the
/// live stats snapshot as JSON (Appendix-D shape) into `buf` (NUL-terminated, truncated to
/// `cap`), returning the **full** length in bytes (excluding the NUL). Pass `buf == NULL` /
/// `cap == 0` to query the length only. `flags` selects detail (`TOPOMALLOC_STATS_BY_*`); an
/// **unknown** flag bit is rejected (§10.4): the function writes nothing and returns `0`.
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_stats_json(buf: *mut c_char, cap: usize, flags: u64) -> usize {
    let Some(flags) = checked_flags(flags) else {
        return 0;
    };
    let _op = topo_core::fork::operation_guard();
    // A length-query (NULL buf / zero cap) does not deliver, so a `RESET_PEAKS` request must not
    // clear the gauge on it — only the caller's real data call resets (§31.2).
    let delivering = !buf.is_null() && cap > 0;
    let json = render_json(flags, None, delivering);
    // SAFETY: the caller's `buf`/`cap` contract is forwarded verbatim.
    unsafe { write_to_c_buf(&json, buf, cap) }
}

/// `size_t topomalloc_stats_json_for_label(char* buf, size_t cap, uint64_t flags,
/// uint32_t observer_label)` (§36.12, W17-6): like [`topomalloc_stats_json`], but the
/// `BY_ARENA` per-arena detail is **redacted** to only the arenas the `observer_label` domain
/// is authorized to see — a low domain cannot observe a higher domain's arenas. On POSIX every
/// arena is `PUBLIC` (`0`), so a `PUBLIC` observer sees everything (the identity case).
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_stats_json_for_label(
    buf: *mut c_char,
    cap: usize,
    flags: u64,
    observer_label: u32,
) -> usize {
    let Some(flags) = checked_flags(flags) else {
        return 0;
    };
    let _op = topo_core::fork::operation_guard();
    // As `topomalloc_stats_json`, a length-query does not reset peaks; additionally, `compose`
    // only resets for a non-redacting (authorized) observer, so a low label cannot clear the
    // process-wide peak it cannot even see (§36.12 cross-domain write).
    let delivering = !buf.is_null() && cap > 0;
    let json = render_json(flags, Some(observer_label), delivering);
    // SAFETY: the caller's `buf`/`cap` contract is forwarded verbatim.
    unsafe { write_to_c_buf(&json, buf, cap) }
}

/// `int topomalloc_stats_snapshot(topomalloc_stats_t* out, uint64_t flags)` (§31.2): fill the
/// fixed core byte-class struct. Returns `0` on success, `-1` on a NULL `out`. `flags` is
/// accepted for the consistent-snapshot read mode (§8.6) and forward-compatibility; the
/// struct carries the summary classes only (detail is the JSON's job).
///
/// # Safety
///
/// `out` must be null or point to a writable [`topomalloc_stats_t`].
#[no_mangle]
pub unsafe extern "C" fn topomalloc_stats_snapshot(
    out: *mut topomalloc_stats_t,
    flags: u64,
) -> c_int {
    let Some(flags) = checked_flags(flags) else {
        return -1;
    };
    if out.is_null() {
        return -1;
    }
    let _op = topo_core::fork::operation_guard();
    // `out` is non-null here, so this call always delivers (RESET_PEAKS, if set, takes effect).
    let (s, ..) = compose(flags, None, true);
    let c = topomalloc_stats_t::from_stats(&s);
    // SAFETY: `out` is non-null and the caller guarantees it points to a writable struct.
    unsafe { *out = c };
    0
}

/// `int topomalloc_stats_print(FILE* out, uint64_t flags)` (§31.2): write a human-readable
/// dump (the §31.6 RSS explanation followed by the §31.2 JSON) to the stream `out`. Returns
/// `0` on success, `-1` on a NULL stream or a short write. A single snapshot is taken, so the
/// explanation and the JSON are consistent with each other.
///
/// # Safety
///
/// `out` must be null or a valid, writable `FILE*` the caller owns.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_stats_print(out: *mut libc::FILE, flags: u64) -> c_int {
    let Some(flags) = checked_flags(flags) else {
        return -1;
    };
    if out.is_null() {
        return -1;
    }
    let _op = topo_core::fork::operation_guard();
    // `out` is non-null here, so this call always delivers (RESET_PEAKS, if set, takes effect).
    let (s, arenas, size_classes, numa_nodes) = compose(flags, None, true);
    let json = s.to_json_with(
        flags,
        &StatsDetail {
            arenas: &arenas,
            size_classes: &size_classes,
            numa_nodes: &numa_nodes,
        },
    );
    // A human-readable header + the §31.6 explanation, then the §31.2 JSON for the full detail.
    let mut text = std::format!(
        "TopoMalloc stats (epoch {}, profile {}, backend {}):\n",
        s.epoch,
        s.profile.as_str(),
        global().map_or("none", |a| a.backend_name()),
    );
    text.push_str(&s.explain());
    text.push('\n');
    text.push_str(&json);
    text.push('\n');
    let bytes = text.as_bytes();
    // SAFETY: `libc::fwrite` reads `bytes.len()` bytes from our live buffer and writes them to
    // the caller's valid stream; we pass element size 1 and the exact count.
    let written = unsafe { libc::fwrite(bytes.as_ptr().cast(), 1, bytes.len(), out) };
    if written == bytes.len() {
        0
    } else {
        -1
    }
}

/// `size_t topomalloc_explain_memory(char* buf, size_t cap)` (§31.6): render a human-readable
/// one-line RSS attribution ("RSS is attributed to: 2.5 GiB live, 700.0 MiB per-CPU cache,
/// …") into `buf` (NUL-terminated, truncated to `cap`), returning the full length. Pass
/// `buf == NULL` / `cap == 0` to query the length only. Adoption hinges on RSS being
/// explainable, not just measurable (§31.6).
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_explain_memory(buf: *mut c_char, cap: usize) -> usize {
    let _op = topo_core::fork::operation_guard();
    // SUMMARY carries no RESET_PEAKS, so `delivering` is moot here; pass the honest value anyway.
    let (s, ..) = compose(StatsFlags::SUMMARY, None, !buf.is_null());
    let text = s.explain();
    // SAFETY: the caller's `buf`/`cap` contract is forwarded verbatim.
    unsafe { write_to_c_buf(&text, buf, cap) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the tests that read/reset the **process-global** peak gauge, so they cannot
    /// perturb each other's high-water assertions when the harness runs tests in parallel.
    static PEAK_GAUGE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn epoch_is_monotonic_across_snapshots() {
        // W17-1b: every composed snapshot carries a strictly newer epoch.
        let (a, ..) = compose(StatsFlags::SUMMARY, None, true);
        let (b, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(
            b.epoch > a.epoch,
            "epoch is monotonic ({} -> {})",
            a.epoch,
            b.epoch
        );
    }

    #[test]
    fn json_is_wellformed_and_reconciles() {
        // Make a little live traffic so the snapshot is non-trivial.
        let p = crate::topomalloc_malloc(4096);
        let json = render_json(StatsFlags::SUMMARY, None, true);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        // §8.6 reconciliation identities hold (quiescent at read on this thread).
        let backend = &v["backend"];
        let retained = backend["retained_bytes"].as_u64().unwrap();
        let dirty = backend["dirty_bytes"].as_u64().unwrap();
        let muzzy = backend["muzzy_bytes"].as_u64().unwrap();
        let released = backend["released_bytes"].as_u64().unwrap();
        let pageheap = backend["pageheap_free_bytes"].as_u64().unwrap();
        let active = backend["active_bytes"].as_u64().unwrap();
        let virt = backend["virtual_bytes"].as_u64().unwrap();
        assert_eq!(pageheap, retained + dirty + muzzy + released);
        assert_eq!(virt, active + pageheap);
        // Epoch present and non-zero; quarantine + fragmentation classes present.
        assert!(v["epoch"].as_u64().unwrap() >= 1);
        assert!(v["quarantine"]["bytes"].is_u64());
        assert!(v["fragmentation"]["external_bytes"].is_u64());
        if !p.is_null() {
            // SAFETY: `p` is a live allocation we just made and now own.
            unsafe { crate::topomalloc_free(p) };
        }
    }

    #[test]
    fn snapshot_struct_matches_json() {
        let mut out = topomalloc_stats_t::default();
        // SAFETY: `out` is a valid, writable struct on our stack.
        let rc = unsafe { topomalloc_stats_snapshot(&mut out, StatsFlags::SUMMARY.bits()) };
        assert_eq!(rc, 0);
        assert!(out.epoch >= 1);
        // The struct's reconciliation identities hold too.
        assert_eq!(
            out.pageheap_free_bytes,
            out.retained_bytes + out.dirty_bytes + out.muzzy_bytes + out.released_bytes
        );
        assert_eq!(
            out.virtual_bytes,
            out.active_bytes + out.pageheap_free_bytes
        );
        // A NULL out is a clean error.
        // SAFETY: passing a null pointer is exactly the contract under test.
        assert_eq!(unsafe { topomalloc_stats_snapshot(ptr::null_mut(), 0) }, -1);
    }

    #[test]
    fn json_length_query_and_truncation() {
        // The length-query form returns a positive full length and writes nothing.
        // (Each call composes a *fresh* snapshot — the epoch advances and byte counts
        // shift — so two calls' lengths need not match; we never compare across calls.)
        // SAFETY: the length-query form writes nothing.
        assert!(unsafe { topomalloc_stats_json(ptr::null_mut(), 0, 0) } > 0);
        // A small buffer truncates and NUL-terminates within `cap`, and reports the full
        // length of *that* snapshot (>= cap, so > 16, since the JSON dwarfs 16 bytes).
        let mut small = [0xAAu8; 16];
        // SAFETY: `small` is 16 writable bytes matching `cap`.
        let n = unsafe { topomalloc_stats_json(small.as_mut_ptr().cast::<c_char>(), 16, 0) };
        assert!(
            n > 16,
            "the JSON is far larger than the 16-byte buffer (truncated)"
        );
        assert_eq!(small[15], 0, "NUL-terminated within cap");
    }

    #[test]
    fn explain_memory_renders() {
        // A single call into a generously-sized buffer: no cross-snapshot length compare
        // (each call is a fresh snapshot). The explanation fits well under 1 KiB.
        let mut buf = [0u8; 1024];
        // SAFETY: `buf` is 1024 writable bytes matching `cap`.
        let n = unsafe { topomalloc_explain_memory(buf.as_mut_ptr().cast::<c_char>(), buf.len()) };
        assert!(n > 0 && n < buf.len(), "fits without truncation");
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        // On Linux the real RSS is read, so it leads with "RSS is <size>:"; elsewhere it
        // falls back to "RSS is attributed to:". Both share the "RSS is " prefix.
        assert!(s.starts_with("RSS is "), "explanation: {s}");
    }

    #[test]
    fn by_arena_json_carries_an_arena_line_and_redaction_is_total() {
        // The default arena (id 0, PUBLIC) is always present, so BY_ARENA has at least one
        // line, and a PUBLIC observer (the POSIX case) sees it (identity redaction).
        let json = render_json(StatsFlags::BY_ARENA, None, true);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let arenas = v["by_arena"].as_array().expect("by_arena present");
        assert!(arenas.iter().any(|a| a["id"] == 0));
        // Redacting for the PUBLIC (0) observer keeps the PUBLIC default arena.
        let redacted = render_json(StatsFlags::BY_ARENA, Some(0), true);
        let rv: serde_json::Value = serde_json::from_str(&redacted).unwrap();
        assert!(rv["by_arena"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == 0));
    }

    #[test]
    fn stats_print_to_devnull_succeeds() {
        let path = c"/dev/null";
        let mode = c"w";
        // SAFETY: `fopen` reads the two NUL-terminated C strings we own and returns a stream
        // (or null); both literals are valid for the call's duration.
        let f = unsafe { libc::fopen(path.as_ptr(), mode.as_ptr()) };
        assert!(!f.is_null());
        // SAFETY: `f` is a valid writable stream.
        let rc = unsafe { topomalloc_stats_print(f, StatsFlags::SUMMARY.bits()) };
        assert_eq!(rc, 0);
        // A NULL stream is a clean error.
        // SAFETY: passing null is the contract under test.
        assert_eq!(unsafe { topomalloc_stats_print(ptr::null_mut(), 0) }, -1);
        // SAFETY: `f` is the stream we opened and have not yet closed.
        unsafe { libc::fclose(f) };
    }

    #[test]
    fn flag_constants_match_the_documented_layout() {
        // The C header (`TOPOMALLOC_STATS_*`) mirrors these exact bits.
        assert_eq!(StatsFlags::SUMMARY.bits(), 0);
        assert_eq!(StatsFlags::BY_ARENA.bits(), 1 << 0);
        assert_eq!(StatsFlags::BY_SIZE_CLASS.bits(), 1 << 1);
        assert_eq!(StatsFlags::BY_CPU.bits(), 1 << 2);
        assert_eq!(StatsFlags::BY_NUMA.bits(), 1 << 3);
        assert_eq!(StatsFlags::BY_HUGEPAGE.bits(), 1 << 4);
        assert_eq!(StatsFlags::CONSISTENT_SNAPSHOT.bits(), 1 << 5);
        assert_eq!(StatsFlags::RESET_PEAKS.bits(), 1 << 6);
    }

    #[test]
    fn unknown_flag_bits_are_rejected() {
        // §10.4 strict validation: an out-of-range flag bit fails loudly, never silently.
        let bad = 1u64 << 40;
        // SAFETY: every call passes a NULL buffer / out pointer; validation or the null check
        // rejects before any dereference, so the whole block is sound (the contract under test).
        unsafe {
            assert_eq!(topomalloc_stats_json(ptr::null_mut(), 0, bad), 0);
            assert_eq!(
                topomalloc_stats_json_for_label(ptr::null_mut(), 0, bad, 0),
                0
            );
            assert_eq!(topomalloc_stats_snapshot(ptr::null_mut(), bad), -1);
            assert_eq!(topomalloc_stats_print(ptr::null_mut(), bad), -1);
            // A valid flag word still works (length query, writes nothing).
            assert!(topomalloc_stats_json(ptr::null_mut(), 0, StatsFlags::ALL.bits()) > 0);
        }
    }

    #[test]
    fn consistent_snapshot_is_coherent_when_quiescent() {
        // W17-1b: with CONSISTENT_SNAPSHOT, the cumulative identity is exact (no torn read)
        // when this thread is the only mutator — the read-twice-stable loop converges.
        let p = crate::topomalloc_malloc(123);
        let (s, ..) = compose(StatsFlags::CONSISTENT_SNAPSHOT, None, true);
        assert_eq!(
            s.live_bytes,
            s.allocated_bytes_total - s.freed_bytes_total,
            "consistent snapshot reconciles exactly"
        );
        if !p.is_null() {
            // SAFETY: `p` is a live allocation we own.
            unsafe { crate::topomalloc_free(p) };
        }
    }

    #[test]
    fn reset_peaks_clears_the_high_water() {
        let _serial = PEAK_GAUGE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // W17-2: a large live allocation lifts the peak; RESET_PEAKS drops it back to the
        // (lower) live bytes after the allocation is freed.
        let big = crate::topomalloc_malloc(8 << 20); // 8 MiB, lifts peak well above steady state
        assert!(!big.is_null());
        let (after_alloc, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(
            after_alloc.peak_live_bytes >= 8 << 20,
            "peak captured the spike"
        );
        // SAFETY: live allocation we own.
        unsafe { crate::topomalloc_free(big) };
        // Reset while live is now low; the peak collapses to ~current live.
        let (reset, ..) = compose(StatsFlags::RESET_PEAKS, None, true);
        let (next, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(
            next.peak_live_bytes < 8 << 20,
            "peak {} dropped after RESET_PEAKS (was >= 8 MiB)",
            next.peak_live_bytes
        );
        assert!(reset.peak_live_bytes >= reset.live_bytes);
    }

    #[test]
    fn reset_peaks_is_skipped_on_a_length_query() {
        let _serial = PEAK_GAUGE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // PR #21 review: with RESET_PEAKS, a length-query (NULL buf) sizing call must NOT clear
        // the gauge — only the caller's real data call resets. Otherwise the documented
        // size-then-write JSON pattern receives an already-reset peak on the data call.
        let big = crate::topomalloc_malloc(8 << 20); // lifts the peak well above steady state
        assert!(!big.is_null());
        // SAFETY: live allocation we own.
        unsafe { crate::topomalloc_free(big) };
        let (before, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(before.peak_live_bytes >= 8 << 20, "peak established");

        // A RESET_PEAKS length-query (NULL buf): returns the full length, must NOT reset.
        // SAFETY: a NULL buffer is exactly the length-query contract.
        let n =
            unsafe { topomalloc_stats_json(ptr::null_mut(), 0, StatsFlags::RESET_PEAKS.bits()) };
        assert!(n > 0, "the length query reports a length");
        let (after_query, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(
            after_query.peak_live_bytes >= 8 << 20,
            "a RESET_PEAKS length query must not reset the gauge (peak {})",
            after_query.peak_live_bytes
        );

        // A real **delivering** data call with RESET_PEAKS does reset it.
        let mut buf = [0u8; 8192];
        // SAFETY: 8192 writable bytes matching `cap`.
        let n2 = unsafe {
            topomalloc_stats_json(
                buf.as_mut_ptr().cast::<c_char>(),
                buf.len(),
                StatsFlags::RESET_PEAKS.bits(),
            )
        };
        assert!(n2 > 0);
        let (after_data, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(
            after_data.peak_live_bytes < 8 << 20,
            "the delivering data call reset the peak (now {})",
            after_data.peak_live_bytes
        );
    }

    #[test]
    fn by_numa_detail_block_is_present_when_flagged() {
        // W17-2: BY_NUMA renders a defined (possibly empty) per-node array — never a missing
        // key. (Populated under the live router; empty in the default extent build.)
        let json = render_json(StatsFlags::BY_NUMA, None, true);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(v["by_numa_node"].is_array());
        // BY_HUGEPAGE renders the labeled per-bin distribution.
        let hj = render_json(StatsFlags::BY_HUGEPAGE, None, true);
        let hv: serde_json::Value = serde_json::from_str(&hj).unwrap();
        let bins = hv["by_hugepage_bin"].as_array().expect("by_hugepage_bin");
        assert_eq!(bins.len(), 9, "the nine §19.4 occupancy bins");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rss_is_read_from_the_os_on_linux() {
        // W17-5: a live process has a non-zero RSS, so the explanation leads with it.
        let (s, ..) = compose(StatsFlags::SUMMARY, None, true);
        assert!(s.rss_bytes > 0, "RSS read from /proc/self/statm");
        assert!(s.explain().starts_with("RSS is "));
    }

    #[test]
    fn exact_internal_fragmentation_tracks_a_live_large_allocation() {
        // W17-4: a large allocation with a non-page-multiple request has exact internal
        // fragmentation `usable − requested`; it returns to ~0 when freed.
        let req = (2 * 1024 * 1024) + 100; // just over the 2 MiB huge threshold, not page-aligned
        let p = crate::topomalloc_malloc(req);
        assert!(!p.is_null());
        let usable = crate::topomalloc_malloc_usable_size(p);
        let (with_live, ..) = compose(StatsFlags::SUMMARY, None, true);
        // The exact figure includes at least this allocation's waste (other live larges may add).
        assert!(
            with_live.exact_internal_fragmentation_bytes >= (usable - req) as u64,
            "exact frag {} >= this alloc's waste {}",
            with_live.exact_internal_fragmentation_bytes,
            usable - req
        );
        assert!(
            usable > req,
            "a non-page-multiple request wastes the page tail"
        );
        // SAFETY: live allocation we own.
        unsafe { crate::topomalloc_free(p) };
    }
}

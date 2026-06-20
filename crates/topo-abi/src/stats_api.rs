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
use topo_stats::{ArenaLine, Profile, SizeClassLine, Stats, StatsDetail, StatsFlags};

use crate::global;

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

// ---------------------------------------------------------------------------
// Live snapshot composition
// ---------------------------------------------------------------------------

/// Compose a live, epoch-stamped [`Stats`] from the running engine plus the requested
/// `BY_*` detail (`arenas` / `size_classes`, empty unless their flag is set), redacting the
/// per-arena detail for `observer_label` if one is given (§36.12, W17-6). The single place a
/// snapshot is taken, so the epoch is bumped exactly once per public stats call.
fn compose(
    flags: StatsFlags,
    observer_label: Option<u32>,
) -> (Stats, Vec<ArenaLine>, Vec<SizeClassLine>) {
    let mut s = Stats {
        epoch: next_epoch(),
        profile: Profile::active(),
        ..Stats::default()
    };
    if let Some(eng) = global() {
        s.record_allocator(&eng.stats());
    }
    // Heap-sampler estimates (off-by-default; all-zero unless sampling is enabled).
    s.record_placement(crate::sampling::placement_stats());
    s.record_fragmentation(crate::sampling::sampled_internal_fragmentation_bytes());
    // Live NUMA router: §15.4/§15.5 counters + §19.7 hugepage coverage (no-op when no router
    // is wired — the default extent build or a single-node host).
    if let Some(r) = crate::numa_api::router() {
        s.record_node_router(r.stats());
        s.record_huge(r.coverage());
    }

    let arenas = if flags.contains(StatsFlags::BY_ARENA) {
        arena_lines(observer_label)
    } else {
        Vec::new()
    };
    let size_classes = if flags.contains(StatsFlags::BY_SIZE_CLASS) {
        size_class_lines()
    } else {
        Vec::new()
    };
    (s, arenas, size_classes)
}

/// The §31.2 `BY_ARENA` per-arena lines, optionally redacted for a low-domain observer
/// (§36.12, W17-6). Enumerates the registered arenas (`MAX_ARENAS` is small — a bounded
/// diagnostic, off the alloc fast path).
fn arena_lines(observer_label: Option<u32>) -> Vec<ArenaLine> {
    let mut v = Vec::new();
    if let Some(eng) = global() {
        for id in 0..topo_core::MAX_ARENAS as u32 {
            // A never-created slot reads as `Destroyed` (its empty-state sentinel), so filter
            // to *registered* arenas — the `!= Destroyed` definition `live_count` uses, so the
            // `by_arena` line count reconciles with the summary `arenas.count`.
            match eng.arena_stats(ArenaId(id)) {
                Some(st) if st.state != ArenaState::Destroyed => v.push(ArenaLine {
                    id: st.id.0,
                    label: st.label.0,
                    used: st.used,
                    reserved: st.reserved,
                }),
                _ => {}
            }
        }
    }
    match observer_label {
        Some(low) => topo_stats::redact_arenas(&v, low),
        None => v,
    }
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
/// `observer_label` if given.
fn render_json(flags: StatsFlags, observer_label: Option<u32>) -> String {
    let (s, arenas, size_classes) = compose(flags, observer_label);
    s.to_json_with(
        flags,
        &StatsDetail {
            arenas: &arenas,
            size_classes: &size_classes,
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
        }
    }
}

// ---------------------------------------------------------------------------
// C entry points (§31.2)
// ---------------------------------------------------------------------------

/// `size_t topomalloc_stats_json(char* buf, size_t cap, uint64_t flags)` (§31.2): render the
/// live stats snapshot as JSON (Appendix-D shape) into `buf` (NUL-terminated, truncated to
/// `cap`), returning the **full** length in bytes (excluding the NUL). Pass `buf == NULL` /
/// `cap == 0` to query the length only. `flags` selects detail (`TOPOMALLOC_STATS_BY_*`);
/// unknown bits are ignored (forward-compatible).
///
/// # Safety
///
/// `buf` must be null or point to at least `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn topomalloc_stats_json(buf: *mut c_char, cap: usize, flags: u64) -> usize {
    let _op = topo_core::fork::operation_guard();
    let json = render_json(StatsFlags(flags), None);
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
    let _op = topo_core::fork::operation_guard();
    let json = render_json(StatsFlags(flags), Some(observer_label));
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
    if out.is_null() {
        return -1;
    }
    let _op = topo_core::fork::operation_guard();
    let (s, _a, _c) = compose(StatsFlags(flags), None);
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
    if out.is_null() {
        return -1;
    }
    let _op = topo_core::fork::operation_guard();
    let (s, arenas, size_classes) = compose(StatsFlags(flags), None);
    let json = s.to_json_with(
        StatsFlags(flags),
        &StatsDetail {
            arenas: &arenas,
            size_classes: &size_classes,
        },
    );
    let mut text = s.explain();
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
    let (s, _a, _c) = compose(StatsFlags::SUMMARY, None);
    let text = s.explain();
    // SAFETY: the caller's `buf`/`cap` contract is forwarded verbatim.
    unsafe { write_to_c_buf(&text, buf, cap) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_monotonic_across_snapshots() {
        // W17-1b: every composed snapshot carries a strictly newer epoch.
        let (a, _, _) = compose(StatsFlags::SUMMARY, None);
        let (b, _, _) = compose(StatsFlags::SUMMARY, None);
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
        let json = render_json(StatsFlags::SUMMARY, None);
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
        assert!(s.starts_with("RSS is attributed to: "));
    }

    #[test]
    fn by_arena_json_carries_an_arena_line_and_redaction_is_total() {
        // The default arena (id 0, PUBLIC) is always present, so BY_ARENA has at least one
        // line, and a PUBLIC observer (the POSIX case) sees it (identity redaction).
        let json = render_json(StatsFlags::BY_ARENA, None);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        let arenas = v["by_arena"].as_array().expect("by_arena present");
        assert!(arenas.iter().any(|a| a["id"] == 0));
        // Redacting for the PUBLIC (0) observer keeps the PUBLIC default arena.
        let redacted = render_json(StatsFlags::BY_ARENA, Some(0));
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
}

// SPDX-License-Identifier: MIT
//! The M0 walking-skeleton vertical slice (W0-14), end to end in one test:
//! classify → allocate through the `TopoBackingProvider` seam → emit the §33.7
//! trace → parse it back → replay against the executable model. This is the
//! single thin slice that proves every tool in the pipeline links and runs
//! before any real allocator logic exists.

use topo_backend_posix::PosixBackingProvider;
use topo_core::classify::RequestKind;
use topo_core::{classify, trace, SkeletonAllocator};
use topo_test_support::{parse_trace_line, LiveModel, TraceRecord};

#[test]
fn classify_allocate_emit_parse_replay() {
    let alloc = SkeletonAllocator::new(PosixBackingProvider::new(), 1 << 20).expect("heap");
    assert_eq!(alloc.backend_name(), "posix");

    // Drive a handful of allocations across small and large paths, building a
    // trace as we go.
    let mut trace_log = String::new();
    let sizes = [1usize, 16, 24, 100, 4096, 200_000];
    let mut live_ptrs = Vec::new();

    for (request_id, &size) in sizes.iter().enumerate() {
        let req = classify(size, 16, 0).expect("classifiable");
        let (usable, sc) = match req.kind {
            RequestKind::Small { sc, usable } => (usable, Some(sc.index() as u64)),
            RequestKind::Medium { bytes } | RequestKind::Large { bytes } => (bytes, None),
        };
        let ptr = alloc.malloc(size, 16);
        assert!(!ptr.is_null(), "alloc of {size} failed");
        assert_eq!(ptr as usize % 16, 0);

        trace::emit_alloc(
            &mut trace_log,
            request_id as u64,
            size,
            16,
            req.arena.0,
            0,
            ptr as usize,
            usable,
            sc,
            sc.map(|_| 0), // span id 0 in the skeleton
        )
        .unwrap();
        live_ptrs.push(ptr as usize);
    }

    // Free everything (leaks in M0) and record the frees in the trace.
    for &p in &live_ptrs {
        trace::emit_free(&mut trace_log, p, 0, None, None).unwrap();
    }

    // Now replay the emitted trace through parse + model and assert it is
    // well-formed at every boundary (the differential spine, W0-14e).
    let mut model = LiveModel::new();
    let mut allocs = 0;
    let mut frees = 0;
    for line in trace_log.lines() {
        let rec = parse_trace_line(line).expect("emitted line must parse");
        match rec {
            TraceRecord::Alloc { .. } => allocs += 1,
            TraceRecord::Free { .. } => frees += 1,
            _ => {}
        }
        model.apply(&rec).expect("trace stays well-formed");
    }

    assert_eq!(allocs, sizes.len());
    assert_eq!(frees, sizes.len());
    assert_eq!(
        model.live_count(),
        0,
        "every allocation was freed in the trace"
    );
}

#[test]
fn skeleton_reports_capacity_and_usage() {
    let alloc = SkeletonAllocator::new(PosixBackingProvider::new(), 64 * 1024).expect("heap");
    assert_eq!(alloc.capacity(), 64 * 1024);
    assert_eq!(alloc.used_bytes(), 0);
    let p = alloc.malloc(100, 16);
    assert!(!p.is_null());
    assert!(alloc.used_bytes() >= 100);
}

#[test]
fn skeleton_oom_returns_null_without_wrapping() {
    // A one-page heap: a small allocation succeeds, but a large allocation
    // (rounded up to whole pages by the trivial table) must fail cleanly — not
    // wrap to a tiny region (§9.7).
    let alloc = SkeletonAllocator::new(PosixBackingProvider::new(), 16384).expect("heap");
    let p1 = alloc.malloc(128, 16); // small: fits
    assert!(!p1.is_null());
    let p2 = alloc.malloc(200_000, 16); // large: rounds past capacity
    assert!(p2.is_null(), "over-capacity allocation must return null");
}

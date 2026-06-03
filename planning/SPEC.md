# TopoMalloc Specification

**Document status:** Design specification, revision 0.3  
**Date:** 2026-06-03  
**Audience:** allocator implementers, systems engineers, runtime maintainers, database engineers, performance engineers, and formal verification engineers  
**Scope:** A proposed general-purpose memory allocator combining per-CPU caching, topology-aware transfer layers, jemalloc-style policy arenas, Temeraire-style hugepage-aware backing, rigorous observability, a Lean-first formal model, and a required seLe4n/seL4-style microkernel integration profile.

## Document control

| Field | Value |
|---|---|
| Name | TopoMalloc Specification |
| Revision | 0.3 |
| Status | Draft technical specification |
| Primary design goal | High-throughput, low-fragmentation, topology-aware allocation with machine-checkable safety invariants |
| Reference allocators | jemalloc and modern Google TCMalloc |
| Preferred verification model | Lean 4 abstract model plus C/C++/assembly implementation contracts |
| Production implementation language | C++20 or Rust plus small per-architecture assembly/RSEQ fragments |
| Supported first platform | Linux x86-64 and AArch64 with mmap, madvise, TLS, atomics, and optional RSEQ |
| First microkernel integration profile | seLe4n user-level allocator and memory-service profile |
| Portability target | Degraded-but-correct mode on POSIX-like systems without RSEQ |

## Document history

| Revision | Date | Summary |
|---|---|---|
| 0.1 | 2026-05 | Initial draft. |
| 0.2 | 2026-06-01 | Expanded the seLe4n integration profile (Section 36) and the appendices. |
| 0.3 | 2026-06-03 | Audit and refinement pass. Made the seLe4n/seL4 integration profile a **required** (non-optional) profile with its own conformance class and a normative preamble to Section 36. Corrected the size-class rounding-waste model to separate the spacing-dominated and alignment-dominated regimes — the previous flat `<= 33%` target was unattainable below ~48 B under a 16-byte ABI quantum. Required `class.size` to be an integer multiple of `class.alignment` and removed per-object slab-offset adjustment. Unified the span object-count conservation law across the descriptor, the bitmap invariants, and empty-span detection. Reordered the lock hierarchy to follow data flow (transfer before central) and added release-before-acquire guidance. Added OOM and retry handling to the small-allocation slow path. Strengthened the RSEQ Lean contract with a frame condition and a distinct empty/underflow case. Added the allocator-TLS recursion rule, `errno`/C23 allocation-API requirements, the calloc rounding-overflow cross-reference, the unmapped-memory accounting caveat, and a new Section 11.7 resolving per-CPU-cache/arena routing. Synchronized the table of contents with the appendix sections. |

## Source basis and limits

This document intentionally separates established allocator mechanisms from proposed TopoMalloc design decisions. The established mechanisms are drawn from public jemalloc, TCMalloc, Linux RSEQ, and Lean/formal-verification references. The proposed mechanisms are normative requirements for TopoMalloc, not claims about any existing allocator.

* TCMalloc is modeled as a front-end, middle-end, back-end allocator; its public design documentation describes a front-end cache, middle-end refill structures, and a back-end that obtains memory from the operating system [R1].
* Modern TCMalloc can use per-CPU caches and has a hugepage-aware pageheap option; its tuning documentation discusses per-CPU cache sizing, heterogeneous per-CPU cache sizing, and the tradeoff between memory release and hugepage preservation [R1, R2].
* jemalloc is modeled as an arena-based allocator with tcaches, explicit arena controls, decay-based dirty/muzzy purging, and extent hooks [R4].
* Linux restartable sequences provide a userspace mechanism for efficient per-CPU updates without heavyweight atomics, when supported and correctly registered [R5].
* Lean is treated as the specification and proof language, not as the hot-path runtime implementation language. Lean 4 is both a theorem prover and an efficient programming language, but production allocator hot paths still require direct control over ABI, TLS, atomics, syscalls, and assembly [R8].
* seLe4n is treated as a candidate microkernel integration target because its public repository describes a Lean 4, capability-based microkernel with machine-checked invariants, a seL4-inspired architecture, a Raspberry Pi 5 target, and an active SMP trajectory [R10, R11].
* seL4-style physical memory management is treated as the authority model for the seLe4n profile: almost all physical memory is delegated to user level as untyped capabilities; objects are created by retyping; and greedy watermark behavior makes largest-first untyped partitioning an allocator best practice [R12].
* Upstream seL4 user-level C libraries include allocman, VKA, and VSpace abstractions for virtual memory, malloc memory, CSpaces, object allocation, and virtual memory management; the same documentation states those C libraries are useful for prototyping and are not verified [R13].
* The seL4 Microkit is a static-system framework on top of seL4; the TopoMalloc seLe4n profile must therefore support fixed-arena operation for statically structured systems [R14]. Within the profile, dynamic retype and runtime arena growth are the features that degrade to fixed-arena mode; the integration profile itself is a required part of this specification, not an optional add-on.
* The seLe4n integration described in this revision is a required profile of this specification. It is not a claim that seLe4n currently ships TopoMalloc or that seLe4n kernel-internal allocation should be replaced by a general-purpose heap; it is a normative requirement on a conforming TopoMalloc to *provide* the integration, subject to the user-level/service boundary defined in Section 36.

The specification does not promise that TopoMalloc will dominate all allocators on all workloads. It defines a design intended to make high performance plausible while making correctness, accounting, safety boundaries, and operational behavior explicit.

## Normative language

The keywords **MUST**, **MUST NOT**, **REQUIRED**, **SHOULD**, **SHOULD NOT**, **MAY**, and **OPTIONAL** are to be interpreted as normative requirements for a conforming TopoMalloc implementation.

* **Core conformance** means the implementation satisfies the public allocation API, ownership invariants, metadata invariants, and safety properties.
* **Performance conformance** means the implementation includes per-CPU fast paths, batching, hugepage-aware backing, adaptive cache budgets, and background memory return.
* **Formal conformance** means the implementation exposes a Lean model, machine-checkable size-class tables, and contracts linking implementation paths to model transitions.
* **Operational conformance** means the implementation exports structured statistics, profiles, controls, and runtime diagnostics sufficient to explain memory residency and fragmentation.
* **Microkernel-integration conformance** means the implementation provides the seLe4n/seL4-style integration profile of Section 36: capability-backed arenas, the backing-provider contract, label-partitioned caches and statistics, the arena revocation protocol, and the Lean bridge theorem checklist. This profile is normative, not optional. On hosts that are not capability-based microkernels it is satisfied by building and proving the Lean bridge model and providing the fixed-arena deployment profile; the POSIX backend remains the default runtime backend there.

## Table of contents

1. Executive summary  
2. Design philosophy  
3. Goals, non-goals, and threat model  
4. Terminology  
5. Requirements  
6. Architecture overview  
7. Memory taxonomy and state machine  
8. Ownership model and invariants  
9. Size classes and alignment  
10. Public APIs  
11. Front-end: per-CPU cache  
12. RSEQ contract and fallback modes  
13. Optional thread cache fallback  
14. Middle-end: transfer caches and central free lists  
15. Topology awareness: CPU, LLC, NUMA  
16. Spans, slabs, bitmaps, and object layout  
17. Metadata, pagemap, and pointer classification  
18. Back-end: extent and page management  
19. Hugepage-aware allocator  
20. Dirty, muzzy, retained, and released memory  
21. Memory release controller  
22. Arena policy domains  
23. Extent hooks and custom backing memory  
24. Lifetime, hotness, and placement policy  
25. Reallocation and aligned allocation  
26. calloc and zeroing policy  
27. Concurrency and memory ordering  
28. fork, signal, and async constraints  
29. Security and hardening  
30. Debugging and sanitization modes  
31. Statistics, telemetry, and profiling  
32. Configuration and control plane  
33. Lean specification and proofs  
34. Testing, benchmarking, and validation  
35. Deployment and ABI compatibility  
36. seLe4n integration profile  
37. Implementation roadmap  
38. Appendix A: Key algorithms  
39. Appendix B: Required invariant checklist  
40. Appendix C: Suggested default constants  
41. Appendix D: Example stats JSON  
42. Appendix E: Example control namespace  
43. Appendix F: Anti-patterns  
44. Appendix G: Open design questions  
45. References

# 1. Executive summary

TopoMalloc is a proposed general-purpose allocator for modern multicore systems. Its design combines five ideas that are usually found in separate allocators or research systems:

1. A TCMalloc-like per-CPU fast path for small objects, using RSEQ where available and a safe fallback where it is not.
2. A jemalloc-like policy model built around explicit arenas, extent hooks, decay controls, and rich introspection.
3. A Temeraire-like hugepage-aware back-end that treats physical memory, hugepage coverage, and TLB behavior as first-class allocation goals.
4. A Lean-first formal specification that defines allocator state, ownership, object disjointness, cache conservation, arena isolation, and safe release-to-OS properties as machine-checkable theorems.
5. A required seLe4n integration profile that treats TopoMalloc as a capability-backed user-level allocator/resource server rather than as a hidden in-kernel heap.

The allocator is layered:

```text
Application API
  -> Small-object front-end: per-CPU cache or fallback tcache
  -> Topology-aware transfer cache: per-LLC-domain exchange
  -> NUMA-local central free lists: span management by size class
  -> Arena policy domains: lifetime, hotness, NUMA, security, release policy
  -> Hugepage-aware backend: hugepage filler, huge cache, region cache, extent allocator
  -> OS interface: mmap, madvise, mprotect, THP hints, optional explicit hugepages
```

The most important TopoMalloc invariant is ownership uniqueness: every allocator-controlled byte range is in exactly one state and has exactly one owner. The allocator MUST never allow a byte range to be live and free at the same time, present in two caches at once, present in both a cache and central list, or released to the OS while containing a live object.

The second most important invariant is metadata consistency: the pagemap, span records, slab bitmaps, free counts, cache entries, arena ownership, hugepage occupancy records, and exported statistics MUST agree. TopoMalloc is allowed to duplicate metadata for speed only when it also defines and tests invariants that keep those copies consistent.

The third most important invariant is explicit policy isolation. Arenas are not merely contention shards. They are policy domains. A policy domain can specify NUMA preference, hugepage strategy, decay behavior, cache budget, debugging behavior, hot/cold placement, lifetime grouping, and custom backing memory. Automatic arenas MAY be used, but explicit arenas MUST be supported for applications that need lifecycle or placement control.

TopoMalloc is designed to be implemented in C++20 or Rust plus small assembly fragments. Lean is used to specify the abstract allocator and verify generated tables and transition invariants. The production implementation SHOULD be checked against the Lean model by trace replay, property tests, runtime invariant checks, and eventually selected refinement proofs.

# 2. Design philosophy

## 2.1 Primary thesis

The allocator should be fast because common cases are local and simple, not because uncommon cases are unsafe or unspecified. The allocator should be memory-efficient because it has explicit accounting and policy, not because it happens to return memory quickly on a benchmark. The allocator should be debuggable because every byte has a named state, not because engineers can infer allocator behavior from RSS alone.

## 2.2 Lessons adopted from jemalloc

TopoMalloc adopts the following ideas from jemalloc-style designs:

* Multiple arenas reduce global contention and allow different allocation policies.
* Explicit arena creation, reset, destroy, and extent hooks are valuable for applications with clear object lifetimes or custom memory sources.
* Thread-local caching is a robust fallback on systems without per-CPU primitives.
* Decay-based dirty and muzzy purging is a useful way to smooth RSS and latency tradeoffs.
* A tree-like control namespace is a strong operational interface.
* Heap profiling, stats printing, and introspection should be available without ad hoc instrumentation.

TopoMalloc does not copy jemalloc's arena model exactly. It changes arenas from mostly concurrency shards into explicit policy domains. Default sharding is topology-aware and adaptive.

## 2.3 Lessons adopted from TCMalloc

TopoMalloc adopts the following ideas from modern TCMalloc-style designs:

* The front-end, middle-end, back-end split is a clear and scalable allocator architecture [R1].
* Per-CPU caches reduce cache blowup relative to per-thread caches on high-thread-count systems [R1].
* RSEQ can make per-CPU fast paths lock-free and atomic-free in the common case on supported Linux systems [R5].
* Transfer caches are useful for moving batches between local caches and central structures [R1].
* Hugepage-aware back-end design can improve fleet-level memory efficiency and TLB behavior [R3].
* Dynamic cache sizing should respond to observed miss pressure rather than use a static global rule [R2].
* Sized delete and hot/cold allocation hints are valuable when available from C++ callers [R6].

TopoMalloc does not copy TCMalloc exactly. It adds explicit arenas, custom extent hooks, dirty/muzzy state vocabulary, per-arena policy, and stronger formal invariants.

## 2.4 Engineering principle: separate safety from policy

Safety properties MUST be unconditional under valid API usage. Policy properties MAY be heuristic.

Examples of unconditional safety properties:

* Allocated objects do not overlap.
* Freed objects are not returned twice unless reallocated in between.
* Released memory contains no live objects.
* The pagemap never routes a valid small object to the wrong size class.
* Cache refill and flush preserve ownership uniqueness.

Examples of heuristic policy properties:

* Hot objects should be placed near other hot objects.
* Short-lived objects should be grouped together.
* Hugepage coverage should be preserved when memory pressure is low.
* Cache budgets should grow for hot CPUs and shrink for idle CPUs.

A policy mistake MUST NOT become a safety mistake.

# 3. Goals, non-goals, and threat model

## 3.1 Goals

TopoMalloc has the following goals, in priority order:

1. **Correctness:** preserve memory safety under the allocator's API contract.
2. **Predictable ownership accounting:** every memory range must have a known state and owner.
3. **Fast small-object allocation:** the hot path should avoid locks and atomics where platform support permits.
4. **Bounded cache memory:** local caches should be globally budgeted and dynamically redistributed.
5. **Low fragmentation:** internal and external fragmentation should be measured, bounded where practical, and actively reduced.
6. **High hugepage coverage:** large server workloads should preserve hugepage-backed dense memory when doing so improves end-to-end performance.
7. **Topology awareness:** placement and cache exchange should respect CPU, LLC, and NUMA topology.
8. **Operational transparency:** all major internal states should be exported through structured statistics and profiles.
9. **Formal model:** safety-critical transformations should be specified in Lean.
10. **Portable degradation:** the allocator should remain correct on platforms without RSEQ or THP.
11. **Microkernel integration (required):** the allocator MUST provide a clean, capability-backed integration with seLe4n/seL4-style microkernels as a user-space/resource-server component, without weakening kernel minimality (Section 36). This is a required profile, not an optional one; what is configurable is the deployment style within the profile.

## 3.2 Non-goals

TopoMalloc is not intended to be:

* A hard real-time allocator with strict upper bounds for every operation.
* A garbage collector.
* A replacement for language-level ownership or lifetime management.
* A cryptographic memory-erasure system by default.
* A universal best allocator for embedded systems with tiny heaps.
* A pure Lean implementation of the hot path.
* A general-purpose dynamic heap inside the seLe4n or seL4 kernel core.
* A proof that malloc performance is globally optimal for every allocation trace.

## 3.3 Threat model

TopoMalloc defines three security profiles:

1. **Performance profile:** optimized for normal production use. Detects only cheap errors on the fast path.
2. **Hardened profile:** adds sampled guard pages, delayed quarantine, encoded freelist pointers, stronger metadata validation, and more double-free detection.
3. **Debug profile:** prioritizes detection over performance. Enables redzones, junk filling, zeroing, guard pages, exhaustive cache validation, and expensive consistency checks.

The allocator MUST defend its own metadata against accidental corruption where practical. It SHOULD make common heap misuse visible in hardened/debug profiles. It does not guarantee protection from arbitrary memory writes by a compromised process.

## 3.4 Reliability model

TopoMalloc MUST be reliable under:

* high thread counts,
* thread creation and destruction,
* CPU migration,
* NUMA migration,
* phase changes in allocation behavior,
* memory pressure,
* fork in multithreaded processes, subject to documented constraints,
* late initialization and early process teardown,
* mixed C and C++ allocation APIs when matching rules are respected.

# 4. Terminology

## 4.1 Core terms

| Term | Meaning |
|---|---|
| Object | A user-visible allocation returned by malloc/new or an equivalent API. |
| Usable size | The allocator-provided object size, which is at least the requested size. |
| Size class | A rounded object size used to group small or medium objects. |
| Slab | A memory region divided into equal-sized objects of one size class. |
| Span | A contiguous sequence of allocator pages managed as a unit. A span may back one slab or a large allocation. |
| Page | TopoMalloc's allocator page, normally a power-of-two multiple of the OS page size. |
| Hugepage | A large hardware/OS page-sized unit, typically 2 MiB on common x86-64 Linux configurations, but treated as a configurable platform property. |
| Arena | A policy domain that owns extents, caches, spans, metadata, and configuration. |
| Extent | A contiguous virtual memory range managed by the backend. |
| Pagemap | Metadata mapping an address/page to the owning span or large allocation descriptor. |
| Front-end | The allocator layer directly used by malloc/free hot paths. |
| Middle-end | The refill/exchange layer between front-end caches and central span management. |
| Back-end | The allocator layer that obtains, retains, purges, and releases memory through the OS. |

## 4.2 Memory state terms

| State | Meaning |
|---|---|
| Live | Currently owned by the application. |
| FreeLocal | Free and held in a CPU-local or thread-local cache. |
| FreeTransfer | Free and held in a transfer cache. |
| FreeCentral | Free and held by a central free list or slab. |
| Dirty | Free and reusable, but still physically backed and may contain old data. |
| Muzzy | Free and reusable, lazily purged or marked discardable according to platform semantics. |
| Retained | Virtual address space retained by the allocator, not necessarily physically backed. |
| Released | Memory returned or advised away such that it must be recommitted or faulted before reuse. |
| Metadata | Allocator-owned control memory. |
| Quarantined | Freed user object deliberately withheld from immediate reuse for debugging or hardening. |

## 4.3 Locality terms

| Term | Meaning |
|---|---|
| CPU | Logical CPU or hardware thread reported by the operating system. |
| LLC domain | A group of CPUs sharing a last-level cache. |
| NUMA node | A memory locality domain with different access cost characteristics. |
| Hot object | Object expected to be accessed frequently. |
| Cold object | Object expected to be accessed rarely after initialization. |
| Lifetime class | Short, medium, long, or unknown lifetime classification inferred from hints or profiles. |

# 5. Requirements

## 5.1 Functional requirements

F-001. TopoMalloc MUST provide C allocation functions compatible with the platform C ABI: malloc, calloc, realloc, free, aligned_alloc where available, and common POSIX allocation interfaces when configured.

F-002. TopoMalloc SHOULD provide C++ operator new/delete replacements, including aligned and sized deallocation overloads where the target language and compiler support them.

F-003. TopoMalloc MUST return memory aligned sufficiently for any object type whose size and allocation API imply that alignment requirement.

F-004. TopoMalloc MUST support size zero allocation semantics in a documented, standards-compatible way for the platform. A zero-size allocation MAY return null or a unique freeable pointer according to configured ABI mode, but behavior MUST be consistent.

F-005. TopoMalloc MUST support explicit arena creation, configuration, reset, and destruction. Arena reset/destroy semantics MUST be documented as invalidating outstanding allocations from that arena.

F-006. TopoMalloc MUST support a default arena policy suitable for applications that never call extended APIs.

F-007. TopoMalloc MUST support large allocations directly from the backend rather than caching them in small-object structures.

F-008. TopoMalloc MUST export usable size introspection for pointers allocated by TopoMalloc.

F-009. TopoMalloc SHOULD support allocation flags for arena selection, alignment, zeroing, tcache bypass, cold allocation, hot allocation, lifetime hint, NUMA preference, and debugging profile.

F-010. TopoMalloc MUST be able to release unused memory to the OS, subject to platform support and configured policy.

## 5.2 Safety requirements

S-001. No two live allocations may overlap.

S-002. A free object may appear in at most one free structure.

S-003. A live object MUST NOT appear in any free structure.

S-004. A released range MUST NOT contain live objects.

S-005. A returned pointer MUST lie within an allocator-owned range and satisfy requested alignment.

S-006. The allocator MUST NOT write outside its metadata or object ranges.

S-007. The allocator MUST NOT call general malloc recursively from allocator hot paths. Internal metadata allocation MUST use bootstrap-safe metadata allocators.

S-008. The allocator MUST be robust against concurrent valid malloc/free operations.

S-009. In fast mode, invalid free is undefined behavior at the C/C++ API level, but the allocator MUST NOT let a detectable invalid free corrupt unrelated allocator state when hardened/debug checks are enabled.

S-010. All atomics, locks, and RSEQ paths MUST have documented memory-ordering requirements.

## 5.3 Performance requirements

P-001. Small allocation and deallocation hot paths SHOULD avoid contended locks.

P-002. On Linux with RSEQ support, the default small-object fast path SHOULD be per-CPU.

P-003. Without RSEQ support, the allocator MUST fall back to a correct mode. The fallback SHOULD be per-thread cache mode or lock-sharded arena mode.

P-004. Refills and flushes SHOULD move batches, not individual objects, except for rare or very large size classes.

P-005. Cache budgets SHOULD adapt to observed miss rates and memory pressure.

P-006. Hugepage-aware backing SHOULD be enabled by default for large server heaps when platform support is available.

P-007. The allocator SHOULD avoid eager subrelease of partial hugepages unless memory pressure justifies the TLB cost.

P-008. Per-arena and global policies SHOULD avoid excessive background purging work on application threads.

## 5.4 Operational requirements

O-001. The allocator MUST expose a structured stats API.

O-002. The allocator MUST report bytes in live allocations, per-CPU caches, optional thread caches, transfer caches, central free lists, pageheap free state, dirty state, muzzy state, retained virtual memory, released memory, metadata, and quarantine.

O-003. The allocator SHOULD report hugepage coverage, partial hugepage occupancy, subreleased bytes, and hugepage fragmentation.

O-004. The allocator SHOULD support sampled heap profiling with stack traces.

O-005. The allocator SHOULD support sampled lifetime profiling.

O-006. The allocator SHOULD support runtime cache flush, memory release, arena purge, arena decay, and stats refresh controls.

O-007. The allocator MUST provide an emergency low-memory mode that reduces caches, accelerates release, and disables nonessential background expansion.

## 5.5 Formal requirements

V-001. The size-class table MUST be generated from or checked by a formal specification.

V-002. The ownership state machine MUST be modeled in Lean.

V-003. The Lean model MUST state and prove at least ownership uniqueness, live-disjointness preservation, allocation soundness, free soundness, cache refill/flush conservation, pagemap consistency, and safe release-to-OS for the abstract allocator.

V-004. Implementation paths MUST be documented as refinements of abstract transitions, even when machine-level proof is incomplete.

V-005. Unsafe primitives, including RSEQ assembly, syscalls, custom extent hooks, and raw metadata writes, MUST have explicit contracts.

# 6. Architecture overview

## 6.1 Layer diagram

```text
+---------------------------------------------------------------+
| Public API                                                    |
| malloc/free/calloc/realloc, C++ new/delete, extended APIs      |
+---------------------------------------------------------------+
| Request classifier                                            |
| size class, alignment, arena, hotness, lifetime, NUMA policy   |
+---------------------------------------------------------------+
| Front-end                                                     |
| per-CPU caches via RSEQ; fallback tcache; debug bypass         |
+---------------------------------------------------------------+
| Middle-end                                                    |
| per-LLC transfer caches; NUMA central free lists; span refill  |
+---------------------------------------------------------------+
| Arena policy domains                                          |
| ownership, decay, hugepage policy, custom hooks, stats         |
+---------------------------------------------------------------+
| Back-end                                                      |
| hugepage filler, huge cache, region cache, extent allocator    |
+---------------------------------------------------------------+
| Operating system interface                                    |
| mmap/munmap/madvise/mprotect/THP/RSEQ/TLS/cgroups             |
+---------------------------------------------------------------+
```

## 6.2 Fast path summary

The default small-object allocation path is:

```text
malloc(size):
    req = classify(size, default alignment, default policy)
    if req.size_class <= front_end_max:
        cpu = get_current_cpu_fast()
        if per_cpu_cache[cpu][req.size_class] not empty:
            return pop_rseq(per_cpu_cache[cpu][req.size_class])
        return refill_then_pop(cpu, req)
    else:
        return large_allocate(req)
```

The default small-object deallocation path is:

```text
free(ptr):
    if ptr == null: return
    meta = classify_pointer(ptr)
    if meta.is_small:
        if sized_delete_hint_valid(meta):
            sc = meta.size_class
        else:
            sc = pagemap_lookup(ptr).size_class
        cpu = get_current_cpu_fast()
        if per_cpu_cache[cpu][sc] has capacity:
            push_rseq(per_cpu_cache[cpu][sc], ptr)
            return
        overflow_flush_then_push(cpu, sc, ptr)
    else:
        large_free(ptr, meta)
```

In the per-CPU pseudocode above, the non-empty/has-capacity test and the `pop_rseq` /
`push_rseq` update are a single restartable transaction, not a check-then-act. The test and the
pointer update commit together or abort together (11.3, 11.4, 33.5); the separate lines are
expository. The emptiness test is therefore not a TOCTOU window.

## 6.3 Slow path summary

The slow path handles all cases that cannot be resolved locally:

* cache refill,
* cache overflow,
* allocation of a new span,
* slab creation,
* large allocation,
* arena migration,
* memory pressure release,
* debug quarantine,
* sampled profiling,
* metadata growth,
* custom extent hook invocation,
* fork handling,
* emergency mode.

The slow path MAY take locks, call OS APIs, update global statistics, and perform expensive validation. The slow path MUST preserve all invariants that the fast path depends on.

## 6.4 Critical separation of concerns

TopoMalloc MUST keep the following concerns separate:

| Concern | Owner |
|---|---|
| User API compatibility | API layer |
| Request classification | classifier |
| Fast local object movement | front-end |
| Cross-CPU reuse | transfer cache |
| Span accounting | central free list |
| Placement policy | arena and backend |
| Physical memory state | backend |
| Statistics and profiling | observability layer |
| Formal invariants | Lean model and invariant checker |

This separation is not merely organizational. It defines proof boundaries. For example, the front-end proves that it transfers ownership of one object between Live and FreeLocal. The central free list proves that it transfers batches between FreeCentral and FreeTransfer. The backend proves that it never releases ranges containing live objects.

# 7. Memory taxonomy and state machine

## 7.1 Global memory states

Every allocator-controlled byte range MUST be classified as one of:

```text
UnmappedExternal
ReservedVirtual
Metadata
FreeBackendRetained
FreeBackendDirty
FreeBackendMuzzy
FreeBackendReleased
FreeCentral
FreeTransfer
FreeLocal
Quarantined
LiveUser
```

Only allocator-owned ranges participate in the ownership invariant. External mappings not owned by the allocator are outside the model.

## 7.2 Object state transitions

For a small object:

```text
FreeBackendRetained -> FreeCentral -> FreeTransfer -> FreeLocal -> LiveUser
LiveUser -> FreeLocal -> FreeTransfer -> FreeCentral -> FreeBackendDirty
FreeBackendDirty -> FreeBackendMuzzy -> FreeBackendReleased
FreeBackendReleased -> FreeBackendRetained -> FreeCentral -> ...
```

Transitions MAY skip layers in optimized cases only if they preserve ownership accounting. For example, a central free list may directly hand a batch to a per-CPU cache when the transfer cache is disabled for a size class.

## 7.3 Span state transitions

A span's state is derived from its objects and backing memory:

| Span state | Definition |
|---|---|
| ActiveNonFull | Contains at least one live or cached object and at least one free object. |
| ActiveFull | All objects are live or in local/transfer cache, none free in the span. |
| EmptyCentral | No live objects; available for central reuse. |
| EmptyBackend | No live objects; returned to backend. |
| Dirty | Empty and physically backed, not yet purged. |
| Muzzy | Empty and lazily purged or discardable. |
| Released | Empty and decommitted/advised away. |

The implementation MAY track more states for efficiency. Exported stats MUST map them into the public taxonomy.

## 7.4 Hugepage state transitions

A hugepage can contain multiple spans and subranges. Its state is summarized by occupancy:

| Hugepage class | Description |
|---|---|
| DenseHot | Mostly live hot objects; preserve hugepage coverage. |
| DenseMixed | Mostly live but with mixed hotness/lifetime. |
| SparseReusable | Substantial free space; good candidate for new allocations. |
| EmptyBacked | No live spans and physically backed. |
| EmptyReleased | No live spans and released to the OS. |
| PartialSubreleased | Some free subpages released while live subpages remain. |

Partial subrelease SHOULD be a last resort because it can reduce hugepage coverage and increase TLB pressure, consistent with the tradeoff described in TCMalloc's tuning documentation [R2].

## 7.5 State machine invariants

M-001. State transitions MUST be explicit in code review, trace logging, and formal model names.

M-002. No transition may silently change the arena owner of a live object.

M-003. No transition may change the size class of an allocated object.

M-004. Decommit/release transitions MUST require proof or runtime evidence that the target range contains no live object.

M-005. Recommit transitions MUST occur before a released page is used for a user allocation.

# 8. Ownership model and invariants

## 8.1 Ownership identity

TopoMalloc uses ownership identity to prevent double allocation and double free. Each allocatable object has a logical object ID. Each object ID is assigned exactly one owner:

```text
Owner =
    Live(application)
  | CpuCache(cpu, size_class)
  | ThreadCache(thread, size_class)
  | TransferCache(domain, size_class)
  | CentralFreeList(arena, size_class)
  | BackendFree(arena)
  | Quarantine(arena, reason)
  | Released(arena)
```

An implementation does not need to store object IDs explicitly for every object in production. It MUST, however, be possible to interpret metadata as implementing this model.

## 8.2 Fundamental invariant

The most important invariant is:

```text
For every object O controlled by TopoMalloc,
there exists exactly one owner Owner(O).
```

This implies:

* O cannot be returned by two concurrent malloc calls.
* O cannot be simultaneously cached and live.
* O cannot be in two free lists.
* O cannot be released while live.
* O cannot be counted twice in statistics.

## 8.3 Live disjointness

All live object ranges MUST be pairwise disjoint:

```text
forall live objects A, B:
    A != B => range(A) does not overlap range(B)
```

This theorem MUST be stated in the Lean model and SHOULD be checked by expensive runtime validation in debug builds.

## 8.4 Free structure uniqueness

An object is free if it appears in a cache, transfer cache, central free list, backend free structure, or quarantine. It MUST appear in exactly one such structure.

Debug validation SHOULD periodically sample free structures and check:

* no duplicate pointer within one list,
* no duplicate pointer across lists,
* pointer belongs to the advertised size class,
* pointer belongs to the advertised arena,
* pointer is not live according to span bitmap,
* pointer's span exists and agrees with pagemap.

## 8.5 Metadata duplication rule

Duplicated metadata is allowed only when one copy is declared authoritative for each transition.

Example:

* The span free count MAY duplicate the free bitmap.
* The free bitmap is authoritative for object state within the span.
* The free count is a cached aggregate.
* Every transition that changes the bitmap MUST update the free count in the same critical section or atomic/RSEQ transaction.

## 8.6 Statistics consistency

Statistics are derived from ownership states. The sum of all exported allocator-owned byte classes SHOULD equal total allocator-managed virtual memory, modulo documented sampling delay, stats epoch behavior, and memory that has been fully unmapped from the managed address space. A `Released` range that was decommitted but retained in virtual memory still counts as managed virtual memory; a `Released` range that was unmapped does not (see 20.1). The accounting identity MUST state which convention the implementation uses so the exported numbers reconcile.

The stats API MUST include an epoch or sequence number. Multi-field reads SHOULD support a consistent snapshot mode for operational debugging.

# 9. Size classes and alignment

## 9.1 Size-class goals

The size-class table controls internal fragmentation and cache behavior. It MUST satisfy:

* coverage for every request size up to `small_max`,
* sufficient alignment for each class,
* bounded rounding waste,
* efficient mapping from size to class,
* good slab packing into pages and hugepages,
* stable ABI behavior within a release series unless explicitly configured otherwise.

## 9.2 Class families

TopoMalloc defines four request families:

| Family | Typical range | Path |
|---|---:|---|
| Tiny | 1 to 64 bytes | per-CPU cache, high density slabs |
| Small | 65 bytes to 32 KiB | per-CPU cache, slabs/spans |
| Medium | 32 KiB to below hugepage threshold | central/arena extent allocator; optional limited cache |
| Large | hugepage threshold and above | hugepage/region backend |

Exact thresholds are configuration parameters. The default target is to cache small objects through 32 KiB or 64 KiB depending on page size and workload class.

## 9.3 Alignment requirements

A class has:

```text
class.size
class.alignment
class.slab_pages
class.objects_per_slab
class.batch_size
class.max_local_capacity
```

For every request `req` mapped to class `c`:

```text
c.size      >= req.size
c.alignment >= req.alignment
c.size      is an integer multiple of c.alignment
```

The third constraint is required, not merely desirable. A slab places object `i` at
`base0 + i * c.size`, where `base0` is aligned to `c.alignment` (§16.3). Every object in
the slab is therefore aligned to `c.alignment` **if and only if** `c.size` is an integer
multiple of `c.alignment`. This is the proof obligation behind "alignment is sufficient"
in §9.5.

Requests whose required alignment exceeds the natural alignment of every front-end-cacheable
class MUST be served by a dedicated over-aligned size class or routed to the medium/large
path (§25.5). The allocator MUST NOT adjust the offset of an individual object inside a
shared uniform slab to satisfy a larger alignment, because variable per-object offsets break
the `base0 + i * c.size` layout, the `objects_per_slab` count, and the pairwise-disjointness
proof of §9.5.

If the implementation supports C++ operator new alignment optimization similar to TCMalloc's documented 8-byte alignment behavior for certain build modes [R1], that behavior MUST be explicitly selected by ABI mode and compiler compatibility checks.

## 9.4 Rounding waste policy

For each request:

```text
waste(req)       = class_size(req) - req.size
waste_ratio(req) = waste(req) / max(req.size, 1)
```

Internal fragmentation has two regimes that MUST be analyzed separately, because a single
flat per-request ratio target is not achievable across all sizes.

**Spacing-dominated regime.** Let `r(c) = size(c) / size(prev(c))` be the ratio of a class
size to the next-smaller class size. The worst case for a request mapped to class `c` is
`req = size(prev(c)) + 1`, which gives

```text
waste_ratio_worstcase(c) ~= size(c) / size(prev(c)) - 1 = r(c) - 1
```

Bounding the per-class spacing ratio `r(c) <= 1 + W` therefore bounds the worst-case
per-request waste to approximately `W`. The spacing ratio is the achievable, measurable
invariant the size-class generator MUST target.

**Alignment-dominated regime.** Let `q` be the alignment quantum for a class range: the
minimum spacing the ABI permits, `q = alignof(max_align_t)` (typically 16 bytes on LP64),
or a smaller `tiny` quantum such as 8 bytes for classes whose required alignment is `<= 8`.
The smallest nonzero spacing is `q`, so for small requests the spacing ratio cannot be made
arbitrarily close to 1, and a per-request waste target `W` is unachievable while
`req < q / W`. In that regime waste is bounded by the quantum, not by `r`:

```text
waste_ratio(req) <= (q - 1) / req     for req in the alignment-dominated regime
```

TopoMalloc SHOULD target the following per-class spacing ratios (equivalently, the
worst-case per-request waste), subject to the alignment-dominated caveat:

| Size range | Max spacing ratio `r` | Worst-case per-request waste | Notes |
|---|---:|---:|---|
| `req < q/W` (tiny; `q=16 B` => roughly `<= 48 B`) | quantum-limited | `<= (q-1)/req` | alignment-dominated; bounded by `q`, not by `r` |
| `q/W` to 128 B (`q=16 B` => ~49 B to 128 B) | `<= 1.33` | `<= ~33%` | 16-byte spacing suffices here |
| 129 B to 1 KiB | `<= 1.20` | `<= ~20%` | |
| 1 KiB to 32 KiB | `<= 1.125` | `<= ~12.5%` | 8 classes per power-of-two group |
| above 32 KiB | page/hugepage granularity | n/a | rounded to page/hugepage units |

Earlier revisions stated a flat `<= 33%` target for "17 to 128 bytes." That target is
mathematically unattainable below `q/W`: a 17-byte request under a 16-byte ABI quantum must
round up to 32 bytes (~88% waste), and no 16-aligned class lies in `(16, 32)`. The bound is
therefore split into the spacing and alignment regimes above so that every published target
is provably achievable.

The exact table MUST be generated from a script or Lean model, not hand-edited in multiple
places. The generator MUST assert, per class, both the spacing-ratio bound for the class's
range and that `size(c)` is an integer multiple of `c.alignment` (§9.3).

## 9.5 Size-class verification

The Lean model MUST prove:

* every request size up to `small_max` maps to a valid class,
* mapped class size is at least requested size,
* alignment is sufficient,
* table indexes are in bounds,
* batch sizes fit local cache capacity,
* slab layout does not overflow the span,
* object ranges in a slab are pairwise disjoint.

## 9.6 Zero-size allocation

TopoMalloc MUST document zero-size allocation behavior. Recommended policy:

* `malloc(0)` returns a minimum-size unique freeable pointer when `compat.zero_unique=true`.
* `malloc(0)` may return null when `compat.zero_null=true`.
* The default should match the dominant platform allocator expectation for ABI compatibility.
* `free(NULL)` MUST be a no-op.

## 9.7 Oversized request handling

Requests that would overflow size calculations MUST fail safely. The implementation MUST check:

* `n * size` overflow in calloc,
* alignment rounding overflow,
* size-class rounding overflow,
* span/page count overflow,
* hugepage rounding overflow,
* metadata indexing overflow.

Failure MUST return null or throw `std::bad_alloc` according to API semantics. It MUST NOT wrap around and allocate too little memory.

# 10. Public APIs

## 10.1 Standard C APIs

TopoMalloc MUST provide:

```c
void* malloc(size_t size);
void  free(void* ptr);
void* calloc(size_t n, size_t size);
void* realloc(void* ptr, size_t size);
```

TopoMalloc SHOULD provide where platform-compatible:

```c
int   posix_memalign(void** memptr, size_t alignment, size_t size);
void* aligned_alloc(size_t alignment, size_t size);
void* memalign(size_t alignment, size_t size);
void* valloc(size_t size);       // optional compatibility
void* pvalloc(size_t size);      // optional compatibility
size_t malloc_usable_size(void* ptr);
void  free_sized(void* ptr, size_t size);                            // C23
void  free_aligned_sized(void* ptr, size_t alignment, size_t size);  // C23
void* reallocarray(void* ptr, size_t n, size_t size);                // BSD/glibc; overflow-checked
```

On allocation failure the C functions MUST set `errno` to `ENOMEM` and return null where the
platform C ABI specifies it, so POSIX callers and `perror`-style diagnostics behave correctly.
`realloc` failure MUST leave the original allocation valid and set `errno` to `ENOMEM`. Per
POSIX, `free` MUST NOT modify `errno`. The C23 `free_sized` / `free_aligned_sized` functions
carry a size the allocator MAY use like a sized delete (10.2); a mismatched size is undefined
behavior in performance mode and SHOULD be sample-checked in hardened/debug mode.

## 10.2 C++ APIs

TopoMalloc SHOULD provide replacement global operators:

```cpp
void* operator new(std::size_t);
void* operator new[](std::size_t);
void* operator new(std::size_t, std::align_val_t);
void* operator new[](std::size_t, std::align_val_t);
void  operator delete(void*) noexcept;
void  operator delete[](void*) noexcept;
void  operator delete(void*, std::size_t) noexcept;
void  operator delete[](void*, std::size_t) noexcept;
void  operator delete(void*, std::align_val_t) noexcept;
void  operator delete[](void*, std::align_val_t) noexcept;
void  operator delete(void*, std::size_t, std::align_val_t) noexcept;
void  operator delete[](void*, std::size_t, std::align_val_t) noexcept;
```

Sized delete hints SHOULD be used to avoid pointer-to-size lookup when valid, following the general principle documented by TCMalloc [R6]. Debug/hardened modes SHOULD sample-check sized delete correctness.

## 10.3 Extended C API

TopoMalloc SHOULD expose:

```c
typedef uint32_t topo_arena_t;
typedef uint32_t topo_tcache_t;
typedef uint64_t topo_flags_t;

topo_arena_t topo_arena_create(const topo_arena_config_t* cfg);
int topo_arena_destroy(topo_arena_t arena);
int topo_arena_reset(topo_arena_t arena);
int topo_arena_purge(topo_arena_t arena);
int topo_arena_set_decay(topo_arena_t arena, int64_t dirty_ms, int64_t muzzy_ms);

void* topo_mallocx(size_t size, topo_flags_t flags);
void* topo_rallocx(void* ptr, size_t size, topo_flags_t flags);
int   topo_xallocx(void* ptr, size_t size, size_t extra, topo_flags_t flags);
void  topo_dallocx(void* ptr, topo_flags_t flags);
void  topo_sdallocx(void* ptr, size_t size, topo_flags_t flags);
size_t topo_nallocx(size_t size, topo_flags_t flags);
```

The naming is illustrative. A real implementation MAY use a different prefix but MUST provide equivalent functionality for conformance.

## 10.4 Extended allocation flags

Recommended flags:

```text
TOPO_ARENA(id)          allocate from explicit arena
TOPO_ALIGN(log2_or_n)   request alignment
TOPO_ZERO              zero returned memory
TOPO_TCACHE_NONE       bypass local cache
TOPO_TCACHE(id)        use explicit tcache where supported
TOPO_HOT(level)        0..255 hotness hint
TOPO_LIFETIME_SHORT    expected short lifetime
TOPO_LIFETIME_MEDIUM   expected medium lifetime
TOPO_LIFETIME_LONG     expected long lifetime
TOPO_NUMA(node)        preferred NUMA node
TOPO_COLD              synonym for hotness near 0
TOPO_GUARDED           sampled or forced guard allocation
TOPO_NO_HUGEPAGE       avoid hugepage placement
TOPO_PREFER_HUGEPAGE   prefer hugepage-backed placement
```

Flags MUST be validated. Invalid flag combinations MUST fail deterministically or ignore documented advisory flags. Mandatory flags such as alignment MUST NOT be silently ignored.

## 10.5 Control API

TopoMalloc MUST expose a structured control namespace. It may be string-based like `mallctl`, object-based like `MallocExtension`, or both.

Required controls:

```text
stats.refresh
stats.read_json
stats.print
cache.flush_all
cache.flush_cpu(cpu)
cache.set_global_budget(bytes)
cache.set_per_cpu_limit(bytes)
release.to_os(bytes)
release.set_rate(bytes_per_second)
arena.create
arena.destroy(id)
arena.reset(id)
arena.purge(id)
arena.set_decay(id, dirty_ms, muzzy_ms)
arena.set_policy(id, policy)
profile.heap.start
profile.heap.stop
profile.heap.dump
profile.lifetime.start
profile.lifetime.stop
emergency.enter
emergency.leave
```

Controls that may block or take global locks MUST be documented.

# 11. Front-end: per-CPU cache

## 11.1 Purpose

The front-end is responsible for making the common small-object allocation and free path extremely fast. It holds free objects in local arrays indexed by CPU and size class. It does not own spans; it owns object references that were transferred from lower layers.

## 11.2 Per-CPU cache structure

Each CPU has a cache descriptor:

```c
struct CpuCache {
    CpuId cpu;
    uint32_t epoch;
    SizeClassCache class_cache[num_size_classes];
    uint64_t bytes_cached;
    uint64_t capacity_bytes;
    uint64_t miss_count;
    uint64_t overflow_count;
};

struct SizeClassCache {
    void** begin;
    void** cur;
    void** end;
    void** dynamic_capacity;
    uint32_t size_class;
    uint32_t count;
};
```

The implementation MAY use a compact slab layout similar in spirit to TCMalloc's documented per-CPU metadata layout [R1]. The abstract invariant is what matters: each per-CPU, per-size-class array contains distinct free objects of the correct size class and arena policy.

## 11.3 Allocation fast path

Requirements:

* The fast path MUST check that the selected size class is front-end cacheable.
* The fast path MUST load the current CPU using a mechanism safe under migration. With RSEQ, the CPU number and update are part of the restartable critical sequence.
* The fast path MUST pop exactly one object or report underflow.
* On success, ownership changes from `CpuCache(cpu, sc)` to `Live(application)`.
* On abort/retry, state is unchanged.
* The fast path MUST NOT allocate metadata, call syscalls, log, or take locks.

Pseudo-code:

```text
small_alloc(req):
    sc = req.size_class
    cpu = rseq_cpu_or_fallback()
    result = rseq_pop(cpu_cache[cpu][sc])
    if result.success:
        publish_live(result.ptr, sc, req)   // usually implicit; sampled path may record
        maybe_sample_alloc(result.ptr, req)
        return result.ptr
    return small_alloc_slow(cpu, req)
```

## 11.4 Deallocation fast path

Requirements:

* `free(NULL)` MUST return immediately.
* Unsized free MUST classify the pointer via pagemap or equivalent metadata.
* Sized free MAY use the size hint to skip lookup when safe.
* The target cache MUST have capacity.
* On success, ownership changes from `Live(application)` to `CpuCache(cpu, sc)`.
* Fast-path deallocation MUST NOT inspect user contents except for optional debug/hardened metadata encoded outside user-visible memory.

Pseudo-code:

```text
small_free(ptr, optional_size):
    meta = classify(ptr, optional_size)
    if meta.requires_slow_path:
        return free_slow(ptr, meta)
    cpu = rseq_cpu_or_fallback()
    result = rseq_push(cpu_cache[cpu][meta.sc], ptr)
    if result.success:
        maybe_sample_free(ptr, meta)
        return
    return small_free_slow(cpu, ptr, meta)
```

## 11.5 Capacity bounds

Each CPU cache has:

```text
hard_capacity_bytes(cpu)
soft_capacity_bytes(cpu)
class_capacity(cpu, sc)
class_count(cpu, sc)
```

Invariant:

```text
sum_sc class_count(cpu, sc) * class_size(sc) <= hard_capacity_bytes(cpu)
```

The soft capacity MAY be exceeded temporarily during refill/flush transitions. The hard capacity MUST NOT be exceeded outside documented critical sections.

## 11.6 Cache idle handling

When a CPU becomes unavailable to the process due to affinity changes, cgroup constraints, hotplug, or long inactivity, TopoMalloc SHOULD flush that CPU's local cache to the appropriate transfer or central layer.

The control plane MUST provide a way to release memory stranded in specific CPU caches, analogous in spirit to public controls that allow releasing memory held by inactive CPU caches in TCMalloc [R1, R2].

## 11.7 Arena interaction with per-CPU and thread caches

A per-CPU (and thread) cache slot is keyed by size class, but on free the object's owning arena
is recovered from the pagemap (6.2, 17.5), so a single `(cpu, sc)` slot could receive frees that
route to different arenas. Because a batch carries exactly one arena (14.2) and an empty span can
only be reclaimed by its owning arena's central list (16.5), the allocator MUST NOT let an object
reach a central or transfer structure under the wrong arena. Two conforming designs satisfy this:

1. **Bound-arena fast path (recommended default).** Per-CPU and thread caches serve a single
   bound arena -- normally the default arena. Allocations and frees for other explicit arenas use
   arena-scoped caches and central lists and MAY bypass the per-CPU fast path. This keeps each
   `(cpu, sc)` slot single-arena and makes the refill (14.3) and flush (14.4) pseudocode exact as
   written for the bound arena.
2. **Arena-partitioned flush.** A `(cpu, sc)` slot MAY hold mixed-arena objects, but flush MUST
   partition the popped objects by arena (recovered via the pagemap) and insert each group into
   its owning arena's transfer/central structure. This costs a classification pass per flush and
   is worthwhile only when explicit-arena traffic is hot enough to need the per-CPU path.

In both designs the fully-qualified indexing is `transfer_cache[domain][arena][sc]` and
`central_free_list[node][arena][sc]`; the refill/flush snippets in 14.3-14.4 and A.4 elide the
arena subscript only because they describe the bound-arena case. The choice MUST be documented
per build. The free-routing rule -- an object always returns to its owning arena's structures --
is a safety property (it follows from ownership uniqueness, 8.2), not a policy.

# 12. RSEQ contract and fallback modes

## 12.1 RSEQ role

RSEQ is used to make per-CPU cache push/pop operations appear atomic with respect to CPU migration and preemption. Linux documentation describes restartable sequences as enabling update operations on per-CPU data without heavyweight atomic operations [R5].

TopoMalloc MUST treat RSEQ as an optional acceleration mechanism, not as a correctness requirement. If RSEQ is unavailable or disabled, the allocator remains correct using fallback synchronization.

## 12.2 Abstract RSEQ contract

TopoMalloc's Lean model and C++ implementation MUST define the following abstract contract:

```text
rseq_pop(cache):
    either AbortUnchanged
    or Success(ptr, state') such that:
        ptr was in cache before
        ptr is removed from cache after
        no other object is changed
        ownership(ptr) changes from CpuCache(cpu, sc) to LivePending
        all invariants are preserved

rseq_push(cache, ptr):
    either AbortUnchanged
    or Success(state') such that:
        ptr was live before
        ptr is added to cache after
        no other object is changed
        ownership(ptr) changes from Live to CpuCache(cpu, sc)
        all invariants are preserved
```

`LivePending` is an implementation-proof state used during an allocation return. It is not visible in runtime metadata, and it collapses to `Owner.live` at the operation's linearization point (27.1). The abstract RSEQ contract (33.5) therefore models a successful pop as transitioning ownership directly from `CpuCache(cpu, sc)` to `Owner.live`; `LivePending` appears only in the finer-grained refinement between the successful RSEQ commit and the return to the caller.

## 12.3 RSEQ implementation requirements

The implementation MUST:

* register RSEQ for each thread when required by platform ABI,
* detect RSEQ availability at startup and thread start,
* use only approved per-architecture sequences,
* maintain an abort handler that leaves logical state unchanged,
* keep RSEQ critical sequences minimal,
* never call functions inside RSEQ critical sections,
* avoid memory references that can fault unexpectedly inside the critical sequence,
* document all clobbers and compiler barriers,
* include tests that force preemption/migration around critical sequences.

## 12.4 Fallback mode requirements

Fallback modes MAY include:

* per-thread cache,
* per-CPU cache protected by a small lock,
* arena-sharded central cache,
* direct central free-list mode for debug builds.

Fallback MUST preserve API and safety behavior. It may have worse throughput.

## 12.5 Dynamic mode transitions

TopoMalloc MAY begin in a conservative mode during early process initialization, then switch to per-CPU mode after TLS, RSEQ, and metadata are initialized. Any transition MUST preserve cached memory and avoid losing ownership information.

# 13. Optional thread cache fallback

## 13.1 Purpose

Thread caches exist for portability and for policy domains that cannot or should not use per-CPU caches. They are not the preferred default on systems with RSEQ.

## 13.2 Thread cache structure

Each thread cache contains per-size-class lists or arrays:

```text
ThreadCache(thread):
    class_cache[sc]
    bytes_cached
    soft_limit
    hard_limit
    last_gc_epoch
    arena_affinity
```

## 13.3 Garbage collection

Thread caches MUST support flushing on:

* thread exit,
* explicit control call,
* arena reset/destroy precondition,
* memory pressure,
* cache over budget,
* inactivity timeout.

## 13.4 Interaction with arenas

If a thread cache contains objects from an explicit arena, arena reset or destroy MUST require either:

* all relevant thread caches are flushed first, or
* the allocator can prove and forcefully drain all relevant caches safely.

This mirrors the key safety idea in jemalloc's explicit arena reset/destroy constraints [R4].

# 14. Middle-end: transfer caches and central free lists

## 14.1 Middle-end responsibilities

The middle-end refills and drains local caches. It owns free objects that are not currently in local caches but are not yet returned to the backend.

Responsibilities:

* batch movement,
* cross-CPU reuse,
* per-size-class contention management,
* span activation/deactivation,
* free object accounting,
* returning empty spans to backend.

## 14.2 Transfer cache

TopoMalloc uses transfer caches as fast batch exchange points. Unlike a single global transfer cache per size class, TopoMalloc SHOULD use topology-aware transfer caches:

```text
TransferCache(domain, size_class)
```

where `domain` is normally an LLC domain. If topology information is unavailable, there is one process-wide domain.

Each transfer cache contains batches:

```text
struct Batch {
    void* objects[N];
    uint16_t count;
    uint16_t size_class;
    ArenaId arena;
    DomainId domain;
};
```

The transfer cache MUST guarantee that all objects in a batch are distinct, correct-size, and free.

## 14.3 Transfer cache refill

```text
refill_cpu_cache(cpu, sc):
    domain = llc_domain(cpu)
    batch = transfer_cache[domain][sc].try_remove_batch()
    if batch.exists:
        cpu_cache[cpu][sc].push_batch(batch)
        return
    batch = central_free_list[numa_node(cpu)][sc].remove_batch()
    if batch.exists:
        cpu_cache[cpu][sc].push_batch(batch)
        return
    span = arena_allocate_span(sc, policy_for(cpu, sc))
    batch = carve_span_batch(span, sc)
    cpu_cache[cpu][sc].push_batch(batch)
```

## 14.4 Transfer cache overflow

```text
flush_cpu_cache(cpu, sc):
    batch = cpu_cache[cpu][sc].pop_batch(flush_size(sc))
    domain = llc_domain(cpu)
    if transfer_cache[domain][sc].has_capacity(batch):
        transfer_cache[domain][sc].insert_batch(batch)
    else:
        central_free_list[numa_node(cpu)][sc].insert_batch(batch)
```

## 14.5 Central free list

A central free list is keyed by NUMA node, arena, and size class:

```text
CentralFreeList(node, arena, size_class)
```

It manages spans and objects. It may maintain:

* non-full slabs,
* empty slabs,
* free object batches,
* span occupancy counters,
* a lock or sharded locks,
* statistics.

## 14.6 Central free-list invariants

C-001. Every object in a central list belongs to that list's size class.

C-002. Every object in a central list belongs to the list's arena or to an arena whose policy permits sharing.

C-003. Empty spans MUST be detected and returned to the backend or kept in an explicit empty-span cache.

C-004. Non-empty spans MUST NOT be released to the backend.

C-005. Lock ordering MUST prevent deadlocks when moving spans between central lists and backend structures.

# 15. Topology awareness: CPU, LLC, NUMA

## 15.1 Rationale

Modern systems have nonuniform memory and cache topology. Treating all CPUs as equivalent can increase cross-socket traffic, reduce cache locality, and strand memory in the wrong domain.

TopoMalloc SHOULD model:

```text
CPU -> LLC domain -> NUMA node -> process/global
```

## 15.2 Topology discovery

The allocator SHOULD discover topology at startup from platform APIs such as sysfs, CPUID, libc, or OS-specific calls. It MUST tolerate missing or inconsistent topology data by falling back to a conservative single-domain model.

The topology snapshot MUST be refreshed when CPU hotplug, affinity, or cgroup constraints change, if the platform exposes reliable notifications. Otherwise, background actions SHOULD periodically detect obvious mismatches.

## 15.3 Placement policy

Default placement:

* allocate from the current CPU's LLC domain,
* prefer current NUMA node for physical backing,
* use arena policy if explicit arena overrides exist,
* keep hot objects local to active CPUs,
* group cold objects away from hot dense hugepages.

## 15.4 Cross-domain rebalancing

Memory MUST NOT be permanently stranded in a domain if other domains need it. The global rebalancer SHOULD move free batches or empty spans from cold domains to high-pressure domains.

Rebalancing preference order:

1. move transfer-cache batches within the same NUMA node,
2. move central free-list batches within the same NUMA node,
3. move empty spans between arenas if policy permits,
4. return memory to backend and reallocate on target node,
5. remote reuse only if the cost is lower than OS allocation or memory pressure demands it.

## 15.5 NUMA policy

TopoMalloc MUST support at least these NUMA modes:

| Mode | Behavior |
|---|---|
| local | Prefer current NUMA node. |
| interleave | Distribute across nodes. |
| bind(node) | Prefer or require a specific node. |
| arena_policy | Use arena-specific placement. |
| OS_default | Do not override OS placement. |

NUMA binding failures MUST be visible in stats.

# 16. Spans, slabs, bitmaps, and object layout

## 16.1 Span purpose

A span is the unit connecting small-object allocation to page and hugepage accounting. A span contains either:

* one slab of equal-size small objects,
* a medium allocation,
* part of a large/huge allocation descriptor,
* metadata, if metadata is allocated from allocator-managed memory.

## 16.2 Span descriptor

Recommended descriptor:

```c
struct Span {
    SpanId id;
    ArenaId arena;
    SizeClassId sc;
    PageId first_page;
    uint32_t page_count;
    HugePageId hugepage;
    SpanState state;
    uint32_t object_count;        // total objects carved from this span
    uint32_t live_count;          // objects currently owned by the application
    uint32_t central_free_count;  // == popcount(free_bitmap): objects resident in the central list
    Bitmap free_bitmap;           // authoritative "resident in central list" state (16.4)
    Bitmap cache_bitmap;          // debug/hardened optional: reconstructs cached residency
    uint32_t generation;
    uint32_t flags;
};

// Canonical conservation law for a span (see 16.4):
//   object_count = live_count
//                + local_cached_count      // per-CPU and thread caches
//                + transfer_cached_count   // transfer caches
//                + central_free_count      // == popcount(free_bitmap)
//                + quarantined_count
// local_cached_count and transfer_cached_count are logical quantities. They are not
// cheaply maintained per span in performance builds and are reconstructed (e.g. from
// cache_bitmap) only in debug/hardened validation. This is why empty-span detection
// (16.5) is one of the hardest accounting problems in the allocator.
```

The production descriptor can be compact. The logical fields MUST be derivable.

## 16.3 Slab layout

For a size class `sc` and span `sp`:

```text
object_i_base = align_up(span.base + slab_header_size, sc.alignment) + i * sc.size
0 <= i < object_count
object_i_range = [object_i_base, object_i_base + sc.size)
```

Requirements:

* object ranges MUST fit inside the span,
* object ranges MUST be disjoint,
* all objects MUST have the same size class,
* all user object starts MUST satisfy alignment,
* any header or bitmap MUST not overlap user object ranges.

## 16.4 Free bitmap

The authoritative slab state SHOULD be a bitmap or equivalent compact representation. A linked free list MAY be used for speed, but if freelist pointers are stored inside free objects, hardened modes SHOULD encode pointers to reduce simple overwrite attacks.

Bitmap invariants:

```text
bit(i) = 1  <=>  object i is currently resident in this span's central free list
central_free_count = popcount(free_bitmap)

object_count = live_count
             + local_cached_count        // per-CPU and thread caches
             + transfer_cached_count      // transfer caches
             + central_free_count         // == popcount(free_bitmap)
             + quarantined_count
```

The free bitmap is authoritative only for *central* residency. An object held in a per-CPU,
thread, or transfer cache is not live, yet its bit is 0 because it is not in the central list.
`local_cached_count` and `transfer_cached_count` are therefore logical quantities that need
not be cheaply exact in performance builds; they MUST be exactly reconstructable in debug
validation (for example from `cache_bitmap`), and the conservation law above MUST then hold
exactly. No object may be counted in two terms simultaneously: the five terms partition
`object_count`.

## 16.5 Empty span detection

A span may be returned to the backend only when, using the canonical terms of 16.4:

```text
live_count            == 0
local_cached_count    == 0
transfer_cached_count == 0
quarantined_count     == 0      (unless quarantine is being drained)
central_free_count    == object_count
```

Because local and transfer caches may hold objects from a span, empty-span detection MUST account for all caches. This is one of the hardest accounting problems in a high-performance allocator.

## 16.6 Span generation counters

Each span SHOULD have a generation counter. The generation increments when a span is recycled for a different size class, arena, or allocation type. Debug and sampled paths SHOULD use the generation counter to detect stale metadata references.

# 17. Metadata, pagemap, and pointer classification

## 17.1 Pagemap purpose

The pagemap maps an address or allocator page to the span or large allocation descriptor that owns it. It is required for unsized free, realloc, usable-size introspection, profiling, and debug checks.

TCMalloc's public design describes using a pagemap to find span and size-class information when size is not known at deallocation [R1]. TopoMalloc adopts this concept.

## 17.2 Pagemap requirements

P-Map-001. Every allocator-owned page MUST map to exactly one descriptor.

P-Map-002. Pages not owned by TopoMalloc MUST map to null or an external sentinel.

P-Map-003. For small-object pages, the descriptor MUST identify arena, span, and size class.

P-Map-004. For large allocations, the descriptor MUST identify allocation base, usable size, alignment, arena, and state.

P-Map-005. Released pages retained in virtual address space MUST still have enough metadata to prevent accidental reuse without recommit.

P-Map-006. Pagemap updates MUST be synchronized with span state transitions.

## 17.3 Metadata placement

TopoMalloc SHOULD separate metadata from user memory where practical. Metadata MAY be allocated from special metadata arenas with stronger protections.

Recommended metadata protections:

* read-mostly metadata may be protected between updates in hardened mode,
* guard pages around metadata superblocks in debug mode,
* metadata never stored in user-live object contents,
* freelist next pointers encoded when stored in free user objects,
* checksums or generation tags for large allocation headers in hardened mode.

## 17.4 Bootstrap allocator

The allocator needs metadata before it can allocate normally. TopoMalloc MUST include a bootstrap metadata allocator with these properties:

* no dependency on public malloc,
* bounded and simple state,
* idempotent initialization,
* safe failure behavior,
* no locks before threading primitives are initialized,
* transition to normal metadata allocator once available.

## 17.5 Pointer classification

Pointer classification returns:

```text
InvalidExternal
Null
SmallObject(span, sc, arena, object_index)
LargeObject(descriptor)
InteriorPointer(descriptor, offset)
MetadataPointer
ReleasedPointer
QuarantinedPointer
```

`free` requires a base pointer returned by allocation APIs. Interior pointers are invalid frees. Hardened/debug modes SHOULD detect many interior pointers and report them.

# 18. Back-end: extent and page management

## 18.1 Back-end responsibilities

The back-end owns virtual address ranges and physical backing state. It provides spans to central free lists and large ranges to large allocations.

Responsibilities:

* reserve virtual memory,
* commit or fault physical backing,
* split and merge extents,
* track dirty/muzzy/released states,
* maintain hugepage-aware structures,
* return memory to OS,
* honor arena extent hooks,
* update pagemap.

## 18.2 Extent descriptor

```c
struct Extent {
    ExtentId id;
    ArenaId arena;
    uintptr_t base;
    size_t length;
    size_t committed_length;
    ExtentState state;
    HugePageRange huge_range;
    uint32_t split_generation;
    uint32_t flags;
};
```

## 18.3 Extent operations

Required operations:

```text
extent_alloc(size, alignment, policy) -> extent
extent_split(extent, prefix_size) -> (left, right)
extent_merge(left, right) -> extent
extent_commit(extent)
extent_decommit(extent)
extent_purge_lazy(extent)
extent_purge_forced(extent)
extent_release(extent)
```

Each operation MUST have preconditions and postconditions in the formal model.

## 18.4 Split and merge rules

Split is allowed only when:

* both resulting ranges are aligned to allocator page size,
* metadata for both resulting ranges can be installed before publication,
* pagemap updates are atomic with respect to readers or protected by locks/epochs,
* neither result overlaps live objects incorrectly.

Merge is allowed only when:

* extents are adjacent,
* extents belong to compatible arenas,
* states are merge-compatible,
* hugepage accounting can be updated consistently,
* no stale descriptors remain visible to pointer classification.

## 18.5 Large allocation path

Large allocations bypass small-object caches:

```text
large_allocate(req):
    arena = choose_arena(req)
    if req.size >= huge_threshold:
        return huge_allocate(arena, req)
    else:
        return medium_allocate(arena, req)
```

Large frees return directly to the backend or quarantine depending on profile.

## 18.6 Region cache

Allocations slightly larger than a hugepage are vulnerable to waste if rounded to multiple full hugepages. TopoMalloc SHOULD include a region cache for awkward sizes, inspired by TCMalloc's hugepage-aware backend documentation [R3].

# 19. Hugepage-aware allocator

## 19.1 Purpose

The hugepage-aware allocator attempts to keep live memory packed into a small number of hugepages while keeping empty hugepages easy to release. This reduces fragmentation and can improve TLB behavior. TCMalloc's Temeraire documentation describes a hugepage-aware allocator whose goals include reducing pageheap size and increasing hugepage usage [R3].

## 19.2 Components

TopoMalloc's hugepage backend has four components:

```text
HugeAllocator: reserves hugepage-aligned virtual address ranges
HugeCache: caches empty backed hugepages for quick reuse
HugePageFiller: packs sub-hugepage allocations into hugepages
RegionCache: handles allocations larger than a hugepage but awkward to round
```

## 19.3 Hugepage filler policy

The filler chooses a hugepage for a span allocation. It scores candidate hugepages:

```text
score =
    packing_bonus
  + locality_bonus
  + lifetime_match_bonus
  + hotness_match_bonus
  + release_preservation_bonus
  - fragmentation_penalty
  - cross_numa_penalty
  - partial_subrelease_penalty
```

The implementation MAY use approximate bins rather than scanning all hugepages.

## 19.4 Hugepage classes

The filler maintains bins by free space and state:

```text
empty_backed
nearly_empty
sparse
medium
nearly_full
full
partial_subreleased
cold_sparse
hot_dense
```

Each hugepage appears in exactly one bin. Bin membership MUST be consistent with hugepage occupancy metadata.

## 19.5 Packing policy

Default policy:

* pack same-lifetime objects together,
* pack hot objects densely when they are long-lived,
* avoid mixing very short-lived and long-lived objects in the same hugepage when possible,
* prefer filling already partially used hugepages over opening new hugepages,
* keep some empty backed hugepages in HugeCache to avoid immediate page faults under bursty load,
* release empty hugepages according to memory pressure and demand prediction.

## 19.6 Partial subrelease policy

Partial subrelease is allowed only when:

* the subrange contains no live object,
* the subrange is aligned to platform release granularity,
* the hugepage is classified as cold/sparse or memory pressure is high,
* the predicted benefit in RSS is greater than the predicted cost in hugepage fragmentation.

Partial subrelease SHOULD be recorded as a metric.

## 19.7 Hugepage coverage metric

TopoMalloc MUST expose:

```text
hugepage.coverage_bytes
hugepage.live_bytes_on_intact_hugepages
hugepage.live_bytes_on_partial_hugepages
hugepage.empty_backed_bytes
hugepage.empty_released_bytes
hugepage.partial_subreleased_bytes
hugepage.fragmentation_bytes
hugepage.coverage_ratio
```

Recommended coverage ratio:

```text
coverage_ratio = live_bytes_on_intact_hugepages / max(live_bytes_total, 1)
```

## 19.8 Hugepage correctness invariants

H-001. A live object range MUST be contained in a committed, nonreleased subrange.

H-002. Hugepage occupancy bytes MUST equal the sum of contained span and large allocation occupancy.

H-003. A hugepage bin assignment MUST match occupancy and state.

H-004. Empty hugepages may be released only after all contained spans are empty and detached.

H-005. Partial subrelease must never release a subpage intersecting a live object.

# 20. Dirty, muzzy, retained, and released memory

## 20.1 State definitions

TopoMalloc uses a state vocabulary inspired by jemalloc's dirty/muzzy decay controls [R4]. The exact OS mechanism varies by platform.

* **Dirty:** free memory that remains physically backed and may contain old data. Reuse is cheap. RSS remains high.
* **Muzzy:** free memory that has been lazily purged or marked discardable. Reuse may be cheap or may fault depending on platform behavior.
* **Retained:** virtual address range retained by the allocator for future use. It may or may not be physically backed.
* **Released:** memory advised away, decommitted, or unmapped such that reuse requires recommit, remap, or page fault.

## 20.2 Decay policy

Each arena has:

```text
dirty_decay_ms
muzzy_decay_ms
release_rate_bytes_per_sec
background_purge_enabled
```

Default policy SHOULD avoid immediate purging in latency-sensitive server mode. Debug or low-RSS mode may use more aggressive decay.

## 20.3 Background purging

Background purging SHOULD:

* run outside application allocation fast paths,
* process arenas fairly,
* respect global memory pressure,
* preserve hugepage coverage when pressure is low,
* yield under CPU pressure,
* expose work done and backlog in stats.

## 20.4 Purge and release operations

```text
purge_lazy(range):
    mark range as discardable if platform supports lazy purge

purge_forced(range):
    discard physical contents promptly if platform supports forced purge

release(range):
    return memory to OS or mark unavailable until recommitted
```

The implementation MUST document platform-specific mappings, such as `madvise` modes on Linux.

## 20.5 Retain versus unmap

Retaining virtual address space can improve reuse and metadata stability. Unmapping can reduce VSS and catch use-after-free in debug mode. TopoMalloc SHOULD retain by default on 64-bit server platforms and MAY unmap more aggressively in embedded or debug configurations.

# 21. Memory release controller

## 21.1 Purpose

The release controller decides when and how unused memory returns to the OS. It balances RSS, page faults, hugepage coverage, latency, and memory pressure.

TCMalloc's tuning documentation explicitly notes that aggressive release can cause refault costs and can break hugepages, increasing TLB misses [R2]. TopoMalloc adopts that tradeoff as a first-class policy input.

## 21.2 Inputs

The controller observes:

```text
live_bytes
rss_bytes
virtual_bytes
metadata_bytes
per_cpu_cache_bytes
thread_cache_bytes
transfer_cache_bytes
central_free_bytes
pageheap_free_bytes
dirty_bytes
muzzy_bytes
released_bytes
hugepage_coverage_ratio
partial_subreleased_bytes
allocation_rate
free_rate
refill_miss_rate
page_fault_rate
cgroup_memory_current
cgroup_memory_max
memory_pressure_notifications
NUMA pressure
application hints
```

## 21.3 Release priority

Default release priority:

1. Drain idle CPU and thread caches.
2. Release completely empty hugepages beyond demand reserve.
3. Purge dirty spans that are not on hot hugepages.
4. Convert dirty to muzzy where cheap.
5. Subrelease cold sparse partial hugepages.
6. Force global cache shrink and emergency release.

## 21.4 Demand reserve

The allocator SHOULD keep a demand reserve:

```text
demand_reserve = f(recent_allocation_rate, recent_peak, refill_latency, memory_pressure)
```

The reserve prevents oscillation where the allocator releases memory only to fault it back immediately.

## 21.5 Memory pressure modes

| Mode | Trigger | Behavior |
|---|---|---|
| Normal | no pressure | preserve hugepages, normal decay |
| Soft pressure | approaching budget | shrink idle caches, release empty hugepages |
| Hard pressure | near limit | accelerate purge, shrink all caches |
| Emergency | allocation failure or cgroup critical | bypass optional caches, release aggressively, disable HugeCache reserve |

## 21.6 Release safety theorem

The Lean model MUST prove:

```text
If WellFormed(s) and release_to_os(s, range) = s',
then every pointer live in s remains live and points to committed memory in s'.
```

# 22. Arena policy domains

## 22.1 Arena purpose

An arena is a policy domain. It owns extents, spans, statistics, decay settings, cache budgets, hugepage policy, NUMA preference, and optional custom hooks.

Arenas are used for:

* default process allocation,
* request-scoped or transaction-scoped memory,
* long-lived indexes,
* cold caches,
* NUMA-local pools,
* debug/hardened subsystems,
* custom memory sources,
* controlled reset/destroy lifecycle.

## 22.2 Arena descriptor

```c
struct Arena {
    ArenaId id;
    char name[32];
    ArenaState state;
    ArenaPolicy policy;
    DecayConfig decay;
    CacheBudget cache_budget;
    HugePolicy huge_policy;
    NumaPolicy numa_policy;
    ExtentHooks hooks;
    ArenaStats stats;
    LockSet locks;
};
```

## 22.3 Arena states

```text
Initializing
Active
Draining
Resetting
Destroyed
```

Allocations are allowed only in Active state. Reset/destroy transitions require cache draining and synchronization.

## 22.4 Arena creation

Arena creation MUST:

* validate policy,
* allocate metadata from a safe metadata arena,
* initialize stats,
* install hooks before first extent allocation,
* publish arena ID only after complete initialization.

## 22.5 Arena reset

Arena reset discards all extant allocations from that arena. It is a dangerous but useful lifecycle operation.

Preconditions:

* arena is explicit, not the default automatic arena unless special mode permits,
* no threads are actively allocating from it,
* all local caches containing arena objects are flushed or invalidated safely,
* caller accepts that outstanding pointers become invalid.

Postconditions:

* no live objects remain in arena accounting,
* all arena extents are returned to backend or retained according to policy,
* arena remains Active unless destroy follows,
* stats record reset generation.

## 22.6 Arena destroy

Arena destroy is reset plus metadata removal. Destruction MUST fail or block if threads are still associated with the arena, unless the API provides a safe revoke protocol.

## 22.7 Arena isolation invariant

For any two arenas A and B:

```text
A != B => no extent belongs to both A and B
A != B => no span belongs to both A and B
A != B => no cache entry is attributed to both A and B
```

Shared global backend structures may contain extents from multiple arenas only if each extent retains its arena identity.

# 23. Extent hooks and custom backing memory

## 23.1 Purpose

Extent hooks allow applications to supply custom memory sources or custom OS policies. jemalloc exposes per-arena extent hooks for allocation, deallocation, commit, decommit, purge, split, and merge behavior [R4]. TopoMalloc adopts the general idea while requiring explicit contracts.

## 23.2 Hook interface

Recommended hook interface:

```c
typedef struct topo_extent_hooks_s {
    void* (*alloc)(void* ctx, size_t size, size_t alignment, bool* zero, bool* commit);
    bool  (*dealloc)(void* ctx, void* addr, size_t size, bool committed);
    bool  (*commit)(void* ctx, void* addr, size_t size, size_t offset, size_t length);
    bool  (*decommit)(void* ctx, void* addr, size_t size, size_t offset, size_t length);
    bool  (*purge_lazy)(void* ctx, void* addr, size_t size, size_t offset, size_t length);
    bool  (*purge_forced)(void* ctx, void* addr, size_t size, size_t offset, size_t length);
    bool  (*split)(void* ctx, void* addr, size_t size, size_t size_a, size_t size_b, bool committed);
    bool  (*merge)(void* ctx, void* addr_a, size_t size_a, void* addr_b, size_t size_b, bool committed);
} topo_extent_hooks_t;
```

## 23.3 Hook contracts

Hooks MUST satisfy:

* returned ranges are aligned,
* returned ranges are at least requested size,
* returned ranges do not overlap existing allocator ranges unless intentionally merging the same extent,
* commit/decommit/purge operations affect only requested subranges,
* split/merge semantics match allocator metadata updates,
* hooks do not call TopoMalloc recursively unless explicitly documented as reentrant-safe,
* hook failures are reported without corrupting allocator state.

## 23.4 Formal hook assumption

The Lean model treats hooks as abstract operations with contracts. A production implementation using unverified hooks remains conditionally correct: allocator correctness assumes hook correctness.

# 24. Lifetime, hotness, and placement policy

## 24.1 Inputs

TopoMalloc can use:

* explicit hot/cold hints,
* explicit lifetime flags,
* C++ hot/cold allocation variants if provided,
* sampled stack traces,
* sampled object lifetime profiles,
* size class,
* arena policy,
* allocation site history,
* free site history,
* NUMA and CPU activity.

TCMalloc's public reference documents a hot/cold hint API in which low values indicate rarely used allocations and high values indicate frequently accessed allocations [R6]. TopoMalloc generalizes this idea.

## 24.2 Lifetime classes

```text
Unknown
Ephemeral        likely freed quickly
Short            request/transaction lifetime
Medium           seconds to minutes
Long             process phase or object-cache lifetime
Persistent       expected to remain until arena reset/process exit
```

## 24.3 Placement decisions

Placement policy decides:

* which arena,
* which NUMA node,
* which transfer domain,
* which central free list,
* which hugepage bin,
* whether to favor dense packing or separation,
* whether to use hugepages,
* whether to avoid cache pollution.

## 24.4 Learning policy

TopoMalloc SHOULD use sampled data to infer allocation-site profiles. It MUST remain safe if profiles are missing or wrong.

Recommended profile record:

```text
AllocationSiteProfile:
    stack_id
    size_class_distribution
    lifetime_histogram
    hotness_estimate
    allocation_rate
    free_rate
    sampled_live_bytes
    confidence
```

## 24.5 Policy safety boundary

A placement decision may affect locality and fragmentation. It MUST NOT affect validity, object size, alignment, or free correctness.

## 24.6 Cold object handling

Cold objects SHOULD be grouped into cold spans or cold hugepages when the allocator has confidence. Cold placement should avoid intermixing rarely accessed objects with hot dense hugepages.

## 24.7 Short-lived object handling

Short-lived objects SHOULD be grouped with other short-lived objects so that whole spans or hugepages become empty together.

## 24.8 Long-lived object handling

Long-lived hot objects SHOULD be densely packed in hugepages to preserve TLB efficiency. Long-lived cold objects SHOULD be packed separately to preserve hot locality.

# 25. Reallocation and aligned allocation

## 25.1 realloc semantics

TopoMalloc MUST implement standard realloc semantics:

```text
realloc(NULL, size) == malloc(size)
realloc(ptr, 0) follows configured platform-compatible behavior
on success, returns pointer to object containing min(old_size, new_size) bytes of old content
on failure, old allocation remains valid
```

## 25.2 In-place growth

In-place growth SHOULD be attempted when:

* old and new sizes map to the same size class,
* a medium allocation can extend into adjacent free extent space,
* a large allocation can merge with a neighboring free extent,
* arena and alignment policies remain satisfied.

## 25.3 In-place shrink

In-place shrink SHOULD be attempted when:

* the new size remains in the same size class,
* large allocation tail pages can be split and returned safely,
* shrink does not create unusable tiny fragments unless policy permits.

## 25.4 Move realloc

Move realloc MUST:

* allocate new object before freeing old object,
* copy only `min(old_usable_size, new_size)` bytes,
* preserve old object on allocation failure,
* respect alignment and arena policy,
* profile the allocation as realloc when sampled.

## 25.5 Aligned allocation

Aligned allocation MUST validate:

* alignment is a power of two when API requires it,
* alignment meets minimum requirements,
* size multiple constraints for APIs such as aligned_alloc,
* rounded size does not overflow.

For over-aligned small objects, TopoMalloc MAY use special aligned size classes or route to medium/large allocation.

# 26. calloc and zeroing policy

## 26.1 calloc overflow

`calloc(n, size)` MUST check multiplication overflow before allocation.

```text
if n != 0 and size > SIZE_MAX / n:
    fail
```

This guard is correct -- it rejects exactly the pairs whose product exceeds `SIZE_MAX`, and the
`n != 0` test avoids a divide-by-zero -- but it is not sufficient on its own. After the product
is known not to overflow, it is still subject to size-class, page, and hugepage rounding, each
of which can overflow per 9.7. calloc MUST also fail (return null, set `errno = ENOMEM`) if any
subsequent rounding would overflow.

## 26.2 Zeroing sources

Memory returned by calloc must be zero. Zeroing may be achieved by:

* OS-provided zero pages for newly committed memory,
* explicit memset,
* cached knowledge that a span is already zero,
* zero-on-free policy in secure arenas.

## 26.3 Zero-state metadata

The allocator MAY track zeroed state:

```text
span.zeroed
extent.zeroed
object.zeroed_sampled_or_debug
```

If zeroed state is tracked, transitions MUST invalidate it when user-visible memory may contain nonzero data.

## 26.4 Security zeroing

Zeroing for calloc is not a secure wipe guarantee. Secure arenas MAY offer zero-on-free or explicit secure wipe APIs, but they MUST document compiler optimization and platform limitations.

# 27. Concurrency and memory ordering

## 27.1 Concurrency model

TopoMalloc operations are linearizable with respect to valid calls. Each public malloc/free operation appears to take effect at a defined logical point:

| Operation | Linearization point |
|---|---|
| per-CPU pop | successful RSEQ final update |
| per-CPU push | successful RSEQ final update |
| transfer cache batch remove | lock-protected batch removal |
| transfer cache batch insert | lock-protected batch insertion |
| central free-list remove | lock-protected object/span state update |
| central free-list insert | lock-protected object/span state update |
| backend extent allocation | publication of descriptor and pagemap |
| release-to-OS | state change to Released after no-live proof |

## 27.2 Lock hierarchy

To avoid deadlocks, locks MUST be acquired in a single global order. The order follows the
data-flow layering front-end -> transfer -> central -> span -> backend, so that the common
refill and flush paths never acquire two middle-end locks in conflicting directions:

```text
Global config lock
  -> Arena registry lock
    -> Arena lock
      -> Transfer cache lock        (per-LLC domain; closer to the CPU)
        -> NUMA central list lock   (per-NUMA node)
          -> Span lock
            -> Backend extent lock
              -> Stats shard lock
```

When two middle-end locks would otherwise be held at once, the transfer-cache lock is
acquired before the central-list lock. Implementations SHOULD prefer release-before-acquire
(hand-over-hand) so that at most one middle-end lock is held at a time; the refill (14.3) and
flush (14.4) paths are written this way and hold no two of these locks simultaneously. This
order may be refined, but it MUST remain a total order, cycles MUST be forbidden, and a
lock-order checker SHOULD enforce it in debug builds.

## 27.3 Atomics

Atomic variables MUST have documented memory order. Defaults:

* counters: relaxed where only approximate stats are required,
* publication of descriptors: release store,
* consumption of descriptors: acquire load,
* state transitions visible to concurrent free/classification: acquire-release,
* lock-free intrusive lists, if any: formally specified or avoided.

## 27.4 Per-CPU cache synchronization

Per-CPU fast paths use RSEQ or fallback locks. Non-owner operations, such as flushing an idle CPU cache, MUST coordinate with the owner CPU's RSEQ operations. Coordination MAY use epochs, stop-the-world for allocator maintenance, per-CPU locks in fallback mode, or OS-supported mechanisms.

## 27.5 ABA and generation risks

Any pointer or descriptor reused across concurrent paths SHOULD carry a generation counter or be protected by epochs. Span descriptors MUST NOT be freed while any thread may still classify a pointer through the pagemap to that descriptor.

## 27.6 Thread lifecycle

On thread start:

* initialize TLS,
* register RSEQ if enabled,
* attach to default arena policy,
* initialize fallback tcache if needed.

On thread exit:

* flush thread-local caches,
* unregister or let libc handle RSEQ according to platform ABI,
* publish final stats,
* avoid calling APIs that may require destroyed thread state.

The allocator's own thread-local storage MUST use a TLS model that cannot re-enter `malloc`
while it is being established -- in practice the initial-exec model, or a static `__thread`
slot reached without a dynamic TLS allocation. General-dynamic TLS can trigger a lazy
allocation on first access, which would re-enter the allocator before its per-thread state
exists and risk unbounded recursion or deadlock. When the allocator is loaded as a shared
library with `dlopen`, where initial-exec TLS may be unavailable, it MUST fall back to an
allocation-free per-thread bootstrap path until TLS is safe, consistent with the phased
initialization of 35.4. This is the threading analogue of the bootstrap-metadata rule S-007.

# 28. fork, signal, and async constraints

## 28.1 fork behavior

In a multithreaded process, `fork()` creates a child with one running thread and potentially locks held by vanished threads. TopoMalloc MUST install atfork handlers where supported.

Recommended behavior:

* pre-fork: acquire global allocator fork lock and quiesce background threads,
* parent post-fork: release locks and resume background threads,
* child post-fork: reset lock states, disable background threads until safe, flush inconsistent per-CPU state or enter conservative mode.

## 28.2 Signal safety

Public malloc/free are generally not async-signal-safe. TopoMalloc MUST document this. Internal logging from signal contexts MUST NOT call malloc.

## 28.3 Reentrancy

TopoMalloc MUST detect dangerous reentrancy during initialization, profiling callbacks, hooks, and error reporting. Reentrant allocation MAY use emergency bootstrap allocation or fail safely depending on context.

## 28.4 Crash handling

Crash-reporting APIs SHOULD be able to dump a minimal allocator summary without taking contended locks or allocating memory. Full stats may be unavailable in crash context.

# 29. Security and hardening

## 29.1 Security goals

TopoMalloc's default profile prioritizes performance but should not be recklessly unsafe. Hardened and debug profiles provide stronger checks.

Security goals:

* make allocator metadata harder to corrupt,
* detect common double-free and invalid-free errors,
* reduce predictability where configured,
* separate metadata from user data where practical,
* provide sampled guard allocation,
* provide quarantine for suspicious frees,
* expose misuse diagnostics.

## 29.2 Metadata protection

Recommended protections:

* out-of-line metadata for large allocations,
* encoded freelist pointers for free small objects,
* guard pages around metadata slabs in debug/hardened mode,
* read-only metadata windows where practical,
* generation counters for spans and large descriptors,
* checksum or tag for large allocation headers,
* strict validation before returning a large allocation to backend.

## 29.3 Double-free detection

Fast mode may not detect all double frees. Hardened mode SHOULD detect:

* immediate double free into the same local cache,
* double free detected during cache flush,
* pointer already in quarantine,
* free of object not marked live in debug bitmap,
* mismatched sized delete for sampled or large allocations.

## 29.4 Quarantine

Quarantine delays reuse of freed objects:

```text
QuarantinePolicy:
    max_bytes
    max_objects
    random_evict
    per_arena_limit
    sampled_only
```

Quarantine MUST be accounted separately. Quarantined objects are free from the application perspective but not available for allocation.

## 29.5 Guarded allocations

TopoMalloc SHOULD support sampled guarded allocations. A guarded allocation places inaccessible pages around a user object to detect overrun or underrun. Guarded allocation is expensive and should be sampled or opt-in.

## 29.6 Junk filling

Debug mode SHOULD support:

* fill newly allocated memory with a pattern,
* fill freed memory with a different pattern,
* optionally verify freed memory pattern before reuse.

Junk filling MUST be disabled by default in performance mode.

## 29.7 Pointer authentication and tagging

On platforms with memory tagging or pointer authentication, TopoMalloc MAY integrate with platform features. Such integration MUST be optional and documented.

# 30. Debugging and sanitization modes

## 30.1 Debug profiles

TopoMalloc defines:

```text
profile=performance
profile=hardened
profile=debug
profile=deterministic_test
profile=low_rss
profile=hugepage_optimized
```

## 30.2 Debug invariant checks

Debug mode SHOULD check:

* all free-list entries are valid,
* all local caches are within capacity,
* no duplicate free objects exist,
* span free counts match bitmaps,
* pagemap agrees with spans,
* hugepage occupancy matches spans,
* arena stats equal sum of owned structures,
* released ranges contain no live objects,
* redzones are intact.

## 30.3 Sanitizer integration

TopoMalloc SHOULD be compatible with AddressSanitizer, MemorySanitizer, ThreadSanitizer, and platform heap-checking tools where practical. It MAY disable custom fast paths under sanitizers to avoid false positives or unsupported assembly sequences.

## 30.4 Deterministic testing mode

Deterministic mode SHOULD:

* disable randomization unless seeded,
* use deterministic cache refill order,
* use reproducible sampling,
* optionally force slow paths,
* optionally force frequent purging,
* expose trace IDs for model replay.

# 31. Statistics, telemetry, and profiling

## 31.1 Statistics principles

Stats MUST answer: where is the memory?

At minimum:

```text
application.live_bytes
application.allocated_bytes_total
application.freed_bytes_total
cache.per_cpu.bytes
cache.thread.bytes
cache.transfer.bytes
central.free_bytes
backend.dirty_bytes
backend.muzzy_bytes
backend.retained_bytes
backend.released_bytes
metadata.bytes
quarantine.bytes
hugepage.coverage_ratio
hugepage.partial_subreleased_bytes
arena.count
arena.destroyed_count
```

TCMalloc's public stats documentation emphasizes that allocator stats contain detailed breakdowns of internal cache and pageheap memory [R7]. TopoMalloc SHOULD make this structured and machine-readable.

## 31.2 Stats snapshot API

Recommended API:

```c
int topo_stats_snapshot(topo_stats_t* out, uint64_t flags);
int topo_stats_json(char* buf, size_t* len, uint64_t flags);
int topo_stats_print(FILE* out, uint64_t flags);
```

Flags:

```text
SUMMARY
BY_ARENA
BY_SIZE_CLASS
BY_CPU
BY_NUMA
BY_HUGEPAGE
CONSISTENT_SNAPSHOT
RESET_PEAKS
```

## 31.3 Profiling

TopoMalloc SHOULD support:

* sampled allocation profiling,
* sampled live heap profiling,
* sampled peak heap profiling,
* sampled lifetime profiling,
* fragmentation profiling by size class and arena,
* per-site hotness profile integration.

## 31.4 Sampling requirements

Sampling MUST:

* avoid hot-path locks,
* use thread-local or per-CPU sampling counters,
* record stack traces on sampled allocations when configured,
* handle deallocation of sampled objects safely,
* account for right-censored lifetimes in lifetime profiling if possible,
* avoid calling malloc recursively from unwinding logic.

## 31.5 Fragmentation metrics

Expose:

```text
internal_fragmentation_bytes = sum(usable_size - requested_size) for sampled or exact allocations
external_fragmentation_bytes = free_backed_bytes not immediately useful for current demand
cache_fragmentation_bytes = bytes stranded in local/transfer caches
hugepage_fragmentation_bytes = backed bytes unavailable due to partial occupancy
metadata_overhead_bytes
```

Exact internal fragmentation for every allocation may require per-allocation requested size tracking. TopoMalloc MAY sample it in performance mode and track exactly in debug mode.

## 31.6 Operational explanations

The stats API SHOULD include an explanation endpoint:

```text
topo_explain_memory():
    "RSS is high because: 2.1 GiB live, 700 MiB per-CPU cache, 1.4 GiB dirty retained, 900 MiB empty hugepages retained due to recent demand."
```

This is not a toy feature. Allocator adoption often depends on making RSS behavior understandable.

# 32. Configuration and control plane

## 32.1 Configuration sources

TopoMalloc may read configuration from:

* environment variables,
* config file,
* linker flags,
* early initialization API,
* runtime control API,
* build-time constants.

Precedence MUST be documented. Security-sensitive environments may disable environment configuration.

## 32.2 Recommended knobs

```text
topo.profile
topo.per_cpu.enabled
topo.rseq.enabled
topo.tcache.enabled
topo.cache.global_budget
topo.cache.per_cpu_soft_limit
topo.cache.per_cpu_hard_limit
topo.small_max
topo.page_size
topo.hugepage.enabled
topo.hugepage.size
topo.hugepage.reserve
topo.release.rate
topo.release.aggression
topo.dirty_decay_ms
topo.muzzy_decay_ms
topo.background_threads
topo.numa.mode
topo.sample.heap_rate
topo.sample.lifetime_rate
topo.security.quarantine_bytes
topo.debug.redzones
topo.debug.junk_fill
topo.abort_on_corruption
```

## 32.3 Safe defaults

Default server profile SHOULD be:

* per-CPU enabled if RSEQ is available,
* fallback tcache enabled otherwise,
* hugepage-aware backend enabled when platform is suitable,
* moderate cache budget,
* moderate dirty/muzzy decay,
* background purge enabled,
* sampled profiling available but low overhead,
* corruption abort enabled for severe metadata violations,
* debug fills disabled.

## 32.4 Runtime changes

Runtime changes MUST be validated. Some changes are immediate; some affect only future allocations; some require quiescence.

| Change | Required behavior |
|---|---|
| cache budget | immediate or gradual shrink/grow |
| decay time | future purge scheduling plus optional immediate decay |
| hugepage policy | future backend placements; existing memory unchanged unless rebalancer acts |
| arena destroy | requires explicit call and preconditions |
| RSEQ enabled | startup-only unless implementation proves safe transition |
| page size | build-time only |

# 33. Lean specification and proofs

## 33.1 Purpose

Lean is used to define the allocator's abstract state machine and prove invariants. It is not required to generate production hot-path code.

The formal model should be executable enough to replay traces and abstract enough to avoid irrelevant machine details.

## 33.2 Core Lean types

Schematic model:

```lean
abbrev Addr := Nat
abbrev Bytes := Nat
abbrev CpuId := Nat
abbrev ArenaId := Nat
abbrev SizeClassId := Nat
abbrev BlockId := Nat
abbrev SpanId := Nat
abbrev HugePageId := Nat

structure Range where
  base : Addr
  len  : Bytes
  len_pos : len > 0

inductive Owner where
  | live
  | cpuCache (cpu : CpuId) (sc : SizeClassId)
  | threadCache (tid : Nat) (sc : SizeClassId)
  | transferCache (domain : Nat) (sc : SizeClassId)
  | centralFree (arena : ArenaId) (sc : SizeClassId)
  | backendFree (arena : ArenaId)
  | quarantine (arena : ArenaId)
  | released (arena : ArenaId)
```

## 33.3 Well-formedness predicate

`WellFormed(state)` MUST include:

* live ranges are pairwise disjoint,
* every block has exactly one owner,
* caches contain only blocks owned by those caches,
* central lists contain only blocks owned by those lists,
* span objects fit within span ranges,
* span bitmaps agree with object ownership,
* pagemap agrees with span descriptors,
* hugepage occupancy agrees with spans,
* arena ownership is unique,
* released ranges contain no live blocks,
* cache capacities are respected.

## 33.4 Required theorems

Required proofs:

```text
malloc_preserves_wellformed
malloc_success_returns_aligned_sufficient_disjoint_object
free_preserves_wellformed_for_valid_pointer
free_removes_liveness_and_adds_exactly_one_free_owner
cache_refill_preserves_ownership_conservation
cache_flush_preserves_ownership_conservation
central_batch_remove_preserves_wellformed
central_batch_insert_preserves_wellformed
span_split_preserves_disjointness
span_merge_preserves_disjointness
pagemap_lookup_sound
release_to_os_preserves_live_objects
arena_reset_invalidates_only_target_arena
arena_destroy_preserves_other_arenas
size_class_table_covers_all_small_requests
```

## 33.5 RSEQ abstraction

RSEQ should initially be modeled as an axiom or trusted primitive with a precise contract.
The contract MUST distinguish three outcomes -- abort, empty, and success -- and MUST include
a frame condition stating that at most the popped object changes owner. The frame condition is
load-bearing: the cache refill/flush conservation theorems (33.4) cannot be proved if a pop is
permitted to silently disturb any other object.

```lean
inductive RseqPop (s : State) (cpu : CpuId) (sc : SizeClassId) where
  | abort                              -- preempted or migrated; state unchanged, caller retries
  | empty                              -- no object of class sc on this CPU; caller takes the slow path
  | success (p : Addr) (s' : State)

axiom rseq_pop_contract :
  ∀ (s : State) (cpu : CpuId) (sc : SizeClassId),
    WellFormed s →
    match rseqPop s cpu sc with
    | .abort        => True
    | .empty        => cpuCacheCount s cpu sc = 0
    | .success p s' =>
        OwnerOf s  p = Owner.cpuCache cpu sc ∧
        OwnerOf s' p = Owner.live ∧
        WellFormed s' ∧
        (∀ q, q ≠ p → OwnerOf s' q = OwnerOf s q)   -- frame: only p changes owner
```

`rseq_push` has the symmetric contract: `abort` (retry), `full` (cache at capacity, caller
flushes), or `success` moving `Owner.live` to `Owner.cpuCache cpu sc` under the same frame
condition. The `abort` and `empty`/`full` cases are deliberately distinct: `abort` means the
hardware sequence was interrupted and the operation must be retried, whereas `empty`/`full`
are genuine logical underflow/overflow that route to the slow path. Conflating them would let
the model "prove" progress that the implementation does not make. Later work MAY refine these
axioms to verified per-architecture assembly (open question 7).

## 33.6 Generated tables

Lean SHOULD generate or verify:

* size-class table,
* size-to-class lookup table,
* slab object counts,
* batch sizes,
* capacity limits,
* alignment masks,
* page and hugepage size relationships.

## 33.7 Trace replay

Production builds SHOULD optionally emit compact traces:

```text
ALLOC request_id size align arena flags -> ptr usable_size sc span
FREE ptr size_hint -> sc span
REFILL cpu sc count source
FLUSH cpu sc count target
SPAN_ALLOC arena sc span pages hugepage
RELEASE range state
```

A Lean executable model SHOULD replay traces and check invariants at trace boundaries.

## 33.8 Relation to verified allocator research

Fully verified real-world allocators are possible but difficult. StarMalloc demonstrates a formally verified concurrent allocator in a different verification stack [R9]. TopoMalloc's immediate formal goal is narrower: machine-check the design invariants and high-risk tables, then progressively connect implementation paths to the model.

# 34. Testing, benchmarking, and validation

## 34.1 Test categories

TopoMalloc MUST include:

* unit tests,
* property tests,
* randomized allocation trace tests,
* concurrency stress tests,
* fork tests,
* alignment tests,
* overflow tests,
* arena lifecycle tests,
* hugepage backend tests,
* memory pressure tests,
* RSEQ abort/migration tests,
* sanitizer builds,
* differential tests against the Lean model,
* ABI compatibility tests.

## 34.2 Unit tests

Unit tests cover:

* size-class mapping,
* alignment rounding,
* slab layout,
* span split/merge,
* bitmap operations,
* pagemap lookup,
* cache push/pop,
* transfer batch insert/remove,
* central free-list span accounting,
* hugepage filler bin membership,
* release controller decisions,
* stats aggregation.

## 34.3 Property tests

Random traces should generate operations:

```text
malloc(size, flags)
free(live_ptr)
realloc(live_ptr, new_size)
calloc(n, size)
aligned_alloc(alignment, size)
arena_create(policy)
arena_reset(arena)
arena_destroy(arena)
flush_cache(cpu)
release_to_os(bytes)
```

Properties:

* no duplicate live pointer,
* live contents preserved across realloc,
* alignment satisfied,
* stats nonnegative,
* ownership conservation,
* model and implementation agree on abstract outcomes.

## 34.4 Concurrency tests

Concurrency stress MUST cover:

* many threads allocating same size class,
* many threads allocating mixed size classes,
* cross-thread free patterns,
* producer/consumer ownership transfer,
* CPU affinity changes,
* thread exit with full caches,
* background purge during allocation,
* arena reset races rejected by API,
* memory pressure during high allocation rate.

## 34.5 RSEQ tests

RSEQ-specific tests SHOULD force:

* CPU migration during attempted critical sequence,
* signal delivery near critical sequence,
* preemption around push/pop,
* fallback after RSEQ registration failure,
* comparison against locked implementation.

## 34.6 Benchmark suite

Benchmarks MUST include more than allocator-call time. Include:

* throughput of malloc/free hot paths,
* cross-thread producer/consumer,
* many-thread idle cache footprint,
* cache churn workloads,
* allocation-size distributions from real services,
* database/cache/index workloads,
* fragmentation over long traces,
* RSS under phase changes,
* page fault rate,
* TLB miss rate where available,
* hugepage coverage,
* tail latency of allocation and application operations.

## 34.7 Correctness under memory pressure

Tests MUST simulate:

* cgroup limit approach,
* allocation failure,
* mmap failure,
* madvise failure,
* hugepage collapse failure,
* NUMA binding failure,
* metadata allocation failure.

Failure behavior MUST be deterministic and documented.

## 34.8 Fuzzing

Fuzzers SHOULD target:

* public API sequences,
* extended flags,
* arena lifecycle,
* control plane inputs,
* stats JSON generation,
* custom extent hook failure behavior,
* corrupted metadata in debug harnesses.

# 35. Deployment and ABI compatibility

## 35.1 Deployment modes

TopoMalloc may be deployed by:

* static linking,
* dynamic linking as the process allocator,
* LD_PRELOAD-like override where safe,
* language runtime integration,
* per-subsystem explicit arena use.

LD_PRELOAD-style deployment is tricky and MUST be documented as such because early initialization, interposition, and mixed allocators can cause subtle failures.

## 35.2 Mixed allocator risks

Memory allocated by TopoMalloc MUST be freed by TopoMalloc. Memory allocated by another allocator MUST NOT be freed by TopoMalloc unless an explicit compatibility bridge exists.

TopoMalloc SHOULD detect obvious foreign pointers in hardened/debug mode and fail safely.

## 35.3 ABI stability

Within a stable release series:

* public C API names and struct ABI MUST remain stable,
* opaque handles SHOULD be used where possible,
* stats JSON fields SHOULD be additive,
* size-class table changes are allowed but MUST be documented because they can affect memory footprint and performance.

## 35.4 Initialization

Initialization phases:

```text
Phase 0: static zero state
Phase 1: bootstrap metadata allocator
Phase 2: OS feature discovery
Phase 3: arena registry and default arena
Phase 4: per-CPU/RSEQ setup
Phase 5: background threads and profiling
Phase 6: normal operation
```

Each phase MUST be reentrancy-safe.

## 35.5 Shutdown

At process exit, TopoMalloc SHOULD avoid complex teardown unless explicitly requested. Many allocators intentionally leak allocator metadata at exit because the OS reclaims process memory. Explicit teardown for tests MUST be available.


# 36. seLe4n integration profile

The seLe4n/seL4-style integration profile defined in this section is a **required** part of
TopoMalloc, not an optional add-on (see the conformance model under "Normative language"). A
conforming implementation MUST provide the capability-backed arena model, the backing-provider
contract, label-partitioned caches and statistics, and the arena revocation protocol specified
here, and MUST ship the Lean bridge model of 36.3.3. What remains configurable *within* the
profile is the deployment style (36.18) and the use of dynamic retype versus fixed-arena
operation -- not whether the profile exists. On hosts that are not capability-based microkernels
the POSIX backend (Section 18) remains the default runtime backend, but the seLe4n bridge model
and the fixed-arena profile MUST still be built and proved. The profile MUST remain a user-level
or service-level component: requiring it does not permit placing TopoMalloc inside the kernel
core (3.2, 36.2).

## 36.1 Fit assessment

TopoMalloc and seLe4n are a good architectural fit when TopoMalloc is treated as a user-space allocator and memory-resource service for seLe4n-hosted components. They are a poor fit if TopoMalloc is inserted as an unrestricted dynamic allocator inside the microkernel core.

The fit is strong for five reasons:

1. seLe4n is a Lean-first, capability-based microkernel with executable transitions and machine-checked invariants. TopoMalloc is likewise specified as a Lean-first allocator with explicit ownership, object-state, and release-to-backing invariants.
2. seLe4n preserves the seL4-style model in which authority over memory is explicit. TopoMalloc's arena policy domains can be made capability-backed rather than implicitly global.
3. seLe4n has service orchestration as a first-class extension. A TopoMalloc resource server can be represented as a service with explicit dependencies, lifecycle, health, and authority boundaries.
4. seLe4n's active SMP work and per-core scheduler model align with TopoMalloc's per-CPU/per-core cache design, provided migration and preemption are explicitly modeled.
5. Upstream seL4 user-level memory management already uses libraries such as allocman, VKA, and VSpace helpers. TopoMalloc can be specified as a more formally disciplined successor or complement for dynamic heap and backing-resource management.

The primary caveat is that a verified microkernel should keep kernel-mode dynamic allocation extremely limited. TopoMalloc MUST NOT become a hidden kernel heap used by arbitrary kernel paths. The seLe4n integration profile is therefore defined as a user-level or service-level integration profile with a tiny bootstrap-only kernel-adjacent subset.

Fit assessment details:

* **Proof culture:** TopoMalloc matches seLe4n well because both benefit from Lean state-machine models and invariant preservation proofs. TopoMalloc's seLe4n mode MUST provide a Lean bridge and theorem checklist.
* **Capability model:** TopoMalloc matches seLe4n if all backing memory and control rights are represented by capabilities. Arenas MUST be capability-backed and quota-bounded.
* **Kernel placement:** TopoMalloc SHOULD NOT run as a general heap inside the seLe4n kernel. The kernel core MUST use static allocation or bounded bootstrap allocation only.
* **Existing memory facilities:** TopoMalloc SHOULD complement, not replace, boot-time/static resource assignment, VSpace mechanisms, CSpace mechanisms, and object-lifecycle operations.
* **Large mappings:** Hugepage-aware placement MAY be useful on the Raspberry Pi 5 target, but the seLe4n profile MUST treat hugepage support as an optional large-mapping abstraction rather than requiring Linux THP or x86-style hugepages.
* **Per-core caches:** The per-CPU cache design remains relevant without Linux RSEQ, but seLe4n mode MUST use a different contract: pinned-core affinity, restartable sections, bounded preemption-disabled fast paths, or a thread-cache fallback.

## 36.2 Integration boundary

The seLe4n profile has three layers:

```text
seLe4n kernel core
  - capabilities, scheduling, IPC, CSpace, VSpace, retype, information flow
  - no general-purpose TopoMalloc heap on arbitrary kernel paths

TopoResourceServer user-space component
  - owns delegated untyped/frame/CSpace/VSpace authority
  - creates, maps, revokes, and accounts backing memory
  - exposes arena creation, quota, purge, and statistics operations over IPC

libtopomalloc-sele4n client runtime
  - implements malloc/free/calloc/realloc and explicit arena APIs
  - serves small allocations from local caches
  - requests backing memory from TopoResourceServer only on slow paths
```

This separation preserves the microkernel contract. The kernel continues to decide whether capability invocations are authorized. TopoMalloc decides how already-authorized backing resources are subdivided into heap objects.

A conforming seLe4n integration MUST obey these boundary rules:

* TopoMalloc MUST NOT create kernel objects without holding the corresponding authority capability.
* TopoMalloc MUST NOT hide capability authority inside untyped integer handles.
* TopoMalloc MUST NOT free or recycle memory across security domains unless the required capability revocation, unmapping, and scrubbing protocol has completed.
* TopoMalloc MUST treat kernel retype, CSpace mutation, and VSpace mapping as fallible operations.
* TopoMalloc MUST preserve a clear distinction between allocator metadata, client payload memory, capability slots, virtual address reservations, and physical backing.
* TopoMalloc SHOULD place resource-server metadata in a separately protected address-space region that clients cannot write.
* TopoMalloc MAY expose a compatibility layer for C/C++ `malloc`, but explicit arena APIs are preferred for security-sensitive seLe4n components.

## 36.3 Component architecture

The integration SHOULD be decomposed into the following components.

### 36.3.1 TopoResourceServer

`TopoResourceServer` is a user-space memory authority component. It owns or is delegated a set of untyped capabilities, empty CSlots, and VSpace mapping authority. It provides backing memory to clients and maintains global accounting.

Responsibilities:

* classify boot-provided untyped memory into normal RAM, device memory, reserved kernel-adjacent ranges, DMA-capable ranges, and unavailable ranges;
* split and retype untyped memory using a largest-first policy to reduce watermark waste;
* create frame capabilities for heap backing;
* reserve and map client VSpace windows;
* allocate CSlots for derived capabilities;
* enforce client quotas;
* revoke, unmap, delete, and recycle derived capabilities when arenas are destroyed;
* maintain authoritative accounting for physical backing, mapped ranges, capability ownership, and per-domain heap quotas;
* expose stats and debug snapshots subject to information-flow policy.

The server MAY be replicated per NUMA node or per resource domain on future non-Raspberry-Pi-5 platforms. The first implementation SHOULD be single-server and single-node unless the seLe4n SMP proof layer already exposes the needed cross-core resource invariants.

### 36.3.2 libtopomalloc-sele4n

`libtopomalloc-sele4n` is the client-side allocator runtime. It provides standard allocation APIs and explicit arena APIs. It owns small-object caches and local metadata for objects already mapped into the client.

Responsibilities:

* serve small allocations without IPC on the common path;
* batch slow-path requests to the resource server;
* preserve per-arena labels, quotas, and cache budgets;
* flush or quarantine cached objects before arena destruction, capability revocation, thread migration, or domain transfer;
* keep client-visible statistics that are safe for the client's security label;
* provide deterministic/debug modes for verification and replay.

The client library MUST be correct even if the resource server denies a slow-path request. Denial can occur because of quota exhaustion, missing capability authority, CSlot exhaustion, VSpace exhaustion, information-flow policy, or memory pressure.

### 36.3.3 Lean bridge modules

The integration SHOULD add a bridge package with a shape similar to:

```text
TopoMalloc/SeLe4n/
  Bridge.lean              -- relation between TopoState and seLe4n SystemState
  CapBackedArena.lean      -- arena capabilities, rights, labels, quotas
  UntypedProvider.lean     -- abstract retype/revoke provider contract
  VSpaceProvider.lean      -- abstract map/unmap provider contract
  CSpaceProvider.lean      -- CSlot allocation and derivation contracts
  ResourceServer.lean      -- server state machine and IPC-visible operations
  ClientRuntime.lean       -- client-side cache/refill/free transitions
  InformationFlow.lean     -- label-sensitive heap observations
  SMP.lean                 -- per-core cache and migration contracts
  Refinement.lean          -- preservation and simulation theorems
```

These modules SHOULD import TopoMalloc's allocator model and seLe4n's public model interfaces, not private implementation details. The bridge MUST be written so that TopoMalloc can still be built without seLe4n.

## 36.4 Capability-backed arenas

In the seLe4n profile, an arena is not merely an allocator policy domain. It is also a capability-controlled resource domain.

```text
TopoArena = {
  arena_id,
  owner_service_or_process,
  authority_cap,
  label,
  quota_bytes,
  backing_provider,
  cspace_provider,
  vspace_provider,
  cache_budget,
  release_policy,
  debug_policy,
  lifecycle_state
}
```

A TopoMalloc implementation SHOULD define the following capability-like authorities. The exact concrete representation depends on seLe4n's object model and CSpace conventions.

The logical authority set SHOULD include:

* `TopoHeapServiceCap`: authority to request heap service operations from `TopoResourceServer`. It is delegable to client services.
* `TopoArenaCap`: authority over one arena's allocations, frees, stats, and lifecycle operations. It is delegable only in attenuated form.
* `TopoBackingCap`: authority for a resource server to consume a bounded set of untyped/frame backing resources. It is restricted to the resource authority layer.
* `TopoControlCap`: authority to tune cache budgets, release policy, and profiling. It is restricted to operator or control services.
* `TopoStatsCap`: authority to observe statistics at an allowed security label. It is delegable according to information-flow policy.
* `TopoEmergencyCap`: authority to draw from a bounded emergency reserve. It is restricted to fault handlers or critical services.

Arena capabilities MUST be attenuable. For example, a parent service may delegate an arena capability that permits allocation and free but not stats, purge, destroy, or quota enlargement.

Mandatory arena-capability invariants:

* **Authority monotonicity:** a delegated arena capability MUST NOT grant rights absent from the parent authority.
* **Quota monotonicity:** delegated quota MUST be less than or equal to the delegator's remaining quota.
* **Label monotonicity:** delegation MUST preserve or restrict the arena's information-flow label; it MUST NOT silently downgrade memory from a high domain into a low domain.
* **Ownership uniqueness:** a live object belongs to exactly one arena and therefore to exactly one authority domain.
* **Revocation safety:** arena destruction MUST revoke or invalidate all derived allocation authority before backing memory is reused for a different label.

## 36.5 Boot-time resource acquisition

At boot, seLe4n-style systems pass physical memory authority to an initial/root task through capabilities. The TopoMalloc integration SHOULD start from that root authority and create a resource-server-owned resource inventory.

Boot algorithm:

```text
sele4n_topomalloc_bootstrap(boot_info, root_authority):
    create static bootstrap metadata region
    enumerate untyped capabilities from boot_info
    classify untyped caps by device flag, size, physical address, and label
    reserve server CSpace slots and server VSpace metadata windows
    allocate emergency reserve before enabling dynamic clients
    split normal untyped memory largest-first into backing pools
    initialize TopoResourceServer state
    mint initial TopoHeapServiceCap and TopoArenaCap values
    start accepting heap IPC requests only after invariants check
```

Boot best practices:

* Device untyped memory MUST NOT be placed in the normal malloc backing pool.
* DMA-capable memory SHOULD be managed by a separate DMA arena with explicit cache-coherency and device-isolation policy.
* The server SHOULD reserve CSlots before allocating frames, because a frame without a place to store its capability cannot be safely delegated.
* The server SHOULD reserve VSpace windows before mapping backing frames, because VSpace exhaustion after retype can otherwise complicate recovery.
* Normal backing pools SHOULD be split largest-first to reduce alignment and watermark loss.
* The bootstrap allocator SHOULD be monotonic and no-free until TopoResourceServer proves its own dynamic metadata allocator safe.
* The boot sequence MUST record enough provenance to prove that every mapped heap page descends from authorized untyped memory.

## 36.6 Backing-provider contract

TopoMalloc's ordinary POSIX backend uses `mmap`, `madvise`, `mprotect`, and related OS interfaces. The seLe4n backend MUST instead use an explicit backing-provider contract.

```text
trait TopoBackingProvider {
    reserve_window(arena, size, align, rights) -> VSpaceWindow
    create_frame(arena, size_bits, label) -> FrameCap
    map_frame(arena, frame, window, rights, cache_policy) -> MappedRange
    unmap_frame(arena, mapped_range) -> FrameCap
    revoke_descendants(arena, cap) -> RevocationResult
    delete_cap(arena, cap) -> DeleteResult
    recycle_untyped(arena, untyped) -> RecycleResult
}
```

The provider MUST expose a state machine compatible with both seLe4n capability semantics and TopoMalloc memory states:

```text
AuthorizedUntyped
  -> ReservedUntyped
  -> FrameCapMinted
  -> MappedToServer
  -> MappedToClient
  -> AllocatorCommitted
  -> AllocatorDirty
  -> AllocatorMuzzyOrScrubbed
  -> Unmapped
  -> Revoked
  -> RecyclableUntyped
```

Required provider properties:

* `create_frame` MUST consume only authorized, non-device backing unless the arena is explicitly a device/DMA arena.
* `map_frame` MUST install mappings only into an authorized VSpace window.
* `unmap_frame` MUST make the range unreachable to the client before revocation or label transfer.
* `revoke_descendants` MUST complete before memory is returned to a pool that can serve another authority domain.
* Recycled untyped memory MUST NOT retain live client mappings or live client capabilities.
* Backing-provider failure MUST leave TopoMalloc and seLe4n state well-formed.

## 36.7 Mapping TopoMalloc memory states to seLe4n resources

State mapping requirements:

* `Live`: represented by a mapped frame, a live client pointer, and valid arena authority. It is not reusable by any arena. It belongs to one label and authority.
* `FreeInLocalCache`: represented by a mapped frame with no live object and a pointer cached in the client. It is reusable by the same arena only. The cache must be flushed before revocation.
* `FreeInTransferCache`: represented by a mapped frame with no live object and a server/client batch state. It is reusable by the same arena and MAY be reusable by another arena with the same label. Cross-label mixing requires scrub and policy approval.
* `CentralFree`: represented by a mapped or server-held frame/span. It is reusable by the same arena and MAY be reusable by another compatible arena after label and quota checks.
* `Dirty`: represented by memory with no live object that may contain old data. It is reusable by the same arena only and MUST be scrubbed before cross-domain use.
* `MuzzyOrScrubbed`: represented by memory with no live object whose contents are invalid or zeroed by policy. It is reusable by another arena only after revocation and unmap obligations are complete.
* `RetainedUnmapped`: represented by a frame capability whose client mapping has been removed. It can be reused by the same authority or delegated according to policy, but stats must not leak high-domain history.
* `Released/Recycled`: represented by backing whose descendants are revoked and whose untyped/frame authority has returned to the provider. It is reusable only through a new authorized provider transition.

The important difference from POSIX is that `Released` does not merely mean that the OS may reclaim physical pages. In seLe4n mode it means that the allocator has completed the necessary capability and mapping transition to make the backing resource safely reusable under the seLe4n authority model.

## 36.8 VSpace, CSpace, and metadata layout

TopoMalloc's seLe4n profile MUST treat VSpace and CSpace as scarce allocator resources.

### 36.8.1 VSpace windows

Each arena SHOULD reserve one or more VSpace windows. A VSpace window is a contiguous virtual region into which resource-server-provided frames can be mapped. It has:

* base address,
* length,
* guard pages or redzones in debug mode,
* rights mask,
* cache policy,
* owner arena,
* information-flow label,
* mapping-generation counter.

The allocator MUST NOT assume that virtual address space is as cheap as it is on 64-bit Linux. Raspberry Pi 5 targets are 64-bit, but seLe4n deployments may choose smaller address-space layouts or statically partitioned VSpaces.

### 36.8.2 CSpace slots

Every retyped object or frame capability needs a CSlot. CSlot exhaustion is therefore an allocator failure mode. TopoMalloc's seLe4n profile MUST account for CSlots explicitly.

Per-arena accounting SHOULD include:

```text
arena.cslots.reserved
arena.cslots.used_frame_caps
arena.cslots.used_untyped_caps
arena.cslots.used_control_caps
arena.cslots.free
arena.cslots.high_watermark
```

Slow-path allocation MUST fail cleanly if there are not enough CSlots for the requested backing operation.

### 36.8.3 Metadata isolation

The resource server's authoritative metadata SHOULD be mapped only in the server. Client metadata MAY exist for fast-path caches, but it MUST be treated as a cache of authority, not the authority itself. A malicious or corrupted client MUST NOT be able to cause the server to revoke the wrong capability, map an unauthorized frame, or reassign memory across labels merely by corrupting client-side allocator metadata.

## 36.9 Large mappings and hugepage policy on seLe4n

TopoMalloc's hugepage-aware design should be generalized for seLe4n as a **large mapping policy** rather than a Linux-specific transparent huge page policy.

The seLe4n profile SHOULD define:

* `normal_page_size`: the smallest mappable frame size used for heap backing;
* `large_mapping_size`: a platform-supported block/page size suitable for dense arenas, if available;
* `contiguous_run_size`: a run of normal pages treated as a large-placement unit when hardware large mappings are unavailable;
* `mapping_granule`: the smallest unit that can be unmapped, scrubbed, or revoked independently;
* `coverage_metric`: the fraction of live heap bytes served by large mappings or contiguous dense runs.

Rules:

* Large mappings MUST be optional.
* The allocator MUST remain correct when every backing range is made of normal pages.
* Large mappings SHOULD be used only for arenas whose lifetime and density justify them.
* Partial release from a large mapping SHOULD be avoided unless memory pressure or label transfer requires it.
* The resource server MUST prefer whole-large-mapping release over partial subrelease when possible.
* If large mappings are not exposed by the seLe4n VSpace backend, TopoMalloc SHOULD still use the same placement model over contiguous normal-frame runs.

## 36.10 Per-core caches without Linux RSEQ

The POSIX/Linux profile uses RSEQ where available. seLe4n on Raspberry Pi 5 is not Linux, so the integration MUST define a different per-core fast-path contract.

Acceptable fast-path contracts are:

1. **Pinned-thread per-core mode:** a thread's CPU affinity is stable while it uses a per-core cache. The cache is flushed or handed off before affinity changes.
2. **Restartable-section mode:** seLe4n exposes a small user-level restartable critical-section contract analogous in purpose to RSEQ. The kernel aborts or restarts the sequence if the thread is preempted or migrated before the commit point.
3. **Preemption-disabled bounded mode:** a very small, bounded fast path executes with a kernel-approved preemption/migration exclusion mechanism. This mode MUST be budget-accounted and MUST NOT be available to arbitrary long critical sections.
4. **Thread-cache fallback mode:** if the platform cannot provide a safe per-core contract, TopoMalloc uses bounded thread-local caches and server batching.

Per-core cache invariants in seLe4n mode:

* A local cache is associated with a `CoreId`, size class, arena, and label.
* A thread may pop from a per-core cache only if its current execution context is authorized for that `CoreId` and arena.
* A cache MUST be flushed or made unreachable before core ownership changes.
* Cross-core transfer caches MUST respect arena labels and authority rights.
* The proof model MUST include an abort/no-change case for restartable or preemption-sensitive fast paths.

The implementation SHOULD prefer pinned-thread mode first because it is simpler to specify and test. Restartable-section mode can be added later if seLe4n exposes a suitable ABI.

## 36.11 Scheduling, budgets, and latency

TopoMalloc's slow paths can involve IPC to TopoResourceServer, CSpace operations, VSpace operations, retype, unmap, revoke, scrubbing, and cache draining. In a microkernel with explicit scheduling objects, those costs must be visible.

The seLe4n profile MUST define latency classes:

Latency classes:

* **Fast path:** local small `malloc/free`; no IPC, bounded instructions, and no unbounded scan.
* **Soft slow path:** batch refill/flush from existing mapped spans; bounded IPC and no retype or revocation.
* **Hard slow path:** new frame creation, mapping, CSlot allocation, or arena growth; may block and requires budget plus quota.
* **Maintenance:** purge, scrub, revoke, or large release; runs in a server maintenance context or explicit caller context.
* **Emergency:** fault-handler reserve allocation; uses pre-reserved memory only and must not introduce a dependency cycle.

Best practices:

* A client allocation SHOULD NOT perform unbounded revocation or scrubbing on the caller's critical path.
* TopoResourceServer SHOULD have a scheduling context sized for worst-case maintenance bursts or divide maintenance into resumable chunks.
* Arena policies MAY declare `no_ipc_fast_only`, `bounded_slow_path`, or `may_block` behavior.
* Real-time components SHOULD preallocate arenas or use fixed-size pools after initialization.
* Resource exhaustion MUST return explicit errors rather than silently borrowing authority from another domain.

## 36.12 Information-flow and security-domain policy

TopoMalloc's seLe4n profile MUST integrate heap allocation with seLe4n's information-flow policy.

Rules:

* Every arena MUST have a security label or domain set.
* Local caches, transfer caches, central lists, and backend pools MUST be partitioned by label unless a formally authorized declassification path exists.
* Dirty memory from a high domain MUST NOT be reused in a low domain until it has been scrubbed and the relevant capabilities/mappings have been revoked or relabeled according to policy.
* Heap statistics exposed to lower domains MUST be aggregated or redacted so that allocation patterns in higher domains are not leaked.
* Profiling stack traces MUST be label-scoped; cross-domain profile aggregation requires explicit authority.
* Shared-memory IPC buffers SHOULD use explicit shared arenas whose labels and rights are agreed at setup time.
* Device/DMA arenas MUST be isolated from normal heap arenas and must include cache-coherency policy.

Required non-interference theorem shape:

```lean
-- Schematic theorem shape, not final code.
theorem topo_step_preserves_low_equivalence
  (hWF : TopoSeLe4nWellFormed st)
  (hLow : LowEquivalent policy low st1 st2)
  (hAuth : AuthorizedTopoStep policy actor step)
  (hStep1 : TopoSeLe4nStep step st1 st1')
  (hStep2 : TopoSeLe4nStep step st2 st2') :
  LowEquivalent policy low st1' st2' := by
  ...
```

## 36.13 Arena lifecycle and revocation protocol

Arena destruction is much more serious in seLe4n mode than in POSIX mode because backing memory may be represented by revocable capabilities and mapped frames.

Required arena destruction protocol:

```text
destroy_arena_sele4n(arena):
    mark arena DRAINING
    reject new allocations and new delegations
    notify participating clients
    flush local per-core/thread caches
    drain transfer caches and central lists
    quarantine or reject stale frees
    unmap client VSpace windows
    scrub dirty pages if cross-label reuse is possible
    revoke derived frame and mapping capabilities
    delete obsolete CSpace entries
    return backing to resource-server free pools
    mark arena DESTROYED with generation increment
```

Required properties:

* Destroying an arena MUST make all old arena pointers invalid.
* A stale pointer MUST NOT become valid for a new arena merely because the virtual address is reused; generation checks SHOULD detect this in hardened/debug mode.
* The server MUST NOT recycle backing into a lower label until the scrubbing and revocation obligations for the old label are complete.
* A partial failure during destruction MUST leave the arena in `DRAINING` or `ERROR_QUARANTINED`, not in `DESTROYED`.
* Emergency allocations MUST NOT depend on an arena that is being destroyed.

## 36.14 API surface for seLe4n mode

The C/Rust ABI SHOULD be capability-explicit. The following names are illustrative.

```c
typedef struct topo_sele4n_runtime topo_sele4n_runtime_t;
typedef struct topo_arena_handle topo_arena_handle_t;
typedef uint64_t topo_rights_t;

topo_status_t topo_sele4n_bootstrap(
    const sele4n_boot_info_t *boot,
    topo_bootstrap_policy_t policy,
    topo_sele4n_runtime_t **out);

topo_status_t topo_sele4n_create_arena(
    topo_sele4n_runtime_t *rt,
    const topo_arena_policy_t *policy,
    topo_arena_handle_t **out);

topo_status_t topo_sele4n_delegate_arena(
    topo_arena_handle_t *parent,
    topo_rights_t rights,
    topo_quota_t quota,
    topo_label_t label,
    topo_arena_handle_t **child);

void *topo_mallocx_arena(
    topo_arena_handle_t *arena,
    size_t size,
    topo_alloc_flags_t flags);

topo_status_t topo_sele4n_destroy_arena(
    topo_arena_handle_t *arena,
    topo_destroy_flags_t flags);

topo_status_t topo_sele4n_snapshot(
    topo_sele4n_runtime_t *rt,
    topo_stats_authority_t stats_cap,
    topo_stats_json_t *out);
```

The API MUST distinguish at least these error classes:

* `TOPO_OK`,
* `TOPO_ERR_INVALID_CAP`,
* `TOPO_ERR_AUTHORITY_DENIED`,
* `TOPO_ERR_QUOTA_EXCEEDED`,
* `TOPO_ERR_CSPACE_EXHAUSTED`,
* `TOPO_ERR_VSPACE_EXHAUSTED`,
* `TOPO_ERR_RETYPE_FAILED`,
* `TOPO_ERR_MAP_FAILED`,
* `TOPO_ERR_REVOKE_FAILED`,
* `TOPO_ERR_LABEL_VIOLATION`,
* `TOPO_ERR_WOULD_BLOCK`,
* `TOPO_ERR_ARENA_DRAINING`,
* `TOPO_ERR_CORRUPTION_DETECTED`.

For C++ integration, `operator new` MAY map selected errors to `std::bad_alloc`, but explicit TopoMalloc APIs SHOULD preserve detailed error codes for systems services.

## 36.15 Compatibility with seL4-style allocman, VKA, and VSpace abstractions

TopoMalloc should not require seLe4n to abandon existing seL4 ecosystem concepts. Instead, it SHOULD provide adapters:

* a VKA-like object allocator adapter for kernel-object creation;
* a VSpace adapter for reserving, mapping, and unmapping heap windows;
* an allocman-like compatibility layer for C programs that expect a combined malloc/VSpace/CSpace allocator;
* a pure TopoMalloc arena API for new verified components;
* a Rust `GlobalAlloc` or allocator-api adapter for Rust seLe4n user-space services.

Compatibility wrappers MUST be thinner than the verified core. The normative model is capability-backed arenas plus explicit backing providers; compatibility APIs are conveniences layered above that model.

## 36.16 Testing and validation for seLe4n integration

The seLe4n profile MUST add tests beyond ordinary allocator tests.

Required tests:

* boot inventory test: every root-provided untyped capability is classified exactly once;
* largest-first retype test: backing splits avoid known alignment-waste patterns;
* CSlot exhaustion test: allocation fails cleanly when slots are exhausted;
* VSpace exhaustion test: allocation fails cleanly when no mapping window is available;
* quota test: delegated arenas cannot exceed parent quota;
* authority test: clients cannot allocate from arenas without rights;
* label test: high-domain dirty memory cannot be reused by low-domain arenas without scrub/revocation;
* revocation test: destroyed arenas leave no live frame capabilities, mappings, or local-cache objects;
* migration test: per-core caches flush or hand off correctly when CPU affinity changes;
* restartable-section abort test: aborted fast paths leave state unchanged;
* deterministic replay test: resource-server traces replay against the Lean model;
* crash/restart test: server failure does not let clients gain memory authority;
* stats redaction test: low-domain statistics cannot reveal high-domain allocation patterns;
* emergency-reserve test: fault-handler allocations do not depend on normal heap availability.

## 36.17 Formal theorem checklist for the bridge

The following theorem families SHOULD be part of the seLe4n bridge acceptance criteria.

Theorem families:

* `arena_cap_authorizes_alloc`: a successful allocation from an arena implies the caller had allocation authority.
* `arena_quota_preserved`: allocation, free, delegation, purge, and destroy preserve quota accounting.
* `backing_descends_from_untyped`: every heap backing frame descends from authorized untyped memory.
* `no_live_object_released`: release, revoke, and unmap never touch live objects.
* `destroy_revokes_descendants`: destroyed arenas retain no live client mappings or derived frame capabilities.
* `label_partition_preserved`: caches and free lists do not mix incompatible labels.
* `scrub_before_downgrade`: memory reused at a lower label has passed the required scrub/revocation protocol.
* `topo_step_preserves_sele4n_invariants`: each TopoMalloc-visible resource transition preserves the seLe4n invariant bundle.
* `client_cache_refines_server_authority`: client caches are a bounded refinement of server-granted backing authority, not independent authority.
* `per_core_cache_abort_no_change`: aborted per-core fast paths leave allocator-visible state unchanged.
* `stats_observation_noninterference`: authorized stats observations do not reveal forbidden higher-domain state.

The first integration milestone MAY prove these for a simplified single-core model. The SMP/per-core theorem set SHOULD be staged after seLe4n's multicore resource invariants stabilize.

## 36.18 Deployment profiles

The same integration should support multiple deployment styles.

### 36.18.1 Static-system profile

For Microkit-like or statically partitioned systems, TopoMalloc SHOULD support a fixed-arena mode:

* all backing memory is granted at boot;
* no runtime retype occurs after initialization;
* no arena growth occurs after initialization;
* local caches are bounded by static configuration;
* destroy/reset is optional or disabled;
* the Lean proof can treat resource authority as a fixed finite map.

This profile is best for high-assurance appliances, drivers, and real-time components.

### 36.18.2 Dynamic-service profile

For more dynamic systems, TopoResourceServer MAY retype and map backing memory on demand. This profile requires stronger proof and runtime machinery:

* quota enforcement;
* resumable maintenance operations;
* revocation protocol;
* CSpace/VSpace exhaustion handling;
* IPC-level authorization;
* label-aware stats and profiling;
* resource-server health monitoring.

This profile is best for dynamic user-space runtimes, language VMs, databases, caches, and components that need jemalloc/TCMalloc-like heap behavior.

### 36.18.3 Kernel-adjacent bootstrap profile

A tiny subset MAY be used in kernel-adjacent boot or generated runtime code, but only under these restrictions:

* monotonic bump allocation only;
* no general free;
* fixed maximum memory;
* no unbounded search;
* no blocking;
* no capability mutation hidden from the kernel model;
* separate proof from full TopoMalloc.

This profile is not general TopoMalloc. It exists only to avoid conflating bootstrapping with production heap allocation.

## 36.19 Integration roadmap

Integration phases:

* **S0 - Fit and boundary document:** accept this section and agree on the kernel/user boundary.
* **S1 - Pure Lean bridge model:** capability-backed arena model, backing-provider state machine, and single-core preservation theorems compile.
* **S2 - Resource-server prototype:** `TopoResourceServer` boots in simulation with static backing inventory and deterministic trace replay.
* **S3 - Client runtime prototype:** `malloc/free/calloc/realloc` work against pre-granted arenas without runtime retype.
* **S4 - Dynamic backing:** server can retype, map, unmap, and revoke normal RAM backing with quota and CSlot/VSpace accounting.
* **S5 - Security labels:** label-partitioned arenas, scrub-before-downgrade, and stats redaction are implemented and tested.
* **S6 - SMP/per-core caches:** pinned-thread per-core cache mode works with migration flush and model replay.
* **S7 - Rust and C++ integration:** Rust allocator adapter and C++ ABI wrappers pass compatibility tests.
* **S8 - Refinement hardening:** selected resource-server operations refine seLe4n model transitions.
* **S9 - Performance validation:** benchmarks compare static pools, allocman-like mode, and TopoMalloc arenas on target hardware or QEMU.

## 36.20 Risks and mitigations

Risk register:

* **Kernel proof-surface expansion:** TopoMalloc could undermine the microkernel minimality story. Mitigation: keep TopoMalloc outside the kernel core and prove only narrow provider contracts at the boundary.
* **Resource-server TCB expansion:** a privileged memory server becomes security-critical. Mitigation: keep the server small, auditable, and label-aware; split policy from mechanism where useful.
* **IPC overhead:** slow-path allocation may be more expensive than POSIX `mmap`. Mitigation: batch backing requests, pre-grant arenas, and use fixed-arena mode for latency-sensitive systems.
* **Capability leaks:** failed destroy/revoke could leave stale authority. Mitigation: generation counters, mandatory revocation tests, and a Lean `destroy_revokes_descendants` theorem.
* **CSpace exhaustion:** the allocator may run out of capability slots before memory. Mitigation: account CSlots as first-class quota and reserve slots before retyping.
* **VSpace fragmentation:** dynamic mapping windows can fragment client address spaces. Mitigation: reserve arena windows at creation and support fixed-layout profiles.
* **Mis-modeled per-core fast path:** per-core cache correctness depends on migration and preemption details. Mitigation: start with pinned-thread mode and add restartable mode only with a formal abort contract.
* **Large-mapping overfit:** hugepage policy may not map cleanly to Raspberry Pi 5 or seLe4n VSpace abstractions. Mitigation: treat large mappings as optional placement units and remain correct with normal pages.
* **License incompatibility:** seLe4n advertises GPLv3 while TopoMalloc may want a different standalone license. Mitigation: keep the standalone TopoMalloc core under its chosen license and provide a separately licensed seLe4n integration layer compatible with the target repo.

## 36.21 Conclusion

TopoMalloc should complement seLe4n as a verified, capability-aware, user-level memory allocator and resource service. The projects fit because both prefer explicit authority, machine-checked invariants, and small trusted boundaries. The integration should not be an in-kernel heap. It should be a first-class, required, proof-carrying service layer that gives seLe4n-based systems a modern dynamic allocator without weakening the microkernel's capability and verification discipline.

# 37. Implementation roadmap

## 37.1 Phase A: minimal correct allocator

Deliver:

* size-class table,
* simple spans and central free lists,
* standard malloc/free/calloc/realloc,
* pagemap,
* basic stats,
* Lean model for sequential allocation/free.

Exit criteria:

* passes ABI tests,
* passes property tests,
* Lean proves core ownership and disjointness.

## 37.2 Phase B: local caches

Deliver:

* fallback thread cache,
* per-CPU cache with locked fallback,
* batch refill/flush,
* cache budget controller v1.

Exit criteria:

* stable under many-thread tests,
* bounded cache footprint,
* model replay for cache transitions.

## 37.3 Phase C: RSEQ fast path

Deliver:

* Linux RSEQ registration,
* x86-64 and AArch64 sequences,
* abort handling,
* locked fallback,
* stress tests.

Exit criteria:

* no correctness difference versus locked mode,
* measurable hot-path improvement,
* RSEQ contract documented.

## 37.4 Phase D: arena policy domains

Deliver:

* arena create/reset/destroy,
* arena stats,
* extent hook API,
* per-arena decay,
* NUMA policy.

Exit criteria:

* explicit arenas usable by applications,
* reset/destroy safety tests pass,
* hooks validated by failure-injection tests.

## 37.5 Phase E: hugepage backend

Deliver:

* huge allocator,
* huge cache,
* hugepage filler,
* region cache,
* hugepage stats,
* release controller integration.

Exit criteria:

* high hugepage coverage on target workloads,
* reduced backend fragmentation,
* release policy avoids pathological refault loops.

## 37.6 Phase F: observability and profiling

Deliver:

* structured stats,
* heap profiles,
* lifetime profiles,
* fragmentation reports,
* memory explanation endpoint,
* operational tools.

Exit criteria:

* engineers can explain RSS from stats,
* profiles have low overhead,
* production-safe sampling.

## 37.7 Phase G: hardening and formal refinement

Deliver:

* hardened profile,
* debug profile,
* quarantine,
* sampled guard pages,
* generated verified tables,
* model trace replay,
* selected refinement proofs.

Exit criteria:

* catches common heap misuse in tests,
* Lean verifies all required abstract theorems,
* implementation/test traces accepted by model.

## 37.8 Phase H: seLe4n integration profile

Deliver:

* capability-backed arena model,
* TopoResourceServer prototype,
* seLe4n backing-provider contracts,
* fixed-arena and dynamic-service deployment profiles,
* label-aware caches and stats,
* C/Rust ABI adapters,
* deterministic trace replay against the Lean bridge.

Exit criteria:

* static fixed-arena profile works without runtime retype,
* dynamic-service profile handles CSpace/VSpace/quota exhaustion cleanly,
* arena destroy/revoke tests leave no live mappings or stale authority,
* selected Lean bridge theorems compile,
* performance is measured against fixed pools and allocman-like baselines.

# 38. Appendix A: Key algorithms

## A.1 Request classification

```text
classify(size, align, flags):
    if size_overflows or align_invalid:
        return Error
    arena = choose_arena(flags)
    if size <= small_max and align <= small_alignment_limit:
        sc = size_class(size, align)
        return SmallRequest(arena, sc, flags)
    if size < huge_threshold:
        return MediumRequest(arena, round_pages(size, align), flags)
    return LargeRequest(arena, round_huge_or_region(size, align), flags)
```

## A.2 Small allocation slow path

```text
small_alloc_slow(cpu, req):
    maybe_grow_cache_capacity(cpu, req.sc)
    loop:
        batch = transfer_try_pop(llc_domain(cpu), req.sc, req.arena)
        if batch.empty:
            batch = central_remove_batch(numa_node(cpu), req.arena, req.sc)
        if batch.empty:
            span = backend_new_span(req.arena, req.sc, placement_policy(req))
            if span == null:
                return null            // OOM: malloc sets errno=ENOMEM; operator new throws std::bad_alloc
            central_attach_span(span)
            continue                   // re-derive a batch; another CPU may have drained the new span
        cpu_cache_insert_batch(cpu, req.sc, batch.tail)
        return batch.head
```

## A.3 Small free slow path

```text
small_free_slow(cpu, ptr, meta):
    if debug_or_hardened:
        validate_free(ptr, meta)
    if quarantine_should_hold(ptr, meta):
        quarantine_insert(ptr, meta)
        return
    batch = cpu_cache_make_space(cpu, meta.sc)
    if batch.not_empty:
        flush_batch(batch, llc_domain(cpu), meta.arena, meta.sc)
    cpu_cache_push(cpu, meta.sc, ptr)
```

## A.4 Central remove batch

```text
central_remove_batch(node, arena, sc):
    lock central[node][arena][sc]
    if free_objects_available:
        batch = remove_objects(batch_size(sc))
        update_span_counts(batch)
        unlock
        return batch
    if empty_or_partial_span_available:
        activate_span
        batch = carve_or_remove(batch_size(sc))
        unlock
        return batch
    unlock
    return empty
```

## A.5 Release controller step

```text
release_controller_step():
    pressure = read_pressure()
    target = compute_release_target(pressure)
    shrink_idle_local_caches(target)
    release_empty_hugepages(target)
    purge_dirty_extents(target)
    if pressure.hard:
        subrelease_cold_sparse_hugepages(target)
    update_stats()
```

# 39. Appendix B: Required invariant checklist

## B.1 Global invariants

* Every object has exactly one owner.
* Every live object is disjoint from every other live object.
* Every free object is reachable from exactly one free structure.
* Every pointer in a cache belongs to a valid span or large descriptor.
* Every span belongs to exactly one arena.
* Every extent belongs to exactly one arena.
* Every page maps to at most one descriptor.
* Every allocator-owned page maps to at least one descriptor state.
* Released ranges contain no live objects.
* Metadata ranges do not overlap user-live ranges.

## B.2 Cache invariants

* Per-CPU cache counts do not exceed capacity.
* Per-CPU cache bytes do not exceed hard budget.
* Thread cache bytes do not exceed hard budget.
* Transfer cache batches contain distinct objects.
* Transfer cache batches have consistent size class and arena.
* Cache flush preserves object count.
* Cache refill preserves object count.

## B.3 Span invariants

* Span object ranges fit within span.
* Span object ranges are disjoint.
* Span size class matches all contained objects.
* Span free count equals authoritative free representation.
* Empty span detection accounts for local, transfer, central, and quarantine states.
* Span generation prevents stale descriptor reuse.

## B.4 Hugepage invariants

* Hugepage bins match occupancy.
* Hugepage live bytes equal sum of live contained ranges.
* Empty hugepage has no live spans.
* Partial subrelease contains no live object.
* Hugepage coverage metrics are computed from committed hugepage state.

## B.5 Arena invariants

* Arena state controls allowed operations.
* Reset/destroy requires cache drain or safe invalidation.
* Extent hooks are installed before hook-owned extents exist.
* Destroyed arena IDs are not reused while stale references can exist.
* Arena stats are merged or retained according to documented policy.

# 40. Appendix C: Suggested default constants

These defaults are initial engineering targets, not universal truths. They MUST be tuned by measurement.

| Parameter | Suggested initial value |
|---|---:|
| tiny_min | 8 bytes |
| small_max | 32 KiB or 64 KiB |
| allocator_page | 8 KiB or 16 KiB server default |
| hugepage_size | platform value, commonly 2 MiB on x86-64 Linux |
| per_cpu_soft_limit | 1 to 4 MiB |
| global_cache_fraction | 1% to 5% of live heap |
| dirty_decay_ms | 5,000 to 10,000 ms server default |
| muzzy_decay_ms | 10,000 to 30,000 ms server default |
| background_threads | min(active_arenas, CPUs / 4), capped |
| heap_sample_rate | workload-dependent, e.g. megabytes per sample |
| lifetime_sample_rate | lower than heap sample rate |
| quarantine_default | disabled in performance profile |

# 41. Appendix D: Example stats JSON

```json
{
  "topomalloc_version": "0.1",
  "epoch": 128931,
  "profile": "performance",
  "application": {
    "live_bytes": 3493224448,
    "allocated_bytes_total": 991234883584,
    "freed_bytes_total": 987741659136
  },
  "cache": {
    "per_cpu_bytes": 268435456,
    "thread_cache_bytes": 0,
    "transfer_bytes": 67108864,
    "global_budget_bytes": 536870912
  },
  "central": {
    "free_bytes": 184549376
  },
  "backend": {
    "dirty_bytes": 536870912,
    "muzzy_bytes": 268435456,
    "retained_bytes": 1073741824,
    "released_bytes": 2147483648
  },
  "hugepage": {
    "coverage_ratio": 0.82,
    "empty_backed_bytes": 134217728,
    "partial_subreleased_bytes": 67108864,
    "fragmentation_bytes": 201326592
  },
  "metadata": {
    "bytes": 33554432
  }
}
```

# 42. Appendix E: Example control namespace

```text
topo.version
topo.profile
topo.stats.refresh
topo.stats.json
topo.stats.print
topo.cache.global_budget
topo.cache.per_cpu_limit
topo.cache.flush_all
topo.cache.flush_cpu.<cpu>
topo.release.to_os
topo.release.rate
topo.arena.<id>.name
topo.arena.<id>.state
topo.arena.<id>.dirty_decay_ms
topo.arena.<id>.muzzy_decay_ms
topo.arena.<id>.purge
topo.arena.<id>.reset
topo.arena.<id>.destroy
topo.hugepage.enabled
topo.hugepage.coverage
topo.profile.heap.start
topo.profile.heap.dump
topo.profile.lifetime.start
topo.security.quarantine_bytes
topo.debug.check_now
```

# 43. Appendix F: Anti-patterns

TopoMalloc implementations MUST avoid these anti-patterns:

* hidden global locks on the hot path,
* unbounded per-thread caches in high-thread-count programs,
* releasing partial hugepages eagerly without memory pressure,
* hand-maintained size-class tables without generated checks,
* storing critical metadata only inside user-writable memory,
* calling malloc from allocator error logging,
* treating RSS as the only memory metric,
* allowing arena reset while local caches still contain arena objects,
* silently ignoring alignment requests,
* using unsynchronized pagemap updates,
* making profiling callbacks allocate through the same allocator recursively,
* reusing span descriptors without generation protection,
* mixing memory from different allocators without clear ownership,
* treating seLe4n capabilities as mere metadata rather than allocation authority,
* using ordinary heap memory for TopoMemory service request-path metadata,
* allowing cross-domain reuse without scrubbing and information-flow checks,
* treating seLe4n capability revocation as equivalent to POSIX page release,
* placing TopoMalloc in the microkernel core as an unbounded general heap.

# 44. Appendix G: Open design questions

The following questions require implementation experiments:

1. What default small-object cache maximum gives the best RSS/throughput tradeoff for target workloads?
2. Should medium allocations have a limited front-end cache or always use central/backend paths?
3. How aggressive should partial hugepage subrelease be under cgroup pressure?
4. Which sampled lifetime features are predictive enough to affect placement?
5. How should TopoMalloc integrate with hardware memory tagging on AArch64?
6. What is the best lock hierarchy for arena and backend operations without sacrificing parallelism?
7. Can selected RSEQ sequences be machine-verified against the Lean abstract contract?
8. Should deterministic debug mode route all allocations through central structures for easier validation?
9. How much metadata should be protected with mprotect in hardened mode?
10. How should TopoMalloc expose stable operational metrics without freezing internal implementation details?
11. What is the smallest seLe4n ABI needed for scheduler-assisted per-core allocator caches?
12. Should seLe4n deployments prefer many static arenas or a central TopoMemory service for typical embedded workloads?
13. Which seLe4n service-orchestration proofs should own arena dependency and destruction order?
14. What large-mapping abstraction best fits Raspberry Pi 5 and future seLe4n targets?
15. Can fixed-arena mode satisfy most high-assurance seLe4n applications without dynamic retype after initialization?

# 45. References

[R1] Google TCMalloc design documentation, "TCMalloc : Thread-Caching Malloc." Describes the front-end, middle-end, back-end architecture, per-CPU mode, size classes, transfer cache, central free list, and pageheap. Source: https://google.github.io/tcmalloc/design.html

[R2] Google TCMalloc tuning documentation, "Performance Tuning TCMalloc." Discusses per-CPU cache sizing, heterogeneous per-CPU cache optimization, memory release, page faults, and hugepage tradeoffs. Source: https://google.github.io/tcmalloc/tuning.html

[R3] Google TCMalloc Temeraire documentation, "Temeraire: Hugepage-Aware Allocator." Describes goals and components of a hugepage-aware allocator including huge cache and hugepage filler. Source: https://github.com/google/tcmalloc/blob/master/docs/temeraire.md

[R4] jemalloc manual page. Describes arenas, tcache controls, arena reset/destroy, dirty/muzzy decay, purging, and extent hooks. Source: https://jemalloc.net/jemalloc.3.html

[R5] Linux kernel documentation, "Restartable Sequences." Describes userspace per-thread RSEQ areas and per-CPU updates without heavyweight atomics. Source: https://docs.kernel.org/userspace-api/rseq.html

[R6] Google TCMalloc basic reference. Describes hot/cold allocation hints and sized delete as a performance optimization. Source: https://google.github.io/tcmalloc/reference.html

[R7] Google TCMalloc stats documentation, "Understanding Malloc Stats." Describes human-readable stats and internal memory breakdowns. Source: https://google.github.io/tcmalloc/stats.html

[R8] Leonardo de Moura and Sebastian Ullrich, "The Lean 4 Theorem Prover and Programming Language." Describes Lean 4 as an extensible theorem prover and efficient programming language. Source: https://lean-lang.org/papers/lean4.pdf

[R9] Antonin Reitz, Aymeric Fromherz, Jonathan Protzenko, "StarMalloc: A Formally Verified, Concurrent, Performant, and Security-Oriented Memory Allocator." Demonstrates a formally verified concurrent allocator in F*/Steel. Source: https://arxiv.org/abs/2403.09435

[R10] hatter6822/seLe4n README. Describes seLe4n as a Lean 4 capability-based microkernel with machine-checked proofs, seL4 inspiration, current project state, Raspberry Pi 5 target, Rust HAL crates, SMP workstream, architecture layers, and GPLv3+ licensing. Source: https://github.com/hatter6822/seLe4n/tree/main

[R11] seLe4n Project Specification. Describes project identity, current state, target hardware, acceptance expectations, and non-negotiable baseline contracts. Source: https://raw.githubusercontent.com/hatter6822/seLe4n/main/docs/spec/SELE4N_SPEC.md

[R12] seL4 docs, "Untyped" tutorial. Describes seL4 physical memory management, untyped capabilities, retyping, device untyped restrictions, watermark behavior, alignment waste, and the largest-first allocation recommendation. Source: https://docs.sel4.systems/Tutorials/untyped.html

[R13] seL4 documentation, User-level C libraries. Describes libsel4allocman, libsel4vka, libsel4vspace, and notes that the user-level C libraries are for prototyping and are not verified. Source: https://docs.sel4.systems/projects/user_libs/

[R14] seL4 documentation, Microkit overview. Describes Microkit as a framework on top of seL4 for statically structured systems. Source: https://docs.sel4.systems/projects/microkit/

[R15] Gernot Heiser, "The seL4 Microkernel - An Introduction," seL4 Foundation whitepaper revision 1.4. Describes seL4 as a microkernel/hypervisor, its capability model, formal verification story, and safety/security goals. Source: https://sel4.systems/About/seL4-whitepaper.pdf

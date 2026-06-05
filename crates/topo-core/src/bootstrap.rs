// SPDX-License-Identifier: MIT
//! The bootstrap metadata allocator (§17.4, S-007, plan 03 W3-1).
//!
//! The allocator needs metadata — span descriptors, pagemap radix nodes, arena
//! records — *before* it can allocate normally, and the path that produces them
//! **must never re-enter the public allocator** while the allocator is still
//! being built (the metadata analogue of the §27.5 TLS bootstrap rule, S-007).
//!
//! The design (DD-2) is a **monotonic bump arena** over a small static (or
//! early-reserved) region:
//!
//! * no dependency on public `malloc` — it only bumps a cursor in a region it was
//!   handed at init;
//! * lock-free before threading — concurrent allocations race only on an atomic
//!   cursor via a compare-exchange loop (no locks, §17.4);
//! * bounded and simple state — a base, a capacity, and a cursor;
//! * idempotent initialization — [`Bootstrap::init`] run twice is a no-op;
//! * safe failure — exhaustion returns `None`, never wraps or panics (§9.7);
//! * hand-off — once the normal metadata allocator exists, [`Bootstrap::hand_off`]
//!   records the transition (§17.4); the bytes already vended stay valid forever
//!   (the arena is monotonic — it never frees or reuses).
//!
//! Two layers:
//!
//! * [`BumpArena`] — the region-agnostic, lock-free bump core (W3-1a). It is unit
//!   tested over a caller-provided buffer, so the carving logic is exercised
//!   without any global state.
//! * [`Bootstrap`] — the process-wide lifecycle wrapper (W3-1b): an idempotent
//!   init state machine guarding one `BumpArena`, plus the hand-off flag.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::overflow::align_up;

/// A source of allocator **metadata** memory (not user memory). Implemented by the
/// bootstrap arena now and by the normal metadata allocator after hand-off
/// (§17.4); the pagemap (W3-3) allocates its radix nodes through this seam so the
/// node source can change at hand-off without the pagemap caring.
///
/// Implementations MUST return memory that stays valid for the lifetime of the
/// allocator (metadata is never freed back across this seam — recycled in place
/// with a generation bump instead, §27.5) and MUST be safe to call concurrently.
pub trait MetadataAlloc {
    /// Allocate `size` bytes aligned to `align` (a power of two) for metadata, or
    /// `None` on exhaustion / overflow. Never wraps, never panics (§9.7).
    fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>>;
}

/// A lock-free monotonic bump allocator over one fixed region (W3-1a).
///
/// Carves disjoint, non-overlapping sub-ranges out of `[base, base + capacity)` by
/// advancing an atomic cursor. It never frees and never reuses a byte, so every
/// vended pointer is valid until the region itself goes away — exactly what
/// metadata needs (a descriptor the pagemap points at must outlive every possible
/// classifier, §27.5).
pub struct BumpArena {
    /// Start of the region. The arena does not own the backing store; the caller
    /// guarantees it stays valid (and otherwise unused) for the arena's lifetime.
    base: *mut u8,
    /// Length of the region in bytes.
    capacity: usize,
    /// Next free offset within the region (the only shared mutable state).
    cursor: AtomicUsize,
}

impl BumpArena {
    /// Create a bump arena over `[base, base + capacity)`.
    ///
    /// # Safety
    /// `base` must point to the start of a single writable allocation of at least
    /// `capacity` bytes that stays valid and is not accessed through any other
    /// path for as long as this arena — and every pointer it vends — is in use.
    #[inline]
    pub const unsafe fn new(base: *mut u8, capacity: usize) -> Self {
        Self {
            base,
            capacity,
            cursor: AtomicUsize::new(0),
        }
    }

    /// Bytes vended so far (monotonic).
    #[inline]
    pub fn used(&self) -> usize {
        self.cursor.load(Ordering::Relaxed)
    }

    /// Total capacity of the region in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether `addr` lies inside this arena's region — used by pointer
    /// classification to report metadata pointers (§17.5, W3-4).
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        let base = self.base as usize;
        // `base + capacity` cannot overflow: it is the end of a real allocation.
        addr >= base && addr - base < self.capacity
    }

    /// Allocate `size` bytes aligned to `align`, or `None` on exhaustion/overflow.
    ///
    /// Lock-free: a compare-exchange loop reserves `[aligned, aligned + size)` by
    /// publishing the new cursor; the winner owns that disjoint range. Every step
    /// is checked, so an exhausting or overflowing request fails safely and never
    /// hands back an under-sized or wrapped region (§9.7). `align` must be a power
    /// of two; a zero `size` is rounded up to one byte so each call yields a
    /// distinct pointer.
    pub fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        let size = size.max(1);
        let base_addr = self.base as usize;

        loop {
            let cur = self.cursor.load(Ordering::Relaxed);
            // Align the *absolute* address so the returned pointer — not merely
            // the offset — satisfies `align`, with every step overflow-checked.
            let start_abs = base_addr.checked_add(cur)?;
            let aligned_abs = align_up(start_abs, align)?;
            let pad = aligned_abs - start_abs; // aligned_abs >= start_abs, no underflow
            let end = cur.checked_add(pad)?.checked_add(size)?;
            if end > self.capacity {
                return None; // genuine exhaustion — no wrap, no under-allocation
            }
            if self
                .cursor
                .compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let offset = cur + pad;
                // SAFETY: `offset + size <= capacity` (checked above), so the
                // result lies within the single region `base` points to. The CAS
                // advanced the cursor past `end`, so no other allocation can hand
                // out an overlapping range — the winner exclusively owns
                // `[offset, offset + size)`. Provenance flows from `base`'s
                // allocation, of which this is a sub-range.
                let ptr = unsafe { self.base.add(offset) };
                // SAFETY: `ptr` is non-null — `base` is non-null (it is the start
                // of a real allocation) and `add` of an in-bounds offset preserves
                // that.
                return Some(unsafe { NonNull::new_unchecked(ptr) });
            }
            // Lost the race; retry with the updated cursor.
        }
    }
}

impl MetadataAlloc for BumpArena {
    #[inline]
    fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        BumpArena::alloc(self, size, align)
    }
}

// SAFETY: the only shared mutable state is the atomic `cursor`; `alloc` hands out
// disjoint, non-overlapping ranges (the CAS reserves `[cur, end)` for the winner),
// so two threads never receive aliasing pointers. The `*mut u8` base is never
// dereferenced through the arena itself — callers own the bytes they are vended.
unsafe impl Send for BumpArena {}
// SAFETY: see the `Send` impl — concurrent `&BumpArena` users contend only on the
// atomic cursor and receive disjoint ranges.
unsafe impl Sync for BumpArena {}

/// Lifecycle state of the process-wide [`Bootstrap`] allocator. A single
/// `AtomicU8` so init is lock-free (§17.4 "no locks before threading").
#[repr(u8)]
enum State {
    /// Not yet initialized; no region bound.
    Uninit = 0,
    /// One thread is binding the region (transient, holds the CAS winner).
    Initializing = 1,
    /// Region bound; serving metadata allocations.
    Active = 2,
    /// Normal metadata allocator is available; bootstrap has handed off. Already
    /// vended bytes remain valid (monotonic); new metadata SHOULD come from the
    /// normal path. The arena still answers `alloc` (safe, monotonic) so a late
    /// bootstrap request during the transition window cannot fault.
    HandedOff = 3,
}

/// The process-wide bootstrap metadata allocator (W3-1b): an idempotent init
/// state machine guarding one [`BumpArena`], plus the hand-off flag (§17.4).
///
/// Construct it `const` (so it can be a `static`), then [`init`](Self::init) it
/// once with a region. `init` is idempotent — a second call (or a lost init race)
/// is a no-op — and exhaustion is reported as `None`, never a panic (safe
/// failure). It never calls back into the public allocator (S-007): an
/// [`alloc`](Self::alloc) only touches the bound `BumpArena`.
pub struct Bootstrap {
    state: AtomicU8,
    /// The guarded arena. Written exactly once, by the `Uninit -> Initializing`
    /// CAS winner, *before* it publishes `Active` with a release store; readers
    /// that observe `Active`/`HandedOff` with an acquire load therefore see a
    /// fully-initialized arena (the publish/consume ordering of §27.3).
    arena: UnsafeCell<MaybeUninit<BumpArena>>,
}

impl Bootstrap {
    /// A fresh, uninitialized bootstrap allocator (usable as a `static`).
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(State::Uninit as u8),
            arena: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Bind the bootstrap region to `[base, base + capacity)`. Idempotent: the
    /// first caller wins and binds the arena; any later call — or a caller that
    /// loses the init race — is a no-op and returns `false` (double init is a
    /// no-op, W3-1b). Returns `true` iff this call performed the binding.
    ///
    /// # Safety
    /// Same contract as [`BumpArena::new`]: `base` must head a single writable
    /// allocation of `capacity` bytes that outlives every pointer the bootstrap
    /// allocator vends and is not aliased elsewhere.
    ///
    /// SPEC-transition: bootstrap metadata `Uninit -> Active` (§17.4)
    pub unsafe fn init(&self, base: *mut u8, capacity: usize) -> bool {
        // Claim the right to initialize. Only the `Uninit -> Initializing` winner
        // proceeds; everyone else observes a non-`Uninit` state and bails (the
        // idempotency guarantee). `Acquire` on success pairs with the `Release`
        // publish below in case of a (degenerate) re-entrant observer.
        if self
            .state
            .compare_exchange(
                State::Uninit as u8,
                State::Initializing as u8,
                Ordering::Acquire,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false; // already initialized or being initialized — no-op
        }
        // SAFETY: we won the `Uninit -> Initializing` CAS, so we hold exclusive
        // write access to `arena` (no other thread can be here, and no reader will
        // touch it until we publish `Active`). We write it exactly once.
        unsafe {
            // SAFETY: `init`'s contract guarantees `base`/`capacity` describe a
            // valid, exclusively-owned region for the arena's lifetime.
            (*self.arena.get()).write(BumpArena::new(base, capacity));
        }
        // Publish: a reader observing `Active` with an acquire load is guaranteed
        // to see the fully-written arena (release/acquire, §27.3).
        self.state.store(State::Active as u8, Ordering::Release);
        true
    }

    /// Whether the allocator has been initialized (is `Active` or `HandedOff`).
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.state.load(Ordering::Acquire) >= State::Active as u8
    }

    /// Whether the allocator has handed off to the normal metadata path (§17.4).
    #[inline]
    pub fn is_handed_off(&self) -> bool {
        self.state.load(Ordering::Acquire) == State::HandedOff as u8
    }

    /// Record the hand-off to the normal metadata allocator (§17.4). Idempotent:
    /// only an `Active -> HandedOff` transition does anything; calling it before
    /// init or twice is a no-op. Returns `true` iff this call performed the
    /// transition. The arena keeps serving (monotonic, safe) so a request racing
    /// the hand-off cannot fault.
    ///
    /// SPEC-transition: bootstrap metadata `Active -> HandedOff` (§17.4)
    pub fn hand_off(&self) -> bool {
        self.state
            .compare_exchange(
                State::Active as u8,
                State::HandedOff as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The bound arena, if initialized. Acquire-loads the state so the arena read
    /// is ordered after the init that published it.
    #[inline]
    fn arena(&self) -> Option<&BumpArena> {
        if self.is_initialized() {
            // SAFETY: `is_initialized()` observed `Active`/`HandedOff` with an
            // acquire load, which synchronizes-with the release store in `init`
            // that followed the one-time write of `arena`. The arena is written
            // exactly once and never moved, so this shared reference is valid.
            Some(unsafe { (*self.arena.get()).assume_init_ref() })
        } else {
            None
        }
    }

    /// Allocate `size` bytes aligned to `align` for metadata, or `None` if the
    /// allocator is uninitialized or exhausted (safe failure, never a panic). This
    /// path provably never re-enters the public allocator (S-007): it only bumps
    /// the bound arena's cursor.
    #[inline]
    pub fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        self.arena()?.alloc(size, align)
    }

    /// Whether `addr` lies in the bootstrap region (a metadata pointer, §17.5).
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        self.arena().is_some_and(|a| a.contains(addr))
    }

    /// Bytes of metadata vended so far (0 before init).
    #[inline]
    pub fn used(&self) -> usize {
        self.arena().map_or(0, BumpArena::used)
    }
}

impl Default for Bootstrap {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataAlloc for Bootstrap {
    #[inline]
    fn alloc(&self, size: usize, align: usize) -> Option<NonNull<u8>> {
        Bootstrap::alloc(self, size, align)
    }
}

// SAFETY: `state` is atomic and gates all access to `arena`; `arena` is written
// exactly once (by the init-CAS winner, before the release publish) and only ever
// read by reference thereafter, so there is no data race on the `UnsafeCell`. The
// guarded `BumpArena` is itself `Sync`.
unsafe impl Sync for Bootstrap {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap-backed region for tests: leaks a `Box<[u8]>` so the pointer stays
    /// valid for the process (matching the `'static` lifetime real bootstrap
    /// regions have), and hands back its base/len.
    fn region(bytes: usize) -> (*mut u8, usize) {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        (ptr, len)
    }

    #[test]
    fn bump_vends_aligned_disjoint_ranges() {
        let (base, len) = region(4096);
        // SAFETY: `region` leaks a live 4096-byte buffer.
        let arena = unsafe { BumpArena::new(base, len) };

        let a = arena.alloc(64, 16).unwrap().as_ptr() as usize;
        let b = arena.alloc(1, 64).unwrap().as_ptr() as usize;
        let c = arena.alloc(128, 8).unwrap().as_ptr() as usize;

        assert_eq!(a % 16, 0);
        assert_eq!(b % 64, 0);
        assert_eq!(c % 8, 0);
        // Distinct, non-overlapping, monotonically increasing.
        assert!(b >= a + 64);
        assert!(c > b);
        assert!(arena.used() <= len);
    }

    #[test]
    fn bump_exhaustion_fails_safely() {
        let (base, len) = region(128);
        // SAFETY: `region` leaks a live 128-byte buffer.
        let arena = unsafe { BumpArena::new(base, len) };
        assert!(arena.alloc(64, 8).is_some());
        // A request that cannot fit returns None (never wraps / under-allocates).
        assert!(arena.alloc(4096, 8).is_none());
        // The failed request did not advance the cursor past capacity.
        assert!(arena.used() <= len);
    }

    #[test]
    fn bump_zero_size_yields_distinct_pointers() {
        let (base, len) = region(256);
        // SAFETY: `region` leaks a live 256-byte buffer.
        let arena = unsafe { BumpArena::new(base, len) };
        let p = arena.alloc(0, 8).unwrap().as_ptr();
        let q = arena.alloc(0, 8).unwrap().as_ptr();
        assert_ne!(p, q, "zero-size allocations must be distinct (zero_unique)");
    }

    #[test]
    fn bump_contains_only_its_region() {
        let (base, len) = region(256);
        // SAFETY: `region` leaks a live 256-byte buffer.
        let arena = unsafe { BumpArena::new(base, len) };
        assert!(arena.contains(base as usize));
        assert!(arena.contains(base as usize + len - 1));
        assert!(!arena.contains(base as usize + len));
        assert!(!arena.contains(base as usize - 1));
    }

    #[test]
    fn bootstrap_init_is_idempotent() {
        let (base, len) = region(1024);
        let boot = Bootstrap::new();
        assert!(!boot.is_initialized());
        // SAFETY: `region` leaks a live 1024-byte buffer.
        assert!(unsafe { boot.init(base, len) }, "first init binds");
        assert!(boot.is_initialized());
        // A second init is a no-op and reports it did nothing (double init no-op).
        // SAFETY: a redundant init with the same region is harmless; it is a no-op.
        assert!(!unsafe { boot.init(base, len) }, "double init is a no-op");
    }

    #[test]
    fn bootstrap_allocates_before_any_arena_and_never_calls_malloc() {
        // DD-2 verify: a pagemap node and a span descriptor's worth of metadata
        // can be obtained *before any arena exists* — purely from the bump region,
        // with no global allocator involvement (the arena only bumps a cursor).
        let (base, len) = region(64 * 1024);
        let boot = Bootstrap::new();
        // SAFETY: `region` leaks a live 64 KiB buffer.
        unsafe { boot.init(base, len) };

        let node = boot.alloc(16 * 1024, 64).expect("radix node");
        let desc = boot.alloc(192, 16).expect("span descriptor");
        assert_ne!(node.as_ptr(), desc.as_ptr());
        assert_eq!(node.as_ptr() as usize % 64, 0);
        assert_eq!(desc.as_ptr() as usize % 16, 0);
    }

    #[test]
    fn bootstrap_uninitialized_alloc_fails_safely() {
        let boot = Bootstrap::new();
        // Before init there is no region; allocation fails safely (no panic, no
        // re-entry into any global allocator).
        assert!(boot.alloc(16, 8).is_none());
        assert_eq!(boot.used(), 0);
        assert!(!boot.contains(0x1000));
    }

    #[test]
    fn bump_is_lock_free_concurrent_safe() {
        // W3-1a "lock-free before threading": many threads bump the same arena and
        // never receive overlapping ranges. Same-size 32-byte allocations at
        // distinct bases are disjoint, so distinct bases ⇒ no aliasing.
        let (base, len) = region(2 * 1024 * 1024);
        // SAFETY: `region` leaks a live 2 MiB buffer for the process.
        let arena = unsafe { BumpArena::new(base, len) };
        let lo = base as usize;
        let hi = lo + len;

        let all: Vec<usize> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let mut ptrs = Vec::new();
                        for _ in 0..2000 {
                            if let Some(p) = arena.alloc(32, 16) {
                                ptrs.push(p.as_ptr() as usize);
                            }
                        }
                        ptrs
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });

        // Every pointer is 16-aligned and inside the region.
        for &p in &all {
            assert_eq!(p % 16, 0);
            assert!(p >= lo && p < hi);
        }
        // Every base is distinct — no two concurrent allocations aliased.
        let distinct: std::collections::BTreeSet<usize> = all.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            all.len(),
            "concurrent allocations overlapped"
        );
    }

    #[test]
    fn bootstrap_hand_off_is_idempotent_and_keeps_serving() {
        let (base, len) = region(1024);
        let boot = Bootstrap::new();
        // hand_off before init does nothing.
        assert!(!boot.hand_off());
        // SAFETY: `region` leaks a live 1024-byte buffer.
        unsafe { boot.init(base, len) };
        assert!(boot.hand_off(), "Active -> HandedOff happens once");
        assert!(boot.is_handed_off());
        assert!(!boot.hand_off(), "second hand_off is a no-op");
        // Still serves after hand-off (monotonic, safe during the window).
        assert!(boot.alloc(16, 8).is_some());
    }
}

// SPDX-License-Identifier: MIT
//! `loom` model-checks of the subtle concurrency protocols W3/W4 rely on — the
//! **seqlock** (W3-4, `SpanDescriptor::read_consistent_geometry` /
//! `LargeDescriptor::read_consistent`), the **publish/read** of the pagemap radix
//! (W3-3c, `child_or_create` / leaf-entry store vs. `lookup`), and the
//! **large-free critical section** (W4, [`crate::large::LargeAllocator::free_with`]:
//! the lookup-under-the-pool-lock discipline that makes a concurrent double-free of
//! the same pointer safe). `loom` exhaustively explores every thread interleaving
//! and memory-ordering outcome permitted by the C11 model, so these are a
//! machine-checked complement to the std-thread stress tests (which exercise *some*
//! interleavings on one machine).
//!
//! These model the protocols with the **identical orderings** the real code uses
//! (AcqRel on the seqlock version, Release on the data/publish stores, Acquire on
//! every reader load); they are not the literal code (loom needs loom's atomics),
//! so they pin the ordering *logic* — the part most likely to be wrong.
//!
//! Gated to `--cfg loom`; invisible to the normal build. Run with:
//! `RUSTFLAGS="--cfg loom" cargo test -p topo-core --test loom_protocols`.
#![cfg(loom)]

use loom::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// The seqlock invariant (W3-4): a reader that observes a *consistent* version
/// (an even `s0` unchanged across the read) never sees a snapshot composed of two
/// incarnations. We model two geometry fields (`base`, `object_count`) that a
/// recycle moves together, and a reader mirroring `read_consistent_geometry`'s
/// version-bracketed read.
#[test]
fn seqlock_consistent_read_is_never_torn() {
    loom::model(|| {
        let base = Arc::new(AtomicUsize::new(0));
        let object_count = Arc::new(AtomicUsize::new(0));
        let seq = Arc::new(AtomicU32::new(0));

        // Writer = one recycle: bracket the geometry writes with odd/even (AcqRel),
        // store the fields with Release — exactly `SpanDescriptor::recycle`.
        let writer = {
            let (base, object_count, seq) = (base.clone(), object_count.clone(), seq.clone());
            thread::spawn(move || {
                seq.fetch_add(1, Ordering::AcqRel); // odd: update in progress
                base.store(0x4000, Ordering::Release);
                object_count.store(64, Ordering::Release);
                seq.fetch_add(1, Ordering::AcqRel); // even: published
            })
        };

        // Reader = one classification attempt, mirroring the real consistent read.
        let s0 = seq.load(Ordering::Acquire);
        let b = base.load(Ordering::Acquire);
        let oc = object_count.load(Ordering::Acquire);
        let s1 = seq.load(Ordering::Acquire);
        if s0 & 1 == 0 && s1 == s0 {
            // A consistent read: the pair must belong to ONE incarnation — either the
            // initial (0, 0) or the recycled (0x4000, 64), never a torn mix.
            assert!(
                (b, oc) == (0, 0) || (b, oc) == (0x4000, 64),
                "seqlock torn read: (base={b:#x}, object_count={oc})"
            );
        }

        writer.join().unwrap();
    });
}

/// The integrity-vs-race disambiguation (W3-4, hardened path): after a consistent
/// read, `read_consistent_geometry` validates the §17.3 integrity tag; on a
/// mismatch it **re-reads the version** and treats the mismatch as genuine
/// corruption *only if the version is unchanged*, otherwise a recycle merely raced
/// the check and it retries. Soundness rests on one ordering fact: if the validating
/// read observes any field (or the tag) from a recycle's bracketed write, a later
/// Acquire-load of the version must observe `seq != s0`. So a benign recycle is
/// never misreported as corruption. We model the tag as a value that equals the
/// field in any consistent incarnation (here `base == ck`); a recycle moves both
/// (Release) within an odd/even bracket. A reader that sees them *disagree* (a torn,
/// mid-recycle validate) must then observe an advanced version.
#[test]
fn integrity_mismatch_under_recycle_is_seen_as_a_version_change() {
    loom::model(|| {
        let base = Arc::new(AtomicUsize::new(0));
        let ck = Arc::new(AtomicUsize::new(0)); // the §17.3 tag; consistent ⟺ ck == base
        let seq = Arc::new(AtomicU32::new(0));

        // Writer = one recycle, exactly `SpanDescriptor::recycle`: open the seqlock
        // (odd), store the field then the recomputed tag (Release), close it (even).
        let writer = {
            let (base, ck, seq) = (base.clone(), ck.clone(), seq.clone());
            thread::spawn(move || {
                seq.fetch_add(1, Ordering::AcqRel); // odd: update in progress
                base.store(0x4000, Ordering::Release);
                ck.store(0x4000, Ordering::Release); // refresh_integrity
                seq.fetch_add(1, Ordering::AcqRel); // even: published
            })
        };

        // Reader = the hardened validate + re-check. It reads the version, then the
        // fields the way `validate_integrity` does (Acquire), and if the tag and field
        // disagree (a mismatch a racing recycle could cause) it must NOT conclude
        // "corruption": re-reading the version has to show it advanced past `s0`.
        let s0 = seq.load(Ordering::Acquire);
        if s0 & 1 == 0 {
            let b = base.load(Ordering::Acquire);
            let c = ck.load(Ordering::Acquire);
            if b != c {
                assert_ne!(
                    seq.load(Ordering::Acquire),
                    s0,
                    "a racing recycle's torn validate was misread as corruption \
                     (base={b:#x}, ck={c:#x}, s0={s0})"
                );
            }
        }

        writer.join().unwrap();
    });
}

/// The publish/read invariant (W3-3c): a reader that observes a published radix
/// node (a non-null child pointer / a non-`Empty` leaf entry) never reads the
/// node's contents before they were initialized. We model a node's contents
/// (`data`) and its publication slot, with the real ordering: initialize with a
/// Release store, publish with a Release store, consume with an Acquire load.
#[test]
fn publish_after_init_is_never_seen_uninitialized() {
    loom::model(|| {
        let data = Arc::new(AtomicUsize::new(0)); // the node's contents (e.g. an entry)
        let slot = Arc::new(AtomicUsize::new(0)); // 0 = unpublished, 1 = published

        // Producer = `child_or_create` + the leaf-entry store: write the contents,
        // *then* publish the node (release/release).
        let producer = {
            let (data, slot) = (data.clone(), slot.clone());
            thread::spawn(move || {
                data.store(42, Ordering::Release); // initialize the node
                slot.store(1, Ordering::Release); // publish it (the linking CAS/store)
            })
        };

        // Consumer = `lookup`: acquire-load the publication, and only then read the
        // contents. If it sees the node published, the contents must be initialized.
        if slot.load(Ordering::Acquire) == 1 {
            assert_eq!(
                data.load(Ordering::Acquire),
                42,
                "published node read before initialization (failure mode F1)"
            );
        }

        producer.join().unwrap();
    });
}

/// The lock-free lazy node creation (W3-3c `child_or_create`): two threads race to
/// install a node into the same null slot via a release CAS; exactly one wins and
/// **both** end up using the same published node, never a null. Models the CAS
/// race with two producers and a reader.
#[test]
fn lazy_node_creation_cas_race_yields_one_node() {
    loom::model(|| {
        let slot = Arc::new(AtomicUsize::new(0)); // 0 = empty; a non-zero value = a node id

        let make = |slot: Arc<AtomicUsize>, id: usize| {
            thread::spawn(move || {
                // `child_or_create`: if empty, try to publish our node with a release
                // CAS; on loss, adopt the winner's. Either way return a non-null node.
                if slot.load(Ordering::Acquire) == 0 {
                    match slot.compare_exchange(0, id, Ordering::Release, Ordering::Acquire) {
                        Ok(_) => id,
                        Err(winner) => winner,
                    }
                } else {
                    slot.load(Ordering::Acquire)
                }
            })
        };
        let t1 = make(slot.clone(), 1);
        let t2 = make(slot.clone(), 2);
        let n1 = t1.join().unwrap();
        let n2 = t2.join().unwrap();

        // Both threads obtained a real (non-null) node, and it is the same one — the
        // single published winner.
        assert!(n1 != 0 && n2 != 0, "a racer observed a null node");
        assert_eq!(n1, n2, "racers disagree on the published node");
        let final_slot = slot.load(Ordering::Acquire);
        assert!(final_slot == 1 || final_slot == 2);
        assert_eq!(final_slot, n1, "published node differs from the slot");
    });
}

/// The large-free critical-section discipline (W4, `LargeAllocator::free_with`): two
/// threads racing to free the SAME pointer must result in EXACTLY ONE slot release
/// (and one winning `free`). Soundness rests on resolving the descriptor (the pagemap
/// `lookup`) *under* the pool lock: the first freer retires the pagemap entry before
/// releasing the lock, so the second observes it gone and rejects. Were the lookup
/// done outside the lock, both freers would resolve the same slot and both
/// `pool.release(idx)` — a double release (free-stack self-cycle → later double-vend).
///
/// We model the whole pool critical section as plain state behind one lock (which is
/// how the real code holds it — `self.lock` over the pagemap entry, the slot's
/// presence, and the free stack): `present` is the pagemap entry, `releases` counts
/// `pool.release`. Reading `present` only *after* `lock()` encodes the fix; `loom`
/// explores both lock-acquisition orders and asserts the slot is released exactly
/// once in every one. (Moving the `present` read before `lock()` — the bug — makes
/// `loom` find the interleaving where both read it live and `releases` reaches 2.)
#[test]
fn large_double_free_under_pool_lock_releases_the_slot_once() {
    loom::model(|| {
        struct Pool {
            present: bool, // the pagemap entry for the freed pointer (live ⟺ true)
            releases: u32, // count of `pool.release(idx)` — must end at exactly 1
            wins: u32,     // count of `free` calls that returned `true`
        }
        let pool = Arc::new(Mutex::new(Pool {
            present: true,
            releases: 0,
            wins: 0,
        }));

        let freer = |pool: Arc<Mutex<Pool>>| {
            thread::spawn(move || {
                // `free_with`: acquire the pool lock, THEN resolve + retire + release.
                let mut p = pool.lock().unwrap();
                if p.present {
                    p.present = false; // retire_large (under the lock)
                    p.releases += 1; // pool.release(idx)
                    p.wins += 1; // returns `true`
                }
                // a second freer that finds `!present` falls through and returns `false`
            })
        };
        let t1 = freer(pool.clone());
        let t2 = freer(pool.clone());
        t1.join().unwrap();
        t2.join().unwrap();

        let p = pool.lock().unwrap();
        assert_eq!(p.releases, 1, "slot released exactly once (no double free)");
        assert_eq!(p.wins, 1, "exactly one freer won");
    });
}

/// The W8 free-path hardening protocol (plan 06; `central.rs` insert_batch +
/// `allocator.rs` retire): under any interleaving of (a) the two frees that
/// empty a span, (b) a stale double-free racing them, and (c) the retirement
/// itself, **exactly one** caller is told to deactivate, the deactivation
/// runs exactly once, `live` never underflows, and a stale free past the
/// teardown mutates nothing. This is the model of the two W8 guards — the
/// span-state check and the `inserted > 0` gate — whose absence allowed a
/// double-deactivation (`span_count` corruption) and a release-mode
/// `live_count` underflow.
///
/// The model mirrors the real shape: the central bin lock is a `Mutex` (the
/// real `CentralLock` spinlock has the same acquire/release semantics), the
/// span fields it protects are plain loads/stores under that lock, and the
/// empty-span cache is "full" so the emptier must claim the deactivation —
/// the configuration in which the historical bugs fired.
#[test]
fn w8_span_retirement_is_claimed_exactly_once() {
    use loom::sync::atomic::AtomicU8;

    const ACTIVE: u8 = 0;
    const RELEASED: u8 = 1;
    const OBJECTS: u8 = 2; // a 2-object span: bits 0 and 1

    struct SpanModel {
        /// Bin lock (outer); everything below is touched only while held.
        bin: Mutex<()>,
        state: AtomicU8,
        bitmap: AtomicU8,
        live: AtomicU8,
        deactivations: AtomicU8,
    }

    /// `insert_batch(span, [i], 1)` with the two W8 guards; returns
    /// "caller must deactivate".
    fn insert(m: &SpanModel, i: u8) -> bool {
        let _g = m.bin.lock().unwrap();
        // Guard 1: a span past its teardown absorbs nothing (state check
        // under the bin lock — `deactivate` writes it under the same lock).
        if m.state.load(Ordering::Relaxed) != ACTIVE {
            return false;
        }
        let bits = m.bitmap.load(Ordering::Relaxed);
        let bit = 1u8 << i;
        let inserted = bits & bit == 0;
        if inserted {
            m.bitmap.store(bits | bit, Ordering::Relaxed);
            let live = m.live.load(Ordering::Relaxed);
            assert!(live > 0, "live_count underflow: the historical bug");
            m.live.store(live - 1, Ordering::Relaxed);
        }
        let empty = m.bitmap.load(Ordering::Relaxed) == (1 << OBJECTS) - 1
            && m.live.load(Ordering::Relaxed) == 0;
        // Guard 2: only the insert that *made* the span empty owns the
        // transition (`inserted > 0`); the empty cache is full in this model,
        // so that owner must deactivate.
        empty && inserted
    }

    /// `deactivate_span` (the caller-side retirement claim).
    fn deactivate(m: &SpanModel) {
        let _g = m.bin.lock().unwrap();
        assert_eq!(
            m.state.load(Ordering::Relaxed),
            ACTIVE,
            "double deactivation: the historical span_count corruption"
        );
        assert_eq!(m.live.load(Ordering::Relaxed), 0, "C-004: must be empty");
        m.state.store(RELEASED, Ordering::Relaxed);
        m.deactivations.fetch_add(1, Ordering::Relaxed);
    }

    loom::model(|| {
        let m = Arc::new(SpanModel {
            bin: Mutex::new(()),
            state: AtomicU8::new(ACTIVE),
            bitmap: AtomicU8::new(0),
            live: AtomicU8::new(OBJECTS),
            deactivations: AtomicU8::new(0),
        });

        // T1 frees object 0; T2 frees object 1; T3 *double*-frees object 0,
        // racing both. When T3 arrives first it is indistinguishable from the
        // legitimate free (the duplicate that loses is whichever runs second
        // — that is what a double free *is*), so every thread follows the
        // same protocol: whichever insert empties the span retires it. The
        // invariant is global: of four insert attempts on a two-object span,
        // exactly two insert, exactly one claims, the claim deactivates
        // exactly once, and no count ever underflows — under EVERY
        // interleaving (the asserts inside `insert`/`deactivate` encode the
        // two historical corruptions).
        let spawn_free = |i: u8| {
            let m = m.clone();
            thread::spawn(move || {
                if insert(&m, i) {
                    deactivate(&m);
                }
            })
        };
        let t1 = spawn_free(0);
        let t2 = spawn_free(1);
        let t3 = spawn_free(0); // the double free
        t1.join().unwrap();
        t2.join().unwrap();
        t3.join().unwrap();

        // Exactly one retirement; the span ended Released with clean counts.
        assert_eq!(m.deactivations.load(Ordering::Relaxed), 1);
        assert_eq!(m.state.load(Ordering::Relaxed), RELEASED);
        assert_eq!(m.live.load(Ordering::Relaxed), 0);
        assert_eq!(m.bitmap.load(Ordering::Relaxed), (1 << OBJECTS) - 1);
    });
}

/// The W9 arena quota protocol (plan 06; `arena.rs` `ArenaTable::try_charge` /
/// `credit` / `reserve_child_quota_locked`): under any interleaving of two
/// concurrent committing operations against a fixed quota, the committed counter
/// **never exceeds the quota** and two operations that together overflow it can
/// never *both* succeed — the §36.4/§36.17 guarantee the lock-free allocation
/// gate must keep under contention. The model uses the identical CAS loop and
/// orderings the real `try_charge` uses (Acquire load, AcqRel/Acquire
/// compare-exchange on the `committed` counter), so it pins the ordering logic,
/// not the literal code. Because a delegation's quota *reservation*
/// (`reserve_child_quota_locked`) commits through the **same** CAS on the same
/// atomic, this also covers an own-allocation racing a reservation: the two
/// chargers below stand for {charge, reserve} equally (PR #13).
#[test]
fn w9_arena_quota_charge_never_exceeds_under_contention() {
    use loom::sync::atomic::AtomicU64;

    const QUOTA: u64 = 100;

    /// `ArenaTable::try_charge`'s bounded CAS loop: charge `size` iff it fits.
    fn try_charge(used: &AtomicU64, size: u64) -> bool {
        let mut cur = used.load(Ordering::Acquire);
        loop {
            let next = cur + size;
            if next > QUOTA {
                return false; // would exceed the quota (§36.4)
            }
            match used.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    loom::model(|| {
        let used = Arc::new(AtomicU64::new(0));
        // Two 60-byte charges; 60 + 60 = 120 > 100, so at most one can win.
        let charger = |used: Arc<AtomicU64>| thread::spawn(move || try_charge(&used, 60));
        let t1 = charger(used.clone());
        let t2 = charger(used.clone());
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // The ceiling is never breached under any interleaving, and the two
        // over-budget charges are not both admitted.
        assert!(
            used.load(Ordering::Acquire) <= QUOTA,
            "quota ceiling breached"
        );
        assert!(
            !(r1 && r2),
            "both 60-byte charges admitted into a 100-byte quota"
        );
        // A winner's charge is exactly recorded; if neither won, used stayed 0.
        let expected = if r1 ^ r2 { 60 } else { 0 };
        assert_eq!(used.load(Ordering::Acquire), expected);
    });
}

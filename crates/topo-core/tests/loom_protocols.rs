// SPDX-License-Identifier: MIT
//! `loom` model-checks of the two subtle lock-free protocols W3 relies on — the
//! **seqlock** (W3-4, `SpanDescriptor::read_consistent_geometry` /
//! `LargeDescriptor::read_consistent`) and the **publish/read** of the pagemap
//! radix (W3-3c, `child_or_create` / leaf-entry store vs. `lookup`). `loom`
//! exhaustively explores every thread interleaving and memory-ordering outcome
//! permitted by the C11 model, so these are a machine-checked complement to the
//! std-thread stress tests (which exercise *some* interleavings on one machine).
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
use loom::sync::Arc;
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

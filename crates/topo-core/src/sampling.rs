// SPDX-License-Identifier: MIT
//! Heap/lifetime **sampling** mechanism (§31.4, plan 07 W17-3, the minimal slice that
//! feeds W14 placement profiles live).
//!
//! Profiling must capture *which* allocations live where without making the common path
//! slower or — the trap §31.4 / Appendix F call out — re-entering the allocator from
//! inside an allocation. This module provides the **pure, `no_std`, allocation-free**
//! pieces; the platform glue (thread-local instances, a monotonic clock, the actual
//! stack unwind, and the malloc/free hooks) lives in `topo-abi`, which owns `std`.
//!
//! The pieces, mapped to the plan's decomposition:
//!
//! * [`Sampler`] (**W17-3a**): a per-thread Poisson "bytes-until-next-sample" counter.
//!   The sampling *decision* touches only this thread-local state — **no lock, no
//!   syscall, no allocation** on the hot path. The inter-sample interval is an
//!   exponential draw (a Poisson process) computed in fixed point so the core stays
//!   floating-point-free (§6).
//! * [`StackBuf`] (**W17-3b**): a fixed, caller-owned frame buffer the platform unwinder
//!   fills *without allocating* (`libc::backtrace` into this array in `topo-abi`); it
//!   folds to an opaque [`StackId`]. The buffer never grows, so the unwind cannot recurse
//!   into the allocator.
//! * [`SampledObjects`] (**W17-3c**): the live-sampled-object set, for resolving a
//!   freed object's lifetime and for right-censored accounting at dump (§31.4). Bounded
//!   and allocation-free.
//! * [`SampleBloom`] (**W17-3c**, DD-1 *F2*): a lock-free atomic Bloom filter so the
//!   **free** hot path can answer "definitely not sampled" without taking the sampled-set
//!   lock — only a (rare) maybe-positive consults [`SampledObjects`].
//!
//! Aggregation into per-site profiles (**W17-3d**) is [`crate::placement::SiteProfileTable`].
//!
//! Nothing here is on the allocator's correctness path: a missed, dropped, or wrongly
//! attributed sample only blurs a profile, never a free (§24.5).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::placement::StackId;

/// Sampling configuration (§31.4 "rate configurable").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SampleConfig {
    /// Mean bytes between samples (the Poisson rate). A larger value samples *less*
    /// often (lower overhead, coarser profiles). `0` disables sampling entirely.
    pub sample_rate_bytes: u64,
}

impl SampleConfig {
    /// The §31.4 default mean sample interval: 1 MiB between samples — TCMalloc-class
    /// overhead (well under 1%) while still attributing the live heap.
    pub const DEFAULT_RATE_BYTES: u64 = 1 << 20;

    /// Sampling disabled (never fires).
    pub const DISABLED: SampleConfig = SampleConfig {
        sample_rate_bytes: 0,
    };

    /// Whether sampling is enabled.
    #[inline]
    pub const fn enabled(self) -> bool {
        self.sample_rate_bytes != 0
    }
}

impl Default for SampleConfig {
    #[inline]
    fn default() -> Self {
        SampleConfig {
            sample_rate_bytes: Self::DEFAULT_RATE_BYTES,
        }
    }
}

/// A tiny SplitMix64 PRNG stream — fast, `no_std`, and statelessly seedable, so each
/// thread's [`Sampler`] gets an independent deterministic stream. Quality is ample for
/// drawing sampling intervals (it is not, and need not be, cryptographic).
#[derive(Clone, Copy, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// SplitMix64's golden-ratio increment.
    const GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

    /// Seed the stream. Any seed (incl. `0`) yields a full-period stream.
    #[inline]
    pub const fn new(seed: u64) -> Rng {
        Rng { state: seed }
    }

    /// The next 64-bit value (SplitMix64).
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(Self::GAMMA);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// `ln(2)` in Q32 fixed point (`round(0.6931471805599453 × 2^32)`).
const LN2_Q32: u64 = 2_977_044_472;

/// Draw an exponential inter-sample interval with mean `mean` bytes (a Poisson process),
/// in fixed point so the core needs no floating point (§6).
///
/// For a uniform `r` (we use `U = (r+1) / 2^64 ∈ (0, 1]`), an exponential variate is
/// `mean × (−ln U)`. We compute `−ln U = ln2 × (−log2 U) = ln2 × (64 − log2 r)` with a
/// fixed-point `log2` whose fractional part is the *linear* mantissa approximation
/// (`log2(1+f) ≈ f`, error ≤ ~0.086 bit). That biases the mean by a small, constant
/// factor which [`MEAN_CORRECTION_Q32`] cancels, so the empirical sample rate matches the
/// configured `mean` (the `sampler_mean_interval_matches_rate` test pins it). The result
/// is clamped to `≥ 1` so progress is guaranteed.
#[inline]
fn exp_interval(mean: u64, rng: &mut Rng) -> u64 {
    // `r = 0` would mean `U = 1/2^64` (the largest interval); shifting handles it.
    let r = rng.next_u64();
    // log2(r) in Q32: integer part `63 - clz`, fractional part = the bits just below the
    // leading 1 (linear mantissa approximation). For `r == 0`, treat log2 as 0 so the
    // interval is the maximal `64·ln2·mean` — a vanishingly rare long gap, harmless.
    let log2_r_q32 = if r == 0 {
        0
    } else {
        let clz = r.leading_zeros() as u64; // 0..=63
        let int_part = 63 - clz; // floor(log2 r)
                                 // Normalize so the leading 1 is the top bit, then drop it; the next 32 bits are
                                 // the fractional mantissa `f` (≈ log2 of the mantissa).
        let norm = r << clz; // top bit set
        let frac_q32 = (norm << 1) >> 32; // drop leading 1, take 32 high bits
        (int_part << 32) | frac_q32
    };
    // −log2(U) = 64 − log2(r), in Q32. `r ≥ 1 ⇒ log2_r_q32 ≤ 63·2^32 + … < 64·2^32`.
    let neg_log2_u_q32 = (64u64 << 32).saturating_sub(log2_r_q32);
    // −ln(U) = ln2 · (−log2 U), in Q32.
    let neg_ln_u_q32 = ((neg_log2_u_q32 as u128 * LN2_Q32 as u128) >> 32) as u64;
    // interval = mean · (−ln U) · correction, de-scaling the two Q32 factors.
    let scaled = (mean as u128 * neg_ln_u_q32 as u128) >> 32;
    let corrected = (scaled * MEAN_CORRECTION_Q32 as u128) >> 32;
    (corrected as u64).max(1)
}

/// Cancels the small positive bias the linear-mantissa `log2` approximation introduces in
/// [`exp_interval`], so `E[interval] ≈ mean`. `log2(1+f) ≈ f` overstates `−log2 U` by a
/// constant `≈ 0.0573` bits on average; the correction `1/(1 + 0.0573·ln2)` ≈ `0.9609`,
/// i.e. `round(0.9609 × 2^32)`. (Pinned empirically by the mean-interval test.)
const MEAN_CORRECTION_Q32: u64 = 4_127_133_696;

/// A per-thread **Poisson sampling counter** (§31.4 / W17-3a). `should_sample(bytes)`
/// decrements a thread-local "bytes until next sample" budget and fires (re-arming with a
/// fresh exponential interval) when it crosses zero. The decision reads and writes only
/// this object — **no lock, no syscall, no allocation** — so it is safe on the hottest
/// allocation path; only a *fired* sample runs the (rare) slow capture/record path.
#[derive(Clone, Copy, Debug)]
pub struct Sampler {
    config: SampleConfig,
    rng: Rng,
    /// Bytes remaining until the next sample fires. Signed so a large allocation can
    /// overshoot; re-armed by adding a fresh interval.
    bytes_until: i64,
}

impl Sampler {
    /// A sampler with the given `config`, its PRNG seeded by `seed` (use a per-thread
    /// seed so threads sample independently). The first interval is drawn immediately.
    #[inline]
    pub fn new(config: SampleConfig, seed: u64) -> Sampler {
        let mut rng = Rng::new(seed);
        let bytes_until = if config.enabled() {
            exp_interval(config.sample_rate_bytes, &mut rng) as i64
        } else {
            i64::MAX
        };
        Sampler {
            config,
            rng,
            bytes_until,
        }
    }

    /// Whether sampling is enabled.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.config.enabled()
    }

    /// The configured mean sample interval (bytes).
    #[inline]
    pub fn rate_bytes(&self) -> u64 {
        self.config.sample_rate_bytes
    }

    /// **The hot-path decision (W17-3a).** Account `bytes` against the budget; return
    /// `true` exactly when this allocation crosses a sample point, re-arming the budget
    /// with a fresh exponential interval. `false` (no sample) is the overwhelmingly common
    /// outcome and costs one subtraction and one branch. Disabled samplers never fire.
    #[inline]
    pub fn should_sample(&mut self, bytes: usize) -> bool {
        if !self.config.enabled() {
            return false;
        }
        self.bytes_until -= bytes as i64;
        if self.bytes_until > 0 {
            return false;
        }
        // Crossed: re-arm. A single huge allocation that overshoots several intervals
        // still fires once here; the residual (possibly still ≤ 0) is carried so the next
        // allocation re-checks — a slight, bounded over-sampling of very large objects.
        self.bytes_until += exp_interval(self.config.sample_rate_bytes, &mut self.rng) as i64;
        true
    }
}

/// The maximum stack depth a [`StackBuf`] records. Deep enough to disambiguate call
/// sites, small enough to keep the buffer (and the unwind) cheap and fixed.
pub const MAX_STACK_FRAMES: usize = 32;

/// A fixed, allocation-free **captured-stack buffer** (§31.4 / W17-3b). The platform
/// unwinder fills it (e.g. `libc::backtrace` writing return addresses) — it **never
/// grows**, so the capture cannot re-enter the allocator — and it folds to an opaque
/// [`StackId`]. `Copy`, so it lives in thread-local storage with no indirection.
#[derive(Clone, Copy, Debug)]
pub struct StackBuf {
    frames: [usize; MAX_STACK_FRAMES],
    len: usize,
}

impl Default for StackBuf {
    #[inline]
    fn default() -> Self {
        StackBuf::new()
    }
}

impl StackBuf {
    /// An empty buffer.
    #[inline]
    pub const fn new() -> StackBuf {
        StackBuf {
            frames: [0; MAX_STACK_FRAMES],
            len: 0,
        }
    }

    /// Reset to empty (reuse the same fixed storage for the next capture).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Append a frame address (ignored once the fixed buffer is full — capture is bounded).
    #[inline]
    pub fn push(&mut self, frame: usize) {
        if self.len < MAX_STACK_FRAMES {
            self.frames[self.len] = frame;
            self.len += 1;
        }
    }

    /// The raw mutable frame storage, for a platform unwinder that fills a `&mut [usize]`
    /// directly (e.g. `libc::backtrace`). After filling, call
    /// [`set_len`](Self::set_len) with the count it returned.
    #[inline]
    pub fn frames_mut(&mut self) -> &mut [usize; MAX_STACK_FRAMES] {
        &mut self.frames
    }

    /// Set the number of valid frames after a direct fill (clamped to the capacity).
    #[inline]
    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(MAX_STACK_FRAMES);
    }

    /// The captured frames.
    #[inline]
    pub fn frames(&self) -> &[usize] {
        &self.frames[..self.len]
    }

    /// Fold the captured frames into an opaque [`StackId`] (order-sensitive SplitMix
    /// mixing). An empty capture yields [`StackId::UNKNOWN`] so an un-attributed sample is
    /// never confused with a real site. Collisions cost only profile quality (§24.5).
    #[inline]
    pub fn stack_id(&self) -> StackId {
        if self.len == 0 {
            return StackId::UNKNOWN;
        }
        let mut h: u64 = 0x9e37_79b9_7f4a_7c15 ^ (self.len as u64);
        for &f in &self.frames[..self.len] {
            h ^= f as u64;
            // SplitMix64 finalizer, so frame *order* matters and bits mix well.
            h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            h ^= h >> 31;
        }
        // Map the (vanishingly unlikely) `0` fold to a fixed non-zero key so a real
        // capture never reads as UNKNOWN.
        StackId(if h == 0 { 1 } else { h })
    }
}

/// A record of one live sampled object (§31.4 / W17-3c).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SampledRecord {
    /// The site that allocated it.
    pub stack_id: StackId,
    /// The **requested** byte size (what the application asked for) — the heap-profile
    /// quantity reported as `sampled_live_bytes`, and the left operand of the §31.5
    /// internal-fragmentation estimate `usable − bytes`.
    pub bytes: u64,
    /// The **usable** byte size the allocator actually handed back (`>= bytes`). The
    /// difference `usable − bytes` is this object's internal fragmentation (§31.5,
    /// W17-4); summed over the live sampled set it is the sampled internal-fragmentation
    /// metric.
    pub usable: u64,
    /// The time (ms) it was allocated, for lifetime resolution.
    pub alloc_ms: u64,
}

/// One open-addressing slot of [`SampledObjects`]. `addr == 0` marks empty (a real
/// allocation is never at address 0).
#[derive(Clone, Copy)]
struct ObjSlot {
    addr: usize,
    rec: SampledRecord,
}

impl ObjSlot {
    const EMPTY: ObjSlot = ObjSlot {
        addr: 0,
        rec: SampledRecord {
            stack_id: StackId::UNKNOWN,
            bytes: 0,
            usable: 0,
            alloc_ms: 0,
        },
    };
}

/// The **live sampled-object set** (§31.4 / W17-3c): a fixed-capacity, allocation-free map
/// from a sampled object's address to its [`SampledRecord`], so a later free resolves its
/// lifetime and still-live objects can be right-censored at dump. Open addressing with
/// linear probing and backward-shift deletion (no tombstones), capped at a 7/8 load so
/// probes stay short. Inserts past the cap are *dropped* (counted) rather than evicting a
/// live record — a bounded, intended loss.
///
/// Consulted under the host's sampled-set lock; the lock-free [`SampleBloom`] keeps the
/// common (non-sampled) free off this path entirely.
pub struct SampledObjects<const CAP: usize> {
    slots: [ObjSlot; CAP],
    len: usize,
    /// Cumulative inserts dropped because the set was at capacity.
    dropped: u64,
}

impl<const CAP: usize> SampledObjects<CAP> {
    const _CAP_POW2: () = assert!(CAP.is_power_of_two(), "SampledObjects CAP must be 2^k");

    /// An empty set.
    #[allow(clippy::new_without_default)]
    pub const fn new() -> Self {
        let () = Self::_CAP_POW2;
        SampledObjects {
            slots: [ObjSlot::EMPTY; CAP],
            len: 0,
            dropped: 0,
        }
    }

    /// The 7/8 load cap (keeps linear-probe chains short and lookups bounded in practice).
    #[inline]
    const fn max_load(&self) -> usize {
        CAP - CAP / 8
    }

    #[inline]
    fn home(addr: usize) -> usize {
        // Mix the address (low bits are page/alignment-correlated) before masking.
        let h = (addr as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        ((h >> 29) as usize) & (CAP - 1)
    }

    /// Number of live records tracked.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the set is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Cumulative dropped inserts (capacity pressure).
    #[inline]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Record a sampled allocation at `addr`. Returns `true` if tracked, `false` if the
    /// set was full (the record is dropped and counted) or `addr` is 0/already present.
    pub fn on_alloc(&mut self, addr: usize, rec: SampledRecord) -> bool {
        if addr == 0 {
            return false;
        }
        if self.len >= self.max_load() {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        let mut i = Self::home(addr);
        loop {
            let s = &self.slots[i];
            if s.addr == 0 {
                self.slots[i] = ObjSlot { addr, rec };
                self.len += 1;
                return true;
            }
            if s.addr == addr {
                // Duplicate address (a prior free was missed): overwrite with the newer
                // record rather than double-count.
                self.slots[i].rec = rec;
                return false;
            }
            i = (i + 1) & (CAP - 1);
        }
    }

    /// Put back a record that was removed for an in-flight realloc which then **failed**
    /// (§25.1 leaves the original allocation live, so its sample must survive untouched).
    ///
    /// Deliberately *not* [`on_alloc`](Self::on_alloc): that path drops the insert once
    /// the table is at its load cap, which is the right answer for a *new* sample the
    /// table never held. This record is different — it was already tracked and its object
    /// is still live, so dropping it silently removes a live allocation from sampled live
    /// bytes and from its site's lifetime histogram, permanently. The window is real: the
    /// take frees a slot, and another thread can fill it while the core realloc runs, so
    /// by the time the restore arrives the table can be back at the cap.
    ///
    /// Honouring one insert past the cap is safe because the cap is a *probe-length*
    /// policy, not the capacity: `max_load == CAP − CAP/8`, so an empty slot always
    /// exists below `CAP` and the probe terminates. Only a genuinely full table (which
    /// the cap makes unreachable) declines, and that is counted like any other drop.
    pub fn on_alloc_restore(&mut self, addr: usize, rec: SampledRecord) -> bool {
        if addr == 0 {
            return false;
        }
        if self.len >= CAP {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        let mut i = Self::home(addr);
        loop {
            let s = &self.slots[i];
            if s.addr == 0 {
                self.slots[i] = ObjSlot { addr, rec };
                self.len += 1;
                return true;
            }
            if s.addr == addr {
                // The address was re-vended and re-sampled while the realloc was in
                // flight. The newer record describes the allocation that owns it now, so
                // it wins; ours is stale and its object is gone by definition.
                return false;
            }
            i = (i + 1) & (CAP - 1);
        }
    }

    /// Resolve and **remove** the record for `addr`, if present (a sampled object being
    /// freed). `None` if `addr` was not a tracked sample.
    pub fn on_free(&mut self, addr: usize) -> Option<SampledRecord> {
        if addr == 0 {
            return None;
        }
        let mut i = Self::home(addr);
        // Bounded by the 7/8 load: an empty slot terminates the probe.
        for _ in 0..CAP {
            let s = &self.slots[i];
            if s.addr == 0 {
                return None; // not present
            }
            if s.addr == addr {
                let rec = s.rec;
                self.remove_at(i);
                self.len -= 1;
                return Some(rec);
            }
            i = (i + 1) & (CAP - 1);
        }
        None
    }

    /// Backward-shift deletion (Knuth 6.4 Algorithm R): clear slot `i`, then pull back any
    /// following probe-chain entries that belong before it, so no lookup chain is broken
    /// and no tombstone is left behind.
    fn remove_at(&mut self, i: usize) {
        let mut hole = i;
        let mut j = i;
        loop {
            j = (j + 1) & (CAP - 1);
            let addr_j = self.slots[j].addr;
            if addr_j == 0 {
                break; // end of the chain
            }
            let home_j = Self::home(addr_j);
            // `j`'s entry can fill `hole` iff `home_j` is not cyclically within (hole, j].
            // (i.e. moving it back to `hole` keeps it reachable from its home.)
            let can_move = if hole <= j {
                !(hole < home_j && home_j <= j)
            } else {
                // The window wraps past 0.
                !(home_j > hole || home_j <= j)
            };
            if can_move {
                self.slots[hole] = self.slots[j];
                hole = j;
            }
        }
        self.slots[hole] = ObjSlot::EMPTY;
    }

    /// Visit every live record (for right-censored accounting at dump, §31.4). The closure
    /// receives each [`SampledRecord`]; the set is left unchanged.
    pub fn for_each<F: FnMut(&SampledRecord)>(&self, mut f: F) {
        for s in self.slots.iter() {
            if s.addr != 0 {
                f(&s.rec);
            }
        }
    }

    /// Visit every live object's **address** (for re-priming a membership filter after a
    /// reset). The set is left unchanged.
    pub fn for_each_addr<F: FnMut(usize)>(&self, mut f: F) {
        for s in self.slots.iter() {
            if s.addr != 0 {
                f(s.addr);
            }
        }
    }
}

/// The number of `u64` words backing a [`SampleBloom`]'s bit array. `1024` words = 64 Kib
/// = comfortable false-positive rate for a few thousand live sampled objects.
pub const BLOOM_WORDS: usize = 1024;

/// A lock-free, fixed-size atomic **Bloom filter** over sampled-object addresses
/// (§31.4 / DD-1 *F2*). The **free** hot path asks [`maybe_contains`](Self::maybe_contains)
/// — a couple of relaxed atomic loads, **no lock** — and only consults the precise
/// [`SampledObjects`] set (under the lock) on a maybe-positive, which is rare. There are
/// **no false negatives**: a sampled address always reads positive, so a sampled free is
/// never missed; false positives merely cost an occasional needless lock.
///
/// Entries are never individually removed (a Bloom filter cannot); the host periodically
/// [`reset`](Self::reset)s and re-primes it from the live set, bounding the false-positive
/// rate over a long run.
pub struct SampleBloom {
    bits: [AtomicU64; BLOOM_WORDS],
}

impl Default for SampleBloom {
    fn default() -> Self {
        SampleBloom::new()
    }
}

impl SampleBloom {
    /// An empty filter.
    pub const fn new() -> SampleBloom {
        // `AtomicU64` is not `Copy`; an inline-const array repeat builds a fresh atomic per
        // slot (a plain `const ZERO` repeat would trip `declare_interior_mutable_const`).
        SampleBloom {
            bits: [const { AtomicU64::new(0) }; BLOOM_WORDS],
        }
    }

    /// Total addressable bits.
    #[inline]
    const fn nbits(&self) -> u64 {
        (BLOOM_WORDS as u64) * 64
    }

    /// The two hash positions for `addr` (a double-hashing scheme: `h1`, `h1 ^ h2`).
    #[inline]
    fn positions(&self, addr: usize) -> (u64, u64) {
        let a = addr as u64;
        let h1 = a.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 17;
        let h2 = (a ^ (a >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd) >> 17;
        let n = self.nbits();
        (h1 % n, (h1 ^ h2) % n)
    }

    /// Record that `addr` is (now) a sampled object. Idempotent; lock-free.
    #[inline]
    pub fn insert(&self, addr: usize) {
        let (p1, p2) = self.positions(addr);
        for p in [p1, p2] {
            let w = (p / 64) as usize;
            let bit = 1u64 << (p % 64);
            self.bits[w].fetch_or(bit, Ordering::Relaxed);
        }
    }

    /// Whether `addr` *might* be sampled. `false` is definitive (never sampled — the
    /// common free path); `true` means "consult the precise set". Lock-free.
    #[inline]
    pub fn maybe_contains(&self, addr: usize) -> bool {
        let (p1, p2) = self.positions(addr);
        for p in [p1, p2] {
            let w = (p / 64) as usize;
            let bit = 1u64 << (p % 64);
            if self.bits[w].load(Ordering::Relaxed) & bit == 0 {
                return false;
            }
        }
        true
    }

    /// Clear every bit (the host re-primes from the live set afterward to keep the
    /// false-positive rate bounded over a long run).
    pub fn reset(&self) {
        for w in self.bits.iter() {
            w.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_sampler_never_fires() {
        let mut s = Sampler::new(SampleConfig::DISABLED, 1);
        assert!(!s.enabled());
        for _ in 0..10_000 {
            assert!(!s.should_sample(4096));
        }
    }

    #[test]
    fn sampler_mean_interval_matches_rate() {
        // The empirical mean number of bytes between fired samples must track the
        // configured rate (the fixed-point exponential + correction). Drive a long stream
        // of fixed-size allocations and count fires.
        let rate = 1 << 16; // 64 KiB mean
        let mut s = Sampler::new(
            SampleConfig {
                sample_rate_bytes: rate,
            },
            0xDEAD_BEEF,
        );
        let step = 64usize;
        let n_allocs = 4_000_000usize; // 256 MiB of traffic
        let mut fires = 0u64;
        for _ in 0..n_allocs {
            if s.should_sample(step) {
                fires += 1;
            }
        }
        let total_bytes = (n_allocs * step) as u64;
        let mean_interval = total_bytes / fires.max(1);
        // Within 12% of the configured mean — tight enough to prove "rate configurable",
        // loose enough for the approximation + finite-sample noise.
        let lo = rate - rate * 12 / 100;
        let hi = rate + rate * 12 / 100;
        assert!(
            (lo..=hi).contains(&mean_interval),
            "mean interval {mean_interval} not within 12% of rate {rate} (fires={fires})"
        );
    }

    #[test]
    fn sampler_is_deterministic_given_seed() {
        let cfg = SampleConfig {
            sample_rate_bytes: 1 << 14,
        };
        let mut a = Sampler::new(cfg, 42);
        let mut b = Sampler::new(cfg, 42);
        for _ in 0..100_000 {
            assert_eq!(a.should_sample(128), b.should_sample(128));
        }
    }

    #[test]
    fn sampler_fires_on_a_single_huge_allocation() {
        // An allocation far larger than the mean must sample (it crosses many intervals).
        let mut s = Sampler::new(
            SampleConfig {
                sample_rate_bytes: 1 << 16,
            },
            7,
        );
        assert!(s.should_sample(1 << 24), "a 16 MiB alloc must be sampled");
    }

    #[test]
    fn stack_buf_hash_is_order_sensitive_and_stable() {
        let mut a = StackBuf::new();
        a.push(0x1000);
        a.push(0x2000);
        let mut b = StackBuf::new();
        b.push(0x2000);
        b.push(0x1000);
        assert_ne!(a.stack_id(), b.stack_id(), "frame order changes the id");
        // Stable: rebuilding the same frames yields the same id.
        let mut c = StackBuf::new();
        c.push(0x1000);
        c.push(0x2000);
        assert_eq!(a.stack_id(), c.stack_id());
        // Empty ⇒ UNKNOWN; a real capture never reads as UNKNOWN.
        assert_eq!(StackBuf::new().stack_id(), StackId::UNKNOWN);
        assert!(a.stack_id().is_known());
    }

    #[test]
    fn stack_buf_is_bounded_and_fills_directly() {
        let mut s = StackBuf::new();
        for i in 0..(MAX_STACK_FRAMES + 10) {
            s.push(i + 1);
        }
        assert_eq!(s.frames().len(), MAX_STACK_FRAMES, "capture is bounded");
        // Direct fill (the libc::backtrace path).
        let mut d = StackBuf::new();
        d.frames_mut()[0] = 0xAAAA;
        d.frames_mut()[1] = 0xBBBB;
        d.set_len(2);
        assert_eq!(d.frames(), &[0xAAAA, 0xBBBB]);
        d.set_len(999); // clamped
        assert_eq!(d.frames().len(), MAX_STACK_FRAMES);
    }

    #[test]
    fn sampled_objects_roundtrip_and_censor() {
        let mut set = SampledObjects::<16>::new();
        let rec = SampledRecord {
            stack_id: StackId(5),
            bytes: 4096,
            usable: 4096,
            alloc_ms: 100,
        };
        assert!(set.on_alloc(0x4000, rec));
        assert_eq!(set.len(), 1);
        // Right-censor scan sees it while live.
        let mut seen = 0;
        set.for_each(|r| {
            assert_eq!(r.stack_id, StackId(5));
            seen += 1;
        });
        assert_eq!(seen, 1);
        // Free resolves and removes.
        assert_eq!(set.on_free(0x4000), Some(rec));
        assert_eq!(set.on_free(0x4000), None);
        assert!(set.is_empty());
        // Address 0 and unknown addresses are safe no-ops.
        assert!(!set.on_alloc(0, rec));
        assert_eq!(set.on_free(0xDEAD), None);
    }

    #[test]
    fn sampled_objects_backward_shift_keeps_lookups_correct() {
        // Insert/remove a randomized stream and confirm every still-present key is found
        // and every removed key is gone — the backward-shift deletion must never break a
        // probe chain. Use addresses that collide in the same home bucket.
        let mut set = SampledObjects::<64>::new();
        let mut rng = Rng::new(1);
        let mut present: Vec<usize> = Vec::new();
        for _ in 0..10_000 {
            if present.is_empty() || rng.next_u64() & 1 == 0 {
                // Insert a fresh page-aligned address (some share home buckets).
                let addr = 0x1_0000 + ((rng.next_u64() as usize % 200) * 0x1000);
                let rec = SampledRecord {
                    stack_id: StackId(addr as u64),
                    bytes: 64,
                    usable: 64,
                    alloc_ms: 0,
                };
                if set.on_alloc(addr, rec) && !present.contains(&addr) {
                    present.push(addr);
                }
            } else {
                // Remove a present address and verify it resolves.
                let k = (rng.next_u64() as usize) % present.len();
                let addr = present.swap_remove(k);
                assert!(set.on_free(addr).is_some(), "present key must resolve");
            }
            // Spot-check: every recorded-present key is still findable (on_free then
            // re-insert to leave the set unchanged would be heavy; instead check len).
        }
        assert_eq!(set.len(), present.len());
        // Every remaining key resolves exactly once.
        for &addr in &present {
            assert!(set.on_free(addr).is_some(), "leftover key {addr:#x} lost");
        }
        assert!(set.is_empty());
    }

    #[test]
    fn sampled_objects_drops_past_capacity_without_eviction() {
        let mut set = SampledObjects::<16>::new();
        let cap_load = 16 - 16 / 8; // 14
        for k in 0..50usize {
            let addr = 0x1000 + k * 0x1000;
            set.on_alloc(
                addr,
                SampledRecord {
                    stack_id: StackId(k as u64 + 1),
                    bytes: 64,
                    usable: 64,
                    alloc_ms: 0,
                },
            );
        }
        assert_eq!(set.len(), cap_load, "load is capped");
        assert!(
            set.dropped() > 0,
            "excess inserts were dropped, not evicted"
        );
    }

    #[test]
    fn bloom_has_no_false_negatives() {
        let bloom = SampleBloom::new();
        let mut rng = Rng::new(99);
        let mut addrs: Vec<usize> = Vec::new();
        for _ in 0..2000 {
            let addr = rng.next_u64() as usize | 0x1000;
            bloom.insert(addr);
            addrs.push(addr);
        }
        // Every inserted address reads positive (no false negatives — the safety property).
        for &addr in &addrs {
            assert!(bloom.maybe_contains(addr), "false negative for {addr:#x}");
        }
        // Reset clears everything.
        bloom.reset();
        // After reset a fresh (never-inserted) address is negative (probabilistically; the
        // all-zero filter is exact).
        assert!(!bloom.maybe_contains(0xABCD_1234));
    }

    /// W17-3c: a realloc that **fails** must give its sample back, even when the table
    /// filled up while the realloc was in flight.
    ///
    /// `take_for_realloc` frees a slot, and another thread can take that slot before the
    /// restore lands — so the restore routinely arrives at a table sitting at its 7/8 load
    /// cap. Routing it through `on_alloc` drops it there, which is right for a *new*
    /// sample but erases a **live** allocation from sampled live bytes and its site's
    /// lifetime histogram for good. The restore path must honour it.
    #[test]
    fn a_failed_realloc_restores_its_sample_even_at_the_load_cap() {
        let mut set = SampledObjects::<16>::new();
        let rec = |n: u64| SampledRecord {
            stack_id: StackId(7),
            bytes: n,
            usable: n,
            alloc_ms: n,
        };
        // Fill to the 7/8 cap (14 of 16).
        for i in 0..14u64 {
            assert!(set.on_alloc(0x1_0000 + (i as usize) * 0x100, rec(i)));
        }
        assert_eq!(set.len(), 14);
        assert!(
            !set.on_alloc(0xDEAD_0000, rec(99)),
            "the cap drops a new sample, which is the intended bounded loss"
        );

        // A realloc takes one out...
        let victim = 0x1_0000;
        let taken = set.on_free(victim).expect("the victim was tracked");
        assert_eq!(set.len(), 13);
        // ...another thread fills the slot it freed, putting the table back at the cap...
        assert!(set.on_alloc(0xBEEF_0000, rec(42)));
        assert_eq!(set.len(), 14);

        // ...and the realloc fails, so the original is still live and must come back.
        // The route the restore used to take drops it here — that is precisely the bug:
        // `on_alloc` cannot tell a new sample from a returning one, and at the cap it
        // silently loses a live object's accounting.
        assert!(
            !set.on_alloc(victim, taken),
            "on_alloc drops at the cap — the behaviour the restore path must not inherit"
        );
        assert!(
            set.on_alloc_restore(victim, taken),
            "a live object's sample was dropped because the table refilled during its \
             realloc"
        );
        assert_eq!(set.len(), 15);
        let mut found = false;
        set.for_each(|r| {
            if r.alloc_ms == 0 && r.stack_id == StackId(7) {
                found = true;
            }
        });
        assert!(
            found,
            "the restored record must be visible to the live scan"
        );

        // A restore whose address was re-vended and re-sampled meanwhile loses to the
        // newer record: that address belongs to the new allocation now.
        let newer = set.on_free(0xBEEF_0000).expect("tracked");
        set.on_free(0x1_0000 + 0x100)
            .expect("a filler to make room under the cap");
        assert!(set.on_alloc(0xBEEF_0000, rec(43)));
        assert!(
            !set.on_alloc_restore(0xBEEF_0000, newer),
            "the newer record owns that address now; the stale restore must lose"
        );
    }
}

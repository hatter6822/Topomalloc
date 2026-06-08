// SPDX-License-Identifier: MIT
//! Cache budget controller (W6-5, plan 05).
//!
//! Adapts per-CPU soft capacities based on miss/overflow statistics. The budget
//! controller reads the per-slot miss and overflow counters (incremented
//! lock-free by the front-end), and adjusts soft capacities to balance cache
//! hit rate against total memory held in caches.
//!
//! **Algorithm.** For each active CPU and each size class:
//! - If misses exceed a threshold and the soft capacity is below the hard
//!   capacity, grow by one batch size (the slot needs more cache).
//! - If overflows exceed a threshold and the soft capacity is above the
//!   minimum (one batch), shrink by one batch size (the slot has too much).
//! - After adjusting, if the total capacity across all CPUs/SCs exceeds the
//!   global budget, shrink slots above the minimum (one batch) in index order
//!   until the budget is met.
//!
//! The controller is designed to run periodically (e.g. on a timer or on
//! every N-th allocation) and is not on the allocation fast path.

use crate::cpu_cache::CpuCache;
use crate::generated::tables::SIZE_CLASSES;
use crate::ids::SizeClassId;
use crate::size_class;

/// Number of size classes in the generated table.
const NUM_SIZE_CLASSES: usize = SIZE_CLASSES.len();

/// Default miss threshold: adapt when misses exceed this count since last
/// reset.
const DEFAULT_MISS_THRESHOLD: u64 = 64;

/// Default overflow threshold: adapt when overflows exceed this count since
/// last reset.
const DEFAULT_OVERFLOW_THRESHOLD: u64 = 64;

/// Default number of allocations between `adapt` calls.
const DEFAULT_ADAPT_INTERVAL: u64 = 4096;

/// The cache budget controller (W6-5).
///
/// **Periodic invocation.** The budget controller must be invoked periodically
/// by the allocator. Two strategies:
/// - **Timer-based:** Call [`adapt`](Self::adapt) every N milliseconds (e.g., 10ms).
/// - **Count-based:** Call [`adapt`](Self::adapt) every N allocations. The fast
///   path increments a per-CPU counter and calls `adapt` when the counter
///   crosses [`adapt_interval`](Self::adapt_interval).
///
/// **Thread cache budgets.** The controller currently manages per-CPU cache
/// soft capacities. Per-thread cache budgets are managed independently by
/// [`ThreadCache::set_budget`](crate::thread_cache::ThreadCache::set_budget);
/// a future enhancement may unify them under a single global budget.
pub struct CacheBudget {
    /// Global budget: maximum total soft capacity across all CPUs and SCs
    /// (in objects). When the total exceeds this, slots above the minimum are
    /// shrunk in index order (lowest CPU, lowest SC first).
    global_budget: usize,
    /// Miss threshold for growing a slot.
    miss_threshold: u64,
    /// Overflow threshold for shrinking a slot.
    overflow_threshold: u64,
    /// Number of allocations between `adapt` calls (count-based invocation).
    adapt_interval: u64,
    /// Per-instance allocation counter for `should_adapt`.
    alloc_counter: core::sync::atomic::AtomicU64,
}

impl CacheBudget {
    /// Create a budget controller with the given global budget.
    pub const fn new(global_budget: usize) -> Self {
        Self {
            global_budget,
            miss_threshold: DEFAULT_MISS_THRESHOLD,
            overflow_threshold: DEFAULT_OVERFLOW_THRESHOLD,
            adapt_interval: DEFAULT_ADAPT_INTERVAL,
            alloc_counter: core::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The global budget (total objects across all CPUs and SCs).
    #[inline]
    pub fn global_budget(&self) -> usize {
        self.global_budget
    }

    /// Set the global budget.
    #[inline]
    pub fn set_global_budget(&mut self, budget: usize) {
        self.global_budget = budget;
    }

    /// The miss threshold (misses above this trigger a grow).
    #[inline]
    pub fn miss_threshold(&self) -> u64 {
        self.miss_threshold
    }

    /// Set the miss threshold.
    #[inline]
    pub fn set_miss_threshold(&mut self, threshold: u64) {
        self.miss_threshold = threshold;
    }

    /// The overflow threshold (overflows above this trigger a shrink).
    #[inline]
    pub fn overflow_threshold(&self) -> u64 {
        self.overflow_threshold
    }

    /// Set the overflow threshold.
    #[inline]
    pub fn set_overflow_threshold(&mut self, threshold: u64) {
        self.overflow_threshold = threshold;
    }

    /// The adapt interval (allocations between `adapt` calls).
    #[inline]
    pub fn adapt_interval(&self) -> u64 {
        self.adapt_interval
    }

    /// Set the adapt interval.
    #[inline]
    pub fn set_adapt_interval(&mut self, interval: u64) {
        self.adapt_interval = interval;
    }

    /// Returns `true` every `adapt_interval` calls, for count-based periodic
    /// invocation. Thread-safe (lock-free atomic counter).
    ///
    /// Fires on the very first call (counter = 0) to prime the controller;
    /// subsequent fires occur every `adapt_interval` calls.
    /// Returns `false` unconditionally when `adapt_interval` is 0.
    #[inline]
    pub fn should_adapt(&self) -> bool {
        let n = self
            .alloc_counter
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.adapt_interval > 0 && n.is_multiple_of(self.adapt_interval)
    }

    /// Run one adaptation cycle (W6-5). Reads miss/overflow counters for each
    /// active CPU and size class, adjusts soft capacities, and enforces the
    /// global budget. Returns the total soft capacity after adaptation.
    pub fn adapt(&self, cpu_cache: &CpuCache) -> usize {
        let active = cpu_cache.active_cpus() as usize;
        if active == 0 {
            return 0;
        }

        // Phase 1: per-slot adaptation based on miss/overflow stats.
        for cpu_idx in 0..active {
            let cpu = match cpu_cache.per_cpu(crate::fe::CoreId(cpu_idx as u32)) {
                Some(c) => c,
                None => continue,
            };

            for sc_idx in 0..NUM_SIZE_CLASSES {
                let sc = SizeClassId::new(sc_idx);
                let slot = match cpu.slot(sc) {
                    Some(s) => s,
                    None => continue,
                };
                if !slot.is_initialized() {
                    continue;
                }

                let batch = size_class::batch(sc) as u32;
                let hard_cap = slot.hard_capacity();
                let cur_soft = slot.soft_capacity();

                // Read and reset counters.
                let misses = slot.reset_misses();
                let overflows = slot.reset_overflows();

                let mut new_soft = cur_soft;

                // Grow on high misses (the slot needs more cache).
                // Shrink on high overflows (the slot has too much).
                // Uses else-if: if both thresholds are exceeded, grow
                // takes precedence (miss recovery is more latency-critical
                // than overflow reduction).
                if misses > self.miss_threshold && cur_soft < hard_cap {
                    new_soft = cur_soft.saturating_add(batch).min(hard_cap);
                } else if overflows > self.overflow_threshold && cur_soft > batch {
                    new_soft = cur_soft.saturating_sub(batch).max(batch);
                }

                if new_soft != cur_soft {
                    slot.set_soft_capacity(new_soft);
                }
            }
        }

        // Phase 2: enforce the global budget. Repeatedly shrink slots that
        // are above the minimum batch size until the total is within budget
        // or no further reduction is possible. Iteration is in index order
        // (lowest CPU, lowest SC first); a future enhancement could sort by
        // activity to prefer shrinking the least-active slots.
        let mut total = self.compute_total_capacity(cpu_cache, active);

        while total > self.global_budget {
            let mut made_progress = false;
            for cpu_idx in 0..active {
                if total <= self.global_budget {
                    break;
                }
                let cpu = match cpu_cache.per_cpu(crate::fe::CoreId(cpu_idx as u32)) {
                    Some(c) => c,
                    None => continue,
                };
                for sc_idx in 0..NUM_SIZE_CLASSES {
                    if total <= self.global_budget {
                        break;
                    }
                    let sc = SizeClassId::new(sc_idx);
                    let slot = match cpu.slot(sc) {
                        Some(s) => s,
                        None => continue,
                    };
                    if !slot.is_initialized() {
                        continue;
                    }

                    let batch = size_class::batch(sc) as u32;
                    let cur_soft = slot.soft_capacity();
                    if cur_soft > batch {
                        let new_soft = cur_soft.saturating_sub(batch).max(batch);
                        let reduction = (cur_soft - new_soft) as usize;
                        slot.set_soft_capacity(new_soft);
                        total = total.saturating_sub(reduction);
                        made_progress = true;
                    }
                }
            }
            if !made_progress {
                break;
            }
        }

        total
    }

    /// Compute the total soft capacity across all active CPUs and SCs.
    fn compute_total_capacity(&self, cpu_cache: &CpuCache, active: usize) -> usize {
        let mut total = 0usize;
        for cpu_idx in 0..active {
            let cpu = match cpu_cache.per_cpu(crate::fe::CoreId(cpu_idx as u32)) {
                Some(c) => c,
                None => continue,
            };
            for sc_idx in 0..NUM_SIZE_CLASSES {
                let sc = SizeClassId::new(sc_idx);
                let slot = match cpu.slot(sc) {
                    Some(s) => s,
                    None => continue,
                };
                if slot.is_initialized() {
                    total = total.saturating_add(slot.soft_capacity() as usize);
                }
            }
        }
        total
    }

    /// Snapshot of per-slot stats for a specific CPU and size class.
    pub fn slot_stats(
        &self,
        cpu_cache: &CpuCache,
        cpu_idx: u32,
        sc: SizeClassId,
    ) -> Option<SlotStats> {
        let cpu = cpu_cache.per_cpu(crate::fe::CoreId(cpu_idx))?;
        let slot = cpu.slot(sc)?;
        if !slot.is_initialized() {
            return None;
        }
        Some(SlotStats {
            len: slot.len(),
            soft_capacity: slot.soft_capacity(),
            hard_capacity: slot.hard_capacity(),
            misses: slot.misses(),
            overflows: slot.overflows(),
        })
    }
}

impl Default for CacheBudget {
    fn default() -> Self {
        // Default budget: enough for 4 CPUs, each with batch_size (32) per
        // SC (72 classes) = 4 * 32 * 72 = 9216.
        Self::new(9216)
    }
}

/// Snapshot of per-slot statistics.
#[derive(Clone, Copy, Debug)]
pub struct SlotStats {
    /// Current number of cached objects.
    pub len: u32,
    /// Current soft capacity.
    pub soft_capacity: u32,
    /// Hard capacity (the absolute ceiling).
    pub hard_capacity: u32,
    /// Cache misses since last reset.
    pub misses: u64,
    /// Cache overflows since last reset.
    pub overflows: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::BumpArena;
    use crate::cpu_cache::CpuCache;
    use crate::fe::CoreId;
    use crate::ids::{ArenaId, SizeClassId};

    const A: ArenaId = ArenaId::DEFAULT;

    fn meta(bytes: usize) -> BumpArena {
        let buf = vec![0u8; bytes].into_boxed_slice();
        let len = buf.len();
        let ptr = Box::into_raw(buf).cast::<u8>();
        // SAFETY: ptr is a valid, owned allocation of `len` bytes from Box.
        unsafe { BumpArena::new(ptr, len) }
    }

    #[test]
    fn adapt_increases_capacity_on_high_misses() {
        let m = meta(4 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;

        // Init slot with batch_size as initial soft capacity.
        cc.init_slot(core, sc, &m, batch);

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        let initial_soft = slot.soft_capacity();
        assert_eq!(initial_soft, batch);

        // Simulate many misses.
        for _ in 0..100 {
            cc.fe_pop(core, A, sc, &m); // each miss increments the counter
        }

        let budget = CacheBudget::new(100_000);
        budget.adapt(&cc);

        // Soft capacity should have increased.
        let new_soft = slot.soft_capacity();
        assert!(
            new_soft > initial_soft,
            "soft capacity should grow on high misses: was {initial_soft}, now {new_soft}"
        );
    }

    #[test]
    fn adapt_decreases_capacity_on_high_overflows() {
        let m = meta(4 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Init slot with hard_capacity as soft capacity (maximum).
        cc.init_slot(core, sc, &m, hard_cap);

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert_eq!(slot.soft_capacity(), hard_cap);

        // Fill to hard capacity.
        for i in 0..hard_cap {
            cc.fe_push(core, A, sc, i as usize + 1, &m);
        }

        // Simulate many overflows.
        for _ in 0..100 {
            cc.fe_push(core, A, sc, 999, &m); // each overflow increments counter
        }

        let budget = CacheBudget::new(100_000);
        budget.adapt(&cc);

        // Soft capacity should have decreased.
        let new_soft = slot.soft_capacity();
        assert!(
            new_soft < hard_cap,
            "soft capacity should shrink on high overflows: was {hard_cap}, now {new_soft}"
        );
        assert!(new_soft >= batch, "should not shrink below batch size");
    }

    #[test]
    fn global_budget_constraint() {
        let m = meta(8 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(2);
        let sc = SizeClassId::new(0);
        let hard_cap = size_class::max_local_capacity(sc) as u32;
        let batch = size_class::batch(sc) as u32;

        // Init two CPUs with maximum soft capacity.
        for cpu_idx in 0..2u32 {
            let core = CoreId(cpu_idx);
            cc.init_slot(core, sc, &m, hard_cap);
        }

        // Set a budget that is reachable (at least batch per initialized
        // slot, since that is the minimum). 2 CPUs x 1 SC x batch = 64.
        let target_budget = (batch as usize) * 2;
        let budget = CacheBudget::new(target_budget);
        let total = budget.adapt(&cc);

        // Total should be at or under the global budget.
        assert!(
            total <= budget.global_budget(),
            "total {total} should be <= budget {}",
            budget.global_budget()
        );
        // Each slot should be at the minimum (batch size).
        for cpu_idx in 0..2u32 {
            let core = CoreId(cpu_idx);
            let cpu = cc.per_cpu(core).unwrap();
            let slot = cpu.slot(sc).unwrap();
            assert_eq!(
                slot.soft_capacity(),
                batch,
                "cpu {cpu_idx} should be at minimum batch size"
            );
        }
    }

    #[test]
    fn adapt_with_no_active_cpus() {
        let cc = CpuCache::new();
        cc.set_active_cpus(0);

        let budget = CacheBudget::new(1000);
        let total = budget.adapt(&cc);
        assert_eq!(total, 0);
    }

    #[test]
    fn slot_stats_returns_correct_values() {
        let m = meta(2 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;

        cc.init_slot(core, sc, &m, batch);

        // Push some addresses.
        cc.fe_push(core, A, sc, 100, &m);
        cc.fe_push(core, A, sc, 200, &m);

        let budget = CacheBudget::new(1000);
        let stats = budget.slot_stats(&cc, 0, sc).unwrap();
        assert_eq!(stats.len, 2);
        assert_eq!(stats.soft_capacity, batch);
        assert!(stats.hard_capacity > 0);
        assert_eq!(stats.misses, 0);
        assert_eq!(stats.overflows, 0);
    }

    #[test]
    fn default_budget_is_reasonable() {
        let budget = CacheBudget::default();
        assert!(budget.global_budget() > 0);
        assert_eq!(budget.miss_threshold(), DEFAULT_MISS_THRESHOLD);
        assert_eq!(budget.overflow_threshold(), DEFAULT_OVERFLOW_THRESHOLD);
    }

    #[test]
    fn should_adapt_fires_at_interval() {
        let mut budget = CacheBudget::new(100_000);
        budget.set_adapt_interval(100);

        let mut fire_count = 0u32;
        for _ in 0..500 {
            if budget.should_adapt() {
                fire_count += 1;
            }
        }
        // Should fire at 0, 100, 200, 300, 400 = 5 times.
        assert_eq!(fire_count, 5);
    }

    #[test]
    fn adapt_interval_zero_never_fires() {
        let mut budget = CacheBudget::new(100_000);
        budget.set_adapt_interval(0);

        let mut fire_count = 0u32;
        for _ in 0..500 {
            if budget.should_adapt() {
                fire_count += 1;
            }
        }
        assert_eq!(fire_count, 0);
    }

    #[test]
    fn simultaneous_misses_and_overflows_grow_wins() {
        let m = meta(4 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId::DEFAULT;
        let sc = SizeClassId::new(0);
        let batch = size_class::batch(sc) as u32;
        let hard_cap = size_class::max_local_capacity(sc) as u32;

        // Start at midpoint so both grow and shrink are possible.
        let mid = (batch + hard_cap) / 2;
        cc.init_slot(core, sc, &m, mid);

        let cpu = cc.per_cpu(core).unwrap();
        let slot = cpu.slot(sc).unwrap();
        assert_eq!(slot.soft_capacity(), mid);

        // Simulate high misses.
        for _ in 0..100 {
            cc.fe_pop(core, A, sc, &m);
        }
        // Also fill to cause overflows.
        for i in 0..mid {
            cc.fe_push(core, A, sc, i as usize + 1, &m);
        }
        for _ in 0..100 {
            cc.fe_push(core, A, sc, 999, &m);
        }

        let budget = CacheBudget::new(100_000);
        budget.adapt(&cc);

        // Grow should take precedence: soft capacity should increase.
        let new_soft = slot.soft_capacity();
        assert!(
            new_soft > mid,
            "grow should win when both thresholds exceeded: was {mid}, now {new_soft}"
        );
    }

    #[test]
    fn multi_sc_phase2_budget_enforcement() {
        let m = meta(8 * 1024 * 1024);
        let cc = CpuCache::new();
        cc.set_active_cpus(1);
        let core = CoreId(0);
        let batch = size_class::batch(SizeClassId::new(0)) as u32;

        // Init 4 size classes with max soft capacity.
        let mut initialized_scs = Vec::new();
        for i in 0..4 {
            let sc = SizeClassId::new(i);
            let hard = size_class::max_local_capacity(sc) as u32;
            cc.init_slot(core, sc, &m, hard);
            initialized_scs.push(sc);
        }

        // Budget: 4 slots x batch = minimum total. This forces Phase 2 to
        // shrink all slots down to their minimums.
        let target = (batch as usize) * 4;
        let budget = CacheBudget::new(target);
        let total = budget.adapt(&cc);

        assert!(
            total <= budget.global_budget(),
            "total {total} should be <= budget {}",
            budget.global_budget()
        );

        // All slots should have been reduced to minimum (batch size).
        let cpu = cc.per_cpu(core).unwrap();
        for &sc in &initialized_scs {
            let s = cpu.slot(sc).unwrap();
            let b = size_class::batch(sc) as u32;
            assert_eq!(
                s.soft_capacity(),
                b,
                "sc {} should be at minimum batch size after tight budget",
                sc.index()
            );
        }
    }

    #[test]
    fn threshold_getters() {
        let mut budget = CacheBudget::new(1000);
        assert_eq!(budget.miss_threshold(), DEFAULT_MISS_THRESHOLD);
        assert_eq!(budget.overflow_threshold(), DEFAULT_OVERFLOW_THRESHOLD);

        budget.set_miss_threshold(128);
        budget.set_overflow_threshold(256);
        assert_eq!(budget.miss_threshold(), 128);
        assert_eq!(budget.overflow_threshold(), 256);
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
//! `Sele4nSim` — a host simulation of the seLe4n capability backend (D2,
//! §36.6). It implements the same [`TopoBackingProvider`] seam as POSIX, so the
//! allocator core runs identically over either (W0-14b, the G-sim spine).
//!
//! **M0 scope.** Unlike the POSIX provider, the simulator models an *authorized
//! untyped pool*: every reservation is "retyped" out of a finite pool and the
//! accounting is enforced, so the simulator can report exhaustion the way the
//! real kernel does (`UntypedRegionExhausted`). This is the seed of the
//! capability accounting that plan 09 (W22-1/W22-3) grows against the real
//! `sele4n-abi`/`sele4n-types` (pinned per D8). The host store itself is the
//! global allocator; M1 swaps in the real ABI behind the `real-abi` feature.

use std::alloc::{alloc, dealloc, Layout};
use std::sync::Mutex;

use topo_core::{ArenaId, BackendError, Region, TopoBackingProvider};

/// A "frame" retyped from the untyped pool, with its host backing.
struct Frame {
    base: usize,
    layout: Layout,
    /// Bytes charged to the untyped pool for this frame (== `layout.size()`).
    charged: usize,
}

struct State {
    /// Bytes still available in the authorized untyped pool.
    pool_remaining: usize,
    frames: Vec<Frame>,
}

/// Host simulator of the seLe4n backing provider.
pub struct Sele4nSim {
    state: Mutex<State>,
    pool_total: usize,
}

impl Sele4nSim {
    /// Create a simulator with `pool_bytes` of authorized untyped memory.
    pub fn new(pool_bytes: usize) -> Self {
        Self {
            state: Mutex::new(State {
                pool_remaining: pool_bytes,
                frames: Vec::new(),
            }),
            pool_total: pool_bytes,
        }
    }

    /// Authorized untyped bytes still available.
    pub fn pool_remaining(&self) -> usize {
        self.state
            .lock()
            .expect("sim mutex poisoned")
            .pool_remaining
    }

    /// Total authorized untyped bytes.
    pub fn pool_total(&self) -> usize {
        self.pool_total
    }
}

impl TopoBackingProvider for Sele4nSim {
    fn reserve(&self, _arena: ArenaId, size: usize, align: usize) -> Result<Region, BackendError> {
        if size == 0 || !align.is_power_of_two() {
            return Err(BackendError::InvalidRequest);
        }
        let layout =
            Layout::from_size_align(size, align).map_err(|_| BackendError::InvalidRequest)?;
        let mut st = self.state.lock().expect("sim mutex poisoned");
        // Retype from the authorized untyped pool: refuse if it is exhausted
        // (the simulated `KernelError::UntypedRegionExhausted`). The allocator
        // must handle this exactly as it would the real kernel error (§36.6).
        if size > st.pool_remaining {
            return Err(BackendError::OutOfMemory);
        }
        // SAFETY: nonzero size + valid power-of-two alignment (checked above).
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(BackendError::OutOfMemory);
        }
        st.pool_remaining -= size;
        st.frames.push(Frame {
            base: base as usize,
            layout,
            charged: size,
        });
        Ok(Region { base, len: size })
    }

    fn commit(&self, _region: Region, _offset: usize, _len: usize) -> Result<(), BackendError> {
        Ok(())
    }

    fn release(&self, _arena: ArenaId, region: Region) -> Result<(), BackendError> {
        let mut st = self.state.lock().expect("sim mutex poisoned");
        let base = region.base as usize;
        let idx = st
            .frames
            .iter()
            .position(|fr| fr.base == base)
            .ok_or(BackendError::InvalidRequest)?;
        let fr = st.frames.swap_remove(idx);
        // Recycle the untyped back to the pool (revoke + recycle, §36.7).
        st.pool_remaining += fr.charged;
        // SAFETY: `fr.base`/`fr.layout` are exactly the pointer and layout from
        // the matching `alloc` in `reserve`; removed from the set so freed once.
        unsafe { dealloc(fr.base as *mut u8, fr.layout) };
        Ok(())
    }

    fn name(&self) -> &'static str {
        "sele4n-sim"
    }
}

impl Drop for Sele4nSim {
    fn drop(&mut self) {
        for fr in self
            .state
            .get_mut()
            .expect("sim mutex poisoned")
            .frames
            .drain(..)
        {
            // SAFETY: same invariant as `release`.
            unsafe { dealloc(fr.base as *mut u8, fr.layout) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retypes_within_pool_and_recycles() {
        let sim = Sele4nSim::new(8192);
        let r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        assert_eq!(sim.pool_remaining(), 4096);
        sim.release(ArenaId::DEFAULT, r).expect("release");
        assert_eq!(sim.pool_remaining(), sim.pool_total());
    }

    #[test]
    fn exhausting_the_untyped_pool_fails_cleanly() {
        let sim = Sele4nSim::new(4096);
        let _r = sim.reserve(ArenaId::DEFAULT, 4096, 16).expect("reserve");
        // Pool exhausted: the next retype fails like the real kernel, without
        // borrowing authority (§36.16 exhaustion behaviour).
        assert!(matches!(
            sim.reserve(ArenaId::DEFAULT, 1, 16),
            Err(BackendError::OutOfMemory)
        ));
        assert_eq!(sim.pool_remaining(), 0);
    }

    #[test]
    fn name_is_sele4n_sim() {
        assert_eq!(Sele4nSim::new(0).name(), "sele4n-sim");
    }
}

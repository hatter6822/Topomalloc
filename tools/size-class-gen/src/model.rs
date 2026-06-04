// SPDX-License-Identifier: MIT
//! Golden-table schema + validation. Validation is the proof obligation the
//! generator discharges before emitting any artifact: it rejects any table that
//! would violate the Section 9.3 / 9.5 invariants the allocator and the Lean
//! proofs rely on. A bad golden fails the build rather than shipping unsound
//! tables (Appendix F).

use serde::Deserialize;

/// One row of the golden JSON, exactly as authored / serialized.
#[derive(Debug, Clone, Deserialize)]
pub struct ClassRow {
    pub size: u32,
    pub align: u32,
    pub slab_pages: u32,
    pub objects_per_slab: u32,
    pub batch: u32,
    pub max_local_capacity: u32,
}

/// The golden file as deserialized. `_comment` and other unknown keys are
/// ignored so the golden can carry human notes.
#[derive(Debug, Clone, Deserialize)]
pub struct Golden {
    pub schema_version: u32,
    pub page_size: u32,
    pub quantum: u32,
    pub tiny_min: u32,
    pub small_max: u32,
    /// First request size (in bytes) served by the hugepage/region backend
    /// rather than the small slab or medium extent path (§9.2, §A.1, §18.5). A
    /// platform/D4 parameter equal to the hugepage size by default (Appendix C).
    pub huge_threshold: u32,
    pub classes: Vec<ClassRow>,
}

/// A validated, internally-consistent table plus the derived lookup. Only this
/// type may be emitted, so every emitted artifact is sound by construction.
pub struct Table {
    pub page_size: u32,
    pub quantum: u32,
    pub tiny_min: u32,
    pub small_max: u32,
    /// First size served by the hugepage/region backend (§9.2/§A.1). Authored in
    /// the golden; validated `> small_max` and a whole multiple of `page_size`.
    pub huge_threshold: u32,
    /// Largest natural alignment any class provides (`max` over the rows). Derived
    /// here — never authored — so it cannot drift from the table. The runtime uses
    /// it to reject, in O(1), an over-aligned request no small class can satisfy
    /// (it routes to medium/large instead of widening a shared slab, §9.3/§25.5).
    pub max_align: u32,
    pub classes: Vec<ClassRow>,
    /// `size_to_class[(size-1)/quantum]` = class index, for sizes in `1..=small_max`.
    pub size_to_class: Vec<u8>,
}

fn is_pow2(v: u32) -> bool {
    v != 0 && (v & (v - 1)) == 0
}

impl Table {
    /// Validate every invariant, then derive and re-check the size→class lookup.
    pub fn validate(g: Golden) -> Result<Table, String> {
        if g.schema_version != 1 {
            return Err(format!("unsupported schema_version {}", g.schema_version));
        }
        if g.page_size == 0 {
            return Err("page_size must be > 0".into());
        }
        if !is_pow2(g.quantum) {
            return Err(format!("quantum {} must be a power of two", g.quantum));
        }
        if g.tiny_min == 0 || g.small_max == 0 {
            return Err("tiny_min and small_max must be > 0".into());
        }
        if !g.small_max.is_multiple_of(g.quantum) {
            return Err(format!(
                "small_max {} must be a multiple of quantum {} (granule lookup soundness)",
                g.small_max, g.quantum
            ));
        }
        // The hugepage/region boundary (§9.2/§A.1/§18.5). It must lie strictly
        // above the small path so the medium range `(small_max, huge_threshold)`
        // is well-defined, and be a whole number of allocator pages so a medium
        // request that page-rounds can never reach into the large regime.
        if g.huge_threshold <= g.small_max {
            return Err(format!(
                "huge_threshold {} must exceed small_max {} (the medium range must be non-empty)",
                g.huge_threshold, g.small_max
            ));
        }
        if !g.huge_threshold.is_multiple_of(g.page_size) {
            return Err(format!(
                "huge_threshold {} must be a whole multiple of page_size {}",
                g.huge_threshold, g.page_size
            ));
        }
        if g.classes.is_empty() {
            return Err("table must have at least one class".into());
        }
        if g.classes.len() > 256 {
            return Err("table may have at most 256 classes (u8 class index)".into());
        }
        if g.classes[0].size != g.tiny_min {
            return Err(format!(
                "first class size {} must equal tiny_min {}",
                g.classes[0].size, g.tiny_min
            ));
        }

        let mut prev_size: u32 = 0;
        for (i, c) in g.classes.iter().enumerate() {
            let at = || format!("class[{i}] (size {})", c.size);
            if c.size == 0 || c.align == 0 {
                return Err(format!("{}: size and align must be > 0", at()));
            }
            if !is_pow2(c.align) {
                return Err(format!(
                    "{}: align {} must be a power of two",
                    at(),
                    c.align
                ));
            }
            // §9.3, the load-bearing constraint: every object at base0 + i*size is
            // aligned iff size is an integer multiple of align.
            if !c.size.is_multiple_of(c.align) {
                return Err(format!(
                    "{}: size {} is not a multiple of align {} (§9.3)",
                    at(),
                    c.size,
                    c.align
                ));
            }
            if c.align > c.size {
                return Err(format!("{}: align {} exceeds size", at(), c.align));
            }
            if !c.size.is_multiple_of(g.quantum) {
                return Err(format!(
                    "{}: size {} must be a multiple of quantum {}",
                    at(),
                    c.size,
                    g.quantum
                ));
            }
            if c.size < g.tiny_min {
                return Err(format!("{}: size below tiny_min {}", at(), g.tiny_min));
            }
            if c.slab_pages == 0 {
                return Err(format!("{}: slab_pages must be >= 1", at()));
            }
            if c.objects_per_slab == 0 {
                return Err(format!("{}: objects_per_slab must be >= 1", at()));
            }
            // Exact slab packing: objects_per_slab == floor(slab_bytes / size).
            // u64 throughout: a hand-edit that breaks the layout is caught here.
            let slab_bytes = c.slab_pages as u64 * g.page_size as u64;
            let expected = slab_bytes / c.size as u64;
            if expected != c.objects_per_slab as u64 {
                return Err(format!(
                    "{}: objects_per_slab {} != floor({} / {}) = {} (§9.5 slab layout)",
                    at(),
                    c.objects_per_slab,
                    slab_bytes,
                    c.size,
                    expected
                ));
            }
            // Objects must fit the slab without overflowing the span (§9.5).
            if c.objects_per_slab as u64 * c.size as u64 > slab_bytes {
                return Err(format!("{}: objects overflow the slab", at()));
            }
            if c.max_local_capacity == 0 {
                return Err(format!("{}: max_local_capacity must be >= 1", at()));
            }
            if c.batch == 0 || c.batch > c.max_local_capacity {
                return Err(format!(
                    "{}: batch {} must be in 1..=max_local_capacity {} (§9.5)",
                    at(),
                    c.batch,
                    c.max_local_capacity
                ));
            }
            // Strictly increasing sizes; and the §9.4 per-range spacing-ratio targets
            // (the policy bound the Lean model proves achievable). Below the q/W tiny
            // boundary the regime is alignment-dominated and the bound is the quantum
            // (ratio ≤ 2); above it the spacing ratio tightens by size range.
            if i > 0 {
                if c.size <= prev_size {
                    return Err(format!("{}: sizes must be strictly increasing", at()));
                }
                let (num, den, target): (u64, u64, &str) = if c.size <= 48 {
                    (2, 1, "≤ 2.0 (tiny, alignment-dominated)")
                } else if c.size <= 128 {
                    (4, 3, "≤ 1.33")
                } else if c.size <= 1024 {
                    (6, 5, "≤ 1.20")
                } else {
                    (9, 8, "≤ 1.125")
                };
                if c.size as u64 * den > num * prev_size as u64 {
                    return Err(format!(
                        "{}: spacing ratio {}/{} exceeds the §9.4 target {}",
                        at(),
                        c.size,
                        prev_size,
                        target
                    ));
                }
            }
            prev_size = c.size;
        }

        let last = g.classes.last().expect("non-empty checked above").size;
        if g.small_max != last {
            return Err(format!(
                "small_max {} must equal the largest class size {} (coverage)",
                g.small_max, last
            ));
        }

        // Derive the granule lookup and prove coverage + minimality exhaustively.
        let granules = (g.small_max / g.quantum) as usize;
        let mut size_to_class = Vec::with_capacity(granules);
        for gr in 0..granules {
            let rep = gr as u32 * g.quantum + 1; // smallest request in this granule
            let idx = g
                .classes
                .iter()
                .position(|c| c.size >= rep)
                .ok_or_else(|| format!("no class covers size {rep}"))?;
            size_to_class.push(idx as u8);
        }
        // Exhaustive: every request size maps to the *smallest* class that fits.
        for s in 1..=g.small_max {
            let gr = ((s - 1) / g.quantum) as usize;
            let ci = size_to_class[gr] as usize;
            let chosen = &g.classes[ci];
            if chosen.size < s {
                return Err(format!(
                    "lookup unsound: size {s} -> class {} too small",
                    chosen.size
                ));
            }
            if ci > 0 && g.classes[ci - 1].size >= s {
                return Err(format!(
                    "lookup non-minimal: size {s} should map to a smaller class"
                ));
            }
        }

        // Derived, never authored: the largest alignment any class provides. The
        // table is non-empty (checked above), so `max` is total.
        let max_align = g
            .classes
            .iter()
            .map(|c| c.align)
            .max()
            .expect("non-empty table checked above");

        Ok(Table {
            page_size: g.page_size,
            quantum: g.quantum,
            tiny_min: g.tiny_min,
            small_max: g.small_max,
            huge_threshold: g.huge_threshold,
            max_align,
            classes: g.classes,
            size_to_class,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Golden {
        Golden {
            schema_version: 1,
            page_size: 16384,
            quantum: 16,
            tiny_min: 16,
            small_max: 48,
            huge_threshold: 2097152,
            classes: vec![
                ClassRow {
                    size: 16,
                    align: 16,
                    slab_pages: 1,
                    objects_per_slab: 1024,
                    batch: 32,
                    max_local_capacity: 256,
                },
                ClassRow {
                    size: 32,
                    align: 16,
                    slab_pages: 1,
                    objects_per_slab: 512,
                    batch: 32,
                    max_local_capacity: 256,
                },
                ClassRow {
                    size: 48,
                    align: 16,
                    slab_pages: 1,
                    objects_per_slab: 341,
                    batch: 32,
                    max_local_capacity: 256,
                },
            ],
        }
    }

    #[test]
    fn accepts_good_table() {
        let t = Table::validate(good()).expect("valid");
        assert_eq!(t.size_to_class, vec![0, 1, 2]);
        // `max_align` is derived from the rows, not authored.
        assert_eq!(t.max_align, 16);
        assert_eq!(t.huge_threshold, 2097152);
    }

    #[test]
    fn rejects_huge_threshold_below_small_max() {
        let mut g = good();
        g.huge_threshold = 48; // == small_max, so the medium range would be empty
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_huge_threshold_not_page_multiple() {
        let mut g = good();
        g.huge_threshold = 2097152 + 1; // not a whole number of pages
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn derives_max_align_from_widest_class() {
        let mut g = good();
        // A genuinely over-aligned class (size 32 is a multiple of align 32) lifts
        // the derived MAX_ALIGN to 32 — proving it tracks the widest row.
        g.classes[1].align = 32;
        let t = Table::validate(g).expect("valid");
        assert_eq!(t.max_align, 32);
    }

    #[test]
    fn rejects_quantum_gap() {
        // A size that is a multiple of align but not of the quantum breaks the
        // direct-mapped granule lookup.
        let mut g = good();
        g.classes[1].size = 40; // 40 % 16 != 0
        g.classes[1].align = 8;
        g.classes[1].objects_per_slab = 16384 / 40;
        assert!(Table::validate(g).is_err());
    }

    #[test]
    fn rejects_unsorted() {
        let mut g = good();
        g.classes.swap(0, 1);
        assert!(Table::validate(g).is_err());
    }
}

// SPDX-License-Identifier: MIT
//! Allocation request flags and decoded placement hints (§10.4, plan 03 W2-3a).
//!
//! [`RequestFlags`] is the allocator's **internal** representation of the advisory
//! `flags` word threaded through [`classify`](crate::classify::classify) (§A.1).
//! The public C `TOPO_*` macro values (§10.4) are owned by the public API
//! (plan 06) and are mapped onto this internal layout at that boundary — nothing
//! here freezes the public ABI.
//!
//! **Alignment is never carried here.** §A.1's `classify(size, align, flags)`
//! takes alignment as a dedicated parameter, so the public wrapper extracts
//! `TOPO_ALIGN` into that argument *before* classification. That is what makes
//! §10.4's "mandatory flags such as alignment MUST NOT be silently ignored" rule
//! trivially hold: alignment is a first-class parameter, never an advisory bit a
//! decoder could drop.
//!
//! **Validation (§10.4).** A flag word is rejected ([`RequestFlags::from_raw`]
//! returns `None`, so `classify` returns `None` deterministically) when it sets a
//! reserved bit or a contradictory combination (`NO_HUGEPAGE` together with
//! `PREFER_HUGEPAGE`). Every other bit pattern decodes to a well-defined
//! [`Hints`] snapshot, so a `RequestFlags` value is *always* internally
//! consistent once constructed.
//!
//! Bit layout (`u32`):
//!
//! | Bits | Field | §10.4 source |
//! |---|---|---|
//! | 0 | `ZERO` | `TOPO_ZERO` |
//! | 1 | `CACHE_BYPASS` | `TOPO_TCACHE_NONE` |
//! | 2 | `GUARDED` | `TOPO_GUARDED` |
//! | 3 | `NO_HUGEPAGE` | `TOPO_NO_HUGEPAGE` |
//! | 4 | `PREFER_HUGEPAGE` | `TOPO_PREFER_HUGEPAGE` |
//! | 5–6 | lifetime (0 none / 1 short / 2 medium / 3 long) | `TOPO_LIFETIME_*` |
//! | 7–14 | hotness (`0..=255`; 0 = cold/neutral) | `TOPO_HOT` / `TOPO_COLD` |
//! | 15–22 | arena+1 (0 = default, `v>0` ⇒ arena `v-1`) | `TOPO_ARENA` |
//! | 23–31 | reserved (MUST be zero) | — |
//!
//! Per-call NUMA-node and explicit-tcache routing (`TOPO_NUMA`, `TOPO_TCACHE`)
//! are intentionally **not** encoded yet: they belong to the placement/cache
//! subsystems (plans 15/05) and would burn bits before those exist. They are
//! added — with their own fields or an out-of-band handle — when those land.

use crate::ids::ArenaId;

/// Expected lifetime hint (§10.4 `TOPO_LIFETIME_*`). Advisory; carried for the
/// placement/lifetime subsystems (plan 07), not acted on by the classifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Lifetime {
    /// No lifetime hint supplied.
    #[default]
    Unspecified,
    /// Expected short lifetime.
    Short,
    /// Expected medium lifetime.
    Medium,
    /// Expected long lifetime.
    Long,
}

/// Hugepage placement preference (§10.4). Modeled as a three-valued enum so the
/// contradictory "avoid *and* prefer" state is unrepresentable — the
/// corresponding flag-bit combination is rejected by [`RequestFlags::from_raw`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HugepagePolicy {
    /// No preference; the backend decides (plan 04).
    #[default]
    Default,
    /// Avoid hugepage-backed placement (`TOPO_NO_HUGEPAGE`).
    Avoid,
    /// Prefer hugepage-backed placement (`TOPO_PREFER_HUGEPAGE`).
    Prefer,
}

/// A decoded, structured snapshot of the advisory hints in a [`RequestFlags`]
/// (§10.4). The front/middle/back ends read this instead of re-parsing bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Hints {
    /// Zero the returned memory (`TOPO_ZERO`).
    pub zero: bool,
    /// Bypass the front-end cache for this request (`TOPO_TCACHE_NONE`).
    pub cache_bypass: bool,
    /// Sampled or forced guard allocation (`TOPO_GUARDED`).
    pub guarded: bool,
    /// Hugepage placement preference.
    pub hugepage: HugepagePolicy,
    /// Expected lifetime.
    pub lifetime: Lifetime,
    /// Hotness hint (`0..=255`; 0 = cold/neutral).
    pub hotness: u8,
}

/// The internal, validated representation of an allocation request's flag word.
///
/// A `RequestFlags` exists only if it passed [`RequestFlags::from_raw`], so it is
/// always free of reserved bits and contradictory combinations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RequestFlags(u32);

impl RequestFlags {
    /// No flags set — the default request (default arena, no hints).
    pub const NONE: RequestFlags = RequestFlags(0);

    /// Zero the returned memory (`TOPO_ZERO`).
    pub const ZERO: u32 = 1 << 0;
    /// Bypass the front-end cache (`TOPO_TCACHE_NONE`).
    pub const CACHE_BYPASS: u32 = 1 << 1;
    /// Sampled/forced guard allocation (`TOPO_GUARDED`).
    pub const GUARDED: u32 = 1 << 2;
    /// Avoid hugepage placement (`TOPO_NO_HUGEPAGE`).
    pub const NO_HUGEPAGE: u32 = 1 << 3;
    /// Prefer hugepage placement (`TOPO_PREFER_HUGEPAGE`).
    pub const PREFER_HUGEPAGE: u32 = 1 << 4;

    const LIFETIME_SHIFT: u32 = 5;
    const LIFETIME_MASK: u32 = 0b11 << Self::LIFETIME_SHIFT;
    const HOTNESS_SHIFT: u32 = 7;
    const HOTNESS_MASK: u32 = 0xFF << Self::HOTNESS_SHIFT;
    const ARENA_SHIFT: u32 = 15;
    const ARENA_MASK: u32 = 0xFF << Self::ARENA_SHIFT;
    /// Largest arena id encodable in the flag word (the field holds `id + 1`).
    pub const MAX_ARENA_ID: u32 = 254;

    /// Every bit the layout defines; the complement is reserved.
    const KNOWN_MASK: u32 = Self::ZERO
        | Self::CACHE_BYPASS
        | Self::GUARDED
        | Self::NO_HUGEPAGE
        | Self::PREFER_HUGEPAGE
        | Self::LIFETIME_MASK
        | Self::HOTNESS_MASK
        | Self::ARENA_MASK;
    /// Bits 23–31, which MUST be zero (§10.4 "invalid flag … MUST fail").
    const RESERVED_MASK: u32 = !Self::KNOWN_MASK;

    /// Validate and wrap a raw flag word (§10.4). Returns `None` — so `classify`
    /// fails deterministically — on any reserved bit or the contradictory
    /// `NO_HUGEPAGE | PREFER_HUGEPAGE` combination. Every other word is accepted.
    #[inline]
    pub const fn from_raw(raw: u32) -> Option<RequestFlags> {
        if raw & Self::RESERVED_MASK != 0 {
            return None;
        }
        if raw & Self::NO_HUGEPAGE != 0 && raw & Self::PREFER_HUGEPAGE != 0 {
            return None;
        }
        Some(RequestFlags(raw))
    }

    /// The raw flag word (for trace emission / round-tripping).
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// The arena this request routes to (§A.1 `choose_arena`): field 0 ⇒ the
    /// default arena, `v > 0` ⇒ `ArenaId(v - 1)`.
    #[inline]
    pub const fn arena(self) -> ArenaId {
        let v = (self.0 & Self::ARENA_MASK) >> Self::ARENA_SHIFT;
        if v == 0 {
            ArenaId::DEFAULT
        } else {
            ArenaId(v - 1)
        }
    }

    /// Decode the advisory hints into a structured [`Hints`] snapshot.
    #[inline]
    pub const fn hints(self) -> Hints {
        let hugepage = if self.0 & Self::NO_HUGEPAGE != 0 {
            HugepagePolicy::Avoid
        } else if self.0 & Self::PREFER_HUGEPAGE != 0 {
            HugepagePolicy::Prefer
        } else {
            HugepagePolicy::Default
        };
        let lifetime = match (self.0 & Self::LIFETIME_MASK) >> Self::LIFETIME_SHIFT {
            1 => Lifetime::Short,
            2 => Lifetime::Medium,
            3 => Lifetime::Long,
            _ => Lifetime::Unspecified,
        };
        Hints {
            zero: self.0 & Self::ZERO != 0,
            cache_bypass: self.0 & Self::CACHE_BYPASS != 0,
            guarded: self.0 & Self::GUARDED != 0,
            hugepage,
            lifetime,
            hotness: ((self.0 & Self::HOTNESS_MASK) >> Self::HOTNESS_SHIFT) as u8,
        }
    }

    // --- builders (the mapping target for plan 06's public TOPO_* macros) ---
    // Each sets exactly one field, so a value built through them is always valid
    // (no reserved bits, and the hugepage policy is single-valued by construction).

    /// Request zeroed memory (`TOPO_ZERO`).
    #[inline]
    pub const fn with_zero(self) -> RequestFlags {
        RequestFlags(self.0 | Self::ZERO)
    }

    /// Bypass the front-end cache for this request (`TOPO_TCACHE_NONE`).
    #[inline]
    pub const fn with_cache_bypass(self) -> RequestFlags {
        RequestFlags(self.0 | Self::CACHE_BYPASS)
    }

    /// Request a guard allocation (`TOPO_GUARDED`).
    #[inline]
    pub const fn with_guarded(self) -> RequestFlags {
        RequestFlags(self.0 | Self::GUARDED)
    }

    /// Set the hugepage policy (clears whichever hugepage bit was set first, so
    /// the avoid/prefer contradiction is impossible to build).
    #[inline]
    pub const fn with_hugepage(self, policy: HugepagePolicy) -> RequestFlags {
        let cleared = self.0 & !(Self::NO_HUGEPAGE | Self::PREFER_HUGEPAGE);
        let bit = match policy {
            HugepagePolicy::Default => 0,
            HugepagePolicy::Avoid => Self::NO_HUGEPAGE,
            HugepagePolicy::Prefer => Self::PREFER_HUGEPAGE,
        };
        RequestFlags(cleared | bit)
    }

    /// Set the lifetime hint.
    #[inline]
    pub const fn with_lifetime(self, lifetime: Lifetime) -> RequestFlags {
        let v: u32 = match lifetime {
            Lifetime::Unspecified => 0,
            Lifetime::Short => 1,
            Lifetime::Medium => 2,
            Lifetime::Long => 3,
        };
        RequestFlags((self.0 & !Self::LIFETIME_MASK) | (v << Self::LIFETIME_SHIFT))
    }

    /// Set the hotness hint (`0..=255`).
    #[inline]
    pub const fn with_hotness(self, hotness: u8) -> RequestFlags {
        RequestFlags((self.0 & !Self::HOTNESS_MASK) | ((hotness as u32) << Self::HOTNESS_SHIFT))
    }

    /// Route to an explicit arena. Returns `None` if `id` exceeds
    /// [`MAX_ARENA_ID`](Self::MAX_ARENA_ID) (larger ids use the explicit-arena
    /// handle of the public API, plan 06).
    #[inline]
    pub const fn with_arena(self, id: u32) -> Option<RequestFlags> {
        if id > Self::MAX_ARENA_ID {
            return None;
        }
        Some(RequestFlags(
            (self.0 & !Self::ARENA_MASK) | ((id + 1) << Self::ARENA_SHIFT),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_reserved_masks_partition_the_word() {
        // Every bit is either known or reserved, and never both (layout sanity).
        assert_eq!(
            RequestFlags::KNOWN_MASK | RequestFlags::RESERVED_MASK,
            u32::MAX
        );
        assert_eq!(RequestFlags::KNOWN_MASK & RequestFlags::RESERVED_MASK, 0);
    }

    #[test]
    fn none_decodes_to_defaults() {
        let h = RequestFlags::NONE.hints();
        assert_eq!(h, Hints::default());
        assert_eq!(RequestFlags::NONE.arena(), ArenaId::DEFAULT);
        assert_eq!(RequestFlags::NONE.raw(), 0);
    }

    #[test]
    fn reserved_bits_are_rejected() {
        // The lowest reserved bit (23) and the top bit must both fail.
        assert_eq!(RequestFlags::from_raw(1 << 23), None);
        assert_eq!(RequestFlags::from_raw(1 << 31), None);
        assert_eq!(RequestFlags::from_raw(u32::MAX), None);
    }

    #[test]
    fn contradictory_hugepage_flags_are_rejected() {
        let bad = RequestFlags::NO_HUGEPAGE | RequestFlags::PREFER_HUGEPAGE;
        assert_eq!(RequestFlags::from_raw(bad), None);
        // Either alone is fine.
        assert!(RequestFlags::from_raw(RequestFlags::NO_HUGEPAGE).is_some());
        assert!(RequestFlags::from_raw(RequestFlags::PREFER_HUGEPAGE).is_some());
    }

    #[test]
    fn boolean_flags_round_trip() {
        let f = RequestFlags::from_raw(
            RequestFlags::ZERO | RequestFlags::CACHE_BYPASS | RequestFlags::GUARDED,
        )
        .unwrap();
        let h = f.hints();
        assert!(h.zero && h.cache_bypass && h.guarded);
        assert_eq!(h.hugepage, HugepagePolicy::Default);
    }

    #[test]
    fn hugepage_policy_decodes() {
        assert_eq!(
            RequestFlags::NONE
                .with_hugepage(HugepagePolicy::Avoid)
                .hints()
                .hugepage,
            HugepagePolicy::Avoid
        );
        assert_eq!(
            RequestFlags::NONE
                .with_hugepage(HugepagePolicy::Prefer)
                .hints()
                .hugepage,
            HugepagePolicy::Prefer
        );
        // The builder is contradiction-proof: setting Prefer after Avoid wins cleanly.
        let f = RequestFlags::NONE
            .with_hugepage(HugepagePolicy::Avoid)
            .with_hugepage(HugepagePolicy::Prefer);
        assert!(RequestFlags::from_raw(f.raw()).is_some());
        assert_eq!(f.hints().hugepage, HugepagePolicy::Prefer);
    }

    #[test]
    fn lifetime_and_hotness_round_trip() {
        for lt in [
            Lifetime::Unspecified,
            Lifetime::Short,
            Lifetime::Medium,
            Lifetime::Long,
        ] {
            assert_eq!(RequestFlags::NONE.with_lifetime(lt).hints().lifetime, lt);
        }
        for h in [0u8, 1, 7, 128, 255] {
            assert_eq!(RequestFlags::NONE.with_hotness(h).hints().hotness, h);
        }
    }

    #[test]
    fn arena_encode_decode_round_trips_and_bounds() {
        assert_eq!(
            RequestFlags::with_arena(RequestFlags::NONE, 0)
                .unwrap()
                .arena(),
            ArenaId(0)
        );
        assert_eq!(
            RequestFlags::NONE.with_arena(5).unwrap().arena(),
            ArenaId(5)
        );
        assert_eq!(
            RequestFlags::NONE
                .with_arena(RequestFlags::MAX_ARENA_ID)
                .unwrap()
                .arena(),
            ArenaId(RequestFlags::MAX_ARENA_ID)
        );
        // Out of range → None (use the explicit-arena handle instead, plan 06).
        assert_eq!(
            RequestFlags::NONE.with_arena(RequestFlags::MAX_ARENA_ID + 1),
            None
        );
    }

    #[test]
    fn fields_are_independent() {
        // Setting every field at once decodes each back without cross-talk.
        let f = RequestFlags::NONE
            .with_zero()
            .with_guarded()
            .with_hugepage(HugepagePolicy::Prefer)
            .with_lifetime(Lifetime::Long)
            .with_hotness(200)
            .with_arena(42)
            .unwrap();
        let f = RequestFlags::from_raw(f.raw()).expect("built word is valid");
        let h = f.hints();
        assert!(h.zero && h.guarded && !h.cache_bypass);
        assert_eq!(h.hugepage, HugepagePolicy::Prefer);
        assert_eq!(h.lifetime, Lifetime::Long);
        assert_eq!(h.hotness, 200);
        assert_eq!(f.arena(), ArenaId(42));
    }
}

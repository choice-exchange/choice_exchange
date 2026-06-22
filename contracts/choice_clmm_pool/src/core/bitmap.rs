use crate::state::TICK_BITMAP;
use choice_clmm_math::bit_math::{least_significant_bit, most_significant_bit};
use choice_clmm_math::utils::{from_u256, to_u256, U256};
use cosmwasm_std::{StdResult, Storage};

/// Calculates the position in the bitmap for a given tick
fn position(tick: i32) -> (i16, u8) {
    let word_pos = (tick >> 8) as i16;
    let bit_pos = (tick & 0xff) as u8;
    (word_pos, bit_pos)
}

/// Flips the initialized state of a tick in the bitmap
pub fn flip_tick(storage: &mut dyn Storage, tick: i32, tick_spacing: i32) -> StdResult<()> {
    if tick % tick_spacing != 0 {
        return Err(cosmwasm_std::StdError::generic_err("Tick not spaced"));
    }

    let compressed = if tick < 0 && tick % tick_spacing != 0 {
        (tick / tick_spacing) - 1
    } else {
        tick / tick_spacing
    };

    let (word_pos, bit_pos) = position(compressed);

    // Cast to u32 for shift safety
    let mask = U256::one() << (bit_pos as u32);

    let word_cw = TICK_BITMAP.may_load(storage, word_pos)?.unwrap_or_default();
    let word = to_u256(word_cw);

    let next_word = word ^ mask;

    if next_word.is_zero() {
        TICK_BITMAP.remove(storage, word_pos);
    } else {
        TICK_BITMAP.save(storage, word_pos, &from_u256(next_word))?;
    }
    Ok(())
}

/// Finds the next initialized tick in the same word (or adjacent)
pub fn next_initialized_tick_in_chunk(
    storage: &dyn Storage,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> StdResult<(i32, bool)> {
    let compressed = if tick < 0 && tick % tick_spacing != 0 {
        (tick / tick_spacing) - 1
    } else {
        tick / tick_spacing
    };

    if lte {
        // Search Down (<= tick)
        let (word_pos, bit_pos) = position(compressed);

        // FIX: Handle overflow for bit_pos=255 and cast to u32
        // We want all bits <= bit_pos.
        // If bit_pos is 255, we want the full word (all ones).
        // Otherwise, we want (1 << (bit_pos + 1)) - 1
        let mask = if bit_pos == 255 {
            !U256::zero() // All ones
        } else {
            (U256::one() << (bit_pos as u32 + 1)) - 1
        };

        let word_cw = TICK_BITMAP.may_load(storage, word_pos)?.unwrap_or_default();
        let word = to_u256(word_cw);

        let masked = word & mask;

        if !masked.is_zero() {
            let msb = most_significant_bit(from_u256(masked))?;
            let next = (word_pos as i32 * 256 + msb as i32) * tick_spacing;
            Ok((next, true))
        } else {
            // Not found in this word
            let next = (word_pos as i32 * 256) * tick_spacing;
            Ok((next, false))
        }
    } else {
        // Search Up (> tick). Mirror Uniswap V3's `position(compressed + 1)`
        // formulation. Using `position(compressed)` with a "strictly above
        // bit_pos" mask is subtly wrong at word boundaries: the not-found case
        // would land on bit 0 of the *next* word, which the subsequent
        // iteration's "strictly above" mask then excludes — silently skipping
        // an initialized tick sitting exactly on a word boundary
        // (compressed tick ≡ 0 mod 256). Searching the word of `compressed + 1`
        // with a "bits >= bit_pos" mask keeps the not-found result inside that
        // word (last tick = ...+255), so the next iteration's full-word mask
        // covers bit 0.
        let (word_pos, bit_pos) = position(compressed + 1);

        // Mask: all bits at or above bit_pos. For bit_pos == 0,
        // (1 << 0) - 1 == 0, so the mask is all ones (search the whole word).
        let mask = !((U256::one() << (bit_pos as u32)) - U256::one());

        let word_cw = TICK_BITMAP.may_load(storage, word_pos)?.unwrap_or_default();
        let word = to_u256(word_cw);

        let masked = word & mask;

        if !masked.is_zero() {
            let lsb = least_significant_bit(from_u256(masked))?;
            // (word_pos * 256 + bit_pos) == compressed + 1, so this is
            // (word_pos * 256 + lsb) * tick_spacing.
            let next = ((compressed + 1) + (lsb as i32 - bit_pos as i32)) * tick_spacing;
            Ok((next, true))
        } else {
            // Last tick within this word: (word_pos * 256 + 255) * tick_spacing.
            let next = ((compressed + 1) + (255 - bit_pos as i32)) * tick_spacing;
            Ok((next, false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::MockStorage;

    /// Regression: an initialized tick sitting exactly on a word boundary
    /// (compressed tick ≡ 0 mod 256) must be discoverable by an upward search
    /// from the last tick of the previous word. The earlier
    /// `position(compressed)` implementation returned `(256, false)` here —
    /// reporting the boundary tick as uninitialized — so the swap loop stepped
    /// the price past tick 256 without ever applying its liquidity delta.
    ///
    /// `next_initialized_tick_in_chunk` searches only *within one word*, so a
    /// start deep in word 0 (e.g. tick 0) correctly returns the word-0 boundary
    /// `(255, false)`; the swap loop then re-enters from 255 and crosses into
    /// word 1. The bug lived in that second step, modelled here by `start = 255`.
    #[test]
    fn up_search_finds_word_boundary_tick() {
        let mut storage = MockStorage::new();
        let spacing = 1;

        // Initialize tick 256 = bit 0 of word 1.
        flip_tick(&mut storage, 256, spacing).unwrap();

        // From the last tick of word 0, the up-search reaches into word 1 and
        // must report tick 256 as INITIALIZED. (Old code: `(256, false)`.)
        assert_eq!(
            next_initialized_tick_in_chunk(&storage, 255, spacing, false).unwrap(),
            (256, true),
        );

        // From deeper in word 0, the search stays within word 0 and stops at its
        // last tick — it must NOT overshoot to 256. (Old code: `(256, false)`,
        // a full-word overshoot that then dropped bit 0 on the next iteration.)
        for start in [0, 100] {
            assert_eq!(
                next_initialized_tick_in_chunk(&storage, start, spacing, false).unwrap(),
                (255, false),
                "up-search from {start} should stay within word 0"
            );
        }
    }

    /// Searching up from the boundary tick itself looks strictly above it. With
    /// only tick 256 initialized, the result is "not found" and must stay within
    /// the word of `compressed + 1` (last tick 511), not overshoot.
    #[test]
    fn up_search_above_boundary_is_not_found_within_word() {
        let mut storage = MockStorage::new();
        let spacing = 1;
        flip_tick(&mut storage, 256, spacing).unwrap();

        let (next, initialized) =
            next_initialized_tick_in_chunk(&storage, 256, spacing, false).unwrap();
        assert_eq!((next, initialized), (511, false));
    }

    /// Upward search with non-unit spacing: `256 * spacing` is the word
    /// boundary in compressed space. Starting from the last tick of word 0
    /// (compressed 255 → tick `255 * spacing`) must find it initialized.
    #[test]
    fn up_search_word_boundary_with_spacing() {
        let mut storage = MockStorage::new();
        let spacing = 60;
        let boundary = 256 * spacing; // compressed 256, bit 0 of word 1

        flip_tick(&mut storage, boundary, spacing).unwrap();

        let (next, initialized) =
            next_initialized_tick_in_chunk(&storage, 255 * spacing, spacing, false).unwrap();
        assert_eq!((next, initialized), (boundary, true));
    }

    /// Downward search must continue to find a word-boundary tick (this branch
    /// was already correct; guard against regressions from the up-search fix).
    #[test]
    fn down_search_finds_word_boundary_tick() {
        let mut storage = MockStorage::new();
        let spacing = 1;

        // Initialize tick 256 (bit 0 of word 1). Search down from word 1.
        flip_tick(&mut storage, 256, spacing).unwrap();
        let (next, initialized) =
            next_initialized_tick_in_chunk(&storage, 300, spacing, true).unwrap();
        assert_eq!((next, initialized), (256, true));
    }

    /// A normal (non-boundary) initialized tick is still found in both
    /// directions, including a negative tick (both within their own word).
    #[test]
    fn finds_non_boundary_and_negative_ticks() {
        let mut storage = MockStorage::new();
        let spacing = 1;

        flip_tick(&mut storage, 50, spacing).unwrap();
        flip_tick(&mut storage, -100, spacing).unwrap();

        // Up from 0 finds 50 (same word).
        assert_eq!(
            next_initialized_tick_in_chunk(&storage, 0, spacing, false).unwrap(),
            (50, true)
        );
        // Down from -50 finds -100 (both in word -1).
        assert_eq!(
            next_initialized_tick_in_chunk(&storage, -50, spacing, true).unwrap(),
            (-100, true)
        );
    }
}

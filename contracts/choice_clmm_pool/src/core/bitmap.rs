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
        // Search Up (> tick)
        let (word_pos, bit_pos) = position(compressed);

        // Mask: all bits strictly above bit_pos
        let mask = if bit_pos == 255 {
            U256::zero()
        } else {
            // Use !0 (All ones) shifted left.
            // Zeros out the bottom (bit_pos + 1) bits.
            !U256::zero() << (bit_pos as u32 + 1)
        };

        let word_cw = TICK_BITMAP.may_load(storage, word_pos)?.unwrap_or_default();
        let word = to_u256(word_cw);

        let masked = word & mask;

        if !masked.is_zero() {
            let lsb = least_significant_bit(from_u256(masked))?;
            let next = (word_pos as i32 * 256 + lsb as i32) * tick_spacing;
            Ok((next, true))
        } else {
            // Return start of next word
            let next = ((word_pos as i32 + 1) * 256) * tick_spacing;
            Ok((next, false))
        }
    }
}

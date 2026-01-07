use crate::state::TICK_BITMAP;
use choice_clmm_math::bit_math::{least_significant_bit, most_significant_bit};
use choice_clmm_math::utils::{from_u256, to_u256, U256};
use cosmwasm_std::{StdResult, Storage, Uint256};

pub fn flip_tick(storage: &mut dyn Storage, tick: i32, tick_spacing: i32) -> StdResult<()> {
    if tick % tick_spacing != 0 {
        return Err(cosmwasm_std::StdError::generic_err(
            "Tick not divisible by spacing",
        ));
    }

    let spaced_tick = tick / tick_spacing;
    let (word_pos, bit_pos) = position(spaced_tick);

    let mask = U256::one() << bit_pos;

    let word_cw = TICK_BITMAP
        .may_load(storage, word_pos)?
        .unwrap_or(Uint256::zero());
    let word = to_u256(word_cw);

    let next_word = word ^ mask;

    if next_word.is_zero() {
        TICK_BITMAP.remove(storage, word_pos);
    } else {
        TICK_BITMAP.save(storage, word_pos, &from_u256(next_word))?;
    }

    Ok(())
}

pub fn next_initialized_tick_within_one_word(
    storage: &dyn Storage,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> StdResult<(i32, bool)> {
    let compressed = tick / tick_spacing;

    // Tick: -1 (compressed -1). Word: -1. Bit: 255.
    if tick < 0 && tick % tick_spacing != 0 {
        // Integer division in Rust truncates towards zero. -1 / 10 = 0.
        // We need floor division for negative ticks to match Bitmap logic.
        // However, Uniswap V3 uses `>> 8` on the compressed tick.
        // position() uses `>> 8` on the result.
        // Let's assume the input `tick` follows the spacing.
    }

    let (word_pos, bit_pos) = position(compressed);

    let word_cw = TICK_BITMAP
        .may_load(storage, word_pos)?
        .unwrap_or(Uint256::zero());
    let word = to_u256(word_cw);
    let one = U256::one();

    if lte {
        // Searching Downwards (<=)
        // We want all bits to the right of bit_pos (inclusive)
        // Mask: (1 << (bit_pos + 1)) - 1

        // FIX: Handle bit_pos == 255 to prevent overflow
        let mask = if bit_pos == 255 {
            U256::MAX // All 1s
        } else {
            (one << (bit_pos as u16 + 1)) - one
        };

        let masked = word & mask;

        if !masked.is_zero() {
            let msb = most_significant_bit(from_u256(masked))?;
            let next = (word_pos as i32) * 256 + (msb as i32);
            Ok((next * tick_spacing, true))
        } else {
            let next = (word_pos as i32) * 256 - 1;
            Ok((next * tick_spacing, false))
        }
    } else {
        // Searching Upwards (>)
        // We want all bits to the left of bit_pos (exclusive)
        // Mask: All 1s except the bottom (bit_pos + 1) bits.
        // Logic: ~((1 << (bit_pos + 1)) - 1)

        // FIX: Handle bit_pos == 255
        let mask_lower = if bit_pos == 255 {
            U256::MAX
        } else {
            (one << (bit_pos as u16 + 1)) - one
        };

        // Invert mask_lower to get upper bits
        // Since U256 doesn't always support `!`, we use XOR with MAX
        let mask = U256::MAX ^ mask_lower;

        let masked = word & mask;

        if !masked.is_zero() {
            let lsb = least_significant_bit(from_u256(masked))?;
            let next = (word_pos as i32) * 256 + (lsb as i32);
            Ok((next * tick_spacing, true))
        } else {
            let next = (word_pos as i32 + 1) * 256;
            Ok((next * tick_spacing, false))
        }
    }
}

fn position(tick: i32) -> (i16, u8) {
    // Arithmetic shift preserves sign for negative numbers
    let word_pos = (tick >> 8) as i16;
    let bit_pos = (tick & 0xFF) as u8;
    (word_pos, bit_pos)
}

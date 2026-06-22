use crate::utils::{to_u256, U256};
use cosmwasm_std::{StdError, StdResult, Uint256};

/// Returns the index of the most significant bit of the number
pub fn most_significant_bit(number: Uint256) -> StdResult<u8> {
    if number.is_zero() {
        return Err(StdError::generic_err("MSB of zero"));
    }

    let mut n = to_u256(number);
    let mut r: u8 = 0;

    let one = U256::one();

    if n >= (one << 128) {
        n >>= 128;
        r += 128;
    }
    if n >= (one << 64) {
        n >>= 64;
        r += 64;
    }
    if n >= (one << 32) {
        n >>= 32;
        r += 32;
    }
    if n >= (one << 16) {
        n >>= 16;
        r += 16;
    }
    if n >= (one << 8) {
        n >>= 8;
        r += 8;
    }
    if n >= (one << 4) {
        n >>= 4;
        r += 4;
    }
    if n >= (one << 2) {
        n >>= 2;
        r += 2;
    }
    if n >= (one << 1) {
        r += 1;
    }

    Ok(r)
}

/// Returns the index of the least significant bit of the number
pub fn least_significant_bit(number: Uint256) -> StdResult<u8> {
    if number.is_zero() {
        return Err(StdError::generic_err("LSB of zero"));
    }

    let mut n = to_u256(number);
    let mut r: u8 = 255;
    let one = U256::one();

    // Now we can use bitwise AND (&) freely on U256
    if (n & ((one << 128) - one)).is_zero() {
        n >>= 128;
    } else {
        r -= 128;
    }
    if (n & ((one << 64) - one)).is_zero() {
        n >>= 64;
    } else {
        r -= 64;
    }
    if (n & ((one << 32) - one)).is_zero() {
        n >>= 32;
    } else {
        r -= 32;
    }
    if (n & ((one << 16) - one)).is_zero() {
        n >>= 16;
    } else {
        r -= 16;
    }
    if (n & ((one << 8) - one)).is_zero() {
        n >>= 8;
    } else {
        r -= 8;
    }
    if (n & ((one << 4) - one)).is_zero() {
        n >>= 4;
    } else {
        r -= 4;
    }
    if (n & ((one << 2) - one)).is_zero() {
        n >>= 2;
    } else {
        r -= 2;
    }
    if (n & ((one << 1) - one)).is_zero() {
        n >>= 1;
    } else {
        r -= 1;
    }

    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(k: u32) -> Uint256 {
        Uint256::one() << k
    }

    #[test]
    fn zero_is_an_error_for_both() {
        assert!(most_significant_bit(Uint256::zero()).is_err());
        assert!(least_significant_bit(Uint256::zero()).is_err());
    }

    /// Exhaustive single-bit check across all 256 positions. For a value with
    /// exactly one set bit, MSB == LSB == that position. This exercises every
    /// decision level (128/64/32/16/8/4/2/1) of both binary searches in both
    /// directions, pinning the exact return value (kills operator/shift mutants).
    #[test]
    fn single_bit_msb_equals_lsb_equals_position() {
        for k in 0u32..256 {
            let n = bit(k);
            assert_eq!(most_significant_bit(n).unwrap(), k as u8, "MSB(1<<{k})");
            assert_eq!(least_significant_bit(n).unwrap(), k as u8, "LSB(1<<{k})");
        }
    }

    /// With a low bit and the top bit set, LSB tracks the low bit and MSB the
    /// top bit — proving LSB ignores higher bits and MSB ignores lower bits.
    #[test]
    fn lsb_ignores_high_bits_msb_ignores_low_bits() {
        for k in 0u32..255 {
            let n = bit(k) + bit(255); // disjoint bits, so + == bitwise-or
            assert_eq!(
                least_significant_bit(n).unwrap(),
                k as u8,
                "LSB low bit {k}"
            );
            assert_eq!(
                most_significant_bit(n).unwrap(),
                255u8,
                "MSB top bit, low {k}"
            );
        }
        for k in 1u32..256 {
            let n = bit(0) + bit(k);
            assert_eq!(least_significant_bit(n).unwrap(), 0u8, "LSB bit0, high {k}");
            assert_eq!(
                most_significant_bit(n).unwrap(),
                k as u8,
                "MSB high bit {k}"
            );
        }
    }

    /// Spot-check small literals and the all-ones value against hand values.
    #[test]
    fn known_small_values() {
        assert_eq!(most_significant_bit(Uint256::from(1u8)).unwrap(), 0);
        assert_eq!(most_significant_bit(Uint256::from(2u8)).unwrap(), 1);
        assert_eq!(most_significant_bit(Uint256::from(3u8)).unwrap(), 1);
        assert_eq!(most_significant_bit(Uint256::from(255u8)).unwrap(), 7);
        assert_eq!(most_significant_bit(Uint256::from(256u32)).unwrap(), 8);

        assert_eq!(least_significant_bit(Uint256::from(1u8)).unwrap(), 0);
        assert_eq!(least_significant_bit(Uint256::from(2u8)).unwrap(), 1);
        assert_eq!(least_significant_bit(Uint256::from(3u8)).unwrap(), 0);
        assert_eq!(least_significant_bit(Uint256::from(6u8)).unwrap(), 1);
        assert_eq!(least_significant_bit(Uint256::from(256u32)).unwrap(), 8);

        // MAX (all 256 bits set): MSB=255, LSB=0.
        assert_eq!(most_significant_bit(Uint256::MAX).unwrap(), 255);
        assert_eq!(least_significant_bit(Uint256::MAX).unwrap(), 0);
    }
}

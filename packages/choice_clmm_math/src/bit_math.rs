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

use std::convert::TryFrom;

use cosmwasm_std::{Uint256, Uint512};

/// Calculates (a * b) / denominator with full precision
/// Panics if denominator is 0 or if result overflows U256
pub fn mul_div(a: Uint256, b: Uint256, denominator: Uint256) -> Uint256 {
    let a_512 = Uint512::from(a);
    let b_512 = Uint512::from(b);
    let denom_512 = Uint512::from(denominator);

    let result = a_512
        .checked_mul(b_512)
        .expect("mul overflow")
        .checked_div(denom_512)
        .expect("div by zero");

    // Convert back to U256 (will panic if > U256::MAX)
    Uint256::try_from(result).expect("mul_div overflow u256")
}

/// Calculates (a * b) / denominator, rounding UP
pub fn mul_div_round_up(a: Uint256, b: Uint256, denominator: Uint256) -> Uint256 {
    let a_512 = Uint512::from(a);
    let b_512 = Uint512::from(b);
    let denom_512 = Uint512::from(denominator);

    let product = a_512.checked_mul(b_512).expect("mul overflow");

    // Logic: (product + denominator - 1) / denominator
    if product.is_zero() {
        return Uint256::zero();
    }

    let numerator = product + denom_512 - Uint512::from(1u64);
    let result = numerator.checked_div(denom_512).expect("div by zero");

    Uint256::try_from(result).expect("mul_div_up overflow u256")
}

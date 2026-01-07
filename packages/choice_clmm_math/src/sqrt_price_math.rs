use crate::full_math::{mul_div, mul_div_round_up};
use cosmwasm_std::Uint256;

// Q96 constant used for bit shifting/fixed point
const Q96: Uint256 = Uint256::from_u128(1u128 << 96); // 2^96

/// Gets amount0 delta (Asset X)
/// formula: liquidity * (sqrt_ratio_b - sqrt_ratio_a) / (sqrt_ratio_b * sqrt_ratio_a)
/// for perfect precision: liquidity << 96 * (sqrt_ratio_b - sqrt_ratio_a) / sqrt_ratio_b / sqrt_ratio_a
pub fn get_amount0_delta(
    sqrt_ratio_a: Uint256,
    sqrt_ratio_b: Uint256,
    liquidity: u128,
    round_up: bool,
) -> Uint256 {
    let (sqrt_ratio_lower, sqrt_ratio_upper) = if sqrt_ratio_a < sqrt_ratio_b {
        (sqrt_ratio_a, sqrt_ratio_b)
    } else {
        (sqrt_ratio_b, sqrt_ratio_a)
    };

    let numerator1 = Uint256::from(liquidity) << 96;
    let numerator2 = sqrt_ratio_upper - sqrt_ratio_lower;

    // We do the division in steps to avoid massive 1024-bit numbers
    // L * (Pb - Pa) / Pb / Pa
    if round_up {
        let tmp = mul_div_round_up(numerator1, numerator2, sqrt_ratio_upper);
        mul_div_round_up(tmp, Uint256::one(), sqrt_ratio_lower)
    } else {
        let tmp = mul_div(numerator1, numerator2, sqrt_ratio_upper);
        mul_div(tmp, Uint256::one(), sqrt_ratio_lower)
    }
}

/// Gets amount1 delta (Asset Y)
/// formula: liquidity * (sqrt_ratio_b - sqrt_ratio_a)
pub fn get_amount1_delta(
    sqrt_ratio_a: Uint256,
    sqrt_ratio_b: Uint256,
    liquidity: u128,
    round_up: bool,
) -> Uint256 {
    let (sqrt_ratio_lower, sqrt_ratio_upper) = if sqrt_ratio_a < sqrt_ratio_b {
        (sqrt_ratio_a, sqrt_ratio_b)
    } else {
        (sqrt_ratio_b, sqrt_ratio_a)
    };

    let diff = sqrt_ratio_upper - sqrt_ratio_lower;

    if round_up {
        mul_div_round_up(Uint256::from(liquidity), diff, Q96)
    } else {
        mul_div(Uint256::from(liquidity), diff, Q96)
    }
}

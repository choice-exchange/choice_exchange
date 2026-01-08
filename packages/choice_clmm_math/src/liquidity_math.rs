use crate::full_math::mul_div;
use cosmwasm_std::{StdError, StdResult, Uint128, Uint256};
use std::cmp::min;
use std::convert::TryFrom;

const Q96: Uint256 = Uint256::from_u128(1u128 << 96);

/// Calculates Liquidity (L) given a range and an amount of Token0.
/// Formula: L = amount0 * (sqrt_upper * sqrt_lower) / (sqrt_upper - sqrt_lower)
pub fn get_liquidity_for_amount0(
    sqrt_ratio_ax96: Uint256,
    sqrt_ratio_bx96: Uint256,
    amount0: Uint256,
) -> StdResult<Uint128> {
    let (lower, upper) = if sqrt_ratio_ax96 < sqrt_ratio_bx96 {
        (sqrt_ratio_ax96, sqrt_ratio_bx96)
    } else {
        (sqrt_ratio_bx96, sqrt_ratio_ax96)
    };

    // intermediate = (lower * upper) / 2^96
    let intermediate = mul_div(lower, upper, Q96);

    // result = amount0 * intermediate / (upper - lower)
    let result = mul_div(amount0, intermediate, upper - lower);

    Uint128::try_from(result).map_err(|_| StdError::generic_err("Liquidity overflow"))
}

/// Calculates Liquidity (L) given a range and an amount of Token1.
/// Formula: L = amount1 / (sqrt_upper - sqrt_lower)
/// In Q96: L = amount1 * 2^96 / (sqrt_upper - sqrt_lower)
pub fn get_liquidity_for_amount1(
    sqrt_ratio_ax96: Uint256,
    sqrt_ratio_bx96: Uint256,
    amount1: Uint256,
) -> StdResult<Uint128> {
    let (lower, upper) = if sqrt_ratio_ax96 < sqrt_ratio_bx96 {
        (sqrt_ratio_ax96, sqrt_ratio_bx96)
    } else {
        (sqrt_ratio_bx96, sqrt_ratio_ax96)
    };

    let result = mul_div(amount1, Q96, upper - lower);

    Uint128::try_from(result).map_err(|_| StdError::generic_err("Liquidity overflow"))
}

/// Computes the maximum liquidity (L) that can be minted given the desired amounts of Token0 and Token1.
///
/// Logic:
/// 1. If Current Price P < Lower Tick:
///    The entire range is "above" the current price. We only need Asset 0 (X).
/// 2. If Current Price P > Upper Tick:
///    The entire range is "below" the current price. We only need Asset 1 (Y).
/// 3. If Lower < P < Upper (In Range):
///    We need both assets. The limiting factor determines L.
///    L = min(L_from_amount0, L_from_amount1)
pub fn get_liquidity_for_amounts(
    sqrt_ratio_current_x96: Uint256,
    sqrt_ratio_ax96: Uint256,
    sqrt_ratio_bx96: Uint256,
    amount0: Uint128,
    amount1: Uint128,
) -> StdResult<Uint128> {
    let (sqrt_ratio_lower, sqrt_ratio_upper) = if sqrt_ratio_ax96 < sqrt_ratio_bx96 {
        (sqrt_ratio_ax96, sqrt_ratio_bx96)
    } else {
        (sqrt_ratio_bx96, sqrt_ratio_ax96)
    };

    let amount0_u256 = Uint256::from(amount0);
    let amount1_u256 = Uint256::from(amount1);

    if sqrt_ratio_current_x96 <= sqrt_ratio_lower {
        // Range is entirely to the right (higher price). We act as if we are buying X.
        get_liquidity_for_amount0(sqrt_ratio_lower, sqrt_ratio_upper, amount0_u256)
    } else if sqrt_ratio_current_x96 >= sqrt_ratio_upper {
        // Range is entirely to the left (lower price). We act as if we are selling X (holding Y).
        get_liquidity_for_amount1(sqrt_ratio_lower, sqrt_ratio_upper, amount1_u256)
    } else {
        // In Range: We need both tokens.
        // 1. Calculate L based on amount0 covering [current, upper]
        let liquidity0 =
            get_liquidity_for_amount0(sqrt_ratio_current_x96, sqrt_ratio_upper, amount0_u256)?;

        // 2. Calculate L based on amount1 covering [lower, current]
        let liquidity1 =
            get_liquidity_for_amount1(sqrt_ratio_lower, sqrt_ratio_current_x96, amount1_u256)?;

        // 3. Return the constrained liquidity
        Ok(min(liquidity0, liquidity1))
    }
}

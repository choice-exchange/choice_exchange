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

    if upper == lower {
        return Err(StdError::generic_err(
            "get_liquidity_for_amount0: lower and upper sqrt prices are equal",
        ));
    }

    // intermediate = (lower * upper) / 2^96
    let intermediate = mul_div(lower, upper, Q96)?;

    // result = amount0 * intermediate / (upper - lower)
    // Round DOWN: we credit the LP with LESS liquidity than amount0 might
    // suggest, matching V3's `getLiquidityForAmount0`. LPs can never be
    // credited more L than the pool can back out of their deposited tokens.
    let result = mul_div(amount0, intermediate, upper - lower)?;

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

    if upper == lower {
        return Err(StdError::generic_err(
            "get_liquidity_for_amount1: lower and upper sqrt prices are equal",
        ));
    }

    // Round DOWN: same reasoning as get_liquidity_for_amount0.
    let result = mul_div(amount1, Q96, upper - lower)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqrt_price_math::{get_amount0_delta, get_amount1_delta};

    fn q96() -> Uint256 {
        Uint256::from(1u128) << 96
    }

    // Reference values are derived from the closed-form formulas at
    // power-of-two sqrt prices, where every division is exact (no rounding),
    // so the expected L is independent of the implementation's mul_div.

    #[test]
    fn liquidity_for_amount1_reference() {
        // L = amount1 * Q96 / (upper - lower); with upper-lower == Q96 => L == amount1.
        let l = get_liquidity_for_amount1(
            q96(),
            q96() * Uint256::from(2u8),
            Uint256::from(1_000_000u128),
        )
        .unwrap();
        assert_eq!(l, Uint128::from(1_000_000u128));
    }

    #[test]
    fn liquidity_for_amount0_reference() {
        // intermediate = lower*upper/Q96 = Q96*2Q96/Q96 = 2Q96;
        // L = amount0 * 2Q96 / (2Q96 - Q96) = 2 * amount0.
        let l = get_liquidity_for_amount0(
            q96(),
            q96() * Uint256::from(2u8),
            Uint256::from(1_000_000u128),
        )
        .unwrap();
        assert_eq!(l, Uint128::from(2_000_000u128));
    }

    #[test]
    fn liquidity_for_amount0_is_order_independent() {
        let a = get_liquidity_for_amount0(q96(), q96() * Uint256::from(2u8), Uint256::from(7u128))
            .unwrap();
        let b = get_liquidity_for_amount0(q96() * Uint256::from(2u8), q96(), Uint256::from(7u128))
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn amounts_below_range_uses_token0_only() {
        // current < lower => entire range above price => amount0 path => 2*amount0.
        let l = get_liquidity_for_amounts(
            q96() / Uint256::from(2u8),
            q96(),
            q96() * Uint256::from(2u8),
            Uint128::from(1_000_000u128),
            Uint128::from(999u128), // amount1 ignored in this regime
        )
        .unwrap();
        assert_eq!(l, Uint128::from(2_000_000u128));
    }

    #[test]
    fn amounts_above_range_uses_token1_only() {
        // current > upper => entire range below price => amount1 path => amount1.
        let l = get_liquidity_for_amounts(
            q96() * Uint256::from(4u8),
            q96(),
            q96() * Uint256::from(2u8),
            Uint128::from(999u128), // amount0 ignored in this regime
            Uint128::from(1_000_000u128),
        )
        .unwrap();
        assert_eq!(l, Uint128::from(1_000_000u128));
    }

    #[test]
    fn amounts_in_range_takes_the_limiting_side() {
        // lower=Q96, current=2Q96, upper=4Q96.
        // L0 over [current,upper] = 4*amount0 ; L1 over [lower,current] = amount1.
        let lower = q96();
        let current = q96() * Uint256::from(2u8);
        let upper = q96() * Uint256::from(4u8);

        // token1-limited: min(4*1000, 1000) = 1000
        let l = get_liquidity_for_amounts(
            current,
            lower,
            upper,
            Uint128::from(1000u128),
            Uint128::from(1000u128),
        )
        .unwrap();
        assert_eq!(l, Uint128::from(1000u128));

        // token0-limited: min(4*1000, 10000) = 4000
        let l = get_liquidity_for_amounts(
            current,
            lower,
            upper,
            Uint128::from(1000u128),
            Uint128::from(10_000u128),
        )
        .unwrap();
        assert_eq!(l, Uint128::from(4000u128));
    }

    #[test]
    fn equal_bounds_is_an_error() {
        assert!(get_liquidity_for_amount0(q96(), q96(), Uint256::from(1u128)).is_err());
        assert!(get_liquidity_for_amount1(q96(), q96(), Uint256::from(1u128)).is_err());
    }

    /// SOLVENCY: crediting L from a deposit, then valuing L back out with the
    /// burn rounding (round DOWN), must never return more than was deposited —
    /// even when the division is inexact. This is the core fund-safety property
    /// of the mint-sizing path.
    #[test]
    fn mint_then_burn_never_exceeds_deposit_with_rounding() {
        // upper-lower = 3*Q96, deposit not divisible by 3 -> inexact L.
        let lower = q96();
        let upper = q96() * Uint256::from(4u8);
        let deposit1 = Uint256::from(1_000_001u128);
        let l1 = get_liquidity_for_amount1(lower, upper, deposit1).unwrap();
        assert_eq!(l1, Uint128::from(333_333u128)); // floor(1_000_001 / 3)
        let back1 = get_amount1_delta(lower, upper, l1.u128(), false).unwrap();
        assert!(back1 <= deposit1, "token1 burn-down <= deposit");

        // token0 side, inexact.
        let upper0 = q96() * Uint256::from(3u8);
        let deposit0 = Uint256::from(1001u128);
        let l0 = get_liquidity_for_amount0(lower, upper0, deposit0).unwrap();
        let back0 = get_amount0_delta(lower, upper0, l0.u128(), false).unwrap();
        assert!(back0 <= deposit0, "token0 burn-down <= deposit");
    }
}

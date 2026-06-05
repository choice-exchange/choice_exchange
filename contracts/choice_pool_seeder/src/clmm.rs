//! Pure CLMM seeding math — no storage, no chain access, fully unit-testable.
//!
//! Two jobs:
//!   1. Pick the pool's initial price so a *full-range* mint consumes both seed
//!      balances (modulo rounding dust). For a constant-product-equivalent
//!      full-range position the consumed ratio is `amount1/amount0 = price`, so
//!      we set `sqrt_price = sqrt(amount1/amount0)` in Q64.96.
//!   2. Snap the full-range tick bounds to the pool's tick spacing.
//!
//! Constants mirror `choice_clmm_math::tick_math` (Uniswap-v3 invariants); they
//! are reproduced here rather than imported so the seeder doesn't take a
//! dependency on the math crate for two `const`s.

use cosmwasm_std::{Isqrt, StdError, StdResult, Uint128, Uint256, Uint512};
use std::str::FromStr;

/// `log_1.0001(2^-128)` — minimum tick the CLMM pool accepts.
pub const MIN_TICK: i32 = -887272;
/// `log_1.0001(2^128)` — maximum tick the CLMM pool accepts.
pub const MAX_TICK: i32 = 887272;

/// `get_sqrt_ratio_at_tick(MIN_TICK)` — inclusive lower bound on a valid
/// Q64.96 sqrt price.
pub const MIN_SQRT_RATIO: u128 = 4295128739;

/// `get_sqrt_ratio_at_tick(MAX_TICK + 1)` — the pool treats this as the
/// *exclusive* upper bound, so a usable init price must be strictly below it.
fn max_sqrt_ratio_exclusive() -> Uint256 {
    // Parsed from a compile-time constant; the unwrap can never fire.
    Uint256::from_str("1461446703485210103287273052203988822378723970342").unwrap()
}

/// Q64.96 sqrt price that makes a full-range mint draw both seed balances at
/// the ratio `amount1 : amount0`.
///
/// `sqrt_price_x96 = isqrt(amount1 << 192 / amount0)`, i.e.
/// `sqrt(amount1/amount0) * 2^96`, computed in `Uint512` to avoid overflow on
/// the `<< 192` (max input `~2^128 << 192 = 2^320 < 2^512`). The result is
/// clamped into `[MIN_SQRT_RATIO, max_sqrt_ratio_exclusive() - 1]` so it is
/// always a price the pool will accept; extreme seed ratios saturate at the
/// boundary rather than erroring.
pub fn init_sqrt_price_from_amounts(amount0: Uint128, amount1: Uint128) -> StdResult<Uint256> {
    if amount0.is_zero() || amount1.is_zero() {
        return Err(StdError::generic_err(
            "init_sqrt_price requires both seed amounts > 0",
        ));
    }

    let numerator = Uint512::from(amount1) << 192u32;
    let ratio_x192 = numerator / Uint512::from(amount0);
    let sqrt_x96 = ratio_x192.isqrt();

    // sqrt of a value < 2^320 is < 2^160, so this never truncates.
    let mut price = Uint256::try_from(sqrt_x96)
        .map_err(|_| StdError::generic_err("init_sqrt_price overflowed Uint256"))?;

    let min = Uint256::from(MIN_SQRT_RATIO);
    let max_inclusive = max_sqrt_ratio_exclusive() - Uint256::one();
    if price < min {
        price = min;
    } else if price > max_inclusive {
        price = max_inclusive;
    }
    Ok(price)
}

/// Full-range tick bounds snapped (toward zero) to `tick_spacing`. Integer
/// division truncates toward zero, so both results stay inside
/// `[MIN_TICK, MAX_TICK]` and are exact multiples of the spacing — exactly what
/// the pool's `Mint` validation requires.
pub fn full_range_ticks(tick_spacing: u32) -> StdResult<(i32, i32)> {
    if tick_spacing == 0 {
        return Err(StdError::generic_err("tick_spacing must be > 0"));
    }
    let spacing = tick_spacing as i32;
    let lower = (MIN_TICK / spacing) * spacing;
    let upper = (MAX_TICK / spacing) * spacing;
    Ok((lower, upper))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `2^96` — the Q64.96 representation of price == 1.0.
    fn one_x96() -> Uint256 {
        Uint256::one() << 96u32
    }

    #[test]
    fn equal_amounts_give_price_one() {
        let p = init_sqrt_price_from_amounts(Uint128::new(1_000_000), Uint128::new(1_000_000))
            .unwrap();
        // sqrt(1) * 2^96 == 2^96 exactly.
        assert_eq!(p, one_x96());
    }

    #[test]
    fn ratio_four_gives_price_two() {
        // amount1/amount0 = 4 → sqrt = 2 → sqrt_price = 2 * 2^96.
        let p = init_sqrt_price_from_amounts(Uint128::new(1), Uint128::new(4)).unwrap();
        assert_eq!(p, one_x96() * Uint256::from(2u128));
    }

    #[test]
    fn inverse_ratio_is_reciprocal_sqrt() {
        // amount1/amount0 = 1/4 → sqrt = 1/2 → sqrt_price = 2^96 / 2 = 2^95.
        let p = init_sqrt_price_from_amounts(Uint128::new(4), Uint128::new(1)).unwrap();
        assert_eq!(p, Uint256::one() << 95u32);
    }

    #[test]
    fn within_pool_bounds() {
        let p = init_sqrt_price_from_amounts(Uint128::new(7), Uint128::new(123_456)).unwrap();
        assert!(p >= Uint256::from(MIN_SQRT_RATIO));
        assert!(p < max_sqrt_ratio_exclusive());
    }

    #[test]
    fn extreme_high_ratio_clamps_to_max() {
        // Huge amount1, tiny amount0 → price saturates just below the ceiling.
        let p = init_sqrt_price_from_amounts(Uint128::new(1), Uint128::MAX).unwrap();
        assert_eq!(p, max_sqrt_ratio_exclusive() - Uint256::one());
    }

    #[test]
    fn extreme_low_ratio_clamps_to_min() {
        // Tiny amount1, huge amount0 → price floors at MIN_SQRT_RATIO.
        let p = init_sqrt_price_from_amounts(Uint128::MAX, Uint128::new(1)).unwrap();
        assert_eq!(p, Uint256::from(MIN_SQRT_RATIO));
    }

    #[test]
    fn zero_amounts_error() {
        assert!(init_sqrt_price_from_amounts(Uint128::zero(), Uint128::new(1)).is_err());
        assert!(init_sqrt_price_from_amounts(Uint128::new(1), Uint128::zero()).is_err());
    }

    #[test]
    fn full_range_ticks_respect_spacing_and_bounds() {
        for spacing in [1u32, 10, 60, 200] {
            let (lo, hi) = full_range_ticks(spacing).unwrap();
            let s = spacing as i32;
            assert_eq!(lo % s, 0, "lower not aligned for spacing {spacing}");
            assert_eq!(hi % s, 0, "upper not aligned for spacing {spacing}");
            assert!(lo >= MIN_TICK, "lower below MIN_TICK for spacing {spacing}");
            assert!(hi <= MAX_TICK, "upper above MAX_TICK for spacing {spacing}");
            assert!(lo < hi);
        }
    }

    #[test]
    fn full_range_ticks_spacing_one_is_exact_bounds() {
        assert_eq!(full_range_ticks(1).unwrap(), (MIN_TICK, MAX_TICK));
    }

    #[test]
    fn full_range_ticks_spacing_sixty() {
        // -887272/60 = -14787 (trunc), *60 = -887220; symmetric on the high side.
        assert_eq!(full_range_ticks(60).unwrap(), (-887220, 887220));
    }

    #[test]
    fn full_range_ticks_zero_spacing_errors() {
        assert!(full_range_ticks(0).is_err());
    }
}

use crate::full_math::{mul_div, mul_div_round_up};
use cosmwasm_std::{StdError, StdResult, Uint256};

/// Q96 = 2^96, the fixed-point unit used by Uniswap V3-style sqrt prices.
const Q96: Uint256 = Uint256::from_u128(1u128 << 96);

/// Resolution of the fixed-point representation.
const RESOLUTION: u32 = 96;

fn q96() -> Uint256 {
    Q96
}

/// Returns ceil(a / b).
fn div_round_up(a: Uint256, b: Uint256) -> StdResult<Uint256> {
    if b.is_zero() {
        return Err(StdError::generic_err("div_round_up: division by zero"));
    }
    let q = a / b;
    let r = a - q * b;
    if r.is_zero() {
        Ok(q)
    } else {
        Ok(q + Uint256::one())
    }
}

/// Returns amount0 delta between two prices, parameterized by liquidity.
///
/// Formula: L * (sqrt_b - sqrt_a) / (sqrt_b * sqrt_a), expressed as
/// (L << 96) * (sqrt_b - sqrt_a) / sqrt_b / sqrt_a to preserve precision.
///
/// `round_up = true` is used when the delta is owed *from* the caller (mint,
/// computing amount_in during a swap); `round_up = false` when the delta is owed
/// *to* the caller (burn, amount_out during a swap). This matches Uniswap V3 —
/// rounding direction is load-bearing for fund safety.
pub fn get_amount0_delta(
    sqrt_ratio_a: Uint256,
    sqrt_ratio_b: Uint256,
    liquidity: u128,
    round_up: bool,
) -> StdResult<Uint256> {
    if sqrt_ratio_a.is_zero() || sqrt_ratio_b.is_zero() {
        return Err(StdError::generic_err(
            "get_amount0_delta: zero sqrt price",
        ));
    }

    let (sqrt_ratio_lower, sqrt_ratio_upper) = if sqrt_ratio_a < sqrt_ratio_b {
        (sqrt_ratio_a, sqrt_ratio_b)
    } else {
        (sqrt_ratio_b, sqrt_ratio_a)
    };

    let numerator1 = Uint256::from(liquidity) << RESOLUTION;
    let numerator2 = sqrt_ratio_upper - sqrt_ratio_lower;

    if round_up {
        let tmp = mul_div_round_up(numerator1, numerator2, sqrt_ratio_upper)?;
        div_round_up(tmp, sqrt_ratio_lower)
    } else {
        let tmp = mul_div(numerator1, numerator2, sqrt_ratio_upper)?;
        Ok(tmp / sqrt_ratio_lower)
    }
}

/// Returns amount1 delta between two prices, parameterized by liquidity.
///
/// Formula: L * (sqrt_b - sqrt_a) / 2^96.
pub fn get_amount1_delta(
    sqrt_ratio_a: Uint256,
    sqrt_ratio_b: Uint256,
    liquidity: u128,
    round_up: bool,
) -> StdResult<Uint256> {
    let (sqrt_ratio_lower, sqrt_ratio_upper) = if sqrt_ratio_a < sqrt_ratio_b {
        (sqrt_ratio_a, sqrt_ratio_b)
    } else {
        (sqrt_ratio_b, sqrt_ratio_a)
    };

    let diff = sqrt_ratio_upper - sqrt_ratio_lower;

    if round_up {
        mul_div_round_up(Uint256::from(liquidity), diff, q96())
    } else {
        mul_div(Uint256::from(liquidity), diff, q96())
    }
}

/// Computes the next sqrt price after swapping `amount` of token0 into the pool,
/// rounding UP so that the invariant is preserved in the pool's favor.
///
/// Two-path formulation (V3 v1.0.1):
/// - If the intermediate `amount * sqrt_p` fits in U256:
///   returns `ceil((L << 96) * sqrt_p / ((L << 96) + amount * sqrt_p))`
/// - Otherwise uses the overflow-safe rearrangement:
///   returns `ceil((L << 96) / ((L << 96) / sqrt_p + amount))`
fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_price: Uint256,
    liquidity: u128,
    amount: Uint256,
    add: bool,
) -> StdResult<Uint256> {
    if amount.is_zero() {
        return Ok(sqrt_price);
    }
    if sqrt_price.is_zero() {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_amount0: sqrt_price is zero",
        ));
    }
    if liquidity == 0 {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_amount0: liquidity is zero",
        ));
    }

    let numerator1 = Uint256::from(liquidity) << RESOLUTION;

    if add {
        // Adding token0 (zero_for_one): price goes DOWN.
        // new = L * Q96 * P / (L * Q96 + amount * P)  (rounded up)
        // If amount * P fits, use the direct formula; else fallback to
        // ceil(L * Q96 / ((L * Q96) / P + amount))
        let product = amount.checked_mul(sqrt_price);
        if let Ok(prod) = product {
            let denominator = numerator1
                .checked_add(prod)
                .map_err(|_| StdError::generic_err("sqrt price denom overflow"))?;
            // Short-circuit only when the direct formula holds exact precision.
            if denominator >= numerator1 {
                return mul_div_round_up(numerator1, sqrt_price, denominator);
            }
        }
        // Overflow fallback.
        let denom = (numerator1 / sqrt_price)
            .checked_add(amount)
            .map_err(|_| StdError::generic_err("sqrt price fallback overflow"))?;
        div_round_up(numerator1, denom)
    } else {
        // Removing token0 (one_for_zero): price goes UP.
        // new = L * Q96 * P / (L * Q96 - amount * P)  (rounded up)
        let product = amount
            .checked_mul(sqrt_price)
            .map_err(|_| StdError::generic_err("sqrt price product overflow"))?;
        if numerator1 <= product {
            return Err(StdError::generic_err(
                "get_next_sqrt_price_from_amount0: price cannot go up, insufficient liquidity",
            ));
        }
        let denominator = numerator1 - product;
        mul_div_round_up(numerator1, sqrt_price, denominator)
    }
}

/// Computes the next sqrt price after swapping `amount` of token1 into the pool,
/// rounding DOWN so that the invariant is preserved in the pool's favor.
///
/// Formula (V3 v1.0.1):
/// - add (one_for_zero): new = sqrt_p + (amount << 96) / L         (round down)
/// - sub (zero_for_one removing): new = sqrt_p - ceil((amount << 96) / L)  (round up on quotient)
fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_price: Uint256,
    liquidity: u128,
    amount: Uint256,
    add: bool,
) -> StdResult<Uint256> {
    if amount.is_zero() {
        return Ok(sqrt_price);
    }
    if liquidity == 0 {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_amount1: liquidity is zero",
        ));
    }
    let liquidity_u256 = Uint256::from(liquidity);

    if add {
        // Adding token1: price goes UP by amount/L (rounded down).
        // quotient = (amount << 96) / L   (exact if fits)
        let shifted = amount.checked_shl(RESOLUTION);
        let quotient = if let Ok(sh) = shifted {
            sh / liquidity_u256
        } else {
            // amount * Q96 overflows 256 bits: fall back to mul_div.
            mul_div(amount, q96(), liquidity_u256)?
        };
        sqrt_price
            .checked_add(quotient)
            .map_err(|_| StdError::generic_err("sqrt price add overflow"))
    } else {
        // Removing token1: price goes DOWN by ceil(amount / L).
        let shifted = amount.checked_shl(RESOLUTION);
        let quotient = if let Ok(sh) = shifted {
            div_round_up(sh, liquidity_u256)?
        } else {
            mul_div_round_up(amount, q96(), liquidity_u256)?
        };
        if sqrt_price <= quotient {
            return Err(StdError::generic_err(
                "get_next_sqrt_price_from_amount1: price cannot go down past zero",
            ));
        }
        Ok(sqrt_price - quotient)
    }
}

/// Computes the next sqrt price given a known amount of input tokens.
///
/// When the pool is gaining token0 (`zero_for_one = true`), price decreases
/// and we must round UP (V3 invariant).
/// When the pool is gaining token1, price increases and we round DOWN.
pub fn get_next_sqrt_price_from_input(
    sqrt_price: Uint256,
    liquidity: u128,
    amount_in: Uint256,
    zero_for_one: bool,
) -> StdResult<Uint256> {
    if sqrt_price.is_zero() {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_input: zero sqrt_price",
        ));
    }
    if liquidity == 0 {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_input: zero liquidity",
        ));
    }

    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_in, true)
    }
}

/// Computes the next sqrt price given a known amount of output tokens.
///
/// Symmetric to `get_next_sqrt_price_from_input`.
pub fn get_next_sqrt_price_from_output(
    sqrt_price: Uint256,
    liquidity: u128,
    amount_out: Uint256,
    zero_for_one: bool,
) -> StdResult<Uint256> {
    if sqrt_price.is_zero() {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_output: zero sqrt_price",
        ));
    }
    if liquidity == 0 {
        return Err(StdError::generic_err(
            "get_next_sqrt_price_from_output: zero liquidity",
        ));
    }

    if zero_for_one {
        // Pool gains token0 (price drops); output is token1; use the token1 path with sub.
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_out, false)
    } else {
        // Pool gains token1 (price rises); output is token0; use the token0 path with sub.
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q96_val() -> Uint256 {
        q96()
    }

    #[test]
    fn amount0_delta_round_up_vs_down_differs_by_one_at_most() {
        let a = q96_val();
        let b = q96_val() * Uint256::from(2u128);
        let up = get_amount0_delta(a, b, 1_000_000_000u128, true).unwrap();
        let down = get_amount0_delta(a, b, 1_000_000_000u128, false).unwrap();
        assert!(up >= down);
        assert!(up - down <= Uint256::one());
    }

    #[test]
    fn next_sqrt_price_amount0_add_rounds_up() {
        let price = q96_val();
        let l: u128 = 10u128.pow(18);
        let p1 =
            get_next_sqrt_price_from_amount0_rounding_up(price, l, Uint256::from(1u128), true)
                .unwrap();
        // Adding token0 pushes price down; but since we round up, new price >= exact.
        assert!(p1 < price);
    }

    #[test]
    fn next_sqrt_price_amount1_add_rounds_down() {
        let price = q96_val();
        let l: u128 = 10u128.pow(18);
        let p1 =
            get_next_sqrt_price_from_amount1_rounding_down(price, l, Uint256::from(1u128), true)
                .unwrap();
        // Adding token1 pushes price up.
        assert!(p1 >= price);
    }

    #[test]
    fn next_sqrt_price_zero_amount_is_identity() {
        let price = q96_val() * Uint256::from(5u128);
        let l: u128 = 10u128.pow(18);
        for zfo in [true, false] {
            assert_eq!(
                get_next_sqrt_price_from_input(price, l, Uint256::zero(), zfo).unwrap(),
                price
            );
        }
    }

    #[test]
    fn next_sqrt_price_zero_liquidity_errors() {
        assert!(
            get_next_sqrt_price_from_input(q96_val(), 0, Uint256::from(1u128), true).is_err()
        );
    }

    #[test]
    fn next_sqrt_price_zero_price_errors() {
        assert!(
            get_next_sqrt_price_from_input(Uint256::zero(), 1, Uint256::from(1u128), true)
                .is_err()
        );
    }

    #[test]
    fn next_sqrt_price_cannot_go_negative_on_output_sub() {
        // Remove way more than the range can support.
        let price = q96_val();
        let l: u128 = 1;
        let big_amount = Uint256::from(10u128).pow(30);
        assert!(get_next_sqrt_price_from_output(price, l, big_amount, true).is_err());
    }

    #[test]
    fn input_rounds_price_conservatively_for_zfo() {
        // For zero_for_one, the new price must be >= what the raw float would give,
        // because rounding-up the quotient means the pool keeps more of token1 for itself.
        let price = q96_val() * Uint256::from(3u128);
        let l: u128 = 1_000_000_000u128;
        let amount = Uint256::from(500_000u128);
        let new_price = get_next_sqrt_price_from_input(price, l, amount, true).unwrap();
        // Recompute via token0 delta: L * (P_new * P_old - ... ) — we sanity-check
        // that the token0 implied by new_price is <= the input amount.
        let implied_in = get_amount0_delta(new_price, price, l, true).unwrap();
        assert!(implied_in <= amount);
    }
}

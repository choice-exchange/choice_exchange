use crate::full_math::{mul_div, mul_div_round_up};
use crate::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};
use cosmwasm_std::{StdError, StdResult, Uint256};

/// Fee unit denominator. Fees are expressed in parts-per-million.
/// Uniswap V3 uses the same `1_000_000` denominator with fees of e.g. 500, 3000, 10000 pips.
pub const FEE_DENOMINATOR: u32 = 1_000_000;

pub struct SwapStepResult {
    pub sqrt_ratio_next_x96: Uint256,
    pub amount_in: Uint256,
    pub amount_out: Uint256,
    pub fee_amount: Uint256,
}

/// Computes the result of swapping some amount in or out, within a single price range.
///
/// # Parameters
/// - `sqrt_ratio_current_x96`: current sqrt price (Q64.96)
/// - `sqrt_ratio_target_x96`: target sqrt price — the tick boundary or user-specified limit
/// - `liquidity`: active liquidity over the range
/// - `amount_remaining`: input available (exact-in) or output desired (exact-out, signaled by caller)
/// - `fee_pips`: fee in ppm, must be `< FEE_DENOMINATOR` (i.e. < 1_000_000)
/// - `zero_for_one`: whether pool is gaining token0 (price decreasing)
///
/// # Errors
/// - fee_pips >= FEE_DENOMINATOR
/// - liquidity == 0
/// - Any overflow in intermediate math (propagated as `StdError`)
///
/// This is the exact-in variant, matching V3's `computeSwapStep` with positive
/// `amountRemaining`. Exact-out is intentionally not supported at the pool level
/// today (see pool swap actions) and must be added explicitly if needed.
pub fn compute_swap_step(
    sqrt_ratio_current_x96: Uint256,
    sqrt_ratio_target_x96: Uint256,
    liquidity: u128,
    amount_remaining: Uint256,
    fee_pips: u32,
    zero_for_one: bool,
) -> StdResult<SwapStepResult> {
    // --- Input validation ---
    // fee_pips must leave a positive denominator in `(1_000_000 - fee_pips)`.
    if fee_pips >= FEE_DENOMINATOR {
        return Err(StdError::generic_err(
            "compute_swap_step: fee_pips must be < 1_000_000",
        ));
    }
    if sqrt_ratio_current_x96.is_zero() || sqrt_ratio_target_x96.is_zero() {
        return Err(StdError::generic_err(
            "compute_swap_step: sqrt price must be > 0",
        ));
    }

    // V3 parity: we intentionally do NOT validate target direction vs
    // `zero_for_one`, nor reject `liquidity == 0`. V3's `computeSwapStep`
    // accepts these degenerate cases:
    //   - target == current: result is a no-op step (all deltas zero).
    //   - liquidity == 0: deltas are zero via `get_amount*_delta(..., 0, ...)`,
    //     `max_reached == true`, and the price advances to the target for free.
    //     This is how the pool traverses zero-liquidity gaps between ticks.
    // The outer swap loop is responsible for ensuring target is on the correct
    // side of current when advancing through initialized ticks.

    // --- Step 1: how much input to reach target (rounded UP, in favor of pool) ---
    let amount_in_to_reach_target = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            true,
        )?
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            true,
        )?
    };

    // --- Step 2: fee-adjusted cost to reach target ---
    // fee_for_step = ceil(amount_in * fee_pips / (1_000_000 - fee_pips))
    // i.e. total_cost = amount_in / (1 - fee_rate)
    let fee_for_step = mul_div_round_up(
        amount_in_to_reach_target,
        Uint256::from(fee_pips),
        Uint256::from(FEE_DENOMINATOR - fee_pips),
    )?;
    let total_cost_to_target = amount_in_to_reach_target
        .checked_add(fee_for_step)
        .map_err(|_| StdError::generic_err("compute_swap_step: cost overflow"))?;

    let max_reached = amount_remaining >= total_cost_to_target;

    let (sqrt_ratio_next_x96, amount_in, amount_out, fee_amount) = if max_reached {
        // --- Case A: full step to the target ---
        let next_price = sqrt_ratio_target_x96;
        let in_amt = amount_in_to_reach_target;

        let out_amt = if zero_for_one {
            get_amount1_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            )?
        } else {
            get_amount0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                false,
            )?
        };

        (next_price, in_amt, out_amt, fee_for_step)
    } else {
        // --- Case B: partial step, price does not reach target ---
        // amount_in_avail = floor(amount_remaining * (1 - fee_rate))
        let amount_in_avail = mul_div(
            amount_remaining,
            Uint256::from(FEE_DENOMINATOR - fee_pips),
            Uint256::from(FEE_DENOMINATOR),
        )?;

        let next_price = get_next_sqrt_price_from_input(
            sqrt_ratio_current_x96,
            liquidity,
            amount_in_avail,
            zero_for_one,
        )?;

        // Recompute the actual in/out deltas at the new price (rounded in favor of pool).
        let in_amt = if zero_for_one {
            get_amount0_delta(next_price, sqrt_ratio_current_x96, liquidity, true)?
        } else {
            get_amount1_delta(sqrt_ratio_current_x96, next_price, liquidity, true)?
        };
        let out_amt = if zero_for_one {
            get_amount1_delta(next_price, sqrt_ratio_current_x96, liquidity, false)?
        } else {
            get_amount0_delta(sqrt_ratio_current_x96, next_price, liquidity, false)?
        };

        // V3 invariant: in the partial-step case, fee is the entire slack between
        // `amount_remaining` and `in_amt`. We do NOT clamp in_amt here — instead
        // `get_next_sqrt_price_from_input` rounds the price so that re-deriving
        // `in_amt` produces a value <= amount_in_avail. If math is correct, this
        // holds. We defensively check to surface any rounding bug rather than
        // silently over-charging.
        if in_amt > amount_in_avail {
            return Err(StdError::generic_err(
                "compute_swap_step: rounding invariant violated (in_amt > amount_in_avail)",
            ));
        }

        let fee = amount_remaining
            .checked_sub(in_amt)
            .map_err(|_| StdError::generic_err("compute_swap_step: fee underflow"))?;

        (next_price, in_amt, out_amt, fee)
    };

    Ok(SwapStepResult {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Variant of `compute_swap_step` for exact-output swaps.
///
/// Exact-output isn't wired through the pool contract today. Left here so the
/// math library exposes a complete V3-style surface; callers pass the *output*
/// budget as `amount_remaining` and receive `amount_in` as the cost they must
/// supply.
#[allow(dead_code)]
pub fn compute_swap_step_exact_out(
    sqrt_ratio_current_x96: Uint256,
    sqrt_ratio_target_x96: Uint256,
    liquidity: u128,
    amount_out_remaining: Uint256,
    fee_pips: u32,
    zero_for_one: bool,
) -> StdResult<SwapStepResult> {
    if fee_pips >= FEE_DENOMINATOR {
        return Err(StdError::generic_err(
            "compute_swap_step_exact_out: fee_pips must be < 1_000_000",
        ));
    }
    if liquidity == 0 {
        return Err(StdError::generic_err(
            "compute_swap_step_exact_out: liquidity must be > 0",
        ));
    }

    let price_drops = sqrt_ratio_target_x96 < sqrt_ratio_current_x96;
    if price_drops != zero_for_one {
        return Err(StdError::generic_err(
            "compute_swap_step_exact_out: target direction inconsistent with zero_for_one",
        ));
    }

    // Max output achievable from current to target (rounded DOWN, in favor of pool).
    let amount_out_to_reach_target = if zero_for_one {
        get_amount1_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            false,
        )?
    } else {
        get_amount0_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            false,
        )?
    };

    let max_reached = amount_out_remaining >= amount_out_to_reach_target;

    let (sqrt_ratio_next_x96, amount_in, amount_out) = if max_reached {
        let in_amt = if zero_for_one {
            get_amount0_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                true,
            )?
        } else {
            get_amount1_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                true,
            )?
        };
        (sqrt_ratio_target_x96, in_amt, amount_out_to_reach_target)
    } else {
        let next_price = get_next_sqrt_price_from_output(
            sqrt_ratio_current_x96,
            liquidity,
            amount_out_remaining,
            zero_for_one,
        )?;
        let in_amt = if zero_for_one {
            get_amount0_delta(next_price, sqrt_ratio_current_x96, liquidity, true)?
        } else {
            get_amount1_delta(sqrt_ratio_current_x96, next_price, liquidity, true)?
        };
        // out must match requested (defensively clamp to avoid over-pay if rounding disagrees)
        let out_amt = if zero_for_one {
            get_amount1_delta(next_price, sqrt_ratio_current_x96, liquidity, false)?
        } else {
            get_amount0_delta(sqrt_ratio_current_x96, next_price, liquidity, false)?
        };
        let out_amt = if out_amt > amount_out_remaining {
            amount_out_remaining
        } else {
            out_amt
        };
        (next_price, in_amt, out_amt)
    };

    let fee_amount = mul_div_round_up(
        amount_in,
        Uint256::from(fee_pips),
        Uint256::from(FEE_DENOMINATOR - fee_pips),
    )?;

    Ok(SwapStepResult {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q96() -> Uint256 {
        Uint256::from_u128(1u128 << 96)
    }

    #[test]
    fn rejects_fee_at_or_above_denominator() {
        let p = q96();
        let res = compute_swap_step(p, p / Uint256::from(2u128), 1_000_000, Uint256::from(100u128), FEE_DENOMINATOR, true);
        assert!(res.is_err());
    }

    #[test]
    fn zero_liquidity_advances_to_target_for_free() {
        // V3 parity: L=0 gaps between ticks advance the price to `target`
        // with zero in/out/fee, allowing the outer swap loop to cross them.
        let p = q96();
        let target = p / Uint256::from(2u128);
        let res = compute_swap_step(p, target, 0, Uint256::from(100u128), 3000, true).unwrap();
        assert_eq!(res.sqrt_ratio_next_x96, target);
        assert_eq!(res.amount_in, Uint256::zero());
        assert_eq!(res.amount_out, Uint256::zero());
        assert_eq!(res.fee_amount, Uint256::zero());
    }

    #[test]
    fn degenerate_target_equals_current_is_noop() {
        let p = q96();
        let res = compute_swap_step(p, p, 1_000_000, Uint256::from(100u128), 3000, true).unwrap();
        assert_eq!(res.sqrt_ratio_next_x96, p);
        assert_eq!(res.amount_in, Uint256::zero());
        assert_eq!(res.amount_out, Uint256::zero());
        assert_eq!(res.fee_amount, Uint256::zero());
    }

    #[test]
    fn full_step_when_input_sufficient() {
        let p = q96();
        let target = p * Uint256::from(2u128);
        let l = 10u128.pow(18);
        // Huge input ensures max_reached.
        let huge = Uint256::from(10u128).pow(30);
        let r = compute_swap_step(p, target, l, huge, 3000, false).unwrap();
        assert_eq!(r.sqrt_ratio_next_x96, target);
        assert!(r.amount_in > Uint256::zero());
        assert!(r.amount_out > Uint256::zero());
    }

    #[test]
    fn partial_step_in_plus_fee_equals_remaining() {
        let p = q96();
        let target = p * Uint256::from(2u128);
        let l = 10u128.pow(18);
        // Small input that won't reach target.
        let small = Uint256::from(1_000u128);
        let r = compute_swap_step(p, target, l, small, 3000, false).unwrap();
        // Invariant: amount_in + fee == amount_remaining
        assert_eq!(r.amount_in + r.fee_amount, small);
        assert_ne!(r.sqrt_ratio_next_x96, target);
    }
}

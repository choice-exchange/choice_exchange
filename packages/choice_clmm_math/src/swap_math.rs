use crate::full_math::{mul_div, mul_div_round_up};
use crate::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use cosmwasm_std::{StdError, StdResult, Uint256};

pub struct SwapStepResult {
    pub sqrt_ratio_next_x96: Uint256,
    pub amount_in: Uint256,
    pub amount_out: Uint256,
    pub fee_amount: Uint256,
}

pub fn compute_swap_step(
    sqrt_ratio_current_x96: Uint256,
    sqrt_ratio_target_x96: Uint256,
    liquidity: u128,
    amount_remaining: Uint256,
    fee_pips: u32,
    zero_for_one: bool,
) -> StdResult<SwapStepResult> {
    // 1. Calculate how much input is needed to reach the Target Price fully
    let amount_in_to_reach_target = if zero_for_one {
        get_amount0_delta(
            sqrt_ratio_target_x96,
            sqrt_ratio_current_x96,
            liquidity,
            true,
        )
    } else {
        get_amount1_delta(
            sqrt_ratio_current_x96,
            sqrt_ratio_target_x96,
            liquidity,
            true,
        )
    };

    // 2. Calculate Fee-adjusted input needed
    // fee = amount * fee_rate / (1 - fee_rate) ? No, standard is fee added on top.
    // We want to know if `amount_remaining` covers `amount_in + fee`.
    let fee_for_step = mul_div_round_up(
        amount_in_to_reach_target,
        Uint256::from(fee_pips),
        Uint256::from(1_000_000u32 - fee_pips),
    );
    let total_cost_to_target = amount_in_to_reach_target + fee_for_step;

    let max_reached = amount_remaining >= total_cost_to_target;

    let (sqrt_ratio_next_x96, amount_in, amount_out, fee_amount) = if max_reached {
        // Case A: Full Step
        let next_price = sqrt_ratio_target_x96;
        let in_amt = amount_in_to_reach_target;
        let fee = fee_for_step;

        let out_amt = if zero_for_one {
            get_amount1_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            )
        } else {
            get_amount0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                false,
            )
        };

        (next_price, in_amt, out_amt, fee)
    } else {
        // Case B: Partial Step
        // Calculate amount available for swap (principal)
        let amount_in_avail = mul_div(
            amount_remaining,
            Uint256::from(1_000_000u32 - fee_pips),
            Uint256::from(1_000_000u32),
        );

        let next_price = get_next_sqrt_price_from_input(
            sqrt_ratio_current_x96,
            liquidity,
            amount_in_avail,
            zero_for_one,
        )?;

        let (mut in_amt, out_amt) = if zero_for_one {
            (
                get_amount0_delta(next_price, sqrt_ratio_current_x96, liquidity, true),
                get_amount1_delta(next_price, sqrt_ratio_current_x96, liquidity, false),
            )
        } else {
            (
                get_amount1_delta(sqrt_ratio_current_x96, next_price, liquidity, true),
                get_amount0_delta(sqrt_ratio_current_x96, next_price, liquidity, false),
            )
        };

        // FIX: Clamp in_amt to amount_in_avail.
        // Due to rounding, get_amount_delta might return 1 wei more than input used to derive price.
        // If this happens, we subtract more than we have, causing panic.
        if in_amt > amount_in_avail {
            in_amt = amount_in_avail;
        }

        // The remaining amount is fee
        // This subtraction is now safe because in_amt <= amount_in_avail <= amount_remaining
        let fee = amount_remaining - in_amt;

        (next_price, in_amt, out_amt, fee)
    };

    Ok(SwapStepResult {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

fn get_next_sqrt_price_from_input(
    sqrt_price: Uint256,
    liquidity: u128,
    amount_in: Uint256,
    zero_for_one: bool,
) -> StdResult<Uint256> {
    let liquidity_u256 = Uint256::from(liquidity) << 96u32;

    if zero_for_one {
        let product = mul_div(amount_in, sqrt_price, Uint256::one());
        let denominator = liquidity_u256
            .checked_add(product)
            .map_err(|_| StdError::generic_err("Price calc overflow"))?;
        Ok(mul_div(liquidity_u256, sqrt_price, denominator))
    } else {
        let quotient = mul_div(amount_in, Uint256::one() << 96u32, Uint256::from(liquidity));
        Ok(sqrt_price + quotient)
    }
}

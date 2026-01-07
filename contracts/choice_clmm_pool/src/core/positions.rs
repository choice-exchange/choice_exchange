use std::convert::TryFrom;

use crate::core::ticks::get_fee_growth_inside;
use crate::state::{FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POSITIONS};
use choice_clmm_math::full_math::mul_div;
use cosmwasm_std::{StdResult, Storage, Uint128, Uint256};

/// Updates a position's fee growth and tokens owed.
/// This MUST be called before modifying the position's liquidity.
pub fn update_position(
    storage: &mut dyn Storage,
    owner: &str,
    lower_tick: i32,
    upper_tick: i32,
    current_tick: i32,
    liquidity_delta: i128, // +L for Mint, -L for Burn, 0 for Fee Claim
) -> StdResult<(Uint128, Uint128)> {
    let key = (owner, lower_tick, upper_tick);
    let mut position = POSITIONS.may_load(storage, key)?.unwrap_or_default();

    // 1. Get Global Fee Growth
    // (In a real app, ensure these are initialized in Instantiate or first Swap)
    let global_0 = FEE_GROWTH_GLOBAL_0
        .may_load(storage)?
        .unwrap_or(Uint256::zero());
    let global_1 = FEE_GROWTH_GLOBAL_1
        .may_load(storage)?
        .unwrap_or(Uint256::zero());

    // 2. Calculate Fee Growth Inside the Range
    // This math handles the fact that fee growth flips when ticks are crossed
    let (fee_growth_inside_0, fee_growth_inside_1) = get_fee_growth_inside(
        storage,
        lower_tick,
        upper_tick,
        current_tick,
        global_0,
        global_1,
    )?;

    // 3. Calculate Uncollected Fees
    // delta_fees = liquidity * (growth_inside - growth_inside_last)
    // We use wrapping_sub because fee accumulators overflow by design.

    let fee_growth_delta_0 = fee_growth_inside_0.wrapping_sub(position.fee_growth_inside_0_last);
    let fee_growth_delta_1 = fee_growth_inside_1.wrapping_sub(position.fee_growth_inside_1_last);

    if position.liquidity > 0 {
        // Calculate raw token amounts
        // FullMath.mulDiv(liquidity, fee_growth_delta, Q128)
        let tokens_0 = mul_div(
            Uint256::from(position.liquidity),
            fee_growth_delta_0,
            Uint256::one() << 128u32,
        );
        let tokens_1 = mul_div(
            Uint256::from(position.liquidity),
            fee_growth_delta_1,
            Uint256::one() << 128u32,
        );

        // Add to owed
        position.tokens_owed_0 += Uint128::try_from(tokens_0).unwrap();
        position.tokens_owed_1 += Uint128::try_from(tokens_1).unwrap();
    }

    // 4. Update Position State
    position.fee_growth_inside_0_last = fee_growth_inside_0;
    position.fee_growth_inside_1_last = fee_growth_inside_1;

    // Apply Liquidity Delta (Mint/Burn)
    if liquidity_delta != 0 {
        if liquidity_delta > 0 {
            position.liquidity += liquidity_delta as u128;
        } else {
            // Safety check for underflow
            let remove = (-liquidity_delta) as u128;
            if remove > position.liquidity {
                return Err(cosmwasm_std::StdError::generic_err("Liquidity underflow"));
            }
            position.liquidity -= remove;
        }
    }

    POSITIONS.save(storage, key, &position)?;

    Ok((position.tokens_owed_0, position.tokens_owed_1))
}

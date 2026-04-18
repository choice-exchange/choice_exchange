use std::convert::TryFrom;

use crate::core::ticks::get_fee_growth_inside;
use crate::state::{FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POSITIONS};
use choice_clmm_math::full_math::mul_div;
use cosmwasm_std::{StdError, StdResult, Storage, Uint128, Uint256};

/// Updates a position's fee growth and tokens owed.
/// This MUST be called before modifying the position's liquidity.
///
/// Requires the position to already exist in storage. `execute_mint` is the
/// sole creator and must save a default `PositionInfo` for the key before
/// calling this. Silent creation here (the prior `may_load().unwrap_or_default()`
/// pattern) was a landmine: any caller passing a non-existent key would silently
/// mint a ghost position with the current fee_growth snapshot, which could then
/// be drained of cumulative fees on a later `update_position` with positive L.
pub fn update_position(
    storage: &mut dyn Storage,
    owner: &str,
    lower_tick: i32,
    upper_tick: i32,
    current_tick: i32,
    liquidity_delta: i128, // +L for Mint, -L for Burn, 0 for Fee Claim
) -> StdResult<(Uint128, Uint128)> {
    let key = (owner, lower_tick, upper_tick);
    let mut position = POSITIONS
        .may_load(storage, key)?
        .ok_or_else(|| StdError::not_found("PositionInfo"))?;

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
        // FullMath.mulDiv(liquidity, fee_growth_delta, Q128)
        let tokens_0 = mul_div(
            Uint256::from(position.liquidity),
            fee_growth_delta_0,
            Uint256::one() << 128u32,
        )?;
        let tokens_1 = mul_div(
            Uint256::from(position.liquidity),
            fee_growth_delta_1,
            Uint256::one() << 128u32,
        )?;

        // Saturate at u128::MAX instead of unwrapping (V3 does the same in
        // `update()` via explicit overflow tolerance). Without saturation a
        // position accruing > u128 of fees on one side would brick its own
        // mint/burn/collect (update_position is always called first),
        // permanently locking the user's principal. The user can always
        // collect the saturated amount via `Collect`; any further accrual is
        // socialized (effectively donated to the pool) — acceptable tradeoff
        // vs. total lockup.
        position.tokens_owed_0 = saturating_add_to_u128(position.tokens_owed_0, tokens_0)?;
        position.tokens_owed_1 = saturating_add_to_u128(position.tokens_owed_1, tokens_1)?;
    }

    // 4. Update Position State
    position.fee_growth_inside_0_last = fee_growth_inside_0;
    position.fee_growth_inside_1_last = fee_growth_inside_1;

    // Apply liquidity delta (mint/burn). Checked arithmetic throughout —
    // `liquidity_delta` is validated by the caller to fit in i128, so
    // `unsigned_abs()` is safe and can't panic.
    if liquidity_delta != 0 {
        if liquidity_delta > 0 {
            let add = liquidity_delta.unsigned_abs();
            position.liquidity = position
                .liquidity
                .checked_add(add)
                .ok_or_else(|| cosmwasm_std::StdError::generic_err("Position liquidity overflow"))?;
        } else {
            let remove = liquidity_delta.unsigned_abs();
            position.liquidity = position
                .liquidity
                .checked_sub(remove)
                .ok_or_else(|| cosmwasm_std::StdError::generic_err("Liquidity underflow"))?;
        }
    }

    POSITIONS.save(storage, key, &position)?;

    Ok((position.tokens_owed_0, position.tokens_owed_1))
}

/// Saturating add of a Uint256 fee amount to a Uint128 running total.
/// Returns `Uint128::MAX` if the sum would overflow u128.
fn saturating_add_to_u128(current: Uint128, add: Uint256) -> StdResult<Uint128> {
    let add_u128 = match Uint128::try_from(add) {
        Ok(v) => v,
        Err(_) => return Ok(Uint128::MAX),
    };
    Ok(current.checked_add(add_u128).unwrap_or(Uint128::MAX))
}

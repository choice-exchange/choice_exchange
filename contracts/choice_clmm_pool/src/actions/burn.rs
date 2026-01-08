use std::convert::TryFrom;

use crate::core::positions::update_position;
use crate::error::ContractError;
use crate::state::{POOL_STATE, TICKS};
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;
use cosmwasm_std::{DepsMut, Env, MessageInfo, Response, StdError, Uint128};

pub fn execute_burn(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    lower_tick: i32,
    upper_tick: i32,
    amount: Uint128, // Amount of Liquidity (L) to burn
) -> Result<Response, ContractError> {
    let mut slot0 = POOL_STATE.load(deps.storage)?;
    let liquidity_burned = amount.u128();

    if liquidity_burned == 0 {
        return Ok(Response::default());
    }

    // 1. Update Position (Calculates Fees + Decreases L)
    // We pass negative amount to signal removal
    update_position(
        deps.storage,
        info.sender.as_str(),
        lower_tick,
        upper_tick,
        slot0.tick,
        -(liquidity_burned as i128),
    )?;

    // 2. Calculate Principal Amounts (Token0/1) from the Burned L
    let sqrt_price_lower = get_sqrt_ratio_at_tick(lower_tick)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(upper_tick)?;

    let amount0: Uint128;
    let amount1: Uint128;

    // Use standard CLMM math to see how much token0/1 this L represents at current price
    if slot0.tick < lower_tick {
        // Range is above current price (Pure Token0)
        let a0 = get_amount0_delta(sqrt_price_lower, sqrt_price_upper, liquidity_burned, false); // false = round down (withdraw)
        amount0 = Uint128::try_from(a0).unwrap();
        amount1 = Uint128::zero();
    } else if slot0.tick >= upper_tick {
        // Range is below current price (Pure Token1)
        amount0 = Uint128::zero();
        let a1 = get_amount1_delta(sqrt_price_lower, sqrt_price_upper, liquidity_burned, false);
        amount1 = Uint128::try_from(a1).unwrap();
    } else {
        // In range (Mixed)
        let a0 = get_amount0_delta(slot0.sqrt_price, sqrt_price_upper, liquidity_burned, false);
        let a1 = get_amount1_delta(sqrt_price_lower, slot0.sqrt_price, liquidity_burned, false);
        amount0 = Uint128::try_from(a0).unwrap();
        amount1 = Uint128::try_from(a1).unwrap();

        // Update Global Liquidity since we are in range
        slot0.liquidity = slot0
            .liquidity
            .checked_sub(amount)
            .map_err(|_| StdError::generic_err("Global liquidity underflow"))?;
        POOL_STATE.save(deps.storage, &slot0)?;
    }

    // 3. Update Ticks (Decrement Liquidity Gross/Net)
    // (Logic similar to Mint but inverted signs)
    let mut tick_lower = TICKS.load(deps.storage, lower_tick)?;
    let mut tick_upper = TICKS.load(deps.storage, upper_tick)?;

    tick_lower.active_positions_count -= liquidity_burned;
    tick_lower.liquidity_delta -= liquidity_burned as i128; // Lower tick crossing reduces net L

    tick_upper.active_positions_count -= liquidity_burned;
    tick_upper.liquidity_delta += liquidity_burned as i128; // Upper tick crossing increases net L (inverted)

    // Clear tick if empty to save gas/storage? (Optional optimization)
    // For now just save.
    TICKS.save(deps.storage, lower_tick, &tick_lower)?;
    TICKS.save(deps.storage, upper_tick, &tick_upper)?;

    // 4. Push Principal to `tokens_owed`
    // We must re-load position because `update_position` saved it, but didn't include these new principal amounts
    // Actually, it's cleaner to handle this update manually or add a helper `credit_position`.

    use crate::state::POSITIONS;
    let key = (info.sender.as_str(), lower_tick, upper_tick);
    let mut position = POSITIONS.load(deps.storage, key)?;

    position.tokens_owed_0 += amount0;
    position.tokens_owed_1 += amount1;
    POSITIONS.save(deps.storage, key, &position)?;

    Ok(Response::new()
        .add_attribute("action", "burn")
        .add_attribute("amount0_burned", amount0)
        .add_attribute("amount1_burned", amount1))
}

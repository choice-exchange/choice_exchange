use std::convert::TryFrom;

use crate::core::positions::update_position;
use crate::error::ContractError;
use crate::state::{POOL_STATE, POSITIONS, TICKS};
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;
use cosmwasm_std::{DepsMut, Env, MessageInfo, Response, StdError, Uint128};

pub fn execute_burn(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    lower_tick: i32,
    upper_tick: i32,
    amount: Uint128, // Amount of liquidity (L) to burn
) -> Result<Response, ContractError> {
    let mut slot0 = POOL_STATE.load(deps.storage)?;
    let liquidity_burned = amount.u128();

    if liquidity_burned == 0 {
        return Ok(Response::default());
    }

    // Guard against `u128 as i128` silently wrapping. The mint path enforces
    // this too, but we defend in depth in case storage ever holds a corrupted
    // liquidity that survived a pre-hardening upgrade.
    let burned_i128 = i128::try_from(liquidity_burned).map_err(|_| {
        StdError::generic_err("Burn amount exceeds i128::MAX (liquidity too large)")
    })?;

    // SECURITY: pool keys positions by `info.sender`. A user can only burn
    // their own positions. The manager burns its own pool-level position via
    // submessage from the manager contract; per-NFT accounting lives there.
    //
    // 1. Update position (computes + saturates fees, decreases stored L).
    update_position(
        deps.storage,
        info.sender.as_str(),
        lower_tick,
        upper_tick,
        slot0.tick,
        -burned_i128,
    )?;

    // 2. Compute principal amounts owed (round DOWN in pool's favor).
    let sqrt_price_lower = get_sqrt_ratio_at_tick(lower_tick)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(upper_tick)?;

    let amount0: Uint128;
    let amount1: Uint128;

    if slot0.tick < lower_tick {
        // Range is above current price (token0 only).
        let a0 = get_amount0_delta(sqrt_price_lower, sqrt_price_upper, liquidity_burned, false)?;
        amount0 = Uint128::try_from(a0)
            .map_err(|_| StdError::generic_err("burn: amount0 exceeds u128"))?;
        amount1 = Uint128::zero();
    } else if slot0.tick >= upper_tick {
        // Range is below current price (token1 only).
        amount0 = Uint128::zero();
        let a1 = get_amount1_delta(sqrt_price_lower, sqrt_price_upper, liquidity_burned, false)?;
        amount1 = Uint128::try_from(a1)
            .map_err(|_| StdError::generic_err("burn: amount1 exceeds u128"))?;
    } else {
        // In range (both tokens).
        let a0 = get_amount0_delta(slot0.sqrt_price, sqrt_price_upper, liquidity_burned, false)?;
        let a1 = get_amount1_delta(sqrt_price_lower, slot0.sqrt_price, liquidity_burned, false)?;
        amount0 = Uint128::try_from(a0)
            .map_err(|_| StdError::generic_err("burn: amount0 exceeds u128"))?;
        amount1 = Uint128::try_from(a1)
            .map_err(|_| StdError::generic_err("burn: amount1 exceeds u128"))?;

        // Decrement global active liquidity.
        slot0.liquidity = slot0
            .liquidity
            .checked_sub(amount)
            .map_err(|_| StdError::generic_err("Global liquidity underflow"))?;
        POOL_STATE.save(deps.storage, &slot0)?;
    }

    // 3. Update ticks (checked arithmetic — no unchecked `+=` / `-=`).
    let mut tick_lower = TICKS.load(deps.storage, lower_tick)?;
    let mut tick_upper = TICKS.load(deps.storage, upper_tick)?;

    tick_lower.active_positions_count = tick_lower
        .active_positions_count
        .checked_sub(liquidity_burned)
        .ok_or_else(|| StdError::generic_err("Lower tick active_positions_count underflow"))?;
    tick_lower.liquidity_delta = tick_lower
        .liquidity_delta
        .checked_sub(burned_i128)
        .ok_or_else(|| StdError::generic_err("Lower tick liquidity_delta underflow"))?;

    tick_upper.active_positions_count = tick_upper
        .active_positions_count
        .checked_sub(liquidity_burned)
        .ok_or_else(|| StdError::generic_err("Upper tick active_positions_count underflow"))?;
    tick_upper.liquidity_delta = tick_upper
        .liquidity_delta
        .checked_add(burned_i128)
        .ok_or_else(|| StdError::generic_err("Upper tick liquidity_delta overflow"))?;

    TICKS.save(deps.storage, lower_tick, &tick_lower)?;
    TICKS.save(deps.storage, upper_tick, &tick_upper)?;

    // 4. Credit the burned principal to the caller's `tokens_owed`. A
    //    subsequent `Collect` transfers the tokens. Saturating-add matches
    //    `update_position` (see positions.rs) so a user can never brick their
    //    own position via fee-accumulator overflow.
    let key = (info.sender.as_str(), lower_tick, upper_tick);
    let mut position = POSITIONS.load(deps.storage, key)?;
    position.tokens_owed_0 = position
        .tokens_owed_0
        .checked_add(amount0)
        .unwrap_or(Uint128::MAX);
    position.tokens_owed_1 = position
        .tokens_owed_1
        .checked_add(amount1)
        .unwrap_or(Uint128::MAX);
    POSITIONS.save(deps.storage, key, &position)?;

    Ok(Response::new()
        .add_attribute("action", "burn")
        .add_attribute("owner", info.sender.as_str())
        .add_attribute("lower_tick", lower_tick.to_string())
        .add_attribute("upper_tick", upper_tick.to_string())
        .add_attribute("liquidity_burned", liquidity_burned.to_string())
        .add_attribute("amount0_burned", amount0)
        .add_attribute("amount1_burned", amount1))
}

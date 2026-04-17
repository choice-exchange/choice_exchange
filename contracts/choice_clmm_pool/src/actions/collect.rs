use crate::error::ContractError;
use crate::state::{POOL_CONFIG, POSITIONS};
use cosmwasm_std::{CosmosMsg, DepsMut, MessageInfo, Response, Uint128};
// We define MaxUint128 to represent "Collect All"
const MAX_UINT128: Uint128 = Uint128::new(u128::MAX);

pub fn execute_collect(
    deps: DepsMut,
    info: MessageInfo,
    recipient: String,
    lower_tick: i32,
    upper_tick: i32,
    amount0_requested: Uint128, // Set to MAX_UINT128 to collect all
    amount1_requested: Uint128,
) -> Result<Response, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    let key = (info.sender.as_str(), lower_tick, upper_tick);
    let mut position = POSITIONS
        .load(deps.storage, key)
        .map_err(|_| ContractError::PositionNotFound {})?;

    // 1. Determine amounts to collect
    let amount0 = if amount0_requested == MAX_UINT128 || amount0_requested > position.tokens_owed_0
    {
        position.tokens_owed_0
    } else {
        amount0_requested
    };

    let amount1 = if amount1_requested == MAX_UINT128 || amount1_requested > position.tokens_owed_1
    {
        position.tokens_owed_1
    } else {
        amount1_requested
    };

    if amount0.is_zero() && amount1.is_zero() {
        return Ok(Response::new()
            .add_attribute("action", "collect")
            .add_attribute("owner", info.sender.as_str())
            .add_attribute("recipient", &recipient)
            .add_attribute("lower_tick", lower_tick.to_string())
            .add_attribute("upper_tick", upper_tick.to_string())
            .add_attribute("amount0", "0")
            .add_attribute("amount1", "0"));
    }

    // 2. Decrement Owed
    position.tokens_owed_0 -= amount0;
    position.tokens_owed_1 -= amount1;
    POSITIONS.save(deps.storage, key, &position)?;

    // 3. Send Tokens (handles both native and CW20)
    let mut messages: Vec<CosmosMsg> = vec![];
    if !amount0.is_zero() {
        messages.push(config.token0.transfer_msg(&recipient, amount0)?);
    }
    if !amount1.is_zero() {
        messages.push(config.token1.transfer_msg(&recipient, amount1)?);
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "collect")
        .add_attribute("owner", info.sender.as_str())
        .add_attribute("recipient", &recipient)
        .add_attribute("lower_tick", lower_tick.to_string())
        .add_attribute("upper_tick", upper_tick.to_string())
        .add_attribute("amount0", amount0)
        .add_attribute("amount1", amount1))
}

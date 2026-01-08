#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult, Uint128,
};
use cw2::set_contract_version;

use crate::actions::burn::execute_burn;
use crate::actions::collect::execute_collect;
use crate::actions::mint::execute_mint;
use crate::actions::swap::execute_swap;
use crate::core::oracle::initialize_oracle;
use crate::error::ContractError;
use crate::state::{PoolConfig, POOL_CONFIG, POOL_STATE, TICKS};
use choice_clmm_common::pool::{ExecuteMsg, InstantiateMsg, PoolState, QueryMsg};
use choice_clmm_math::tick_math::{get_tick_at_sqrt_ratio, MAX_TICK, MIN_TICK};

// Version info for migration info
const CONTRACT_NAME: &str = "crates.io:choice-clmm-pool";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // 1. Validate Token Order
    if msg.token0 >= msg.token1 {
        return Err(ContractError::InvalidTokenOrder {});
    }

    // 2. Store Configuration
    let config = PoolConfig {
        factory: info.sender.clone(),
        token0: msg.token0,
        token1: msg.token1,
        tick_spacing: msg.tick_spacing,
        fee_config: msg.fee_config,
    };
    POOL_CONFIG.save(deps.storage, &config)?;

    // 3. Initialize Slot0 (The Global State)

    // Calculate the initial tick from the provided square root price
    let current_tick = get_tick_at_sqrt_ratio(msg.initial_sqrt_price)
        .map_err(|_| ContractError::Std(StdError::generic_err("Invalid initial price")))?;

    // Validate the calculated tick is within protocol bounds
    if !(MIN_TICK..=MAX_TICK).contains(&current_tick) {
        return Err(ContractError::Std(StdError::generic_err(
            "Initial tick out of bounds",
        )));
    }

    let slot0 = PoolState {
        sqrt_price: msg.initial_sqrt_price,
        tick: current_tick,
        liquidity: Uint128::zero(),
    };
    POOL_STATE.save(deps.storage, &slot0)?;

    initialize_oracle(
        deps.storage,
        _env.block.time.seconds(),
        msg.initial_sqrt_price,
    )?;

    Ok(Response::new()
        .add_attribute("method", "instantiate")
        .add_attribute("token0", config.token0)
        .add_attribute("token1", config.token1)
        .add_attribute("initial_tick", current_tick.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Mint {
            recipient,
            lower_tick,
            upper_tick,
            amount,
            ..
        } => execute_mint(
            deps,
            env,
            info,
            recipient,
            lower_tick,
            upper_tick,
            amount.u128(),
        ),
        ExecuteMsg::Swap {
            recipient,
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96,
        } => execute_swap(
            deps,
            env,
            info,
            recipient,
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96,
        ),
        ExecuteMsg::Burn {
            lower_tick,
            upper_tick,
            amount,
        } => execute_burn(deps, env, info, lower_tick, upper_tick, amount),
        // NEW
        ExecuteMsg::Collect {
            recipient,
            lower_tick,
            upper_tick,
            amount0_requested,
            amount1_requested,
        } => execute_collect(
            deps,
            info,
            recipient,
            lower_tick,
            upper_tick,
            amount0_requested,
            amount1_requested,
        ),
        _ => Ok(Response::default()), // Placeholder
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetConfig {} => to_json_binary(&POOL_CONFIG.load(deps.storage)?),
        QueryMsg::GetSlot0 {} => to_json_binary(&POOL_STATE.load(deps.storage)?),
        QueryMsg::GetTickInfo { tick } => {
            let info = TICKS.may_load(deps.storage, tick)?.unwrap_or_default();
            to_json_binary(&info)
        }
    }
}

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdError,
    StdResult, Uint128,
};
use cw2::set_contract_version;
use cw20::Cw20ReceiveMsg;

use crate::actions::burn::execute_burn;
use crate::actions::collect::execute_collect;
use crate::actions::mint::execute_mint;
use crate::actions::swap::{
    execute_swap, execute_swap_exact_input, execute_swap_exact_input_cw20, query_quote,
};
use crate::core::oracle::initialize_oracle;
use crate::error::ContractError;
use crate::state::{PoolConfig, POOL_CONFIG, POOL_STATE, TICKS};
use choice_clmm_common::pool::{
    Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, PoolState, QueryMsg,
};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::tick_math::{get_tick_at_sqrt_ratio, MAX_TICK, MIN_TICK};

// Version info for migration info
const CONTRACT_NAME: &str = "crates.io:choice-clmm-pool";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // 1. Validate Token Order
    if msg.token0 >= msg.token1 {
        return Err(ContractError::InvalidTokenOrder {});
    }

    // Validate CW20 addresses
    if let AssetInfo::Token { contract_addr } = &msg.token0 {
        deps.api.addr_validate(contract_addr)?;
    }
    if let AssetInfo::Token { contract_addr } = &msg.token1 {
        deps.api.addr_validate(contract_addr)?;
    }

    // 2. Validate tick_spacing and fee_config
    if msg.tick_spacing == 0 {
        return Err(ContractError::InvalidConfig {
            reason: "tick_spacing must be > 0".to_string(),
        });
    }
    if msg.fee_config.base_fee_ppm >= 1_000_000 {
        return Err(ContractError::InvalidConfig {
            reason: "base_fee_ppm must be < 1_000_000".to_string(),
        });
    }
    if msg.fee_config.max_fee_ppm >= 1_000_000 {
        return Err(ContractError::InvalidConfig {
            reason: "max_fee_ppm must be < 1_000_000".to_string(),
        });
    }
    if msg.fee_config.base_fee_ppm > msg.fee_config.max_fee_ppm {
        return Err(ContractError::InvalidConfig {
            reason: "base_fee_ppm must be <= max_fee_ppm".to_string(),
        });
    }
    if msg.fee_config.ema_halflife_seconds == 0 {
        return Err(ContractError::InvalidConfig {
            reason: "ema_halflife_seconds must be > 0".to_string(),
        });
    }

    // 3. Store Configuration
    let config = PoolConfig {
        factory: info.sender.clone(),
        token0: msg.token0,
        token1: msg.token1,
        tick_spacing: msg.tick_spacing,
        fee_config: msg.fee_config,
    };
    POOL_CONFIG.save(deps.storage, &config)?;

    // 4. Initialize Slot0 (The Global State)
    let current_tick = get_tick_at_sqrt_ratio(msg.initial_sqrt_price)
        .map_err(|_| ContractError::Std(StdError::generic_err("Invalid initial price")))?;

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
        env.block.time.seconds(),
        msg.initial_sqrt_price,
    )?;

    Ok(Response::new()
        .add_attribute("method", "instantiate")
        .add_attribute("token0", config.token0.to_string())
        .add_attribute("token1", config.token1.to_string())
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
        ExecuteMsg::SwapExactInput {
            minimum_amount_out,
            recipient,
            deadline,
        } => execute_swap_exact_input(deps, env, info, minimum_amount_out, recipient, deadline),
        ExecuteMsg::Burn {
            lower_tick,
            upper_tick,
            amount,
        } => execute_burn(deps, env, info, lower_tick, upper_tick, amount),
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
        ExecuteMsg::Receive(cw20_msg) => receive_cw20(deps, env, info, cw20_msg),
    }
}

fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;

    // Verify sender is one of the pool's CW20 tokens
    let sender_asset = AssetInfo::Token {
        contract_addr: info.sender.to_string(),
    };
    let zero_for_one = if sender_asset == config.token0 {
        true
    } else if sender_asset == config.token1 {
        false
    } else {
        return Err(ContractError::Unauthorized {});
    };

    let original_sender = deps.api.addr_validate(&cw20_msg.sender)?;

    match from_json(&cw20_msg.msg)? {
        Cw20HookMsg::SwapExactInput {
            minimum_amount_out,
            recipient,
            deadline,
        } => execute_swap_exact_input_cw20(
            deps,
            env,
            original_sender,
            cw20_msg.amount,
            zero_for_one,
            minimum_amount_out,
            recipient,
            deadline,
        ),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    let version = cw2::get_contract_version(deps.storage)?;
    if version.contract != CONTRACT_NAME {
        return Err(ContractError::Std(StdError::generic_err(
            "Cannot migrate from different contract",
        )));
    }
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", version.version)
        .add_attribute("to_version", CONTRACT_VERSION))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetConfig {} => to_json_binary(&POOL_CONFIG.load(deps.storage)?),
        QueryMsg::GetSlot0 {} => to_json_binary(&POOL_STATE.load(deps.storage)?),
        QueryMsg::GetTickInfo { tick } => {
            let info = TICKS.may_load(deps.storage, tick)?.unwrap_or_default();
            to_json_binary(&info)
        }
        QueryMsg::Quote {
            token_in,
            amount_in,
        } => {
            let resp = query_quote(deps, env, token_in, amount_in)
                .map_err(|e| StdError::generic_err(e.to_string()))?;
            to_json_binary(&resp)
        }
    }
}

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, from_json, to_json_binary, Addr, BankMsg, Binary, CanonicalAddr, CosmosMsg, Decimal,
    Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult, Uint128, WasmMsg,
};

use choice::asset::AssetInfo;

use choice::staking::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg,
    StakerInfoResponse, StateResponse,
};

use crate::state::{
    read_config, read_staker_info, read_state, remove_staker_info, store_config, store_staker_info,
    store_state, Config, StakerInfo, State,
};

use cw2::{get_contract_version, set_contract_version};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use std::collections::BTreeMap;

const CONTRACT_NAME: &str = "crates.io:choice-farm";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    validate_asset_info(deps.as_ref(), &msg.reward_token)?;
    validate_asset_info(deps.as_ref(), &msg.staking_token)?;
    validate_distribution_schedule(&msg.distribution_schedule)?;

    store_config(
        deps.storage,
        &Config {
            owner: deps.api.addr_canonicalize(info.sender.as_str())?,
            reward_token: msg.reward_token,
            staking_token: msg.staking_token,
            distribution_schedule: msg.distribution_schedule,
        },
    )?;

    store_state(
        deps.storage,
        &State {
            last_distributed: env.block.time.seconds(),
            total_bond_amount: Uint128::zero(),
            global_reward_index: Decimal::zero(),
            undistributed_rewards: Uint128::zero(),
        },
    )?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::Bond { amount } => {
            let config: Config = read_config(deps.storage)?;
            if let AssetInfo::NativeToken { ref denom } = config.staking_token {
                if info.funds.len() != 1 {
                    return Err(StdError::generic_err(
                        "Only the staking token is accepted for bonding",
                    ));
                }
                let received_coin = &info.funds[0];
                if received_coin.denom != *denom || received_coin.amount != amount {
                    return Err(StdError::generic_err(format!(
                        "Incorrect native token sent. Expected {} {}, got {} {}",
                        amount, denom, received_coin.amount, received_coin.denom
                    )));
                }
                bond(deps, env, info.sender.clone(), amount)
            } else {
                Err(StdError::generic_err(
                    "Cannot call bond directly with non native tokens",
                ))
            }
        }
        ExecuteMsg::Unbond { amount } => unbond(deps, env, info, amount),
        ExecuteMsg::Withdraw {} => withdraw(deps, env, info),
        ExecuteMsg::Fund {} => fund_native(deps, env, info),
        ExecuteMsg::MigrateStaking {
            new_staking_contract,
        } => migrate_staking(deps, env, info, new_staking_contract),
        ExecuteMsg::UpdateConfig {
            distribution_schedule,
        } => update_config(deps, env, info, distribution_schedule),
    }
}

pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;

    match from_json(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Bond {}) => {
            match config.staking_token {
                AssetInfo::Token { ref contract_addr } => {
                    if deps.api.addr_validate(contract_addr)? != info.sender {
                        return Err(StdError::generic_err("unauthorized"));
                    }
                }
                AssetInfo::NativeToken { ref denom } => {
                    return Err(StdError::generic_err(format!(
                        "staking token is native: {}",
                        denom
                    )));
                }
            }

            let cw20_sender = deps.api.addr_validate(&cw20_msg.sender)?;
            bond(deps, env, cw20_sender, cw20_msg.amount)
        }
        Ok(Cw20HookMsg::Fund {}) => {
            match config.reward_token {
                AssetInfo::Token { ref contract_addr } => {
                    if deps.api.addr_validate(contract_addr)? != info.sender {
                        return Err(StdError::generic_err("unauthorized"));
                    }
                }
                AssetInfo::NativeToken { ref denom } => {
                    return Err(StdError::generic_err(format!(
                        "reward token is native: {}",
                        denom
                    )));
                }
            }
            fund(deps, cw20_msg.amount, &cw20_msg.sender)
        }
        Err(_) => Err(StdError::generic_err("data should be given")),
    }
}

pub fn bond(deps: DepsMut, env: Env, sender_addr: Addr, amount: Uint128) -> StdResult<Response> {
    if amount.is_zero() {
        return Err(StdError::generic_err("Cannot bond zero amount"));
    }

    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(sender_addr.as_str())?;

    let config: Config = read_config(deps.storage)?;
    let mut state: State = read_state(deps.storage)?;
    let mut staker_info: StakerInfo = read_staker_info(deps.storage, &sender_addr_raw)?;

    compute_reward(&config, &mut state, env.block.time.seconds());
    compute_staker_reward(&state, &mut staker_info)?;

    increase_bond_amount(&mut state, &mut staker_info, amount);

    store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    store_state(deps.storage, &state)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "bond"),
        ("owner", sender_addr.as_str()),
        ("amount", amount.to_string().as_str()),
    ]))
}

pub fn unbond(deps: DepsMut, env: Env, info: MessageInfo, amount: Uint128) -> StdResult<Response> {
    if amount.is_zero() {
        return Err(StdError::generic_err("Cannot unbond zero amount"));
    }

    let config: Config = read_config(deps.storage)?;
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;

    let mut state: State = read_state(deps.storage)?;
    let mut staker_info: StakerInfo = read_staker_info(deps.storage, &sender_addr_raw)?;

    if staker_info.bond_amount < amount {
        return Err(StdError::generic_err("Cannot unbond more than bond amount"));
    }

    compute_reward(&config, &mut state, env.block.time.seconds());
    compute_staker_reward(&state, &mut staker_info)?;

    decrease_bond_amount(&mut state, &mut staker_info, amount)?;

    if staker_info.pending_reward.is_zero() && staker_info.bond_amount.is_zero() {
        remove_staker_info(deps.storage, &sender_addr_raw);
    } else {
        store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    }

    store_state(deps.storage, &state)?;

    let unbond_msg = match config.staking_token {
        AssetInfo::Token { ref contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: info.sender.to_string(),
                amount,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { ref denom } => CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(amount.u128(), denom),
        }),
    };

    Ok(Response::new()
        .add_messages(vec![unbond_msg])
        .add_attributes(vec![
            ("action", "unbond"),
            ("owner", info.sender.as_str()),
            ("amount", amount.to_string().as_str()),
        ]))
}

pub fn withdraw(deps: DepsMut, env: Env, info: MessageInfo) -> StdResult<Response> {
    let sender_addr_raw = deps.api.addr_canonicalize(info.sender.as_str())?;

    let config: Config = read_config(deps.storage)?;
    let mut state: State = read_state(deps.storage)?;
    let mut staker_info = read_staker_info(deps.storage, &sender_addr_raw)?;

    compute_reward(&config, &mut state, env.block.time.seconds());
    compute_staker_reward(&state, &mut staker_info)?;

    let amount = staker_info.pending_reward;
    if amount.is_zero() {
        return Err(StdError::generic_err("Nothing to withdraw"));
    }
    staker_info.pending_reward = Uint128::zero();

    if staker_info.bond_amount.is_zero() {
        remove_staker_info(deps.storage, &sender_addr_raw);
    } else {
        store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    }

    store_state(deps.storage, &state)?;

    let reward_msg: CosmosMsg = match config.reward_token {
        AssetInfo::Token { ref contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: info.sender.to_string(),
                amount,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { ref denom } => CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(amount.u128(), denom),
        }),
    };

    Ok(Response::new()
        .add_messages(vec![reward_msg])
        .add_attributes(vec![
            ("action", "withdraw"),
            ("owner", info.sender.as_str()),
            ("amount", amount.to_string().as_str()),
        ]))
}

pub fn fund_native(deps: DepsMut, _env: Env, info: MessageInfo) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;
    let denom = match config.reward_token {
        AssetInfo::NativeToken { denom } => denom,
        AssetInfo::Token { contract_addr } => {
            return Err(StdError::generic_err(format!(
                "reward token is cw20: {}. Fund via Cw20 Send hook.",
                contract_addr
            )));
        }
    };

    if info.funds.len() != 1 {
        return Err(StdError::generic_err(
            "Exactly one native coin (the reward token) must be sent with Fund",
        ));
    }
    let received = &info.funds[0];
    if received.denom != denom {
        return Err(StdError::generic_err(format!(
            "Incorrect fund denom. Expected {}, got {}",
            denom, received.denom
        )));
    }

    fund(deps, received.amount, info.sender.as_str())
}

fn fund(deps: DepsMut, amount: Uint128, funder: &str) -> StdResult<Response> {
    if amount.is_zero() {
        return Err(StdError::generic_err("Cannot fund zero amount"));
    }

    let mut state: State = read_state(deps.storage)?;
    state.undistributed_rewards = state.undistributed_rewards.checked_add(amount)?;
    store_state(deps.storage, &state)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "fund"),
        ("funder", funder),
        ("amount", amount.to_string().as_str()),
    ]))
}

pub fn update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    distribution_schedule: Vec<(u64, u64, Uint128)>,
) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;

    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    validate_distribution_schedule(&distribution_schedule)?;

    // Flush pending rewards into global_reward_index so assert_new_schedules
    // compares the new schedule against the current real block time, not a
    // stale last_distributed. Without this, the owner could retroactively add
    // or remove slots in the window (last_distributed, now].
    let mut state: State = read_state(deps.storage)?;
    compute_reward(&config, &mut state, env.block.time.seconds());
    store_state(deps.storage, &state)?;

    assert_new_schedules(&config, &state, distribution_schedule.clone())?;

    let new_config = Config {
        owner: config.owner,
        reward_token: config.reward_token,
        staking_token: config.staking_token,
        distribution_schedule,
    };
    store_config(deps.storage, &new_config)?;

    Ok(Response::new().add_attributes(vec![("action", "update_config")]))
}

pub fn migrate_staking(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_staking_contract: String,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let config: Config = read_config(deps.storage)?;
    let mut state: State = read_state(deps.storage)?;

    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let new_staking_contract = deps.api.addr_validate(&new_staking_contract)?.into_string();

    // Flush any pending credits first, then forward the entire undistributed
    // pool. No more schedule re-math: `undistributed_rewards` is the source of
    // truth for what hasn't been credited. Tokens backing already-credited
    // pending rewards stay in the contract so stakers can still withdraw.
    compute_reward(&config, &mut state, env.block.time.seconds());

    let remaining_tokens = state.undistributed_rewards;
    state.undistributed_rewards = Uint128::zero();
    store_state(deps.storage, &state)?;

    let mut response = Response::new().add_attributes(vec![
        ("action", "migrate_staking"),
        ("remaining_amount", remaining_tokens.to_string().as_str()),
    ]);

    if !remaining_tokens.is_zero() {
        let reward_token_msg = match config.reward_token {
            AssetInfo::Token { ref contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: new_staking_contract,
                    amount: remaining_tokens,
                })?,
                funds: vec![],
            }),
            AssetInfo::NativeToken { ref denom } => CosmosMsg::Bank(BankMsg::Send {
                to_address: new_staking_contract,
                amount: coins(remaining_tokens.u128(), denom),
            }),
        };
        response = response.add_message(reward_token_msg);
    }

    Ok(response)
}

fn increase_bond_amount(state: &mut State, staker_info: &mut StakerInfo, amount: Uint128) {
    state.total_bond_amount += amount;
    staker_info.bond_amount += amount;
}

fn decrease_bond_amount(
    state: &mut State,
    staker_info: &mut StakerInfo,
    amount: Uint128,
) -> StdResult<()> {
    state.total_bond_amount = state.total_bond_amount.checked_sub(amount)?;
    staker_info.bond_amount = staker_info.bond_amount.checked_sub(amount)?;
    Ok(())
}

// compute distributed rewards and update global reward index
fn compute_reward(config: &Config, state: &mut State, block_time: u64) {
    // Safe no-op for historical queries: compute_reward is called with a
    // user-supplied block_time from query endpoints, and an earlier time would
    // underflow the passed_time subtraction below.
    if block_time <= state.last_distributed {
        return;
    }

    if state.total_bond_amount.is_zero() {
        // Nobody is staking; skip crediting. undistributed_rewards is left
        // untouched so the tokens remain in the pool for future stakers or
        // for `migrate_staking` to forward.
        state.last_distributed = block_time;
        return;
    }

    let mut theoretical: Uint128 = Uint128::zero();
    for s in config.distribution_schedule.iter() {
        if s.0 >= block_time || s.1 <= state.last_distributed {
            continue;
        }

        // Schedule is validated: s.0 < s.1, so (s.1 - s.0) is always non-zero
        // and these min/max subtractions can never underflow.
        let passed_time =
            std::cmp::min(s.1, block_time) - std::cmp::max(s.0, state.last_distributed);

        let time = s.1 - s.0;
        // multiply_ratio computes floor(s.2 * passed_time / time) in one step,
        // avoiding the double-floor precision loss of
        //   passed_time * Decimal::from_ratio(s.2, time).
        theoretical += s.2.multiply_ratio(passed_time, time);
    }

    // The actual amount distributed is capped by what's been funded. This is
    // the solvency invariant: we never credit rewards the contract cannot pay.
    // Any shortfall vs. the schedule is lost for that window — the owner must
    // fund ahead of time.
    let distributed = std::cmp::min(theoretical, state.undistributed_rewards);

    state.last_distributed = block_time;
    if !distributed.is_zero() {
        state.undistributed_rewards -= distributed;
        state.global_reward_index +=
            Decimal::from_ratio(distributed, state.total_bond_amount);
    }
}

// withdraw reward to pending reward
fn compute_staker_reward(state: &State, staker_info: &mut StakerInfo) -> StdResult<()> {
    // Compute the per-user reward as bond * (global_index - user_index).
    // The earlier formulation `floor(bond * global) - floor(bond * user)`
    // accumulated a floor-truncation error each call.
    let index_delta = state
        .global_reward_index
        .checked_sub(staker_info.reward_index)
        .map_err(|e| StdError::generic_err(e.to_string()))?;
    let pending_reward = staker_info.bond_amount.mul_floor(index_delta);

    staker_info.reward_index = state.global_reward_index;
    staker_info.pending_reward += pending_reward;
    Ok(())
}

fn validate_asset_info(deps: Deps, asset: &AssetInfo) -> StdResult<()> {
    match asset {
        AssetInfo::Token { contract_addr } => {
            deps.api.addr_validate(contract_addr)?;
            Ok(())
        }
        AssetInfo::NativeToken { denom } => {
            if denom.is_empty() {
                Err(StdError::generic_err("empty native denom"))
            } else {
                Ok(())
            }
        }
    }
}

fn validate_distribution_schedule(schedule: &[(u64, u64, Uint128)]) -> StdResult<()> {
    for (start, end, amount) in schedule {
        if start >= end {
            return Err(StdError::generic_err(
                "distribution schedule: start must be strictly less than end",
            ));
        }
        if amount.is_zero() {
            return Err(StdError::generic_err(
                "distribution schedule: amount must be non-zero",
            ));
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::State { block_time } => to_json_binary(&query_state(deps, block_time)?),
        QueryMsg::StakerInfo { staker, block_time } => {
            to_json_binary(&query_staker_info(deps, staker, block_time)?)
        }
    }
}

pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = read_config(deps.storage)?;

    let reward_token_str = match config.reward_token {
        AssetInfo::Token { ref contract_addr } => contract_addr.clone(),
        AssetInfo::NativeToken { ref denom } => denom.clone(),
    };

    let staking_token_str = match config.staking_token {
        AssetInfo::Token { ref contract_addr } => contract_addr.clone(),
        AssetInfo::NativeToken { ref denom } => denom.clone(),
    };

    let resp = ConfigResponse {
        reward_token: reward_token_str,
        staking_token: staking_token_str,
        distribution_schedule: config.distribution_schedule,
    };

    Ok(resp)
}

pub fn query_state(deps: Deps, block_time: Option<u64>) -> StdResult<StateResponse> {
    let mut state: State = read_state(deps.storage)?;
    if let Some(block_time) = block_time {
        let config = read_config(deps.storage)?;
        compute_reward(&config, &mut state, block_time);
    }

    Ok(StateResponse {
        last_distributed: state.last_distributed,
        total_bond_amount: state.total_bond_amount,
        global_reward_index: state.global_reward_index,
        undistributed_rewards: state.undistributed_rewards,
    })
}

pub fn query_staker_info(
    deps: Deps,
    staker: String,
    block_time: Option<u64>,
) -> StdResult<StakerInfoResponse> {
    let staker_raw = deps.api.addr_canonicalize(&staker)?;

    let mut staker_info: StakerInfo = read_staker_info(deps.storage, &staker_raw)?;
    if let Some(block_time) = block_time {
        let config = read_config(deps.storage)?;
        let mut state = read_state(deps.storage)?;

        compute_reward(&config, &mut state, block_time);
        compute_staker_reward(&state, &mut staker_info)?;
    }

    Ok(StakerInfoResponse {
        staker,
        reward_index: staker_info.reward_index,
        bond_amount: staker_info.bond_amount,
        pending_reward: staker_info.pending_reward,
    })
}

pub fn assert_new_schedules(
    config: &Config,
    state: &State,
    distribution_schedule: Vec<(u64, u64, Uint128)>,
) -> StdResult<()> {
    if distribution_schedule.len() < config.distribution_schedule.len() {
        return Err(StdError::generic_err(
            "cannot update; the new schedule must support all of the previous schedule",
        ));
    }

    let mut existing_counts: BTreeMap<(u64, u64, Uint128), u32> = BTreeMap::new();
    for schedule in config.distribution_schedule.clone() {
        let counter = existing_counts.entry(schedule).or_insert(0);
        *counter += 1;
    }

    let mut new_counts: BTreeMap<(u64, u64, Uint128), u32> = BTreeMap::new();
    for schedule in distribution_schedule {
        let counter = new_counts.entry(schedule).or_insert(0);
        *counter += 1;
    }

    for (schedule, count) in existing_counts.into_iter() {
        // if began ensure its in the new schedule
        if schedule.0 <= state.last_distributed {
            if count > *new_counts.get(&schedule).unwrap_or(&0u32) {
                return Err(StdError::generic_err(
                    "new schedule removes already started distribution",
                ));
            }
            // after this new_counts will only contain the newly added schedules
            *new_counts.get_mut(&schedule).unwrap() -= count;
        }
    }

    for (schedule, count) in new_counts.into_iter() {
        if count > 0 && schedule.0 <= state.last_distributed {
            return Err(StdError::generic_err(
                "new schedule adds an already started distribution",
            ));
        }
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    let stored = get_contract_version(deps.storage)?;
    if stored.contract != CONTRACT_NAME {
        return Err(StdError::generic_err(format!(
            "cannot migrate: expected contract {}, found {}",
            CONTRACT_NAME, stored.contract
        )));
    }
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", stored.version)
        .add_attribute("to_version", CONTRACT_VERSION))
}

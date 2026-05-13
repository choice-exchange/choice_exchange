#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, from_json, to_json_binary, Addr, BankMsg, Binary, CanonicalAddr, CosmosMsg, Decimal,
    Deps, DepsMut, Env, MessageInfo, Response, StdError, StdResult, Uint128, WasmMsg,
};

use choice::asset::AssetInfo;

use choice::staking::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg,
    PendingConfigUpdateResponse, PendingMigrationResponse, PendingOwnerRotationResponse, QueryMsg,
    StakerInfoResponse, StateResponse,
};

use crate::state::{
    read_config, read_staker_info, read_state, remove_staker_info, store_config, store_staker_info,
    store_state, Config, PendingConfigUpdate, PendingMigration, StakerInfo, State,
    LAST_SEEN_CW20_BALANCE, MAX_SCHEDULE_SLOTS, MAX_SCHEDULE_SLOT_DURATION_SECONDS,
    PENDING_CONFIG_UPDATE, PENDING_MIGRATION, TIMELOCK_DELAY_SECONDS,
};

use cw2::{get_contract_version, set_contract_version};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use std::collections::BTreeMap;

const CONTRACT_NAME: &str = "crates.io:choice-farm";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Deployment hygiene: the wasm `admin` address on this contract instance
/// should be the same Choice governance multisig that is passed as `msg.owner`.
/// An unrestricted admin can call `MigrateContract` to swap the code ID out
/// from under users, which bypasses the `ProposeMigrateStaking` timelock below.
/// The factory enforces both (admin and Config.owner are set to the same
/// configured governance address). When instantiating directly outside the
/// factory, the deployer is responsible for matching them.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let owner = deps.api.addr_validate(&msg.owner)?;
    validate_asset_info(deps.as_ref(), &msg.reward_token)?;
    validate_asset_info(deps.as_ref(), &msg.staking_token)?;
    validate_distribution_schedule(&msg.distribution_schedule)?;

    let (reward_token_kind, reward_token_addr) = asset_info_event_parts(&msg.reward_token);
    let (staking_token_kind, staking_token_addr) = asset_info_event_parts(&msg.staking_token);
    let (start_time, end_time, total_reward) = distribution_schedule_summary(&msg.distribution_schedule)?;

    // Accept reward pre-funding atomically with creation. Two valid shapes:
    //   1) native reward + info.funds == [{reward_denom: total_reward}] → credit now
    //   2) info.funds empty → credit later via Fund{} (factory's reply, or hand-call)
    // CW20 rewards must use shape 2 — funds attached to instantiate can't be a CW20.
    // Any other shape (wrong denom, wrong amount, multiple coins, funds-with-cw20)
    // is rejected here so stranded-balance bugs can't slip through.
    let undistributed_rewards = match &msg.reward_token {
        AssetInfo::NativeToken { denom } => match info.funds.len() {
            0 => Uint128::zero(),
            1 => {
                let c = &info.funds[0];
                if c.denom != *denom {
                    return Err(StdError::generic_err(format!(
                        "instantiate funds denom mismatch: expected {}, got {}",
                        denom, c.denom
                    )));
                }
                if c.amount != total_reward {
                    return Err(StdError::generic_err(format!(
                        "instantiate funds amount mismatch: expected {} (total_reward), got {}",
                        total_reward, c.amount
                    )));
                }
                total_reward
            }
            _ => {
                return Err(StdError::generic_err(
                    "instantiate accepts at most one coin (the reward token)",
                ));
            }
        },
        AssetInfo::Token { .. } => {
            if !info.funds.is_empty() {
                return Err(StdError::generic_err(
                    "cw20 reward farms must be instantiated with empty funds; \
                     funding is delivered via the Cw20 Send→Fund hook",
                ));
            }
            Uint128::zero()
        }
    };

    store_config(
        deps.storage,
        &Config {
            owner: deps.api.addr_canonicalize(owner.as_str())?,
            reward_token: msg.reward_token,
            staking_token: msg.staking_token,
            distribution_schedule: msg.distribution_schedule,
            pending_owner: None,
            pending_owner_effective_at: None,
        },
    )?;

    store_state(
        deps.storage,
        &State {
            last_distributed: env.block.time.seconds(),
            total_bond_amount: Uint128::zero(),
            global_reward_index: Decimal::zero(),
            undistributed_rewards,
            unclaimed_pending: Uint128::zero(),
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate_farm")
        .add_attribute("instantiator", info.sender.as_str())
        .add_attribute("owner", owner.as_str())
        .add_attribute("reward_token", reward_token_addr)
        .add_attribute("reward_token_kind", reward_token_kind)
        .add_attribute("staking_token", staking_token_addr)
        .add_attribute("staking_token_kind", staking_token_kind)
        .add_attribute("start_time", start_time.to_string())
        .add_attribute("end_time", end_time.to_string())
        .add_attribute("total_reward", total_reward.to_string())
        .add_attribute("undistributed_rewards", undistributed_rewards.to_string()))
}

fn asset_info_event_parts(info: &AssetInfo) -> (&'static str, String) {
    match info {
        AssetInfo::NativeToken { denom } => ("native", denom.clone()),
        AssetInfo::Token { contract_addr } => ("cw20", contract_addr.clone()),
    }
}

fn distribution_schedule_summary(schedule: &[(u64, u64, Uint128)]) -> StdResult<(u64, u64, Uint128)> {
    let start = schedule.iter().map(|(s, _, _)| *s).min().unwrap_or(0);
    let end = schedule.iter().map(|(_, e, _)| *e).max().unwrap_or(0);
    let total = schedule
        .iter()
        .try_fold(Uint128::zero(), |acc, (_, _, amt)| acc.checked_add(*amt))?;
    Ok((start, end, total))
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
        ExecuteMsg::ProposeMigrateStaking {
            new_staking_contract,
        } => propose_migrate_staking(deps, env, info, new_staking_contract),
        ExecuteMsg::ApplyMigrateStaking {} => apply_migrate_staking(deps, env, info),
        ExecuteMsg::CancelMigrateStakingProposal {} => cancel_migrate_staking_proposal(deps, info),
        ExecuteMsg::ProposeUpdateConfig {
            distribution_schedule,
        } => propose_update_config(deps, env, info, distribution_schedule),
        ExecuteMsg::ApplyUpdateConfig {} => apply_update_config(deps, env, info),
        ExecuteMsg::CancelUpdateConfigProposal {} => cancel_update_config_proposal(deps, info),
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            propose_new_owner(deps, env, info, new_owner)
        }
        ExecuteMsg::ApplyOwnerRotation {} => apply_owner_rotation(deps, env, info),
        ExecuteMsg::CancelOwnerProposal {} => cancel_owner_proposal(deps, info),
    }
}

pub fn receive_cw20(
    mut deps: DepsMut,
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

            // M-1: verify the CW20 actually moved the tokens it claims to have
            // before crediting the bond.
            reconcile_cw20_receive(
                deps.branch(),
                &env,
                info.sender.as_str(),
                cw20_msg.amount,
            )?;

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
            // M-1: same reconcile for Fund flow.
            reconcile_cw20_receive(
                deps.branch(),
                &env,
                info.sender.as_str(),
                cw20_msg.amount,
            )?;
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
    compute_staker_reward(&mut state, &mut staker_info)?;

    increase_bond_amount(&mut state, &mut staker_info, amount);

    store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    store_state(deps.storage, &state)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "bond"),
        ("owner", sender_addr.as_str()),
        ("amount", amount.to_string().as_str()),
    ]))
}

pub fn unbond(mut deps: DepsMut, env: Env, info: MessageInfo, amount: Uint128) -> StdResult<Response> {
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
    compute_staker_reward(&mut state, &mut staker_info)?;

    decrease_bond_amount(&mut state, &mut staker_info, amount)?;

    if staker_info.pending_reward.is_zero() && staker_info.bond_amount.is_zero() {
        remove_staker_info(deps.storage, &sender_addr_raw);
    } else {
        store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    }

    store_state(deps.storage, &state)?;

    let unbond_msg = match config.staking_token {
        AssetInfo::Token { ref contract_addr } => {
            // M-1: shrink the last-seen ledger to match the post-transfer
            // balance before the message dispatches.
            decrement_last_seen_cw20(deps.branch(), contract_addr, amount)?;
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: info.sender.to_string(),
                    amount,
                })?,
                funds: vec![],
            })
        }
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

pub fn withdraw(mut deps: DepsMut, env: Env, info: MessageInfo) -> StdResult<Response> {
    let sender_addr_raw = deps.api.addr_canonicalize(info.sender.as_str())?;

    let config: Config = read_config(deps.storage)?;
    let mut state: State = read_state(deps.storage)?;
    let mut staker_info = read_staker_info(deps.storage, &sender_addr_raw)?;

    compute_reward(&config, &mut state, env.block.time.seconds());
    compute_staker_reward(&mut state, &mut staker_info)?;

    let amount = staker_info.pending_reward;
    if amount.is_zero() {
        return Err(StdError::generic_err("Nothing to withdraw"));
    }
    staker_info.pending_reward = Uint128::zero();
    // L-1: shrink the global pending-reserve mirror by what's being paid out.
    state.unclaimed_pending = state.unclaimed_pending.saturating_sub(amount);

    if staker_info.bond_amount.is_zero() {
        remove_staker_info(deps.storage, &sender_addr_raw);
    } else {
        store_staker_info(deps.storage, &sender_addr_raw, &staker_info)?;
    }

    store_state(deps.storage, &state)?;

    let reward_msg: CosmosMsg = match config.reward_token {
        AssetInfo::Token { ref contract_addr } => {
            decrement_last_seen_cw20(deps.branch(), contract_addr, amount)?;
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: info.sender.to_string(),
                    amount,
                })?,
                funds: vec![],
            })
        }
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

/// H-2: owner proposes a new `distribution_schedule`. The actual install is
/// gated behind `apply_update_config`, which requires `TIMELOCK_DELAY_SECONDS`
/// to have elapsed. Re-proposing overwrites the pending proposal and resets
/// the timer.
pub fn propose_update_config(
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

    let effective_at = env.block.time.seconds() + TIMELOCK_DELAY_SECONDS;
    PENDING_CONFIG_UPDATE.save(
        deps.storage,
        &PendingConfigUpdate {
            distribution_schedule,
            effective_at,
        },
    )?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_update_config"),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

/// Owner finalizes a pending schedule update once the timelock has expired.
/// Flushes accrued credits at the OLD schedule first so `assert_new_schedules`
/// compares the new schedule against the current real block time, not a
/// stale `last_distributed`.
pub fn apply_update_config(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let pending = PENDING_CONFIG_UPDATE
        .may_load(deps.storage)?
        .ok_or_else(|| StdError::generic_err("no pending config update"))?;

    if env.block.time.seconds() < pending.effective_at {
        return Err(StdError::generic_err(
            "config update timelock has not elapsed",
        ));
    }

    // Re-validate at apply time — the proposal was validated at propose, but
    // the per-slot duration cap is a safety net so the schedule that lands
    // satisfies the invariant.
    validate_distribution_schedule(&pending.distribution_schedule)?;

    // Flush rewards against the OLD schedule before swapping.
    let mut state: State = read_state(deps.storage)?;
    compute_reward(&config, &mut state, env.block.time.seconds());
    store_state(deps.storage, &state)?;

    assert_new_schedules(&config, &state, pending.distribution_schedule.clone())?;

    let new_config = Config {
        owner: config.owner,
        reward_token: config.reward_token,
        staking_token: config.staking_token,
        distribution_schedule: pending.distribution_schedule,
        pending_owner: config.pending_owner,
        pending_owner_effective_at: config.pending_owner_effective_at,
    };
    store_config(deps.storage, &new_config)?;
    PENDING_CONFIG_UPDATE.remove(deps.storage);

    Ok(Response::new().add_attributes(vec![("action", "apply_update_config")]))
}

/// Owner clears a pending schedule update.
pub fn cancel_update_config_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if PENDING_CONFIG_UPDATE.may_load(deps.storage)?.is_none() {
        return Err(StdError::generic_err("no pending config update"));
    }

    PENDING_CONFIG_UPDATE.remove(deps.storage);

    Ok(Response::new().add_attribute("action", "cancel_update_config_proposal"))
}

/// Owner proposes a migration target. The actual fund-forwarding is gated
/// behind `apply_migrate_staking`, which requires `TIMELOCK_DELAY_SECONDS`
/// to have elapsed. Proposing again overwrites the pending proposal and
/// resets the timer.
pub fn propose_migrate_staking(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_staking_contract: String,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let new_staking_addr = deps.api.addr_validate(&new_staking_contract)?;
    let new_staking_raw = deps.api.addr_canonicalize(new_staking_addr.as_str())?;
    let effective_at = env.block.time.seconds() + TIMELOCK_DELAY_SECONDS;

    PENDING_MIGRATION.save(
        deps.storage,
        &PendingMigration {
            new_staking_contract: new_staking_raw,
            effective_at,
        },
    )?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_migrate_staking"),
        ("new_staking_contract", new_staking_addr.as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

/// Owner finalizes a pending migration. Flushes accrued credits, then
/// forwards every reward-token unit that isn't earmarked for an existing
/// staker claim or for the bonded balance (when the staking and reward
/// tokens are the same denom/contract). This includes `undistributed_rewards`
/// AND any per-call floor dust that compute_reward rolled back. Tokens
/// backing already-credited `pending_reward` stay in the contract so stakers
/// can still `Withdraw`.
pub fn apply_migrate_staking(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let pending = PENDING_MIGRATION
        .may_load(deps.storage)?
        .ok_or_else(|| StdError::generic_err("no pending migration"))?;

    if env.block.time.seconds() < pending.effective_at {
        return Err(StdError::generic_err("migration timelock has not elapsed"));
    }

    let new_staking_contract = deps.api.addr_humanize(&pending.new_staking_contract)?;
    let mut state: State = read_state(deps.storage)?;

    compute_reward(&config, &mut state, env.block.time.seconds());

    // L-1: query the contract's actual reward-token balance and forward
    // everything that isn't reserved for staker withdrawals or bonded stake.
    // This catches the per-call floor-truncation dust that lives in the
    // contract balance but isn't tracked in `undistributed_rewards`.
    let actual_balance = query_self_balance(deps.as_ref(), &env, &config.reward_token)?;
    let reserved_for_bonded = if config.reward_token.equal(&config.staking_token) {
        state.total_bond_amount
    } else {
        Uint128::zero()
    };
    let reserved_for_pending = state.unclaimed_pending;
    let remaining_tokens = actual_balance
        .saturating_sub(reserved_for_bonded)
        .saturating_sub(reserved_for_pending);

    // The schedule's emission pool is fully drained at this point — any
    // future compute_reward calls will see undistributed_rewards == 0 and
    // credit nothing further to the global index.
    state.undistributed_rewards = Uint128::zero();
    store_state(deps.storage, &state)?;
    PENDING_MIGRATION.remove(deps.storage);

    let mut response = Response::new().add_attributes(vec![
        ("action", "apply_migrate_staking"),
        ("new_staking_contract", new_staking_contract.as_str()),
        ("remaining_amount", remaining_tokens.to_string().as_str()),
        // I-1: surface the post-migration UX expectation as a chain attribute
        // so indexers / UIs can flag it: this farm's reward pool has been
        // forwarded; existing stakers must Unbond + Withdraw here to access
        // their bonded tokens and already-credited pending rewards.
        (
            "migration_notice",
            "reward pool forwarded; stakers must Unbond + Withdraw from this \
             farm to access bonded tokens and pending rewards",
        ),
    ]);

    if !remaining_tokens.is_zero() {
        let reward_token_msg = match config.reward_token {
            AssetInfo::Token { ref contract_addr } => {
                decrement_last_seen_cw20(deps.branch(), contract_addr, remaining_tokens)?;
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.clone(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: new_staking_contract.to_string(),
                        amount: remaining_tokens,
                    })?,
                    funds: vec![],
                })
            }
            AssetInfo::NativeToken { ref denom } => CosmosMsg::Bank(BankMsg::Send {
                to_address: new_staking_contract.to_string(),
                amount: coins(remaining_tokens.u128(), denom),
            }),
        };
        response = response.add_message(reward_token_msg);
    }

    Ok(response)
}

/// Query this contract's own balance of the reward token.
fn query_self_balance(deps: Deps, env: &Env, reward_token: &AssetInfo) -> StdResult<Uint128> {
    match reward_token {
        AssetInfo::NativeToken { denom } => {
            let coin = deps
                .querier
                .query_balance(env.contract.address.clone(), denom)?;
            Ok(coin.amount)
        }
        AssetInfo::Token { contract_addr } => query_cw20_self_balance(deps, env, contract_addr),
    }
}

fn query_cw20_self_balance(deps: Deps, env: &Env, cw20_contract: &str) -> StdResult<Uint128> {
    let resp: cw20::BalanceResponse = deps.querier.query_wasm_smart(
        cw20_contract,
        &cw20::Cw20QueryMsg::Balance {
            address: env.contract.address.to_string(),
        },
    )?;
    Ok(resp.balance)
}

/// M-1: verify that the CW20 at `cw20_addr_raw` (the calling contract on a
/// Receive hook) has actually moved at least `claimed_amount` units to this
/// farm since the last seen balance. Returns Ok on a healthy reconcile and
/// updates the stored ledger to the current balance (silently absorbing any
/// out-of-band donation surplus into the contract's untracked balance —
/// preferred over rejecting, which would let an attacker grief by dust-
/// donating before each legitimate Receive).
fn reconcile_cw20_receive(
    deps: DepsMut,
    env: &Env,
    cw20_contract: &str,
    claimed_amount: Uint128,
) -> StdResult<()> {
    let cw20_raw = deps.api.addr_canonicalize(cw20_contract)?;
    let last_seen = LAST_SEEN_CW20_BALANCE
        .may_load(deps.storage, cw20_raw.as_slice())?
        .unwrap_or_default();
    let current = query_cw20_self_balance(deps.as_ref(), env, cw20_contract)?;
    let delta = current.checked_sub(last_seen).map_err(|_| {
        // current < last_seen would mean the CW20 lied about a previous
        // transfer or some external mechanism drained the balance. Either
        // way: refuse this Receive so the staker isn't credited.
        StdError::generic_err("cw20 balance reconcile: current < last_seen")
    })?;
    if delta < claimed_amount {
        return Err(StdError::generic_err(format!(
            "cw20 balance reconcile: claimed {} but only {} arrived",
            claimed_amount, delta
        )));
    }
    LAST_SEEN_CW20_BALANCE.save(deps.storage, cw20_raw.as_slice(), &current)?;
    Ok(())
}

/// M-1: mirror an outgoing Cw20::Transfer of `amount` from this contract by
/// decrementing the last-seen ledger before the message dispatches. The
/// reconcile on the next inbound Receive of this CW20 will then see a delta
/// that correctly equals (new_inbound + any_residual).
fn decrement_last_seen_cw20(
    deps: DepsMut,
    cw20_contract: &str,
    amount: Uint128,
) -> StdResult<()> {
    let cw20_raw = deps.api.addr_canonicalize(cw20_contract)?;
    let last_seen = LAST_SEEN_CW20_BALANCE
        .may_load(deps.storage, cw20_raw.as_slice())?
        .unwrap_or_default();
    LAST_SEEN_CW20_BALANCE.save(
        deps.storage,
        cw20_raw.as_slice(),
        &last_seen.saturating_sub(amount),
    )?;
    Ok(())
}

/// Owner clears a pending migration. No-op if no proposal exists.
pub fn cancel_migrate_staking_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if PENDING_MIGRATION.may_load(deps.storage)?.is_none() {
        return Err(StdError::generic_err("no pending migration"));
    }

    PENDING_MIGRATION.remove(deps.storage);

    Ok(Response::new().add_attribute("action", "cancel_migrate_staking_proposal"))
}

/// Owner proposes a new owner. The rotation cannot take effect until
/// `TIMELOCK_DELAY_SECONDS` have elapsed; the delay is the user exit window.
/// Re-proposing overwrites the pending proposal and resets the timer.
pub fn propose_new_owner(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_owner: String,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let mut config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let new_owner_addr = deps.api.addr_validate(&new_owner)?;
    let new_owner_raw = deps.api.addr_canonicalize(new_owner_addr.as_str())?;
    let effective_at = env.block.time.seconds() + TIMELOCK_DELAY_SECONDS;

    config.pending_owner = Some(new_owner_raw);
    config.pending_owner_effective_at = Some(effective_at);
    store_config(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_new_owner"),
        ("pending_owner", new_owner_addr.as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

/// Owner finalizes a pending owner rotation once the timelock has expired.
pub fn apply_owner_rotation(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let mut config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let pending = config
        .pending_owner
        .take()
        .ok_or_else(|| StdError::generic_err("no pending owner rotation"))?;
    let effective_at = config
        .pending_owner_effective_at
        .take()
        .ok_or_else(|| StdError::generic_err("no pending owner rotation"))?;

    if env.block.time.seconds() < effective_at {
        return Err(StdError::generic_err("owner rotation timelock has not elapsed"));
    }

    let new_owner_addr = deps.api.addr_humanize(&pending)?;
    config.owner = pending;
    store_config(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "apply_owner_rotation"),
        ("new_owner", new_owner_addr.as_str()),
    ]))
}

/// Owner clears a pending owner proposal.
pub fn cancel_owner_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response> {
    let sender_addr_raw: CanonicalAddr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let mut config: Config = read_config(deps.storage)?;
    if sender_addr_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if config.pending_owner.is_none() {
        return Err(StdError::generic_err("no pending owner rotation"));
    }

    config.pending_owner = None;
    config.pending_owner_effective_at = None;
    store_config(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "cancel_owner_proposal"))
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

    // M-2: with no stakers, the schedule effectively pauses. We deliberately
    // do NOT advance `last_distributed` here — emission units that would have
    // accrued during the gap stay in `undistributed_rewards` and get credited
    // to the next compute_reward call once `total_bond_amount > 0`. This means
    // a late-arriving first bonder collects the post-instantiate / post-empty
    // emission window in one sweep, which is acceptable: the schedule's full
    // budget is honored, biased toward whoever shows up.
    if state.total_bond_amount.is_zero() {
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
    let distributed = std::cmp::min(theoretical, state.undistributed_rewards);

    state.last_distributed = block_time;
    if distributed.is_zero() {
        return;
    }

    // H-1: compute the index increment with overflow protection. With a tiny
    // `total_bond_amount`, `distributed / total_bond` can exceed Decimal::MAX
    // (~3.4e20) and panic. We instead cap the increment at the remaining
    // headroom on `global_reward_index`, credit only what fits, and leave the
    // rest in `undistributed_rewards` for a future call (with a healthier
    // ratio, or after a Withdraw resets index_delta).
    let raw_increment =
        Decimal::checked_from_ratio(distributed, state.total_bond_amount)
            .unwrap_or(Decimal::MAX);
    let headroom = Decimal::MAX
        .checked_sub(state.global_reward_index)
        .unwrap_or(Decimal::zero());
    let applied_increment = std::cmp::min(raw_increment, headroom);

    // Convert the applied index increment back to raw token units. mul_floor
    // can only fail for extreme `total_bond * applied_increment` combinations;
    // if it does, fall back to `distributed` (safe upper bound) but cap the
    // deduction so we never decrement below zero.
    let credited = state
        .total_bond_amount
        .checked_mul_floor(applied_increment)
        .unwrap_or(distributed);
    let credited = std::cmp::min(credited, state.undistributed_rewards);

    state.undistributed_rewards -= credited;
    // L-1: `unclaimed_pending` tracks "tokens owed via the schedule" — every
    // unit credited to `global_reward_index` here is implicitly promised to
    // whichever staker bonded during this period, even though it hasn't been
    // swept into any specific user's `pending_reward` yet. Decremented in
    // `withdraw` when a user actually claims.
    state.unclaimed_pending = state.unclaimed_pending.saturating_add(credited);
    state.global_reward_index = state
        .global_reward_index
        .checked_add(applied_increment)
        .unwrap_or(Decimal::MAX);
}

// withdraw reward to pending reward
fn compute_staker_reward(state: &mut State, staker_info: &mut StakerInfo) -> StdResult<()> {
    // Compute the per-user reward as bond * (global_index - user_index).
    let index_delta = state
        .global_reward_index
        .checked_sub(staker_info.reward_index)
        .map_err(|e| StdError::generic_err(e.to_string()))?;

    // L-3: saturate rather than panic on `bond * index_delta` overflow. Only
    // reachable for an extreme combination of a very large `bond_amount` and a
    // long-unsettled index_delta. The user can recover by calling Withdraw
    // (which resets `reward_index`) before the next interaction.
    let pending_reward = staker_info
        .bond_amount
        .checked_mul_floor(index_delta)
        .unwrap_or(Uint128::MAX);

    staker_info.reward_index = state.global_reward_index;
    staker_info.pending_reward = staker_info.pending_reward.saturating_add(pending_reward);
    // No state mutation: this just moves credit from "implicit pending"
    // (captured globally in state.unclaimed_pending via compute_reward) into
    // the user's explicit pending bucket — the total owed is unchanged.
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
    if schedule.is_empty() {
        return Err(StdError::generic_err(
            "distribution schedule must have at least one slot",
        ));
    }
    if schedule.len() > MAX_SCHEDULE_SLOTS {
        return Err(StdError::generic_err(format!(
            "distribution schedule: at most {} slots (got {})",
            MAX_SCHEDULE_SLOTS,
            schedule.len()
        )));
    }
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
        // H-2: bound per-slot duration so even a compromised owner who
        // pushes a propose-update through can't stretch a single slot out
        // to a horizon beyond MAX_SCHEDULE_SLOT_DURATION_SECONDS.
        if end - start > MAX_SCHEDULE_SLOT_DURATION_SECONDS {
            return Err(StdError::generic_err(format!(
                "distribution schedule: slot duration exceeds max ({} seconds)",
                MAX_SCHEDULE_SLOT_DURATION_SECONDS
            )));
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
        QueryMsg::PendingOwnerRotation {} => {
            to_json_binary(&query_pending_owner_rotation(deps)?)
        }
        QueryMsg::PendingMigration {} => to_json_binary(&query_pending_migration(deps)?),
        QueryMsg::PendingConfigUpdate {} => to_json_binary(&query_pending_config_update(deps)?),
    }
}

pub fn query_pending_config_update(deps: Deps) -> StdResult<PendingConfigUpdateResponse> {
    match PENDING_CONFIG_UPDATE.may_load(deps.storage)? {
        Some(pending) => Ok(PendingConfigUpdateResponse {
            distribution_schedule: Some(pending.distribution_schedule),
            effective_at: Some(pending.effective_at),
        }),
        None => Ok(PendingConfigUpdateResponse {
            distribution_schedule: None,
            effective_at: None,
        }),
    }
}

pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = read_config(deps.storage)?;

    let owner_str = deps.api.addr_humanize(&config.owner)?.to_string();

    let reward_token_str = match config.reward_token {
        AssetInfo::Token { ref contract_addr } => contract_addr.clone(),
        AssetInfo::NativeToken { ref denom } => denom.clone(),
    };

    let staking_token_str = match config.staking_token {
        AssetInfo::Token { ref contract_addr } => contract_addr.clone(),
        AssetInfo::NativeToken { ref denom } => denom.clone(),
    };

    let resp = ConfigResponse {
        owner: owner_str,
        reward_token: reward_token_str,
        staking_token: staking_token_str,
        distribution_schedule: config.distribution_schedule,
    };

    Ok(resp)
}

pub fn query_pending_owner_rotation(deps: Deps) -> StdResult<PendingOwnerRotationResponse> {
    let config = read_config(deps.storage)?;
    let pending_owner = match config.pending_owner {
        Some(raw) => Some(deps.api.addr_humanize(&raw)?.to_string()),
        None => None,
    };
    Ok(PendingOwnerRotationResponse {
        pending_owner,
        effective_at: config.pending_owner_effective_at,
    })
}

pub fn query_pending_migration(deps: Deps) -> StdResult<PendingMigrationResponse> {
    match PENDING_MIGRATION.may_load(deps.storage)? {
        Some(pending) => Ok(PendingMigrationResponse {
            new_staking_contract: Some(
                deps.api
                    .addr_humanize(&pending.new_staking_contract)?
                    .to_string(),
            ),
            effective_at: Some(pending.effective_at),
        }),
        None => Ok(PendingMigrationResponse {
            new_staking_contract: None,
            effective_at: None,
        }),
    }
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
        compute_staker_reward(&mut state, &mut staker_info)?;
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

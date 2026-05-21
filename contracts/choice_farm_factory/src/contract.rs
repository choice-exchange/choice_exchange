#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Order, Reply, Response, StdError, StdResult, SubMsg, Uint128, WasmMsg,
};

use cw2::{get_contract_version, set_contract_version};
use cw20::Cw20ExecuteMsg;
use cw_storage_plus::Bound;
use cw_utils::parse_instantiate_response_data;

use choice::asset::AssetInfo;
use choice::farm_factory::{
    ConfigResponse, ExecuteMsg, FarmCountResponse, FarmRecord as FarmRecordResp, FarmsResponse,
    InstantiateMsg, MigrateMsg, PendingFarmCodeIdUpdateResponse, PendingOwnerRotationResponse,
    QueryMsg,
};
use choice::staking::{
    Cw20HookMsg as FarmCw20HookMsg, ExecuteMsg as FarmExecuteMsg,
    InstantiateMsg as FarmInstantiateMsg,
};

use crate::state::{
    Config, FarmRecord, FundAfter, PendingFarm, PendingFarmCodeIdUpdate, CONFIG, DEFAULT_LIMIT,
    FARMS, FARM_BY_ADDR, INJ_DENOM, INSTANTIATE_FARM_REPLY_ID, MAX_LIMIT,
    MAX_SCHEDULE_SLOT_DURATION_SECONDS, MAX_SCHEDULE_SLOTS, NEXT_FARM_ID, PENDING_FARM,
    PENDING_FARM_CODE_ID, TIMELOCK_DELAY_SECONDS,
};

const CONTRACT_NAME: &str = "crates.io:choice-farm-factory";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // The factory has no handler that consumes attached funds at construction
    // time, so any coins sent here would be unrecoverable. Mirror the farm's
    // shape-check at instantiate so a misconfigured deploy fails fast.
    if !info.funds.is_empty() {
        return Err(StdError::generic_err(
            "factory instantiate accepts no funds",
        ));
    }

    let owner = deps.api.addr_validate(&msg.owner)?;
    let fee_collector = deps.api.addr_validate(&msg.fee_collector)?;
    let farm_owner = deps.api.addr_validate(&msg.farm_owner)?;

    if msg.instantiate_fee_inj.is_zero() {
        return Err(StdError::generic_err("instantiate_fee_inj must be non-zero"));
    }
    if msg.farm_code_id == 0 {
        return Err(StdError::generic_err("farm_code_id must be non-zero"));
    }

    CONFIG.save(
        deps.storage,
        &Config {
            owner: deps.api.addr_canonicalize(owner.as_str())?,
            fee_collector: deps.api.addr_canonicalize(fee_collector.as_str())?,
            instantiate_fee_inj: msg.instantiate_fee_inj,
            farm_code_id: msg.farm_code_id,
            farm_owner: deps.api.addr_canonicalize(farm_owner.as_str())?,
            pending_owner: None,
            pending_owner_effective_at: None,
        },
    )?;
    NEXT_FARM_ID.save(deps.storage, &0u64)?;

    Ok(Response::new()
        .add_attribute("action", "instantiate_farm_factory")
        .add_attribute("instantiator", info.sender.as_str())
        .add_attribute("owner", owner.as_str())
        .add_attribute("fee_collector", fee_collector.as_str())
        .add_attribute("instantiate_fee_inj", msg.instantiate_fee_inj.to_string())
        .add_attribute("farm_code_id", msg.farm_code_id.to_string())
        .add_attribute("farm_owner", farm_owner.as_str()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::CreateFarm {
            reward_token,
            staking_token,
            distribution_schedule,
        } => execute_create_farm(
            deps,
            env,
            info,
            reward_token,
            staking_token,
            distribution_schedule,
        ),
        ExecuteMsg::UpdateConfig {
            fee_collector,
            instantiate_fee_inj,
            farm_owner,
        } => execute_update_config(
            deps,
            info,
            fee_collector,
            instantiate_fee_inj,
            farm_owner,
        ),
        ExecuteMsg::ProposeUpdateFarmCodeId { farm_code_id } => {
            execute_propose_update_farm_code_id(deps, env, info, farm_code_id)
        }
        ExecuteMsg::ApplyUpdateFarmCodeId {} => {
            execute_apply_update_farm_code_id(deps, env, info)
        }
        ExecuteMsg::CancelUpdateFarmCodeIdProposal {} => {
            execute_cancel_update_farm_code_id(deps, info)
        }
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, env, info, new_owner)
        }
        ExecuteMsg::ApplyOwnerRotation {} => execute_apply_owner_rotation(deps, env, info),
        ExecuteMsg::CancelOwnerProposal {} => execute_cancel_owner_proposal(deps, info),
    }
}

fn execute_create_farm(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    reward_token: AssetInfo,
    staking_token: AssetInfo,
    distribution_schedule: Vec<(u64, u64, Uint128)>,
) -> StdResult<Response> {
    // Defensive reentrancy guard: if a CW20 reward token's TransferFrom handler
    // tries to re-enter `CreateFarm`, the outer reply would later fail with
    // "no pending farm" and the whole tx would revert. Reject up-front so the
    // failure mode is explicit and PENDING_FARM is never silently overwritten.
    if PENDING_FARM.may_load(deps.storage)?.is_some() {
        return Err(StdError::generic_err(
            "create_farm already in flight (reentrant call)",
        ));
    }

    let config = CONFIG.load(deps.storage)?;

    // C-1: refuse to spawn farms when the factory's own wasm admin doesn't
    // match the `farm_owner` configured here. Both farms and the factory
    // share the same admin (the governance timelock) by construction; a
    // mismatch means deployment hygiene was broken (e.g. someone ran
    // MsgUpdateAdmin on the factory but didn't update farm_owner). Spawning
    // would create a farm whose admin diverges from what users believe.
    let factory_info = deps.querier.query_wasm_contract_info(env.contract.address.as_str())?;
    let expected_admin = deps.api.addr_humanize(&config.farm_owner)?;
    match &factory_info.admin {
        Some(actual) if actual.as_str() == expected_admin.as_str() => {}
        Some(actual) => {
            return Err(StdError::generic_err(format!(
                "factory admin mismatch: factory.admin = {}, farm_owner = {}",
                actual, expected_admin
            )));
        }
        None => {
            return Err(StdError::generic_err(
                "factory has no wasm admin set; refusing to spawn farm",
            ));
        }
    }
    validate_asset_info(deps.as_ref(), &reward_token)?;
    validate_asset_info(deps.as_ref(), &staking_token)?;
    validate_distribution_schedule(&distribution_schedule)?;
    // Reject schedules whose latest end has already passed: the resulting farm
    // would credit `undistributed_rewards` but never distribute, stranding the
    // funds (recoverable only via apply_migrate_staking on the new farm).
    let now = env.block.time.seconds();
    let max_end = distribution_schedule
        .iter()
        .map(|(_, end, _)| *end)
        .max()
        .unwrap_or(0);
    if max_end <= now {
        return Err(StdError::generic_err(
            "distribution schedule: end must be in the future",
        ));
    }

    let total_reward = distribution_schedule
        .iter()
        .try_fold(Uint128::zero(), |acc, (_, _, amt)| acc.checked_add(*amt))?;
    if total_reward.is_zero() {
        return Err(StdError::generic_err("total reward must be non-zero"));
    }

    let fee_amount = config.instantiate_fee_inj;
    // For all reward kinds the factory now keeps the reward in its own
    // balance through the execute step and forwards it via the farm's `Fund`
    // entrypoint in the reply. This makes the native and cw20 paths symmetric
    // and guarantees `state.undistributed_rewards` is credited atomically with
    // farm creation — passing funds as instantiate funds would land in the
    // farm's bank balance without crediting state.
    let fund_after = match &reward_token {
        AssetInfo::NativeToken { denom } if denom == INJ_DENOM => {
            // Fee denom and reward denom collide; caller sends a single combined
            // `inj` coin.
            let combined = fee_amount.checked_add(total_reward)?;
            assert_single_coin(&info.funds, INJ_DENOM, combined)?;
            FundAfter::Native {
                denom: INJ_DENOM.to_string(),
                amount: total_reward,
            }
        }
        AssetInfo::NativeToken { denom } => {
            // Two coins: fee + native reward.
            assert_two_coins(&info.funds, INJ_DENOM, fee_amount, denom, total_reward)?;
            FundAfter::Native {
                denom: denom.clone(),
                amount: total_reward,
            }
        }
        AssetInfo::Token { contract_addr } => {
            // CW20 reward: caller pre-approves the factory; we pull on behalf of
            // the caller and forward into the farm in the reply via Cw20::Send.
            assert_single_coin(&info.funds, INJ_DENOM, fee_amount)?;
            FundAfter::Cw20 {
                cw20_contract: contract_addr.clone(),
                amount: total_reward,
            }
        }
    };

    let farm_id = NEXT_FARM_ID.load(deps.storage)?;
    let fee_collector_addr = deps.api.addr_humanize(&config.fee_collector)?;
    // Two distinct addresses are installed on the spawned farm:
    //   - wasm `admin`        = `config.farm_owner` (the protocol timelock).
    //     Controls `MsgMigrateContract` against the farm — protocol-gated.
    //   - farm `Config.owner` = `info.sender` (the creator).
    //     Controls `AddSchedules`, `Fund`, `ProposeUpdateConfig`,
    //     `ProposeNewOwner`, etc. — creator-self-service.
    // Separating these lets the protocol keep the kill-switch on the wasm
    // code while letting the creator manage their own farm's reward
    // schedules without going through the multisig for every change.
    let farm_owner_addr = deps.api.addr_humanize(&config.farm_owner)?;

    PENDING_FARM.save(
        deps.storage,
        &PendingFarm {
            id: farm_id,
            operator: deps.api.addr_canonicalize(info.sender.as_str())?,
            farm_owner: config.farm_owner.clone(),
            reward_token: reward_token.clone(),
            staking_token: staking_token.clone(),
            total_reward,
            fund_after: Some(fund_after.clone()),
        },
    )?;

    let mut response = Response::new()
        .add_attribute("action", "create_farm")
        .add_attribute("farm_id", farm_id.to_string())
        .add_attribute("operator", info.sender.as_str())
        .add_attribute("farm_owner", farm_owner_addr.as_str())
        .add_attribute("instantiate_fee", fee_amount.to_string())
        .add_attribute("total_reward", total_reward.to_string())
        .add_attribute("fee_collector", fee_collector_addr.as_str());

    // 1. Send the launch fee to Choice's treasury.
    response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
        to_address: fee_collector_addr.to_string(),
        amount: coins(fee_amount.u128(), INJ_DENOM),
    }));

    // 2. Pull CW20 rewards from caller into the factory (no-op for native —
    //    native rewards already arrived as `info.funds`).
    //    Caller must have run `IncreaseAllowance(spender=factory)` first.
    if let FundAfter::Cw20 {
        cw20_contract,
        amount,
    } = &fund_after
    {
        response = response.add_message(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: cw20_contract.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                owner: info.sender.to_string(),
                recipient: env.contract.address.to_string(),
                amount: *amount,
            })?,
            funds: vec![],
        }));
    }

    // 3. Spawn the farm with empty `funds`. The reply forwards the reward via
    //    `Fund {}` so `undistributed_rewards` is credited atomically.
    //
    //    wasm `admin` = `farm_owner` (timelock) — protocol controls code
    //    migrations on the farm.
    //    `Config.owner` = `info.sender` (creator) — creator self-serves
    //    schedule changes, funding, etc.
    response = response.add_submessage(SubMsg::reply_on_success(
        WasmMsg::Instantiate {
            admin: Some(farm_owner_addr.to_string()),
            code_id: config.farm_code_id,
            msg: to_json_binary(&FarmInstantiateMsg {
                owner: info.sender.to_string(),
                reward_token,
                staking_token,
                distribution_schedule,
            })?,
            funds: vec![],
            label: format!("Choice Farm #{}", farm_id),
        },
        INSTANTIATE_FARM_REPLY_ID,
    ));

    Ok(response)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, reply: Reply) -> StdResult<Response> {
    match reply.id {
        INSTANTIATE_FARM_REPLY_ID => handle_instantiate_farm_reply(deps, env, reply),
        _ => Err(StdError::generic_err(format!("unknown reply id {}", reply.id))),
    }
}

fn handle_instantiate_farm_reply(deps: DepsMut, env: Env, reply: Reply) -> StdResult<Response> {
    let pending = PENDING_FARM
        .may_load(deps.storage)?
        .ok_or_else(|| StdError::generic_err("no pending farm for reply"))?;
    PENDING_FARM.remove(deps.storage);

    let farm_addr_str = parse_instantiated_addr(&reply)?;
    let farm_addr = deps.api.addr_validate(&farm_addr_str)?;
    let farm_addr_raw = deps.api.addr_canonicalize(farm_addr.as_str())?;

    let record = FarmRecord {
        id: pending.id,
        farm_addr: farm_addr_raw.clone(),
        operator: pending.operator.clone(),
        farm_owner: pending.farm_owner.clone(),
        reward_token: pending.reward_token.clone(),
        staking_token: pending.staking_token.clone(),
        total_reward: pending.total_reward,
        created_at: env.block.time.seconds(),
        created_at_height: env.block.height,
    };
    FARMS.save(deps.storage, pending.id, &record)?;
    FARM_BY_ADDR.save(deps.storage, farm_addr_raw.as_slice(), &pending.id)?;
    NEXT_FARM_ID.save(deps.storage, &(pending.id + 1))?;

    let mut response = Response::new()
        .add_attribute("action", "farm_created")
        .add_attribute("farm_id", pending.id.to_string())
        .add_attribute("farm_addr", farm_addr.as_str())
        .add_attribute(
            "operator",
            deps.api.addr_humanize(&pending.operator)?.to_string(),
        )
        .add_attribute(
            "farm_owner",
            deps.api.addr_humanize(&pending.farm_owner)?.to_string(),
        )
        .add_attribute("total_reward", pending.total_reward.to_string());

    // Forward the reward to the new farm via its `Fund` entrypoint so
    // `state.undistributed_rewards` is credited atomically with creation. The
    // farm contract never holds untracked balance — the funds either arrive
    // here via Fund{} (the normal flow) or via a later hand-call to Fund{}.
    if let Some(fund_after) = pending.fund_after {
        let fund_msg = match fund_after {
            FundAfter::Native { denom, amount } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: farm_addr.to_string(),
                msg: to_json_binary(&FarmExecuteMsg::Fund {})?,
                funds: vec![Coin::new(amount.u128(), denom)],
            }),
            FundAfter::Cw20 {
                cw20_contract,
                amount,
            } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: cw20_contract,
                msg: to_json_binary(&Cw20ExecuteMsg::Send {
                    contract: farm_addr.to_string(),
                    amount,
                    msg: to_json_binary(&FarmCw20HookMsg::Fund {})?,
                })?,
                funds: vec![],
            }),
        };
        response = response.add_message(fund_msg);
    }

    Ok(response)
}

fn parse_instantiated_addr(reply: &Reply) -> StdResult<String> {
    // Primary path: cosmwasm 2.x exposes the MsgInstantiateContractResponse
    // proto on `reply.result.msg_responses`. Decoding it directly via
    // `cw_utils::parse_instantiate_response_data` is robust against extra
    // events the farm's instantiate might emit (today it emits none beyond
    // attributes, but pinning to msg_responses removes the implicit coupling).
    let result = reply
        .result
        .clone()
        .into_result()
        .map_err(StdError::generic_err)?;
    if let Some(resp) = result.msg_responses.first() {
        let parsed = parse_instantiate_response_data(resp.value.as_slice())
            .map_err(|e| StdError::generic_err(format!("parse instantiate response: {}", e)))?;
        return Ok(parsed.contract_address);
    }
    // Fallback: walk events for the `_contract_address` attribute on the
    // `instantiate` event. Some test harnesses (and older chain releases)
    // populate events but not msg_responses.
    for ev in result.events.iter() {
        if ev.ty == "instantiate" {
            for attr in ev.attributes.iter() {
                if attr.key == "_contract_address" || attr.key == "contract_address" {
                    return Ok(attr.value.clone());
                }
            }
        }
    }
    Err(StdError::generic_err(
        "instantiate reply missing both msg_responses and _contract_address attribute",
    ))
}

/// Mutates `fee_collector`, `instantiate_fee_inj`, and `farm_owner` immediately
/// on owner signature — **no timelock**. Pre-mainnet audit flagged this as
/// asymmetric with `ProposeUpdateFarmCodeId` / `ProposeNewOwner` (both
/// timelocked) and with the farm-side `ProposeUpdateConfig` (timelocked).
///
/// Risk accepted: a compromised factory `owner` can (a) redirect future launch
/// fees by swapping `fee_collector`, (b) set a malicious `farm_owner` for
/// farms spawned *after* the swap (existing farms are unaffected — their
/// owner is canonicalized at instantiate time), or (c) brick farm creation
/// by setting an absurd `instantiate_fee_inj`. None of these can drain user
/// funds from already-spawned farms; the worst case is launch-fee theft and
/// griefing of future creators.
///
/// Mitigation: the factory's `owner` must be the governance multisig /
/// `choice_admin_timelock`. Hold the key in cold storage with the same
/// operational discipline as the wasm-admin key. If the centralization
/// surface ever expands (e.g., factory gains the power to migrate spawned
/// farms), this function MUST be timelocked.
fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    fee_collector: Option<String>,
    instantiate_fee_inj: Option<Uint128>,
    farm_owner: Option<String>,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let mut attrs = vec![("action", "update_config".to_string())];

    if let Some(addr) = fee_collector {
        let validated = deps.api.addr_validate(&addr)?;
        config.fee_collector = deps.api.addr_canonicalize(validated.as_str())?;
        attrs.push(("fee_collector", validated.to_string()));
    }
    if let Some(fee) = instantiate_fee_inj {
        if fee.is_zero() {
            return Err(StdError::generic_err("instantiate_fee_inj must be non-zero"));
        }
        config.instantiate_fee_inj = fee;
        attrs.push(("instantiate_fee_inj", fee.to_string()));
    }
    if let Some(addr) = farm_owner {
        let validated = deps.api.addr_validate(&addr)?;
        config.farm_owner = deps.api.addr_canonicalize(validated.as_str())?;
        attrs.push(("farm_owner", validated.to_string()));
    }

    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new().add_attributes(attrs))
}

/// H-2: owner proposes a new `farm_code_id`. The actual swap is gated behind
/// `ApplyUpdateFarmCodeId`, which requires `TIMELOCK_DELAY_SECONDS` to have
/// elapsed. Re-proposing overwrites and resets the timer.
fn execute_propose_update_farm_code_id(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    farm_code_id: u64,
) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }
    if farm_code_id == 0 {
        return Err(StdError::generic_err("farm_code_id must be non-zero"));
    }

    let effective_at = env.block.time.seconds() + TIMELOCK_DELAY_SECONDS;
    PENDING_FARM_CODE_ID.save(
        deps.storage,
        &PendingFarmCodeIdUpdate {
            farm_code_id,
            effective_at,
        },
    )?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_update_farm_code_id"),
        ("farm_code_id", farm_code_id.to_string().as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

fn execute_apply_update_farm_code_id(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let pending = PENDING_FARM_CODE_ID
        .may_load(deps.storage)?
        .ok_or_else(|| StdError::generic_err("no pending farm_code_id update"))?;

    if env.block.time.seconds() < pending.effective_at {
        return Err(StdError::generic_err(
            "farm_code_id update timelock has not elapsed",
        ));
    }

    config.farm_code_id = pending.farm_code_id;
    CONFIG.save(deps.storage, &config)?;
    PENDING_FARM_CODE_ID.remove(deps.storage);

    Ok(Response::new().add_attributes(vec![
        ("action", "apply_update_farm_code_id"),
        ("farm_code_id", pending.farm_code_id.to_string().as_str()),
    ]))
}

fn execute_cancel_update_farm_code_id(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if PENDING_FARM_CODE_ID.may_load(deps.storage)?.is_none() {
        return Err(StdError::generic_err("no pending farm_code_id update"));
    }

    PENDING_FARM_CODE_ID.remove(deps.storage);

    Ok(Response::new().add_attribute("action", "cancel_update_farm_code_id_proposal"))
}

fn execute_propose_new_owner(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_owner: String,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let new_owner_addr = deps.api.addr_validate(&new_owner)?;
    let new_owner_raw = deps.api.addr_canonicalize(new_owner_addr.as_str())?;
    let effective_at = env.block.time.seconds() + TIMELOCK_DELAY_SECONDS;

    config.pending_owner = Some(new_owner_raw);
    config.pending_owner_effective_at = Some(effective_at);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_new_owner"),
        ("pending_owner", new_owner_addr.as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

fn execute_apply_owner_rotation(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
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
        return Err(StdError::generic_err(
            "owner rotation timelock has not elapsed",
        ));
    }

    let new_owner_addr = deps.api.addr_humanize(&pending)?;
    config.owner = pending;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "apply_owner_rotation"),
        ("new_owner", new_owner_addr.as_str()),
    ]))
}

fn execute_cancel_owner_proposal(deps: DepsMut, info: MessageInfo) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if config.pending_owner.is_none() {
        return Err(StdError::generic_err("no pending owner rotation"));
    }

    config.pending_owner = None;
    config.pending_owner_effective_at = None;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "cancel_owner_proposal"))
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
        // M-2: mirror the farm's per-slot duration cap. The farm enforces it
        // too, but rejecting here avoids dispatching the fee BankSend / cw20
        // TransferFrom only to revert in the instantiate reply.
        if end - start > MAX_SCHEDULE_SLOT_DURATION_SECONDS {
            return Err(StdError::generic_err(format!(
                "distribution schedule: slot duration exceeds max ({} seconds)",
                MAX_SCHEDULE_SLOT_DURATION_SECONDS
            )));
        }
    }
    Ok(())
}

fn assert_single_coin(funds: &[Coin], denom: &str, amount: Uint128) -> StdResult<()> {
    if funds.len() != 1 {
        return Err(StdError::generic_err(format!(
            "expected exactly 1 coin ({} {}), got {} coins",
            amount,
            denom,
            funds.len()
        )));
    }
    let c = &funds[0];
    if c.denom != denom || c.amount != amount {
        return Err(StdError::generic_err(format!(
            "expected {} {}, got {} {}",
            amount, denom, c.amount, c.denom
        )));
    }
    Ok(())
}

fn assert_two_coins(
    funds: &[Coin],
    denom_a: &str,
    amount_a: Uint128,
    denom_b: &str,
    amount_b: Uint128,
) -> StdResult<()> {
    if denom_a == denom_b {
        return Err(StdError::generic_err(
            "internal: assert_two_coins with same denom",
        ));
    }
    if funds.len() != 2 {
        return Err(StdError::generic_err(format!(
            "expected 2 coins ({} {} + {} {}), got {} coins",
            amount_a,
            denom_a,
            amount_b,
            denom_b,
            funds.len()
        )));
    }
    let (got_a, got_b) = if funds[0].denom == denom_a && funds[1].denom == denom_b {
        (&funds[0], &funds[1])
    } else if funds[0].denom == denom_b && funds[1].denom == denom_a {
        (&funds[1], &funds[0])
    } else {
        return Err(StdError::generic_err(format!(
            "expected denoms [{}, {}], got [{}, {}]",
            denom_a, denom_b, funds[0].denom, funds[1].denom
        )));
    };
    if got_a.amount != amount_a {
        return Err(StdError::generic_err(format!(
            "expected {} {}, got {} {}",
            amount_a, denom_a, got_a.amount, denom_a
        )));
    }
    if got_b.amount != amount_b {
        return Err(StdError::generic_err(format!(
            "expected {} {}, got {} {}",
            amount_b, denom_b, got_b.amount, denom_b
        )));
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::PendingOwnerRotation {} => to_json_binary(&query_pending_owner(deps)?),
        QueryMsg::PendingFarmCodeIdUpdate {} => {
            to_json_binary(&query_pending_farm_code_id(deps)?)
        }
        QueryMsg::Farm { id } => to_json_binary(&query_farm(deps, id)?),
        QueryMsg::FarmByAddr { addr } => to_json_binary(&query_farm_by_addr(deps, addr)?),
        QueryMsg::Farms { start_after, limit } => {
            to_json_binary(&query_farms(deps, start_after, limit)?)
        }
        QueryMsg::FarmCount {} => to_json_binary(&query_farm_count(deps)?),
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: deps.api.addr_humanize(&config.owner)?.to_string(),
        fee_collector: deps
            .api
            .addr_humanize(&config.fee_collector)?
            .to_string(),
        instantiate_fee_inj: config.instantiate_fee_inj,
        farm_code_id: config.farm_code_id,
        farm_owner: deps.api.addr_humanize(&config.farm_owner)?.to_string(),
    })
}

fn query_pending_owner(deps: Deps) -> StdResult<PendingOwnerRotationResponse> {
    let config = CONFIG.load(deps.storage)?;
    let pending_owner = match config.pending_owner {
        Some(raw) => Some(deps.api.addr_humanize(&raw)?.to_string()),
        None => None,
    };
    Ok(PendingOwnerRotationResponse {
        pending_owner,
        effective_at: config.pending_owner_effective_at,
    })
}

fn query_pending_farm_code_id(deps: Deps) -> StdResult<PendingFarmCodeIdUpdateResponse> {
    match PENDING_FARM_CODE_ID.may_load(deps.storage)? {
        Some(pending) => Ok(PendingFarmCodeIdUpdateResponse {
            farm_code_id: Some(pending.farm_code_id),
            effective_at: Some(pending.effective_at),
        }),
        None => Ok(PendingFarmCodeIdUpdateResponse {
            farm_code_id: None,
            effective_at: None,
        }),
    }
}

fn query_farm(deps: Deps, id: u64) -> StdResult<FarmRecordResp> {
    let raw = FARMS
        .may_load(deps.storage, id)?
        .ok_or_else(|| StdError::generic_err(format!("farm {} not found", id)))?;
    humanize_farm(deps, &raw)
}

fn query_farm_by_addr(deps: Deps, addr: String) -> StdResult<FarmRecordResp> {
    let validated = deps.api.addr_validate(&addr)?;
    let canonical = deps.api.addr_canonicalize(validated.as_str())?;
    let id = FARM_BY_ADDR
        .may_load(deps.storage, canonical.as_slice())?
        .ok_or_else(|| StdError::generic_err(format!("farm at {} not in registry", validated)))?;
    let raw = FARMS
        .load(deps.storage, id)
        .map_err(|_| StdError::generic_err("registry consistency error: id missing"))?;
    humanize_farm(deps, &raw)
}

fn query_farms(
    deps: Deps,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<FarmsResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let lower = start_after.map(Bound::exclusive);

    let farms = FARMS
        .range(deps.storage, lower, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (_, record) = item?;
            humanize_farm(deps, &record)
        })
        .collect::<StdResult<Vec<_>>>()?;

    Ok(FarmsResponse { farms })
}

fn query_farm_count(deps: Deps) -> StdResult<FarmCountResponse> {
    let count = NEXT_FARM_ID.load(deps.storage)?;
    Ok(FarmCountResponse { count })
}

fn humanize_farm(deps: Deps, raw: &FarmRecord) -> StdResult<FarmRecordResp> {
    Ok(FarmRecordResp {
        id: raw.id,
        farm_addr: deps.api.addr_humanize(&raw.farm_addr)?.to_string(),
        operator: deps.api.addr_humanize(&raw.operator)?.to_string(),
        farm_owner: deps.api.addr_humanize(&raw.farm_owner)?.to_string(),
        reward_token: raw.reward_token.clone(),
        staking_token: raw.staking_token.clone(),
        total_reward: raw.total_reward,
        created_at: raw.created_at,
        created_at_height: raw.created_at_height,
    })
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


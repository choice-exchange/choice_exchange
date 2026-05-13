#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    to_json_binary, Binary, CanonicalAddr, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Response,
    StdError, StdResult, WasmMsg,
};

use cw2::{get_contract_version, set_contract_version};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::{
    Config, PendingMigration, CONFIG, MIN_TIMELOCK_SECONDS, PENDING_MIGRATION,
};

const CONTRACT_NAME: &str = "crates.io:choice-admin-timelock";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct InstantiateMsg {
    pub owner: String,
    pub timelock_seconds: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Owner queues a `MsgMigrateContract` against `contract` to `code_id`.
    /// Apply unlocks `timelock_seconds` from now. Re-proposing overwrites the
    /// pending proposal and resets the timer.
    Propose {
        contract: String,
        code_id: u64,
        msg: Binary,
    },
    /// Anyone can apply once the timelock has elapsed — the contract
    /// dispatches `WasmMsg::Migrate` as itself. Public on purpose: users who
    /// see a queued migration can settle it without depending on the owner.
    Apply {},
    /// Owner clears a pending migration.
    Cancel {},
    /// Owner proposes a new owner. Rotation cannot take effect until the
    /// timelock has elapsed.
    ProposeNewOwner { new_owner: String },
    /// Anyone can finalize a pending owner rotation once the timelock has
    /// expired — same public-apply rationale.
    ApplyOwnerRotation {},
    /// Current owner clears a pending owner proposal.
    CancelOwnerProposal {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct MigrateMsg {}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Config {},
    PendingMigration {},
    PendingOwnerRotation {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct ConfigResponse {
    pub owner: String,
    pub timelock_seconds: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingMigrationResponse {
    pub contract: Option<String>,
    pub code_id: Option<u64>,
    pub msg: Option<Binary>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingOwnerRotationResponse {
    pub pending_owner: Option<String>,
    pub effective_at: Option<u64>,
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    if !info.funds.is_empty() {
        return Err(StdError::generic_err(
            "timelock instantiate accepts no funds",
        ));
    }
    if msg.timelock_seconds < MIN_TIMELOCK_SECONDS {
        return Err(StdError::generic_err(format!(
            "timelock_seconds must be at least {} (one hour)",
            MIN_TIMELOCK_SECONDS
        )));
    }

    let owner = deps.api.addr_validate(&msg.owner)?;
    CONFIG.save(
        deps.storage,
        &Config {
            owner: deps.api.addr_canonicalize(owner.as_str())?,
            timelock_seconds: msg.timelock_seconds,
            pending_owner: None,
            pending_owner_effective_at: None,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate_admin_timelock")
        .add_attribute("owner", owner.as_str())
        .add_attribute("timelock_seconds", msg.timelock_seconds.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Propose {
            contract,
            code_id,
            msg,
        } => execute_propose(deps, env, info, contract, code_id, msg),
        ExecuteMsg::Apply {} => execute_apply(deps, env),
        ExecuteMsg::Cancel {} => execute_cancel(deps, info),
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, env, info, new_owner)
        }
        ExecuteMsg::ApplyOwnerRotation {} => execute_apply_owner_rotation(deps, env),
        ExecuteMsg::CancelOwnerProposal {} => execute_cancel_owner_proposal(deps, info),
    }
}

fn assert_owner(deps: Deps, sender: &str, config: &Config) -> StdResult<()> {
    let sender_raw = deps.api.addr_canonicalize(sender)?;
    if sender_raw != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }
    Ok(())
}

fn execute_propose(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    contract: String,
    code_id: u64,
    msg: Binary,
) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    assert_owner(deps.as_ref(), info.sender.as_str(), &config)?;

    let validated_contract = deps.api.addr_validate(&contract)?;
    if code_id == 0 {
        return Err(StdError::generic_err("code_id must be non-zero"));
    }

    let effective_at = env.block.time.seconds() + config.timelock_seconds;
    PENDING_MIGRATION.save(
        deps.storage,
        &PendingMigration {
            contract: validated_contract.to_string(),
            code_id,
            msg: msg.clone(),
            effective_at,
        },
    )?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose"),
        ("contract", validated_contract.as_str()),
        ("code_id", code_id.to_string().as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

fn execute_apply(deps: DepsMut, env: Env) -> StdResult<Response> {
    let pending = PENDING_MIGRATION
        .may_load(deps.storage)?
        .ok_or_else(|| StdError::generic_err("no pending migration"))?;

    if env.block.time.seconds() < pending.effective_at {
        return Err(StdError::generic_err("migration timelock has not elapsed"));
    }

    PENDING_MIGRATION.remove(deps.storage);

    let migrate_msg = CosmosMsg::Wasm(WasmMsg::Migrate {
        contract_addr: pending.contract.clone(),
        new_code_id: pending.code_id,
        msg: pending.msg,
    });

    Ok(Response::new()
        .add_message(migrate_msg)
        .add_attributes(vec![
            ("action", "apply"),
            ("contract", pending.contract.as_str()),
            ("code_id", pending.code_id.to_string().as_str()),
        ]))
}

fn execute_cancel(deps: DepsMut, info: MessageInfo) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    assert_owner(deps.as_ref(), info.sender.as_str(), &config)?;

    if PENDING_MIGRATION.may_load(deps.storage)?.is_none() {
        return Err(StdError::generic_err("no pending migration"));
    }
    PENDING_MIGRATION.remove(deps.storage);
    Ok(Response::new().add_attribute("action", "cancel"))
}

fn execute_propose_new_owner(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_owner: String,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
    assert_owner(deps.as_ref(), info.sender.as_str(), &config)?;

    let new_owner_addr = deps.api.addr_validate(&new_owner)?;
    let new_owner_raw: CanonicalAddr = deps.api.addr_canonicalize(new_owner_addr.as_str())?;
    let effective_at = env.block.time.seconds() + config.timelock_seconds;
    config.pending_owner = Some(new_owner_raw);
    config.pending_owner_effective_at = Some(effective_at);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "propose_new_owner"),
        ("pending_owner", new_owner_addr.as_str()),
        ("effective_at", effective_at.to_string().as_str()),
    ]))
}

fn execute_apply_owner_rotation(deps: DepsMut, env: Env) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;
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
    assert_owner(deps.as_ref(), info.sender.as_str(), &config)?;

    if config.pending_owner.is_none() {
        return Err(StdError::generic_err("no pending owner rotation"));
    }
    config.pending_owner = None;
    config.pending_owner_effective_at = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new().add_attribute("action", "cancel_owner_proposal"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::PendingMigration {} => to_json_binary(&query_pending_migration(deps)?),
        QueryMsg::PendingOwnerRotation {} => to_json_binary(&query_pending_owner(deps)?),
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: deps.api.addr_humanize(&config.owner)?.to_string(),
        timelock_seconds: config.timelock_seconds,
    })
}

fn query_pending_migration(deps: Deps) -> StdResult<PendingMigrationResponse> {
    match PENDING_MIGRATION.may_load(deps.storage)? {
        Some(p) => Ok(PendingMigrationResponse {
            contract: Some(p.contract),
            code_id: Some(p.code_id),
            msg: Some(p.msg),
            effective_at: Some(p.effective_at),
        }),
        None => Ok(PendingMigrationResponse {
            contract: None,
            code_id: None,
            msg: None,
            effective_at: None,
        }),
    }
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

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    let stored = get_contract_version(deps.storage)?;
    if stored.contract != CONTRACT_NAME {
        return Err(StdError::generic_err(format!(
            "cannot migrate: expected {}, found {}",
            CONTRACT_NAME, stored.contract
        )));
    }
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", stored.version)
        .add_attribute("to_version", CONTRACT_VERSION))
}

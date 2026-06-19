use crate::clmm::{full_range_ticks, init_sqrt_price_from_amounts};
use crate::error::ContractError;
use crate::msg::{
    CallbackMsg, ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg,
    LockerConfigResponse, LockerInit, MigrateMsg, PoolKind, QueryMsg, RoleResponse,
    SinkConfigResponse, SinkInit, SinkStateResponse,
};
// S-5: `LpDestination` only appears in the XYK arm of `validate_pool_kind`,
// compiled solely under the `xyk` feature. Gate the import so the default
// (XYK-disabled) production build carries no unused import.
#[cfg(feature = "xyk")]
use crate::msg::LpDestination;
use crate::state::{
    FactoryConfig, LockerConfig, LpDestinationStored, PendingClmmMint, PoolKindStored, Role,
    SinkConfig, SinkState, SinkStatus, FACTORY_CONFIG, LOCKER_CONFIG, PENDING_CLMM_MINT, ROLE,
    SINK_CONFIG, SINK_STATE,
};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Reply,
    Response, StdError, StdResult, SubMsg, SubMsgResult, Uint128, Uint256, WasmMsg,
};
use cw2::set_contract_version;
use serde::Deserialize;

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{ExecuteMsg as FactoryExecuteMsg, QueryMsg as ChoiceFactoryQueryMsg};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use choice::querier::query_token_factory_denom_create_fee;

use choice_clmm_common::factory::{
    ExecuteMsg as ClmmFactoryExecuteMsg, FeeTierEntry, QueryMsg as ClmmFactoryQueryMsg,
};
use choice_clmm_common::manager::{
    ExecuteMsg as ClmmManagerExecuteMsg, QueryMsg as ClmmManagerQueryMsg,
};
use choice_clmm_common::types::AssetInfo as ClmmAssetInfo;

use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::InjectiveMsgWrapper;

const CONTRACT_NAME: &str = "crates.io:choice-pool-seeder";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Label used by `Instantiate2` for spawned sinks. The chain prepends a
/// 64-byte truncated identifier so this just needs to be human-recognizable.
const SINK_LABEL_PREFIX: &str = "choice-pool-seeder-sink";

/// Label prefix for spawned lockers.
const LOCKER_LABEL_PREFIX: &str = "choice-pool-seeder-locker";

/// Cap on the number of position NFTs `CollectFees { token_id: None }` will
/// enumerate-and-collect in one call. A launchpad locker holds exactly one,
/// but the bound keeps the message count sane if a locker is ever reused.
const COLLECT_FEES_PAGE: u32 = 30;

/// Basis-points denominator for the locker fee split. `creator_fee_share_bps`
/// is a fraction of the fee out of this, so it can range over `[0, BPS_DENOM]`.
const BPS_DENOM: u16 = 10_000;

/// S-1: minimum `deadline_seconds` a sink may be instantiated with. Mirrors the
/// issuer's `MIN_REFUND_DEADLINE_SECONDS` (= 3600 = 1 hour). A too-short
/// deadline opens the permissionless `Refund` path almost immediately, letting
/// anyone race a fully-funded sink into a refund before the keeper can `Settle`
/// it — permanently denying graduation. Combined with the S-1(b)
/// settleable-sink guard in `exec_refund`, this keeps the refund path confined
/// to genuinely-failed launches.
const MIN_DEADLINE_SECONDS: u64 = 3600;

/// OBSERVABILITY: reply id for the CLMM `MintPosition` sub-message. Its
/// `ReplyOn::Success` handler parses the minted position `token_id` (and records
/// the seeded pool address) into `SinkState`. `ReplyOn::Success` preserves
/// atomicity — only the success branch runs, and any earlier failure in the
/// settle tx still reverts everything. `pub(crate)` so the unit tests can wire
/// a `Reply` with the matching id.
pub(crate) const REPLY_CLMM_MINT: u64 = 1;

// ------------------------------------------------------------------------
// Entry points
// ------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    match msg {
        InstantiateMsg::Factory(init) => instantiate_factory(deps, info, init),
        InstantiateMsg::Sink(init) => instantiate_sink(deps, env, info, init),
        InstantiateMsg::Locker(init) => instantiate_locker(deps, info, init),
    }
}

fn instantiate_factory(
    deps: DepsMut<InjectiveQueryWrapper>,
    _info: MessageInfo,
    init: FactoryInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let admin = deps.api.addr_validate(&init.admin)?;
    let choice_factory = deps.api.addr_validate(&init.choice_factory)?;

    // CLMM addresses are all-or-nothing: a factory either knows the full CLMM
    // route (factory + manager) or seeds XYK only.
    let (clmm_factory, clmm_manager) = match (init.clmm_factory, init.clmm_manager) {
        (Some(f), Some(m)) => (
            Some(deps.api.addr_validate(&f)?),
            Some(deps.api.addr_validate(&m)?),
        ),
        (None, None) => (None, None),
        _ => return Err(ContractError::ClmmHalfConfigured {}),
    };

    ROLE.save(deps.storage, &Role::Factory)?;
    FACTORY_CONFIG.save(
        deps.storage,
        &FactoryConfig {
            admin: admin.clone(),
            pending_admin: None,
            sink_code_id: init.sink_code_id,
            choice_factory: choice_factory.clone(),
            clmm_factory: clmm_factory.clone(),
            clmm_manager: clmm_manager.clone(),
            paused: false,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("role", "factory")
        .add_attribute("admin", admin)
        .add_attribute("sink_code_id", init.sink_code_id.to_string())
        .add_attribute("choice_factory", choice_factory)
        .add_attribute(
            "clmm_factory",
            clmm_factory.map(|a| a.to_string()).unwrap_or_default(),
        )
        .add_attribute(
            "clmm_manager",
            clmm_manager.map(|a| a.to_string()).unwrap_or_default(),
        ))
}

fn instantiate_sink(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    init: SinkInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if init.token_denom == init.pair_denom {
        return Err(ContractError::SameDenom {
            denom: init.token_denom,
        });
    }
    if init.deadline_seconds == 0 {
        return Err(ContractError::ZeroDeadline {});
    }
    // S-1: enforce a floor on the refund deadline. A too-short deadline opens
    // the permissionless `Refund` almost immediately, letting anyone race a
    // fully-funded sink into a refund before the keeper can `Settle`. This gate
    // lives on `instantiate_sink` (NOT only `exec_create_sink`) so it covers
    // BOTH the factory `CreateSink` path — whose `Instantiate2` reaches here —
    // and the direct-instantiate debug path. Mirrors the issuer's
    // `MIN_REFUND_DEADLINE_SECONDS`.
    if init.deadline_seconds < MIN_DEADLINE_SECONDS {
        return Err(ContractError::DeadlineTooShort {
            got: init.deadline_seconds,
            min: MIN_DEADLINE_SECONDS,
        });
    }
    // H-1 / M-1: committed seed amounts must be supplied together (or both
    // omitted, for the legacy debug path) and, when supplied, be non-zero.
    match (init.expected_token, init.expected_pair) {
        (Some(t), Some(p)) => {
            if t.is_zero() || p.is_zero() {
                return Err(ContractError::ZeroExpectedAmount {});
            }
        }
        (None, None) => {}
        _ => return Err(ContractError::ExpectedAmountsHalfSet {}),
    }

    let issuer = deps.api.addr_validate(&init.issuer)?;
    let refund_receiver = deps.api.addr_validate(&init.refund_receiver)?;
    let pool_kind = validate_pool_kind(deps.as_ref(), &init.pool_kind)?;
    let pool_kind_label = match &pool_kind {
        PoolKindStored::Xyk { .. } => "xyk",
        PoolKindStored::Clmm { .. } => "clmm",
    };

    ROLE.save(deps.storage, &Role::Sink)?;
    SINK_CONFIG.save(
        deps.storage,
        &SinkConfig {
            // The factory that issued the `Instantiate2` is `info.sender`; the
            // sink records it so `Settle` can honour the factory pause and
            // `ForceRefund` can authenticate the factory admin.
            //
            // Security note: `CreateSink`/`instantiate_sink` is permissionless,
            // so anyone can stand up a sink with an arbitrary (even code-less)
            // `factory`. That is harmless because the only funds a sink ever
            // receives come from the issuer, which delivers EXCLUSIVELY to the
            // Instantiate2 address it derives + verifies on-chain
            // (`verify_seeder_addr_derivation`, with `verify_seeder_derivation`
            // ON by default). A look-alike sink at any other address is never
            // funded, so its spoofed `factory` (and the `ensure_factory_not_paused`
            // fail-open on a code-less factory) grants nothing. Keep
            // `verify_seeder_derivation` ON in production — it is the real gate.
            factory: Some(info.sender.clone()),
            issuer: issuer.clone(),
            token_denom: init.token_denom.clone(),
            pair_denom: init.pair_denom.clone(),
            token_decimals: init.token_decimals,
            pair_decimals: init.pair_decimals,
            pool_kind,
            refund_receiver: refund_receiver.clone(),
            deadline_seconds: init.deadline_seconds,
            instantiated_at: env.block.time.seconds(),
            expected_token: init.expected_token,
            expected_pair: init.expected_pair,
        },
    )?;
    SINK_STATE.save(
        deps.storage,
        &SinkState {
            status: SinkStatus::Pending,
            pair_addr: None,
            lp_minted: None,
            // OBSERVABILITY: populated on a successful CLMM settle (see the
            // `REPLY_CLMM_MINT` reply handler).
            pool_addr: None,
            position_token_id: None,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("role", "sink")
        .add_attribute("issuer", issuer)
        .add_attribute("token_denom", init.token_denom)
        .add_attribute("pair_denom", init.pair_denom)
        .add_attribute("refund_receiver", refund_receiver)
        .add_attribute("pool_kind", pool_kind_label)
        .add_attribute("deadline_seconds", init.deadline_seconds.to_string()))
}

/// Address-validate a `PoolKind` into its stored form. Does NOT check it
/// against a factory's pinned addresses — that's the factory's job in
/// `CreateSink` (a directly-instantiated sink has no factory to check
/// against).
fn validate_pool_kind(
    deps: Deps<InjectiveQueryWrapper>,
    pool_kind: &PoolKind,
) -> Result<PoolKindStored, ContractError> {
    Ok(match pool_kind {
        // S-5: XYK graduation is feature-gated. In production (default build,
        // no `xyk` feature) instantiating an XYK sink is rejected here — the
        // earliest possible point — so a sink with a `PoolKindStored::Xyk`
        // config can never exist on-chain and the (unaudited-for-committed-
        // ratio) `settle_xyk` path is unreachable. The XYK arm still COMPILES
        // (it builds `PoolKindStored::Xyk`) so the code path and its tests can
        // be exercised under `--features xyk`.
        #[cfg(not(feature = "xyk"))]
        PoolKind::Xyk { .. } => return Err(ContractError::XykDisabled {}),
        #[cfg(feature = "xyk")]
        PoolKind::Xyk {
            choice_factory,
            lp_destination,
        } => PoolKindStored::Xyk {
            choice_factory: deps.api.addr_validate(choice_factory)?,
            lp_destination: match lp_destination {
                LpDestination::Burn => LpDestinationStored::Burn,
                LpDestination::SendTo(s) => LpDestinationStored::SendTo(deps.api.addr_validate(s)?),
            },
        },
        PoolKind::Clmm {
            clmm_factory,
            clmm_manager,
            fee_tier,
            position_recipient,
            max_fee_multiple,
        } => PoolKindStored::Clmm {
            clmm_factory: deps.api.addr_validate(clmm_factory)?,
            clmm_manager: deps.api.addr_validate(clmm_manager)?,
            fee_tier: *fee_tier,
            position_recipient: deps.api.addr_validate(position_recipient)?,
            max_fee_multiple: *max_fee_multiple,
        },
    })
}

fn instantiate_locker(
    deps: DepsMut<InjectiveQueryWrapper>,
    _info: MessageInfo,
    init: LockerInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let manager = deps.api.addr_validate(&init.manager)?;
    let treasury = deps.api.addr_validate(&init.treasury)?;
    let creator = deps.api.addr_validate(&init.creator)?;
    if init.creator_fee_share_bps > BPS_DENOM {
        return Err(ContractError::LockerCreatorFeeShareTooHigh {
            value: init.creator_fee_share_bps,
            max: BPS_DENOM,
        });
    }
    let admin = init
        .admin
        .as_deref()
        .map(|a| deps.api.addr_validate(a))
        .transpose()?;

    ROLE.save(deps.storage, &Role::Locker)?;
    LOCKER_CONFIG.save(
        deps.storage,
        &LockerConfig {
            manager: manager.clone(),
            treasury: treasury.clone(),
            creator: creator.clone(),
            creator_fee_share_bps: init.creator_fee_share_bps,
            admin: admin.clone(),
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("role", "locker")
        .add_attribute("manager", manager)
        .add_attribute("treasury", treasury)
        .add_attribute("creator", creator)
        .add_attribute(
            "creator_fee_share_bps",
            init.creator_fee_share_bps.to_string(),
        )
        .add_attribute("admin", admin.map(|a| a.to_string()).unwrap_or_default()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg {
        ExecuteMsg::CreateSink { salt, sink_init } => {
            exec_create_sink(deps, env, info, salt, sink_init)
        }
        ExecuteMsg::CreateLocker { salt, locker_init } => {
            exec_create_locker(deps, env, info, salt, locker_init)
        }
        ExecuteMsg::Settle {} => exec_settle(deps, env, info),
        ExecuteMsg::Refund {} => exec_refund(deps, env, info),
        ExecuteMsg::ForceRefund {} => exec_force_refund(deps, env, info),
        ExecuteMsg::UpdateAdmin { new_admin } => exec_update_admin(deps, info, new_admin),
        ExecuteMsg::AcceptAdmin {} => exec_accept_admin(deps, info),
        ExecuteMsg::UpdateSinkCodeId { new_sink_code_id } => {
            exec_update_sink_code_id(deps, info, new_sink_code_id)
        }
        ExecuteMsg::SetPaused { paused } => exec_set_paused(deps, info, paused),
        ExecuteMsg::UpdateChoiceFactory { new_choice_factory } => {
            exec_update_choice_factory(deps, info, new_choice_factory)
        }
        ExecuteMsg::UpdateClmmAddresses {
            clmm_factory,
            clmm_manager,
        } => exec_update_clmm_addresses(deps, info, clmm_factory, clmm_manager),
        ExecuteMsg::Callback(cb) => exec_callback(deps, env, info, cb),
        ExecuteMsg::CollectFees { token_id } => exec_collect_fees(deps, env, info, token_id),
        ExecuteMsg::UpdateTreasury { new_treasury } => {
            exec_update_treasury(deps, info, new_treasury)
        }
    }
}

/// OBSERVABILITY: reply entry point. Today there is exactly one reply — the
/// `MintPosition` success reply (`REPLY_CLMM_MINT`) emitted by `settle_clmm`,
/// which records the seeded pool address + minted position `token_id` onto
/// `SinkState`. `ReplyOn::Success` means this only ever runs on success, so it
/// records observability data without ever masking a settle failure (a failed
/// mint propagates and reverts the whole tx).
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    reply: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match reply.id {
        REPLY_CLMM_MINT => reply_clmm_mint(deps, reply),
        other => Err(ContractError::UnknownReplyId { id: other }),
    }
}

// ------------------------------------------------------------------------
// Factory-side
// ------------------------------------------------------------------------

fn exec_create_sink(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    salt: Binary,
    sink_init: SinkInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_factory(deps.as_ref(), "create_sink")?;
    if cfg.paused {
        return Err(ContractError::Paused {});
    }

    // Reject — don't silently rewrite — so the caller sees what's wrong. A
    // sink only ever talks to the DEX deployment(s) this factory pins;
    // consumers must construct `sink_init.pool_kind` against the same
    // addresses.
    //
    // NOTE: XYK graduation remains code-capable but is UNREACHABLE on the live
    // SHROOM path — `LaunchpadCore.createLaunch` rejects non-CLMM venues
    // (`XykGraduationDisabled`), so the keeper/issuer never forward an XYK
    // `CreateSink`. The XYK seed path has NOT received the committed-ratio
    // hardening below; do not re-enable it without first re-auditing
    // `settle_xyk` for the donation-reprice and create-fee paths.
    require_pool_kind_matches_factory(&cfg, &sink_init.pool_kind)?;

    // H-1 / M-1: a factory-created CLMM sink MUST commit its seed amounts so
    // `Settle` seeds an exact, price-pinned ratio and rejects undershoot. The
    // seed-the-live-balance fallback (both `expected_*` omitted) is repriceable
    // by anyone who donates to the sink before settle, so it is confined to the
    // direct-instantiate debug path (`instantiate_sink`) and forbidden here on
    // the production factory path. (`instantiate_sink` still enforces the
    // both-or-neither + non-zero invariants.)
    if matches!(sink_init.pool_kind, PoolKind::Clmm { .. })
        && (sink_init.expected_token.is_none() || sink_init.expected_pair.is_none())
    {
        return Err(ContractError::CommittedAmountsRequiredForClmm {});
    }

    let label = format!("{}-{}", SINK_LABEL_PREFIX, sink_init.token_denom);

    let init_msg = to_json_binary(&InstantiateMsg::Sink(sink_init.clone()))?;
    let msg = CosmosMsg::Wasm(WasmMsg::Instantiate2 {
        // No admin → sink is immutable post-instantiate. Auditors don't need
        // to chase a migration vector on individual sinks; the factory's
        // admin only controls future `sink_code_id`, not in-flight sinks.
        admin: None,
        code_id: cfg.sink_code_id,
        label,
        msg: init_msg,
        funds: vec![],
        salt,
    });

    Ok(Response::new()
        .add_message(msg)
        .add_attribute("action", "create_sink")
        .add_attribute("sink_code_id", cfg.sink_code_id.to_string())
        .add_attribute("token_denom", sink_init.token_denom)
        .add_attribute("pair_denom", sink_init.pair_denom))
}

fn exec_create_locker(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    salt: Binary,
    locker_init: LockerInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_factory(deps.as_ref(), "create_locker")?;
    if cfg.paused {
        return Err(ContractError::Paused {});
    }

    // A locker only makes sense alongside CLMM graduation; pin its `manager`
    // to the factory's CLMM manager so a locker can't be aimed at an
    // unrelated NFT contract.
    let clmm_manager = cfg
        .clmm_manager
        .as_ref()
        .ok_or(ContractError::ClmmNotConfigured {})?;
    if locker_init.manager != clmm_manager.as_str() {
        return Err(ContractError::SinkClmmAddressMismatch {
            which: "manager".to_string(),
            got: locker_init.manager.clone(),
            expected: clmm_manager.to_string(),
        });
    }

    let label = format!("{}-{}", LOCKER_LABEL_PREFIX, locker_init.creator);
    let init_msg = to_json_binary(&InstantiateMsg::Locker(locker_init.clone()))?;
    let msg = CosmosMsg::Wasm(WasmMsg::Instantiate2 {
        // No admin → locker code is immutable; the only mutable knob is the
        // optional `treasury` rotation inside the locker itself.
        admin: None,
        code_id: cfg.sink_code_id,
        label,
        msg: init_msg,
        funds: vec![],
        salt,
    });

    Ok(Response::new()
        .add_message(msg)
        .add_attribute("action", "create_locker")
        .add_attribute("code_id", cfg.sink_code_id.to_string())
        .add_attribute("manager", locker_init.manager)
        .add_attribute("treasury", locker_init.treasury)
        .add_attribute("creator", locker_init.creator))
}

/// Step 1 of the two-step admin rotation: park `new_admin` as pending. The
/// live `admin` is untouched until the pending key accepts, so a typo'd /
/// uncontrolled target can never brick governance.
fn exec_update_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_admin: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "update_admin")?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_admin = deps.api.addr_validate(&new_admin)?;
    cfg.pending_admin = Some(new_admin.clone());
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_admin")
        .add_attribute("pending_admin", new_admin))
}

/// Step 2 of the rotation: the pending admin claims the role.
fn exec_accept_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "accept_admin")?;
    let pending = cfg
        .pending_admin
        .clone()
        .ok_or(ContractError::NoPendingAdmin {})?;
    if info.sender != pending {
        return Err(ContractError::Unauthorized {});
    }
    cfg.admin = pending.clone();
    cfg.pending_admin = None;
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "accept_admin")
        .add_attribute("new_admin", pending))
}

fn exec_set_paused(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    paused: bool,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "set_paused")?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    cfg.paused = paused;
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "set_paused")
        .add_attribute("paused", paused.to_string()))
}

/// Re-point the XYK `choice_factory` (e.g. after an XYK redeploy). Future
/// sinks only — already-spawned sinks carry their own snapshot.
fn exec_update_choice_factory(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_choice_factory: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "update_choice_factory")?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_choice_factory = deps.api.addr_validate(&new_choice_factory)?;
    cfg.choice_factory = new_choice_factory.clone();
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_choice_factory")
        .add_attribute("new_choice_factory", new_choice_factory))
}

/// Re-point (or disable) the CLMM factory + manager. All-or-nothing, mirroring
/// instantiate: both set to repoint/enable, both `None` to disable CLMM
/// graduation. Future sinks only.
fn exec_update_clmm_addresses(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    clmm_factory: Option<String>,
    clmm_manager: Option<String>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "update_clmm_addresses")?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    let (clmm_factory, clmm_manager) = match (clmm_factory, clmm_manager) {
        (Some(f), Some(m)) => (
            Some(deps.api.addr_validate(&f)?),
            Some(deps.api.addr_validate(&m)?),
        ),
        (None, None) => (None, None),
        _ => return Err(ContractError::ClmmAddressesHalfSet {}),
    };
    cfg.clmm_factory = clmm_factory.clone();
    cfg.clmm_manager = clmm_manager.clone();
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_clmm_addresses")
        .add_attribute(
            "clmm_factory",
            clmm_factory.map(|a| a.to_string()).unwrap_or_default(),
        )
        .add_attribute(
            "clmm_manager",
            clmm_manager.map(|a| a.to_string()).unwrap_or_default(),
        ))
}

/// Consult the parent factory's circuit breaker before settling. Fails CLOSED
/// (P1-A): if the factory contract EXISTS but its config can't be read — it was
/// migrated to an incompatible schema, or any other read failure — settlement
/// is blocked rather than silently proceeding into a possibly-broken DEX, which
/// is exactly the incident the breaker exists to stop. The previous version
/// `unwrap_or(false)`'d every read error, so a single failed query silently
/// defeated the pause.
///
/// Two cases are deliberately NOT blocked: a sink with no recorded factory (the
/// direct-instantiate debug path), and a factory address that hosts no contract
/// code at all. The latter is unreachable on the real path — a sink's `factory`
/// is the contract that `Instantiate2`'d it, which always has code — so it only
/// arises in unit fixtures; there is no live breaker to consult, so blocking
/// would be wrong.
fn ensure_factory_not_paused(
    deps: Deps<InjectiveQueryWrapper>,
    factory: &Option<cosmwasm_std::Addr>,
) -> Result<(), ContractError> {
    let Some(f) = factory else {
        return Ok(());
    };
    match deps
        .querier
        .query_wasm_smart::<FactoryConfigResponse>(f, &QueryMsg::FactoryConfig {})
    {
        Ok(cfg) => {
            if cfg.paused {
                Err(ContractError::Paused {})
            } else {
                Ok(())
            }
        }
        Err(read_err) => {
            // Factory exists but its config is unreadable → fail CLOSED. Only a
            // code-less address (never the real factory) is allowed through.
            if deps.querier.query_wasm_contract_info(f.as_str()).is_ok() {
                Err(ContractError::FactoryUnreadable {
                    factory: f.to_string(),
                    reason: read_err.to_string(),
                })
            } else {
                Ok(())
            }
        }
    }
}

fn exec_update_sink_code_id(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_sink_code_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_factory(deps.as_ref(), "update_sink_code_id")?;
    if info.sender != cfg.admin {
        return Err(ContractError::Unauthorized {});
    }
    cfg.sink_code_id = new_sink_code_id;
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_sink_code_id")
        .add_attribute("new_sink_code_id", new_sink_code_id.to_string()))
}

// ------------------------------------------------------------------------
// Sink-side
// ------------------------------------------------------------------------

fn exec_settle(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_sink(deps.as_ref(), "settle")?;
    let mut state = SINK_STATE.load(deps.storage)?;
    if state.status != SinkStatus::Pending {
        return Err(ContractError::SinkTerminal {
            status: format!("{:?}", state.status),
        });
    }
    // Honour the parent factory's circuit breaker: during a paused incident,
    // freeze settlement into a potentially-broken DEX even though `Settle` is
    // otherwise permissionless. Fails CLOSED if the factory exists but its
    // config can't be read (P1-A).
    ensure_factory_not_paused(deps.as_ref(), &cfg.factory)?;

    let token_bal = deps
        .querier
        .query_balance(&env.contract.address, &cfg.token_denom)?
        .amount;
    let pair_bal = deps
        .querier
        .query_balance(&env.contract.address, &cfg.pair_denom)?
        .amount;
    if token_bal.is_zero() || pair_bal.is_zero() {
        return Err(ContractError::InsufficientBalanceForSettle {
            token: token_bal.to_string(),
            pair: pair_bal.to_string(),
        });
    }

    // Mark terminal *before* dispatching messages: this contract has no
    // re-entry path today, but in CW a bug elsewhere that re-enters this
    // handler would skip the recheck if we flipped the bit after — flipping
    // first is the safer default and the in-tx revert still cleans up on
    // failure.
    state.status = SinkStatus::Settled;
    SINK_STATE.save(deps.storage, &state)?;

    match cfg.pool_kind.clone() {
        PoolKindStored::Xyk { choice_factory, .. } => {
            settle_xyk(deps, env, info, &cfg, &choice_factory, token_bal, pair_bal)
        }
        PoolKindStored::Clmm {
            clmm_factory,
            clmm_manager,
            fee_tier,
            position_recipient,
            max_fee_multiple,
        } => settle_clmm(
            deps,
            env,
            info,
            &cfg,
            ClmmSettleParams {
                clmm_factory,
                clmm_manager,
                fee_tier,
                position_recipient,
                max_fee_multiple,
            },
            token_bal,
            pair_bal,
        ),
    }
}

/// Resolve the amount to seed for one leg (H-1 / M-1). With a committed
/// `expected`, require the live `available` balance to have reached it (else the
/// full graduation deposit has not landed — refuse rather than seed a
/// partial/skewed pool) and seed EXACTLY `expected`, ignoring any surplus. With
/// no commitment (legacy debug path), seed the whole `available` balance.
fn resolve_seed(
    available: Uint128,
    expected: Option<Uint128>,
    which: &str,
) -> Result<Uint128, ContractError> {
    match expected {
        Some(e) => {
            if available < e {
                return Err(ContractError::SeedBelowCommitted {
                    which: which.to_string(),
                    available: available.to_string(),
                    expected: e.to_string(),
                });
            }
            Ok(e)
        }
        None => Ok(available),
    }
}

fn settle_xyk(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cfg: &SinkConfig,
    choice_factory: &cosmwasm_std::Addr,
    token_bal: Uint128,
    pair_bal: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // Tokenfactory `create_pair` fee. The chain debits this from the
    // contract's bank balance at dispatch time; we require `info.funds` to
    // match the fee EXACTLY (denom-set equal, amounts equal). That makes the
    // post-fee seed balance trivially `bal - info.funds[denom]` for every
    // denom — no over-pay refund path, no chance of mistakenly depositing
    // the caller's fee contribution into the pool. The keeper queries the
    // live chain fee in preflight; on a governance fee change it just
    // retries with the new value.
    let create_fee: Vec<Coin> = query_token_factory_denom_create_fee(&deps.querier)?;
    require_exact_create_fee_funds(&info, &create_fee)?;

    let caller_pair_funds = info
        .funds
        .iter()
        .find(|c| c.denom == cfg.pair_denom)
        .map(|c| c.amount)
        .unwrap_or_default();
    let caller_token_funds = info
        .funds
        .iter()
        .find(|c| c.denom == cfg.token_denom)
        .map(|c| c.amount)
        .unwrap_or_default();
    let avail_pair = pair_bal.checked_sub(caller_pair_funds)?;
    let avail_token = token_bal.checked_sub(caller_token_funds)?;
    if avail_pair.is_zero() || avail_token.is_zero() {
        return Err(ContractError::InsufficientBalanceForSettle {
            token: avail_token.to_string(),
            pair: avail_pair.to_string(),
        });
    }
    // H-1 / M-1: seed EXACTLY the committed amounts (rejecting a still-partial
    // deposit), so a donation that inflated `avail_*` can't skew the opening
    // pool ratio. Any surplus above the committed seed is swept to the
    // refund/issuer legs by the trailing `SweepDust`. Uncommitted (debug) sinks
    // fall back to seeding the live balance.
    let seed_token = resolve_seed(avail_token, cfg.expected_token, "token")?;
    let seed_pair = resolve_seed(avail_pair, cfg.expected_pair, "pair")?;

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: cfg.token_denom.clone(),
            },
            // `factory.create_pair` ignores `amount` — only the AssetInfo
            // matters for pair lookup / decimals query. Pass zero.
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: cfg.pair_denom.clone(),
            },
            amount: Uint128::zero(),
        },
    ];

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();

    messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: choice_factory.to_string(),
        msg: to_json_binary(&FactoryExecuteMsg::CreatePair { assets })?,
        funds: create_fee,
    }));

    // Callback chain: each step is a self-`WasmMsg::Execute`, processed
    // depth-first in CW so messages emitted by `ProvideLiquidity` run before
    // `DistributeLp` starts. The full pair balance seeds the pool — the cranker
    // takes no tip (P1-B).
    messages.push(self_callback(
        &env,
        CallbackMsg::ProvideLiquidity {
            token_amount: seed_token,
            pair_amount: seed_pair,
        },
    )?);
    messages.push(self_callback(&env, CallbackMsg::DistributeLp {})?);
    // H-1/M-1: route any committed-seed surplus (donation above the committed
    // amount, plus the pair contract's own one-sided refund) out of the
    // terminal sink so nothing strands.
    messages.push(self_callback(&env, CallbackMsg::SweepDust {})?);

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "settle")
        .add_attribute("pool_kind", "xyk")
        .add_attribute("caller", info.sender)
        .add_attribute("token_amount", seed_token)
        .add_attribute("pair_amount", seed_pair))
}

struct ClmmSettleParams {
    clmm_factory: cosmwasm_std::Addr,
    clmm_manager: cosmwasm_std::Addr,
    fee_tier: u32,
    position_recipient: cosmwasm_std::Addr,
    /// `None` = CLMM factory default (2x base).
    max_fee_multiple: Option<u32>,
}

fn settle_clmm(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cfg: &SinkConfig,
    params: ClmmSettleParams,
    token_bal: Uint128,
    pair_bal: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // CLMM pool creation is free, so the sink should hold *only* the seed
    // balances. Reject any attached funds rather than silently folding them
    // into the deposit.
    if !info.funds.is_empty() {
        return Err(ContractError::UnexpectedFundsForClmmSettle {});
    }

    let ClmmSettleParams {
        clmm_factory,
        clmm_manager,
        fee_tier,
        position_recipient,
        max_fee_multiple,
    } = params;

    // H-1 / M-1: seed EXACTLY the committed amounts (rejecting a still-partial
    // deposit). `init_sqrt_price_from_amounts` below is then computed from the
    // committed ratio, NOT the live balance — so a donation bank-sent to the
    // sink can't move the opening price. The trailing `SweepDust` routes any
    // surplus (donation + the manager's one-sided refund) out of the sink.
    // Uncommitted (debug) sinks fall back to the whole live balance.
    let seed_token = resolve_seed(token_bal, cfg.expected_token, "token")?;
    let pair_deposit = resolve_seed(pair_bal, cfg.expected_pair, "pair")?;

    // Full-range bounds for this fee tier's spacing.
    let tick_spacing = query_fee_tier_spacing(deps.as_ref(), &clmm_factory, fee_tier)?;
    let (tick_lower, tick_upper) = full_range_ticks(tick_spacing)?;

    // Sort the two native denoms into (token0, token1) using the SAME ordering
    // the CLMM factory applies, and carry each side's seed amount along.
    let token_ai = ClmmAssetInfo::NativeToken {
        denom: cfg.token_denom.clone(),
    };
    let pair_ai = ClmmAssetInfo::NativeToken {
        denom: cfg.pair_denom.clone(),
    };
    let (token0, token1, amount0, amount1) = if token_ai < pair_ai {
        (token_ai, pair_ai, seed_token, pair_deposit)
    } else {
        (pair_ai, token_ai, pair_deposit, seed_token)
    };

    // Guard: refuse to seed into a pool someone pre-created (and thus
    // pre-priced) — our seed ratio would mismatch its price and the mint would
    // be lopsided. The keeper can then triage (different fee tier, or Refund).
    if let Some(pool) = query_clmm_pool(deps.as_ref(), &clmm_factory, &token0, &token1, fee_tier)? {
        return Err(ContractError::ClmmPoolAlreadyExists { pool });
    }

    // Initial price that makes the full-range mint draw both balances at the
    // seed ratio.
    let init_sqrt_price: Uint256 = init_sqrt_price_from_amounts(amount0, amount1)?;

    // 1. Create the pool at our price (no funds; reply inside the factory
    //    registers it before the next top-level message runs).
    let create_pool_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: clmm_factory.to_string(),
        msg: to_json_binary(&ClmmFactoryExecuteMsg::CreatePool {
            token_a: token0.clone(),
            token_b: token1.clone(),
            fee: fee_tier,
            init_sqrt_price,
            max_fee_multiple,
        })?,
        funds: vec![],
    });

    // 2. Mint a full-range position to `position_recipient`, attaching both
    //    seed amounts. The manager resolves the pool, computes liquidity, and
    //    refunds any one-sided surplus back to this sink. `amount*_min = 0` is
    //    safe: `init_sqrt_price_from_amounts` errored out above if the seed
    //    ratio could not be priced in-range, so the pool price matches the seed
    //    ratio and the mint draws both sides; we created it in this same atomic
    //    tx, so no one can move the price between create and mint. `deadline = 0`
    //    disables the manager's deadline check.
    let mut mint_funds = vec![
        Coin {
            denom: token0.key().to_string(),
            amount: amount0,
        },
        Coin {
            denom: token1.key().to_string(),
            amount: amount1,
        },
    ];
    mint_funds.sort_by(|a, b| a.denom.cmp(&b.denom));

    let mint_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: clmm_manager.to_string(),
        msg: to_json_binary(&ClmmManagerExecuteMsg::MintPosition {
            token0: token0.clone(),
            token1: token1.clone(),
            fee: fee_tier,
            tick_lower,
            tick_upper,
            amount0_desired: amount0,
            amount1_desired: amount1,
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            recipient: Some(position_recipient.to_string()),
            deadline: 0,
        })?,
        funds: mint_funds,
    });

    // OBSERVABILITY: stage the context the `REPLY_CLMM_MINT` success reply needs
    // to (a) parse the minted `token_id` out of the manager's emitted attributes
    // and (b) query the just-created pool's address from the factory. Written
    // before dispatch; removed by the reply. If the settle tx reverts anywhere,
    // this write rolls back with it.
    PENDING_CLMM_MINT.save(
        deps.storage,
        &PendingClmmMint {
            clmm_factory: clmm_factory.clone(),
            token0_denom: token0.key().to_string(),
            token1_denom: token1.key().to_string(),
            fee: fee_tier,
        },
    )?;

    // OBSERVABILITY: dispatch the mint as a `ReplyOn::Success` sub-message so the
    // reply can record `pool_addr` + `position_token_id` on `SinkState`.
    // `ReplyOn::Success` preserves atomicity — the reply runs ONLY if the mint
    // sub-tree succeeded, and a failing mint (or any later error in this settle
    // tx) still reverts everything. The sibling `SweepDust` top-level message is
    // dispatched after the mint's full sub-tree (mint + reply), so ordering is
    // unchanged: CreatePool → MintPosition (+reply) → SweepDust.
    let mint_submsg = SubMsg::reply_on_success(mint_msg, REPLY_CLMM_MINT);

    // 3. Sweep whatever dust the manager refunded.
    let sweep_msg = self_callback(&env, CallbackMsg::SweepDust {})?;

    Ok(Response::new()
        .add_message(create_pool_msg)
        .add_submessage(mint_submsg)
        .add_message(sweep_msg)
        .add_attribute("action", "settle")
        .add_attribute("pool_kind", "clmm")
        .add_attribute("caller", info.sender)
        .add_attribute("clmm_factory", clmm_factory)
        .add_attribute("clmm_manager", clmm_manager)
        .add_attribute("fee_tier", fee_tier.to_string())
        .add_attribute("token0", token0.key())
        .add_attribute("token1", token1.key())
        .add_attribute("amount0", amount0)
        .add_attribute("amount1", amount1)
        .add_attribute("tick_lower", tick_lower.to_string())
        .add_attribute("tick_upper", tick_upper.to_string())
        .add_attribute("init_sqrt_price", init_sqrt_price.to_string())
        .add_attribute("position_recipient", position_recipient))
}

/// OBSERVABILITY: handle the `MintPosition` success reply. Records the minted
/// position `token_id` (parsed from the manager's emitted `wasm` attributes)
/// and the seeded pool address (queried from the factory's `GetPool`, now that
/// the pool exists) onto `SinkState`, then emits both as attributes.
///
/// Atomicity: this only runs after the mint succeeded (`ReplyOn::Success`), and
/// any error returned here reverts the whole settle tx. We therefore treat a
/// missing `token_id` attribute as a hard error rather than silently skipping —
/// the data is a required graduation invariant, and the manager always emits it.
/// `pool_addr` is best-effort: if the post-mint `GetPool` somehow can't be read
/// we leave it `None` (the pool unquestionably exists by now and is
/// re-derivable) rather than fail an otherwise-successful graduation.
fn reply_clmm_mint(
    deps: DepsMut<InjectiveQueryWrapper>,
    reply: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let pending = PENDING_CLMM_MINT.load(deps.storage)?;
    PENDING_CLMM_MINT.remove(deps.storage);

    // Parse the minted `token_id` out of the sub-execution's emitted events.
    // The manager attaches `("token_id", <decimal>)` to its `MintPosition`
    // response (a `wasm` event attribute), which surfaces here in
    // `reply.result`'s events.
    let token_id = token_id_from_reply(&reply).ok_or(ContractError::MintReplyMissingTokenId {})?;

    // Resolve the just-created pool address. It exists now (the mint succeeded
    // against it), so `GetPool` returns it. Best-effort — see fn doc.
    let token0_ai = ClmmAssetInfo::NativeToken {
        denom: pending.token0_denom.clone(),
    };
    let token1_ai = ClmmAssetInfo::NativeToken {
        denom: pending.token1_denom.clone(),
    };
    let pool_addr = query_clmm_pool(
        deps.as_ref(),
        &pending.clmm_factory,
        &token0_ai,
        &token1_ai,
        pending.fee,
    )?
    .and_then(|p| deps.api.addr_validate(&p).ok());

    let mut state = SINK_STATE.load(deps.storage)?;
    state.pool_addr = pool_addr.clone();
    state.position_token_id = Some(token_id.clone());
    SINK_STATE.save(deps.storage, &state)?;

    Ok(Response::new()
        .add_attribute("action", "reply_clmm_mint")
        .add_attribute("position_token_id", token_id)
        .add_attribute(
            "pool_addr",
            pool_addr.map(|a| a.to_string()).unwrap_or_default(),
        ))
}

/// Scan a reply's `wasm` event attributes for the manager-emitted `token_id`.
/// Returns the LAST `token_id` seen so a (hypothetical) wrapping event can't
/// shadow the manager's own; in practice the mint emits exactly one.
fn token_id_from_reply(reply: &Reply) -> Option<String> {
    let SubMsgResult::Ok(ref resp) = reply.result else {
        // `ReplyOn::Success` guarantees this arm, but be defensive.
        return None;
    };
    let mut found: Option<String> = None;
    for event in &resp.events {
        // Manager attributes land under the `wasm` event (cosmwasm namespaces
        // contract-emitted attributes there). Accept any event carrying a
        // `token_id` to stay robust to event-type prefixing across SDK versions.
        for attr in &event.attributes {
            if attr.key == "token_id" {
                found = Some(attr.value.clone());
            }
        }
    }
    found
}

/// Permissionless refund, available once `deadline_seconds` past instantiate.
fn exec_refund(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_sink(deps.as_ref(), "refund")?;
    let state = SINK_STATE.load(deps.storage)?;
    if state.status != SinkStatus::Pending {
        return Err(ContractError::SinkTerminal {
            status: format!("{:?}", state.status),
        });
    }

    let now = env.block.time.seconds();
    let deadline = cfg.instantiated_at.saturating_add(cfg.deadline_seconds);
    if now < deadline {
        return Err(ContractError::RefundDeadlineNotReached {
            remaining_seconds: deadline - now,
        });
    }

    // S-1(b): close the post-deadline Settle/Refund race. Once the deadline
    // passes, `Refund` is permissionless — but a committed sink that ACTUALLY
    // HOLDS both committed legs can still `Settle` into a healthy pool. Letting
    // anyone refund it then would permanently deny graduation. So: for a
    // committed sink (both `expected_*` set), refuse the permissionless refund
    // when the live balances are `>=` BOTH committed amounts (i.e. it is
    // settleable). A short leg means the full graduation deposit never landed,
    // so a refund is the correct terminal state and is allowed (as before).
    // Only the admin `ForceRefund` (which never calls this function) may
    // override, for genuinely-unsettleable-but-funded sinks (e.g. an
    // out-of-range seed ratio that trips `SeedRatioOutOfRange`).
    if let (Some(exp_token), Some(exp_pair)) = (cfg.expected_token, cfg.expected_pair) {
        let token_bal = deps
            .querier
            .query_balance(&env.contract.address, &cfg.token_denom)?
            .amount;
        let pair_bal = deps
            .querier
            .query_balance(&env.contract.address, &cfg.pair_denom)?
            .amount;
        if token_bal >= exp_token && pair_bal >= exp_pair {
            return Err(ContractError::SinkIsSettleableUseSettle {});
        }
    }

    do_refund(deps, env, &cfg, state, "refund")
}

/// Admin-gated emergency refund of a `Pending` sink, BYPASSING the deadline.
/// The recovery lever for a sink that cannot settle (out-of-range seed ratio,
/// unsupported fee tier) or one caught in a paused incident — instead of
/// waiting out `deadline_seconds`, the factory admin reclaims the seed now.
/// Authenticated against the parent factory's `admin`.
fn exec_force_refund(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_sink(deps.as_ref(), "force_refund")?;
    let state = SINK_STATE.load(deps.storage)?;
    if state.status != SinkStatus::Pending {
        return Err(ContractError::SinkTerminal {
            status: format!("{:?}", state.status),
        });
    }

    // Authenticate against the parent factory's admin (the sink has no admin
    // of its own). The direct-instantiate debug path has no factory and so no
    // force-refund — it must wait out the deadline like any other caller.
    let factory = cfg
        .factory
        .clone()
        .ok_or(ContractError::SinkHasNoFactory {})?;
    let fcfg: FactoryConfigResponse = deps
        .querier
        .query_wasm_smart(&factory, &QueryMsg::FactoryConfig {})?;
    if info.sender.as_str() != fcfg.admin {
        return Err(ContractError::Unauthorized {});
    }

    do_refund(deps, env, &cfg, state, "force_refund")
}

/// Shared refund body: route the sink's `token_denom` back to the issuer and
/// `pair_denom` to the refund receiver, flip the sink terminal. Callers gate
/// access (deadline vs. admin) before invoking.
/// Route EVERY residual bank balance out of the sink (M-2): the launch
/// `token_denom` back to the `issuer` (which burns it via `RefundFailedLaunch`,
/// leaving no zombie supply), and every OTHER denom — the pair plus any
/// stray/donated denom — to the `refund_receiver`. Denom-agnostic (enumerates
/// `query_all_balances`) so nothing strands in a terminal sink, unlike the old
/// token-and-pair-only routing. Returns the messages plus the routed token /
/// pair amounts for logging.
fn route_residual(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    cfg: &SinkConfig,
) -> StdResult<(Vec<CosmosMsg<InjectiveMsgWrapper>>, Uint128, Uint128)> {
    // `query_all_balances` is deprecated (doesn't scale with many denoms) but a
    // sink holds at most token + pair + the odd stray/donated denom, and we
    // genuinely need denom-agnostic enumeration so nothing strands. Same
    // justification as the locker's `callback_distribute_fees`.
    #[allow(deprecated)]
    let balances = deps.querier.query_all_balances(&env.contract.address)?;
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();
    let mut token_routed = Uint128::zero();
    let mut pair_routed = Uint128::zero();
    for coin in balances {
        if coin.amount.is_zero() {
            continue;
        }
        let to_address = if coin.denom == cfg.token_denom {
            token_routed = coin.amount;
            cfg.issuer.to_string()
        } else {
            if coin.denom == cfg.pair_denom {
                pair_routed = coin.amount;
            }
            cfg.refund_receiver.to_string()
        };
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address,
            amount: vec![coin],
        }));
    }
    Ok((messages, token_routed, pair_routed))
}

fn do_refund(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    cfg: &SinkConfig,
    mut state: SinkState,
    action: &str,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let (messages, token_routed, pair_routed) = route_residual(deps.as_ref(), &env, cfg)?;
    if messages.is_empty() {
        return Err(ContractError::NothingToRefund {});
    }

    state.status = SinkStatus::Refunded;
    SINK_STATE.save(deps.storage, &state)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", action)
        .add_attribute("token_refunded", token_routed)
        .add_attribute("pair_refunded", pair_routed)
        .add_attribute("issuer", cfg.issuer.to_string())
        .add_attribute("refund_receiver", cfg.refund_receiver.to_string()))
}

fn exec_callback(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cb: CallbackMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if info.sender != env.contract.address {
        return Err(ContractError::CallbackUnauthorized {});
    }
    // Role check per callback: the sink-side settlement chain requires a sink
    // instance; the locker-side `DistributeFees` requires a locker. Defense in
    // depth on top of the `sender == self` gate above.
    match cb {
        CallbackMsg::ProvideLiquidity {
            token_amount,
            pair_amount,
        } => {
            let _ = require_sink(deps.as_ref(), "callback")?;
            callback_provide_liquidity(deps, env, token_amount, pair_amount)
        }
        CallbackMsg::DistributeLp {} => {
            let _ = require_sink(deps.as_ref(), "callback")?;
            callback_distribute_lp(deps, env)
        }
        CallbackMsg::SweepDust {} => {
            let _ = require_sink(deps.as_ref(), "callback")?;
            callback_sweep_dust(deps, env)
        }
        CallbackMsg::DistributeFees {} => {
            let cfg = require_locker(deps.as_ref(), "callback")?;
            callback_distribute_fees(deps, env, cfg)
        }
    }
}

/// Post-CLMM-mint cleanup: the manager refunds one-sided surplus to this sink,
/// so any `token_denom` / `pair_denom` left here is dust. Route it the same way
/// as `exec_refund` — launch-token dust back to the `issuer` (which can burn it
/// cleanly, leaving no zombie launch-denom supply on the CW side) and pair dust
/// to `refund_receiver`. No-op (and no error) when both balances are zero.
fn callback_sweep_dust(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = SINK_CONFIG.load(deps.storage)?;

    // M-2: route ALL residual denoms, not just token + pair — the committed-seed
    // surplus, the manager/pair one-sided refund, AND any stray donated denom.
    let (messages, token_dust, pair_dust) = route_residual(deps.as_ref(), &env, &cfg)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "callback_sweep_dust")
        .add_attribute("token_dust", token_dust)
        .add_attribute("token_dust_to", cfg.issuer)
        .add_attribute("pair_dust", pair_dust))
}

fn callback_provide_liquidity(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    token_amount: Uint128,
    pair_amount: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = SINK_CONFIG.load(deps.storage)?;

    let pair_info = query_pair_info(deps.as_ref(), &cfg)?;
    let pair_addr = deps.api.addr_validate(&pair_info.contract_addr)?;

    // Persist the pair address now so `SinkState` reflects it even if
    // `DistributeLp` somehow fails. `DistributeLp` will overwrite
    // `lp_minted` after the deposit lands; that field stays `None` until
    // then.
    let mut state = SINK_STATE.load(deps.storage)?;
    state.pair_addr = Some(pair_addr.clone());
    SINK_STATE.save(deps.storage, &state)?;

    // SDK requires lexicographic ordering on bank funds. Both sides are
    // native by construction (this sink doesn't support CW20 — the seeder
    // ecosystem is bank-denom-only). Sort once.
    let mut funds = vec![
        Coin {
            denom: cfg.token_denom.clone(),
            amount: token_amount,
        },
        Coin {
            denom: cfg.pair_denom.clone(),
            amount: pair_amount,
        },
    ];
    funds.sort_by(|a, b| a.denom.cmp(&b.denom));

    let provide_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: pair_addr.to_string(),
        msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: AssetInfo::NativeToken {
                        denom: cfg.token_denom.clone(),
                    },
                    amount: token_amount,
                },
                Asset {
                    info: AssetInfo::NativeToken {
                        denom: cfg.pair_denom.clone(),
                    },
                    amount: pair_amount,
                },
            ],
            // Sink receives the LP at its own address; `DistributeLp`
            // forwards or burns it next.
            receiver: Some(env.contract.address.to_string()),
            // Initial deposit into a fresh pair — `total_share == 0`, so the
            // pair's slippage gate is a no-op. Skip the field for clarity.
            slippage_tolerance: None,
            deadline: None,
        })?,
        funds,
    });

    Ok(Response::new()
        .add_message(provide_msg)
        .add_attribute("action", "callback_provide_liquidity")
        .add_attribute("pair_addr", pair_addr)
        .add_attribute("token_amount", token_amount)
        .add_attribute("pair_amount", pair_amount))
}

fn callback_distribute_lp(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = SINK_CONFIG.load(deps.storage)?;
    let mut state = SINK_STATE.load(deps.storage)?;

    // `pair_addr` is set by `callback_provide_liquidity`. Missing here would
    // mean the chain ran `DistributeLp` without running its predecessor —
    // not possible under CW depth-first semantics, but defensive.
    let pair_addr = state
        .pair_addr
        .as_ref()
        .ok_or(ContractError::PairNotFoundPostCreate {})?
        .clone();

    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &choice::pair::QueryMsg::Pair {})?;
    let lp_denom = pair_info.liquidity_token;

    let lp_balance = deps
        .querier
        .query_balance(&env.contract.address, &lp_denom)?
        .amount;
    if lp_balance.is_zero() {
        return Err(ContractError::ZeroLpMinted {});
    }
    state.lp_minted = Some(lp_balance);
    SINK_STATE.save(deps.storage, &state)?;

    // This callback is only emitted by the XYK `Settle` path; a CLMM sink
    // never enqueues it.
    let lp_destination = match &cfg.pool_kind {
        PoolKindStored::Xyk { lp_destination, .. } => lp_destination,
        PoolKindStored::Clmm { .. } => {
            return Err(ContractError::WrongRole {
                action: "distribute_lp".to_string(),
                required: "xyk sink".to_string(),
                actual: "clmm sink".to_string(),
            })
        }
    };

    let msg: CosmosMsg<InjectiveMsgWrapper> = match lp_destination {
        LpDestinationStored::Burn => CosmosMsg::Bank(BankMsg::Burn {
            amount: coins(lp_balance.u128(), &lp_denom),
        }),
        LpDestinationStored::SendTo(addr) => CosmosMsg::Bank(BankMsg::Send {
            to_address: addr.to_string(),
            amount: coins(lp_balance.u128(), &lp_denom),
        }),
    };

    let destination_label = match lp_destination {
        LpDestinationStored::Burn => "burn".to_string(),
        LpDestinationStored::SendTo(a) => format!("send_to:{}", a),
    };

    Ok(Response::new()
        .add_message(msg)
        .add_attribute("action", "callback_distribute_lp")
        .add_attribute("lp_denom", lp_denom)
        .add_attribute("lp_amount", lp_balance)
        .add_attribute("destination", destination_label))
}

// ------------------------------------------------------------------------
// Locker-side
// ------------------------------------------------------------------------

/// Minimal mirror of `cw721::msg::TokensResponse` so the locker can enumerate
/// the NFTs it owns without taking a direct cw721 dependency.
#[derive(Deserialize)]
struct TokensResp {
    tokens: Vec<String>,
}

fn exec_collect_fees(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
    token_id: Option<String>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_locker(deps.as_ref(), "collect_fees")?;

    // Which positions to collect on. Explicit `token_id` wins; otherwise
    // enumerate the NFTs this locker owns (a launchpad locker holds exactly
    // one, but enumeration keeps the contract correct if it ever holds more).
    let token_ids: Vec<String> = match token_id {
        Some(id) => vec![id],
        None => {
            let resp: TokensResp = deps.querier.query_wasm_smart(
                &cfg.manager,
                &ClmmManagerQueryMsg::Tokens {
                    owner: env.contract.address.to_string(),
                    start_after: None,
                    limit: Some(COLLECT_FEES_PAGE),
                },
            )?;
            resp.tokens
        }
    };

    if token_ids.is_empty() {
        return Err(ContractError::LockerNoPositions {});
    }

    // One `Collect` per position, routed INTO this locker (recipient = self).
    // We can't split a single `Collect` across two recipients, so the fees
    // land here first; the chained `DistributeFees` callback (appended after
    // all collects) then partitions every collected denom between the treasury
    // and creator legs. CosmWasm executes messages sequentially, so the
    // callback observes the bank credits the collects produced.
    let self_addr = env.contract.address.to_string();
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = token_ids
        .iter()
        .map(|id| {
            Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: cfg.manager.to_string(),
                msg: to_json_binary(&ClmmManagerExecuteMsg::Collect {
                    token_id: id.clone(),
                    recipient: Some(self_addr.clone()),
                })?,
                funds: vec![],
            }))
        })
        .collect::<StdResult<Vec<_>>>()?;
    messages.push(self_callback(&env, CallbackMsg::DistributeFees {})?);

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "collect_fees")
        .add_attribute("treasury", cfg.treasury)
        .add_attribute("creator", cfg.creator)
        .add_attribute(
            "creator_fee_share_bps",
            cfg.creator_fee_share_bps.to_string(),
        )
        .add_attribute("positions", token_ids.len().to_string()))
}

/// Locker step 2: split every nonzero bank balance the `Collect`s just routed
/// into this locker between the treasury and creator legs. Treasury takes the
/// remainder (`amount - floor(amount * share / 10_000)`) so rounding dust is
/// never stranded. Denom-agnostic — iterates the locker's full balance, which
/// is only ever transient collected fees (the locker holds no principal).
fn callback_distribute_fees(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    cfg: LockerConfig,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // `query_all_balances` is deprecated because it doesn't scale with many
    // denoms, but the locker genuinely needs denom-agnostic enumeration: it
    // holds transient collected fees in arbitrary pool denoms not known ahead
    // of time. There is no non-deprecated replacement that lists all balances.
    #[allow(deprecated)]
    let balances = deps.querier.query_all_balances(&env.contract.address)?;

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();
    let mut denoms = 0u32;
    for coin in balances {
        if coin.amount.is_zero() {
            continue;
        }
        let creator_amt = coin
            .amount
            .multiply_ratio(cfg.creator_fee_share_bps as u128, BPS_DENOM as u128);
        let treasury_amt = coin.amount.checked_sub(creator_amt)?;
        if !treasury_amt.is_zero() {
            messages.push(CosmosMsg::Bank(BankMsg::Send {
                to_address: cfg.treasury.to_string(),
                amount: coins(treasury_amt.u128(), &coin.denom),
            }));
        }
        if !creator_amt.is_zero() {
            messages.push(CosmosMsg::Bank(BankMsg::Send {
                to_address: cfg.creator.to_string(),
                amount: coins(creator_amt.u128(), &coin.denom),
            }));
        }
        denoms += 1;
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "distribute_fees")
        .add_attribute("treasury", cfg.treasury)
        .add_attribute("creator", cfg.creator)
        .add_attribute(
            "creator_fee_share_bps",
            cfg.creator_fee_share_bps.to_string(),
        )
        .add_attribute("denoms", denoms.to_string()))
}

fn exec_update_treasury(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_treasury: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut cfg = require_locker(deps.as_ref(), "update_treasury")?;
    let admin = cfg.admin.clone().ok_or(ContractError::LockerNoAdmin {})?;
    if info.sender != admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_treasury = deps.api.addr_validate(&new_treasury)?;
    cfg.treasury = new_treasury.clone();
    LOCKER_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_treasury")
        .add_attribute("new_treasury", new_treasury))
}

// ------------------------------------------------------------------------
// Queries
// ------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Role {} => to_json_binary(&query_role(deps)?),
        QueryMsg::FactoryConfig {} => to_json_binary(&query_factory_config(deps)?),
        QueryMsg::SinkConfig {} => to_json_binary(&query_sink_config(deps)?),
        QueryMsg::SinkState {} => to_json_binary(&query_sink_state(deps)?),
        QueryMsg::LockerConfig {} => to_json_binary(&query_locker_config(deps)?),
    }
}

fn query_role(deps: Deps<InjectiveQueryWrapper>) -> StdResult<RoleResponse> {
    let role = ROLE.load(deps.storage)?;
    match role {
        Role::Factory => Ok(RoleResponse::Factory(query_factory_config(deps)?)),
        Role::Sink => Ok(RoleResponse::Sink {
            config: query_sink_config(deps)?,
            state: query_sink_state(deps)?,
        }),
        Role::Locker => Ok(RoleResponse::Locker(query_locker_config(deps)?)),
    }
}

fn query_factory_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<FactoryConfigResponse> {
    if ROLE.load(deps.storage)? != Role::Factory {
        return Err(StdError::generic_err("not a factory instance"));
    }
    let c = FACTORY_CONFIG.load(deps.storage)?;
    Ok(FactoryConfigResponse {
        admin: c.admin.into_string(),
        pending_admin: c.pending_admin.map(|a| a.into_string()),
        sink_code_id: c.sink_code_id,
        choice_factory: c.choice_factory.into_string(),
        clmm_factory: c.clmm_factory.map(|a| a.into_string()),
        clmm_manager: c.clmm_manager.map(|a| a.into_string()),
        paused: c.paused,
    })
}

fn query_sink_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<SinkConfigResponse> {
    if ROLE.load(deps.storage)? != Role::Sink {
        return Err(StdError::generic_err("not a sink instance"));
    }
    let c = SINK_CONFIG.load(deps.storage)?;
    Ok(SinkConfigResponse {
        factory: c.factory.map(|a| a.into_string()),
        issuer: c.issuer.into_string(),
        token_denom: c.token_denom,
        pair_denom: c.pair_denom,
        token_decimals: c.token_decimals,
        pair_decimals: c.pair_decimals,
        pool_kind: (&c.pool_kind).into(),
        refund_receiver: c.refund_receiver.into_string(),
        deadline_seconds: c.deadline_seconds,
        instantiated_at: c.instantiated_at,
        expected_token: c.expected_token,
        expected_pair: c.expected_pair,
    })
}

fn query_locker_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<LockerConfigResponse> {
    if ROLE.load(deps.storage)? != Role::Locker {
        return Err(StdError::generic_err("not a locker instance"));
    }
    let c = LOCKER_CONFIG.load(deps.storage)?;
    Ok(LockerConfigResponse {
        manager: c.manager.into_string(),
        treasury: c.treasury.into_string(),
        creator: c.creator.into_string(),
        creator_fee_share_bps: c.creator_fee_share_bps,
        admin: c.admin.map(|a| a.into_string()),
    })
}

fn query_sink_state(deps: Deps<InjectiveQueryWrapper>) -> StdResult<SinkStateResponse> {
    if ROLE.load(deps.storage)? != Role::Sink {
        return Err(StdError::generic_err("not a sink instance"));
    }
    let s = SINK_STATE.load(deps.storage)?;
    Ok(SinkStateResponse {
        status: s.status,
        pair_addr: s.pair_addr.map(|a| a.into_string()),
        lp_minted: s.lp_minted,
        pool_addr: s.pool_addr.map(|a| a.into_string()),
        position_token_id: s.position_token_id,
    })
}

// ------------------------------------------------------------------------
// Migrate
// ------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    msg: MigrateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let current = cw2::get_contract_version(deps.storage)?;
    if current.contract != CONTRACT_NAME {
        return Err(ContractError::MigrationWrongContract {
            found: current.contract,
            expected: CONTRACT_NAME.to_string(),
        });
    }
    match msg {
        MigrateMsg::FromV1 {} => {
            if !current.version.starts_with("1.") {
                return Err(ContractError::InvalidMigration {
                    from: current.version,
                    requested: "from_v1".to_string(),
                });
            }
            // No v1 → v2 schema delta yet; the variant exists so consumers
            // can compile against a stable migrate-msg shape. When a v2
            // schema bump lands, do the state conversion here.
            set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
            Ok(Response::new()
                .add_attribute("action", "migrate")
                .add_attribute("variant", "from_v1")
                .add_attribute("from_version", current.version)
                .add_attribute("to_version", CONTRACT_VERSION))
        }
        MigrateMsg::Patch {} => {
            // P2-A: accept an in-MAJOR patch against the CURRENT major, not a
            // hard-coded `1.`. The old `starts_with("1.")` bricked every patch
            // once the contract shipped a 2.x build (both variants rejected),
            // leaving no incident-patch path. `FromV1` still handles the
            // cross-major 1.x → current hop.
            let cur_major = parse_major(&current.version);
            let this_major = parse_major(CONTRACT_VERSION);
            if cur_major.is_none() || cur_major != this_major {
                return Err(ContractError::InvalidMigration {
                    from: current.version,
                    requested: "patch".to_string(),
                });
            }
            set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
            Ok(Response::new()
                .add_attribute("action", "migrate")
                .add_attribute("variant", "patch")
                .add_attribute("from_version", current.version)
                .add_attribute("to_version", CONTRACT_VERSION))
        }
    }
}

/// Parse the leading `major` component of a semver string (e.g. `"2.3.1"` →
/// `Some(2)`). Returns `None` if the string has no parseable leading integer.
fn parse_major(version: &str) -> Option<u64> {
    version
        .split('.')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
}

// ------------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------------

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Factory => "factory",
        Role::Sink => "sink",
        Role::Locker => "locker",
    }
}

fn require_factory(
    deps: Deps<InjectiveQueryWrapper>,
    action: &str,
) -> Result<FactoryConfig, ContractError> {
    let role = ROLE.load(deps.storage)?;
    match role {
        Role::Factory => Ok(FACTORY_CONFIG.load(deps.storage)?),
        other => Err(ContractError::WrongRole {
            action: action.to_string(),
            required: "factory".to_string(),
            actual: role_name(other).to_string(),
        }),
    }
}

fn require_sink(
    deps: Deps<InjectiveQueryWrapper>,
    action: &str,
) -> Result<SinkConfig, ContractError> {
    let role = ROLE.load(deps.storage)?;
    match role {
        Role::Sink => Ok(SINK_CONFIG.load(deps.storage)?),
        other => Err(ContractError::WrongRole {
            action: action.to_string(),
            required: "sink".to_string(),
            actual: role_name(other).to_string(),
        }),
    }
}

fn require_locker(
    deps: Deps<InjectiveQueryWrapper>,
    action: &str,
) -> Result<LockerConfig, ContractError> {
    let role = ROLE.load(deps.storage)?;
    match role {
        Role::Locker => Ok(LOCKER_CONFIG.load(deps.storage)?),
        other => Err(ContractError::WrongRole {
            action: action.to_string(),
            required: "locker".to_string(),
            actual: role_name(other).to_string(),
        }),
    }
}

/// Validate a `CreateSink` payload's `pool_kind` against the factory's pinned
/// DEX addresses. Reject (don't rewrite) on mismatch so the caller sees what's
/// wrong.
fn require_pool_kind_matches_factory(
    cfg: &FactoryConfig,
    pool_kind: &PoolKind,
) -> Result<(), ContractError> {
    match pool_kind {
        PoolKind::Xyk { choice_factory, .. } => {
            if choice_factory != cfg.choice_factory.as_str() {
                return Err(ContractError::SinkChoiceFactoryMismatch {
                    got: choice_factory.clone(),
                    expected: cfg.choice_factory.to_string(),
                });
            }
        }
        PoolKind::Clmm {
            clmm_factory,
            clmm_manager,
            ..
        } => {
            let pinned_factory = cfg
                .clmm_factory
                .as_ref()
                .ok_or(ContractError::ClmmNotConfigured {})?;
            let pinned_manager = cfg
                .clmm_manager
                .as_ref()
                .ok_or(ContractError::ClmmNotConfigured {})?;
            if clmm_factory != pinned_factory.as_str() {
                return Err(ContractError::SinkClmmAddressMismatch {
                    which: "factory".to_string(),
                    got: clmm_factory.clone(),
                    expected: pinned_factory.to_string(),
                });
            }
            if clmm_manager != pinned_manager.as_str() {
                return Err(ContractError::SinkClmmAddressMismatch {
                    which: "manager".to_string(),
                    got: clmm_manager.clone(),
                    expected: pinned_manager.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Look up the tick spacing for `fee` on the CLMM factory. Scans the enabled
/// fee tiers (default set is ≤4 entries; one page of 100 covers any realistic
/// config).
fn query_fee_tier_spacing(
    deps: Deps<InjectiveQueryWrapper>,
    clmm_factory: &cosmwasm_std::Addr,
    fee: u32,
) -> Result<u32, ContractError> {
    let tiers: Vec<FeeTierEntry> = deps.querier.query_wasm_smart(
        clmm_factory,
        &ClmmFactoryQueryMsg::GetFeeTiers {
            start_after: None,
            limit: Some(100),
        },
    )?;
    tiers
        .into_iter()
        .find(|t| t.fee == fee)
        .map(|t| t.tick_spacing)
        .ok_or(ContractError::FeeTierNotSupported { fee })
}

/// Returns `Some(pool_address)` if a CLMM pool already exists for
/// `(token0, token1, fee)`, else `None`.
///
/// S-4 (fail-closed analysis): the natural hardening would be to let a genuine
/// query ERROR propagate (abort settle) and map only a structured "absent"
/// response to `None`. That is only possible if the factory distinguishes
/// "no pool" from a query failure. It does NOT: `choice_clmm_factory`'s
/// `GetPool` is `#[returns(String)]` and its handler does `POOLS.load(...)?`,
/// which returns `StdError::NotFound` on absence — the SAME error channel a
/// genuine failure (bad schema, wrong address) uses. There is no "absent"
/// sentinel to key on, so we cannot fail closed here without also blocking the
/// (common, expected) absent case and bricking every first-time graduation.
///
/// We therefore RETAIN the `res.ok()` behaviour (any error ⇒ treated as
/// absent). The backstop is atomicity: this guard is a courtesy that lets the
/// keeper triage a pre-priced pool with a clear `ClmmPoolAlreadyExists` error.
/// If the guard is wrong (a pool exists but the query errored), the subsequent
/// `clmm_factory.CreatePool` in the SAME atomic settle tx reverts on the
/// duplicate — the pool is never double-created and no value is lost.
fn query_clmm_pool(
    deps: Deps<InjectiveQueryWrapper>,
    clmm_factory: &cosmwasm_std::Addr,
    token0: &ClmmAssetInfo,
    token1: &ClmmAssetInfo,
    fee: u32,
) -> Result<Option<String>, ContractError> {
    let res: StdResult<String> = deps.querier.query_wasm_smart(
        clmm_factory,
        &ClmmFactoryQueryMsg::GetPool {
            token_a: token0.clone(),
            token_b: token1.clone(),
            fee,
        },
    );
    Ok(res.ok())
}

/// Require `info.funds` to be exactly the chain's tokenfactory create fee —
/// same denom set, same per-denom amounts. Over-pay and extra denoms are
/// rejected (rather than refunded) so the post-fee seed balance is trivially
/// `bal - info.funds[denom]`: no chance of the caller's fee contribution
/// leaking into the pool deposit, and no refund-vs-deposit ordering hazard.
/// The keeper reads the live chain fee in preflight and on a governance fee
/// change retries with the new value.
pub(crate) fn require_exact_create_fee_funds(
    info: &MessageInfo,
    create_fee: &[Coin],
) -> Result<(), ContractError> {
    for fee in create_fee {
        let supplied = info
            .funds
            .iter()
            .find(|c| c.denom == fee.denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        if supplied < fee.amount {
            return Err(ContractError::InsufficientCreateFee {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
        if supplied > fee.amount {
            return Err(ContractError::CreateFeeOverpaid {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
    }
    for c in info.funds.iter() {
        if !create_fee.iter().any(|f| f.denom == c.denom) {
            return Err(ContractError::UnexpectedFundsDenom {
                denom: c.denom.clone(),
            });
        }
    }
    Ok(())
}

fn self_callback(env: &Env, cb: CallbackMsg) -> StdResult<CosmosMsg<InjectiveMsgWrapper>> {
    Ok(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(cb))?,
        funds: vec![],
    }))
}

/// `factory.Pair { asset_infos }` against this sink's `choice_factory` for
/// the (`token_denom`, `pair_denom`) pair. Returns the live `PairInfo`.
///
/// `choice_factory.pair_key` sorts the asset_infos before keying storage,
/// so ordering doesn't matter at query time. Errors are mapped to
/// `PairNotFoundPostCreate` so the caller sees a meaningful failure rather
/// than a generic StdError.
fn query_pair_info(
    deps: Deps<InjectiveQueryWrapper>,
    cfg: &SinkConfig,
) -> Result<PairInfo, ContractError> {
    // Only reachable from the XYK `Settle` path.
    let choice_factory = match &cfg.pool_kind {
        PoolKindStored::Xyk { choice_factory, .. } => choice_factory,
        PoolKindStored::Clmm { .. } => return Err(ContractError::PairNotFoundPostCreate {}),
    };
    let asset_infos = [
        AssetInfo::NativeToken {
            denom: cfg.token_denom.clone(),
        },
        AssetInfo::NativeToken {
            denom: cfg.pair_denom.clone(),
        },
    ];
    deps.querier
        .query_wasm_smart(choice_factory, &ChoiceFactoryQueryMsg::Pair { asset_infos })
        .map_err(|_| ContractError::PairNotFoundPostCreate {})
}

use crate::error::ContractError;
use crate::msg::{
    CallbackMsg, ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg, LpDestination,
    MigrateMsg, QueryMsg, RoleResponse, SinkConfigResponse, SinkInit, SinkStateResponse,
};
use crate::state::{
    FactoryConfig, LpDestinationStored, Role, SinkConfig, SinkState, SinkStatus, FACTORY_CONFIG,
    ROLE, SINK_CONFIG, SINK_STATE,
};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Response, StdError, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{ExecuteMsg as FactoryExecuteMsg, QueryMsg as ChoiceFactoryQueryMsg};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use choice::querier::query_token_factory_denom_create_fee;

use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::InjectiveMsgWrapper;

const CONTRACT_NAME: &str = "crates.io:choice-pool-seeder";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard ceiling on `FactoryInit.max_tip_bps`. 1000 bps == 10%. The factory's
/// admin can set anything `≤ HARD_MAX_TIP_BPS`; the per-sink `tip_bps` is
/// then bounded by the factory's chosen `max_tip_bps`. Belt-and-suspenders
/// against a misconfigured factory.
pub const HARD_MAX_TIP_BPS: u16 = 1000;

/// Label used by `Instantiate2` for spawned sinks. The chain prepends a
/// 64-byte truncated identifier so this just needs to be human-recognizable.
const SINK_LABEL_PREFIX: &str = "choice-pool-seeder-sink";

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
    }
}

fn instantiate_factory(
    deps: DepsMut<InjectiveQueryWrapper>,
    _info: MessageInfo,
    init: FactoryInit,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if init.max_tip_bps > HARD_MAX_TIP_BPS {
        return Err(ContractError::TipTooHigh {
            value: init.max_tip_bps,
            max: HARD_MAX_TIP_BPS,
        });
    }

    let admin = deps.api.addr_validate(&init.admin)?;
    let choice_factory = deps.api.addr_validate(&init.choice_factory)?;

    ROLE.save(deps.storage, &Role::Factory)?;
    FACTORY_CONFIG.save(
        deps.storage,
        &FactoryConfig {
            admin: admin.clone(),
            sink_code_id: init.sink_code_id,
            choice_factory: choice_factory.clone(),
            max_tip_bps: init.max_tip_bps,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("role", "factory")
        .add_attribute("admin", admin)
        .add_attribute("sink_code_id", init.sink_code_id.to_string())
        .add_attribute("choice_factory", choice_factory)
        .add_attribute("max_tip_bps", init.max_tip_bps.to_string()))
}

fn instantiate_sink(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
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
    // The factory's `CreateSink` enforces the per-instance `max_tip_bps`;
    // the sink itself only re-checks the hard ceiling so a direct-instantiate
    // path can't smuggle a runaway tip.
    if init.tip_bps > HARD_MAX_TIP_BPS {
        return Err(ContractError::TipTooHigh {
            value: init.tip_bps,
            max: HARD_MAX_TIP_BPS,
        });
    }

    let issuer = deps.api.addr_validate(&init.issuer)?;
    let refund_receiver = deps.api.addr_validate(&init.refund_receiver)?;
    let choice_factory = deps.api.addr_validate(&init.choice_factory)?;
    let lp_destination = match init.lp_destination {
        LpDestination::Burn => LpDestinationStored::Burn,
        LpDestination::SendTo(s) => LpDestinationStored::SendTo(deps.api.addr_validate(&s)?),
    };

    ROLE.save(deps.storage, &Role::Sink)?;
    SINK_CONFIG.save(
        deps.storage,
        &SinkConfig {
            issuer: issuer.clone(),
            token_denom: init.token_denom.clone(),
            pair_denom: init.pair_denom.clone(),
            token_decimals: init.token_decimals,
            pair_decimals: init.pair_decimals,
            lp_destination,
            refund_receiver: refund_receiver.clone(),
            deadline_seconds: init.deadline_seconds,
            instantiated_at: env.block.time.seconds(),
            tip_bps: init.tip_bps,
            choice_factory: choice_factory.clone(),
        },
    )?;
    SINK_STATE.save(
        deps.storage,
        &SinkState {
            status: SinkStatus::Pending,
            pair_addr: None,
            lp_minted: None,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("role", "sink")
        .add_attribute("issuer", issuer)
        .add_attribute("token_denom", init.token_denom)
        .add_attribute("pair_denom", init.pair_denom)
        .add_attribute("refund_receiver", refund_receiver)
        .add_attribute("choice_factory", choice_factory)
        .add_attribute("deadline_seconds", init.deadline_seconds.to_string())
        .add_attribute("tip_bps", init.tip_bps.to_string()))
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
        ExecuteMsg::Settle {} => exec_settle(deps, env, info),
        ExecuteMsg::Refund {} => exec_refund(deps, env, info),
        ExecuteMsg::UpdateAdmin { new_admin } => exec_update_admin(deps, info, new_admin),
        ExecuteMsg::UpdateSinkCodeId { new_sink_code_id } => {
            exec_update_sink_code_id(deps, info, new_sink_code_id)
        }
        ExecuteMsg::Callback(cb) => exec_callback(deps, env, info, cb),
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

    if sink_init.tip_bps > cfg.max_tip_bps {
        return Err(ContractError::TipTooHigh {
            value: sink_init.tip_bps,
            max: cfg.max_tip_bps,
        });
    }
    // Reject — don't silently rewrite — so the caller sees what's wrong. The
    // factory's pinned `choice_factory` is the only DEX a sink it spawns will
    // talk to; consumers must construct `sink_init` against the same value.
    if sink_init.choice_factory != cfg.choice_factory.as_str() {
        return Err(ContractError::SinkChoiceFactoryMismatch {
            got: sink_init.choice_factory.clone(),
            expected: cfg.choice_factory.to_string(),
        });
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
        .add_attribute("pair_denom", sink_init.pair_denom)
        .add_attribute("tip_bps", sink_init.tip_bps.to_string()))
}

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
    cfg.admin = new_admin.clone();
    FACTORY_CONFIG.save(deps.storage, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_admin")
        .add_attribute("new_admin", new_admin))
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
    let seed_pair = pair_bal.checked_sub(caller_pair_funds)?;
    let seed_token = token_bal.checked_sub(caller_token_funds)?;
    if seed_pair.is_zero() || seed_token.is_zero() {
        return Err(ContractError::InsufficientBalanceForSettle {
            token: seed_token.to_string(),
            pair: seed_pair.to_string(),
        });
    }

    // Tip is taken from the pair-asset side — the side with real value. The
    // launch token has no market price until the pool exists, so paying tips
    // in `token_denom` would just print free tokens to the cranker.
    let tip = if cfg.tip_bps == 0 {
        Uint128::zero()
    } else {
        seed_pair.multiply_ratio(cfg.tip_bps as u128, 10_000u128)
    };
    let pair_deposit = seed_pair.checked_sub(tip)?;
    if pair_deposit.is_zero() {
        return Err(ContractError::TipExhaustsBalance {});
    }

    // Mark terminal *before* dispatching messages: this contract has no
    // re-entry path today, but in CW a bug elsewhere that re-enters this
    // handler would skip the recheck if we flipped the bit after — flipping
    // first is the safer default and the in-tx revert still cleans up on
    // failure.
    state.status = SinkStatus::Settled;
    SINK_STATE.save(deps.storage, &state)?;

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

    if !tip.is_zero() {
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: coins(tip.u128(), &cfg.pair_denom),
        }));
    }

    messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: cfg.choice_factory.to_string(),
        msg: to_json_binary(&FactoryExecuteMsg::CreatePair { assets })?,
        funds: create_fee,
    }));

    // Callback chain: each step is a self-`WasmMsg::Execute`, processed
    // depth-first in CW so messages emitted by `ProvideLiquidity` run before
    // `DistributeLp` starts.
    messages.push(self_callback(
        &env,
        CallbackMsg::ProvideLiquidity {
            token_amount: seed_token,
            pair_amount: pair_deposit,
        },
    )?);
    messages.push(self_callback(&env, CallbackMsg::DistributeLp {})?);

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "settle")
        .add_attribute("caller", info.sender)
        .add_attribute("token_amount", seed_token)
        .add_attribute("pair_balance", seed_pair)
        .add_attribute("pair_deposit", pair_deposit)
        .add_attribute("tip", tip))
}

fn exec_refund(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let cfg = require_sink(deps.as_ref(), "refund")?;
    let mut state = SINK_STATE.load(deps.storage)?;
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

    let token_bal = deps
        .querier
        .query_balance(&env.contract.address, &cfg.token_denom)?
        .amount;
    let pair_bal = deps
        .querier
        .query_balance(&env.contract.address, &cfg.pair_denom)?
        .amount;

    if token_bal.is_zero() && pair_bal.is_zero() {
        return Err(ContractError::NothingToRefund {});
    }

    state.status = SinkStatus::Refunded;
    SINK_STATE.save(deps.storage, &state)?;

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();
    if !token_bal.is_zero() {
        // Token side back to the issuer — which has `RefundFailedLaunch` to
        // burn it cleanly, so no zombie supply lingers on the CW side.
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: cfg.issuer.to_string(),
            amount: coins(token_bal.u128(), &cfg.token_denom),
        }));
    }
    if !pair_bal.is_zero() {
        // Pair side to the consumer dApp's refund authority (for SHROOM,
        // LaunchpadCore's proportional-refund accounting).
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: cfg.refund_receiver.to_string(),
            amount: coins(pair_bal.u128(), &cfg.pair_denom),
        }));
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "refund")
        .add_attribute("token_refunded", token_bal)
        .add_attribute("pair_refunded", pair_bal)
        .add_attribute("issuer", cfg.issuer)
        .add_attribute("refund_receiver", cfg.refund_receiver))
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
    // `Callback` is only ever emitted by sink-side `Settle`, but the role
    // check provides defense in depth in case a future change wires
    // callbacks on the factory side too.
    let _ = require_sink(deps.as_ref(), "callback")?;
    match cb {
        CallbackMsg::ProvideLiquidity {
            token_amount,
            pair_amount,
        } => callback_provide_liquidity(deps, env, token_amount, pair_amount),
        CallbackMsg::DistributeLp {} => callback_distribute_lp(deps, env),
    }
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

    let pair_info: PairInfo = deps.querier.query_wasm_smart(
        &pair_addr,
        &choice::pair::QueryMsg::Pair {},
    )?;
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

    let msg: CosmosMsg<InjectiveMsgWrapper> = match &cfg.lp_destination {
        LpDestinationStored::Burn => CosmosMsg::Bank(BankMsg::Burn {
            amount: coins(lp_balance.u128(), &lp_denom),
        }),
        LpDestinationStored::SendTo(addr) => CosmosMsg::Bank(BankMsg::Send {
            to_address: addr.to_string(),
            amount: coins(lp_balance.u128(), &lp_denom),
        }),
    };

    let destination_label = match &cfg.lp_destination {
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
// Queries
// ------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Role {} => to_json_binary(&query_role(deps)?),
        QueryMsg::FactoryConfig {} => to_json_binary(&query_factory_config(deps)?),
        QueryMsg::SinkConfig {} => to_json_binary(&query_sink_config(deps)?),
        QueryMsg::SinkState {} => to_json_binary(&query_sink_state(deps)?),
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
    }
}

fn query_factory_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<FactoryConfigResponse> {
    if ROLE.load(deps.storage)? != Role::Factory {
        return Err(StdError::generic_err("not a factory instance"));
    }
    let c = FACTORY_CONFIG.load(deps.storage)?;
    Ok(FactoryConfigResponse {
        admin: c.admin.into_string(),
        sink_code_id: c.sink_code_id,
        choice_factory: c.choice_factory.into_string(),
        max_tip_bps: c.max_tip_bps,
    })
}

fn query_sink_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<SinkConfigResponse> {
    if ROLE.load(deps.storage)? != Role::Sink {
        return Err(StdError::generic_err("not a sink instance"));
    }
    let c = SINK_CONFIG.load(deps.storage)?;
    Ok(SinkConfigResponse {
        issuer: c.issuer.into_string(),
        token_denom: c.token_denom,
        pair_denom: c.pair_denom,
        token_decimals: c.token_decimals,
        pair_decimals: c.pair_decimals,
        lp_destination: (&c.lp_destination).into(),
        refund_receiver: c.refund_receiver.into_string(),
        deadline_seconds: c.deadline_seconds,
        instantiated_at: c.instantiated_at,
        tip_bps: c.tip_bps,
        choice_factory: c.choice_factory.into_string(),
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
            if !current.version.starts_with("1.") {
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

// ------------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------------

fn require_factory(
    deps: Deps<InjectiveQueryWrapper>,
    action: &str,
) -> Result<FactoryConfig, ContractError> {
    let role = ROLE.load(deps.storage)?;
    match role {
        Role::Factory => Ok(FACTORY_CONFIG.load(deps.storage)?),
        Role::Sink => Err(ContractError::WrongRole {
            action: action.to_string(),
            required: "factory".to_string(),
            actual: "sink".to_string(),
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
        Role::Factory => Err(ContractError::WrongRole {
            action: action.to_string(),
            required: "sink".to_string(),
            actual: "factory".to_string(),
        }),
    }
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

fn self_callback(
    env: &Env,
    cb: CallbackMsg,
) -> StdResult<CosmosMsg<InjectiveMsgWrapper>> {
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
    let asset_infos = [
        AssetInfo::NativeToken {
            denom: cfg.token_denom.clone(),
        },
        AssetInfo::NativeToken {
            denom: cfg.pair_denom.clone(),
        },
    ];
    deps.querier
        .query_wasm_smart(
            &cfg.choice_factory,
            &ChoiceFactoryQueryMsg::Pair { asset_infos },
        )
        .map_err(|_| ContractError::PairNotFoundPostCreate {})
}


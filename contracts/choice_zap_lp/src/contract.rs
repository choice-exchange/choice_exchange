use crate::error::ContractError;
use crate::math::optimal_swap_in;
use crate::msg::{
    CallbackMsg, ConfigResponse, ExecuteMsg, InstantiateMsg, IsKeeperResponse, KeepersResponse,
    MigrateMsg, QueryMsg, RouteResponse, RoutesResponse, SimulateZapResponse,
};
use crate::state::{Config, CONFIG, KEEPERS, ROUTES};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps, DepsMut, Empty, Env,
    MessageInfo, Order, Response, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::pair::{
    ExecuteMsg as PairExecuteMsg, PoolResponse as PairPoolResponse, QueryMsg as PairQueryMsg,
    SimulationResponse as PairSimulationResponse,
};

use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::InjectiveMsgWrapper;

const CONTRACT_NAME: &str = "crates.io:choice-zap-lp";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default `max_spread` for the swap step (0.5%).
const DEFAULT_MAX_SPREAD_PERMILLE: u64 = 5;
/// Default `slippage_tolerance` for the LP step (1%).
const DEFAULT_SLIPPAGE_PERMILLE: u64 = 10;
/// Hard cap on `tip_bps` (1%). Belt-and-suspenders against a misclick on
/// UpdateConfig draining royalties to keepers.
pub const MAX_TIP_BPS: u16 = 100;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let owner = match msg.owner {
        Some(o) => deps.api.addr_validate(&o)?,
        None => info.sender.clone(),
    };
    let default_recipient = msg
        .default_recipient
        .map(|r| deps.api.addr_validate(&r))
        .transpose()?;
    let tip_bps = msg.tip_bps.unwrap_or(0);
    if tip_bps > MAX_TIP_BPS {
        return Err(ContractError::TipTooHigh {
            value: tip_bps,
            max: MAX_TIP_BPS,
        });
    }
    let min_zap_amount = msg.min_zap_amount.unwrap_or_default();

    CONFIG.save(
        deps.storage,
        &Config {
            owner: owner.clone(),
            default_recipient: default_recipient.clone(),
            tip_bps,
            min_zap_amount,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", owner)
        .add_attribute(
            "default_recipient",
            default_recipient
                .map(|a| a.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute("tip_bps", tip_bps.to_string())
        .add_attribute("min_zap_amount", min_zap_amount))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg {
        ExecuteMsg::Zap {
            pair,
            recipient,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        } => execute_zap(
            deps,
            env,
            info,
            pair,
            recipient,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        ),
        ExecuteMsg::ZapBalance {
            input_denom,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        } => execute_zap_balance(
            deps,
            env,
            info,
            input_denom,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        ),
        ExecuteMsg::RegisterRoute { input_denom, pair } => {
            execute_register_route(deps, info, input_denom, pair)
        }
        ExecuteMsg::UnregisterRoute { input_denom } => {
            execute_unregister_route(deps, info, input_denom)
        }
        ExecuteMsg::AddKeeper { address } => execute_add_keeper(deps, info, address),
        ExecuteMsg::RemoveKeeper { address } => execute_remove_keeper(deps, info, address),
        ExecuteMsg::UpdateConfig {
            owner,
            default_recipient,
            tip_bps,
            min_zap_amount,
        } => execute_update_config(deps, info, owner, default_recipient, tip_bps, min_zap_amount),
        ExecuteMsg::Sweep { recipient, denoms } => execute_sweep(deps, env, info, recipient, denoms),
        ExecuteMsg::Callback(cb) => execute_callback(deps, env, info, cb),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_zap(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    pair: String,
    recipient: Option<String>,
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
    min_lp_out: Option<Uint128>,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;

    if info.funds.len() != 1 {
        return Err(ContractError::InvalidInputFunds {
            count: info.funds.len(),
        });
    }
    let input_coin = info.funds[0].clone();
    if input_coin.amount.is_zero() {
        return Err(ContractError::ZeroInputAmount {});
    }

    let pair_addr = deps.api.addr_validate(&pair)?;
    // Recipient defaults to the caller. Per-call override is safe here because
    // snapshots (below) ensure only the caller's own funds flow to recipient.
    let recipient_addr = match recipient {
        Some(r) => deps.api.addr_validate(&r)?,
        None => info.sender.clone(),
    };

    // Pair metadata: need both denoms to snapshot their pre-zap balances. The
    // pair is queried again inside `plan_zap` (single source of truth for the
    // checks); the small redundancy keeps the signature clean.
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pair {})?;
    let denom_0 = native_denom(&pair_info.asset_infos[0])?;
    let denom_1 = native_denom(&pair_info.asset_infos[1])?;
    let (snap_denom_a, snap_denom_b) = if input_coin.denom == denom_0 {
        (denom_0.clone(), denom_1.clone())
    } else if input_coin.denom == denom_1 {
        (denom_1.clone(), denom_0.clone())
    } else {
        return Err(ContractError::InputDenomMismatch {
            denom: input_coin.denom,
        });
    };

    // `bal_a_now` already includes `info.funds` — subtract it to recover the
    // baseline. This is what makes the user-facing zap safely permissionless:
    // anything in the contract before this call is not reachable by this
    // call's recipient.
    let bal_a_now = deps
        .querier
        .query_balance(&env.contract.address, snap_denom_a.clone())?
        .amount;
    let pre_a = bal_a_now.checked_sub(input_coin.amount)?;
    let pre_b = deps
        .querier
        .query_balance(&env.contract.address, snap_denom_b.clone())?
        .amount;
    let pre_lp = deps
        .querier
        .query_balance(&env.contract.address, pair_info.liquidity_token.clone())?
        .amount;

    let plan = plan_zap(
        deps.as_ref(),
        &env,
        &pair_addr,
        &input_coin.denom,
        input_coin.amount,
        &recipient_addr,
        pre_a,
        pre_b,
        pre_lp,
        max_spread,
        slippage_tolerance,
        min_lp_out,
        deadline,
    )?;

    Ok(Response::new()
        .add_messages(plan.msgs)
        .add_attribute("action", "zap")
        .add_attribute("pair", pair_addr)
        .add_attribute("input_denom", input_coin.denom)
        .add_attribute("input_amount", input_coin.amount)
        .add_attribute("swap_amount", plan.swap_amount)
        .add_attribute("recipient", recipient_addr))
}

#[allow(clippy::too_many_arguments)]
fn execute_zap_balance(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    input_denom: String,
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
    min_lp_out: Option<Uint128>,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;

    let config = CONFIG.load(deps.storage)?;

    // Auth: owner is implicitly allowed; otherwise must be in the keeper map.
    if info.sender != config.owner && !KEEPERS.has(deps.storage, &info.sender) {
        return Err(ContractError::NotKeeper {});
    }

    let recipient_addr = config
        .default_recipient
        .clone()
        .ok_or(ContractError::DefaultRecipientUnset {})?;

    // Route is owner-managed: caller cannot redirect into a fake pair.
    let pair_addr = ROUTES
        .may_load(deps.storage, input_denom.as_str())?
        .ok_or_else(|| ContractError::NoRouteForDenom {
            denom: input_denom.clone(),
        })?;

    let balance = deps
        .querier
        .query_balance(&env.contract.address, input_denom.clone())?
        .amount;
    if balance.is_zero() || balance < config.min_zap_amount {
        return Err(ContractError::BalanceBelowMin {
            balance: balance.to_string(),
            min: config.min_zap_amount.to_string(),
        });
    }

    // Tip first, then zap the remainder. After the tip BankMsg fires, the
    // contract's remaining balance equals exactly `input_amount`, which is
    // what `plan_zap`'s optimal split is computed on.
    let tip = if config.tip_bps == 0 {
        Uint128::zero()
    } else {
        balance.multiply_ratio(config.tip_bps as u128, 10_000u128)
    };
    let input_amount = balance.checked_sub(tip)?;
    if input_amount.is_zero() {
        return Err(ContractError::ZeroInputAmount {});
    }

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    if !tip.is_zero() {
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: vec![Coin {
                denom: input_denom.clone(),
                amount: tip,
            }],
        }));
    }

    // Drain mode: no snapshots. Everything in the contract gets swept after
    // the zap. Royalty contract holds only royalties; pre-existing balances
    // are by definition prior leftovers that belong with the recipient.
    let plan = plan_zap(
        deps.as_ref(),
        &env,
        &pair_addr,
        &input_denom,
        input_amount,
        &recipient_addr,
        Uint128::zero(),
        Uint128::zero(),
        Uint128::zero(),
        max_spread,
        slippage_tolerance,
        min_lp_out,
        deadline,
    )?;
    messages.extend(plan.msgs);

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "zap_balance")
        .add_attribute("pair", pair_addr)
        .add_attribute("input_denom", input_denom)
        .add_attribute("balance", balance)
        .add_attribute("tip", tip)
        .add_attribute("input_amount", input_amount)
        .add_attribute("swap_amount", plan.swap_amount)
        .add_attribute("caller", info.sender)
        .add_attribute("recipient", recipient_addr))
}

struct ZapPlan {
    msgs: Vec<CosmosMsg<InjectiveMsgWrapper>>,
    swap_amount: Uint128,
}

/// Shared planner used by both `execute_zap` and `execute_zap_balance`. Reads
/// pair metadata + reserves, computes the optimal split, and emits the
/// three-message chain: pair.Swap → self.Callback::ProvideLiquidity →
/// self.Callback::Sweep.
///
/// `pre_a` / `pre_b` / `pre_lp` are the baseline contract balances *not*
/// belonging to this zap. The callbacks treat them as untouchable — only the
/// deltas this call generates flow to `recipient`. `execute_zap` passes the
/// real pre-existing balances (so a user zap can't drain queued royalties);
/// `execute_zap_balance` passes zeros (drain mode).
#[allow(clippy::too_many_arguments)]
fn plan_zap(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    pair_addr: &Addr,
    input_denom: &str,
    input_amount: Uint128,
    recipient: &Addr,
    pre_a: Uint128,
    pre_b: Uint128,
    pre_lp: Uint128,
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
    min_lp_out: Option<Uint128>,
    deadline: Option<u64>,
) -> Result<ZapPlan, ContractError> {
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(pair_addr, &PairQueryMsg::Pair {})?;
    let denom_0 = native_denom(&pair_info.asset_infos[0])?;
    let denom_1 = native_denom(&pair_info.asset_infos[1])?;
    let (denom_a, denom_b) = if input_denom == denom_0 {
        (denom_0.clone(), denom_1.clone())
    } else if input_denom == denom_1 {
        (denom_1.clone(), denom_0.clone())
    } else {
        return Err(ContractError::InputDenomMismatch {
            denom: input_denom.to_string(),
        });
    };
    if denom_a == pair_info.liquidity_token || denom_b == pair_info.liquidity_token {
        return Err(ContractError::InputDenomMismatch {
            denom: pair_info.liquidity_token,
        });
    }

    let pool: PairPoolResponse = deps
        .querier
        .query_wasm_smart(pair_addr, &PairQueryMsg::Pool {})?;
    let (r_a, _r_b) = orient_reserves(&pool, &denom_a)?;
    if pool.total_share.is_zero() || r_a.is_zero() {
        return Err(ContractError::EmptyPool {});
    }

    let swap_amount = optimal_swap_in(r_a, input_amount)?;
    if swap_amount.is_zero() {
        return Err(ContractError::ZeroInputAmount {});
    }

    let max_spread = max_spread.unwrap_or_else(|| Decimal::permille(DEFAULT_MAX_SPREAD_PERMILLE));
    let slippage_tolerance =
        slippage_tolerance.unwrap_or_else(|| Decimal::permille(DEFAULT_SLIPPAGE_PERMILLE));

    let swap_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: pair_addr.to_string(),
        msg: to_json_binary(&PairExecuteMsg::Swap {
            offer_asset: Asset {
                info: AssetInfo::NativeToken {
                    denom: denom_a.clone(),
                },
                amount: swap_amount,
            },
            belief_price: None,
            max_spread: Some(max_spread),
            to: None,
            deadline,
        })?,
        funds: vec![Coin {
            denom: denom_a.clone(),
            amount: swap_amount,
        }],
    });

    let provide_cb = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity {
            pair: pair_addr.to_string(),
            denom_a: denom_a.clone(),
            denom_b: denom_b.clone(),
            pre_a,
            pre_b,
            slippage_tolerance,
            deadline,
        }))?,
        funds: vec![],
    });

    let sweep_cb = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient: recipient.to_string(),
            denom_a,
            denom_b,
            lp_denom: pair_info.liquidity_token,
            pre_a,
            pre_b,
            pre_lp,
            min_lp_out,
        }))?,
        funds: vec![],
    });

    Ok(ZapPlan {
        msgs: vec![swap_msg, provide_cb, sweep_cb],
        swap_amount,
    })
}

fn execute_callback(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cb: CallbackMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    if info.sender != env.contract.address {
        return Err(ContractError::Unauthorized {});
    }
    match cb {
        CallbackMsg::ProvideLiquidity {
            pair,
            denom_a,
            denom_b,
            pre_a,
            pre_b,
            slippage_tolerance,
            deadline,
        } => callback_provide_liquidity(
            deps,
            env,
            pair,
            denom_a,
            denom_b,
            pre_a,
            pre_b,
            slippage_tolerance,
            deadline,
        ),
        CallbackMsg::Sweep {
            recipient,
            denom_a,
            denom_b,
            lp_denom,
            pre_a,
            pre_b,
            pre_lp,
            min_lp_out,
        } => callback_sweep(
            deps, env, recipient, denom_a, denom_b, lp_denom, pre_a, pre_b, pre_lp, min_lp_out,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn callback_provide_liquidity(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    pair: String,
    denom_a: String,
    denom_b: String,
    pre_a: Uint128,
    pre_b: Uint128,
    slippage_tolerance: Decimal,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;
    let pair_addr = deps.api.addr_validate(&pair)?;

    let bal_a = deps
        .querier
        .query_balance(&env.contract.address, denom_a.clone())?
        .amount;
    let bal_b = deps
        .querier
        .query_balance(&env.contract.address, denom_b.clone())?
        .amount;

    // Work only with the freshly generated balances — pre-existing dust /
    // queued royalties stay untouched. `saturating_sub` is defensive: a pre_*
    // snapshot greater than the live balance means something external moved
    // funds out mid-zap, in which case we LP nothing rather than panic.
    let delta_a = bal_a.saturating_sub(pre_a);
    let delta_b = bal_b.saturating_sub(pre_b);

    // The pair's provide_liquidity computes `desired = pool * share / total`
    // and may round it up by 1 for the non-limiting side, which would underflow
    // `deposit - desired`. A 1-wei haircut on each side keeps us strictly below
    // the rounding ceiling without affecting share math — so we need *both*
    // deltas to be at least 2 wei. Below that, post-haircut deposit is zero,
    // funds get filtered out, and the pair's `share == 0` branch rejects with
    // `InvalidZeroAmount`. Skip the LP step instead and let the sweep forward
    // the unused deltas back to recipient.
    let one = Uint128::new(1);
    if delta_a <= one || delta_b <= one {
        return Ok(Response::new()
            .add_attribute("action", "zap_provide_skip")
            .add_attribute("delta_a", delta_a)
            .add_attribute("delta_b", delta_b));
    }
    let deposit_a = delta_a - one;
    let deposit_b = delta_b - one;

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: denom_a.clone(),
            },
            amount: deposit_a,
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: denom_b.clone(),
            },
            amount: deposit_b,
        },
    ];

    let mut funds = vec![
        Coin {
            denom: denom_a.clone(),
            amount: deposit_a,
        },
        Coin {
            denom: denom_b.clone(),
            amount: deposit_b,
        },
    ];
    // SDK requires lexicographic ordering on bank funds; the pair doesn't care.
    funds.sort_by(|a, b| a.denom.cmp(&b.denom));

    let provide_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: pair_addr.to_string(),
        msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
            assets,
            receiver: Some(env.contract.address.to_string()),
            deadline,
            slippage_tolerance: Some(slippage_tolerance),
        })?,
        funds,
    });

    Ok(Response::new()
        .add_message(provide_msg)
        .add_attribute("action", "zap_provide")
        .add_attribute("deposit_a", deposit_a)
        .add_attribute("deposit_b", deposit_b))
}

#[allow(clippy::too_many_arguments)]
fn callback_sweep(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    recipient: String,
    denom_a: String,
    denom_b: String,
    lp_denom: String,
    pre_a: Uint128,
    pre_b: Uint128,
    pre_lp: Uint128,
    min_lp_out: Option<Uint128>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let recipient_addr = deps.api.addr_validate(&recipient)?;

    let bal_a = deps
        .querier
        .query_balance(&env.contract.address, denom_a.clone())?
        .amount;
    let bal_b = deps
        .querier
        .query_balance(&env.contract.address, denom_b.clone())?
        .amount;
    let bal_lp = deps
        .querier
        .query_balance(&env.contract.address, lp_denom.clone())?
        .amount;

    // Only the freshly minted LP + the freshly produced dust leave; whatever
    // was here before this call stays. `saturating_sub` for the same reason as
    // in the provide step — a snapshot > balance means an external mover got
    // in between, so we forward nothing rather than panic.
    let lp_out = bal_lp.saturating_sub(pre_lp);
    let dust_a = bal_a.saturating_sub(pre_a);
    let dust_b = bal_b.saturating_sub(pre_b);

    if let Some(min) = min_lp_out {
        if lp_out < min {
            return Err(ContractError::MinLpAssertion {
                got: lp_out.to_string(),
                min: min.to_string(),
            });
        }
    }

    let mut coins = vec![];
    if !lp_out.is_zero() {
        coins.push(Coin {
            denom: lp_denom.clone(),
            amount: lp_out,
        });
    }
    if !dust_a.is_zero() {
        coins.push(Coin {
            denom: denom_a.clone(),
            amount: dust_a,
        });
    }
    if !dust_b.is_zero() {
        coins.push(Coin {
            denom: denom_b.clone(),
            amount: dust_b,
        });
    }

    let mut response = Response::new()
        .add_attribute("action", "zap_sweep")
        .add_attribute("recipient", recipient_addr.clone())
        .add_attribute("lp_amount", lp_out)
        .add_attribute("dust_a", dust_a)
        .add_attribute("dust_b", dust_b);

    if !coins.is_empty() {
        coins.sort_by(|x, y| x.denom.cmp(&y.denom));
        response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient_addr.to_string(),
            amount: coins,
        }));
    }
    Ok(response)
}

fn execute_update_config(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    owner: Option<String>,
    default_recipient: Option<String>,
    tip_bps: Option<u16>,
    min_zap_amount: Option<Uint128>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    if let Some(o) = owner {
        config.owner = deps.api.addr_validate(&o)?;
    }
    if let Some(r) = default_recipient {
        // Empty string clears it.
        config.default_recipient = if r.is_empty() {
            None
        } else {
            Some(deps.api.addr_validate(&r)?)
        };
    }
    if let Some(t) = tip_bps {
        if t > MAX_TIP_BPS {
            return Err(ContractError::TipTooHigh {
                value: t,
                max: MAX_TIP_BPS,
            });
        }
        config.tip_bps = t;
    }
    if let Some(m) = min_zap_amount {
        config.min_zap_amount = m;
    }
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_config")
        .add_attribute("owner", config.owner)
        .add_attribute(
            "default_recipient",
            config
                .default_recipient
                .map(|a| a.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute("tip_bps", config.tip_bps.to_string())
        .add_attribute("min_zap_amount", config.min_zap_amount))
}

fn execute_register_route(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    input_denom: String,
    pair: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    if input_denom.is_empty() {
        return Err(ContractError::InputDenomMismatch {
            denom: input_denom,
        });
    }
    let pair_addr = deps.api.addr_validate(&pair)?;
    let prior = ROUTES.may_load(deps.storage, input_denom.as_str())?;
    ROUTES.save(deps.storage, input_denom.as_str(), &pair_addr)?;
    Ok(Response::new()
        .add_attribute("action", "register_route")
        .add_attribute("input_denom", input_denom)
        .add_attribute("pair", pair_addr)
        .add_attribute(
            "previous_pair",
            prior.map(|a| a.to_string()).unwrap_or_else(|| "none".into()),
        ))
}

fn execute_unregister_route(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    input_denom: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    let existed = ROUTES.may_load(deps.storage, input_denom.as_str())?.is_some();
    ROUTES.remove(deps.storage, input_denom.as_str());
    Ok(Response::new()
        .add_attribute("action", "unregister_route")
        .add_attribute("input_denom", input_denom)
        .add_attribute("existed", existed.to_string()))
}

fn execute_add_keeper(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    address: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    let keeper = deps.api.addr_validate(&address)?;
    KEEPERS.save(deps.storage, &keeper, &Empty {})?;
    Ok(Response::new()
        .add_attribute("action", "add_keeper")
        .add_attribute("keeper", keeper))
}

fn execute_remove_keeper(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    address: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    let keeper = deps.api.addr_validate(&address)?;
    let existed = KEEPERS.has(deps.storage, &keeper);
    KEEPERS.remove(deps.storage, &keeper);
    Ok(Response::new()
        .add_attribute("action", "remove_keeper")
        .add_attribute("keeper", keeper)
        .add_attribute("existed", existed.to_string()))
}

fn execute_sweep(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    recipient: String,
    denoms: Vec<String>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    let recipient_addr = deps.api.addr_validate(&recipient)?;
    let mut coins: Vec<Coin> = vec![];
    for denom in denoms {
        let bal = deps
            .querier
            .query_balance(&env.contract.address, denom.clone())?;
        if !bal.amount.is_zero() {
            coins.push(bal);
        }
    }
    let mut response = Response::new()
        .add_attribute("action", "sweep")
        .add_attribute("recipient", recipient_addr.clone());
    if !coins.is_empty() {
        coins.sort_by(|a, b| a.denom.cmp(&b.denom));
        response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient_addr.to_string(),
            amount: coins,
        }));
    }
    Ok(response)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::SimulateZap {
            pair,
            input_denom,
            input_amount,
        } => to_json_binary(&query_simulate(deps, pair, input_denom, input_amount)?),
        QueryMsg::Route { input_denom } => to_json_binary(&query_route(deps, input_denom)?),
        QueryMsg::Routes {} => to_json_binary(&query_routes(deps)?),
        QueryMsg::Keepers {} => to_json_binary(&query_keepers(deps)?),
        QueryMsg::IsKeeper { address } => to_json_binary(&query_is_keeper(deps, address)?),
    }
}

fn query_route(deps: Deps<InjectiveQueryWrapper>, input_denom: String) -> StdResult<RouteResponse> {
    let pair = ROUTES.may_load(deps.storage, input_denom.as_str())?.ok_or_else(|| {
        cosmwasm_std::StdError::generic_err(format!("no route for {}", input_denom))
    })?;
    Ok(RouteResponse {
        input_denom,
        pair: pair.to_string(),
    })
}

fn query_routes(deps: Deps<InjectiveQueryWrapper>) -> StdResult<RoutesResponse> {
    let routes: Vec<RouteResponse> = ROUTES
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            item.map(|(k, v)| RouteResponse {
                input_denom: k,
                pair: v.to_string(),
            })
        })
        .collect::<StdResult<Vec<_>>>()?;
    Ok(RoutesResponse { routes })
}

fn query_keepers(deps: Deps<InjectiveQueryWrapper>) -> StdResult<KeepersResponse> {
    let keepers: Vec<String> = KEEPERS
        .keys(deps.storage, None, None, Order::Ascending)
        .map(|k| k.map(|a| a.to_string()))
        .collect::<StdResult<Vec<_>>>()?;
    Ok(KeepersResponse { keepers })
}

fn query_is_keeper(
    deps: Deps<InjectiveQueryWrapper>,
    address: String,
) -> StdResult<IsKeeperResponse> {
    let addr = deps.api.addr_validate(&address)?;
    let config = CONFIG.load(deps.storage)?;
    let is_keeper = addr == config.owner || KEEPERS.has(deps.storage, &addr);
    Ok(IsKeeperResponse { is_keeper })
}

fn query_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<ConfigResponse> {
    let c = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        owner: c.owner.to_string(),
        default_recipient: c.default_recipient.map(|a| a.to_string()),
        tip_bps: c.tip_bps,
        min_zap_amount: c.min_zap_amount,
    })
}

fn query_simulate(
    deps: Deps<InjectiveQueryWrapper>,
    pair: String,
    input_denom: String,
    input_amount: Uint128,
) -> StdResult<SimulateZapResponse> {
    let pair_addr = deps.api.addr_validate(&pair)?;
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pair {})?;
    let denom_0 = native_denom(&pair_info.asset_infos[0])
        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;
    let denom_1 = native_denom(&pair_info.asset_infos[1])
        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;
    let denom_a = if input_denom == denom_0 {
        denom_0.clone()
    } else if input_denom == denom_1 {
        denom_1.clone()
    } else {
        return Err(cosmwasm_std::StdError::generic_err(
            "input denom does not match pair",
        ));
    };

    let pool: PairPoolResponse = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pool {})?;
    let (r_a, _r_b) = orient_reserves(&pool, &denom_a)
        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;

    let swap_amount = optimal_swap_in(r_a, input_amount)
        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;

    // Delegate the swap-return math to the pair itself. Anything we replicated
    // locally would drift from the pair's exact rounding semantics; calling
    // `Simulation` guarantees the response matches what the live swap will
    // produce wei-for-wei (modulo concurrent state changes).
    let expected_return = if swap_amount.is_zero() {
        Uint128::zero()
    } else {
        let sim: PairSimulationResponse = deps.querier.query_wasm_smart(
            &pair_addr,
            &PairQueryMsg::Simulation {
                offer_asset: Asset {
                    info: AssetInfo::NativeToken {
                        denom: denom_a.clone(),
                    },
                    amount: swap_amount,
                },
            },
        )?;
        sim.return_amount
    };

    Ok(SimulateZapResponse {
        swap_amount,
        expected_return,
        deposit_input_side: input_amount.checked_sub(swap_amount).unwrap_or_default(),
    })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _msg: MigrateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new().add_attribute("action", "migrate"))
}

// ---------- helpers ----------

fn native_denom(info: &AssetInfo) -> Result<String, ContractError> {
    match info {
        AssetInfo::NativeToken { denom } => Ok(denom.clone()),
        AssetInfo::Token { .. } => Err(ContractError::Cw20NotSupported {}),
    }
}

fn orient_reserves(
    pool: &PairPoolResponse,
    denom_a: &str,
) -> Result<(Uint128, Uint128), ContractError> {
    let (r_0, r_1) = (pool.assets[0].amount, pool.assets[1].amount);
    match (&pool.assets[0].info, &pool.assets[1].info) {
        (AssetInfo::NativeToken { denom: d0 }, AssetInfo::NativeToken { denom: d1 }) => {
            if d0 == denom_a {
                Ok((r_0, r_1))
            } else if d1 == denom_a {
                Ok((r_1, r_0))
            } else {
                Err(ContractError::InputDenomMismatch {
                    denom: denom_a.to_string(),
                })
            }
        }
        _ => Err(ContractError::Cw20NotSupported {}),
    }
}

fn assert_deadline(blocktime: u64, deadline: Option<u64>) -> Result<(), ContractError> {
    if let Some(d) = deadline {
        if blocktime >= d {
            return Err(ContractError::ExpiredDeadline {});
        }
    }
    Ok(())
}

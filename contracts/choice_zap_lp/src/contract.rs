use crate::error::ContractError;
use crate::math::optimal_swap_in;
use crate::msg::{
    CallbackMsg, ConfigResponse, ExecuteMsg, InstantiateMsg, IsKeeperResponse, KeepersResponse,
    MigrateMsg, QueryMsg, SimulateZapResponse, ZapHookMsg,
};
use crate::state::{Config, CONFIG, KEEPERS};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    from_json, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Decimal, Deps, DepsMut,
    Empty, Env, MessageInfo, Order, QuerierWrapper, Response, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg, Expiration};

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::pair::{
    Cw20HookMsg as PairCw20HookMsg, ExecuteMsg as PairExecuteMsg, PoolResponse as PairPoolResponse,
    QueryMsg as PairQueryMsg, SimulationResponse as PairSimulationResponse,
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
/// Hard cap on caller-supplied `max_spread` and `slippage_tolerance`. The
/// pair contract imposes no upper bound of its own, so a 100% value would
/// silently disable MEV protection. Capping at 50% matches the standard
/// Cosmos AMM router default and rejects obviously-broken UI inputs without
/// blocking unusual but legitimate volatile-asset zaps.
const MAX_TOLERANCE_PERMILLE: u64 = 500;

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
    let pair = msg.pair.map(|p| deps.api.addr_validate(&p)).transpose()?;
    let input = msg
        .input
        .map(|i| validate_asset_info(deps.api, &i))
        .transpose()?;

    CONFIG.save(
        deps.storage,
        &Config {
            owner: owner.clone(),
            default_recipient: default_recipient.clone(),
            tip_bps,
            min_zap_amount,
            input: input.clone(),
            pair: pair.clone(),
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
        .add_attribute("min_zap_amount", min_zap_amount)
        .add_attribute(
            "input",
            input
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute(
            "pair",
            pair.as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ))
}

/// Bech32-validate the contract address embedded in a `Token` variant. Empty
/// denoms / addresses (degenerate cases) are rejected up front.
fn validate_asset_info(
    api: &dyn cosmwasm_std::Api,
    input: &AssetInfo,
) -> Result<AssetInfo, ContractError> {
    match input {
        AssetInfo::NativeToken { denom } => {
            if denom.is_empty() {
                return Err(ContractError::InputAssetMismatch {
                    got: input.to_string(),
                });
            }
            Ok(input.clone())
        }
        AssetInfo::Token { contract_addr } => {
            let addr = api.addr_validate(contract_addr)?;
            Ok(AssetInfo::Token {
                contract_addr: addr.to_string(),
            })
        }
    }
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
        ExecuteMsg::Receive(cw20_msg) => execute_receive(deps, env, info, cw20_msg),
        ExecuteMsg::ZapBalance {
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        } => execute_zap_balance(
            deps,
            env,
            info,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        ),
        ExecuteMsg::AddKeeper { address } => execute_add_keeper(deps, info, address),
        ExecuteMsg::RemoveKeeper { address } => execute_remove_keeper(deps, info, address),
        ExecuteMsg::UpdateConfig {
            owner,
            default_recipient,
            tip_bps,
            min_zap_amount,
            input,
            pair,
        } => execute_update_config(
            deps,
            info,
            owner,
            default_recipient,
            tip_bps,
            min_zap_amount,
            input,
            pair,
        ),
        ExecuteMsg::Sweep { recipient, assets } => {
            execute_sweep(deps, env, info, recipient, assets)
        }
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
    assert_tolerance_caps(max_spread, slippage_tolerance)?;

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
    let recipient_addr = match recipient {
        Some(r) => deps.api.addr_validate(&r)?,
        None => info.sender.clone(),
    };
    let input_info = AssetInfo::NativeToken {
        denom: input_coin.denom.clone(),
    };

    run_zap(
        deps,
        env,
        pair_addr,
        input_info,
        input_coin.amount,
        recipient_addr,
        max_spread,
        slippage_tolerance,
        min_lp_out,
        deadline,
        "zap",
    )
}

fn execute_receive(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    // `info.sender` is the CW20 token contract that just transferred us
    // `cw20_msg.amount`. `cw20_msg.sender` is the original user. We must NOT
    // accept native funds tagged onto the same call — that's a misuse pattern
    // and we'd otherwise silently take the funds.
    if !info.funds.is_empty() {
        return Err(ContractError::ReceiveWithFunds {
            count: info.funds.len(),
        });
    }
    if cw20_msg.amount.is_zero() {
        return Err(ContractError::ZeroInputAmount {});
    }

    let hook: ZapHookMsg = from_json(&cw20_msg.msg)?;
    let cw20_contract = info.sender.clone();
    match hook {
        ZapHookMsg::Zap {
            pair,
            recipient,
            max_spread,
            slippage_tolerance,
            min_lp_out,
            deadline,
        } => {
            assert_deadline(env.block.time.seconds(), deadline)?;
            assert_tolerance_caps(max_spread, slippage_tolerance)?;

            let pair_addr = deps.api.addr_validate(&pair)?;
            let original_sender = deps.api.addr_validate(&cw20_msg.sender)?;
            let recipient_addr = match recipient {
                Some(r) => deps.api.addr_validate(&r)?,
                None => original_sender,
            };
            let input_info = AssetInfo::Token {
                contract_addr: cw20_contract.to_string(),
            };

            run_zap(
                deps,
                env,
                pair_addr,
                input_info,
                cw20_msg.amount,
                recipient_addr,
                max_spread,
                slippage_tolerance,
                min_lp_out,
                deadline,
                "zap_cw20",
            )
        }
    }
}

/// Shared core for both `Zap` (native input) and `Receive`/`ZapHookMsg::Zap`
/// (CW20 input). At entry, the contract has already received `input_amount` of
/// `input_info`. The function snapshots the pair-side balances *excluding*
/// that just-received delta, then emits the three-message zap chain.
#[allow(clippy::too_many_arguments)]
fn run_zap(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    pair_addr: Addr,
    input_info: AssetInfo,
    input_amount: Uint128,
    recipient_addr: Addr,
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
    min_lp_out: Option<Uint128>,
    deadline: Option<u64>,
    action_label: &str,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pair {})?;
    let (asset_a, asset_b) = orient_assets(&pair_info, &input_info)?;

    // `bal_a_now` already includes `input_amount` (Bank credit for native /
    // CW20 transfer for tokens). Subtract it to recover the baseline. This is
    // what makes the user-facing zap safely permissionless: anything in the
    // contract before this call is not reachable by this call's recipient.
    let bal_a_now = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_a)?;
    let pre_a = bal_a_now.checked_sub(input_amount)?;
    let pre_b = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_b)?;
    let pre_lp = deps
        .querier
        .query_balance(&env.contract.address, pair_info.liquidity_token.clone())?
        .amount;

    let plan = plan_zap(
        deps.as_ref(),
        &env,
        &pair_addr,
        &pair_info,
        &asset_a,
        &asset_b,
        input_amount,
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
        .add_attribute("action", action_label)
        .add_attribute("pair", pair_addr)
        .add_attribute("input", input_info.to_string())
        .add_attribute("input_amount", input_amount)
        .add_attribute("swap_amount", plan.swap_amount)
        .add_attribute("recipient", recipient_addr))
}

#[allow(clippy::too_many_arguments)]
fn execute_zap_balance(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
    min_lp_out: Option<Uint128>,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;
    assert_tolerance_caps(max_spread, slippage_tolerance)?;

    let config = CONFIG.load(deps.storage)?;

    // Auth: owner is implicitly allowed; otherwise must be in the keeper map.
    if info.sender != config.owner && !KEEPERS.has(deps.storage, &info.sender) {
        return Err(ContractError::NotKeeper {});
    }

    let recipient_addr = config
        .default_recipient
        .clone()
        .ok_or(ContractError::DefaultRecipientUnset {})?;

    // Route must be wired (instantiate accepted None; the owner sets it later
    // via UpdateConfig). Refuse to drain anything until both sides are set.
    let pair_addr = config
        .pair
        .clone()
        .ok_or(ContractError::RoyaltyRouteUnset {})?;
    let input = config
        .input
        .clone()
        .ok_or(ContractError::RoyaltyRouteUnset {})?;

    let balance = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &input)?;
    if balance.is_zero() || balance < config.min_zap_amount {
        return Err(ContractError::BalanceBelowMin {
            balance: balance.to_string(),
            min: config.min_zap_amount.to_string(),
        });
    }

    // Tip first, then zap the remainder. After the tip transfer fires, the
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
        messages.push(transfer_asset_msg(&input, &info.sender, tip)?);
    }

    // Drain mode for the INPUT side only. Asset_b and LP are snapshotted so
    // that an accidental transfer (mis-sent royalty, airdrop, leftover from
    // an earlier in-flight zap) doesn't get bundled into the deposit and
    // trip the pair's `MaxSlippageAssertion`. Pre-existing asset_b stays
    // untouched and can be rescued by the owner via `Sweep`.
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pair {})?;
    let (asset_a, asset_b) = orient_assets(&pair_info, &input)?;

    let pre_b = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_b)?;
    let pre_lp = deps
        .querier
        .query_balance(&env.contract.address, pair_info.liquidity_token.clone())?
        .amount;

    let plan = plan_zap(
        deps.as_ref(),
        &env,
        &pair_addr,
        &pair_info,
        &asset_a,
        &asset_b,
        input_amount,
        &recipient_addr,
        Uint128::zero(),
        pre_b,
        pre_lp,
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
        .add_attribute("input", input.to_string())
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

/// Shared planner used by `run_zap` and `execute_zap_balance`. Reads pair
/// reserves, computes the optimal split, and emits the three-message chain:
/// swap → self.Callback::ProvideLiquidity → self.Callback::Sweep.
///
/// The swap message is asymmetric:
///   - native input → `pair.Swap { offer_asset }` with funds
///   - CW20 input   → `cw20.Send { contract: pair, msg: Cw20HookMsg::Swap }`
///
/// `pre_a` / `pre_b` / `pre_lp` are the baseline contract balances *not*
/// belonging to this zap. The callbacks treat them as untouchable — only the
/// deltas this call generates flow to `recipient`.
#[allow(clippy::too_many_arguments)]
fn plan_zap(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    pair_addr: &Addr,
    pair_info: &PairInfo,
    asset_a: &AssetInfo,
    asset_b: &AssetInfo,
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
    let pool: PairPoolResponse = deps
        .querier
        .query_wasm_smart(pair_addr, &PairQueryMsg::Pool {})?;
    let r_a = orient_reserve(&pool, asset_a)?;
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

    let swap_msg = build_swap_msg(pair_addr, asset_a, swap_amount, max_spread, deadline)?;

    let provide_cb = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity {
            pair: pair_addr.to_string(),
            asset_a: asset_a.clone(),
            asset_b: asset_b.clone(),
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
            asset_a: asset_a.clone(),
            asset_b: asset_b.clone(),
            lp_denom: pair_info.liquidity_token.clone(),
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
            asset_a,
            asset_b,
            pre_a,
            pre_b,
            slippage_tolerance,
            deadline,
        } => callback_provide_liquidity(
            deps,
            env,
            pair,
            asset_a,
            asset_b,
            pre_a,
            pre_b,
            slippage_tolerance,
            deadline,
        ),
        CallbackMsg::Sweep {
            recipient,
            asset_a,
            asset_b,
            lp_denom,
            pre_a,
            pre_b,
            pre_lp,
            min_lp_out,
        } => callback_sweep(
            deps, env, recipient, asset_a, asset_b, lp_denom, pre_a, pre_b, pre_lp, min_lp_out,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn callback_provide_liquidity(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    pair: String,
    asset_a: AssetInfo,
    asset_b: AssetInfo,
    pre_a: Uint128,
    pre_b: Uint128,
    slippage_tolerance: Decimal,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;
    let pair_addr = deps.api.addr_validate(&pair)?;

    let bal_a = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_a)?;
    let bal_b = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_b)?;

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
    //
    // CW20 sides don't suffer the underflow (the pair sets `remain_amount=0`
    // for them), but we apply the haircut uniformly to keep one code path.
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
            info: asset_a.clone(),
            amount: deposit_a,
        },
        Asset {
            info: asset_b.clone(),
            amount: deposit_b,
        },
    ];

    // Build the provide chain: CW20 sides need an IncreaseAllowance hop first
    // (the pair `TransferFrom`s the deposit), with `Expiration::AtHeight(h+1)`
    // so any rounding residual auto-expires next block.
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    let mut funds: Vec<Coin> = vec![];
    for (asset, deposit) in [(&asset_a, deposit_a), (&asset_b, deposit_b)] {
        match asset {
            AssetInfo::NativeToken { denom } => {
                funds.push(Coin {
                    denom: denom.clone(),
                    amount: deposit,
                });
            }
            AssetInfo::Token { contract_addr } => {
                messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.clone(),
                    msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                        spender: pair_addr.to_string(),
                        amount: deposit,
                        expires: Some(Expiration::AtHeight(env.block.height + 1)),
                    })?,
                    funds: vec![],
                }));
            }
        }
    }
    // SDK requires lexicographic ordering on bank funds; the pair doesn't care.
    funds.sort_by(|a, b| a.denom.cmp(&b.denom));

    messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: pair_addr.to_string(),
        msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
            assets,
            receiver: Some(env.contract.address.to_string()),
            deadline,
            slippage_tolerance: Some(slippage_tolerance),
        })?,
        funds,
    }));

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "zap_provide")
        .add_attribute("deposit_a", deposit_a)
        .add_attribute("deposit_b", deposit_b))
}

#[allow(clippy::too_many_arguments)]
fn callback_sweep(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    recipient: String,
    asset_a: AssetInfo,
    asset_b: AssetInfo,
    lp_denom: String,
    pre_a: Uint128,
    pre_b: Uint128,
    pre_lp: Uint128,
    min_lp_out: Option<Uint128>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let recipient_addr = deps.api.addr_validate(&recipient)?;

    let bal_a = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_a)?;
    let bal_b = query_asset_balance(&deps.querier, deps.api, &env.contract.address, &asset_b)?;
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

    // Bank-sending an `erc20:`-linked denom TO A CONTRACT trips the Injective
    // bank→EVM hook and panics the whole nested execution (code 9). The royalty
    // recipient is the Treasury cw3 contract, so its erc20 dust must stay here
    // instead of being forwarded; an EOA recipient (frontend user) is unchanged
    // and keeps the full dust return. Retained dust (~1 base unit per zap) can be
    // `Sweep`-ed to an EOA later. `Err` from the query means a non-contract addr.
    let recipient_is_contract = deps
        .querier
        .query_wasm_contract_info(recipient_addr.as_str())
        .is_ok();

    // LP is always native; native dust bundles into the same `BankMsg::Send`
    // for cheaper gas. CW20 dust goes via `Cw20::Transfer`.
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    let mut bank_coins: Vec<Coin> = vec![];
    if !lp_out.is_zero() {
        bank_coins.push(Coin {
            denom: lp_denom.clone(),
            amount: lp_out,
        });
    }
    let retained_a = push_dust(
        &asset_a,
        dust_a,
        &recipient_addr,
        recipient_is_contract,
        &mut bank_coins,
        &mut messages,
    )?;
    let retained_b = push_dust(
        &asset_b,
        dust_b,
        &recipient_addr,
        recipient_is_contract,
        &mut bank_coins,
        &mut messages,
    )?;

    if !bank_coins.is_empty() {
        bank_coins.sort_by(|x, y| x.denom.cmp(&y.denom));
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient_addr.to_string(),
            amount: bank_coins,
        }));
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "zap_sweep")
        .add_attribute("recipient", recipient_addr)
        .add_attribute("lp_amount", lp_out)
        .add_attribute("dust_a", dust_a)
        .add_attribute("dust_b", dust_b)
        .add_attribute("dust_retained_a", retained_a)
        .add_attribute("dust_retained_b", retained_b))
}

#[allow(clippy::too_many_arguments)]
fn execute_update_config(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    owner: Option<String>,
    default_recipient: Option<String>,
    tip_bps: Option<u16>,
    min_zap_amount: Option<Uint128>,
    input: Option<AssetInfo>,
    pair: Option<String>,
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
    if let Some(i) = input {
        config.input = Some(validate_asset_info(deps.api, &i)?);
    }
    if let Some(p) = pair {
        config.pair = Some(deps.api.addr_validate(&p)?);
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
        .add_attribute("min_zap_amount", config.min_zap_amount)
        .add_attribute(
            "input",
            config
                .input
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute(
            "pair",
            config
                .pair
                .as_ref()
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ))
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
    assets: Vec<AssetInfo>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    let recipient_addr = deps.api.addr_validate(&recipient)?;

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    let mut bank_coins: Vec<Coin> = vec![];
    for asset in &assets {
        let bal = query_asset_balance(&deps.querier, deps.api, &env.contract.address, asset)?;
        if bal.is_zero() {
            continue;
        }
        match asset {
            AssetInfo::NativeToken { denom } => bank_coins.push(Coin {
                denom: denom.clone(),
                amount: bal,
            }),
            AssetInfo::Token { contract_addr } => {
                messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.clone(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: recipient_addr.to_string(),
                        amount: bal,
                    })?,
                    funds: vec![],
                }))
            }
        }
    }
    if !bank_coins.is_empty() {
        bank_coins.sort_by(|a, b| a.denom.cmp(&b.denom));
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient_addr.to_string(),
            amount: bank_coins,
        }));
    }
    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "sweep")
        .add_attribute("recipient", recipient_addr))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::SimulateZap {
            pair,
            input,
            input_amount,
        } => to_json_binary(&query_simulate(deps, pair, input, input_amount)?),
        QueryMsg::Keepers {} => to_json_binary(&query_keepers(deps)?),
        QueryMsg::IsKeeper { address } => to_json_binary(&query_is_keeper(deps, address)?),
    }
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
        input: c.input,
        pair: c.pair.map(|a| a.to_string()),
    })
}

fn query_simulate(
    deps: Deps<InjectiveQueryWrapper>,
    pair: String,
    input: AssetInfo,
    input_amount: Uint128,
) -> StdResult<SimulateZapResponse> {
    let pair_addr = deps.api.addr_validate(&pair)?;
    let pair_info: PairInfo = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pair {})?;
    let (asset_a, _asset_b) = orient_assets(&pair_info, &input)
        .map_err(|e| cosmwasm_std::StdError::generic_err(e.to_string()))?;

    let pool: PairPoolResponse = deps
        .querier
        .query_wasm_smart(&pair_addr, &PairQueryMsg::Pool {})?;
    let r_a = orient_reserve(&pool, &asset_a)
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
                    info: asset_a.clone(),
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

/// Subset of v1 `Config` used to read pre-migration bytes. v1 had no
/// `input` / `pair`, so we deserialize what's present and supply the route
/// from `MigrateMsg::FromV1`. The orphaned `ROUTES` map entries from v1 are
/// left in storage — unreachable from v2 code, no functional impact.
#[derive(serde::Deserialize)]
struct LegacyConfigV1 {
    owner: Addr,
    default_recipient: Option<Addr>,
    tip_bps: u16,
    min_zap_amount: Uint128,
}

/// Migrate dispatch. The variant must match the cw2 version recorded in
/// storage — `FromV1` requires `1.x.x`, `Patch` requires `2.x.x`. This is
/// what blocks an on-chain admin from rewriting the supposedly-immutable
/// `(input, pair)` route via a v2 → v2 migration with a v1-shaped payload.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    msg: MigrateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let current = cw2::get_contract_version(deps.storage)?;

    match msg {
        MigrateMsg::FromV1 { input, pair } => {
            if !current.version.starts_with("1.") {
                return Err(ContractError::InvalidMigration {
                    from: current.version,
                    requested: "from_v1".to_string(),
                });
            }

            let raw = deps
                .storage
                .get(b"config")
                .ok_or(ContractError::MigrationStateMissing {})?;
            let legacy: LegacyConfigV1 = cosmwasm_std::from_json(&raw)?;

            let pair = deps.api.addr_validate(&pair)?;
            let input = validate_asset_info(deps.api, &input)?;

            CONFIG.save(
                deps.storage,
                &Config {
                    owner: legacy.owner,
                    default_recipient: legacy.default_recipient,
                    tip_bps: legacy.tip_bps,
                    min_zap_amount: legacy.min_zap_amount,
                    input: Some(input.clone()),
                    pair: Some(pair.clone()),
                },
            )?;

            set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
            Ok(Response::new()
                .add_attribute("action", "migrate")
                .add_attribute("variant", "from_v1")
                .add_attribute("from_version", current.version)
                .add_attribute("to_version", CONTRACT_VERSION)
                .add_attribute("input", input.to_string())
                .add_attribute("pair", pair))
        }
        MigrateMsg::Patch {} => {
            if !current.version.starts_with("2.") {
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

// ---------- helpers ----------

fn orient_assets(
    pair_info: &PairInfo,
    input: &AssetInfo,
) -> Result<(AssetInfo, AssetInfo), ContractError> {
    if pair_info.asset_infos[0].equal(input) {
        Ok((
            pair_info.asset_infos[0].clone(),
            pair_info.asset_infos[1].clone(),
        ))
    } else if pair_info.asset_infos[1].equal(input) {
        Ok((
            pair_info.asset_infos[1].clone(),
            pair_info.asset_infos[0].clone(),
        ))
    } else {
        Err(ContractError::InputAssetMismatch {
            got: input.to_string(),
        })
    }
}

fn orient_reserve(pool: &PairPoolResponse, asset_a: &AssetInfo) -> Result<Uint128, ContractError> {
    if pool.assets[0].info.equal(asset_a) {
        Ok(pool.assets[0].amount)
    } else if pool.assets[1].info.equal(asset_a) {
        Ok(pool.assets[1].amount)
    } else {
        Err(ContractError::InputAssetMismatch {
            got: asset_a.to_string(),
        })
    }
}

fn query_asset_balance(
    querier: &QuerierWrapper<InjectiveQueryWrapper>,
    api: &dyn cosmwasm_std::Api,
    contract: &Addr,
    info: &AssetInfo,
) -> StdResult<Uint128> {
    info.query_pool(querier, api, contract.clone())
}

fn transfer_asset_msg(
    info: &AssetInfo,
    recipient: &Addr,
    amount: Uint128,
) -> StdResult<CosmosMsg<InjectiveMsgWrapper>> {
    match info {
        AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Bank(BankMsg::Send {
            to_address: recipient.to_string(),
            amount: vec![Coin {
                denom: denom.clone(),
                amount,
            }],
        })),
        AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: recipient.to_string(),
                amount,
            })?,
            funds: vec![],
        })),
    }
}

/// Forward `amount` of `asset` to `recipient`, returning the amount RETAINED in
/// the zap contract (non-zero only when an `erc20:` native denom is withheld
/// from a contract recipient to dodge the bank→EVM hook panic).
fn push_dust(
    asset: &AssetInfo,
    amount: Uint128,
    recipient: &Addr,
    recipient_is_contract: bool,
    bank_coins: &mut Vec<Coin>,
    messages: &mut Vec<CosmosMsg<InjectiveMsgWrapper>>,
) -> StdResult<Uint128> {
    if amount.is_zero() {
        return Ok(Uint128::zero());
    }
    match asset {
        AssetInfo::NativeToken { denom } => {
            // `erc20:`-linked denom → contract recipient = bank→EVM hook panic.
            // Retain it here rather than forwarding (and reverting the whole zap).
            if recipient_is_contract && denom.starts_with("erc20:") {
                return Ok(amount);
            }
            bank_coins.push(Coin {
                denom: denom.clone(),
                amount,
            });
        }
        AssetInfo::Token { .. } => messages.push(transfer_asset_msg(asset, recipient, amount)?),
    }
    Ok(Uint128::zero())
}

fn assert_deadline(blocktime: u64, deadline: Option<u64>) -> Result<(), ContractError> {
    if let Some(d) = deadline {
        if blocktime >= d {
            return Err(ContractError::ExpiredDeadline {});
        }
    }
    Ok(())
}

fn assert_tolerance_caps(
    max_spread: Option<Decimal>,
    slippage_tolerance: Option<Decimal>,
) -> Result<(), ContractError> {
    let cap = Decimal::permille(MAX_TOLERANCE_PERMILLE);
    if let Some(v) = max_spread {
        if v > cap {
            return Err(ContractError::MaxSpreadTooHigh {
                value: v.to_string(),
                max: cap.to_string(),
            });
        }
    }
    if let Some(v) = slippage_tolerance {
        if v > cap {
            return Err(ContractError::SlippageToleranceTooHigh {
                value: v.to_string(),
                max: cap.to_string(),
            });
        }
    }
    Ok(())
}

// Build the swap-step message for `plan_zap`.
//   - Native input: pair.Swap with funds=[offer_asset]
//   - CW20 input:   cw20.Send(pair, amount, Cw20HookMsg::Swap)
fn build_swap_msg(
    pair_addr: &Addr,
    asset_a: &AssetInfo,
    amount: Uint128,
    max_spread: Decimal,
    deadline: Option<u64>,
) -> StdResult<CosmosMsg<InjectiveMsgWrapper>> {
    match asset_a {
        AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: pair_addr.to_string(),
            msg: to_json_binary(&PairExecuteMsg::Swap {
                offer_asset: Asset {
                    info: asset_a.clone(),
                    amount,
                },
                belief_price: None,
                max_spread: Some(max_spread),
                to: None,
                deadline,
            })?,
            funds: vec![Coin {
                denom: denom.clone(),
                amount,
            }],
        })),
        AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: pair_addr.to_string(),
                amount,
                msg: to_json_binary(&PairCw20HookMsg::Swap {
                    belief_price: None,
                    max_spread: Some(max_spread),
                    to: None,
                    deadline,
                })?,
            })?,
            funds: vec![],
        })),
    }
}

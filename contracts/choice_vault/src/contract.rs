use std::convert::TryFrom;

use choice::asset::{Asset, AssetInfo};
use choice::pair::{
    Cw20HookMsg as PairCw20HookMsg, ExecuteMsg as PairExecuteMsg, PoolResponse,
    QueryMsg as PairQueryMsg, SimulationResponse,
};
use choice::querier::{query_balance, query_token_balance};
use choice::staking::{ExecuteMsg as FarmExecuteMsg, QueryMsg as FarmQueryMsg, StakerInfoResponse};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Addr, Binary, CosmosMsg, Decimal, Deps, DepsMut, Env, Isqrt,
    MessageInfo, Order, Reply, ReplyOn, Response, StdError, StdResult, SubMsg, Uint128, Uint256,
    WasmMsg,
};
use cw2::{get_contract_version, set_contract_version};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use cw_storage_plus::Bound;

use crate::error::ContractError;
use crate::msg::{
    CompoundRoutePayload, Cw20HookMsg, ExecuteMsg, HarvestReplyPayload, InstantiateMsg, MigrateMsg,
    PendingDepositsResponse, QueryMsg, UserInfoResponse, WithdrawSharesReplyPayload,
};
use crate::state::{
    CompoundingInfo, Config, SwapHop as StateSwapHop, UserInfo, COMPOUNDER_ROTATION_DELAY_SECONDS,
    COMPOUNDING_INFO, CONFIG, MAX_SLIPPAGE_RAISE_DELAY_SECONDS, TOTAL_PENDING_DEPOSITS,
    TOTAL_SHARES, USERS,
};

const CONTRACT_NAME: &str = "crates.io:choice-vault";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HARVEST_REPLY_ID: u64 = 1;
pub const ROUTE_SWAP_REPLY_ID: u64 = 2;
pub const FINAL_SWAP_REPLY_ID: u64 = 3;
pub const PROVIDE_LIQUIDITY_REPLY_ID: u64 = 4;
pub const WITHDRAW_SHARES_REPLY_ID: u64 = 5;

/// Default value for `Config.max_slippage_tolerance` in newly-instantiated vaults.
/// Raises go through `ProposeMaxSlippageRaise` / `ApplyMaxSlippageRaise`
/// (48h timelock); tightening is instant via `TightenMaxSlippage`. Set generously
/// (25%) to cover low-liquidity pools where legitimate ops might need more
/// headroom — still well below the hard ceiling so a misconfigured tolerance
/// can't silently disable the pair-side `assert_max_spread` / asymmetric-provide
/// check.
pub const DEFAULT_MAX_SLIPPAGE_TOLERANCE: Decimal = Decimal::percent(25);

/// Absolute ceiling on `Config.max_slippage_tolerance`. Even a timelocked raise
/// cannot propose a cap above this value — backstop against nonsensical
/// configurations that the 48h exit window alone can't compensate for.
pub const MAX_SLIPPAGE_TOLERANCE_CEILING: Decimal = Decimal::percent(50);

/// Multiplier applied to the fair-market expected LP estimate when validating
/// `minimum_lp_to_receive` in `execute_compound`. Callers must commit to a
/// floor of at least `MIN_LP_HEURISTIC_K * expected_lp`. Chosen to catch
/// blatantly-low specifications (e.g. `Uint128::new(1)`) without false-
/// positive-rejecting honest callers operating at the slippage ceiling over
/// a multi-hop route.
///
/// Worst-case legitimate shortfall: `(1 - max_slippage_tolerance) ^ (hops)`.
/// For the ceiling at 50% and a 2-hop route (3 swaps), that's `0.5^3 = 12.5%`
/// of expected LP — k = 10% admits this worst case while still forcing any
/// caller to commit to >10% of fair LP. Not a tight MEV bound; per-swap
/// `belief_prices` + `assert_max_spread` remain the primary MEV defense.
/// See `audit/audit_followup_progress.md` (vault-governance batch) for the
/// derivation.
pub const MIN_LP_HEURISTIC_K: Decimal = Decimal::percent(10);

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let owner_addr = deps.api.addr_validate(&msg.owner)?;
    let pair_contract_addr = deps.api.addr_validate(&msg.pair_contract)?;
    let farm_contract_addr = deps.api.addr_validate(&msg.farm_contract)?;

    let fee_recipient_addr = if let Some(fee_recipient) = &msg.fee_recipient {
        Some(deps.api.addr_validate(fee_recipient)?)
    } else {
        None
    };

    if let Some(fee_percentage) = msg.fee_percentage {
        if fee_percentage > Decimal::one() {
            return Err(ContractError::InvalidFeePercentage {});
        }
    }

    if msg.slippage_tolerance > DEFAULT_MAX_SLIPPAGE_TOLERANCE {
        return Err(ContractError::SlippageToleranceAboveMax {
            got: msg.slippage_tolerance,
            max: DEFAULT_MAX_SLIPPAGE_TOLERANCE,
        });
    }

    let compounder_addr = deps.api.addr_validate(&msg.compounder)?;

    let mut reward_to_lp_token_route_validated: Vec<StateSwapHop> = vec![];
    for hop in msg.reward_to_lp_token_route {
        reward_to_lp_token_route_validated.push(StateSwapHop {
            pair_contract: deps.api.addr_validate(&hop.pair_contract)?,
            to_asset_info: hop.to_asset_info,
        });
    }

    // H-2: the compound flow's final 50/50 split goes through `pair_contract`, so the asset
    // that arrives at the end of the route must be one of the pair's two assets. Without this
    // guard, a misconfigured route would either revert every compound (best case) or — if the
    // intermediate pools happen to accept the orphan asset — leave rewards stranded in the
    // vault as a non-pair token that no other code path can recover.
    let terminal_asset = reward_to_lp_token_route_validated
        .last()
        .map(|hop| &hop.to_asset_info)
        .unwrap_or(&msg.reward_token);
    if terminal_asset != &msg.asset_infos[0] && terminal_asset != &msg.asset_infos[1] {
        return Err(ContractError::CompoundPathMustEndOnPairAsset {});
    }

    let config = Config {
        owner: owner_addr,
        pair_contract: pair_contract_addr,
        farm_contract: farm_contract_addr,
        lp_token: msg.lp_token,
        reward_token: msg.reward_token,
        asset_infos: msg.asset_infos,
        fee_recipient: fee_recipient_addr,
        fee_percentage: msg.fee_percentage,
        minimum_reward_to_compound: msg.minimum_reward_to_compound,
        proposed_owner: None,
        pending_compounder: None,
        pending_compounder_effective_at: None,
        compounder: compounder_addr,
        slippage_tolerance: msg.slippage_tolerance,
        reward_to_lp_token_route: reward_to_lp_token_route_validated,
        paused: false,
        max_slippage_tolerance: DEFAULT_MAX_SLIPPAGE_TOLERANCE,
        pending_max_slippage: None,
        pending_max_slippage_effective_at: None,
    };

    CONFIG.save(deps.storage, &config)?;
    TOTAL_SHARES.save(deps.storage, &Uint128::zero())?;
    TOTAL_PENDING_DEPOSITS.save(deps.storage, &Uint128::zero())?;
    COMPOUNDING_INFO.save(deps.storage, &CompoundingInfo::default())?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", msg.owner)
        .add_attribute("pair_contract", msg.pair_contract)
        .add_attribute("farm_contract", msg.farm_contract))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::Deposit {} => execute_deposit_native(deps, env, info),
        ExecuteMsg::WithdrawPending { amount } => execute_withdraw_pending(deps, env, info, amount),
        ExecuteMsg::WithdrawShares { shares_to_burn } => {
            execute_withdraw_shares(deps, env, info, shares_to_burn)
        }
        ExecuteMsg::Compound {
            belief_prices,
            minimum_lp_to_receive,
        } => execute_compound(deps, env, info, belief_prices, minimum_lp_to_receive),
        ExecuteMsg::ActivatePendingDeposits { users } => {
            execute_activate_pending_deposits(deps, env, info, users)
        }
        ExecuteMsg::ActivateMyDeposit {} => execute_activate_my_deposit(deps, env, info),
        ExecuteMsg::UpdateConfig {
            slippage_tolerance,
            fee_recipient,
            fee_percentage,
            minimum_reward_to_compound,
        } => execute_update_config(
            deps,
            info,
            slippage_tolerance,
            fee_recipient,
            fee_percentage,
            minimum_reward_to_compound,
        ),
        ExecuteMsg::ClearFeeRecipient => execute_clear_fee_recipient(deps, info),
        ExecuteMsg::Pause => execute_set_paused(deps, info, true),
        ExecuteMsg::Unpause => execute_set_paused(deps, info, false),
        ExecuteMsg::ProposeCompounder { new_compounder } => {
            execute_propose_compounder(deps, env, info, new_compounder)
        }
        ExecuteMsg::ApplyCompounderRotation => execute_apply_compounder_rotation(deps, env, info),
        ExecuteMsg::CancelCompounderProposal => execute_cancel_compounder_proposal(deps, info),
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, info, new_owner)
        }
        ExecuteMsg::AcceptOwnership => execute_accept_ownership(deps, info),
        ExecuteMsg::CancelOwnershipProposal => execute_cancel_ownership_proposal(deps, info),
        ExecuteMsg::TightenMaxSlippage { new_max } => {
            execute_tighten_max_slippage(deps, info, new_max)
        }
        ExecuteMsg::ProposeMaxSlippageRaise { new_max } => {
            execute_propose_max_slippage_raise(deps, env, info, new_max)
        }
        ExecuteMsg::ApplyMaxSlippageRaise => execute_apply_max_slippage_raise(deps, env, info),
        ExecuteMsg::CancelMaxSlippageProposal => execute_cancel_max_slippage_proposal(deps, info),
    }
}

pub fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    slippage_tolerance: Option<Decimal>,
    fee_recipient: Option<String>,
    fee_percentage: Option<Decimal>,
    minimum_reward_to_compound: Option<Uint128>,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Only the owner can update the config
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(slippage) = slippage_tolerance {
        if slippage > config.max_slippage_tolerance {
            return Err(ContractError::SlippageToleranceAboveMax {
                got: slippage,
                max: config.max_slippage_tolerance,
            });
        }
        config.slippage_tolerance = slippage;
    }

    if let Some(fee_recipient) = fee_recipient {
        config.fee_recipient = Some(deps.api.addr_validate(&fee_recipient)?);
    }

    if let Some(fee_percentage) = fee_percentage {
        // Validate that the fee percentage is not greater than 100%
        if fee_percentage > Decimal::one() {
            return Err(ContractError::InvalidFeePercentage {});
        }
        config.fee_percentage = Some(fee_percentage);
    }

    if let Some(minimum_reward) = minimum_reward_to_compound {
        config.minimum_reward_to_compound = minimum_reward;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "update_config"))
}

pub fn execute_clear_fee_recipient(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    config.fee_recipient = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new().add_attribute("action", "clear_fee_recipient"))
}

pub fn execute_set_paused(
    deps: DepsMut,
    info: MessageInfo,
    paused: bool,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }
    config.paused = paused;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", if paused { "pause" } else { "unpause" })
        .add_attribute("paused", paused.to_string()))
}

fn assert_not_paused(config: &Config) -> Result<(), ContractError> {
    if config.paused {
        return Err(ContractError::VaultPaused {});
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    // L-11: minimal migrate entry point. Guards against swapping the contract binary
    // underneath the same code id (name must match). Bumps the stored version.
    // The compound reply-chain's complexity means we want this lever available without
    // needing a contract rewrite the first time migration is needed.
    let stored = get_contract_version(deps.storage)?;
    if stored.contract != CONTRACT_NAME {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "cannot migrate: expected contract {}, found {}",
            CONTRACT_NAME, stored.contract
        ))));
    }
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", stored.version)
        .add_attribute("to_version", CONTRACT_VERSION))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> Result<Response, ContractError> {
    match msg.id {
        HARVEST_REPLY_ID => handle_harvest_reply(deps, env, msg),
        ROUTE_SWAP_REPLY_ID => handle_route_swap_reply(deps, env, msg),
        FINAL_SWAP_REPLY_ID => handle_final_swap_reply(deps, env, msg),
        PROVIDE_LIQUIDITY_REPLY_ID => handle_provide_liquidity_reply(deps, env, msg),
        WITHDRAW_SHARES_REPLY_ID => handle_withdraw_shares_reply(deps, env, msg),
        _ => Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Unknown reply id",
        ))),
    }
}

pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    assert_not_paused(&config)?;

    // Check that the configured LP token is the one sending the message
    match config.lp_token {
        AssetInfo::Token { contract_addr } => {
            if info.sender.to_string() != contract_addr {
                return Err(ContractError::Unauthorized {});
            }
        }
        AssetInfo::NativeToken { .. } => {
            // This hook should not be called for native LP tokens
            return Err(ContractError::Std(StdError::generic_err(
                "Receive hook called for a native LP token vault",
            )));
        }
    }

    match from_json(&cw20_msg.msg)? {
        Cw20HookMsg::Deposit {} => execute_deposit(deps, env, cw20_msg.sender, cw20_msg.amount),
    }
}

pub fn execute_deposit_native(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    assert_not_paused(&config)?;

    let native_lp_denom = match config.lp_token {
        AssetInfo::NativeToken { denom } => denom,
        AssetInfo::Token { .. } => {
            return Err(ContractError::Std(StdError::generic_err(
                "Native deposit called for a CW20 LP token vault",
            )));
        }
    };

    // M-1: require exactly one coin of the LP denom. The previous `find` + ignore-rest
    // pattern silently pocketed any extra coins the caller sent alongside.
    if info.funds.len() != 1 {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "expected exactly one coin ({}) for deposit, got {}",
            native_lp_denom,
            info.funds.len()
        ))));
    }
    let coin = &info.funds[0];
    if coin.denom != native_lp_denom {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "expected {} for deposit, got {}",
            native_lp_denom, coin.denom
        ))));
    }
    if coin.amount.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "No native LP tokens sent for deposit",
        )));
    }

    // Pass the sender's address (the actual depositor) and amount to the common deposit logic
    execute_deposit(deps, env, info.sender.to_string(), coin.amount)
}

pub fn execute_deposit(
    deps: DepsMut,
    _env: Env,
    sender: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let sender_addr = deps.api.addr_validate(&sender)?;

    // Update the user's pending deposit count.
    USERS.update(
        deps.storage,
        &sender_addr,
        |user_info| -> StdResult<UserInfo> {
            let mut info = user_info.unwrap_or_default();
            info.pending_deposit += amount; // Add to pending, not shares
            Ok(info)
        },
    )?;

    TOTAL_PENDING_DEPOSITS.update(deps.storage, |mut total| -> StdResult<_> {
        total += amount;
        Ok(total)
    })?;

    // The capital should be put to work immediately to generate rewards.
    let bond_msg = match config.lp_token {
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: config.farm_contract.to_string(),
                amount,
                msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.farm_contract.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Bond { amount })?,
            funds: vec![cosmwasm_std::coin(amount.u128(), denom)],
        }),
    };

    Ok(Response::new()
        .add_message(bond_msg)
        .add_attribute("action", "deposit_pending")
        .add_attribute("depositor", sender)
        .add_attribute("lp_amount_pending", amount.to_string()))
}

/// Withdraws a user's pending LP tokens that have not yet been converted to shares.
pub fn execute_withdraw_pending(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    amount: Option<Uint128>,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let sender_addr = info.sender;

    let mut user_info = USERS
        .load(deps.storage, &sender_addr)
        .map_err(|_| ContractError::InsufficientShares {})?;

    let amount_to_withdraw = amount.unwrap_or(user_info.pending_deposit);

    if amount_to_withdraw.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "Withdrawal amount must be greater than zero.",
        )));
    }

    if amount_to_withdraw > user_info.pending_deposit {
        return Err(ContractError::Std(StdError::generic_err(
            "Insufficient pending deposit.",
        )));
    }

    // Update user state
    user_info.pending_deposit -= amount_to_withdraw;

    // If user has no remaining assets, remove them from storage.
    if user_info.shares.is_zero() && user_info.pending_deposit.is_zero() {
        USERS.remove(deps.storage, &sender_addr);
    } else {
        USERS.save(deps.storage, &sender_addr, &user_info)?;
    }

    // Update total pending deposits
    TOTAL_PENDING_DEPOSITS.update(deps.storage, |total| -> StdResult<_> {
        total
            .checked_sub(amount_to_withdraw)
            .map_err(StdError::from)
    })?;

    send_withdrawal_messages(
        config,
        sender_addr,
        amount_to_withdraw,
        Uint128::zero(), // No shares are burnt
        amount_to_withdraw,
    )
}

/// Withdraws a user's funds by redeeming active, value-accruing shares.
///
/// H-1/H-5: before unbonding LP, trigger `farm.Withdraw` to harvest any pending reward tokens
/// into the vault, then in the reply send the exiter their proportional slice of both LP and
/// reward tokens. Without this, share price would use a denominator that excludes unharvested
/// rewards and the exiter would forfeit their claim to them (they'd be absorbed into the next
/// compound and distributed only to remaining shareholders).
pub fn execute_withdraw_shares(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    shares_to_burn: Uint128,
) -> Result<Response, ContractError> {
    if shares_to_burn.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "Must withdraw a non-zero amount of shares.",
        )));
    }

    let config = CONFIG.load(deps.storage)?;
    let sender_addr = info.sender;

    let mut user_info = USERS
        .load(deps.storage, &sender_addr)
        .map_err(|_| ContractError::InsufficientShares {})?;

    if user_info.shares < shares_to_burn {
        return Err(ContractError::InsufficientShares {});
    }

    let total_shares = TOTAL_SHARES.load(deps.storage)?;
    let total_pending_deposits = TOTAL_PENDING_DEPOSITS.load(deps.storage)?;

    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: Some(env.block.time.seconds()),
        },
    )?;
    let total_lp_staked = staker_info.bond_amount;

    let lp_value_of_all_shares = total_lp_staked
        .checked_sub(total_pending_deposits)
        .map_err(StdError::from)?;

    // M-8: `user_info.shares >= shares_to_burn >= 1` is asserted above, which implies
    // `total_shares >= 1`. The previous `if total_shares.is_zero()` guard silently
    // returned zero and let the downstream "too small" error surface — fail loudly on
    // the real invariant violation instead.
    if total_shares.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "invariant violation: user holds shares but total_shares == 0",
        )));
    }
    let lp_to_withdraw = shares_to_burn.multiply_ratio(lp_value_of_all_shares, total_shares);

    if lp_to_withdraw.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "Withdrawal request is too small, resulting in zero LP value.",
        )));
    }

    user_info.shares -= shares_to_burn;
    if user_info.shares.is_zero() && user_info.pending_deposit.is_zero() {
        USERS.remove(deps.storage, &sender_addr);
    } else {
        USERS.save(deps.storage, &sender_addr, &user_info)?;
    }
    TOTAL_SHARES.save(deps.storage, &(total_shares - shares_to_burn))?;

    // Fast path: farm has no pending rewards — old direct unbond + transfer is sufficient.
    if staker_info.pending_reward.is_zero() {
        return send_withdrawal_messages(
            config,
            sender_addr,
            lp_to_withdraw,
            shares_to_burn,
            Uint128::zero(),
        );
    }

    // Reward path: harvest first, then in the reply send LP + proportional reward tokens.
    let payload = WithdrawSharesReplyPayload {
        recipient: sender_addr.to_string(),
        shares_burnt: shares_to_burn,
        total_shares_pre_burn: total_shares,
        lp_to_withdraw,
    };

    let withdraw_rewards_msg = SubMsg {
        id: WITHDRAW_SHARES_REPLY_ID,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.farm_contract.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Withdraw {})?,
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload: to_json_binary(&payload)?,
    };

    Ok(Response::new()
        .add_submessage(withdraw_rewards_msg)
        .add_attribute("action", "withdraw_shares_harvest_started")
        .add_attribute("withdrawer", sender_addr.to_string())
        .add_attribute("shares_burnt", shares_to_burn.to_string())
        .add_attribute("lp_to_withdraw", lp_to_withdraw.to_string()))
}

/// Handles the reply from `farm.Withdraw` during a share exit. The reward_token balance the
/// vault now holds is the freshly harvested pending_reward plus any dust left over from
/// previous compound cycles. The exiter is entitled to their proportional slice of the total —
/// dust belonged to them as a shareholder too.
pub fn handle_withdraw_shares_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    let payload: WithdrawSharesReplyPayload = from_json(&msg.payload)?;
    let config = CONFIG.load(deps.storage)?;
    let recipient = deps.api.addr_validate(&payload.recipient)?;

    let reward_balance = match &config.reward_token {
        AssetInfo::Token { contract_addr } => query_token_balance(
            &deps.querier,
            deps.api.addr_validate(contract_addr)?,
            env.contract.address.clone(),
        )?,
        AssetInfo::NativeToken { denom } => {
            query_balance(&deps.querier, env.contract.address.clone(), denom.clone())?
        }
    };

    let mut messages: Vec<CosmosMsg> = Vec::new();

    // Unbond + transfer LP (same as the fast path).
    messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.farm_contract.to_string(),
        msg: to_json_binary(&FarmExecuteMsg::Unbond {
            amount: payload.lp_to_withdraw,
        })?,
        funds: vec![],
    }));

    messages.push(match &config.lp_token {
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: recipient.to_string(),
                amount: payload.lp_to_withdraw,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: recipient.to_string(),
            amount: vec![cosmwasm_std::Coin {
                denom: denom.clone(),
                amount: payload.lp_to_withdraw,
            }],
        }),
    });

    // Proportional reward share. `total_shares_pre_burn` is guaranteed non-zero here — the
    // caller held `shares_burnt` of it and we validated `shares_burnt > 0` at entry.
    let reward_share = if reward_balance.is_zero() {
        Uint128::zero()
    } else {
        reward_balance.multiply_ratio(payload.shares_burnt, payload.total_shares_pre_burn)
    };

    if !reward_share.is_zero() {
        messages.push(match &config.reward_token {
            AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: recipient.to_string(),
                    amount: reward_share,
                })?,
                funds: vec![],
            }),
            AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                to_address: recipient.to_string(),
                amount: vec![cosmwasm_std::Coin {
                    denom: denom.clone(),
                    amount: reward_share,
                }],
            }),
        });
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "withdraw_shares_completed")
        .add_attribute("withdrawer", recipient.to_string())
        .add_attribute("shares_burnt", payload.shares_burnt.to_string())
        .add_attribute("lp_withdrawn", payload.lp_to_withdraw.to_string())
        .add_attribute("reward_withdrawn", reward_share.to_string()))
}

/// Helper function to create and wrap withdrawal messages and attributes.
fn send_withdrawal_messages(
    config: Config,
    recipient: Addr,
    total_lp_to_withdraw: Uint128,
    shares_burnt: Uint128,
    pending_lp_withdrawn: Uint128,
) -> Result<Response, ContractError> {
    if total_lp_to_withdraw.is_zero() {
        return Ok(Response::new()
            .add_attribute("action", "withdraw")
            .add_attribute("status", "no_amount_to_withdraw"));
    }

    let unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.farm_contract.to_string(),
        msg: to_json_binary(&FarmExecuteMsg::Unbond {
            amount: total_lp_to_withdraw,
        })?,
        funds: vec![],
    });

    let transfer_lp_msg = match config.lp_token {
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: recipient.to_string(),
                amount: total_lp_to_withdraw,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: recipient.to_string(),
            amount: vec![cosmwasm_std::Coin {
                denom,
                amount: total_lp_to_withdraw,
            }],
        }),
    };

    Ok(Response::new()
        .add_messages(vec![unbond_msg, transfer_lp_msg])
        .add_attribute("action", "withdraw")
        .add_attribute("withdrawer", recipient.to_string())
        .add_attribute("shares_burnt", shares_burnt.to_string())
        .add_attribute("pending_lp_withdrawn", pending_lp_withdrawn.to_string())
        .add_attribute("total_lp_withdrawn", total_lp_to_withdraw.to_string()))
}

pub fn execute_compound(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    belief_prices: Vec<Decimal>,
    minimum_lp_to_receive: Uint128,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    assert_not_paused(&config)?;

    // C-3: compound is permissionless. The `config.compounder` field is now advisory
    // (used by off-chain keepers as the canonical caller, and retained for rotation
    // ergonomics) rather than a gatekeeper. Every caller MUST commit to an LP floor
    // so the vault's downside is bounded if their belief prices are too lenient or
    // they get sandwiched.
    if minimum_lp_to_receive.is_zero() {
        return Err(ContractError::MinimumLpToReceiveZero {});
    }

    let expected_swap_count = if config.reward_to_lp_token_route.is_empty() {
        1
    } else {
        config.reward_to_lp_token_route.len() + 1
    };
    if belief_prices.len() != expected_swap_count {
        return Err(ContractError::InvalidBeliefPrices {
            expected: expected_swap_count,
            got: belief_prices.len(),
        });
    }
    if belief_prices.iter().any(|p| p.is_zero()) {
        return Err(ContractError::ZeroBeliefPrice {});
    }

    let env_time = Some(env.block.time.seconds());

    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: env_time,
        },
    )?;

    if staker_info.pending_reward.is_zero() {
        return Ok(Response::new()
            .add_attribute("action", "compound")
            .add_attribute("status", "no_rewards"));
    }

    // Ensure rewards are above the minimum threshold.
    // This prevents unprofitable compounding.
    if staker_info.pending_reward < config.minimum_reward_to_compound {
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Pending rewards are below the minimum threshold to compound",
        )));
    }

    // Heuristic floor on `minimum_lp_to_receive`: the C-3 permissionless compound
    // requires non-zero min_lp, but non-zero alone admits `Uint128::new(1)` which
    // makes the safeguard vacuous. Here we simulate the compound at current pool
    // state and require the caller's floor clear `MIN_LP_HEURISTIC_K * expected`.
    // See the const's doc comment for the k derivation. When `expected` rounds to
    // zero (e.g. reward dust below `minimum_reward_to_compound`), the heuristic
    // naturally fails open — the non-zero check above still applies.
    let reward_after_fee = compute_reward_after_fee(
        staker_info.pending_reward,
        config.fee_recipient.as_ref(),
        config.fee_percentage,
    )?;
    let expected_lp = estimate_expected_lp(deps.as_ref(), &config, reward_after_fee)?;
    if !expected_lp.is_zero() {
        let floor = expected_lp.mul_floor(MIN_LP_HEURISTIC_K);
        if !floor.is_zero() && minimum_lp_to_receive < floor {
            return Err(ContractError::MinimumLpBelowHeuristic {
                minimum: minimum_lp_to_receive,
                floor,
            });
        }
    }

    let payload = HarvestReplyPayload {
        reward_amount_to_compound: staker_info.pending_reward,
        tvl_before_compound: staker_info.bond_amount,
        belief_prices,
        minimum_lp_to_receive,
    };

    // M-3: ReplyOn::Always so handle_harvest_reply gets a chance to surface a
    // structured error attribute before reverting. Behaviour on failure is still a
    // full-tx revert — partial compound state is unsound — but tx logs now
    // distinguish "harvest reverted" from later steps.
    let harvest_msg = SubMsg {
        id: HARVEST_REPLY_ID,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.farm_contract.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Withdraw {})?,
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Always,
        payload: to_json_binary(&payload)?,
    };

    Ok(Response::new()
        .add_submessage(harvest_msg)
        .add_attribute("action", "compound")
        .add_attribute("status", "step_1_harvest_initiated"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::TotalShares {} => to_json_binary(&query_total_shares(deps)?),
        QueryMsg::UserInfo { user } => to_json_binary(&query_user_info(deps, user)?),
        QueryMsg::CompoundingInfo {} => to_json_binary(&query_compounding_info(deps)?),
        QueryMsg::PendingDeposits { start_after, limit } => {
            to_json_binary(&query_pending_deposits(deps, start_after, limit)?)
        }
        QueryMsg::TotalPendingDeposits {} => to_json_binary(&query_total_pending_deposits(deps)?),
        QueryMsg::PendingCompounderRotation {} => {
            to_json_binary(&query_pending_compounder_rotation(deps)?)
        }
        QueryMsg::PendingMaxSlippageRaise {} => {
            to_json_binary(&query_pending_max_slippage_raise(deps)?)
        }
    }
}

fn query_compounding_info(deps: Deps) -> StdResult<CompoundingInfo> {
    COMPOUNDING_INFO.load(deps.storage)
}

fn query_config(deps: Deps) -> StdResult<Config> {
    CONFIG.load(deps.storage)
}

fn query_total_shares(deps: Deps) -> StdResult<Uint128> {
    TOTAL_SHARES.load(deps.storage)
}

fn query_total_pending_deposits(deps: Deps) -> StdResult<Uint128> {
    TOTAL_PENDING_DEPOSITS.load(deps.storage)
}

fn query_pending_compounder_rotation(
    deps: Deps,
) -> StdResult<crate::msg::PendingCompounderRotationResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(crate::msg::PendingCompounderRotationResponse {
        pending_compounder: config.pending_compounder.map(|a| a.to_string()),
        effective_at: config.pending_compounder_effective_at,
    })
}

fn query_pending_max_slippage_raise(
    deps: Deps,
) -> StdResult<crate::msg::PendingMaxSlippageRaiseResponse> {
    let config = CONFIG.load(deps.storage)?;
    Ok(crate::msg::PendingMaxSlippageRaiseResponse {
        pending_max_slippage: config.pending_max_slippage,
        effective_at: config.pending_max_slippage_effective_at,
    })
}

fn query_user_info(deps: Deps, user: String) -> StdResult<UserInfoResponse> {
    let user_addr = deps.api.addr_validate(&user)?;
    let user_info = USERS
        .may_load(deps.storage, &user_addr)?
        .unwrap_or_default();
    Ok(UserInfoResponse {
        shares: user_info.shares,
        pending_deposit: user_info.pending_deposit,
    })
}

const DEFAULT_PAGINATION_LIMIT: u32 = 10;
const MAX_PAGINATION_LIMIT: u32 = 30;

pub fn query_pending_deposits(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<PendingDepositsResponse> {
    let limit = limit
        .unwrap_or(DEFAULT_PAGINATION_LIMIT)
        .min(MAX_PAGINATION_LIMIT) as usize;

    // Validate the start_after address if provided
    let start = start_after
        .map(|addr| deps.api.addr_validate(&addr))
        .transpose()?;

    // Create the lower bound for the range query
    let start_bound = start.as_ref().map(Bound::exclusive);

    let mut users_with_pending = vec![];
    let mut last_user: Option<String> = None;

    // Iterate over the USERS map within the specified range
    for result in USERS
        .range(deps.storage, start_bound, None, Order::Ascending)
        .take(limit)
    {
        let (addr, user_info) = result?;
        let user_addr_str = addr.to_string();

        // Check if the user has a pending deposit
        if !user_info.pending_deposit.is_zero() {
            users_with_pending.push(user_addr_str.clone());
        }

        // Advance the cursor on every iteration, not just on matches. Otherwise
        // a full page of non-pending users would leave last_user = None and the
        // caller would stop paginating, silently missing pending deposits that
        // sort after the page.
        last_user = Some(user_addr_str);
    }

    Ok(PendingDepositsResponse {
        users: users_with_pending,
        last_user,
    })
}

// This function is called after the HARVEST submessage returns. With M-3 we use
// ReplyOn::Always, so a failed harvest still invokes this handler — it surfaces a
// structured attribute and then propagates the error to revert the tx.
pub fn handle_harvest_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    if let cosmwasm_std::SubMsgResult::Err(err) = &msg.result {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "compound step 1 (farm.Withdraw) failed: {}",
            err
        ))));
    }

    let payload: HarvestReplyPayload = from_json(&msg.payload)?;
    let config = CONFIG.load(deps.storage)?;
    let slippage_tolerance = config.slippage_tolerance;

    // Query the balance of the reward token we just harvested
    let reward_asset_info = config.reward_token.clone();
    let mut reward_balance = match reward_asset_info.clone() {
        AssetInfo::Token { contract_addr } => query_token_balance(
            &deps.querier,
            deps.api.addr_validate(&contract_addr)?,
            env.contract.address.clone(),
        )?,
        AssetInfo::NativeToken { denom } => {
            query_balance(&deps.querier, env.contract.address.clone(), denom)?
        }
    };

    if reward_balance.is_zero() {
        return Ok(Response::new().add_attribute("status", "no_rewards_after_harvest"));
    }

    // Handle fees
    let mut messages: Vec<CosmosMsg> = vec![];
    if let (Some(recipient), Some(percentage)) = (config.fee_recipient, config.fee_percentage) {
        let fee_amount = reward_balance.mul_floor(percentage);

        if !fee_amount.is_zero() {
            let fee_msg = match reward_asset_info.clone() {
                AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr,
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: recipient.to_string(),
                        amount: fee_amount,
                    })?,
                    funds: vec![],
                }),
                AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                    to_address: recipient.to_string(),
                    amount: vec![cosmwasm_std::Coin {
                        denom,
                        amount: fee_amount,
                    }],
                }),
            };
            messages.push(fee_msg);
            reward_balance = reward_balance
                .checked_sub(fee_amount)
                .map_err(StdError::from)?;
        }
    }

    let first_belief_price = payload.belief_prices[0];

    // M-2: carry the actual post-harvest, post-fee balance through the reply chain so
    // `last_reward_amount_compounded` reflects what was really compounded rather than
    // the pre-harvest `staker_info.pending_reward` prediction.
    let actual_reward_amount = reward_balance;

    if !config.reward_to_lp_token_route.is_empty() {
        // A. Multi-hop route is defined
        let first_hop = &config.reward_to_lp_token_route[0];

        let offer_asset = Asset {
            info: reward_asset_info,
            amount: reward_balance,
        };

        let route_payload = CompoundRoutePayload {
            hop_index: 1,
            reward_amount_to_compound: actual_reward_amount,
            tvl_before_compound: payload.tvl_before_compound,
            belief_prices: payload.belief_prices,
            minimum_lp_to_receive: payload.minimum_lp_to_receive,
        };

        let swap_msg = create_swap_submsg(
            first_hop.pair_contract.clone(),
            offer_asset,
            first_belief_price,
            slippage_tolerance,
            ROUTE_SWAP_REPLY_ID,
            to_json_binary(&route_payload)?,
        )?;

        Ok(Response::new()
            .add_messages(messages)
            .add_submessage(swap_msg)
            .add_attribute("action", "compound_route_started"))
    } else {
        // B. No route defined — reward_token is already a pair asset. Use the optimal
        // zap-in formula (H-3) instead of a naive 50/50 split, which over-supplies the
        // offer side by the swap fee.
        let offer_reserve =
            query_pair_offer_reserve(deps.as_ref(), &config.pair_contract, &reward_asset_info)?;
        let amount_to_swap = optimal_zap_amount_xyk(reward_balance, offer_reserve)?;
        let offer_asset = Asset {
            info: reward_asset_info,
            amount: amount_to_swap,
        };

        let final_payload = HarvestReplyPayload {
            reward_amount_to_compound: actual_reward_amount,
            tvl_before_compound: payload.tvl_before_compound,
            belief_prices: payload.belief_prices,
            minimum_lp_to_receive: payload.minimum_lp_to_receive,
        };

        let swap_msg = create_swap_submsg(
            config.pair_contract,
            offer_asset,
            first_belief_price,
            slippage_tolerance,
            FINAL_SWAP_REPLY_ID,
            to_json_binary(&final_payload)?,
        )?;

        Ok(Response::new()
            .add_messages(messages)
            .add_submessage(swap_msg)
            .add_attribute("status", "step_2_swap_initiated")
            .add_attribute("amount_to_swap", amount_to_swap))
    }
}

pub fn handle_route_swap_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    let payload: CompoundRoutePayload = from_json(&msg.payload)?;
    let config = CONFIG.load(deps.storage)?;
    let slippage_tolerance = config.slippage_tolerance;
    let current_hop_index = (payload.hop_index - 1) as usize;
    let next_hop_index = payload.hop_index as usize;

    if next_hop_index < config.reward_to_lp_token_route.len() {
        // --- THERE ARE MORE HOPS ---
        let previous_hop = &config.reward_to_lp_token_route[current_hop_index];
        let next_hop = &config.reward_to_lp_token_route[next_hop_index];

        let amount_to_swap = match previous_hop.to_asset_info.clone() {
            AssetInfo::Token { contract_addr } => query_token_balance(
                &deps.querier,
                deps.api.addr_validate(&contract_addr)?,
                env.contract.address,
            )?,
            AssetInfo::NativeToken { denom } => {
                query_balance(&deps.querier, env.contract.address, denom)?
            }
        };

        let offer_asset = Asset {
            info: previous_hop.to_asset_info.clone(),
            amount: amount_to_swap,
        };

        let belief_price = payload.belief_prices[next_hop_index];
        let next_payload = CompoundRoutePayload {
            hop_index: payload.hop_index + 1,
            reward_amount_to_compound: payload.reward_amount_to_compound,
            tvl_before_compound: payload.tvl_before_compound,
            belief_prices: payload.belief_prices,
            minimum_lp_to_receive: payload.minimum_lp_to_receive,
        };

        let swap_msg = create_swap_submsg(
            next_hop.pair_contract.clone(),
            offer_asset,
            belief_price,
            slippage_tolerance,
            ROUTE_SWAP_REPLY_ID,
            to_json_binary(&next_payload)?,
        )?;

        Ok(Response::new()
            .add_submessage(swap_msg)
            .add_attribute("action", "route_swap")
            .add_attribute("hop_executed", current_hop_index.to_string()))
    } else {
        // --- THE ROUTE IS COMPLETE ---
        let last_hop = config.reward_to_lp_token_route.last().unwrap();

        let final_asset_balance = match last_hop.to_asset_info.clone() {
            AssetInfo::Token { contract_addr } => query_token_balance(
                &deps.querier,
                deps.api.addr_validate(&contract_addr)?,
                env.contract.address,
            )?,
            AssetInfo::NativeToken { denom } => {
                query_balance(&deps.querier, env.contract.address, denom)?
            }
        };

        // H-3: optimal zap-in instead of naive 50/50 split.
        let offer_reserve = query_pair_offer_reserve(
            deps.as_ref(),
            &config.pair_contract,
            &last_hop.to_asset_info,
        )?;
        let amount_to_swap = optimal_zap_amount_xyk(final_asset_balance, offer_reserve)?;
        let offer_asset = Asset {
            info: last_hop.to_asset_info.clone(),
            amount: amount_to_swap,
        };

        let final_belief_price = *payload.belief_prices.last().unwrap();
        let final_payload = HarvestReplyPayload {
            reward_amount_to_compound: payload.reward_amount_to_compound,
            tvl_before_compound: payload.tvl_before_compound,
            belief_prices: payload.belief_prices,
            minimum_lp_to_receive: payload.minimum_lp_to_receive,
        };

        let swap_msg = create_swap_submsg(
            config.pair_contract,
            offer_asset,
            final_belief_price,
            slippage_tolerance,
            FINAL_SWAP_REPLY_ID,
            to_json_binary(&final_payload)?,
        )?;

        Ok(Response::new()
            .add_submessage(swap_msg)
            .add_attribute("action", "route_swap_complete"))
    }
}

pub fn handle_final_swap_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let payload: HarvestReplyPayload = from_json(&msg.payload)?;

    // Query balances for both assets in the pair using the provided library functions
    let mut assets_to_provide: [Asset; 2] = [
        Asset {
            info: config.asset_infos[0].clone(),
            amount: Uint128::zero(),
        },
        Asset {
            info: config.asset_infos[1].clone(),
            amount: Uint128::zero(),
        },
    ];

    for asset in &mut assets_to_provide {
        asset.amount = match &asset.info {
            AssetInfo::Token { contract_addr } => query_token_balance(
                &deps.querier,
                deps.api.addr_validate(contract_addr)?,
                env.contract.address.clone(),
            )?,
            AssetInfo::NativeToken { denom } => {
                query_balance(&deps.querier, env.contract.address.clone(), denom.clone())?
            }
        };
    }

    let mut messages: Vec<CosmosMsg> = vec![];

    for asset in &assets_to_provide {
        // If the asset is a CW20 token and we have a balance, grant an allowance to the pair contract
        if let AssetInfo::Token { contract_addr } = &asset.info {
            if !asset.amount.is_zero() {
                let allowance_msg = CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                        spender: config.pair_contract.to_string(),
                        amount: asset.amount,
                        expires: None,
                    })?,
                    funds: vec![],
                });
                messages.push(allowance_msg);
            }
        }
    }

    // Collect native token funds to send with the ProvideLiquidity message
    let funds: Vec<_> = assets_to_provide
        .iter()
        .filter_map(|a| {
            if let AssetInfo::NativeToken { denom } = &a.info {
                if !a.amount.is_zero() {
                    return Some(cosmwasm_std::Coin {
                        denom: denom.clone(),
                        amount: a.amount,
                    });
                }
            }
            None
        })
        .collect();

    let provide_liquidity_msg = SubMsg {
        id: PROVIDE_LIQUIDITY_REPLY_ID,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.pair_contract.to_string(),
            msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
                assets: assets_to_provide,
                receiver: None,
                deadline: None,
                slippage_tolerance: Some(config.slippage_tolerance),
            })?,
            funds,
        }),
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload: to_json_binary(&payload)?,
    };

    Ok(Response::new()
        .add_messages(messages) // Add the allowance messages before the submessage
        .add_submessage(provide_liquidity_msg)
        .add_attribute("status", "step_3_provide_liquidity_initiated"))
}

pub fn handle_provide_liquidity_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let payload: HarvestReplyPayload = from_json(&msg.payload)?;

    let new_compounding_info = CompoundingInfo {
        last_compound_time: env.block.time.seconds(),
        last_reward_amount_compounded: payload.reward_amount_to_compound,
        total_lp_staked_at_last_compound: payload.tvl_before_compound,
    };
    COMPOUNDING_INFO.save(deps.storage, &new_compounding_info)?;

    // Query the balance of the new LP tokens and create the correct bond message
    let (new_lp_balance, bond_msg) = match config.lp_token {
        // Case 1: The LP token is a CW20 token
        AssetInfo::Token { contract_addr } => {
            // We must validate the string address from the config into an Addr type before querying.
            let lp_token_addr = deps.api.addr_validate(&contract_addr)?;

            let balance = query_token_balance(&deps.querier, lp_token_addr, env.contract.address)?;

            // For CW20, we use the `Send` hook, which calls the farm's `Receive` entry point
            let msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::Send {
                    contract: config.farm_contract.to_string(),
                    amount: balance,
                    msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
                })?,
                funds: vec![],
            });
            (balance, msg)
        }
        // Case 2: The LP token is a native token
        AssetInfo::NativeToken { denom } => {
            let balance = query_balance(&deps.querier, env.contract.address, denom.clone())?;

            // For native tokens, we call the farm's `Bond` execute message directly,
            // passing the amount in the message body and the actual coins in the `funds` array.
            let msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: config.farm_contract.to_string(),
                msg: to_json_binary(&FarmExecuteMsg::Bond { amount: balance })?,
                funds: vec![cosmwasm_std::Coin {
                    denom,
                    amount: balance,
                }],
            });
            (balance, msg)
        }
    };

    if new_lp_balance.is_zero() {
        return Ok(Response::new().add_attribute("status", "no_lp_tokens_received"));
    }

    if new_lp_balance < payload.minimum_lp_to_receive {
        return Err(ContractError::InsufficientLpReceived {
            minimum: payload.minimum_lp_to_receive,
            got: new_lp_balance,
        });
    }

    Ok(Response::new()
        .add_message(bond_msg)
        .add_attribute("action", "compound")
        .add_attribute("status", "step_4_complete")
        .add_attribute("lp_tokens_staked", new_lp_balance))
}

/// Computes the zap-in swap amount for a legacy XYK pair: given `amount_to_zap` of
/// asset X and a pool with reserve `offer_reserve` of X, returns the portion of X to
/// swap for Y so the remaining X matches the post-swap pool ratio. Naive 50/50
/// splits over-supply X by the swap fee, leaving dust stranded after
/// `ProvideLiquidity`'s min-ratio rule (H-3).
///
/// Derivation for fee f, γ = 1-f:
///   After swapping s of X: y = γ·s·Ry / (Rx + γ·s)
///   Matched-provide constraint: (A-s)/y = (Rx+s)/(Ry-y)
///   Reduces to: γ·s² + s·Rx·(1+γ) − Rx·A = 0
///   Positive root: s = (√(Rx·(Rx·(1+γ)² + 4·γ·A)) − Rx·(1+γ)) / (2γ)
///
/// Constants below hardcode the legacy pair's `COMMISSION_RATE = 3 permille` (0.3%):
///   (1+γ) = 1.997 → scaled ×10³ = 1997 (C3)
///   (1+γ)² = 1997² = 3_988_009 (C1)
///   4γ × 10³ × 10³ = 4 · 997 · 1000 = 3_988_000 (C2)
///   2γ × 10³ = 1994 (C4)
///
/// *Not CLMM-compatible.* When the vault starts backing CLMM pools (deferred), this
/// helper needs a separate branch — CLMM reserves are tick-local and the closed form
/// no longer applies.
pub(crate) fn optimal_zap_amount_xyk(
    amount_to_zap: Uint128,
    offer_reserve: Uint128,
) -> StdResult<Uint128> {
    if offer_reserve.is_zero() {
        return Err(StdError::generic_err(
            "optimal_zap: pool has zero offer-side reserve",
        ));
    }
    if amount_to_zap.is_zero() {
        return Ok(Uint128::zero());
    }

    const C1: u128 = 3_988_009;
    const C2: u128 = 3_988_000;
    const C3: u128 = 1_997;
    const C4: u128 = 1_994;

    let rx: Uint256 = offer_reserve.into();
    let a: Uint256 = amount_to_zap.into();

    // K = Rx² · C1 + Rx · A · C2  (Uint256 comfortably absorbs the widened range)
    let rx_sq = rx.checked_mul(rx).map_err(StdError::from)?;
    let term1 = rx_sq
        .checked_mul(Uint256::from(C1))
        .map_err(StdError::from)?;
    let term2 = rx
        .checked_mul(a)
        .map_err(StdError::from)?
        .checked_mul(Uint256::from(C2))
        .map_err(StdError::from)?;
    let k = term1.checked_add(term2).map_err(StdError::from)?;

    let sqrt_k = k.isqrt();
    let offset = rx.checked_mul(Uint256::from(C3)).map_err(StdError::from)?;
    // sqrt_k >= offset whenever A > 0 (the formula's discriminant includes +4γ·Rx·A);
    // the checked_sub guards against precision pathologies in the isqrt floor.
    let numerator = sqrt_k.checked_sub(offset).map_err(StdError::from)?;
    let s = numerator
        .checked_div(Uint256::from(C4))
        .map_err(StdError::from)?;

    Uint128::try_from(s).map_err(|_| StdError::generic_err("optimal_zap: overflow"))
}

/// Queries the pair's pool reserves and picks out the reserve matching `offer_info`.
fn query_pair_offer_reserve(
    deps: Deps,
    pair_contract: &cosmwasm_std::Addr,
    offer_info: &AssetInfo,
) -> StdResult<Uint128> {
    let pool: PoolResponse = deps
        .querier
        .query_wasm_smart(pair_contract.to_string(), &PairQueryMsg::Pool {})?;
    pool.assets
        .iter()
        .find(|a| a.info == *offer_info)
        .map(|a| a.amount)
        .ok_or_else(|| StdError::generic_err("optimal_zap: offer asset not found in pair reserves"))
}

/// Applies the configured compound fee to the pending-reward amount and returns the
/// net portion that will actually be zapped into LP. Mirrors the fee deduction in
/// `handle_harvest_reply` — kept in sync so the B-6 heuristic sees the same amount
/// the real compound flow will.
fn compute_reward_after_fee(
    pending_reward: Uint128,
    fee_recipient: Option<&cosmwasm_std::Addr>,
    fee_percentage: Option<Decimal>,
) -> StdResult<Uint128> {
    if let (Some(_), Some(pct)) = (fee_recipient, fee_percentage) {
        if !pct.is_zero() {
            let fee = pending_reward.mul_floor(pct);
            return pending_reward.checked_sub(fee).map_err(StdError::from);
        }
    }
    Ok(pending_reward)
}

/// Estimates the LP that the compound path would mint if executed at the *current*
/// pool state (no-slippage, no-sandwich assumption). Used in `execute_compound`
/// to floor `minimum_lp_to_receive` — see `MIN_LP_HEURISTIC_K`.
///
/// Walks the reward-to-lp route via pair `Simulation` queries (decimals-aware),
/// then projects the final zap using the closed-form XYK math. Returns `0` if
/// the amount rounds out of existence at any step, or if the pair is empty —
/// in both cases the heuristic fails open and only the non-zero check on
/// `minimum_lp_to_receive` applies. XYK-only; see the CLMM deferral in
/// `optimal_zap_amount_xyk`.
fn estimate_expected_lp(
    deps: Deps,
    config: &Config,
    reward_after_fee: Uint128,
) -> StdResult<Uint128> {
    if reward_after_fee.is_zero() {
        return Ok(Uint128::zero());
    }

    // Walk the route, simulating each hop on its pair contract.
    let mut current_amount = reward_after_fee;
    let mut current_asset = config.reward_token.clone();
    for hop in &config.reward_to_lp_token_route {
        if current_amount.is_zero() {
            return Ok(Uint128::zero());
        }
        let sim: SimulationResponse = deps.querier.query_wasm_smart(
            hop.pair_contract.to_string(),
            &PairQueryMsg::Simulation {
                offer_asset: Asset {
                    info: current_asset.clone(),
                    amount: current_amount,
                },
            },
        )?;
        current_amount = sim.return_amount;
        current_asset = hop.to_asset_info.clone();
    }

    // `current_amount` is the terminal asset on `pair_contract`.
    if current_amount.is_zero() {
        return Ok(Uint128::zero());
    }

    let pool: PoolResponse = deps
        .querier
        .query_wasm_smart(config.pair_contract.to_string(), &PairQueryMsg::Pool {})?;
    if pool.total_share.is_zero() {
        // Empty pair would mint LP as sqrt(x*y) rather than the proportional formula.
        // Vaults operate on established pairs, so this is a pathological path — fail open.
        return Ok(Uint128::zero());
    }

    let offer_reserve = pool
        .assets
        .iter()
        .find(|a| a.info == current_asset)
        .map(|a| a.amount)
        .ok_or_else(|| {
            StdError::generic_err("estimate_expected_lp: terminal asset not in pair reserves")
        })?;
    if offer_reserve.is_zero() {
        return Ok(Uint128::zero());
    }

    let s = optimal_zap_amount_xyk(current_amount, offer_reserve)?;
    if s.is_zero() || s >= current_amount {
        return Ok(Uint128::zero());
    }

    // After the optimal zap: provide `(current_amount - s)` of the offer side.
    // XYK LP mint (existing pool): min(provided_0 * total / reserve_0, provided_1 * total / reserve_1).
    // Since the zap is optimal, both sides yield the same LP — we compute the offer-side ratio only.
    let provide_offer = current_amount.checked_sub(s).map_err(StdError::from)?;
    let new_offer_reserve = offer_reserve.checked_add(s).map_err(StdError::from)?;

    let expected_lp = Uint256::from(pool.total_share)
        .checked_mul(Uint256::from(provide_offer))
        .map_err(StdError::from)?
        .checked_div(Uint256::from(new_offer_reserve))
        .map_err(StdError::from)?;

    Uint128::try_from(expected_lp)
        .map_err(|_| StdError::generic_err("estimate_expected_lp: overflow"))
}

/// Helper function to create a swap submessage.
fn create_swap_submsg(
    pair_contract: cosmwasm_std::Addr,
    offer_asset: Asset,
    belief_price: Decimal,
    slippage_tolerance: Decimal,
    reply_id: u64,
    payload: Binary,
) -> StdResult<SubMsg> {
    let swap_cosmos_msg = match &offer_asset.info {
        AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: pair_contract.to_string(),
            msg: to_json_binary(&PairExecuteMsg::Swap {
                offer_asset: offer_asset.clone(),
                belief_price: Some(belief_price),
                max_spread: Some(slippage_tolerance),
                to: None,
                deadline: None,
            })?,
            funds: vec![cosmwasm_std::Coin {
                denom: denom.clone(),
                amount: offer_asset.amount,
            }],
        }),
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: pair_contract.to_string(),
                amount: offer_asset.amount,
                msg: to_json_binary(&PairCw20HookMsg::Swap {
                    belief_price: Some(belief_price),
                    max_spread: Some(slippage_tolerance),
                    to: None,
                    deadline: None,
                })?,
            })?,
            funds: vec![],
        }),
    };

    Ok(SubMsg {
        id: reply_id,
        msg: swap_cosmos_msg,
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload,
    })
}

/// Creates a proposal to transfer ownership of the contract.
/// Only the current owner can call this.
pub fn execute_propose_new_owner(
    deps: DepsMut,
    info: MessageInfo,
    new_owner: String,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if the sender is the current owner
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    // Validate and store the proposed new owner address
    let new_owner_addr = deps.api.addr_validate(&new_owner)?;
    config.proposed_owner = Some(new_owner_addr);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_new_owner")
        .add_attribute("proposed_owner", new_owner))
}

/// Accepts an ownership transfer proposal.
/// Only the proposed new owner can call this.
pub fn execute_accept_ownership(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if there is a proposal and if the sender is the proposed owner
    match config.proposed_owner {
        Some(proposed) if proposed == info.sender => {
            config.owner = proposed; // The proposed owner is now the new owner
            config.proposed_owner = None; // Clear the proposal
            CONFIG.save(deps.storage, &config)?;

            Ok(Response::new()
                .add_attribute("action", "accept_ownership")
                .add_attribute("new_owner", info.sender.to_string()))
        }
        _ => Err(ContractError::Std(StdError::generic_err(
            "No ownership proposal for this address to accept",
        ))),
    }
}

/// Cancels an ownership transfer proposal.
/// Only the current owner can call this.
pub fn execute_cancel_ownership_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if the sender is the current owner
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    // Clear the proposed owner
    config.proposed_owner = None;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "cancel_ownership_proposal")
        .add_attribute("owner", info.sender.to_string()))
}

pub fn execute_activate_pending_deposits(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    users: Vec<String>,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    assert_not_paused(&config)?;

    // PERMISSION CHECK: Only the compounder can run this.
    if info.sender != config.compounder {
        return Err(ContractError::Unauthorized {});
    }

    // BATCH SIZE GUARD: Protect against gas limit attacks.
    const MAX_BATCH_SIZE: usize = 30;
    if users.len() > MAX_BATCH_SIZE {
        return Err(ContractError::BatchTooLarge {});
    }

    let ActivationContext {
        mut total_shares,
        total_lp_staked,
        mut total_pending_deposits,
        lp_value_of_all_shares,
    } = load_activation_context(deps.as_ref(), &env, &config)?;

    let mut activated_count = 0u32;
    let mut skipped_dust_count = 0u32;

    for user_addr_str in users {
        let user_addr = deps.api.addr_validate(&user_addr_str)?;
        let mut user_info = match USERS.may_load(deps.storage, &user_addr)? {
            Some(info) => info,
            None => continue, // Skip if user not found
        };

        if user_info.pending_deposit.is_zero() {
            continue; // Skip if user has no pending deposits
        }

        let amount_to_activate = user_info.pending_deposit;

        let shares_to_mint = compute_shares_to_mint(
            amount_to_activate,
            total_shares,
            total_lp_staked,
            lp_value_of_all_shares,
        );

        if shares_to_mint.is_zero() {
            // L-15: skip in the batch path rather than auto-refund — each refund is an
            // unbond+transfer pair, and a malicious actor spraying dust could blow up
            // the batch's gas envelope. Affected users can self-rescue by calling
            // `ActivateMyDeposit` (auto-refunds) or `WithdrawPending`.
            skipped_dust_count += 1;
            continue;
        }

        user_info.shares += shares_to_mint;
        user_info.pending_deposit = Uint128::zero();
        USERS.save(deps.storage, &user_addr, &user_info)?;

        total_pending_deposits = total_pending_deposits
            .checked_sub(amount_to_activate)
            .map_err(StdError::from)?;

        total_shares += shares_to_mint;
        activated_count += 1;
    }

    // Save the final total_shares value to storage ONCE after the loop.
    TOTAL_SHARES.save(deps.storage, &total_shares)?;
    TOTAL_PENDING_DEPOSITS.save(deps.storage, &total_pending_deposits)?;

    Ok(Response::new()
        .add_attribute("action", "batch_activate_deposits")
        .add_attribute("caller", info.sender)
        .add_attribute("activated_user_count", activated_count.to_string())
        .add_attribute("skipped_dust_count", skipped_dust_count.to_string()))
}

/// Lets a user activate their own pending deposit without the keeper.
/// Still subject to the C-2 dilution guard: pending farm rewards must be below the compound
/// threshold so new shares are minted against a fair denominator. Paired with the keeper path:
/// the keeper batches normal operations, individual users can self-rescue if the keeper is
/// selective or down.
pub fn execute_activate_my_deposit(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    assert_not_paused(&config)?;

    let mut user_info = USERS
        .may_load(deps.storage, &info.sender)?
        .ok_or(ContractError::NoPendingDeposit {})?;

    if user_info.pending_deposit.is_zero() {
        return Err(ContractError::NoPendingDeposit {});
    }

    let ActivationContext {
        total_shares,
        total_lp_staked,
        total_pending_deposits,
        lp_value_of_all_shares,
    } = load_activation_context(deps.as_ref(), &env, &config)?;

    let amount_to_activate = user_info.pending_deposit;
    let shares_to_mint = compute_shares_to_mint(
        amount_to_activate,
        total_shares,
        total_lp_staked,
        lp_value_of_all_shares,
    );

    if shares_to_mint.is_zero() {
        // L-15: dust deposit — share price has moved past this amount. Auto-refund
        // the pending LP rather than leaving the user to discover WithdrawPending.
        user_info.pending_deposit = Uint128::zero();
        if user_info.shares.is_zero() {
            USERS.remove(deps.storage, &info.sender);
        } else {
            USERS.save(deps.storage, &info.sender, &user_info)?;
        }
        let new_total_pending = total_pending_deposits
            .checked_sub(amount_to_activate)
            .map_err(StdError::from)?;
        TOTAL_PENDING_DEPOSITS.save(deps.storage, &new_total_pending)?;

        return send_withdrawal_messages(
            config,
            info.sender,
            amount_to_activate,
            Uint128::zero(),
            amount_to_activate,
        );
    }

    user_info.shares += shares_to_mint;
    user_info.pending_deposit = Uint128::zero();
    USERS.save(deps.storage, &info.sender, &user_info)?;

    let new_total_shares = total_shares + shares_to_mint;
    let new_total_pending = total_pending_deposits
        .checked_sub(amount_to_activate)
        .map_err(StdError::from)?;

    TOTAL_SHARES.save(deps.storage, &new_total_shares)?;
    TOTAL_PENDING_DEPOSITS.save(deps.storage, &new_total_pending)?;

    Ok(Response::new()
        .add_attribute("action", "activate_my_deposit")
        .add_attribute("user", info.sender)
        .add_attribute("amount_activated", amount_to_activate.to_string())
        .add_attribute("shares_minted", shares_to_mint.to_string()))
}

struct ActivationContext {
    total_shares: Uint128,
    total_lp_staked: Uint128,
    total_pending_deposits: Uint128,
    lp_value_of_all_shares: Uint128,
}

/// Loads the totals and farm snapshot needed to price a pending-deposit activation and
/// enforces the dilution guard before returning. Any caller that mints shares against a
/// pending deposit must go through this.
fn load_activation_context(
    deps: Deps,
    env: &Env,
    config: &Config,
) -> Result<ActivationContext, ContractError> {
    let total_shares = TOTAL_SHARES.load(deps.storage)?;
    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: Some(env.block.time.seconds()),
        },
    )?;

    // See C-2 rationale: unharvested rewards are excluded from the share price denominator,
    // so activating before compound() dilutes existing holders. Clamped to >= 1 so a
    // zero-configured minimum still blocks any nonzero pending reward.
    let dilution_threshold = config.minimum_reward_to_compound.max(Uint128::new(1));
    if staker_info.pending_reward >= dilution_threshold {
        return Err(ContractError::PendingRewardsMustBeCompounded {
            pending: staker_info.pending_reward,
        });
    }

    let total_lp_staked = staker_info.bond_amount;
    let total_pending_deposits = TOTAL_PENDING_DEPOSITS.load(deps.storage)?;
    let lp_value_of_all_shares = total_lp_staked
        .checked_sub(total_pending_deposits)
        .map_err(StdError::from)?;

    Ok(ActivationContext {
        total_shares,
        total_lp_staked,
        total_pending_deposits,
        lp_value_of_all_shares,
    })
}

fn compute_shares_to_mint(
    amount_to_activate: Uint128,
    total_shares: Uint128,
    total_lp_staked: Uint128,
    lp_value_of_all_shares: Uint128,
) -> Uint128 {
    if total_shares.is_zero() || total_lp_staked.is_zero() {
        amount_to_activate
    } else {
        amount_to_activate.multiply_ratio(total_shares, lp_value_of_all_shares)
    }
}

pub fn execute_propose_compounder(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_compounder: String,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    let new_compounder_addr = deps.api.addr_validate(&new_compounder)?;
    let effective_at = env.block.time.seconds() + COMPOUNDER_ROTATION_DELAY_SECONDS;
    config.pending_compounder = Some(new_compounder_addr);
    config.pending_compounder_effective_at = Some(effective_at);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_compounder")
        .add_attribute("proposed_compounder", new_compounder)
        .add_attribute("effective_at", effective_at.to_string()))
}

pub fn execute_apply_compounder_rotation(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    let pending = config
        .pending_compounder
        .take()
        .ok_or(ContractError::NoPendingCompounderRotation {})?;
    let effective_at = config
        .pending_compounder_effective_at
        .take()
        .ok_or(ContractError::NoPendingCompounderRotation {})?;

    if env.block.time.seconds() < effective_at {
        return Err(ContractError::CompounderRotationNotReady {});
    }

    let new_compounder_str = pending.to_string();
    config.compounder = pending;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "apply_compounder_rotation")
        .add_attribute("new_compounder", new_compounder_str))
}

pub fn execute_cancel_compounder_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if config.pending_compounder.is_none() {
        return Err(ContractError::NoPendingCompounderRotation {});
    }

    config.pending_compounder = None;
    config.pending_compounder_effective_at = None;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "cancel_compounder_proposal"))
}

pub fn execute_tighten_max_slippage(
    deps: DepsMut,
    info: MessageInfo,
    new_max: Decimal,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if new_max > config.max_slippage_tolerance {
        return Err(ContractError::MaxSlippageMustNotRaise {
            proposed: new_max,
            current: config.max_slippage_tolerance,
        });
    }

    let previous_max = config.max_slippage_tolerance;
    config.max_slippage_tolerance = new_max;
    // Clamp operational `slippage_tolerance` if it would otherwise exceed the new cap.
    // The invariant `slippage_tolerance <= max_slippage_tolerance` must hold at all times.
    if config.slippage_tolerance > new_max {
        config.slippage_tolerance = new_max;
    }
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "tighten_max_slippage")
        .add_attribute("previous_max", previous_max.to_string())
        .add_attribute("new_max", new_max.to_string())
        .add_attribute("slippage_tolerance", config.slippage_tolerance.to_string()))
}

pub fn execute_propose_max_slippage_raise(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    new_max: Decimal,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if config.pending_max_slippage.is_some() {
        return Err(ContractError::MaxSlippageRaiseAlreadyPending {});
    }
    if new_max <= config.max_slippage_tolerance {
        return Err(ContractError::MaxSlippageMustBeHigher {
            proposed: new_max,
            current: config.max_slippage_tolerance,
        });
    }
    if new_max > MAX_SLIPPAGE_TOLERANCE_CEILING {
        return Err(ContractError::MaxSlippageAboveCeiling {
            proposed: new_max,
            ceiling: MAX_SLIPPAGE_TOLERANCE_CEILING,
        });
    }

    let effective_at = env.block.time.seconds() + MAX_SLIPPAGE_RAISE_DELAY_SECONDS;
    config.pending_max_slippage = Some(new_max);
    config.pending_max_slippage_effective_at = Some(effective_at);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_max_slippage_raise")
        .add_attribute("proposed_max", new_max.to_string())
        .add_attribute("effective_at", effective_at.to_string()))
}

pub fn execute_apply_max_slippage_raise(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    let pending = config
        .pending_max_slippage
        .take()
        .ok_or(ContractError::NoPendingMaxSlippageRaise {})?;
    let effective_at = config
        .pending_max_slippage_effective_at
        .take()
        .ok_or(ContractError::NoPendingMaxSlippageRaise {})?;

    if env.block.time.seconds() < effective_at {
        return Err(ContractError::MaxSlippageRaiseNotReady {});
    }

    // Revalidate against the ceiling in case the constant is lowered in a future
    // contract migration while a raise is pending. Protects against a scenario where
    // a pending proposal outlives the ceiling that validated it at propose-time.
    if pending > MAX_SLIPPAGE_TOLERANCE_CEILING {
        return Err(ContractError::MaxSlippageAboveCeiling {
            proposed: pending,
            ceiling: MAX_SLIPPAGE_TOLERANCE_CEILING,
        });
    }

    let previous_max = config.max_slippage_tolerance;
    config.max_slippage_tolerance = pending;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "apply_max_slippage_raise")
        .add_attribute("previous_max", previous_max.to_string())
        .add_attribute("new_max", pending.to_string()))
}

pub fn execute_cancel_max_slippage_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if config.pending_max_slippage.is_none() {
        return Err(ContractError::NoPendingMaxSlippageRaise {});
    }

    config.pending_max_slippage = None;
    config.pending_max_slippage_effective_at = None;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "cancel_max_slippage_proposal"))
}

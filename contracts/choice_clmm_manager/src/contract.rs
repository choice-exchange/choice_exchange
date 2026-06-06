#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    ensure, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, Event,
    MessageInfo, Reply, Response, StdError, StdResult, SubMsg, Uint128, Uint256, WasmMsg,
};
use cw2::set_contract_version;
use std::convert::TryFrom;
use std::str::FromStr;

use cw721::msg::NftExtensionMsg;
use cw721::traits::Cw721Query;
use cw721_base::traits::Cw721Execute;
use cw721_metadata_onchain::Cw721MetadataContract;

use choice_clmm_common::factory::QueryMsg as FactoryQueryMsg;
use choice_clmm_common::manager::{
    ExecuteMsg, InstantiateMsg, MigrateMsg, PositionWithFeesResponse, QueryMsg,
};
use choice_clmm_common::pool::{
    ExecuteMsg as PoolExecuteMsg, FeeGrowthInsideResponse, PoolState, QueryMsg as PoolQueryMsg,
};
use choice_clmm_common::types::AssetInfo;

use choice_clmm_math::full_math::mul_div;
use choice_clmm_math::liquidity_math::get_liquidity_for_amounts;
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;

use crate::error::ContractError;
use crate::state::{
    next_token_id, q128, Config, PendingCollect, PendingDecrease, PendingIncrease, PendingMint,
    PositionState,
    CONFIG, PENDING_COLLECT, PENDING_DECREASE, PENDING_INCREASE, PENDING_MINT, POSITIONS,
    REPLY_COLLECT, REPLY_DECREASE_LIQUIDITY, REPLY_INCREASE_LIQUIDITY, REPLY_MINT_POSITION,
    TOKEN_ID_COUNTER,
};

const CONTRACT_NAME: &str = "crates.io:choice-clmm-manager";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

// =========================================================================
// Parameter structs (keep clippy::too_many_arguments happy)
// =========================================================================

pub struct MintParams {
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub amount0_desired: Uint128,
    pub amount1_desired: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub recipient: Option<String>,
    pub deadline: u64,
}

pub struct IncreaseParams {
    pub token_id: String,
    pub amount0_desired: Uint128,
    pub amount1_desired: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub deadline: u64,
}

pub struct DecreaseParams {
    pub token_id: String,
    pub liquidity: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub deadline: u64,
}

/// Reject any native funds attached to an entrypoint that does not consume
/// them (mirrors `cw_utils::nonpayable`, which is not a dependency). Only
/// `MintPosition` / `IncreaseLiquidity` forward funds to the pool.
fn ensure_nonpayable(info: &MessageInfo) -> Result<(), ContractError> {
    if !info.funds.is_empty() {
        return Err(ContractError::Std(StdError::generic_err(
            "no funds may be attached to this message",
        )));
    }
    Ok(())
}

/// Parse a `token_id` string (decimal u64, as produced by `next_token_id`)
/// into the numeric key used for the `PENDING_*` maps and the SubMsg payload.
fn token_id_key(token_id: &str) -> Result<u64, ContractError> {
    token_id
        .parse::<u64>()
        .map_err(|e| ContractError::Std(StdError::generic_err(format!(
            "invalid token_id {token_id}: {e}"
        ))))
}

/// Recover the `token_id` numeric key a reply was tagged with via
/// `SubMsg::with_payload(token_id.to_be_bytes())`.
fn token_id_from_payload(payload: &Binary) -> Result<u64, ContractError> {
    let bytes = <[u8; 8]>::try_from(payload.as_slice()).map_err(|_| {
        ContractError::InvalidReply {
            reason: format!(
                "expected 8-byte token_id payload, got {} bytes",
                payload.len()
            ),
        }
    })?;
    Ok(u64::from_be_bytes(bytes))
}

fn validate_deadline(env: &Env, deadline: u64) -> Result<(), ContractError> {
    if deadline > 0 && env.block.time.seconds() > deadline {
        return Err(ContractError::DeadlineExceeded {});
    }
    Ok(())
}

// =========================================================================
// Authorization helpers
// =========================================================================

/// Assert that `info.sender` is the NFT owner or authorized via cw721
/// per-token approval OR cw721 operator-wide (`ApproveAll`) approval. Expired
/// approvals are rejected.
///
/// Regression fixes (audit HIGH):
///   - Include cw721 `Operator` approvals (ApproveAll), previously ignored.
///   - Reject expired approvals by passing `include_expired = false`.
fn assert_owner_or_approved(
    deps: Deps,
    env: &Env,
    info: &MessageInfo,
    token_id: &str,
) -> Result<(), ContractError> {
    let base = Cw721MetadataContract::default();

    let owner_res = base.query_owner_of(deps, env, token_id.to_string(), false)?;
    if info.sender.to_string() == owner_res.owner {
        return Ok(());
    }

    // Check per-token approvals (ignores expired).
    for approval in &owner_res.approvals {
        if approval.spender == info.sender {
            return Ok(());
        }
    }

    // Check ApproveAll operator approval (ignores expired).
    let operator_res = base.query_operator(
        deps,
        env,
        owner_res.owner,
        info.sender.to_string(),
        false,
    );
    if let Ok(op) = operator_res {
        // query_operator errors if no operator is set; success means the
        // operator exists and (since include_expired=false) is not expired.
        let _ = op;
        return Ok(());
    }

    Err(ContractError::Unauthorized {})
}

// =========================================================================
// Entrypoints
// =========================================================================

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let config = Config {
        factory: deps.api.addr_validate(&msg.factory_addr)?,
    };
    CONFIG.save(deps.storage, &config)?;
    TOKEN_ID_COUNTER.save(deps.storage, &1u64)?;

    let cw721_msg = cw721_base::msg::InstantiateMsg {
        name: msg.name,
        symbol: msg.symbol,
        minter: Some(env.contract.address.to_string()),
        withdraw_address: None,
        collection_info_extension: None,
        creator: None,
    };

    Cw721MetadataContract::default().instantiate(deps, &env, &info, cw721_msg)?;

    Ok(Response::new().add_attribute("action", "instantiate"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let base_contract = Cw721MetadataContract::default();

    // Reject native funds on every entrypoint that does not consume them. Only
    // `MintPosition` / `IncreaseLiquidity` legitimately carry funds (forwarded
    // to the pool, surplus refunded); all other entrypoints — decrease, collect,
    // burn, and the cw721 pass-throughs — would otherwise strand attached coins
    // in the manager contract.
    let payable = matches!(
        msg,
        ExecuteMsg::MintPosition { .. } | ExecuteMsg::IncreaseLiquidity { .. }
    );
    if !payable {
        ensure_nonpayable(&info)?;
    }

    match msg {
        ExecuteMsg::MintPosition {
            token0,
            token1,
            fee,
            tick_lower,
            tick_upper,
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
            recipient,
            deadline,
        } => execute_mint_position(
            deps,
            env,
            info,
            MintParams {
                token0,
                token1,
                fee,
                tick_lower,
                tick_upper,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
                recipient,
                deadline,
            },
        ),

        ExecuteMsg::IncreaseLiquidity {
            token_id,
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
            deadline,
        } => execute_increase_liquidity(
            deps,
            env,
            info,
            IncreaseParams {
                token_id,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
                deadline,
            },
        ),

        ExecuteMsg::DecreaseLiquidity {
            token_id,
            liquidity,
            amount0_min,
            amount1_min,
            deadline,
        } => execute_decrease_liquidity(
            deps,
            env,
            info,
            DecreaseParams {
                token_id,
                liquidity,
                amount0_min,
                amount1_min,
                deadline,
            },
        ),

        ExecuteMsg::Collect {
            token_id,
            recipient,
        } => execute_collect(deps, env, info, token_id, recipient),

        ExecuteMsg::Burn { token_id } => execute_burn(deps, env, info, token_id),

        // Standard CW721 delegations
        ExecuteMsg::TransferNft {
            recipient,
            token_id,
        } => Ok(base_contract.transfer_nft(deps, &env, &info, recipient, token_id)?),
        ExecuteMsg::SendNft {
            contract,
            token_id,
            msg,
        } => Ok(base_contract.send_nft(deps, &env, &info, contract, token_id, msg)?),
        ExecuteMsg::Approve {
            spender,
            token_id,
            expires,
        } => Ok(base_contract.approve(deps, &env, &info, spender, token_id, expires)?),
        ExecuteMsg::Revoke { spender, token_id } => {
            Ok(base_contract.revoke(deps, &env, &info, spender, token_id)?)
        }
        ExecuteMsg::ApproveAll { operator, expires } => {
            Ok(base_contract.approve_all(deps, &env, &info, operator, expires)?)
        }
        ExecuteMsg::RevokeAll { operator } => {
            Ok(base_contract.revoke_all(deps, &env, &info, operator)?)
        }
    }
}

// =========================================================================
// MintPosition
// =========================================================================

fn execute_mint_position(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    mut params: MintParams,
) -> Result<Response, ContractError> {
    validate_deadline(&env, params.deadline)?;

    // C-M1: canonicalize token order BEFORE any computation, pool query, or
    // liquidity calc. The factory's `GetPool` sorts `(token0, token1)` with
    // `AssetInfo`'s `Ord` (NativeToken < Token, then lexicographic) and the
    // pool stores its tokens in that canonical order, so a caller may pass
    // either order and still resolve the same pool. If we kept the caller's
    // order we would mislabel amounts, mis-route funds, and persist a swapped
    // `PositionState`. We canonicalize transparently so integrators that pass
    // either order work.
    //
    // Tick bounds are NOT swapped: they are price-range bounds expressed in the
    // pool's canonical orientation (we feed them straight to `pool.Mint`, which
    // interprets them against its canonical slot0 that we also query here). They
    // are independent of which token the caller happens to label `token0`. Only
    // the token/amount pairs follow the sort so `get_liquidity_for_amounts`
    // receives `amount0` paired with the canonical `token0`.
    if params.token0 >= params.token1 {
        std::mem::swap(&mut params.token0, &mut params.token1);
        std::mem::swap(&mut params.amount0_desired, &mut params.amount1_desired);
        std::mem::swap(&mut params.amount0_min, &mut params.amount1_min);
    }

    if params.tick_lower >= params.tick_upper {
        return Err(ContractError::Std(StdError::generic_err(
            "tick_lower must be < tick_upper",
        )));
    }

    let config = CONFIG.load(deps.storage)?;

    // Resolve Pool
    let pool_addr_str: String = deps.querier.query_wasm_smart(
        config.factory,
        &FactoryQueryMsg::GetPool {
            token_a: params.token0.clone(),
            token_b: params.token1.clone(),
            fee: params.fee,
        },
    )?;
    let pool_address = deps.api.addr_validate(&pool_addr_str)?;

    // Allocate token_id.
    let counter = TOKEN_ID_COUNTER.load(deps.storage)?;
    let (token_id, next_counter) = next_token_id(counter);
    TOKEN_ID_COUNTER.save(deps.storage, &next_counter)?;
    // Numeric key for the PENDING_MINT map / reply payload. `counter` is the
    // exact u64 that `next_token_id` stringified.
    let token_id_key = counter;

    // Current slot0 to compute actuals.
    let slot0: PoolState = deps
        .querier
        .query_wasm_smart(pool_address.to_string(), &PoolQueryMsg::GetSlot0 {})?;

    let sqrt_price_lower = get_sqrt_ratio_at_tick(params.tick_lower)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(params.tick_upper)?;

    let liquidity = get_liquidity_for_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        params.amount0_desired,
        params.amount1_desired,
    )?;
    if liquidity.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "computed liquidity is zero — check desired amounts vs current price",
        )));
    }

    // Compute exact amounts the pool will pull (matches the pool's own calc).
    let (amount0_actual, amount1_actual) = compute_token_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        liquidity.u128(),
        slot0.tick,
        params.tick_lower,
        params.tick_upper,
    )?;

    // Slippage: enforce pre-submission. Pool's slot0 is committed state
    // visible atomically to this same tx, so pre-check matches reality.
    if amount0_actual < params.amount0_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount0 {} below minimum {}",
                amount0_actual, params.amount0_min
            ),
        });
    }
    if amount1_actual < params.amount1_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount1 {} below minimum {}",
                amount1_actual, params.amount1_min
            ),
        });
    }

    let recipient_str = params.recipient.unwrap_or_else(|| info.sender.to_string());
    let recipient_addr = deps.api.addr_validate(&recipient_str)?;

    // Prepare funds flow: pull exactly `amount0_actual` / `amount1_actual`.
    //
    // Native refunds: `info.funds` minus actuals, returned to sender in the
    // same response.
    // CW20: TransferFrom only the actual amount + grant pool an exact-amount
    // allowance. No excess allowance lingers.
    let mut pre_messages: Vec<CosmosMsg> = vec![];
    let mut funds_to_pool: Vec<Coin> = vec![];
    let mut native_refunds: Vec<Coin> = vec![];

    prepare_token_forward(
        &params.token0,
        amount0_actual,
        &info,
        pool_address.as_ref(),
        env.contract.address.as_ref(),
        &mut funds_to_pool,
        &mut pre_messages,
    )?;
    prepare_token_forward(
        &params.token1,
        amount1_actual,
        &info,
        pool_address.as_ref(),
        env.contract.address.as_ref(),
        &mut funds_to_pool,
        &mut pre_messages,
    )?;
    compute_native_refunds(
        &info,
        &[
            (&params.token0, amount0_actual),
            (&params.token1, amount1_actual),
        ],
        &mut native_refunds,
    )?;

    // Stage pending context for the reply.
    let pending = PendingMint {
        token_id: token_id.clone(),
        owner: recipient_addr.to_string(),
        pool_address: pool_address.clone(),
        token0: params.token0,
        token1: params.token1,
        fee: params.fee,
        tick_lower: params.tick_lower,
        tick_upper: params.tick_upper,
        liquidity,
        amount0_min: params.amount0_min,
        amount1_min: params.amount1_min,
        amount0_sent_to_pool: amount0_actual,
        amount1_sent_to_pool: amount1_actual,
        native_refunds: native_refunds.clone(),
    };
    PENDING_MINT.save(deps.storage, token_id_key, &pending)?;

    let pool_mint_msg = PoolExecuteMsg::Mint {
        lower_tick: params.tick_lower,
        upper_tick: params.tick_upper,
        amount: liquidity,
    };
    let pool_mint_submsg = SubMsg::reply_on_success(
        WasmMsg::Execute {
            contract_addr: pool_address.to_string(),
            msg: to_json_binary(&pool_mint_msg)?,
            funds: funds_to_pool,
        },
        REPLY_MINT_POSITION,
    )
    .with_payload(token_id_key.to_be_bytes().to_vec());

    // Build response: pre-messages, then the submsg.
    let mut response = Response::new().add_messages(pre_messages);
    // Native refunds can go out immediately — they only return surplus that
    // never needed to enter the pool. Failure of the pool submsg will revert
    // everything anyway.
    for coin in native_refunds {
        response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: vec![coin],
        }));
    }
    Ok(response.add_submessage(pool_mint_submsg).add_attributes([
        ("action", "mint_position"),
        ("token_id", token_id.as_str()),
        ("liquidity", liquidity.to_string().as_str()),
        ("amount0_sent", amount0_actual.to_string().as_str()),
        ("amount1_sent", amount1_actual.to_string().as_str()),
    ]))
}

fn reply_mint_position(deps: DepsMut, env: Env, reply: Reply) -> Result<Response, ContractError> {
    let token_id_key = token_id_from_payload(&reply.payload)?;
    let pending = PENDING_MINT.load(deps.storage, token_id_key)?;
    PENDING_MINT.remove(deps.storage, token_id_key);

    // Verify the pool consumed exactly what we sent. Matches the increase-
    // liquidity reply's invariant — surfaces any future mint-math divergence
    // between manager and pool rather than stranding pool-refunded native
    // dust in the manager contract.
    let (amount0_consumed, amount1_consumed) = parse_amounts_from_reply(
        &reply,
        "amount0_consumed",
        "amount1_consumed",
        pending.pool_address.as_str(),
    )?;
    if amount0_consumed != pending.amount0_sent_to_pool
        || amount1_consumed != pending.amount1_sent_to_pool
    {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "pool consumed ({}, {}) but manager sent ({}, {}) — mint math diverged",
            amount0_consumed,
            amount1_consumed,
            pending.amount0_sent_to_pool,
            pending.amount1_sent_to_pool,
        ))));
    }

    // Fetch current fee_growth_inside so the NFT's starting snapshot is right.
    // After pool.Mint, ticks at `tick_lower`/`tick_upper` are initialized, so
    // the query cannot fail due to missing ticks.
    let fg: FeeGrowthInsideResponse = deps.querier.query_wasm_smart(
        pending.pool_address.to_string(),
        &PoolQueryMsg::GetFeeGrowthInside {
            tick_lower: pending.tick_lower,
            tick_upper: pending.tick_upper,
        },
    )?;

    let state = PositionState {
        pool_address: pending.pool_address.clone(),
        token0: pending.token0,
        token1: pending.token1,
        fee: pending.fee,
        tick_lower: pending.tick_lower,
        tick_upper: pending.tick_upper,
        liquidity: pending.liquidity,
        fee_growth_inside_0_last_x128: fg.fee_growth_inside_0_x128,
        fee_growth_inside_1_last_x128: fg.fee_growth_inside_1_x128,
        tokens_owed_0: Uint128::zero(),
        tokens_owed_1: Uint128::zero(),
    };
    POSITIONS.save(deps.storage, &pending.token_id, &state)?;

    // Mint NFT.
    let metadata: NftExtensionMsg = state.metadata().into();
    let self_info = MessageInfo {
        sender: env.contract.address.clone(),
        funds: vec![],
    };
    Cw721MetadataContract::default().mint(
        deps,
        &env,
        &self_info,
        pending.token_id.clone(),
        pending.owner.clone(),
        None,
        Some(metadata),
    )?;

    Ok(Response::new()
        .add_event(
            Event::new("mint_position_complete")
                .add_attribute("token_id", pending.token_id)
                .add_attribute("owner", pending.owner)
                .add_attribute("pool_address", pending.pool_address.as_str())
                .add_attribute("fee", pending.fee.to_string())
                .add_attribute("tick_lower", pending.tick_lower.to_string())
                .add_attribute("tick_upper", pending.tick_upper.to_string())
                .add_attribute("liquidity", pending.liquidity)
                .add_attribute("amount0", pending.amount0_sent_to_pool)
                .add_attribute("amount1", pending.amount1_sent_to_pool),
        ))
}

// =========================================================================
// IncreaseLiquidity
// =========================================================================

fn execute_increase_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: IncreaseParams,
) -> Result<Response, ContractError> {
    validate_deadline(&env, params.deadline)?;
    assert_owner_or_approved(deps.as_ref(), &env, &info, &params.token_id)?;
    let token_id_key = token_id_key(&params.token_id)?;

    let state = POSITIONS
        .load(deps.storage, &params.token_id)
        .map_err(|_| ContractError::Std(StdError::generic_err("Position not found")))?;

    let slot0: PoolState = deps
        .querier
        .query_wasm_smart(state.pool_address.to_string(), &PoolQueryMsg::GetSlot0 {})?;

    let sqrt_price_lower = get_sqrt_ratio_at_tick(state.tick_lower)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(state.tick_upper)?;

    let liquidity_added = get_liquidity_for_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        params.amount0_desired,
        params.amount1_desired,
    )?;
    if liquidity_added.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "computed liquidity is zero — check desired amounts vs current price",
        )));
    }

    let (amount0_actual, amount1_actual) = compute_token_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        liquidity_added.u128(),
        slot0.tick,
        state.tick_lower,
        state.tick_upper,
    )?;
    if amount0_actual < params.amount0_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount0 {} below minimum {}",
                amount0_actual, params.amount0_min
            ),
        });
    }
    if amount1_actual < params.amount1_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount1 {} below minimum {}",
                amount1_actual, params.amount1_min
            ),
        });
    }

    let mut pre_messages: Vec<CosmosMsg> = vec![];
    let mut funds_to_pool: Vec<Coin> = vec![];
    let mut native_refunds: Vec<Coin> = vec![];

    prepare_token_forward(
        &state.token0,
        amount0_actual,
        &info,
        state.pool_address.as_ref(),
        env.contract.address.as_ref(),
        &mut funds_to_pool,
        &mut pre_messages,
    )?;
    prepare_token_forward(
        &state.token1,
        amount1_actual,
        &info,
        state.pool_address.as_ref(),
        env.contract.address.as_ref(),
        &mut funds_to_pool,
        &mut pre_messages,
    )?;
    compute_native_refunds(
        &info,
        &[
            (&state.token0, amount0_actual),
            (&state.token1, amount1_actual),
        ],
        &mut native_refunds,
    )?;

    let pending = PendingIncrease {
        token_id: params.token_id.clone(),
        liquidity_added,
        amount0_min: params.amount0_min,
        amount1_min: params.amount1_min,
        amount0_sent_to_pool: amount0_actual,
        amount1_sent_to_pool: amount1_actual,
        native_refunds: native_refunds.clone(),
    };
    PENDING_INCREASE.save(deps.storage, token_id_key, &pending)?;

    let pool_mint_msg = PoolExecuteMsg::Mint {
        lower_tick: state.tick_lower,
        upper_tick: state.tick_upper,
        amount: liquidity_added,
    };
    let pool_mint_submsg = SubMsg::reply_on_success(
        WasmMsg::Execute {
            contract_addr: state.pool_address.to_string(),
            msg: to_json_binary(&pool_mint_msg)?,
            funds: funds_to_pool,
        },
        REPLY_INCREASE_LIQUIDITY,
    )
    .with_payload(token_id_key.to_be_bytes().to_vec());

    let mut response = Response::new().add_messages(pre_messages);
    for coin in native_refunds {
        response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: vec![coin],
        }));
    }
    Ok(response
        .add_submessage(pool_mint_submsg)
        .add_attribute("action", "increase_liquidity")
        .add_attribute("token_id", params.token_id)
        .add_attribute("liquidity_added", liquidity_added))
}

fn reply_increase_liquidity(deps: DepsMut, _env: Env, reply: Reply) -> Result<Response, ContractError> {
    let token_id_key = token_id_from_payload(&reply.payload)?;
    let pending = PENDING_INCREASE.load(deps.storage, token_id_key)?;
    PENDING_INCREASE.remove(deps.storage, token_id_key);

    let mut state = POSITIONS.load(deps.storage, &pending.token_id)?;

    // Verify the pool consumed exactly what we sent. Manager and pool use the
    // same delta math today so this is a tautology — but asymmetry with the
    // decrease reply (which parses amounts) was fragile: a future rounding
    // tweak on one side only would silently strand pool-refunded native dust
    // in the manager contract. Error out on mismatch instead.
    let (amount0_consumed, amount1_consumed) = parse_amounts_from_reply(
        &reply,
        "amount0_consumed",
        "amount1_consumed",
        state.pool_address.as_str(),
    )?;
    if amount0_consumed != pending.amount0_sent_to_pool
        || amount1_consumed != pending.amount1_sent_to_pool
    {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "pool consumed ({}, {}) but manager sent ({}, {}) — mint math diverged",
            amount0_consumed,
            amount1_consumed,
            pending.amount0_sent_to_pool,
            pending.amount1_sent_to_pool,
        ))));
    }

    // Accrue pending fees to NFT before modifying liquidity (V3 pattern: any
    // change to L closes the current "fee period" for this NFT).
    let fg: FeeGrowthInsideResponse = deps.querier.query_wasm_smart(
        state.pool_address.to_string(),
        &PoolQueryMsg::GetFeeGrowthInside {
            tick_lower: state.tick_lower,
            tick_upper: state.tick_upper,
        },
    )?;
    accrue_fees_to_nft(&mut state, &fg)?;

    // Apply liquidity delta.
    state.liquidity = state.liquidity.checked_add(pending.liquidity_added).map_err(|_| {
        StdError::generic_err("NFT liquidity overflow")
    })?;

    POSITIONS.save(deps.storage, &pending.token_id, &state)?;

    Ok(Response::new().add_event(
        Event::new("increase_liquidity_complete")
            .add_attribute("token_id", pending.token_id)
            .add_attribute("liquidity_added", pending.liquidity_added)
            .add_attribute("new_liquidity", state.liquidity)
            .add_attribute("amount0_consumed", amount0_consumed)
            .add_attribute("amount1_consumed", amount1_consumed),
    ))
}

// =========================================================================
// DecreaseLiquidity
// =========================================================================

fn execute_decrease_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: DecreaseParams,
) -> Result<Response, ContractError> {
    validate_deadline(&env, params.deadline)?;
    assert_owner_or_approved(deps.as_ref(), &env, &info, &params.token_id)?;
    let token_id_key = token_id_key(&params.token_id)?;

    let state = POSITIONS.load(deps.storage, &params.token_id)?;

    if params.liquidity.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "liquidity to remove must be > 0",
        )));
    }
    if params.liquidity > state.liquidity {
        return Err(ContractError::Std(StdError::generic_err(
            "liquidity to remove exceeds NFT liquidity",
        )));
    }

    let pending = PendingDecrease {
        token_id: params.token_id.clone(),
        liquidity_removed: params.liquidity,
        amount0_min: params.amount0_min,
        amount1_min: params.amount1_min,
    };
    PENDING_DECREASE.save(deps.storage, token_id_key, &pending)?;

    let burn_msg = PoolExecuteMsg::Burn {
        lower_tick: state.tick_lower,
        upper_tick: state.tick_upper,
        amount: params.liquidity,
    };
    let submsg = SubMsg::reply_on_success(
        WasmMsg::Execute {
            contract_addr: state.pool_address.to_string(),
            msg: to_json_binary(&burn_msg)?,
            funds: vec![],
        },
        REPLY_DECREASE_LIQUIDITY,
    )
    .with_payload(token_id_key.to_be_bytes().to_vec());

    Ok(Response::new()
        .add_submessage(submsg)
        .add_attribute("action", "decrease_liquidity")
        .add_attribute("token_id", params.token_id)
        .add_attribute("liquidity", params.liquidity))
}

fn reply_decrease_liquidity(
    deps: DepsMut,
    _env: Env,
    reply: Reply,
) -> Result<Response, ContractError> {
    let token_id_key = token_id_from_payload(&reply.payload)?;
    let pending = PENDING_DECREASE.load(deps.storage, token_id_key)?;
    PENDING_DECREASE.remove(deps.storage, token_id_key);

    let mut state = POSITIONS.load(deps.storage, &pending.token_id)?;

    // Extract amount0_burned / amount1_burned attributes from the pool event.
    // Filter by emitter so a malicious CW20 in the burn submsg chain can't
    // inject counterfeit attributes. Today `pool.Burn` dispatches no
    // submessages, so the filter is defense in depth.
    let (amount0_burned, amount1_burned) = parse_amounts_from_reply(
        &reply,
        "amount0_burned",
        "amount1_burned",
        state.pool_address.as_str(),
    )?;

    if amount0_burned < pending.amount0_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount0_burned {} below minimum {}",
                amount0_burned, pending.amount0_min
            ),
        });
    }
    if amount1_burned < pending.amount1_min {
        return Err(ContractError::Slippage {
            reason: format!(
                "amount1_burned {} below minimum {}",
                amount1_burned, pending.amount1_min
            ),
        });
    }

    // Accrue pending fees before liquidity change (V3 pattern). Read the
    // post-update `fee_growth_inside` directly from the burn event attributes
    // — a post-burn `GetFeeGrowthInside` query would read zeros if this burn
    // cleared either tick (final-exit case), silently advancing the NFT's
    // snapshot past fees it never credited.
    let (fg_inside_0, fg_inside_1) =
        parse_fg_inside_from_reply(&reply, state.pool_address.as_str())?;
    let fg = FeeGrowthInsideResponse {
        fee_growth_inside_0_x128: fg_inside_0,
        fee_growth_inside_1_x128: fg_inside_1,
    };
    accrue_fees_to_nft(&mut state, &fg)?;

    // Credit burned principal to NFT's tokens_owed.
    state.tokens_owed_0 = state
        .tokens_owed_0
        .checked_add(amount0_burned)
        .unwrap_or(Uint128::MAX);
    state.tokens_owed_1 = state
        .tokens_owed_1
        .checked_add(amount1_burned)
        .unwrap_or(Uint128::MAX);

    // Reduce NFT's liquidity.
    state.liquidity = state
        .liquidity
        .checked_sub(pending.liquidity_removed)
        .map_err(|_| StdError::generic_err("NFT liquidity underflow"))?;

    POSITIONS.save(deps.storage, &pending.token_id, &state)?;

    Ok(Response::new().add_event(
        Event::new("decrease_liquidity_complete")
            .add_attribute("token_id", pending.token_id)
            .add_attribute("amount0_burned", amount0_burned)
            .add_attribute("amount1_burned", amount1_burned)
            .add_attribute("new_liquidity", state.liquidity),
    ))
}

// =========================================================================
// Collect
// =========================================================================

fn execute_collect(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_id: String,
    recipient: Option<String>,
) -> Result<Response, ContractError> {
    assert_owner_or_approved(deps.as_ref(), &env, &info, &token_id)?;

    let base = Cw721MetadataContract::default();
    let owner_res = base.query_owner_of(deps.as_ref(), &env, token_id.clone(), false)?;
    let dest = match recipient {
        Some(r) => deps.api.addr_validate(&r)?.to_string(),
        None => owner_res.owner,
    };

    let mut state = POSITIONS.load(deps.storage, &token_id)?;

    // If NFT has active liquidity, force the pool to credit its own
    // position.tokens_owed with the latest fee delta BEFORE we call
    // pool.Collect. V3 does this with `pool.burn(0)`; we mirror it.
    //
    // After that, we query fee_growth_inside (unchanged by burn(0)) and
    // accrue fees to this NFT's tokens_owed locally.
    //
    // NOTE: we do NOT use a submsg here because fee_growth_inside is stable
    // across burn(0) — swaps are the only thing that can change it, and swaps
    // can't happen in the middle of our own tx. So we can:
    //   1. (msg) pool.burn(0)           — credits pool.position.tokens_owed
    //   2. (msg) pool.collect(amount)   — pays out to recipient
    //   3. (inline) update this NFT's state locally
    //
    // Step 3 depends only on data we already have plus fg queried NOW.
    let mut messages: Vec<CosmosMsg> = vec![];

    if !state.liquidity.is_zero() {
        // 1. Force pool to roll fees into position.tokens_owed.
        let burn_zero = PoolExecuteMsg::Burn {
            lower_tick: state.tick_lower,
            upper_tick: state.tick_upper,
            amount: Uint128::zero(),
        };
        messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: state.pool_address.to_string(),
            msg: to_json_binary(&burn_zero)?,
            funds: vec![],
        }));

        // 2. Accrue fees into this NFT's tokens_owed using current
        //    fee_growth_inside. burn(0) doesn't change fee_growth_inside, so
        //    reading it BEFORE the message dispatches is still correct (the
        //    value we read is exactly the value the pool's update_position
        //    will compare against during burn(0)).
        let fg: FeeGrowthInsideResponse = deps.querier.query_wasm_smart(
            state.pool_address.to_string(),
            &PoolQueryMsg::GetFeeGrowthInside {
                tick_lower: state.tick_lower,
                tick_upper: state.tick_upper,
            },
        )?;
        accrue_fees_to_nft(&mut state, &fg)?;
    }

    // Snapshot how much we'll ask the pool for. Persist NFT state WITHOUT
    // zeroing `tokens_owed_*` — the reply handler decrements by the actual
    // paid amount so we never strand funds if the pool clamps its payout to
    // its own `position.tokens_owed` (e.g. after u128::MAX fee saturation
    // or any future invariant drift between manager and pool).
    let pay0 = state.tokens_owed_0;
    let pay1 = state.tokens_owed_1;

    POSITIONS.save(deps.storage, &token_id, &state)?;

    if pay0.is_zero() && pay1.is_zero() {
        return Ok(Response::new()
            .add_messages(messages)
            .add_attribute("action", "collect")
            .add_attribute("manager_action", "collect")
            .add_attribute("token_id", token_id)
            .add_attribute("recipient", dest)
            .add_attribute("amount0", pay0)
            .add_attribute("amount1", pay1));
    }

    let token_id_key = token_id_key(&token_id)?;
    PENDING_COLLECT.save(
        deps.storage,
        token_id_key,
        &PendingCollect {
            token_id: token_id.clone(),
            amount0_requested: pay0,
            amount1_requested: pay1,
        },
    )?;

    let collect_msg = PoolExecuteMsg::Collect {
        recipient: dest.clone(),
        lower_tick: state.tick_lower,
        upper_tick: state.tick_upper,
        amount0_requested: pay0,
        amount1_requested: pay1,
    };
    let collect_submsg = SubMsg::reply_on_success(
        WasmMsg::Execute {
            contract_addr: state.pool_address.to_string(),
            msg: to_json_binary(&collect_msg)?,
            funds: vec![],
        },
        REPLY_COLLECT,
    )
    .with_payload(token_id_key.to_be_bytes().to_vec());

    let mut response = Response::new();
    for m in messages {
        response = response.add_message(m);
    }
    Ok(response
        .add_submessage(collect_submsg)
        .add_attribute("action", "collect")
        .add_attribute("manager_action", "collect")
        .add_attribute("token_id", token_id)
        .add_attribute("recipient", dest)
        .add_attribute("amount0_requested", pay0)
        .add_attribute("amount1_requested", pay1))
}

fn reply_collect(
    deps: DepsMut,
    _env: Env,
    reply: Reply,
) -> Result<Response, ContractError> {
    let token_id_key = token_id_from_payload(&reply.payload)?;
    let pending = PENDING_COLLECT.load(deps.storage, token_id_key)?;
    PENDING_COLLECT.remove(deps.storage, token_id_key);

    let mut state = POSITIONS.load(deps.storage, &pending.token_id)?;

    // Pool's Collect emits the actual paid amounts as `amount0` / `amount1`
    // (which equal the request when the pool has enough owed, or clamp to
    // `pool.position.tokens_owed_*` otherwise).
    let (amount0_paid, amount1_paid) = parse_amounts_from_reply(
        &reply,
        "amount0",
        "amount1",
        state.pool_address.as_str(),
    )?;

    // Defensive: pool is supposed to cap at our request, never exceed it.
    if amount0_paid > pending.amount0_requested || amount1_paid > pending.amount1_requested {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "pool paid ({}, {}) above requested ({}, {}) — invariant violated",
            amount0_paid,
            amount1_paid,
            pending.amount0_requested,
            pending.amount1_requested,
        ))));
    }

    // Decrement NFT's owed by exactly what the pool paid. If the pool paid
    // less than requested (shortfall), the leftover stays on the NFT for a
    // later Collect to retry. Surfaces as a loud attribute so it's not
    // silent — in normal operation paid == requested.
    state.tokens_owed_0 = state
        .tokens_owed_0
        .checked_sub(amount0_paid)
        .map_err(|_| StdError::generic_err("tokens_owed_0 underflow in collect reply"))?;
    state.tokens_owed_1 = state
        .tokens_owed_1
        .checked_sub(amount1_paid)
        .map_err(|_| StdError::generic_err("tokens_owed_1 underflow in collect reply"))?;

    POSITIONS.save(deps.storage, &pending.token_id, &state)?;

    let shortfall_0 = pending.amount0_requested.checked_sub(amount0_paid).unwrap_or(Uint128::zero());
    let shortfall_1 = pending.amount1_requested.checked_sub(amount1_paid).unwrap_or(Uint128::zero());

    Ok(Response::new().add_event(
        Event::new("collect_complete")
            .add_attribute("token_id", pending.token_id)
            .add_attribute("amount0_paid", amount0_paid)
            .add_attribute("amount1_paid", amount1_paid)
            .add_attribute("amount0_shortfall", shortfall_0)
            .add_attribute("amount1_shortfall", shortfall_1),
    ))
}

// =========================================================================
// Burn (NFT destruction)
// =========================================================================

fn execute_burn(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_id: String,
) -> Result<Response, ContractError> {
    // SECURITY (audit CRIT-3):
    //   - Assert ownership BEFORE mutating any storage.
    //   - Require the NFT's position to be fully cleared: zero liquidity and
    //     zero tokens_owed on both sides. Otherwise funds are stranded.
    assert_owner_or_approved(deps.as_ref(), &env, &info, &token_id)?;

    let state = POSITIONS.load(deps.storage, &token_id)?;
    if !state.liquidity.is_zero() {
        return Err(ContractError::PositionNotCleared {
            field: "liquidity".to_string(),
        });
    }
    if !state.tokens_owed_0.is_zero() {
        return Err(ContractError::PositionNotCleared {
            field: "tokens_owed_0".to_string(),
        });
    }
    if !state.tokens_owed_1.is_zero() {
        return Err(ContractError::PositionNotCleared {
            field: "tokens_owed_1".to_string(),
        });
    }

    POSITIONS.remove(deps.storage, &token_id);

    let base = Cw721MetadataContract::default();
    let response = base.burn_nft(deps, &env, &info, token_id.clone())?;
    Ok(response.add_event(Event::new("burn_position_complete").add_attribute("token_id", token_id)))
}

// =========================================================================
// Reply dispatcher
// =========================================================================

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, reply: Reply) -> Result<Response, ContractError> {
    match reply.id {
        REPLY_MINT_POSITION => reply_mint_position(deps, env, reply),
        REPLY_INCREASE_LIQUIDITY => reply_increase_liquidity(deps, env, reply),
        REPLY_DECREASE_LIQUIDITY => reply_decrease_liquidity(deps, env, reply),
        REPLY_COLLECT => reply_collect(deps, env, reply),
        other => Err(ContractError::InvalidReply {
            reason: format!("unknown reply id {}", other),
        }),
    }
}

// =========================================================================
// Shared helpers
// =========================================================================

/// For each token+actual_amount pair this builds either attached native
/// `funds_to_pool` entries or CW20 `TransferFrom(user→manager) +
/// IncreaseAllowance(manager→pool, actual)` messages.
fn prepare_token_forward(
    token: &AssetInfo,
    amount: Uint128,
    info: &MessageInfo,
    pool_addr: &str,
    manager_addr: &str,
    funds_to_pool: &mut Vec<Coin>,
    pre_messages: &mut Vec<CosmosMsg>,
) -> Result<(), ContractError> {
    if amount.is_zero() {
        return Ok(());
    }
    match token {
        AssetInfo::NativeToken { denom } => {
            let sent = info
                .funds
                .iter()
                .find(|c| c.denom == *denom)
                .map(|c| c.amount)
                .unwrap_or_default();
            ensure!(
                sent >= amount,
                ContractError::Std(StdError::generic_err(format!(
                    "Insufficient funds for {}: need {}, sent {}",
                    denom, amount, sent
                )))
            );
            funds_to_pool.push(Coin {
                denom: denom.clone(),
                amount,
            });
        }
        AssetInfo::Token { .. } => {
            if let Some(msg) =
                token.transfer_from_msg(info.sender.as_ref(), manager_addr, amount)?
            {
                pre_messages.push(msg);
            }
            if let Some(msg) = token.increase_allowance_msg(pool_addr, amount)? {
                pre_messages.push(msg);
            }
        }
    }
    Ok(())
}

/// Compute native refunds: for every native token among `tokens`, subtract
/// `actual` from what `info.funds` supplied; residual is refundable.
///
/// CW20 tokens need no refund path — `TransferFrom(actual)` pulls only the
/// exact amount and the remaining user-granted allowance (if any) is outside
/// this contract's control.
fn compute_native_refunds(
    info: &MessageInfo,
    tokens: &[(&AssetInfo, Uint128)],
    refunds: &mut Vec<Coin>,
) -> Result<(), ContractError> {
    // Track per-denom required amount.
    use std::collections::BTreeMap;
    let mut required: BTreeMap<String, Uint128> = BTreeMap::new();
    for (token, amount) in tokens {
        if let AssetInfo::NativeToken { denom } = token {
            let entry = required.entry(denom.clone()).or_insert_with(Uint128::zero);
            *entry = entry.checked_add(*amount).map_err(|_| {
                StdError::generic_err(format!("required overflow for {}", denom))
            })?;
        }
    }
    for coin in &info.funds {
        let need = required.get(&coin.denom).copied().unwrap_or_else(Uint128::zero);
        if coin.amount > need {
            let refund = coin.amount.checked_sub(need).map_err(|_| {
                StdError::generic_err("refund subtraction underflow (unreachable)")
            })?;
            if !refund.is_zero() {
                refunds.push(Coin {
                    denom: coin.denom.clone(),
                    amount: refund,
                });
            }
        }
    }
    Ok(())
}

/// Compute the actuals `(amount0, amount1)` for a given liquidity at a given
/// sqrt price. Matches the pool's mint math exactly.
fn compute_token_amounts(
    sqrt_price_current: cosmwasm_std::Uint256,
    sqrt_price_lower: cosmwasm_std::Uint256,
    sqrt_price_upper: cosmwasm_std::Uint256,
    liquidity: u128,
    current_tick: i32,
    tick_lower: i32,
    tick_upper: i32,
) -> StdResult<(Uint128, Uint128)> {
    let (amount0, amount1) = if current_tick < tick_lower {
        (
            get_amount0_delta(sqrt_price_lower, sqrt_price_upper, liquidity, true)?,
            cosmwasm_std::Uint256::zero(),
        )
    } else if current_tick >= tick_upper {
        (
            cosmwasm_std::Uint256::zero(),
            get_amount1_delta(sqrt_price_lower, sqrt_price_upper, liquidity, true)?,
        )
    } else {
        (
            get_amount0_delta(sqrt_price_current, sqrt_price_upper, liquidity, true)?,
            get_amount1_delta(sqrt_price_lower, sqrt_price_current, liquidity, true)?,
        )
    };
    Ok((
        Uint128::try_from(amount0).unwrap_or(Uint128::MAX),
        Uint128::try_from(amount1).unwrap_or(Uint128::MAX),
    ))
}

/// Apply the V3 fee-accrual step to an NFT's state:
///   delta_N = current_fgi_N - state.fee_growth_inside_N_last
///   tokens_owed_N += state.liquidity * delta_N / 2^128
///   state.fee_growth_inside_N_last = current_fgi_N
///
/// Uses wrapping subtraction on Uint256 because fee_growth accumulators are
/// intentionally allowed to overflow (V3 convention).
fn accrue_fees_to_nft(
    state: &mut PositionState,
    current: &FeeGrowthInsideResponse,
) -> Result<(), ContractError> {
    if state.liquidity.is_zero() {
        // No fees owed — just snapshot current value.
        state.fee_growth_inside_0_last_x128 = current.fee_growth_inside_0_x128;
        state.fee_growth_inside_1_last_x128 = current.fee_growth_inside_1_x128;
        return Ok(());
    }

    let delta_0 = current
        .fee_growth_inside_0_x128
        .wrapping_sub(state.fee_growth_inside_0_last_x128);
    let delta_1 = current
        .fee_growth_inside_1_x128
        .wrapping_sub(state.fee_growth_inside_1_last_x128);

    let liquidity_u256 = Uint256::from(state.liquidity);
    let q128 = q128();
    let fees_0 = mul_div(liquidity_u256, delta_0, q128)?;
    let fees_1 = mul_div(liquidity_u256, delta_1, q128)?;

    // Saturate per the pool's own convention: a position accruing > u128 of
    // fees on one side should not brick itself. The small amount lost to
    // saturation is acceptable vs. permanent lockup.
    let fees_0_u128 = Uint128::try_from(fees_0).unwrap_or(Uint128::MAX);
    let fees_1_u128 = Uint128::try_from(fees_1).unwrap_or(Uint128::MAX);
    state.tokens_owed_0 = state
        .tokens_owed_0
        .checked_add(fees_0_u128)
        .unwrap_or(Uint128::MAX);
    state.tokens_owed_1 = state
        .tokens_owed_1
        .checked_add(fees_1_u128)
        .unwrap_or(Uint128::MAX);

    state.fee_growth_inside_0_last_x128 = current.fee_growth_inside_0_x128;
    state.fee_growth_inside_1_last_x128 = current.fee_growth_inside_1_x128;
    Ok(())
}

/// Extract two Uint128 attributes from the sub-message reply events, keyed by
/// `key0` and `key1`. Only events emitted by `emitter` (matched on the
/// `_contract_address` attribute) are considered — this prevents a malicious
/// CW20 in the submsg chain from forging attributes with these keys.
fn parse_amounts_from_reply(
    reply: &Reply,
    key0: &str,
    key1: &str,
    emitter: &str,
) -> Result<(Uint128, Uint128), ContractError> {
    let result = match &reply.result {
        cosmwasm_std::SubMsgResult::Ok(r) => r,
        cosmwasm_std::SubMsgResult::Err(e) => {
            return Err(ContractError::InvalidReply {
                reason: format!("pool submsg failed: {e}"),
            });
        }
    };

    let mut v0: Option<Uint128> = None;
    let mut v1: Option<Uint128> = None;

    for event in &result.events {
        // Only `wasm` (or `wasm-*`) events carry a `_contract_address`
        // attribute naming the emitting contract; other event types (e.g.
        // `execute`, `coin_received`) don't and are skipped.
        if !event.ty.starts_with("wasm") {
            continue;
        }
        let event_emitter = event
            .attributes
            .iter()
            .find(|a| a.key == "_contract_address")
            .map(|a| a.value.as_str());
        if event_emitter != Some(emitter) {
            continue;
        }
        for attr in &event.attributes {
            // Reject duplicate attribute keys from the trusted emitter. Pool
            // today emits each key exactly once; a future pool change that
            // emits multiple would otherwise silently bind the first value
            // and discard the rest, giving wrong amounts.
            if attr.key == key0 {
                if v0.is_some() {
                    return Err(ContractError::InvalidReply {
                        reason: format!("duplicate attribute {key0} from {emitter}"),
                    });
                }
                v0 = Some(Uint128::from_str_checked(&attr.value).map_err(|e| {
                    ContractError::InvalidReply {
                        reason: format!("parse {key0} = {} : {e}", attr.value),
                    }
                })?);
            } else if attr.key == key1 {
                if v1.is_some() {
                    return Err(ContractError::InvalidReply {
                        reason: format!("duplicate attribute {key1} from {emitter}"),
                    });
                }
                v1 = Some(Uint128::from_str_checked(&attr.value).map_err(|e| {
                    ContractError::InvalidReply {
                        reason: format!("parse {key1} = {} : {e}", attr.value),
                    }
                })?);
            }
        }
    }

    Ok((
        v0.ok_or_else(|| ContractError::InvalidReply {
            reason: format!("missing attribute {key0}"),
        })?,
        v1.ok_or_else(|| ContractError::InvalidReply {
            reason: format!("missing attribute {key1}"),
        })?,
    ))
}

trait Uint128FromStr {
    fn from_str_checked(s: &str) -> Result<Uint128, String>;
}

impl Uint128FromStr for Uint128 {
    fn from_str_checked(s: &str) -> Result<Uint128, String> {
        s.parse::<u128>()
            .map(Uint128::new)
            .map_err(|e| e.to_string())
    }
}

/// Extract post-update `fee_growth_inside` as Uint256 from a pool.Burn reply.
/// Used instead of `GetFeeGrowthInside` query because the burn may have cleared
/// the ticks (final-exit case), which would make a post-burn query read 0 from
/// now-default tick entries instead of the value the pool actually used when
/// crediting fees.
fn parse_fg_inside_from_reply(
    reply: &Reply,
    emitter: &str,
) -> Result<(Uint256, Uint256), ContractError> {
    let result = match &reply.result {
        cosmwasm_std::SubMsgResult::Ok(r) => r,
        cosmwasm_std::SubMsgResult::Err(e) => {
            return Err(ContractError::InvalidReply {
                reason: format!("pool submsg failed: {e}"),
            });
        }
    };

    let key0 = "fee_growth_inside_0_x128";
    let key1 = "fee_growth_inside_1_x128";
    let mut v0: Option<Uint256> = None;
    let mut v1: Option<Uint256> = None;

    for event in &result.events {
        if !event.ty.starts_with("wasm") {
            continue;
        }
        let event_emitter = event
            .attributes
            .iter()
            .find(|a| a.key == "_contract_address")
            .map(|a| a.value.as_str());
        if event_emitter != Some(emitter) {
            continue;
        }
        for attr in &event.attributes {
            if attr.key == key0 {
                if v0.is_some() {
                    return Err(ContractError::InvalidReply {
                        reason: format!("duplicate attribute {key0} from {emitter}"),
                    });
                }
                v0 = Some(Uint256::from_str(&attr.value).map_err(|e| {
                    ContractError::InvalidReply {
                        reason: format!("parse {key0} = {} : {e}", attr.value),
                    }
                })?);
            } else if attr.key == key1 {
                if v1.is_some() {
                    return Err(ContractError::InvalidReply {
                        reason: format!("duplicate attribute {key1} from {emitter}"),
                    });
                }
                v1 = Some(Uint256::from_str(&attr.value).map_err(|e| {
                    ContractError::InvalidReply {
                        reason: format!("parse {key1} = {} : {e}", attr.value),
                    }
                })?);
            }
        }
    }

    Ok((
        v0.ok_or_else(|| ContractError::InvalidReply {
            reason: format!("missing attribute {key0}"),
        })?,
        v1.ok_or_else(|| ContractError::InvalidReply {
            reason: format!("missing attribute {key1}"),
        })?,
    ))
}

// =========================================================================
// Migrate
// =========================================================================

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

// =========================================================================
// Query
// =========================================================================

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    let base_contract = Cw721MetadataContract::default();

    match msg {
        QueryMsg::Position { token_id } => {
            let state = POSITIONS.load(deps.storage, &token_id)?;
            Ok(to_json_binary(&state.metadata())?)
        }

        QueryMsg::OwnerOf {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::OwnerOf {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Approval {
            token_id,
            spender,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Approval {
                token_id,
                spender,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Approvals {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Approvals {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::NumTokens {} => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::NumTokens {};
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::NftInfo { token_id } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::NftInfo { token_id };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::AllNftInfo {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::AllNftInfo {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Tokens {
            owner,
            start_after,
            limit,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Tokens {
                owner,
                start_after,
                limit,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::PositionWithFees { token_id } => {
            let state = POSITIONS.load(deps.storage, &token_id)?;

            // Pending fees: compute the delta `(current_fgi - last_fgi) * L / Q128`
            // but DO NOT mutate storage; queries are read-only. Added to
            // `tokens_owed` already recorded on the NFT.
            let pending = if state.liquidity.is_zero() {
                (Uint128::zero(), Uint128::zero())
            } else {
                let fg: FeeGrowthInsideResponse = deps.querier.query_wasm_smart(
                    state.pool_address.to_string(),
                    &PoolQueryMsg::GetFeeGrowthInside {
                        tick_lower: state.tick_lower,
                        tick_upper: state.tick_upper,
                    },
                )?;
                let delta_0 = fg
                    .fee_growth_inside_0_x128
                    .wrapping_sub(state.fee_growth_inside_0_last_x128);
                let delta_1 = fg
                    .fee_growth_inside_1_x128
                    .wrapping_sub(state.fee_growth_inside_1_last_x128);
                let l = Uint256::from(state.liquidity);
                let q = q128();
                let f0 = mul_div(l, delta_0, q)?;
                let f1 = mul_div(l, delta_1, q)?;
                (
                    Uint128::try_from(f0).unwrap_or(Uint128::MAX),
                    Uint128::try_from(f1).unwrap_or(Uint128::MAX),
                )
            };

            let tokens_owed_0 = state
                .tokens_owed_0
                .checked_add(pending.0)
                .unwrap_or(Uint128::MAX);
            let tokens_owed_1 = state
                .tokens_owed_1
                .checked_add(pending.1)
                .unwrap_or(Uint128::MAX);

            Ok(to_json_binary(&PositionWithFeesResponse {
                position: state.metadata(),
                liquidity: state.liquidity,
                tokens_owed_0,
                tokens_owed_1,
            })?)
        }
    }
}

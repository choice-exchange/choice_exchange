use std::convert::TryFrom;

use cosmwasm_std::{
    ensure, Addr, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Response, StdError, Storage,
    Uint128, Uint256,
};

use crate::core::bitmap::next_initialized_tick_in_chunk;
use crate::core::oracle::{get_dynamic_fee, update_oracle};
use crate::error::ContractError;
use crate::state::{
    PoolConfig, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POOL_CONFIG, POOL_STATE, TICKS,
};

use choice_clmm_common::pool::{QuoteResponse, TickInfo};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::full_math::mul_div;
use choice_clmm_math::swap_math::compute_swap_step;
use choice_clmm_math::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};

/// Result of the pure swap computation (no state writes).
pub struct SwapComputation {
    pub amount_in: Uint128,
    pub amount_out: Uint128,
    pub fee_amount: Uint128,
    pub sqrt_price_after: Uint256,
    pub tick_after: i32,
    pub liquidity_after: u128,
    pub fee_growth_global_0: Uint256,
    pub fee_growth_global_1: Uint256,
    /// Ticks that were crossed and need fee_growth_outside updated.
    pub tick_updates: Vec<(i32, TickInfo)>,
}

/// Core swap loop extracted as a read-only computation.
/// Reads ticks from storage but never writes. Returns all state mutations
/// so callers can choose whether to apply them.
#[allow(clippy::too_many_arguments)]
pub fn compute_swap(
    storage: &dyn Storage,
    sqrt_price: Uint256,
    tick: i32,
    liquidity: u128,
    fee_growth_global_0: Uint256,
    fee_growth_global_1: Uint256,
    tick_spacing: i32,
    zero_for_one: bool,
    amount_specified: Uint128,
    sqrt_price_limit_x96: Uint256,
    fee_pips: u32,
) -> Result<SwapComputation, ContractError> {
    let mut state_amount_remaining = Uint256::from(amount_specified);
    let mut state_amount_calculated = Uint256::zero();
    let mut state_fee_total = Uint256::zero();
    let mut state_sqrt_price = sqrt_price;
    let mut state_tick = tick;
    let mut state_liquidity = liquidity;
    let mut state_fg0 = fee_growth_global_0;
    let mut state_fg1 = fee_growth_global_1;
    let mut tick_updates: Vec<(i32, TickInfo)> = Vec::new();

    while !state_amount_remaining.is_zero() && state_sqrt_price != sqrt_price_limit_x96 {
        let (mut step_tick_next, step_initialized) =
            next_initialized_tick_in_chunk(storage, state_tick, tick_spacing, zero_for_one)?;

        step_tick_next = step_tick_next.clamp(MIN_TICK, MAX_TICK);
        let step_sqrt_price_next = get_sqrt_ratio_at_tick(step_tick_next)?;

        let mut target_price = step_sqrt_price_next;
        let mut reached_limit = false;

        if zero_for_one {
            if step_sqrt_price_next < sqrt_price_limit_x96 {
                target_price = sqrt_price_limit_x96;
                reached_limit = true;
            }
        } else if step_sqrt_price_next > sqrt_price_limit_x96 {
            target_price = sqrt_price_limit_x96;
            reached_limit = true;
        }

        let step = compute_swap_step(
            state_sqrt_price,
            target_price,
            state_liquidity,
            state_amount_remaining,
            fee_pips,
            zero_for_one,
        )?;

        if state_liquidity > 0 && !step.fee_amount.is_zero() {
            let q128 = Uint256::one() << 128u32;
            let fee_growth_delta = mul_div(step.fee_amount, q128, Uint256::from(state_liquidity));

            if zero_for_one {
                state_fg0 = state_fg0.wrapping_add(fee_growth_delta);
            } else {
                state_fg1 = state_fg1.wrapping_add(fee_growth_delta);
            }
        }

        state_fee_total += step.fee_amount;
        state_amount_remaining -= step.amount_in + step.fee_amount;
        state_amount_calculated += step.amount_out;
        state_sqrt_price = step.sqrt_ratio_next_x96;

        if state_sqrt_price == step_sqrt_price_next && !reached_limit {
            if step_initialized {
                let mut tick_info = TICKS.load(storage, step_tick_next)?;
                let liquidity_delta = tick_info.liquidity_delta;

                tick_info.fee_growth_outside_0 =
                    state_fg0.wrapping_sub(tick_info.fee_growth_outside_0);
                tick_info.fee_growth_outside_1 =
                    state_fg1.wrapping_sub(tick_info.fee_growth_outside_1);

                tick_updates.push((step_tick_next, tick_info));

                if zero_for_one {
                    if liquidity_delta >= 0 {
                        let net_abs = liquidity_delta as u128;
                        state_liquidity =
                            state_liquidity.checked_sub(net_abs).ok_or_else(|| {
                                ContractError::Std(StdError::generic_err(format!(
                                    "L underflow: State={} Net={}",
                                    state_liquidity, net_abs
                                )))
                            })?;
                    } else {
                        state_liquidity = state_liquidity
                            .checked_add(liquidity_delta.unsigned_abs())
                            .ok_or(ContractError::Std(StdError::generic_err("L overflow")))?;
                    }
                } else if liquidity_delta >= 0 {
                    state_liquidity = state_liquidity
                        .checked_add(liquidity_delta as u128)
                        .ok_or(ContractError::Std(StdError::generic_err("L overflow")))?;
                } else {
                    state_liquidity = state_liquidity
                        .checked_sub(liquidity_delta.unsigned_abs())
                        .ok_or(ContractError::Std(StdError::generic_err(format!(
                            "L underflow (up): State={} Net={}",
                            state_liquidity,
                            liquidity_delta.unsigned_abs()
                        ))))?;
                }
            }

            if zero_for_one {
                state_tick = step_tick_next - 1;
            } else {
                state_tick = step_tick_next;
            }
        } else if state_sqrt_price != step_sqrt_price_next {
            state_tick = get_tick_at_sqrt_ratio(state_sqrt_price)?;
        }
    }

    let amount_in = amount_specified - Uint128::try_from(state_amount_remaining).unwrap();
    let amount_out = Uint128::try_from(state_amount_calculated).unwrap();
    let fee_amount = Uint128::try_from(state_fee_total).unwrap();

    Ok(SwapComputation {
        amount_in,
        amount_out,
        fee_amount,
        sqrt_price_after: state_sqrt_price,
        tick_after: state_tick,
        liquidity_after: state_liquidity,
        fee_growth_global_0: state_fg0,
        fee_growth_global_1: state_fg1,
        tick_updates,
    })
}

/// Apply a swap computation's side effects to storage and build transfer messages.
/// `sender` is the user who initiated the swap (for refunds/TransferFrom).
/// `tokens_already_in_pool` indicates CW20 tokens were already sent via Receive hook.
#[allow(clippy::too_many_arguments)]
fn apply_swap(
    deps: DepsMut,
    env: &Env,
    sender: &Addr,
    info_funds: &[Coin],
    config: &PoolConfig,
    zero_for_one: bool,
    recipient: &str,
    result: &SwapComputation,
    tokens_already_in_pool: bool,
) -> Result<Response, ContractError> {
    // Save tick updates
    for (tick, tick_info) in &result.tick_updates {
        TICKS.save(deps.storage, *tick, tick_info)?;
    }

    // Save global state
    POOL_STATE.save(
        deps.storage,
        &choice_clmm_common::pool::PoolState {
            sqrt_price: result.sqrt_price_after,
            tick: result.tick_after,
            liquidity: Uint128::from(result.liquidity_after),
        },
    )?;
    FEE_GROWTH_GLOBAL_0.save(deps.storage, &result.fee_growth_global_0)?;
    FEE_GROWTH_GLOBAL_1.save(deps.storage, &result.fee_growth_global_1)?;

    let mut messages: Vec<CosmosMsg> = vec![];

    let in_token = if zero_for_one {
        &config.token0
    } else {
        &config.token1
    };
    let out_token = if zero_for_one {
        &config.token1
    } else {
        &config.token0
    };

    // Handle input: verify funds / pull CW20 / skip if already in pool
    if !tokens_already_in_pool {
        match in_token {
            AssetInfo::NativeToken { denom } => {
                let sent = info_funds
                    .iter()
                    .find(|c| c.denom == *denom)
                    .map(|c| c.amount)
                    .unwrap_or_default();

                if sent > result.amount_in {
                    let refund = sent - result.amount_in;
                    messages.push(in_token.transfer_msg(sender.as_ref(), refund)?);
                } else {
                    ensure!(
                        sent == result.amount_in,
                        ContractError::Std(StdError::generic_err("Insufficient funds"))
                    );
                }
            }
            AssetInfo::Token { .. } => {
                // Pull exact amount via TransferFrom (caller must have approved pool)
                if let Some(msg) = in_token.transfer_from_msg(
                    sender.as_ref(),
                    env.contract.address.as_ref(),
                    result.amount_in,
                )? {
                    messages.push(msg);
                }
            }
        }
    }

    // Handle output
    if !result.amount_out.is_zero() {
        messages.push(out_token.transfer_msg(recipient, result.amount_out)?);
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "swap")
        .add_attribute("amount_in", result.amount_in)
        .add_attribute("amount_out", result.amount_out)
        .add_attribute("final_price", result.sqrt_price_after.to_string())
        .add_attribute("final_tick", result.tick_after.to_string()))
}

/// Low-level swap (original interface). Supports both native (via info.funds) and CW20 (via allowance).
pub fn execute_swap(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    recipient: String,
    zero_for_one: bool,
    amount_specified: Uint128,
    sqrt_price_limit_x96: Uint256,
) -> Result<Response, ContractError> {
    if amount_specified.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }

    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    // Validation
    if zero_for_one {
        if sqrt_price_limit_x96 >= slot0.sqrt_price
            || sqrt_price_limit_x96 < Uint256::from(MIN_SQRT_RATIO)
        {
            return Err(ContractError::Std(StdError::generic_err(
                "Invalid price limit for selling Token0",
            )));
        }
    } else if sqrt_price_limit_x96 <= slot0.sqrt_price || sqrt_price_limit_x96 >= max_sqrt_ratio() {
        return Err(ContractError::Std(StdError::generic_err(
            "Invalid price limit for selling Token1",
        )));
    }

    // Oracle & Dynamic Fee
    update_oracle(deps.storage, &env, slot0.sqrt_price)?;
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price)?;

    let result = compute_swap(
        deps.storage,
        slot0.sqrt_price,
        slot0.tick,
        slot0.liquidity.u128(),
        fg0,
        fg1,
        config.tick_spacing as i32,
        zero_for_one,
        amount_specified,
        sqrt_price_limit_x96,
        fee_pips,
    )?;

    let sender = info.sender.clone();
    let funds = info.funds.clone();
    apply_swap(
        deps,
        &env,
        &sender,
        &funds,
        &config,
        zero_for_one,
        &recipient,
        &result,
        false,
    )
}

/// User-friendly exact-input swap. Direction inferred from attached native funds.
pub fn execute_swap_exact_input(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    minimum_amount_out: Uint128,
    recipient: Option<String>,
    deadline: Option<u64>,
) -> Result<Response, ContractError> {
    // Deadline check
    if let Some(deadline) = deadline {
        if env.block.time.seconds() > deadline {
            return Err(ContractError::DeadlineExceeded {});
        }
    }

    let config = POOL_CONFIG.load(deps.storage)?;

    // Determine direction from native funds
    let (zero_for_one, amount_specified) = resolve_direction_native(&info, &config)?;

    // Use full price range as limit
    let sqrt_price_limit_x96 = if zero_for_one {
        Uint256::from(MIN_SQRT_RATIO) + Uint256::one()
    } else {
        max_sqrt_ratio() - Uint256::one()
    };

    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    // Oracle & Dynamic Fee
    update_oracle(deps.storage, &env, slot0.sqrt_price)?;
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price)?;

    let result = compute_swap(
        deps.storage,
        slot0.sqrt_price,
        slot0.tick,
        slot0.liquidity.u128(),
        fg0,
        fg1,
        config.tick_spacing as i32,
        zero_for_one,
        amount_specified,
        sqrt_price_limit_x96,
        fee_pips,
    )?;

    // Slippage check
    if result.amount_out < minimum_amount_out {
        return Err(ContractError::InsufficientOutput {
            minimum: minimum_amount_out.to_string(),
            actual: result.amount_out.to_string(),
        });
    }

    let recipient = recipient.unwrap_or_else(|| info.sender.to_string());
    let sender = info.sender.clone();
    let funds = info.funds.clone();

    apply_swap(
        deps,
        &env,
        &sender,
        &funds,
        &config,
        zero_for_one,
        &recipient,
        &result,
        false,
    )
}

/// CW20 exact-input swap via Receive hook. Tokens already in pool.
#[allow(clippy::too_many_arguments)]
pub fn execute_swap_exact_input_cw20(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    amount: Uint128,
    zero_for_one: bool,
    minimum_amount_out: Uint128,
    recipient: Option<String>,
    deadline: Option<u64>,
) -> Result<Response, ContractError> {
    if amount.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }

    // Deadline check
    if let Some(deadline) = deadline {
        if env.block.time.seconds() > deadline {
            return Err(ContractError::DeadlineExceeded {});
        }
    }

    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    // Use full price range as limit
    let sqrt_price_limit_x96 = if zero_for_one {
        Uint256::from(MIN_SQRT_RATIO) + Uint256::one()
    } else {
        max_sqrt_ratio() - Uint256::one()
    };

    // Oracle & Dynamic Fee
    update_oracle(deps.storage, &env, slot0.sqrt_price)?;
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price)?;

    let result = compute_swap(
        deps.storage,
        slot0.sqrt_price,
        slot0.tick,
        slot0.liquidity.u128(),
        fg0,
        fg1,
        config.tick_spacing as i32,
        zero_for_one,
        amount,
        sqrt_price_limit_x96,
        fee_pips,
    )?;

    // Slippage check
    if result.amount_out < minimum_amount_out {
        return Err(ContractError::InsufficientOutput {
            minimum: minimum_amount_out.to_string(),
            actual: result.amount_out.to_string(),
        });
    }

    let recipient = recipient.unwrap_or_else(|| sender.to_string());

    apply_swap(
        deps,
        &env,
        &sender,
        &[],
        &config,
        zero_for_one,
        &recipient,
        &result,
        true, // tokens already in pool from CW20 Send
    )
}

/// Read-only swap simulation for quoting.
pub fn query_quote(
    deps: Deps,
    env: Env,
    token_in: AssetInfo,
    amount_in: Uint128,
) -> Result<QuoteResponse, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    let zero_for_one = if token_in == config.token0 {
        true
    } else if token_in == config.token1 {
        false
    } else {
        return Err(ContractError::InvalidFunds {
            reason: format!("token_in '{}' is not a pool token", token_in),
        });
    };

    let sqrt_price_limit_x96 = if zero_for_one {
        Uint256::from(MIN_SQRT_RATIO) + Uint256::one()
    } else {
        max_sqrt_ratio() - Uint256::one()
    };

    // Use current oracle state without updating (read-only)
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price)?;

    let result = compute_swap(
        deps.storage,
        slot0.sqrt_price,
        slot0.tick,
        slot0.liquidity.u128(),
        fg0,
        fg1,
        config.tick_spacing as i32,
        zero_for_one,
        amount_in,
        sqrt_price_limit_x96,
        fee_pips,
    )?;

    Ok(QuoteResponse {
        amount_out: result.amount_out,
        amount_in_consumed: result.amount_in,
        fee_amount: result.fee_amount,
    })
}

/// Determine swap direction and amount from attached native funds.
fn resolve_direction_native(
    info: &MessageInfo,
    config: &PoolConfig,
) -> Result<(bool, Uint128), ContractError> {
    // Get native denoms for pool tokens (CW20 tokens won't match any coin denom)
    let token0_denom = match &config.token0 {
        AssetInfo::NativeToken { denom } => Some(denom.as_str()),
        _ => None,
    };
    let token1_denom = match &config.token1 {
        AssetInfo::NativeToken { denom } => Some(denom.as_str()),
        _ => None,
    };

    let relevant: Vec<&Coin> = info
        .funds
        .iter()
        .filter(|c| {
            (token0_denom.is_some_and(|d| c.denom == d)
                || token1_denom.is_some_and(|d| c.denom == d))
                && !c.amount.is_zero()
        })
        .collect();

    if relevant.len() != 1 {
        return Err(ContractError::InvalidFunds {
            reason: "must send exactly one native pool token".to_string(),
        });
    }

    let coin = &relevant[0];
    let zero_for_one = token0_denom.is_some_and(|d| coin.denom == d);
    Ok((zero_for_one, coin.amount))
}

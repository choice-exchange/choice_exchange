use std::convert::TryFrom;

use cosmwasm_std::{
    ensure, Addr, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Response, StdError, Storage,
    Uint128, Uint256,
};

use crate::core::bitmap::next_initialized_tick_in_chunk;
use crate::core::oracle::{get_dynamic_fee, update_oracle_and_fee};
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
            let fee_growth_delta =
                mul_div(step.fee_amount, q128, Uint256::from(state_liquidity))?;

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

                // Tick crossing: apply `liquidity_delta`. Direction of the
                // swap flips the sign per V3 convention — zero_for_one moving
                // downward crosses a tick and *removes* its upper-edge
                // contribution (or adds its lower-edge contribution).
                //
                // Use `unsigned_abs()` uniformly so a misparsed `i128 as u128`
                // cast can never silently wrap a stored-negative value.
                let delta_abs = liquidity_delta.unsigned_abs();
                let add_liquidity = if zero_for_one {
                    liquidity_delta < 0
                } else {
                    liquidity_delta >= 0
                };
                state_liquidity = if add_liquidity {
                    state_liquidity
                        .checked_add(delta_abs)
                        .ok_or(ContractError::Std(StdError::generic_err("L overflow")))?
                } else {
                    state_liquidity.checked_sub(delta_abs).ok_or_else(|| {
                        ContractError::Std(StdError::generic_err(format!(
                            "L underflow: State={} Net={}",
                            state_liquidity, delta_abs
                        )))
                    })?
                };
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

    // Conversions here cannot overflow in practice (amounts are bounded by
    // input `amount_specified: Uint128`), but we surface typed errors instead
    // of `.unwrap()` so the swap cannot panic on any input.
    let consumed_remaining = Uint128::try_from(state_amount_remaining)
        .map_err(|_| StdError::generic_err("swap: remaining amount exceeds u128"))?;
    let amount_in = amount_specified.checked_sub(consumed_remaining).map_err(|_| {
        StdError::generic_err("swap: remaining exceeds specified amount (invariant broken)")
    })?;
    let amount_out = Uint128::try_from(state_amount_calculated)
        .map_err(|_| StdError::generic_err("swap: amount_out exceeds u128"))?;
    let fee_amount = Uint128::try_from(state_fee_total)
        .map_err(|_| StdError::generic_err("swap: fee_amount exceeds u128"))?;

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

/// How the caller delivered input to the pool.
enum SwapInputSource<'a> {
    /// Native coins attached to the message. Any surplus over `amount_in` is
    /// refunded to `sender`.
    Native(&'a [Coin]),
    /// CW20 tokens already transferred to the pool via `Cw20ExecuteMsg::Send`
    /// (Receive hook). `total_sent` is the full amount that arrived; if the
    /// swap consumes less (partial fill), the delta is refunded via `Transfer`.
    Cw20AlreadySent { total_sent: Uint128 },
    /// CW20 via allowance — pool pulls exactly `amount_in` via `TransferFrom`.
    Cw20Allowance,
}

/// Apply a swap computation's side effects to storage and build transfer messages.
///
/// Input handling: see `SwapInputSource`. Output is always a single transfer
/// of `amount_out` to `recipient`.
#[allow(clippy::too_many_arguments)]
fn apply_swap(
    deps: DepsMut,
    env: &Env,
    sender: &Addr,
    input_source: SwapInputSource,
    config: &PoolConfig,
    zero_for_one: bool,
    recipient: &str,
    result: &SwapComputation,
    fee_pips: u32,
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

    // Handle input — reconcile what the caller delivered vs. what the swap consumed.
    match input_source {
        SwapInputSource::Native(info_funds) => {
            let denom = match in_token {
                AssetInfo::NativeToken { denom } => denom.clone(),
                AssetInfo::Token { .. } => {
                    return Err(ContractError::Std(StdError::generic_err(
                        "native input source used with CW20 in-token",
                    )));
                }
            };
            let sent = info_funds
                .iter()
                .find(|c| c.denom == denom)
                .map(|c| c.amount)
                .unwrap_or_default();

            if sent > result.amount_in {
                let refund = sent.checked_sub(result.amount_in).map_err(|_| {
                    StdError::generic_err("refund underflow (invariant broken)")
                })?;
                messages.push(in_token.transfer_msg(sender.as_ref(), refund)?);
            } else {
                ensure!(
                    sent == result.amount_in,
                    ContractError::Std(StdError::generic_err("Insufficient funds"))
                );
            }
        }
        SwapInputSource::Cw20AlreadySent { total_sent } => {
            // V3 parity: if the swap did not consume all attached input
            // (partial fill due to liquidity exhaustion or price limit),
            // return the unused CW20 amount to the original sender. Without
            // this, the delta is silently captured by the pool and can be
            // extracted by any later swap's rounding.
            if total_sent < result.amount_in {
                return Err(ContractError::Std(StdError::generic_err(
                    "CW20 receive: consumed more than sent (invariant broken)",
                )));
            }
            let refund = total_sent.checked_sub(result.amount_in).map_err(|_| {
                StdError::generic_err("CW20 refund underflow (invariant broken)")
            })?;
            if !refund.is_zero() {
                messages.push(in_token.transfer_msg(sender.as_ref(), refund)?);
            }
        }
        SwapInputSource::Cw20Allowance => {
            if let Some(msg) = in_token.transfer_from_msg(
                sender.as_ref(),
                env.contract.address.as_ref(),
                result.amount_in,
            )? {
                messages.push(msg);
            }
        }
    }

    // Handle output — always a single transfer to `recipient`.
    if !result.amount_out.is_zero() {
        messages.push(out_token.transfer_msg(recipient, result.amount_out)?);
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "swap")
        .add_attribute("amount_in", result.amount_in)
        .add_attribute("amount_out", result.amount_out)
        .add_attribute("fee_amount", result.fee_amount)
        .add_attribute("fee_ppm", fee_pips.to_string())
        .add_attribute("zero_for_one", zero_for_one.to_string())
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
    // Single-call oracle update: blends EMA, computes raw fee, clamps per-block
    // change, and persists. Returns the rate-limited ppm to use for this swap.
    let fee_pips = update_oracle_and_fee(deps.storage, &env, slot0.sqrt_price)?;

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
    // execute_swap accepts both native-with-funds and CW20-with-allowance. Pick
    // the right source based on the in-token type.
    let in_token = if zero_for_one {
        &config.token0
    } else {
        &config.token1
    };
    let input_source = match in_token {
        AssetInfo::NativeToken { .. } => SwapInputSource::Native(&funds),
        AssetInfo::Token { .. } => SwapInputSource::Cw20Allowance,
    };
    apply_swap(
        deps,
        &env,
        &sender,
        input_source,
        &config,
        zero_for_one,
        &recipient,
        &result,
        fee_pips,
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
    // Single-call oracle update: blends EMA, computes raw fee, clamps per-block
    // change, and persists. Returns the rate-limited ppm to use for this swap.
    let fee_pips = update_oracle_and_fee(deps.storage, &env, slot0.sqrt_price)?;

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

    // `execute_swap_exact_input` only supports native input (direction is
    // inferred from `info.funds`). CW20 callers use the Receive hook path.
    apply_swap(
        deps,
        &env,
        &sender,
        SwapInputSource::Native(&funds),
        &config,
        zero_for_one,
        &recipient,
        &result,
        fee_pips,
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
    // Single-call oracle update: blends EMA, computes raw fee, clamps per-block
    // change, and persists. Returns the rate-limited ppm to use for this swap.
    let fee_pips = update_oracle_and_fee(deps.storage, &env, slot0.sqrt_price)?;

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

    // Tokens already in pool from the CW20 Send. If the swap partially filled
    // (liquidity exhausted / price limit hit) and consumed less than `amount`,
    // `apply_swap` refunds the delta via Cw20 Transfer.
    apply_swap(
        deps,
        &env,
        &sender,
        SwapInputSource::Cw20AlreadySent { total_sent: amount },
        &config,
        zero_for_one,
        &recipient,
        &result,
        fee_pips,
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

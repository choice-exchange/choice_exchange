use std::convert::TryFrom;

use cosmwasm_std::{
    ensure, BankMsg, Coin, DepsMut, Env, MessageInfo, Response, StdError, Uint128, Uint256,
};

use crate::core::bitmap::next_initialized_tick_within_one_word;
use crate::core::oracle::{get_dynamic_fee, update_oracle};
use crate::error::ContractError;
use crate::state::{CONFIG, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, SLOT0, TICKS};

use choice_clmm_math::full_math::mul_div;
use choice_clmm_math::swap_math::compute_swap_step;
use choice_clmm_math::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};

pub fn execute_swap(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    recipient: String,
    zero_for_one: bool,
    amount_specified: Uint128,
    sqrt_price_limit_x96: Uint256,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let mut slot0 = SLOT0.load(deps.storage)?;

    let mut fee_growth_global_0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let mut fee_growth_global_1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    // 1. Validation
    if zero_for_one {
        if sqrt_price_limit_x96 >= slot0.sqrt_price_x96
            || sqrt_price_limit_x96 < Uint256::from(MIN_SQRT_RATIO)
        {
            return Err(ContractError::Std(StdError::generic_err(
                "Invalid price limit for selling Token0",
            )));
        }
    } else if sqrt_price_limit_x96 <= slot0.sqrt_price_x96
        || sqrt_price_limit_x96 >= max_sqrt_ratio()
    {
        return Err(ContractError::Std(StdError::generic_err(
            "Invalid price limit for selling Token1",
        )));
    }

    // 2. Oracle & Dynamic Fee
    update_oracle(deps.storage, &env, slot0.sqrt_price_x96)?;
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price_x96)?;

    // 3. Initialize Loop State
    let mut state_amount_remaining = Uint256::from(amount_specified);
    let mut state_amount_calculated = Uint256::zero();
    let mut state_sqrt_price = slot0.sqrt_price_x96;
    let mut state_tick = slot0.tick;
    // CRITICAL: Ensure we load u128 correctly
    let mut state_liquidity = slot0.liquidity.u128();

    // 4. The Swap Loop
    while !state_amount_remaining.is_zero() && state_sqrt_price != sqrt_price_limit_x96 {
        let (mut step_tick_next, step_initialized) = next_initialized_tick_within_one_word(
            deps.storage,
            state_tick,
            config.tick_spacing as i32,
            zero_for_one,
        )?;

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
                fee_growth_global_0 = fee_growth_global_0.wrapping_add(fee_growth_delta);
            } else {
                fee_growth_global_1 = fee_growth_global_1.wrapping_add(fee_growth_delta);
            }
        }

        state_amount_remaining -= step.amount_in + step.fee_amount;
        state_amount_calculated += step.amount_out;
        state_sqrt_price = step.sqrt_ratio_next_x96;

        if state_sqrt_price == step_sqrt_price_next && !reached_limit {
            if step_initialized {
                let mut tick_info = TICKS.load(deps.storage, step_tick_next)?;
                let liquidity_net = tick_info.liquidity_net;

                tick_info.fee_growth_outside_0 =
                    fee_growth_global_0.wrapping_sub(tick_info.fee_growth_outside_0);
                tick_info.fee_growth_outside_1 =
                    fee_growth_global_1.wrapping_sub(tick_info.fee_growth_outside_1);

                TICKS.save(deps.storage, step_tick_next, &tick_info)?;

                if zero_for_one {
                    // Moving Down
                    if liquidity_net >= 0 {
                        // Subtracting positive net liquidity
                        // NOTE: using saturating_sub for safety on exact matches,
                        // but checked_sub + error is better for detecting logic bugs.
                        let net_abs = liquidity_net as u128;
                        state_liquidity =
                            state_liquidity.checked_sub(net_abs).ok_or_else(|| {
                                ContractError::Std(StdError::generic_err(format!(
                                    "L underflow: State={} Net={}",
                                    state_liquidity, net_abs
                                )))
                            })?;
                    } else {
                        // Adding negative net liquidity (absolute value)
                        state_liquidity = state_liquidity
                            .checked_add(liquidity_net.unsigned_abs())
                            .ok_or(ContractError::Std(StdError::generic_err("L overflow")))?;
                    }
                } else {
                    // Moving Up
                    if liquidity_net >= 0 {
                        state_liquidity = state_liquidity
                            .checked_add(liquidity_net as u128)
                            .ok_or(ContractError::Std(StdError::generic_err("L overflow")))?;
                    } else {
                        state_liquidity = state_liquidity
                            .checked_sub(liquidity_net.unsigned_abs())
                            .ok_or(ContractError::Std(StdError::generic_err(format!(
                                "L underflow (up): State={} Net={}",
                                state_liquidity,
                                liquidity_net.unsigned_abs()
                            ))))?;
                    }
                }
            }

            if zero_for_one {
                state_tick = step_tick_next - 1;
            } else {
                state_tick = step_tick_next;
            }
        } else if state_sqrt_price != step_sqrt_price_next {
            // Re-calculate tick if we didn't reach the crossing
            state_tick = get_tick_at_sqrt_ratio(state_sqrt_price)?;
        }
    }

    // 5. Save Global State
    slot0.sqrt_price_x96 = state_sqrt_price;
    slot0.tick = state_tick;
    // CRITICAL: Ensure we save the modified state_liquidity back to storage
    slot0.liquidity = Uint128::from(state_liquidity);
    SLOT0.save(deps.storage, &slot0)?;

    FEE_GROWTH_GLOBAL_0.save(deps.storage, &fee_growth_global_0)?;
    FEE_GROWTH_GLOBAL_1.save(deps.storage, &fee_growth_global_1)?;

    // 6. Transfers
    let mut messages: Vec<BankMsg> = vec![];
    let amount_used = amount_specified - Uint128::try_from(state_amount_remaining).unwrap();
    let amount_out = Uint128::try_from(state_amount_calculated).unwrap();

    let in_denom = if zero_for_one {
        config.token0.clone()
    } else {
        config.token1.clone()
    };

    if !info.funds.is_empty() {
        let sent = info
            .funds
            .iter()
            .find(|c| c.denom == in_denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        // Calc Refund
        // Ensure we don't underflow if user sent exact amount
        if sent > amount_used {
            let refund = sent - amount_used;
            messages.push(BankMsg::Send {
                to_address: info.sender.to_string(),
                amount: vec![Coin {
                    denom: in_denom,
                    amount: refund,
                }],
            });
        } else {
            ensure!(
                sent == amount_used,
                ContractError::Std(StdError::generic_err("Insufficient funds"))
            );
        }
    }

    let out_denom = if zero_for_one {
        config.token1
    } else {
        config.token0
    };
    if !amount_out.is_zero() {
        messages.push(BankMsg::Send {
            to_address: recipient,
            amount: vec![Coin {
                denom: out_denom,
                amount: amount_out,
            }],
        });
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "swap")
        .add_attribute("amount_in", amount_used)
        .add_attribute("amount_out", amount_out)
        .add_attribute("final_price", state_sqrt_price.to_string())
        .add_attribute("final_tick", state_tick.to_string()))
}

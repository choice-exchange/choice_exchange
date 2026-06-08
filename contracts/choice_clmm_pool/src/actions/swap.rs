use std::convert::TryFrom;

use cosmwasm_std::{
    ensure, Addr, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Response, StdError, Storage,
    Uint128, Uint256,
};

use crate::core::bitmap::next_initialized_tick_in_chunk;
use crate::core::oracle::{get_dynamic_fee, update_oracle_and_fee};
use crate::error::ContractError;
use crate::state::{
    PoolConfig, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POOL_CONFIG, POOL_STATE,
    PROTOCOL_FEES_0, PROTOCOL_FEES_1, PROTOCOL_FEE_CONFIG, TICKS,
};

use choice_clmm_common::pool::{QuoteResponse, TickInfo};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::full_math::mul_div;
use choice_clmm_math::swap_math::{
    compute_swap_step, compute_swap_step_exact_out, SwapStepResult,
};
use choice_clmm_math::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};

/// Hard cap on the number of price-range steps a single swap may traverse.
///
/// Each loop iteration in `compute_swap` advances the price to either the next
/// initialized tick or a bitmap word boundary (one storage read + O(1) bit
/// math per step). With `tick_spacing == 1` the full tick range spans only
/// ~6,932 words, and any realistic swap crosses a handful to a few hundred
/// ticks, so this ceiling never bites legitimate use. It exists to bound the
/// work of a swap/quote against a pathological pool — e.g. one with liquidity
/// only at extreme ticks (or none) that would otherwise force a full-range
/// word-by-word walk on every read-only `Quote`. Exceeding it is a clean,
/// explicit error rather than an opaque out-of-gas.
pub const MAX_SWAP_ITERATIONS: u32 = 10_000;

/// Result of the pure swap computation (no state writes).
#[derive(Debug)]
pub struct SwapComputation {
    pub amount_in: Uint128,
    pub amount_out: Uint128,
    /// Total fee charged to the swapper (LP share + protocol share). The swapper
    /// pays this regardless of the protocol carve; the split is internal.
    pub fee_amount: Uint128,
    pub sqrt_price_after: Uint256,
    pub tick_after: i32,
    pub liquidity_after: u128,
    pub fee_growth_global_0: Uint256,
    pub fee_growth_global_1: Uint256,
    /// Protocol's carved share of the fee, by token. Accrued to `PROTOCOL_FEES_*`
    /// and excluded from `fee_growth_global_*` (so LPs never earn it).
    pub protocol_fee_0: Uint128,
    pub protocol_fee_1: Uint128,
    /// Ticks that were crossed and need fee_growth_outside updated.
    pub tick_updates: Vec<(i32, TickInfo)>,
}

/// Load the protocol-fee carve divisors, defaulting to `(0, 0)` (off) when no
/// protocol-fee config has been set.
fn load_protocol_fee_rates(storage: &dyn Storage) -> (u8, u8) {
    PROTOCOL_FEE_CONFIG
        .may_load(storage)
        .ok()
        .flatten()
        .map(|c| (c.fee_protocol_0, c.fee_protocol_1))
        .unwrap_or((0, 0))
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
    fee_protocol_0: u8,
    fee_protocol_1: u8,
    // `true`: `amount_specified` is the input budget (exact-input). `false`:
    // `amount_specified` is the desired output (exact-output); `amount_in` in the
    // result is then the gross input cost the swapper must supply.
    exact_input: bool,
) -> Result<SwapComputation, ContractError> {
    let mut state_amount_remaining = Uint256::from(amount_specified);
    let mut state_amount_calculated = Uint256::zero();
    let mut state_fee_total = Uint256::zero();
    let mut state_protocol_fee = Uint256::zero();
    let mut state_sqrt_price = sqrt_price;
    let mut state_tick = tick;
    let mut state_liquidity = liquidity;
    let mut state_fg0 = fee_growth_global_0;
    let mut state_fg1 = fee_growth_global_1;
    let mut tick_updates: Vec<(i32, TickInfo)> = Vec::new();
    let mut iterations: u32 = 0;

    // Fee is always charged in the input token, so a swap accrues protocol fee
    // to exactly one side: token0 when `zero_for_one`, else token1.
    let fee_protocol = if zero_for_one {
        fee_protocol_0
    } else {
        fee_protocol_1
    };

    while !state_amount_remaining.is_zero() && state_sqrt_price != sqrt_price_limit_x96 {
        // Bound total work: a pathological pool (sparse/zero liquidity across a
        // wide range) must not let a single swap/quote walk the bitmap without
        // limit. See `MAX_SWAP_ITERATIONS`.
        iterations += 1;
        if iterations > MAX_SWAP_ITERATIONS {
            return Err(ContractError::SwapIterationLimit { iterations });
        }

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

        let step = if exact_input {
            compute_swap_step(
                state_sqrt_price,
                target_price,
                state_liquidity,
                state_amount_remaining,
                fee_pips,
                zero_for_one,
            )?
        } else if state_liquidity == 0 || target_price == state_sqrt_price {
            // Exact-out: the step helper rejects zero liquidity and a degenerate
            // `target == current` (which occurs when the next initialized tick
            // sits exactly at the current price). Advance to the target for free
            // in both cases, mirroring how `compute_swap_step` handles them for
            // exact-input; the crossing logic below then advances the tick.
            SwapStepResult {
                sqrt_ratio_next_x96: target_price,
                amount_in: Uint256::zero(),
                amount_out: Uint256::zero(),
                fee_amount: Uint256::zero(),
            }
        } else {
            compute_swap_step_exact_out(
                state_sqrt_price,
                target_price,
                state_liquidity,
                state_amount_remaining,
                fee_pips,
                zero_for_one,
            )?
        };

        // Carve the protocol's share off the fee BEFORE it accrues to LPs.
        // `fee_protocol` is a divisor (v3 convention): protocol takes
        // `fee_amount / n` (floor, in the pool's favor), LPs get the remainder.
        // The swapper's cost is unchanged — only the split differs.
        let mut lp_fee = step.fee_amount;
        if fee_protocol != 0 {
            let protocol_delta = step.fee_amount / Uint256::from(fee_protocol);
            lp_fee = lp_fee.checked_sub(protocol_delta).map_err(|_| {
                StdError::generic_err("protocol fee exceeds step fee (invariant broken)")
            })?;
            state_protocol_fee += protocol_delta;
        }

        if state_liquidity > 0 && !lp_fee.is_zero() {
            let q128 = Uint256::one() << 128u32;
            let fee_growth_delta = mul_div(lp_fee, q128, Uint256::from(state_liquidity))?;

            if zero_for_one {
                state_fg0 = state_fg0.wrapping_add(fee_growth_delta);
            } else {
                state_fg1 = state_fg1.wrapping_add(fee_growth_delta);
            }
        }

        state_fee_total += step.fee_amount;
        if exact_input {
            // Remaining tracks the input budget; calculated accrues output.
            state_amount_remaining -= step.amount_in + step.fee_amount;
            state_amount_calculated += step.amount_out;
        } else {
            // Remaining tracks the output budget; calculated accrues the gross
            // input cost (in + fee), which is what the swapper must pay.
            state_amount_remaining -= step.amount_out;
            state_amount_calculated += step.amount_in + step.fee_amount;
        }
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
    let calculated = Uint128::try_from(state_amount_calculated)
        .map_err(|_| StdError::generic_err("swap: calculated amount exceeds u128"))?;
    // The amount actually drawn down from `amount_specified` (input budget for
    // exact-input, output budget for exact-output).
    let consumed = amount_specified.checked_sub(consumed_remaining).map_err(|_| {
        StdError::generic_err("swap: remaining exceeds specified amount (invariant broken)")
    })?;
    // Exact-input: gross input = consumed budget, output = calculated.
    // Exact-output: output delivered = consumed budget, gross input = calculated.
    let (amount_in, amount_out) = if exact_input {
        (consumed, calculated)
    } else {
        (calculated, consumed)
    };
    let fee_amount = Uint128::try_from(state_fee_total)
        .map_err(|_| StdError::generic_err("swap: fee_amount exceeds u128"))?;
    let protocol_fee = Uint128::try_from(state_protocol_fee)
        .map_err(|_| StdError::generic_err("swap: protocol_fee exceeds u128"))?;
    // Protocol fee accrues to the input-token side only.
    let (protocol_fee_0, protocol_fee_1) = if zero_for_one {
        (protocol_fee, Uint128::zero())
    } else {
        (Uint128::zero(), protocol_fee)
    };

    Ok(SwapComputation {
        amount_in,
        amount_out,
        fee_amount,
        sqrt_price_after: state_sqrt_price,
        tick_after: state_tick,
        liquidity_after: state_liquidity,
        fee_growth_global_0: state_fg0,
        fee_growth_global_1: state_fg1,
        protocol_fee_0,
        protocol_fee_1,
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
    /// `attached_native` is any native coin attached to the `Receive` envelope
    /// (normally empty); it is refunded in full to `sender` (the original user
    /// carried in the hook).
    Cw20AlreadySent {
        total_sent: Uint128,
        attached_native: &'a [Coin],
    },
    /// CW20 via allowance — pool pulls exactly `amount_in` via `TransferFrom`.
    /// `attached_native` is any native coin the caller wrongly attached to the
    /// (payable-classified) swap message; the in-token is a CW20 so none of it
    /// is consumed and it is refunded in full to `sender`.
    Cw20Allowance { attached_native: &'a [Coin] },
}

/// Append a `BankMsg::Send` refunding every (non-zero) attached native coin
/// back to `recipient`. No-op when nothing was attached. Used by the CW20-input
/// branches: a CW20 swap consumes no native coins, so any attached funds must be
/// returned rather than silently absorbed into reserves (C-L1).
fn push_native_refund(messages: &mut Vec<CosmosMsg>, recipient: &Addr, funds: &[Coin]) {
    let refunds: Vec<Coin> = funds.iter().filter(|c| !c.amount.is_zero()).cloned().collect();
    if !refunds.is_empty() {
        messages.push(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: recipient.to_string(),
            amount: refunds,
        }));
    }
}

/// Apply a swap computation's side effects to storage and build transfer messages.
///
/// Input handling: see `SwapInputSource`. Output is always a single transfer
/// of `amount_out` to `recipient`.
///
/// RESERVED HOOK CALL SITES (see `PoolConfig.hook` — not implemented yet):
/// - A `HOOK_BEFORE_SWAP` hook would be invoked in each swap entrypoint *before*
///   `compute_swap` runs (so it could adjust the dynamic fee or short-circuit).
/// - A `HOOK_AFTER_SWAP` hook would be invoked here, *after* state is persisted
///   and the in/out transfer messages are built (appended as a submessage so the
///   hook observes the final `SwapComputation`). Mirror this pattern for
///   mint/burn (`HOOK_BEFORE_MINT`/`HOOK_AFTER_MINT`/...). Because CosmWasm hooks
///   are cross-contract calls, any hook that re-enters the pool must respect the
///   `REENTRANCY_LOCK` (see Phase 2).
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
    // Validate the output recipient up front (parity with flash/collect). The
    // low-level `Swap` entrypoint forwards a caller-supplied `recipient` string
    // straight into the output transfer; an invalid bech32 would otherwise only
    // surface deep inside the BankMsg/Cw20 transfer as an opaque error. Fail fast
    // with a clear one. (Sender-defaulted recipients are already valid, so this
    // is a no-op for them.)
    deps.api.addr_validate(recipient)?;

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

    // Accrue carved protocol fees. These stay in the pool's balance (the swapper
    // already paid them in as part of `amount_in`) but are tracked separately so
    // they are excluded from LP withdrawals and swept via CollectProtocol.
    if !result.protocol_fee_0.is_zero() {
        let cur = PROTOCOL_FEES_0.may_load(deps.storage)?.unwrap_or_default();
        let next = cur
            .checked_add(result.protocol_fee_0)
            .map_err(|e| StdError::generic_err(e.to_string()))?;
        PROTOCOL_FEES_0.save(deps.storage, &next)?;
    }
    if !result.protocol_fee_1.is_zero() {
        let cur = PROTOCOL_FEES_1.may_load(deps.storage)?.unwrap_or_default();
        let next = cur
            .checked_add(result.protocol_fee_1)
            .map_err(|e| StdError::generic_err(e.to_string()))?;
        PROTOCOL_FEES_1.save(deps.storage, &next)?;
    }

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

            // The input denom must cover the swap cost; surplus is refunded.
            ensure!(
                sent >= result.amount_in,
                ContractError::Std(StdError::generic_err("Insufficient funds"))
            );

            // Refund the surplus of the input denom AND the full amount of every
            // other denom attached to the message. Parity with the mint path
            // (`compute_native_refunds`): without refunding non-input coins, any
            // extra denom sent alongside — including the pool's *other* token —
            // is silently absorbed into reserves and stranded forever.
            let mut refunds: Vec<Coin> = vec![];
            for coin in info_funds {
                let refund = if coin.denom == denom {
                    coin.amount.checked_sub(result.amount_in).map_err(|_| {
                        StdError::generic_err("refund underflow (invariant broken)")
                    })?
                } else {
                    coin.amount
                };
                if !refund.is_zero() {
                    refunds.push(Coin {
                        denom: coin.denom.clone(),
                        amount: refund,
                    });
                }
            }
            if !refunds.is_empty() {
                messages.push(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                    to_address: sender.to_string(),
                    amount: refunds,
                }));
            }
        }
        SwapInputSource::Cw20AlreadySent {
            total_sent,
            attached_native,
        } => {
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
            // The swap consumes only the CW20 input; refund any native coins
            // attached to the `Receive` envelope back to the original user.
            push_native_refund(&mut messages, sender, attached_native);
        }
        SwapInputSource::Cw20Allowance { attached_native } => {
            if let Some(msg) = in_token.transfer_from_msg(
                sender.as_ref(),
                env.contract.address.as_ref(),
                result.amount_in,
            )? {
                messages.push(msg);
            }
            // The in-token is a CW20 (pulled via TransferFrom); the swap never
            // consumes native coins, so refund the FULL `info.funds` the caller
            // wrongly attached to this payable-classified swap message. Without
            // this, those coins are silently absorbed into reserves (C-L1).
            push_native_refund(&mut messages, sender, attached_native);
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
        .add_attribute(
            "protocol_fee",
            (result.protocol_fee_0 + result.protocol_fee_1).to_string(),
        )
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
    let (fee_protocol_0, fee_protocol_1) = load_protocol_fee_rates(deps.storage);

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
        fee_protocol_0,
        fee_protocol_1,
        true,
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
        AssetInfo::Token { .. } => SwapInputSource::Cw20Allowance {
            attached_native: &funds,
        },
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
    let (fee_protocol_0, fee_protocol_1) = load_protocol_fee_rates(deps.storage);

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
        fee_protocol_0,
        fee_protocol_1,
        true,
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
    // Native coins attached to the `Receive` envelope (normally empty); refunded
    // in full to the original user (`sender`) since the CW20 swap consumes none.
    attached_native: Vec<Coin>,
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
    let (fee_protocol_0, fee_protocol_1) = load_protocol_fee_rates(deps.storage);

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
        fee_protocol_0,
        fee_protocol_1,
        true,
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
        SwapInputSource::Cw20AlreadySent {
            total_sent: amount,
            attached_native: &attached_native,
        },
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

    // The protocol carve only redistributes the fee between LPs and the
    // protocol; it does not change `amount_out`/`amount_in`/`fee_amount`, so the
    // quote can pass `(0, 0)` rather than reading the config.
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
        0,
        0,
        true,
    )?;

    Ok(QuoteResponse {
        amount_out: result.amount_out,
        amount_in_consumed: result.amount_in,
        fee_amount: result.fee_amount,
    })
}

/// User-friendly exact-output swap. Caller specifies the desired output amount
/// and a maximum input; the pool charges only the input actually needed and
/// refunds the surplus (native) or pulls exactly the cost (CW20 allowance).
#[allow(clippy::too_many_arguments)]
pub fn execute_swap_exact_output(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    zero_for_one: bool,
    amount_out: Uint128,
    maximum_amount_in: Uint128,
    recipient: Option<String>,
    deadline: Option<u64>,
) -> Result<Response, ContractError> {
    if amount_out.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }
    if let Some(deadline) = deadline {
        if env.block.time.seconds() > deadline {
            return Err(ContractError::DeadlineExceeded {});
        }
    }

    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    let sqrt_price_limit_x96 = if zero_for_one {
        Uint256::from(MIN_SQRT_RATIO) + Uint256::one()
    } else {
        max_sqrt_ratio() - Uint256::one()
    };

    let fee_pips = update_oracle_and_fee(deps.storage, &env, slot0.sqrt_price)?;
    let (fee_protocol_0, fee_protocol_1) = load_protocol_fee_rates(deps.storage);

    let result = compute_swap(
        deps.storage,
        slot0.sqrt_price,
        slot0.tick,
        slot0.liquidity.u128(),
        fg0,
        fg1,
        config.tick_spacing as i32,
        zero_for_one,
        amount_out,
        sqrt_price_limit_x96,
        fee_pips,
        fee_protocol_0,
        fee_protocol_1,
        false,
    )?;

    // Exact-out must deliver the full requested output, else liquidity or the
    // price limit prevented it — revert rather than partially fill.
    if result.amount_out < amount_out {
        return Err(ContractError::InsufficientOutput {
            minimum: amount_out.to_string(),
            actual: result.amount_out.to_string(),
        });
    }
    // Slippage: the input cost must not exceed the caller's maximum.
    if result.amount_in > maximum_amount_in {
        return Err(ContractError::ExcessiveInput {
            maximum: maximum_amount_in.to_string(),
            actual: result.amount_in.to_string(),
        });
    }

    let recipient = recipient.unwrap_or_else(|| info.sender.to_string());
    let sender = info.sender.clone();
    let funds = info.funds.clone();

    let in_token = if zero_for_one {
        &config.token0
    } else {
        &config.token1
    };
    // Native: attached funds (apply_swap refunds the surplus over the cost).
    // CW20: pull exactly the cost via allowance, refunding any wrongly-attached
    // native coins (C-L1).
    let input_source = match in_token {
        AssetInfo::NativeToken { .. } => SwapInputSource::Native(&funds),
        AssetInfo::Token { .. } => SwapInputSource::Cw20Allowance {
            attached_native: &funds,
        },
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

/// Read-only exact-output simulation: cost of buying `amount_out` of `token_out`.
pub fn query_quote_exact_output(
    deps: Deps,
    env: Env,
    token_out: AssetInfo,
    amount_out: Uint128,
) -> Result<QuoteResponse, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;
    let fg0 = FEE_GROWTH_GLOBAL_0.load(deps.storage).unwrap_or_default();
    let fg1 = FEE_GROWTH_GLOBAL_1.load(deps.storage).unwrap_or_default();

    // Output token determines direction: receiving token1 means paying token0.
    let zero_for_one = if token_out == config.token1 {
        true
    } else if token_out == config.token0 {
        false
    } else {
        return Err(ContractError::InvalidFunds {
            reason: format!("token_out '{}' is not a pool token", token_out),
        });
    };

    let sqrt_price_limit_x96 = if zero_for_one {
        Uint256::from(MIN_SQRT_RATIO) + Uint256::one()
    } else {
        max_sqrt_ratio() - Uint256::one()
    };

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
        amount_out,
        sqrt_price_limit_x96,
        fee_pips,
        0,
        0,
        false,
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

#[cfg(test)]
mod iteration_cap_tests {
    use super::*;
    use crate::core::bitmap::flip_tick;
    use crate::state::TICKS;
    use choice_clmm_common::pool::TickInfo;
    use cosmwasm_std::testing::MockStorage;
    use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;

    /// A pool with an initialized tick at (almost) every spacing-1 tick forces
    /// the swap loop to cross one tick per iteration. A wide swap therefore tries
    /// to traverse far more than `MAX_SWAP_ITERATIONS` ticks and must bail out
    /// with a clean `SwapIterationLimit` error rather than running unbounded
    /// (the "dust at every tick" griefing vector that would otherwise make every
    /// quote/swap against such a pool arbitrarily expensive).
    #[test]
    fn swap_bails_out_past_iteration_cap() {
        let mut storage = MockStorage::new();
        let spacing = 1i32;

        // Dust liquidity at ticks 1..=cap+5, each adding +1 L as price rises.
        let n = (MAX_SWAP_ITERATIONS as i32) + 5;
        for tick in 1..=n {
            let info = TickInfo {
                active_positions_count: 1,
                liquidity_delta: 1,
                fee_growth_outside_0: Uint256::zero(),
                fee_growth_outside_1: Uint256::zero(),
                initialized: true,
            };
            TICKS.save(&mut storage, tick, &info).unwrap();
            flip_tick(&mut storage, tick, spacing).unwrap();
        }

        // Start at tick 0, price rising (one_for_zero), huge input, full-range
        // limit so nothing stops the walk except the iteration guard.
        let start_price = get_sqrt_ratio_at_tick(0).unwrap();
        let huge = Uint128::MAX;
        let limit = max_sqrt_ratio() - Uint256::one();

        let err = compute_swap(
            &storage,
            start_price,
            0,
            1_000,         // initial active liquidity
            Uint256::zero(),
            Uint256::zero(),
            spacing,
            false,         // one_for_zero: price increases through ticks 1,2,3,...
            huge,
            limit,
            3_000,
            0,
            0,
            true,
        )
        .unwrap_err();

        match err {
            ContractError::SwapIterationLimit { iterations } => {
                assert_eq!(iterations, MAX_SWAP_ITERATIONS + 1);
            }
            other => panic!("expected SwapIterationLimit, got {:?}", other),
        }
    }

    /// Control: a normal swap that crosses only a handful of ticks completes
    /// well under the cap (guards against the limit being set absurdly low).
    #[test]
    fn normal_swap_well_under_cap() {
        let mut storage = MockStorage::new();
        let spacing = 60i32;

        for tick in [60, 120, 180, 240, 300] {
            let info = TickInfo {
                active_positions_count: 1,
                liquidity_delta: 1_000_000,
                fee_growth_outside_0: Uint256::zero(),
                fee_growth_outside_1: Uint256::zero(),
                initialized: true,
            };
            TICKS.save(&mut storage, tick, &info).unwrap();
            flip_tick(&mut storage, tick, spacing).unwrap();
        }

        let start_price = get_sqrt_ratio_at_tick(0).unwrap();
        let limit = max_sqrt_ratio() - Uint256::one();

        // Modest input — should stop on input exhaustion, not the iteration cap.
        let res = compute_swap(
            &storage,
            start_price,
            0,
            10_000_000,
            Uint256::zero(),
            Uint256::zero(),
            spacing,
            false,
            Uint128::new(1_000),
            limit,
            3_000,
            0,
            0,
            true,
        );
        assert!(res.is_ok(), "normal small swap must not hit the iteration cap");
    }
}

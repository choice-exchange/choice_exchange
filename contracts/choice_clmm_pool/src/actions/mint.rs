use std::convert::TryFrom;

use std::collections::BTreeMap;

use crate::core::bitmap::flip_tick;
use crate::core::positions::update_position;
use crate::error::ContractError;
use crate::state::{
    PoolConfig, PositionInfo, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POOL_CONFIG, POOL_STATE,
    POSITIONS, TICKS,
};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use cosmwasm_std::{
    ensure, Coin, CosmosMsg, DepsMut, Env, MessageInfo, Response, StdError, Uint128, Uint256,
};

// Uniswap V3 `tickSpacingToMaxLiquidityPerTick`:
//
//     max_per_tick = u128::MAX / num_ticks
//     num_ticks    = (MAX_TICK / spacing - (-MAX_TICK / spacing)) + 1
//
// Caps total gross liquidity at any single tick so per-tick u128 math never
// overflows regardless of how many positions reference it. Computed at
// request time (cheap; never changes for a given pool's tick_spacing).
fn tick_spacing_to_max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    // V3 uses round-down on both bounds symmetric to MAX_TICK, so num_ticks is:
    //   floor(MAX_TICK / spacing) - ceil(-MAX_TICK / spacing) + 1
    //   = 2 * floor(MAX_TICK / spacing) + 1
    let min_tick = (MIN_TICK / tick_spacing) * tick_spacing;
    let max_tick = (MAX_TICK / tick_spacing) * tick_spacing;
    let num_ticks = (((max_tick - min_tick) / tick_spacing) as u128).saturating_add(1);
    u128::MAX / num_ticks
}

pub fn execute_mint(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    lower_tick: i32,
    upper_tick: i32,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let config: PoolConfig = POOL_CONFIG.load(deps.storage)?;
    let mut slot0 = POOL_STATE.load(deps.storage)?;

    // --- 1. Validation ---
    if amount.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "Mint amount must be > 0",
        )));
    }
    let amount_liquidity: u128 = amount.u128();

    // Bound liquidity so `u128 -> i128` conversions later in this file and in
    // `positions.rs` / `burn.rs` are guaranteed lossless. Without this, values
    // `> i128::MAX` silently wrap to negative deltas and corrupt tick state.
    let amount_liquidity_i128 = i128::try_from(amount_liquidity).map_err(|_| {
        StdError::generic_err("Mint amount exceeds i128::MAX (liquidity too large)")
    })?;

    if lower_tick >= upper_tick {
        return Err(ContractError::Std(StdError::generic_err(
            "Invalid tick range: lower must be < upper",
        )));
    }
    if !(MIN_TICK..=MAX_TICK).contains(&lower_tick) {
        return Err(ContractError::Std(StdError::generic_err(
            "lower_tick out of bounds",
        )));
    }
    if !(MIN_TICK..=MAX_TICK).contains(&upper_tick) {
        return Err(ContractError::Std(StdError::generic_err(
            "upper_tick out of bounds",
        )));
    }

    let spacing = config.tick_spacing as i32;
    if lower_tick % spacing != 0 || upper_tick % spacing != 0 {
        return Err(ContractError::Std(StdError::generic_err(
            "Tick not divisible by tick spacing",
        )));
    }

    let max_liquidity_per_tick = tick_spacing_to_max_liquidity_per_tick(spacing);

    // --- 2. Price boundaries ---
    let sqrt_price_lower = get_sqrt_ratio_at_tick(lower_tick)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(upper_tick)?;
    let sqrt_price_current = slot0.sqrt_price;

    // --- 3. Update ticks first (so update_position can read them) ---
    let get_global_fees = |storage: &dyn cosmwasm_std::Storage| -> (Uint256, Uint256) {
        (
            FEE_GROWTH_GLOBAL_0
                .may_load(storage)
                .unwrap()
                .unwrap_or(Uint256::zero()),
            FEE_GROWTH_GLOBAL_1
                .may_load(storage)
                .unwrap()
                .unwrap_or(Uint256::zero()),
        )
    };
    let (fee_g_0, fee_g_1) = get_global_fees(deps.storage);

    // Lower tick
    let mut tick_lower = TICKS
        .may_load(deps.storage, lower_tick)?
        .unwrap_or_default();
    if !tick_lower.initialized {
        tick_lower.initialized = true;
        if lower_tick <= slot0.tick {
            tick_lower.fee_growth_outside_0 = fee_g_0;
            tick_lower.fee_growth_outside_1 = fee_g_1;
        }
        flip_tick(deps.storage, lower_tick, spacing)?;
    }
    tick_lower.active_positions_count = tick_lower
        .active_positions_count
        .checked_add(amount_liquidity)
        .ok_or_else(|| StdError::generic_err("active_positions_count overflow on lower tick"))?;
    if tick_lower.active_positions_count > max_liquidity_per_tick {
        return Err(ContractError::Std(StdError::generic_err(
            "Lower tick gross liquidity exceeds MAX_LIQUIDITY_PER_TICK",
        )));
    }
    tick_lower.liquidity_delta = tick_lower
        .liquidity_delta
        .checked_add(amount_liquidity_i128)
        .ok_or_else(|| StdError::generic_err("liquidity_delta overflow on lower tick"))?;
    TICKS.save(deps.storage, lower_tick, &tick_lower)?;

    // Upper tick
    let mut tick_upper = TICKS
        .may_load(deps.storage, upper_tick)?
        .unwrap_or_default();
    if !tick_upper.initialized {
        tick_upper.initialized = true;
        if upper_tick <= slot0.tick {
            tick_upper.fee_growth_outside_0 = fee_g_0;
            tick_upper.fee_growth_outside_1 = fee_g_1;
        }
        flip_tick(deps.storage, upper_tick, spacing)?;
    }
    tick_upper.active_positions_count = tick_upper
        .active_positions_count
        .checked_add(amount_liquidity)
        .ok_or_else(|| StdError::generic_err("active_positions_count overflow on upper tick"))?;
    if tick_upper.active_positions_count > max_liquidity_per_tick {
        return Err(ContractError::Std(StdError::generic_err(
            "Upper tick gross liquidity exceeds MAX_LIQUIDITY_PER_TICK",
        )));
    }
    tick_upper.liquidity_delta = tick_upper
        .liquidity_delta
        .checked_sub(amount_liquidity_i128)
        .ok_or_else(|| StdError::generic_err("liquidity_delta overflow on upper tick"))?;
    TICKS.save(deps.storage, upper_tick, &tick_upper)?;

    // --- 4. Credit position to the caller ---
    //
    // SECURITY: the pool always keys positions by `info.sender`, matching
    // Uniswap V3's invariant. Callers wanting delegated / NFT-wrapped positions
    // must go through `choice_clmm_manager`. Do not reintroduce a `recipient`
    // parameter here — it enables attackers to mint onto arbitrary keys,
    // overwrite `fee_growth_inside_last` on victims, and strand liquidity
    // under the manager's shared key with no NFT attached.
    //
    // `update_position` requires the position to already exist, so initialize
    // an empty `PositionInfo` on first mint. Never overwrite an existing entry
    // (it may carry non-zero `tokens_owed_*` from a prior fully-burned cycle).
    let position_key = (info.sender.as_str(), lower_tick, upper_tick);
    if POSITIONS.may_load(deps.storage, position_key)?.is_none() {
        POSITIONS.save(deps.storage, position_key, &PositionInfo::default())?;
    }
    update_position(
        deps.storage,
        info.sender.as_str(),
        lower_tick,
        upper_tick,
        slot0.tick,
        amount_liquidity_i128,
    )?;

    // --- 5. Update global liquidity if in-range ---
    if slot0.tick >= lower_tick && slot0.tick < upper_tick {
        slot0.liquidity = slot0
            .liquidity
            .checked_add(Uint128::from(amount_liquidity))
            .map_err(|_| StdError::generic_err("Global liquidity overflow"))?;
        POOL_STATE.save(deps.storage, &slot0)?;
    }

    // --- 6. Compute required token amounts (round UP — pay the pool fully) ---
    let amount0: Uint256;
    let amount1: Uint256;

    if slot0.tick < lower_tick {
        amount0 = get_amount0_delta(sqrt_price_lower, sqrt_price_upper, amount_liquidity, true)?;
        amount1 = Uint256::zero();
    } else if slot0.tick >= upper_tick {
        amount0 = Uint256::zero();
        amount1 = get_amount1_delta(sqrt_price_lower, sqrt_price_upper, amount_liquidity, true)?;
    } else {
        amount0 = get_amount0_delta(sqrt_price_current, sqrt_price_upper, amount_liquidity, true)?;
        amount1 = get_amount1_delta(sqrt_price_lower, sqrt_price_current, amount_liquidity, true)?;
    }

    // --- 7. Verify and pull funds ---
    let amount0_u128 =
        Uint128::try_from(amount0).map_err(|_| StdError::generic_err("Amount0 too large"))?;
    let amount1_u128 =
        Uint128::try_from(amount1).map_err(|_| StdError::generic_err("Amount1 too large"))?;

    let mut transfer_msgs: Vec<CosmosMsg> = vec![];

    // Required native amounts, keyed by denom. CW20 tokens contribute nothing.
    let mut required_native: BTreeMap<String, Uint128> = BTreeMap::new();

    if !amount0_u128.is_zero() {
        verify_or_pull_funds(
            &config.token0,
            &info,
            &env,
            amount0_u128,
            &mut transfer_msgs,
            &mut required_native,
        )?;
    }

    if !amount1_u128.is_zero() {
        verify_or_pull_funds(
            &config.token1,
            &info,
            &env,
            amount1_u128,
            &mut transfer_msgs,
            &mut required_native,
        )?;
    }

    // Refund any surplus native funds: coins of a pool denom sent beyond what
    // the mint required, or coins of denoms the pool doesn't use at all. Parity
    // with the swap path — without this, surplus is silently absorbed into the
    // pool reserves.
    let refunds = compute_native_refunds(&info.funds, &required_native)?;
    if !refunds.is_empty() {
        transfer_msgs.push(CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: refunds,
        }));
    }

    Ok(Response::new()
        .add_messages(transfer_msgs)
        .add_attribute("action", "mint")
        .add_attribute("owner", info.sender.as_str())
        .add_attribute("lower_tick", lower_tick.to_string())
        .add_attribute("upper_tick", upper_tick.to_string())
        .add_attribute("liquidity_added", amount_liquidity.to_string())
        .add_attribute("amount0_consumed", amount0_u128)
        .add_attribute("amount1_consumed", amount1_u128))
}

/// For native tokens: verify info.funds contains enough, and record the
/// required amount so surplus can be refunded.
/// For CW20 tokens: add a TransferFrom message to pull from the sender.
fn verify_or_pull_funds(
    token: &AssetInfo,
    info: &MessageInfo,
    env: &Env,
    amount: Uint128,
    messages: &mut Vec<CosmosMsg>,
    required_native: &mut BTreeMap<String, Uint128>,
) -> Result<(), ContractError> {
    match token {
        AssetInfo::NativeToken { denom } => {
            let sent = info
                .funds
                .iter()
                .find(|c| c.denom == *denom)
                .map(|c| c.amount)
                .unwrap_or(Uint128::zero());
            ensure!(
                sent >= amount,
                ContractError::Std(StdError::generic_err(format!(
                    "Insufficient {}. Needed: {}, Sent: {}",
                    denom, amount, sent
                )))
            );
            let entry = required_native
                .entry(denom.clone())
                .or_insert_with(Uint128::zero);
            *entry = entry.checked_add(amount).map_err(|_| {
                StdError::generic_err(format!("required native overflow for {}", denom))
            })?;
        }
        AssetInfo::Token { .. } => {
            if let Some(msg) = token.transfer_from_msg(
                info.sender.as_ref(),
                env.contract.address.as_ref(),
                amount,
            )? {
                messages.push(msg);
            }
        }
    }
    Ok(())
}

/// Subtract `required_native[denom]` from each coin in `funds` and return the
/// positive residue. Coins whose denom is not a pool denom refund in full.
fn compute_native_refunds(
    funds: &[Coin],
    required: &BTreeMap<String, Uint128>,
) -> Result<Vec<Coin>, ContractError> {
    let mut refunds: Vec<Coin> = Vec::new();
    for coin in funds {
        let need = required
            .get(&coin.denom)
            .copied()
            .unwrap_or_else(Uint128::zero);
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
    Ok(refunds)
}

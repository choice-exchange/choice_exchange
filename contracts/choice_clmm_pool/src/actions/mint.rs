use std::convert::TryFrom;

use crate::core::bitmap::flip_tick;
use crate::core::positions::update_position;
use crate::error::ContractError;
use crate::state::{PoolConfig, CONFIG, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, SLOT0, TICKS};
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;
use cosmwasm_std::{ensure, DepsMut, Env, MessageInfo, Response, StdError, Uint128, Uint256};

pub fn execute_mint(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    recipient: String,
    lower_tick: i32,
    upper_tick: i32,
    amount_liquidity: u128,
) -> Result<Response, ContractError> {
    let config: PoolConfig = CONFIG.load(deps.storage)?;
    let mut slot0 = SLOT0.load(deps.storage)?;

    // 1. Validation
    if lower_tick >= upper_tick {
        return Err(ContractError::Std(StdError::generic_err(
            "Invalid tick range: lower must be < upper",
        )));
    }

    let spacing = config.tick_spacing as i32;
    if lower_tick % spacing != 0 || upper_tick % spacing != 0 {
        return Err(ContractError::Std(StdError::generic_err(
            "Tick not divisible by tick spacing",
        )));
    }

    // 2. Calculate SqrtPrices
    let sqrt_price_lower = get_sqrt_ratio_at_tick(lower_tick)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(upper_tick)?;
    let sqrt_price_current = slot0.sqrt_price_x96;

    // 3. Update Ticks FIRST (Initialize if needed)
    // We must initialize ticks before updating position, because position calculates fees based on ticks.

    // Helper to fetch global fees safely
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

    // --- Update Lower Tick ---
    let mut tick_lower = TICKS
        .may_load(deps.storage, lower_tick)?
        .unwrap_or_default();
    if !tick_lower.initialized {
        tick_lower.initialized = true;
        // Important: If tick is below current, assume all global fees happened "outside" (above) it previously?
        // Convention:
        // If tick <= current: fee_outside = fee_global
        // If tick > current: fee_outside = 0
        // This ensures that for a new range, fee_inside = global - outside_lower - outside_upper = 0
        if lower_tick <= slot0.tick {
            tick_lower.fee_growth_outside_0 = fee_g_0;
            tick_lower.fee_growth_outside_1 = fee_g_1;
        }
        // Update bitmap
        flip_tick(deps.storage, lower_tick, spacing)?;
    }
    tick_lower.liquidity_gross += amount_liquidity;
    tick_lower.liquidity_net = tick_lower
        .liquidity_net
        .checked_add(amount_liquidity as i128)
        .unwrap();
    TICKS.save(deps.storage, lower_tick, &tick_lower)?;

    // --- Update Upper Tick ---
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
    tick_upper.liquidity_gross += amount_liquidity;
    // Upper tick crossing subtracts liquidity
    tick_upper.liquidity_net = tick_upper
        .liquidity_net
        .checked_sub(amount_liquidity as i128)
        .unwrap();
    TICKS.save(deps.storage, upper_tick, &tick_upper)?;

    let owner = deps.api.addr_validate(&recipient)?;
    // 4. Update Position (Now that ticks exist)
    update_position(
        deps.storage,
        owner.as_str(),
        lower_tick,
        upper_tick,
        slot0.tick,
        amount_liquidity as i128,
    )?;

    // 5. Update Global Liquidity
    if slot0.tick >= lower_tick && slot0.tick < upper_tick {
        slot0.liquidity += Uint128::from(amount_liquidity);
        SLOT0.save(deps.storage, &slot0)?;
    }

    // 6. Calculate Token Amounts Needed (No changes here)
    let amount0: Uint256;
    let amount1: Uint256;

    if slot0.tick < lower_tick {
        amount0 = get_amount0_delta(sqrt_price_lower, sqrt_price_upper, amount_liquidity, true);
        amount1 = Uint256::zero();
    } else if slot0.tick >= upper_tick {
        amount0 = Uint256::zero();
        amount1 = get_amount1_delta(sqrt_price_lower, sqrt_price_upper, amount_liquidity, true);
    } else {
        amount0 = get_amount0_delta(sqrt_price_current, sqrt_price_upper, amount_liquidity, true);
        amount1 = get_amount1_delta(sqrt_price_lower, sqrt_price_current, amount_liquidity, true);
    }

    // 7. Verify Funds
    let amount0_u128 =
        Uint128::try_from(amount0).map_err(|_| StdError::generic_err("Amount0 too large"))?;
    let amount1_u128 =
        Uint128::try_from(amount1).map_err(|_| StdError::generic_err("Amount1 too large"))?;

    if !amount0_u128.is_zero() {
        let sent = info
            .funds
            .iter()
            .find(|c| c.denom == config.token0)
            .map(|c| c.amount)
            .unwrap_or(Uint128::zero());
        ensure!(
            sent >= amount0_u128,
            ContractError::Std(StdError::generic_err(format!(
                "Insufficient {}. Needed: {}, Sent: {}",
                config.token0, amount0_u128, sent
            )))
        );
    }

    if !amount1_u128.is_zero() {
        let sent = info
            .funds
            .iter()
            .find(|c| c.denom == config.token1)
            .map(|c| c.amount)
            .unwrap_or(Uint128::zero());
        ensure!(
            sent >= amount1_u128,
            ContractError::Std(StdError::generic_err(format!(
                "Insufficient {}. Needed: {}, Sent: {}",
                config.token1, amount1_u128, sent
            )))
        );
    }

    Ok(Response::new()
        .add_attribute("action", "mint")
        .add_attribute("liquidity_added", amount_liquidity.to_string())
        .add_attribute("amount0_consumed", amount0_u128)
        .add_attribute("amount1_consumed", amount1_u128))
}

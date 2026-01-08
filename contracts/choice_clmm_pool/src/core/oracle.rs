use std::convert::TryFrom;

use crate::state::{OracleData, ORACLE, POOL_CONFIG};
use choice_clmm_math::full_math::mul_div;
use cosmwasm_std::{Env, StdResult, Storage, Uint128, Uint256};

/// Initialize the oracle during pool creation
pub fn initialize_oracle(storage: &mut dyn Storage, time: u64, price: Uint256) -> StdResult<()> {
    let data = OracleData {
        price_ema_x96: price,
        last_block_time: time,
    };
    ORACLE.save(storage, &data)
}

/// Updates the EMA based on time elapsed since last block.
/// Should be called at the start of an interaction (Swap).
pub fn update_oracle(
    storage: &mut dyn Storage,
    env: &Env,
    current_price: Uint256,
) -> StdResult<()> {
    let mut oracle = ORACLE.load(storage)?;
    let config = POOL_CONFIG.load(storage)?;

    let now = env.block.time.seconds();
    // If multiple txs in same block, delta is 0. No update needed.
    if now <= oracle.last_block_time {
        return Ok(());
    }

    let delta = now - oracle.last_block_time;
    let halflife = config.fee_config.ema_halflife_seconds;

    // Logic: Weighted Average
    // If time_delta >= halflife, the old price is fully decayed.
    // If time_delta < halflife, we interpolate.
    // EMA_new = (EMA_old * (HalfLife - Delta) + Price_curr * Delta) / HalfLife

    if delta >= halflife {
        oracle.price_ema_x96 = current_price;
    } else {
        let weight_old = Uint256::from(halflife - delta);
        let weight_new = Uint256::from(delta);
        let total_weight = Uint256::from(halflife);

        // Safe math for U256
        let term1 = oracle
            .price_ema_x96
            .checked_mul(weight_old)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle overflow 1"))?;
        let term2 = current_price
            .checked_mul(weight_new)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle overflow 2"))?;

        oracle.price_ema_x96 = (term1 + term2)
            .checked_div(total_weight)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle div 0"))?;
    }

    oracle.last_block_time = now;
    ORACLE.save(storage, &oracle)
}

/// Calculates the fee based on volatility (Deviation from EMA).
/// Fee = Base + ( |Price - EMA| / EMA ) * Multiplier
pub fn get_dynamic_fee(
    storage: &dyn Storage,
    _env: &Env,
    current_price: Uint256,
) -> StdResult<u32> {
    let oracle = ORACLE.load(storage)?;
    let config = POOL_CONFIG.load(storage)?;

    let ema = oracle.price_ema_x96;

    // 1. Calculate Volatility (Difference)
    let diff = if current_price > ema {
        current_price - ema
    } else {
        ema - current_price
    };

    // 2. Calculate Dynamic Part
    // Formula: (Diff * Multiplier) / EMA
    let multiplier = Uint256::from(config.fee_config.volatility_multiplier);

    // dynamic = (diff * multiplier) / ema
    // We use mul_div to handle the 256-bit intermediate overflow safely
    let dynamic_pips = mul_div(diff, multiplier, ema);

    // 3. Add Base and Cap
    let base = Uint256::from(config.fee_config.base_fee_ppm);
    let total = base + dynamic_pips;
    let max = Uint256::from(config.fee_config.max_fee_ppm);

    let final_fee = if total > max { max } else { total };

    // FIX: Convert Uint256 -> Uint128 -> u128 -> u32
    let fee_u128 = Uint128::try_from(final_fee)
        .map_err(|_| cosmwasm_std::StdError::generic_err("Fee calculation overflow"))?;

    Ok(fee_u128.u128() as u32)
}

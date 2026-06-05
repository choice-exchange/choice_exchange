use std::convert::TryFrom;

use crate::state::{OracleData, ORACLE, POOL_CONFIG};
use choice_clmm_math::full_math::mul_div;
use cosmwasm_std::{Env, StdResult, Storage, Uint128, Uint256};

/// If the oracle hasn't been touched in this long, `get_dynamic_fee` falls back
/// to `base_fee_ppm` — long gaps mean we have no recent price reference, so
/// charging volatility-based fees would be guessing.
const MAX_ORACLE_AGE_SECONDS: u64 = 3600; // 1 hour

/// Initialize the oracle during pool creation.
pub fn initialize_oracle(storage: &mut dyn Storage, time: u64, price: Uint256) -> StdResult<()> {
    let config = POOL_CONFIG.load(storage)?;
    let data = OracleData {
        price_ema_x96: price,
        last_block_time: time,
        last_fee_ppm: config.fee_config.base_fee_ppm,
    };
    ORACLE.save(storage, &data)
}

/// Blend the stored EMA toward the latest price, compute the raw volatility-
/// driven fee, clamp it to the per-block rate limit, and persist both.
///
/// Returns the rate-limited fee (ppm) that the caller should use for this
/// swap. This replaces the old split of `update_oracle` (state write) +
/// `get_dynamic_fee` (read): having a single write point means the fee is
/// committed to storage before the swap runs, so rate-limit state can't be
/// inverted by reordering calls.
pub fn update_oracle_and_fee(
    storage: &mut dyn Storage,
    env: &Env,
    current_price: Uint256,
) -> StdResult<u32> {
    let config = POOL_CONFIG.load(storage)?;
    let mut oracle = ORACLE.load(storage)?;

    let now = env.block.time.seconds();
    let delta = now.saturating_sub(oracle.last_block_time);

    // Within the same block, the oracle state (ema + last_fee) is treated as
    // immutable — multiple swaps within one block all pay the SAME fee that
    // was committed by whichever swap ran first. This prevents attacker
    // swaps from re-deriving the fee mid-block to dodge the rate limit.
    if delta == 0 {
        return Ok(oracle.last_fee_ppm);
    }

    // --- EMA update ---
    //
    //   EMA_new = (EMA_old * (halflife - delta) + price * delta) / halflife,
    //   collapsing to `price` when `delta >= halflife`.
    let halflife = config.fee_config.ema_halflife_seconds.max(1);
    if delta >= halflife {
        oracle.price_ema_x96 = current_price;
    } else {
        let weight_old = Uint256::from(halflife - delta);
        let weight_new = Uint256::from(delta);
        let total_weight = Uint256::from(halflife);
        let term1 = oracle
            .price_ema_x96
            .checked_mul(weight_old)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle EMA overflow"))?;
        let term2 = current_price
            .checked_mul(weight_new)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle price overflow"))?;
        oracle.price_ema_x96 = (term1 + term2)
            .checked_div(total_weight)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle div zero"))?;
    }

    // --- Raw dynamic fee from (updated) EMA ---
    let raw_fee = compute_raw_dynamic_fee(
        &config.fee_config,
        oracle.price_ema_x96,
        current_price,
    )?;

    // --- Rate-limit the fee change per elapsed second ---
    let prev_fee = oracle.last_fee_ppm;
    let max_change_ppm = config
        .fee_config
        .max_fee_change_per_second_ppm
        .saturating_mul(delta.min(u32::MAX as u64) as u32);
    let clamped = if max_change_ppm == 0 {
        // Rate-limiting disabled — legacy behavior.
        raw_fee
    } else if raw_fee > prev_fee {
        prev_fee.saturating_add(max_change_ppm).min(raw_fee)
    } else {
        prev_fee.saturating_sub(max_change_ppm).max(raw_fee)
    };

    oracle.last_fee_ppm = clamped;
    oracle.last_block_time = now;
    ORACLE.save(storage, &oracle)?;

    Ok(clamped)
}

/// Read-only fee accessor for queries / quotes.
///
/// Returns the most recent rate-limited fee. If the oracle is stale
/// (> `MAX_ORACLE_AGE_SECONDS` since the last update), falls back to
/// `base_fee_ppm` — stale state means we have no recent price reference.
pub fn get_dynamic_fee(
    storage: &dyn Storage,
    env: &Env,
    _current_price: Uint256,
) -> StdResult<u32> {
    let oracle = ORACLE.load(storage)?;
    let config = POOL_CONFIG.load(storage)?;

    let now = env.block.time.seconds();
    if now > oracle.last_block_time + MAX_ORACLE_AGE_SECONDS {
        return Ok(config.fee_config.base_fee_ppm);
    }
    Ok(oracle.last_fee_ppm)
}

/// `base_fee + |current - EMA| / EMA * multiplier`, capped at `max_fee_ppm`.
/// Never panics.
fn compute_raw_dynamic_fee(
    config: &choice_clmm_common::pool::FeeConfig,
    ema: Uint256,
    current_price: Uint256,
) -> StdResult<u32> {
    if ema.is_zero() {
        return Ok(config.base_fee_ppm);
    }

    let diff = if current_price > ema {
        current_price - ema
    } else {
        ema - current_price
    };
    let multiplier = Uint256::from(config.volatility_multiplier);
    let dynamic_pips = mul_div(diff, multiplier, ema)?;

    let total = Uint256::from(config.base_fee_ppm)
        .checked_add(dynamic_pips)
        .map_err(|_| cosmwasm_std::StdError::generic_err("Fee total overflow"))?;
    let max = Uint256::from(config.max_fee_ppm);
    let capped = if total > max { max } else { total };

    let fee_u128 = Uint128::try_from(capped)
        .map_err(|_| cosmwasm_std::StdError::generic_err("Fee conversion overflow"))?;
    // max_fee_ppm was constructor-validated to be < 1_000_000, so the cast is safe.
    Ok(fee_u128.u128() as u32)
}

/// Legacy shim — kept for call sites that still split update + read.
/// Prefer `update_oracle_and_fee` going forward.
pub fn update_oracle(
    storage: &mut dyn Storage,
    env: &Env,
    current_price: Uint256,
) -> StdResult<()> {
    update_oracle_and_fee(storage, env, current_price).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use choice_clmm_common::pool::FeeConfig;

    // Extreme but in-range sqrt prices (Q64.96): the inclusive min and one below
    // the exclusive max that `get_tick_at_sqrt_ratio` accepts.
    fn min_sqrt() -> Uint256 {
        Uint256::from(4_295_128_739u128)
    }
    fn max_sqrt_minus_one() -> Uint256 {
        use std::str::FromStr;
        Uint256::from_str("1461446703485210103287273052203988822378723970341").unwrap()
    }

    fn cfg(base: u32, max: u32, mult: u32) -> FeeConfig {
        FeeConfig {
            base_fee_ppm: base,
            max_fee_ppm: max,
            volatility_multiplier: mult,
            ema_halflife_seconds: 600,
            max_fee_change_per_second_ppm: 0,
        }
    }

    /// The load-bearing security invariant: for the most adversarial in-range
    /// inputs (maximum allowed multiplier against maximum price divergence), the
    /// raw fee saturates to `max_fee_ppm` and never reaches `1_000_000`, never
    /// overflows, and never panics. This is what makes the `as u32` cast and the
    /// (unreachable) `fee_pips >= FEE_DENOMINATOR` rejection in `compute_swap_step`
    /// safe.
    #[test]
    fn raw_fee_caps_below_denominator_at_extremes() {
        // max_fee_ppm at its own ceiling, multiplier at the new bound.
        let config = cfg(3_000, 999_999, 1_000_000);

        // Maximum upward divergence: tiny EMA, huge current price.
        let fee_up = compute_raw_dynamic_fee(&config, min_sqrt(), max_sqrt_minus_one()).unwrap();
        assert_eq!(fee_up, 999_999, "huge divergence must saturate to the cap");
        assert!(fee_up < 1_000_000);

        // Maximum downward divergence: huge EMA, tiny current price.
        let fee_down = compute_raw_dynamic_fee(&config, max_sqrt_minus_one(), min_sqrt()).unwrap();
        assert_eq!(fee_down, 999_999);
        assert!(fee_down < 1_000_000);
    }

    /// `volatility_multiplier == 0` disables the volatility component: the fee is
    /// exactly `base_fee_ppm` regardless of how far price has diverged.
    #[test]
    fn raw_fee_zero_multiplier_is_constant_base() {
        let config = cfg(3_000, 999_999, 0);
        let fee = compute_raw_dynamic_fee(&config, min_sqrt(), max_sqrt_minus_one()).unwrap();
        assert_eq!(fee, 3_000);
    }

    /// A zero EMA (degenerate, shouldn't occur in practice) returns the base fee
    /// rather than dividing by zero.
    #[test]
    fn raw_fee_zero_ema_is_base() {
        let config = cfg(3_000, 999_999, 100);
        let fee = compute_raw_dynamic_fee(&config, Uint256::zero(), min_sqrt()).unwrap();
        assert_eq!(fee, 3_000);
    }

    /// Between the extremes the fee scales: a small divergence lands strictly
    /// between base and the cap; a large one is clamped to the cap.
    #[test]
    fn raw_fee_scales_then_clamps() {
        let config = cfg(3_000, 10_000, 1_000_000);
        let ema = Uint256::from(1_000_000_000_000u128);

        // ~0.1% price divergence with a 1e6 multiplier ≈ +1000 ppm over base.
        let small = ema + ema / Uint256::from(1_000u128);
        let fee_small = compute_raw_dynamic_fee(&config, ema, small).unwrap();
        assert!(
            fee_small > 3_000 && fee_small < 10_000,
            "small divergence should land between base and cap, got {}",
            fee_small
        );

        // 50% divergence vastly exceeds the cap → clamped.
        let large = ema + ema / Uint256::from(2u128);
        let fee_large = compute_raw_dynamic_fee(&config, ema, large).unwrap();
        assert_eq!(fee_large, 10_000);
    }
}

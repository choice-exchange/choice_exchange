use std::convert::TryFrom;

use crate::state::{OracleData, ORACLE, POOL_CONFIG};
use choice_clmm_math::full_math::mul_div;
use cosmwasm_std::{Env, StdResult, Storage, Uint128, Uint256};

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

/// Pure fee computation shared by the write path (`update_oracle_and_fee`) and the read-only
/// quote path (`simulate_fee`). Given the current oracle state, config, block time, and price,
/// returns `(fee_ppm, next_oracle)` where `next_oracle` is the state the write path persists.
///
/// Both paths going through one function is the load-bearing property: it guarantees a `Quote`
/// returns exactly what a swap landing in the same block will charge. `get_dynamic_fee` (the old
/// read path) only returned the stored `last_fee_ppm` and ignored `current_price` entirely, so
/// quotes systematically diverged from execution whenever price had moved since the last swap.
fn compute_fee(
    fee_config: &choice_clmm_common::pool::FeeConfig,
    oracle: &OracleData,
    now: u64,
    current_price: Uint256,
) -> StdResult<(u32, OracleData)> {
    let delta = now.saturating_sub(oracle.last_block_time);

    // Within the same block, the oracle state (ema + last_fee) is treated as
    // immutable — multiple swaps within one block all pay the SAME fee that
    // was committed by whichever swap ran first. This prevents attacker
    // swaps from re-deriving the fee mid-block to dodge the rate limit.
    if delta == 0 {
        return Ok((oracle.last_fee_ppm, oracle.clone()));
    }

    let mut next = oracle.clone();

    // --- EMA update ---
    //
    //   EMA_new = (EMA_old * (halflife - delta) + price * delta) / halflife,
    //   collapsing to `price` when `delta >= halflife`.
    let halflife = fee_config.ema_halflife_seconds.max(1);
    if delta >= halflife {
        next.price_ema_x96 = current_price;
    } else {
        let weight_old = Uint256::from(halflife - delta);
        let weight_new = Uint256::from(delta);
        let total_weight = Uint256::from(halflife);
        let term1 = next
            .price_ema_x96
            .checked_mul(weight_old)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle EMA overflow"))?;
        let term2 = current_price
            .checked_mul(weight_new)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle price overflow"))?;
        next.price_ema_x96 = term1
            .checked_add(term2)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle EMA blend overflow"))?
            .checked_div(total_weight)
            .map_err(|_| cosmwasm_std::StdError::generic_err("Oracle div zero"))?;
    }

    // --- Raw dynamic fee, measured against the PRE-blend EMA ---
    //
    // The deviation must be taken against the EMA as of the LAST update, not
    // the freshly blended one: `p - blend(ema, p)` shrinks by a factor of
    // (halflife - delta)/halflife, so measuring post-blend silently scaled
    // the volatility signal down with the time since the last swap — half
    // lost at delta = halflife/2 and exactly zero at delta >= halflife,
    // letting the followers of a price move trade at base fee on any
    // infrequently-traded pool. The blended EMA is still persisted below, so
    // an elevated fee lasts only until the reference catches up with price.
    let raw_fee = compute_raw_dynamic_fee(fee_config, oracle.price_ema_x96, current_price)?;

    // --- Rate-limit the fee change per elapsed second ---
    let prev_fee = oracle.last_fee_ppm;
    let max_change_ppm = fee_config
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

    next.last_fee_ppm = clamped;
    next.last_block_time = now;

    Ok((clamped, next))
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
    let oracle = ORACLE.load(storage)?;
    let now = env.block.time.seconds();

    let (fee, next) = compute_fee(&config.fee_config, &oracle, now, current_price)?;

    // delta == 0 leaves the oracle untouched (next == oracle); skip the redundant write to
    // preserve the prior same-block no-op behavior.
    if next.last_block_time != oracle.last_block_time {
        ORACLE.save(storage, &next)?;
    }

    Ok(fee)
}

/// Read-only twin of `update_oracle_and_fee`: returns the fee a swap landing in THIS block
/// would be charged, WITHOUT persisting the oracle update. `Quote` / `QuoteExactOutput` use this
/// so a quote matches what the swap will actually charge in the same block (same `env.block.time`
/// and starting price). Cross-block drift is irreducible — it depends on when the swap lands.
pub fn simulate_fee(storage: &dyn Storage, env: &Env, current_price: Uint256) -> StdResult<u32> {
    let config = POOL_CONFIG.load(storage)?;
    let oracle = ORACLE.load(storage)?;
    let now = env.block.time.seconds();

    let (fee, _next) = compute_fee(&config.fee_config, &oracle, now, current_price)?;
    Ok(fee)
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

    fn oracle(ema: u128, last_time: u64, last_fee: u32) -> OracleData {
        OracleData {
            price_ema_x96: Uint256::from(ema),
            last_block_time: last_time,
            last_fee_ppm: last_fee,
        }
    }

    /// Within the same block (delta == 0) the fee is frozen at the committed value and the
    /// oracle is returned unchanged — multiple swaps in one block pay the same fee.
    #[test]
    fn compute_fee_same_block_is_frozen() {
        let config = cfg(3_000, 30_000, 1_000_000);
        let o = oracle(1_000_000_000_000, 100, 7_777);
        let (fee, next) =
            compute_fee(&config, &o, 100, Uint256::from(2_000_000_000_000u128)).unwrap();
        assert_eq!(fee, 7_777, "same-block fee must be the frozen last_fee_ppm");
        assert_eq!(next, o, "same-block oracle must be unchanged");
    }

    /// After a gap with price diverged from the EMA, the recomputed fee rises above the stale
    /// `last_fee_ppm` — this is exactly what the old `get_dynamic_fee` quote path missed.
    #[test]
    fn compute_fee_reacts_to_price_divergence() {
        // No rate limit, so the raw volatility fee comes through directly.
        let config = cfg(3_000, 30_000, 1_000_000);
        let o = oracle(1_000_000_000_000, 100, 3_000);
        // 1s later, price +0.2% vs the EMA.
        let (fee, next) =
            compute_fee(&config, &o, 101, Uint256::from(1_002_000_000_000u128)).unwrap();
        assert!(
            fee > 3_000,
            "diverged price must raise the fee above last_fee, got {}",
            fee
        );
        assert_eq!(next.last_block_time, 101);
        assert_eq!(next.last_fee_ppm, fee);
    }

    /// Pins the FACTORY default calibration (multiplier 100_000, max = 2x base) against
    /// real-world price moves, remembering the oracle works in sqrt-price space (a P% price
    /// move is ~P/2% of sqrt deviation). A ~6% price deviation from the EMA must saturate
    /// the 0.30% tier to its 0.60% cap, while a 1% deviation lands meaningfully between
    /// base and cap. The old multiplier of 100 produced 0.5 ppm for a 1% move — a no-op.
    #[test]
    fn factory_default_calibration_responds_to_real_moves() {
        let config = cfg(3_000, 6_000, 100_000);
        let ema = 1_000_000_000_000u128;

        // +6.2% price => sqrt factor ~1.03054 => dynamic ~3054 ppm => capped at 6000.
        let fee = compute_raw_dynamic_fee(
            &config,
            Uint256::from(ema),
            Uint256::from(1_030_540_000_000u128),
        )
        .unwrap();
        assert_eq!(fee, 6_000, "~6% price deviation must saturate the 2x cap");

        // +1% price => sqrt factor ~1.004987 => dynamic ~498 ppm => 3498.
        let fee = compute_raw_dynamic_fee(
            &config,
            Uint256::from(ema),
            Uint256::from(1_004_987_000_000u128),
        )
        .unwrap();
        assert_eq!(fee, 3_498, "1% price deviation must add ~500 ppm");
    }

    /// A price move discovered after a gap >= halflife is still charged: the deviation is
    /// measured against the PRE-blend EMA, so the snap-to-price (which is persisted) cannot
    /// zero the volatility signal for the swap that observes the move. Once the snapped EMA
    /// matches a now-stable price, the next update returns to base. (Before the pre-blend
    /// fix, the first call here charged exactly base — the followers of any move older than
    /// one halflife traded free.)
    #[test]
    fn gap_move_is_charged_then_fee_returns_to_base() {
        let config = cfg(3_000, 30_000, 1_000_000); // halflife 600, no rate limit
        let o = oracle(1_000_000_000_000, 100, 3_000);

        // 600s later the price sits 5x away from the last-known EMA: the raw fee
        // saturates to the cap, and the EMA snaps to the new price for storage.
        let price = Uint256::from(5_000_000_000_000u128);
        let (fee, next) = compute_fee(&config, &o, 100 + 600, price).unwrap();
        assert_eq!(fee, 30_000, "a gap-discovered move must be charged");
        assert_eq!(next.price_ema_x96, price, "EMA must snap after >= halflife");

        // Price holds: with the reference caught up, the fee returns to base.
        let (fee, _) = compute_fee(&config, &next, 100 + 601, price).unwrap();
        assert_eq!(fee, 3_000, "stable price after the snap → base fee");
    }

    /// Regression for the EMA self-dilution bug: at delta = halflife/2 the post-blend EMA
    /// sits halfway to the current price, so measuring against it halved the volatility
    /// signal. With factory-default calibration, a ~6.2% price move after 300 quiet seconds
    /// must still saturate the 2x cap (pre-blend reference), not land at ~4500 ppm.
    #[test]
    fn mid_gap_deviation_is_not_diluted() {
        let mut config = cfg(3_000, 6_000, 100_000); // factory defaults, halflife 600
        config.max_fee_change_per_second_ppm = 100;
        let o = oracle(1_000_000_000_000, 0, 3_000);

        let (fee, _) = compute_fee(&config, &o, 300, Uint256::from(1_030_540_000_000u128)).unwrap();
        assert_eq!(
            fee, 6_000,
            "half-halflife gap must not dilute the volatility signal"
        );
    }

    /// The load-bearing fix: a `Quote` (simulate_fee) returns exactly what a swap landing in the
    /// same block charges (update_oracle_and_fee), and the quote does NOT mutate the oracle.
    #[test]
    fn quote_matches_in_block_execution() {
        use crate::state::PoolConfig;
        use choice_clmm_common::types::AssetInfo;
        use cosmwasm_std::testing::{mock_dependencies, mock_env};
        use cosmwasm_std::Addr;

        let mut deps = mock_dependencies();
        let mut env = mock_env();

        let fee_config = FeeConfig {
            base_fee_ppm: 3_000,
            max_fee_ppm: 30_000,
            volatility_multiplier: 1_000_000,
            ema_halflife_seconds: 600,
            max_fee_change_per_second_ppm: 100,
        };
        POOL_CONFIG
            .save(
                deps.as_mut().storage,
                &PoolConfig {
                    factory: Addr::unchecked("factory"),
                    token0: AssetInfo::NativeToken {
                        denom: "uatom".into(),
                    },
                    token1: AssetInfo::NativeToken {
                        denom: "uusd".into(),
                    },
                    tick_spacing: 60,
                    fee_config,
                    hook: None,
                    hook_permissions: 0,
                },
            )
            .unwrap();

        let start = env.block.time.seconds();
        ORACLE
            .save(
                deps.as_mut().storage,
                &oracle(1_000_000_000_000, start, 3_000),
            )
            .unwrap();

        // Advance a block and move price away from the EMA.
        env.block.time = env.block.time.plus_seconds(60);
        let price = Uint256::from(1_002_000_000_000u128);

        // Read-only quote must not touch storage.
        let before = ORACLE.load(deps.as_ref().storage).unwrap();
        let quoted = simulate_fee(deps.as_ref().storage, &env, price).unwrap();
        assert_eq!(
            before,
            ORACLE.load(deps.as_ref().storage).unwrap(),
            "simulate_fee mutated the oracle"
        );

        // Execution in the same block must charge exactly the quoted fee.
        let executed = update_oracle_and_fee(deps.as_mut().storage, &env, price).unwrap();
        assert_eq!(quoted, executed, "quote != in-block execution fee");
        assert_ne!(
            quoted, 3_000,
            "price moved, so the fee must differ from the stale last_fee"
        );

        // Execution persisted the update; a same-block re-quote returns the committed fee.
        assert_eq!(
            simulate_fee(deps.as_ref().storage, &env, price).unwrap(),
            executed
        );
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

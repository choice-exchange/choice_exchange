use crate::state::{OracleData, ORACLE, POOL_CONFIG};
use cosmwasm_std::{Env, StdResult, Storage};

/// Fixed scale for the convex variable fee: `variable_ppm = control · v_a² / VFEE_SCALE`.
/// At 1e6, `control = 8800` adds ~2990 ppm at a single 6% move (~583 ticks) on the
/// 0.30% tier (calibration: docs/clmm_dynamic_fee_v2_plan.md §3). Kept in lockstep with
/// `VFEE_SCALE` in examples/fee_backtest.rs — that harness is the executable spec.
pub const VFEE_SCALE: u128 = 1_000_000;

/// Initialize the oracle during pool creation. Seeds `last_tick` from the pool's
/// opening tick so the first swap's volatility measurement is relative to the
/// price the pool was created at (not an arbitrary zero).
pub fn initialize_oracle(storage: &mut dyn Storage, time: u64, tick: i32) -> StdResult<()> {
    let config = POOL_CONFIG.load(storage)?;
    let data = OracleData {
        last_update_time: time,
        last_tick: tick,
        volatility_accumulator: 0,
        last_fee_ppm: config.fee_config.base_fee_ppm,
    };
    ORACLE.save(storage, &data)
}

/// Pure fee computation shared by the write path (`update_oracle_and_fee`) and the read-only
/// quote path (`simulate_fee`). Given the current oracle state, config, block time, and tick,
/// returns `(fee_ppm, next_oracle)` where `next_oracle` is the state the write path persists.
///
/// Both paths going through one function is the load-bearing property: it guarantees a `Quote`
/// returns exactly what a swap landing in the same block will charge.
///
/// # v2 fee math (decaying realized-tick-movement accumulator → convex fee)
///
/// ```text
/// delta = now − last_update_time
/// if delta == 0: return (last_fee_ppm, oracle)              // same-block freeze
/// window  = max(volatility_decay_seconds, 1)
/// decayed = (delta >= window) ? 0 : v_a · (window − delta) / window
/// v_a     = min(decayed + |current_tick − last_tick|, max_volatility_accumulator)
/// variable_ppm = control · v_a² / VFEE_SCALE                // u128 intermediate
/// raw_fee = min(base_fee_ppm + variable_ppm, max_fee_ppm)
/// clamped = clamp(raw_fee, prev_fee ± max_fee_change_per_second_ppm · delta)   // rate-limit
/// persist {last_update_time=now, last_tick=current_tick, v_a, last_fee=clamped}
/// ```
///
/// Never panics: every arithmetic step is checked or saturating, and the result is provably
/// `< 1_000_000` (it is `min(_, max_fee_ppm)` and `max_fee_ppm` is constructor-bounded
/// `< 1_000_000`), so the downstream `as u32` cast and the `fee_pips >= FEE_DENOMINATOR`
/// rejection in `compute_swap_step` are safe.
fn compute_fee(
    fee_config: &choice_clmm_common::pool::FeeConfig,
    oracle: &OracleData,
    now: u64,
    current_tick: i32,
) -> StdResult<(u32, OracleData)> {
    let delta = now.saturating_sub(oracle.last_update_time);

    // Within the same block, the oracle state (accumulator + last_fee) is treated
    // as immutable — multiple swaps within one block all pay the SAME fee that was
    // committed by whichever swap ran first. This prevents attacker swaps from
    // re-deriving the fee mid-block to dodge the rate limit, and keeps `v_a` from
    // double-counting a move that several swaps in one block each observe.
    if delta == 0 {
        return Ok((oracle.last_fee_ppm, oracle.clone()));
    }

    let mut next = oracle.clone();

    // --- 1. Decay the accumulator by idle time, then add the realized move ---
    //
    // Linear full-forget window: the prior accumulator is scaled by
    // `(window − delta) / window`, reaching 0 once `delta >= window`. We then add
    // the realized move observed since the last update, `|current_tick − last_tick|`.
    // The reference is the *instantaneous* last tick (no EMA lag), so a one-step
    // gap registers in full (unlike a re-anchored displacement reference, which
    // would zero on the gap step) and a slow trend reads as a sequence of small
    // increments rather than a large persistent displacement.
    let window = fee_config.volatility_decay_seconds.max(1) as u64;
    let decayed: u64 = if delta >= window {
        0
    } else {
        // `v_a <= max_volatility_accumulator <= u32::MAX` and `(window - delta) < window <= u32::MAX`,
        // so the product fits in u128; the result is `<= v_a`, back within u64.
        let prod = (oracle.volatility_accumulator as u128)
            .checked_mul((window - delta) as u128)
            .ok_or_else(|| cosmwasm_std::StdError::generic_err("Oracle decay overflow"))?;
        (prod / window as u128) as u64
    };
    let step_move = (current_tick as i64 - oracle.last_tick as i64).unsigned_abs();
    let v_a = decayed
        .saturating_add(step_move)
        .min(fee_config.max_volatility_accumulator as u64);

    // --- 2. Convex variable fee (u128 intermediate for the square) ---
    //
    // `v_a <= max_volatility_accumulator <= u32::MAX`, so `v_a² <= u64::MAX` and
    // `control · v_a²` fits in u128 (≤ u32::MAX · u64::MAX < u128::MAX). The
    // instantiate bound on `max_volatility_accumulator` keeps this far from the
    // u128 ceiling.
    let v_a_u128 = v_a as u128;
    let variable_ppm = (fee_config.variable_fee_control as u128)
        .checked_mul(v_a_u128)
        .and_then(|x| x.checked_mul(v_a_u128))
        .ok_or_else(|| cosmwasm_std::StdError::generic_err("Oracle variable fee overflow"))?
        / VFEE_SCALE;

    // base_fee + variable, capped at max_fee_ppm. The cap makes the result
    // provably `< 1_000_000` regardless of how large the variable term grew.
    let raw_total = (fee_config.base_fee_ppm as u128)
        .checked_add(variable_ppm)
        .ok_or_else(|| cosmwasm_std::StdError::generic_err("Oracle fee total overflow"))?;
    let raw_fee = raw_total.min(fee_config.max_fee_ppm as u128) as u32;

    // --- 3. Rate-limit the fee change per elapsed second (KEEP, unchanged) ---
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

    next.last_update_time = now;
    next.last_tick = current_tick;
    next.volatility_accumulator = v_a;
    next.last_fee_ppm = clamped;

    Ok((clamped, next))
}

/// Decay the volatility accumulator, fold in the realized tick-move, compute the
/// convex variable fee, clamp it to the per-second rate limit, and persist the
/// new oracle state.
///
/// Returns the rate-limited fee (ppm) the caller should use for this swap. Having
/// a single write point means the fee is committed to storage before the swap
/// runs, so rate-limit state can't be inverted by reordering calls.
pub fn update_oracle_and_fee(
    storage: &mut dyn Storage,
    env: &Env,
    current_tick: i32,
) -> StdResult<u32> {
    let config = POOL_CONFIG.load(storage)?;
    let oracle = ORACLE.load(storage)?;
    let now = env.block.time.seconds();

    let (fee, next) = compute_fee(&config.fee_config, &oracle, now, current_tick)?;

    // delta == 0 leaves the oracle untouched (next == oracle); skip the redundant write to
    // preserve the prior same-block no-op behavior.
    if next.last_update_time != oracle.last_update_time {
        ORACLE.save(storage, &next)?;
    }

    Ok(fee)
}

/// Read-only twin of `update_oracle_and_fee`: returns the fee a swap landing in THIS block
/// would be charged, WITHOUT persisting the oracle update. `Quote` / `QuoteExactOutput` use this
/// so a quote matches what the swap will actually charge in the same block (same `env.block.time`
/// and starting tick). Cross-block drift is irreducible — it depends on when the swap lands.
pub fn simulate_fee(storage: &dyn Storage, env: &Env, current_tick: i32) -> StdResult<u32> {
    let config = POOL_CONFIG.load(storage)?;
    let oracle = ORACLE.load(storage)?;
    let now = env.block.time.seconds();

    let (fee, _next) = compute_fee(&config.fee_config, &oracle, now, current_tick)?;
    Ok(fee)
}

#[cfg(test)]
mod tests {
    use super::*;
    use choice_clmm_common::pool::FeeConfig;

    /// Factory-style config (0.30% tier), Phase-0 defaults. `rate_limit` overrides
    /// the per-second clamp so individual tests can isolate the raw convex fee.
    fn cfg(rate_limit: u32) -> FeeConfig {
        FeeConfig {
            base_fee_ppm: 3_000,
            max_fee_ppm: 6_000,
            variable_fee_control: 8_800,
            max_volatility_accumulator: 2_000,
            volatility_decay_seconds: 600,
            max_fee_change_per_second_ppm: rate_limit,
        }
    }

    fn oracle(last_tick: i32, v_a: u64, last_time: u64, last_fee: u32) -> OracleData {
        OracleData {
            last_update_time: last_time,
            last_tick,
            volatility_accumulator: v_a,
            last_fee_ppm: last_fee,
        }
    }

    /// Within the same block (delta == 0) the fee is frozen at the committed value and the
    /// oracle is returned unchanged — multiple swaps in one block pay the same fee and the
    /// accumulator does not double-count a move several swaps each observe.
    #[test]
    fn compute_fee_same_block_is_frozen() {
        let config = cfg(0);
        let o = oracle(100, 1_234, 100, 7_777);
        let (fee, next) = compute_fee(&config, &o, 100, 9_999).unwrap();
        assert_eq!(fee, 7_777, "same-block fee must be the frozen last_fee_ppm");
        assert_eq!(next, o, "same-block oracle must be unchanged");
    }

    /// CONVEXITY: the variable fee grows with the square of the realized move. Doubling the
    /// tick excursion roughly quadruples the variable component (base subtracted out). Uses a
    /// high cap so neither sample saturates.
    #[test]
    fn variable_fee_is_convex_in_the_move() {
        let mut config = cfg(0);
        config.max_fee_ppm = 999_999;

        // Single 100-tick move: variable = 8800·100²/1e6 = 88 ppm.
        let (fee_100, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 100).unwrap();
        assert_eq!(fee_100, 3_088, "100-tick move adds 88 ppm");

        // Single 200-tick move: variable = 8800·200²/1e6 = 352 ppm ≈ 4×88.
        let (fee_200, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 200).unwrap();
        assert_eq!(
            fee_200, 3_352,
            "200-tick move adds 352 ppm (4× the 100-tick bump)"
        );

        let bump_100 = fee_100 - 3_000;
        let bump_200 = fee_200 - 3_000;
        assert_eq!(
            bump_200,
            4 * bump_100,
            "doubling the move quadruples the variable fee"
        );
    }

    /// The §3 calibration table, pinned: a single 6% move (~583 ticks) adds ~2990 ppm and
    /// saturates the 0.30% tier's 2× cap; a single 1% move (~100 ticks) lands just above base.
    #[test]
    fn calibration_table_single_step_moves() {
        let config = cfg(0); // base 3000, cap 6000, control 8800

        // ~1% move: 8800·100²/1e6 = 88 → 3088.
        let (fee, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 100).unwrap();
        assert_eq!(fee, 3_088);

        // ~6% move (583 ticks): 8800·583²/1e6 = 2991 → 5991, just under the 6000 cap.
        let (fee, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 583).unwrap();
        assert_eq!(fee, 5_991, "single 6% move adds ~2990 ppm");

        // ~9% move (862 ticks): 8800·862²/1e6 = 6538 → capped at 6000.
        let (fee, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 862).unwrap();
        assert_eq!(fee, 6_000, "9% move saturates the cap");
    }

    /// DECAY TO BASE: after a move spikes the accumulator, an idle period decays it linearly,
    /// and once `delta >= window` the accumulator is fully forgotten — the fee returns to base
    /// *regardless of the price level* (the headline v1 gap-then-calm fix). No rate limit here
    /// so the decay shows through directly.
    #[test]
    fn accumulator_decays_to_base_after_idle() {
        let config = cfg(0);

        // Gap to tick 600 from 0: v_a = 600, fee well above base.
        let (gap_fee, after_gap) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 600).unwrap();
        assert!(gap_fee > 3_000, "the gap is charged");
        assert_eq!(after_gap.volatility_accumulator, 600);

        // Hold the new level (600) but trade again half a window (300s) later: the accumulator
        // decays to 600·(600−300)/600 = 300, plus a 0-tick move.
        let (mid_fee, mid) = compute_fee(&config, &after_gap, 1 + 300, 600).unwrap();
        assert_eq!(
            mid.volatility_accumulator, 300,
            "half-window idle halves v_a"
        );
        assert!(
            mid_fee < gap_fee && mid_fee > 3_000,
            "fee relaxes but is not yet base"
        );

        // Trade again a full window later at the same level: accumulator fully forgotten → base.
        let (calm_fee, calm) = compute_fee(&config, &mid, 1 + 300 + 600, 600).unwrap();
        assert_eq!(
            calm.volatility_accumulator, 0,
            "full-window idle forgets v_a"
        );
        assert_eq!(calm_fee, 3_000, "calm at the new level returns to base");
    }

    /// GAP CAPTURED: a one-step gap registers its full excursion in the accumulator (the
    /// re-anchored-reference flaw that left the gap uncharged is avoided). A 6% gap saturates
    /// the cap on the gap step itself.
    #[test]
    fn one_step_gap_is_captured() {
        let config = cfg(0);
        // Sitting calm at tick 0, then a +600-tick (~6%) gap in one step.
        let (fee, next) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 600).unwrap();
        assert_eq!(
            next.volatility_accumulator, 600,
            "the whole gap is accumulated"
        );
        assert!(
            fee >= 5_900,
            "the gap step is charged near/at the cap, got {}",
            fee
        );
    }

    /// TREND NOT CONFLATED: a slow steady drift reads as a sequence of small increments, each
    /// decaying, so it does NOT saturate — unlike v1's displacement, which grew with the trend.
    /// Walk 5 ticks/step over a 30s cadence and confirm the fee stays near base.
    #[test]
    fn steady_trend_is_not_conflated_with_volatility() {
        let config = cfg(0);
        let mut o = oracle(0, 0, 0, 3_000);
        let mut tick = 0i32;
        let mut t = 0u64;
        let mut max_fee = 0u32;
        for _ in 0..100 {
            t += 30;
            tick += 5; // 5 ticks per 30s — a gentle ramp
            let (fee, next) = compute_fee(&config, &o, t, tick).unwrap();
            max_fee = max_fee.max(fee);
            o = next;
        }
        // Each step adds only |Δtick| = 5 to a mostly-decayed accumulator; the fee stays a hair
        // above base and never approaches the cap.
        assert!(
            max_fee < 3_100,
            "slow trend must read as low volatility, peak fee was {}",
            max_fee
        );
    }

    /// ROUND-TRIP CHARGED: a fast there-and-back move accumulates BOTH legs as realized
    /// movement (intended LVR recapture), so the fee rises even though the net displacement is
    /// zero. Ramp up 500 ticks then back to 0 inside the decay window.
    #[test]
    fn round_trip_is_charged_as_realized_variance() {
        let config = cfg(0);
        // Up to 500 in one step, then back to 0 one second later (well inside the 600s window).
        let (_, up) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 500).unwrap();
        let (fee_back, back) = compute_fee(&config, &up, 2, 0).unwrap();
        // 1s decay of v_a=500 → 500·599/600 ≈ 499, plus the 500-tick return leg ≈ 999.
        assert!(
            back.volatility_accumulator > 900,
            "both legs accumulate, v_a = {}",
            back.volatility_accumulator
        );
        assert_eq!(back.last_tick, 0, "ends where it started");
        assert!(
            fee_back >= 5_900,
            "round-trip is charged despite zero net move, got {}",
            fee_back
        );
    }

    /// FILTER VIA DECAY: choppy sustained volatility accumulates ABOVE a single step (each step
    /// adds onto the decayed prior accumulator), so a series of moderate moves saturates the cap
    /// faster than any one of them would alone.
    #[test]
    fn sustained_chop_accumulates_above_a_single_step() {
        let config = cfg(0);
        // A single 200-tick move adds only 352 ppm (well under cap).
        let (single, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 200).unwrap();
        assert!(single < 4_000);

        // Five back-to-back 200-tick swings at a 1s cadence: accumulator climbs each step.
        let mut o = oracle(0, 0, 0, 3_000);
        let mut t = 0u64;
        let mut last_fee = 0u32;
        for i in 0..5 {
            t += 1;
            let tick = if i % 2 == 0 { 200 } else { 0 };
            let (fee, next) = compute_fee(&config, &o, t, tick).unwrap();
            last_fee = fee;
            o = next;
        }
        assert!(
            o.volatility_accumulator > 500,
            "sustained chop accumulates above one step, v_a = {}",
            o.volatility_accumulator
        );
        assert!(
            last_fee > single,
            "sustained chop charges more than a single move"
        );
    }

    /// RATE-LIMIT INTERACTION: with the per-second cap engaged, a large raw fee jump is throttled
    /// to `prev_fee + max_change·delta`, and relaxation downward is likewise throttled.
    #[test]
    fn rate_limit_throttles_fee_changes() {
        let config = cfg(100); // 100 ppm/sec

        // UPWARD: big move 1s after base — raw fee would jump to the cap (6000), but the rate
        // limit caps the rise at 3000 + 100·1 = 3100.
        let (fee, next) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 800).unwrap();
        assert_eq!(fee, 3_100, "upward fee change is rate-limited to +100/sec");
        assert_eq!(next.last_fee_ppm, 3_100);

        // DOWNWARD: an oracle whose accumulator has already decayed to 0 but whose committed fee
        // is still elevated at 6000 (a prior spike). A calm 1s step computes raw = base (3000),
        // but the fall is throttled to 6000 − 100·1 = 5900.
        let (fee, _) = compute_fee(&config, &oracle(0, 0, 0, 6_000), 1, 0).unwrap();
        assert_eq!(
            fee, 5_900,
            "downward fee change is rate-limited to −100/sec"
        );
    }

    /// QUOTE/SWAP PARITY: `simulate_fee` returns exactly what `update_oracle_and_fee` charges in
    /// the same block, and the quote does NOT mutate the oracle. This is the load-bearing
    /// honest-quote property, preserved verbatim from v1.
    #[test]
    fn quote_matches_in_block_execution() {
        use crate::state::PoolConfig;
        use choice_clmm_common::types::AssetInfo;
        use cosmwasm_std::testing::{mock_dependencies, mock_env};
        use cosmwasm_std::Addr;

        let mut deps = mock_dependencies();
        let mut env = mock_env();

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
                    fee_config: cfg(100),
                    hook: None,
                    hook_permissions: 0,
                },
            )
            .unwrap();

        let start = env.block.time.seconds();
        ORACLE
            .save(deps.as_mut().storage, &oracle(0, 0, start, 3_000))
            .unwrap();

        // Advance a block and move the tick away from last_tick.
        env.block.time = env.block.time.plus_seconds(60);
        let tick = 400;

        // Read-only quote must not touch storage.
        let before = ORACLE.load(deps.as_ref().storage).unwrap();
        let quoted = simulate_fee(deps.as_ref().storage, &env, tick).unwrap();
        assert_eq!(
            before,
            ORACLE.load(deps.as_ref().storage).unwrap(),
            "simulate_fee mutated the oracle"
        );

        // Execution in the same block must charge exactly the quoted fee.
        let executed = update_oracle_and_fee(deps.as_mut().storage, &env, tick).unwrap();
        assert_eq!(quoted, executed, "quote != in-block execution fee");
        assert_ne!(
            quoted, 3_000,
            "tick moved, so the fee must differ from the stale last_fee"
        );

        // Execution persisted the update; a same-block re-quote returns the committed fee.
        assert_eq!(
            simulate_fee(deps.as_ref().storage, &env, tick).unwrap(),
            executed
        );
    }

    /// OVERFLOW AT EXTREMES: for the most adversarial in-range inputs (max control, max
    /// accumulator cap, MIN→MAX tick gap in one step), the fee saturates to `max_fee_ppm`,
    /// stays `< 1_000_000`, and never overflows or panics. This is what makes the `as u32`
    /// cast and the (unreachable) `fee_pips >= FEE_DENOMINATOR` rejection in
    /// `compute_swap_step` safe.
    #[test]
    fn fee_caps_below_denominator_at_extremes() {
        use choice_clmm_math::tick_math::{MAX_TICK, MIN_TICK};

        // Every knob at its instantiate ceiling; cap at its own ceiling.
        let config = FeeConfig {
            base_fee_ppm: 3_000,
            max_fee_ppm: 999_999,
            variable_fee_control: u32::MAX,
            max_volatility_accumulator: (2 * MAX_TICK) as u32, // the instantiate bound
            volatility_decay_seconds: 600,
            max_fee_change_per_second_ppm: 0,
        };

        // Maximum single-step excursion: MIN_TICK → MAX_TICK.
        let (fee, next) =
            compute_fee(&config, &oracle(MIN_TICK, 0, 0, 3_000), 1, MAX_TICK).unwrap();
        assert_eq!(fee, 999_999, "extreme inputs saturate to the cap");
        assert!(fee < 1_000_000);
        // The accumulator itself is capped, so it cannot grow without bound either.
        assert_eq!(next.volatility_accumulator, (2 * MAX_TICK) as u64);

        // And the reverse direction is symmetric.
        let (fee_down, _) =
            compute_fee(&config, &oracle(MAX_TICK, 0, 0, 3_000), 1, MIN_TICK).unwrap();
        assert_eq!(fee_down, 999_999);
        assert!(fee_down < 1_000_000);
    }

    /// `variable_fee_control == 0` disables the volatility component: the fee is exactly
    /// `base_fee_ppm` regardless of how large the realized move is.
    #[test]
    fn zero_control_is_constant_base() {
        let mut config = cfg(0);
        config.variable_fee_control = 0;
        let (fee, _) = compute_fee(&config, &oracle(0, 0, 0, 3_000), 1, 800).unwrap();
        assert_eq!(fee, 3_000);
    }
}

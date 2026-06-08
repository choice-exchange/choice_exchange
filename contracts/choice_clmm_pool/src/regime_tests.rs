//! Targeted adversarial regime tests for the CLMM pool (mock backend).
//!
//! These complement the randomized `adversarial_fuzz` battery with hand-built
//! extreme scenarios that random op streams reach only with vanishing
//! probability: the U256 fee-growth wraparound, quote/execution equivalence,
//! constant-price round-trip value conservation, the per-tick liquidity cap, and
//! word-boundary tick crossings.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod regime_tests {
    use crate::actions::flash::{reply_flash, REPLY_FLASH};
    use crate::contract::{execute, instantiate, query};
    use crate::state::{
        is_locked, FEE_GROWTH_GLOBAL_0, POSITIONS, PROTOCOL_FEES_0, PROTOCOL_FEE_CONFIG, TICKS,
    };
    use choice_clmm_common::pool::{
        ExecuteMsg, FeeConfig, InstantiateMsg, QueryMsg, QuoteResponse,
    };
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env, MockApi, MockQuerier};
    use cosmwasm_std::{
        from_json, Addr, Binary, Coin, Env, MemoryStorage, OwnedDeps, Reply, Response, SubMsgResponse,
        SubMsgResult, Uint128, Uint256,
    };

    const T0: &str = "uaaa";
    const T1: &str = "ubbb";

    fn native(d: &str) -> AssetInfo {
        AssetInfo::NativeToken { denom: d.to_string() }
    }
    fn price_one() -> Uint256 {
        Uint256::from_u128(1) << 96
    }

    /// Instantiate a native/native pool. `vol_mult == 0` pins the fee at
    /// `base_fee_ppm` (needed for exact quote/execute equivalence).
    fn setup(
        spacing: u32,
        base_fee: u32,
        vol_mult: u32,
    ) -> (OwnedDeps<MemoryStorage, MockApi, MockQuerier>, Env, Addr) {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&factory, &[]),
            InstantiateMsg {
                token0: native(T0),
                token1: native(T1),
                tick_spacing: spacing,
                fee_config: FeeConfig {
                    base_fee_ppm: base_fee,
                    max_fee_ppm: base_fee.max(6000),
                    volatility_multiplier: vol_mult,
                    ema_halflife_seconds: 600,
                    max_fee_change_per_second_ppm: 0,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();
        let lp = deps.api.addr_make("lp");
        (deps, mock_env(), lp)
    }

    fn attr(res: &Response, key: &str) -> String {
        res.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.value.clone())
            .unwrap_or_else(|| panic!("attribute {key} missing"))
    }

    fn big_coins() -> Vec<Coin> {
        vec![
            Coin::new(Uint128::MAX, T0),
            Coin::new(Uint128::MAX, T1),
        ]
    }

    // ---------------------------------------------------------------------
    // REGIME 1: U256 fee-growth wraparound — attribution must be invariant
    // across the wrap. We run the IDENTICAL mint+swap twice: once with the
    // global accumulator (and the in-range lower tick's fee_growth_outside)
    // pre-loaded to U256::MAX so the swap's fee growth wraps past the modulus,
    // and once from zero. The position's credited fee must be byte-identical.
    // ---------------------------------------------------------------------

    /// Returns (owed0_after_burn0, fee_amount_of_swap, wrapped?). Mints one
    /// in-range position holding all liquidity, optionally offsets the fee-growth
    /// origin by `offset`, runs one token0->token1 swap, then `Burn(0)` to roll
    /// fees into `tokens_owed_0`.
    fn accrue_with_offset(offset: Uint256) -> (u128, u128, bool) {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        let lower = -10000;
        let upper = 10000;
        let liq: u128 = 1_000_000_000_000; // 1e12

        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: lower, upper_tick: upper, amount: Uint128::new(liq) },
        )
        .unwrap();

        // Pre-load the fee-growth origin. The position was minted with global==0
        // so its `fee_growth_inside_0_last == 0` and the lower tick's
        // `fee_growth_outside_0 == 0`; to keep the *baseline* inside == 0 we shift
        // BOTH global and the lower tick's outside by the same offset.
        if !offset.is_zero() {
            FEE_GROWTH_GLOBAL_0.save(&mut deps.storage, &offset).unwrap();
            let mut tl = TICKS.load(&deps.storage, lower).unwrap();
            tl.fee_growth_outside_0 = offset;
            TICKS.save(&mut deps.storage, lower, &tl).unwrap();
        }
        let global_before = FEE_GROWTH_GLOBAL_0.may_load(&deps.storage).unwrap().unwrap_or_default();

        // One token0->token1 swap, staying within the range (small relative to L).
        let funds = vec![Coin::new(Uint128::new(5_000_000), T0)];
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &funds),
            ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
        )
        .unwrap();
        let fee_amount: u128 = attr(&res, "fee_amount").parse().unwrap();
        let global_after = FEE_GROWTH_GLOBAL_0.load(&deps.storage).unwrap();
        let wrapped = global_after < global_before; // accumulator decreased ⇒ wrapped

        // Roll accrued fees into tokens_owed via Burn(0).
        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &[]),
            ExecuteMsg::Burn { lower_tick: lower, upper_tick: upper, amount: Uint128::zero() },
        )
        .unwrap();
        let pos = POSITIONS.load(&deps.storage, (lp.as_str(), lower, upper)).unwrap();
        (pos.tokens_owed_0.u128(), fee_amount, wrapped)
    }

    #[test]
    fn fee_growth_wraparound_attribution_exact() {
        let (owed_ctrl, fee_ctrl, wrapped_ctrl) = accrue_with_offset(Uint256::zero());
        let (owed_wrap, fee_wrap, wrapped) = accrue_with_offset(Uint256::MAX);

        // The control path must NOT wrap; the offset path MUST.
        assert!(!wrapped_ctrl, "control run unexpectedly wrapped");
        assert!(wrapped, "offset run did not wrap the U256 accumulator (test ineffective)");

        // Same swap ⇒ same fee charged, regardless of accumulator origin.
        assert_eq!(fee_ctrl, fee_wrap, "fee charged differs across wrap");
        // The crown jewel: fee ATTRIBUTION is byte-identical across the wrap.
        assert_eq!(
            owed_ctrl, owed_wrap,
            "fee attribution diverged across U256 wrap: control {owed_ctrl} vs wrapped {owed_wrap}"
        );
        // And the position is never over-credited vs the fee actually paid
        // (flooring by /2^128 means it may be a hair under).
        assert!(owed_wrap <= fee_wrap, "over-credit: owed {owed_wrap} > fee {fee_wrap}");
        assert!(owed_wrap > 0, "position credited nothing");
        // Flooring loss is bounded by 1 unit of (2^128 / L) rounded — in practice
        // the gap is tiny; assert it's within a small absolute slack.
        assert!(fee_wrap - owed_wrap <= 2, "attribution lost {} (> rounding)", fee_wrap - owed_wrap);
    }

    /// Wraparound WITH a tick crossing: the crossed tick's `fee_growth_outside`
    /// is recomputed via `wrapping_sub`, which must also survive the wrap. We
    /// drive two positions and a swap that crosses the shared tick, with and
    /// without the U256 offset, and require identical credited fees on the
    /// always-in-range position.
    #[test]
    fn fee_growth_wraparound_with_crossing_exact() {
        fn run(offset: Uint256) -> u128 {
            let (mut deps, env, lp) = setup(10, 3000, 0);
            // Wide position holding the bulk of liquidity, always in range.
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: -20000, upper_tick: 20000, amount: Uint128::new(1_000_000_000_000) },
            ).unwrap();
            // Narrow position above current price that a one_for_zero swap crosses.
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: 1000, upper_tick: 2000, amount: Uint128::new(500_000_000_000) },
            ).unwrap();

            if !offset.is_zero() {
                // Shift global + the in-range lower tick (-20000) outside so the
                // wide position's baseline inside stays 0.
                FEE_GROWTH_GLOBAL_0.save(&mut deps.storage, &offset).unwrap();
                let mut tl = TICKS.load(&deps.storage, -20000).unwrap();
                tl.fee_growth_outside_0 = offset;
                TICKS.save(&mut deps.storage, -20000, &tl).unwrap();
                // The crossable tick 1000 was initialized with outside=0 (it sits
                // above current tick at mint); leave it — crossing recomputes it.
            }

            // token0->token1 pushes price DOWN; to cross tick 1000..2000 we need
            // price to RISE, so swap token1->token0 (one_for_zero). That accrues
            // fee to fg1, not fg0. Instead, accrue fg0 with a zero_for_one swap
            // that does NOT cross (stays in wide range) — crossing test for fg0
            // requires a downward-crossed tick below price. Add one.
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: -2000, upper_tick: -1000, amount: Uint128::new(500_000_000_000) },
            ).unwrap();

            // Large zero_for_one swap: price falls, crossing -1000 then into
            // [-2000,-1000]; accrues fee to fg0 and recomputes outside at -1000.
            let funds = vec![Coin::new(Uint128::new(80_000_000_000), T0)];
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &funds),
                ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
            ).unwrap();

            // Roll fees on the always-in-range wide position.
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[]),
                ExecuteMsg::Burn { lower_tick: -20000, upper_tick: 20000, amount: Uint128::zero() },
            ).unwrap();
            POSITIONS.load(&deps.storage, (lp.as_str(), -20000, 20000)).unwrap().tokens_owed_0.u128()
        }
        let ctrl = run(Uint256::zero());
        let wrap = run(Uint256::MAX);
        assert_eq!(ctrl, wrap, "crossing fee attribution diverged across U256 wrap: {ctrl} vs {wrap}");
        assert!(ctrl > 0, "no fee credited (test ineffective)");
    }

    // ---------------------------------------------------------------------
    // REGIME 8: Quote must equal execution exactly (fee pinned via vol_mult=0).
    // ---------------------------------------------------------------------
    #[test]
    fn quote_matches_execution_exactly() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // Several positions across overlapping ranges for a non-trivial curve.
        for (l, u, q) in [(-30000i32, 30000i32, 800_000_000_000u128), (-5000, 5000, 400_000_000_000), (1000, 9000, 200_000_000_000)] {
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: l, upper_tick: u, amount: Uint128::new(q) },
            ).unwrap();
        }

        let mut st: u64 = 0xC0FFEE;
        let nxt = |s: &mut u64| { *s = s.wrapping_mul(6364136223846793005).wrapping_add(1); (*s >> 33) as u128 };
        for _ in 0..400 {
            let zfo = (nxt(&mut st) & 1) == 0;
            let amt = 1 + nxt(&mut st) % 2_000_000_000;
            let token_in = if zfo { native(T0) } else { native(T1) };

            // Exact-input quote vs execute.
            let q: QuoteResponse = from_json(
                query(deps.as_ref(), env.clone(), QueryMsg::Quote { token_in: token_in.clone(), amount_in: Uint128::new(amt) }).unwrap()
            ).unwrap();
            let denom = if zfo { T0 } else { T1 };
            let funds = vec![Coin::new(Uint128::new(amt), denom)];
            let res = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &funds),
                ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
            ).unwrap();
            let exec_out: u128 = attr(&res, "amount_out").parse().unwrap();
            let exec_in: u128 = attr(&res, "amount_in").parse().unwrap();
            assert_eq!(q.amount_out.u128(), exec_out, "quote amount_out != exec (zfo={zfo}, amt={amt})");
            assert_eq!(q.amount_in_consumed.u128(), exec_in, "quote amount_in != exec (zfo={zfo}, amt={amt})");
        }
    }

    #[test]
    fn quote_exact_output_matches_execution() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        for (l, u, q) in [(-30000i32, 30000i32, 800_000_000_000u128), (-8000, 8000, 300_000_000_000)] {
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: l, upper_tick: u, amount: Uint128::new(q) },
            ).unwrap();
        }
        let mut st: u64 = 0xBEEF;
        let nxt = |s: &mut u64| { *s = s.wrapping_mul(6364136223846793005).wrapping_add(1); (*s >> 33) as u128 };
        for _ in 0..300 {
            let zfo = (nxt(&mut st) & 1) == 0;
            let want_out = 1 + nxt(&mut st) % 100_000_000;
            // token_out is token1 when zero_for_one.
            let token_out = if zfo { native(T1) } else { native(T0) };
            let q: QuoteResponse = from_json(
                query(deps.as_ref(), env.clone(), QueryMsg::QuoteExactOutput { token_out, amount_out: Uint128::new(want_out) }).unwrap()
            ).unwrap();
            // If the quote can't deliver the full output (liquidity bound) skip.
            if q.amount_out.u128() < want_out { continue; }
            let in_denom = if zfo { T0 } else { T1 };
            let res = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[Coin::new(Uint128::MAX, in_denom)]),
                ExecuteMsg::SwapExactOutput { zero_for_one: zfo, amount_out: Uint128::new(want_out), maximum_amount_in: Uint128::MAX, recipient: None, deadline: None },
            ).unwrap();
            let exec_out: u128 = attr(&res, "amount_out").parse().unwrap();
            let exec_in: u128 = attr(&res, "amount_in").parse().unwrap();
            assert_eq!(exec_out, want_out, "exact-out delivered != requested");
            assert_eq!(q.amount_in_consumed.u128(), exec_in, "quote-exact-out cost != exec (zfo={zfo}, out={want_out})");
        }
    }

    // ---------------------------------------------------------------------
    // REGIME 4: constant-price round-trip can never return more than deposited.
    // Mint then immediately burn the same liquidity at the unchanged price; the
    // returned principal (rounded DOWN) must be <= the deposited principal
    // (rounded UP) for BOTH tokens — no rounding-extraction of value.
    // ---------------------------------------------------------------------
    #[test]
    fn constant_price_roundtrip_never_profits() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        let mut st: u64 = 0xD15EA5E;
        let nxt = |s: &mut u64| { *s = s.wrapping_mul(6364136223846793005).wrapping_add(1); (*s >> 33) as u128 };
        for _ in 0..2000 {
            let lower = -2000 + 10 * (nxt(&mut st) % 400) as i32;
            let width = 10 * (1 + nxt(&mut st) % 50) as i32;
            let upper = lower + width;
            // Bias toward tiny liquidity where rounding bites hardest.
            let liq = 1 + nxt(&mut st) % 1_000;

            let mint = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
                ExecuteMsg::Mint { lower_tick: lower, upper_tick: upper, amount: Uint128::new(liq as u128) },
            );
            let mint = match mint { Ok(m) => m, Err(_) => continue };
            let din0: u128 = attr(&mint, "amount0_consumed").parse().unwrap();
            let din1: u128 = attr(&mint, "amount1_consumed").parse().unwrap();

            let burn = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[]),
                ExecuteMsg::Burn { lower_tick: lower, upper_tick: upper, amount: Uint128::new(liq as u128) },
            ).unwrap();
            let out0: u128 = attr(&burn, "amount0_burned").parse().unwrap();
            let out1: u128 = attr(&burn, "amount1_burned").parse().unwrap();

            assert!(out0 <= din0, "token0 extraction: deposited {din0} < withdrawn {out0} (range {lower}..{upper}, L {liq})");
            assert!(out1 <= din1, "token1 extraction: deposited {din1} < withdrawn {out1} (range {lower}..{upper}, L {liq})");

            // Collect to clear owed before next iteration.
            let _ = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[]),
                ExecuteMsg::Collect { recipient: lp.to_string(), lower_tick: lower, upper_tick: upper, amount0_requested: Uint128::MAX, amount1_requested: Uint128::MAX },
            );
        }
    }

    // ---------------------------------------------------------------------
    // REGIME 5: MAX_LIQUIDITY_PER_TICK is enforced exactly — cap mintable, one
    // over reverts, on BOTH the shared lower tick and via a second position.
    // ---------------------------------------------------------------------
    #[test]
    fn max_liquidity_per_tick_enforced() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // cap = u128::MAX / num_ticks; num_ticks = 2*floor(MAX_TICK/spacing)+1.
        let max_tick = 887272i32;
        let num_ticks = 2 * ((max_tick / 10) as u128) + 1;
        let cap = u128::MAX / num_ticks;

        // A 1-spacing range [0,10]; minting `cap` exactly must succeed.
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: 0, upper_tick: 10, amount: Uint128::new(cap) },
        )
        .expect("minting exactly the cap must succeed");

        // One more unit referencing tick 0 (as lower) must exceed the cap.
        let over = execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: 0, upper_tick: 20, amount: Uint128::new(1) },
        );
        assert!(over.is_err(), "minting one over MAX_LIQUIDITY_PER_TICK must revert");
        let msg = format!("{:?}", over.unwrap_err());
        assert!(msg.contains("MAX_LIQUIDITY_PER_TICK"), "wrong error: {msg}");
    }

    // ---------------------------------------------------------------------
    // REGIME 2/3: word-boundary tick crossing + extreme/full range lifecycle.
    // A swap crossing a tick at a bitmap word boundary (compressed ≡ 0 mod 256)
    // must actually apply that tick's liquidity. With spacing 10, tick 2560 is
    // compressed 256 = word 1, bit 0.
    // ---------------------------------------------------------------------
    #[test]
    fn word_boundary_cross_applies_liquidity() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        let boundary = 2560; // compressed 256
        // Position straddling current price (in range) so swaps execute.
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: -5120, upper_tick: 5120, amount: Uint128::new(500_000_000_000) },
        ).unwrap();
        // Position whose LOWER edge sits exactly on the word boundary, above price.
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: boundary, upper_tick: boundary + 1280, amount: Uint128::new(700_000_000_000) },
        ).unwrap();

        // Walk price UP in small one_for_zero steps. After EACH step assert the
        // exact active-L-from-tick formula (INV-ACTL). The decisive case: at any
        // landing tick inside [2560, 3840) the boundary position's +L MUST be
        // included — proving the word-boundary tick crossing applied its delta
        // (the historical bug skipped exactly word-aligned ticks).
        let mut tested_boundary = false;
        for _ in 0..40 {
            let res = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[Coin::new(Uint128::new(3_000_000_000), T1)]),
                ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
            ).unwrap();
            let final_tick: i32 = attr(&res, "final_tick").parse().unwrap();
            let slot0: choice_clmm_common::pool::PoolState = from_json(
                query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap()).unwrap();

            let mut expected = 0u128;
            if -5120 <= final_tick && final_tick < 5120 { expected += 500_000_000_000; }
            if boundary <= final_tick && final_tick < boundary + 1280 { expected += 700_000_000_000; }
            assert_eq!(
                slot0.liquidity.u128(), expected,
                "active L {} != expected {} at final_tick {} (boundary-cross liquidity mis-applied)",
                slot0.liquidity, expected, final_tick
            );
            if boundary <= final_tick && final_tick < boundary + 1280 {
                assert!(slot0.liquidity.u128() >= 700_000_000_000);
                tested_boundary = true;
            }
        }
        assert!(tested_boundary, "price never landed in the boundary position's range; resize the walk");
    }

    #[test]
    fn full_range_and_min_width_lifecycle() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // Full-range position (max ticks divisible by spacing).
        let lo = -887270;
        let hi = 887270;
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: lo, upper_tick: hi, amount: Uint128::new(1_000_000) },
        ).unwrap();
        // One-spacing-wide position straddling price.
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: 0, upper_tick: 10, amount: Uint128::new(1_000_000_000) },
        ).unwrap();

        // Swap both directions hard; must never panic and must stay solvent
        // (mock querier can't track bank balances, so we just require no error /
        // the swap to either fill or cleanly stop).
        for (denom, amt) in [(T0, 5_000_000u128), (T1, 9_000_000)] {
            let _ = execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[Coin::new(Uint128::new(amt), denom)]),
                ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
            );
        }
        // Burn both fully — must succeed and zero out.
        for (l, u, q) in [(lo, hi, 1_000_000u128), (0, 10, 1_000_000_000)] {
            execute(
                deps.as_mut(), env.clone(), message_info(&lp, &[]),
                ExecuteMsg::Burn { lower_tick: l, upper_tick: u, amount: Uint128::new(q) },
            ).unwrap();
        }
        let slot0: choice_clmm_common::pool::PoolState = from_json(
            query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap()).unwrap();
        assert_eq!(slot0.liquidity.u128(), 0, "active L not zero after full burn");
    }

    // ---------------------------------------------------------------------
    // REGIME 9: protocol-fee carve conservation. With a divisor n, the protocol
    // takes floor(fee/n) and LPs get the remainder; the swapper's cost is
    // unchanged and LPs are never short-changed beyond the floor. We bypass the
    // (factory-owner-gated) setter by writing PROTOCOL_FEE_CONFIG directly.
    // ---------------------------------------------------------------------
    #[test]
    fn protocol_fee_carve_conserves_and_never_shorts_lp() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // Enable a 25% (divisor 4) carve on token0 (zero_for_one) direction.
        let mut pfc = PROTOCOL_FEE_CONFIG.load(&deps.storage).unwrap();
        pfc.fee_protocol_0 = 4;
        pfc.fee_protocol_1 = 4;
        PROTOCOL_FEE_CONFIG.save(&mut deps.storage, &pfc).unwrap();

        // Single in-range position holds all liquidity.
        let (lower, upper, liq) = (-20000i32, 20000i32, 1_000_000_000_000u128);
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: lower, upper_tick: upper, amount: Uint128::new(liq) },
        ).unwrap();

        // One small zero_for_one swap (single step, no crossing) so the floor is
        // applied exactly once.
        let res = execute(
            deps.as_mut(), env.clone(), message_info(&lp, &[Coin::new(Uint128::new(5_000_000), T0)]),
            ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
        ).unwrap();
        let fee: u128 = attr(&res, "fee_amount").parse().unwrap();
        let protocol: u128 = attr(&res, "protocol_fee").parse().unwrap();
        assert!(fee > 0, "no fee charged (test ineffective)");

        // Carve is floor(fee/4), in the pool's favor (protocol <= fee/4).
        assert_eq!(protocol, fee / 4, "protocol carve != floor(fee/4)");
        // Accrued protocol fees match the carve exactly.
        let accrued = PROTOCOL_FEES_0.load(&deps.storage).unwrap().u128();
        assert_eq!(accrued, protocol, "PROTOCOL_FEES_0 != carved amount");

        // LP share = fee - protocol. Roll it onto the sole position and confirm
        // the LP is credited the full remainder (minus only /2^128 flooring), and
        // protocol + LP never exceeds the total fee the swapper paid.
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &[]),
            ExecuteMsg::Burn { lower_tick: lower, upper_tick: upper, amount: Uint128::zero() },
        ).unwrap();
        let owed = POSITIONS.load(&deps.storage, (lp.as_str(), lower, upper)).unwrap().tokens_owed_0.u128();
        let lp_share = fee - protocol;
        assert!(owed <= lp_share, "LP over-credited: {owed} > {lp_share}");
        assert!(lp_share - owed <= 2, "LP short-changed by {} (> rounding)", lp_share - owed);
        assert!(protocol + owed <= fee, "protocol+LP {} exceeds fee charged {}", protocol + owed, fee);
    }

    // ---------------------------------------------------------------------
    // REGIME 10: flash loan — reentrancy lock blocks mutators, repayment is
    // verified by balance delta, 1-wei underpay reverts. We drive the bank
    // balances directly via the mock querier.
    // ---------------------------------------------------------------------
    fn ok_reply() -> Reply {
        Reply {
            id: REPLY_FLASH,
            payload: Binary::default(),
            gas_used: 0,
            result: SubMsgResult::Ok(
                #[allow(deprecated)]
                SubMsgResponse { events: vec![], data: None, msg_responses: vec![] },
            ),
        }
    }

    const FLASH_HELD: u128 = 1_000_000;
    const FLASH_BORROW: u128 = 400_000;

    /// Build a pool mid-flash: mint in-range liquidity, set the pool's bank
    /// balance to `FLASH_HELD` of each token, then execute Flash (borrowing
    /// token0). Returns the deps/env/pool plus the loan fee. After this, the
    /// reentrancy lock is held and PENDING_FLASH is set — exactly the state a
    /// `reply` observes. (Each call is its own pool, so a failed `reply_flash`
    /// in one scenario can't bleed into another.)
    fn flashed_pool() -> (OwnedDeps<MemoryStorage, MockApi, MockQuerier>, Env, Addr, u128) {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        let pool = env.contract.address.clone();
        execute(
            deps.as_mut(), env.clone(), message_info(&lp, &big_coins()),
            ExecuteMsg::Mint { lower_tick: -20000, upper_tick: 20000, amount: Uint128::new(1_000_000_000_000) },
        ).unwrap();
        deps.querier.bank.update_balance(pool.clone(), vec![Coin::new(Uint128::new(FLASH_HELD), T0), Coin::new(Uint128::new(FLASH_HELD), T1)]);
        let res = execute(
            deps.as_mut(), env.clone(), message_info(&lp, &[]),
            ExecuteMsg::Flash { recipient: lp.to_string(), amount0: Uint128::new(FLASH_BORROW), amount1: Uint128::zero(), data: Binary::default() },
        ).unwrap();
        let fee0: u128 = attr(&res, "fee0").parse().unwrap();
        (deps, env, pool, fee0)
    }

    #[test]
    fn flash_lock_blocks_reentrancy() {
        let (mut fdeps, fenv, _pool, fee0) = flashed_pool();
        let lp = fdeps.api.addr_make("lp");
        assert!(fee0 > 0, "flash fee zero (test ineffective)");
        // Lock held mid-flash: every mutator entrypoint must reject.
        assert!(is_locked(&fdeps.storage), "reentrancy lock not held during flash");
        for msg in [
            ExecuteMsg::Mint { lower_tick: -100, upper_tick: 100, amount: Uint128::new(1) },
            ExecuteMsg::Burn { lower_tick: -20000, upper_tick: 20000, amount: Uint128::new(1) },
            ExecuteMsg::Collect { recipient: lp.to_string(), lower_tick: -20000, upper_tick: 20000, amount0_requested: Uint128::MAX, amount1_requested: Uint128::MAX },
            ExecuteMsg::Flash { recipient: lp.to_string(), amount0: Uint128::new(1), amount1: Uint128::zero(), data: Binary::default() },
            ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None },
        ] {
            let r = execute(fdeps.as_mut(), fenv.clone(), message_info(&lp, &[Coin::new(Uint128::new(1), T0)]), msg);
            assert!(matches!(r, Err(crate::error::ContractError::Reentrancy {})), "mutator not blocked mid-flash: {r:?}");
        }
    }

    #[test]
    fn flash_underpay_one_wei_reverts() {
        let (mut deps, env, pool, fee0) = flashed_pool();
        deps.querier.bank.update_balance(pool, vec![Coin::new(Uint128::new(FLASH_HELD + fee0 - 1), T0), Coin::new(Uint128::new(FLASH_HELD), T1)]);
        let bad = reply_flash(deps.as_mut(), env, ok_reply());
        assert!(matches!(bad, Err(crate::error::ContractError::FlashNotRepaid { .. })), "1-wei underpay accepted: {bad:?}");
    }

    #[test]
    fn flash_exact_repay_distributes_fee_and_clears_lock() {
        let (mut deps, env, pool, fee0) = flashed_pool();
        let fg_before = FEE_GROWTH_GLOBAL_0.may_load(&deps.storage).unwrap().unwrap_or_default();
        deps.querier.bank.update_balance(pool, vec![Coin::new(Uint128::new(FLASH_HELD + fee0), T0), Coin::new(Uint128::new(FLASH_HELD), T1)]);
        reply_flash(deps.as_mut(), env, ok_reply()).expect("exact repayment must succeed");
        assert!(!is_locked(&deps.storage), "lock not released after repayment");
        let fg_after = FEE_GROWTH_GLOBAL_0.load(&deps.storage).unwrap();
        assert!(fg_after > fg_before, "flash fee not distributed to LPs");
    }

    // ---------------------------------------------------------------------
    // MIS-ATTRIBUTION: two LPs sharing one range, both minted before any swap,
    // must be credited fees in EXACT proportion to their liquidity — one LP's
    // fees can never leak to another. Equal L ⇒ byte-equal fees.
    // ---------------------------------------------------------------------
    #[test]
    fn fee_attribution_proportional_between_lps() {
        for (la, lb) in [(1_000_000_000u128, 1_000_000_000u128), (1_000_000_000, 3_000_000_000)] {
            let (mut deps, env, _lp) = setup(10, 3000, 0);
            let a = deps.api.addr_make("alice");
            let b = deps.api.addr_make("bob");
            let (lower, upper) = (-20000i32, 20000i32);
            // Both mint the SAME range before any swap (inside_last == 0 for both).
            execute(deps.as_mut(), env.clone(), message_info(&a, &big_coins()),
                ExecuteMsg::Mint { lower_tick: lower, upper_tick: upper, amount: Uint128::new(la) }).unwrap();
            execute(deps.as_mut(), env.clone(), message_info(&b, &big_coins()),
                ExecuteMsg::Mint { lower_tick: lower, upper_tick: upper, amount: Uint128::new(lb) }).unwrap();

            // Swaps both directions, staying within the shared range.
            for (denom, amt) in [(T0, 7_000_000u128), (T1, 11_000_000), (T0, 3_000_000)] {
                execute(deps.as_mut(), env.clone(), message_info(&a, &[Coin::new(Uint128::new(amt), denom)]),
                    ExecuteMsg::SwapExactInput { minimum_amount_out: Uint128::zero(), recipient: None, deadline: None }).unwrap();
            }

            // Roll fees onto both positions.
            for who in [&a, &b] {
                execute(deps.as_mut(), env.clone(), message_info(who, &[]),
                    ExecuteMsg::Burn { lower_tick: lower, upper_tick: upper, amount: Uint128::zero() }).unwrap();
            }
            let pa = POSITIONS.load(&deps.storage, (a.as_str(), lower, upper)).unwrap();
            let pb = POSITIONS.load(&deps.storage, (b.as_str(), lower, upper)).unwrap();

            // owed_a / la == owed_b / lb  ⇒  owed_a * lb == owed_b * la (± rounding).
            for (oa, ob) in [(pa.tokens_owed_0.u128(), pb.tokens_owed_0.u128()), (pa.tokens_owed_1.u128(), pb.tokens_owed_1.u128())] {
                assert!(oa > 0 && ob > 0, "no fees credited (la={la}, lb={lb})");
                if la == lb {
                    assert_eq!(oa, ob, "equal-L LPs credited unequal fees: {oa} vs {ob}");
                } else {
                    // Cross-multiply; allow a small slack for independent flooring.
                    let lhs = oa as u128 * lb;
                    let rhs = ob as u128 * la;
                    let diff = lhs.abs_diff(rhs);
                    let slack = la.max(lb) * 2;
                    assert!(diff <= slack, "fee attribution not proportional: {oa}/{la} vs {ob}/{lb} (diff {diff} > {slack})");
                }
            }
        }
    }
}

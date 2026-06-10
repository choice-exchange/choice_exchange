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
        from_json, Addr, BankMsg, Binary, Coin, CosmosMsg, Env, MemoryStorage, OwnedDeps, Reply,
        Response, SubMsgResponse, SubMsgResult, Timestamp, Uint128, Uint256, WasmMsg,
    };

    const T0: &str = "uaaa";
    const T1: &str = "ubbb";

    fn native(d: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: d.to_string(),
        }
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
            .unwrap_or_else(|| panic!("attribute {} missing", key))
    }

    fn big_coins() -> Vec<Coin> {
        vec![Coin::new(Uint128::MAX, T0), Coin::new(Uint128::MAX, T1)]
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
    /// Zero the protocol-fee divisors (default-ON since the v3-table change)
    /// for tests that assert EXACT LP fee math with no carve.
    fn disable_protocol_carve(storage: &mut dyn cosmwasm_std::Storage) {
        let mut cfg = PROTOCOL_FEE_CONFIG.load(storage).unwrap();
        cfg.fee_protocol_0 = 0;
        cfg.fee_protocol_1 = 0;
        PROTOCOL_FEE_CONFIG.save(storage, &cfg).unwrap();
    }

    fn accrue_with_offset(offset: Uint256) -> (u128, u128, bool) {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // This test asserts byte-identical LP fee attribution across the U256
        // wrap; a protocol carve would just scale the fee, so pin it off.
        disable_protocol_carve(&mut deps.storage);
        let lower = -10000;
        let upper = 10000;
        let liq: u128 = 1_000_000_000_000; // 1e12

        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: lower,
                upper_tick: upper,
                amount: Uint128::new(liq),
            },
        )
        .unwrap();

        // Pre-load the fee-growth origin. The position was minted with global==0
        // so its `fee_growth_inside_0_last == 0` and the lower tick's
        // `fee_growth_outside_0 == 0`; to keep the *baseline* inside == 0 we shift
        // BOTH global and the lower tick's outside by the same offset.
        if !offset.is_zero() {
            FEE_GROWTH_GLOBAL_0
                .save(&mut deps.storage, &offset)
                .unwrap();
            let mut tl = TICKS.load(&deps.storage, lower).unwrap();
            tl.fee_growth_outside_0 = offset;
            TICKS.save(&mut deps.storage, lower, &tl).unwrap();
        }
        let global_before = FEE_GROWTH_GLOBAL_0
            .may_load(&deps.storage)
            .unwrap()
            .unwrap_or_default();

        // One token0->token1 swap, staying within the range (small relative to L).
        let funds = vec![Coin::new(Uint128::new(5_000_000), T0)];
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &funds),
            ExecuteMsg::SwapExactInput {
                minimum_amount_out: Uint128::zero(),
                recipient: None,
                deadline: None,
            },
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
            ExecuteMsg::Burn {
                lower_tick: lower,
                upper_tick: upper,
                amount: Uint128::zero(),
            },
        )
        .unwrap();
        let pos = POSITIONS
            .load(&deps.storage, (lp.as_str(), lower, upper))
            .unwrap();
        (pos.tokens_owed_0.u128(), fee_amount, wrapped)
    }

    #[test]
    fn fee_growth_wraparound_attribution_exact() {
        let (owed_ctrl, fee_ctrl, wrapped_ctrl) = accrue_with_offset(Uint256::zero());
        let (owed_wrap, fee_wrap, wrapped) = accrue_with_offset(Uint256::MAX);

        // The control path must NOT wrap; the offset path MUST.
        assert!(!wrapped_ctrl, "control run unexpectedly wrapped");
        assert!(
            wrapped,
            "offset run did not wrap the U256 accumulator (test ineffective)"
        );

        // Same swap ⇒ same fee charged, regardless of accumulator origin.
        assert_eq!(fee_ctrl, fee_wrap, "fee charged differs across wrap");
        // The crown jewel: fee ATTRIBUTION is byte-identical across the wrap.
        assert_eq!(
            owed_ctrl, owed_wrap,
            "fee attribution diverged across U256 wrap: control {owed_ctrl} vs wrapped {owed_wrap}"
        );
        // And the position is never over-credited vs the fee actually paid
        // (flooring by /2^128 means it may be a hair under).
        assert!(
            owed_wrap <= fee_wrap,
            "over-credit: owed {} > fee {}",
            owed_wrap,
            fee_wrap
        );
        assert!(owed_wrap > 0, "position credited nothing");
        // Flooring loss is bounded by 1 unit of (2^128 / L) rounded — in practice
        // the gap is tiny; assert it's within a small absolute slack.
        assert!(
            fee_wrap - owed_wrap <= 2,
            "attribution lost {} (> rounding)",
            fee_wrap - owed_wrap
        );
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
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: -20000,
                    upper_tick: 20000,
                    amount: Uint128::new(1_000_000_000_000),
                },
            )
            .unwrap();
            // Narrow position above current price that a one_for_zero swap crosses.
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: 1000,
                    upper_tick: 2000,
                    amount: Uint128::new(500_000_000_000),
                },
            )
            .unwrap();

            if !offset.is_zero() {
                // Shift global + the in-range lower tick (-20000) outside so the
                // wide position's baseline inside stays 0.
                FEE_GROWTH_GLOBAL_0
                    .save(&mut deps.storage, &offset)
                    .unwrap();
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
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: -2000,
                    upper_tick: -1000,
                    amount: Uint128::new(500_000_000_000),
                },
            )
            .unwrap();

            // Large zero_for_one swap: price falls, crossing -1000 then into
            // [-2000,-1000]; accrues fee to fg0 and recomputes outside at -1000.
            let funds = vec![Coin::new(Uint128::new(80_000_000_000), T0)];
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &funds),
                ExecuteMsg::SwapExactInput {
                    minimum_amount_out: Uint128::zero(),
                    recipient: None,
                    deadline: None,
                },
            )
            .unwrap();

            // Roll fees on the always-in-range wide position.
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[]),
                ExecuteMsg::Burn {
                    lower_tick: -20000,
                    upper_tick: 20000,
                    amount: Uint128::zero(),
                },
            )
            .unwrap();
            POSITIONS
                .load(&deps.storage, (lp.as_str(), -20000, 20000))
                .unwrap()
                .tokens_owed_0
                .u128()
        }
        let ctrl = run(Uint256::zero());
        let wrap = run(Uint256::MAX);
        assert_eq!(
            ctrl, wrap,
            "crossing fee attribution diverged across U256 wrap: {ctrl} vs {wrap}"
        );
        assert!(ctrl > 0, "no fee credited (test ineffective)");
    }

    // ---------------------------------------------------------------------
    // REGIME 8: Quote must equal execution exactly (fee pinned via vol_mult=0).
    // ---------------------------------------------------------------------
    #[test]
    fn quote_matches_execution_exactly() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // Several positions across overlapping ranges for a non-trivial curve.
        for (l, u, q) in [
            (-30000i32, 30000i32, 800_000_000_000u128),
            (-5000, 5000, 400_000_000_000),
            (1000, 9000, 200_000_000_000),
        ] {
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: l,
                    upper_tick: u,
                    amount: Uint128::new(q),
                },
            )
            .unwrap();
        }

        let mut st: u64 = 0xC0FFEE;
        let nxt = |s: &mut u64| {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (*s >> 33) as u128
        };
        for _ in 0..400 {
            let zfo = (nxt(&mut st) & 1) == 0;
            let amt = 1 + nxt(&mut st) % 2_000_000_000;
            let token_in = if zfo { native(T0) } else { native(T1) };

            // Exact-input quote vs execute.
            let q: QuoteResponse = from_json(
                query(
                    deps.as_ref(),
                    env.clone(),
                    QueryMsg::Quote {
                        token_in: token_in.clone(),
                        amount_in: Uint128::new(amt),
                    },
                )
                .unwrap(),
            )
            .unwrap();
            let denom = if zfo { T0 } else { T1 };
            let funds = vec![Coin::new(Uint128::new(amt), denom)];
            let res = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &funds),
                ExecuteMsg::SwapExactInput {
                    minimum_amount_out: Uint128::zero(),
                    recipient: None,
                    deadline: None,
                },
            )
            .unwrap();
            let exec_out: u128 = attr(&res, "amount_out").parse().unwrap();
            let exec_in: u128 = attr(&res, "amount_in").parse().unwrap();
            assert_eq!(
                q.amount_out.u128(),
                exec_out,
                "quote amount_out != exec (zfo={zfo}, amt={amt})"
            );
            assert_eq!(
                q.amount_in_consumed.u128(),
                exec_in,
                "quote amount_in != exec (zfo={zfo}, amt={amt})"
            );
        }
    }

    #[test]
    fn quote_exact_output_matches_execution() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        for (l, u, q) in [
            (-30000i32, 30000i32, 800_000_000_000u128),
            (-8000, 8000, 300_000_000_000),
        ] {
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: l,
                    upper_tick: u,
                    amount: Uint128::new(q),
                },
            )
            .unwrap();
        }
        let mut st: u64 = 0xBEEF;
        let nxt = |s: &mut u64| {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (*s >> 33) as u128
        };
        for _ in 0..300 {
            let zfo = (nxt(&mut st) & 1) == 0;
            let want_out = 1 + nxt(&mut st) % 100_000_000;
            // token_out is token1 when zero_for_one.
            let token_out = if zfo { native(T1) } else { native(T0) };
            let q: QuoteResponse = from_json(
                query(
                    deps.as_ref(),
                    env.clone(),
                    QueryMsg::QuoteExactOutput {
                        token_out,
                        amount_out: Uint128::new(want_out),
                    },
                )
                .unwrap(),
            )
            .unwrap();
            // If the quote can't deliver the full output (liquidity bound) skip.
            if q.amount_out.u128() < want_out {
                continue;
            }
            let in_denom = if zfo { T0 } else { T1 };
            let res = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[Coin::new(Uint128::MAX, in_denom)]),
                ExecuteMsg::SwapExactOutput {
                    zero_for_one: zfo,
                    amount_out: Uint128::new(want_out),
                    maximum_amount_in: Uint128::MAX,
                    recipient: None,
                    deadline: None,
                },
            )
            .unwrap();
            let exec_out: u128 = attr(&res, "amount_out").parse().unwrap();
            let exec_in: u128 = attr(&res, "amount_in").parse().unwrap();
            assert_eq!(exec_out, want_out, "exact-out delivered != requested");
            assert_eq!(
                q.amount_in_consumed.u128(),
                exec_in,
                "quote-exact-out cost != exec (zfo={zfo}, out={want_out})"
            );
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
        let nxt = |s: &mut u64| {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            (*s >> 33) as u128
        };
        for _ in 0..2000 {
            let lower = -2000 + 10 * (nxt(&mut st) % 400) as i32;
            let width = 10 * (1 + nxt(&mut st) % 50) as i32;
            let upper = lower + width;
            // Bias toward tiny liquidity where rounding bites hardest.
            let liq = 1 + nxt(&mut st) % 1_000;

            let mint = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: lower,
                    upper_tick: upper,
                    amount: Uint128::new(liq),
                },
            );
            let mint = match mint {
                Ok(m) => m,
                Err(_) => continue,
            };
            let din0: u128 = attr(&mint, "amount0_consumed").parse().unwrap();
            let din1: u128 = attr(&mint, "amount1_consumed").parse().unwrap();

            let burn = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[]),
                ExecuteMsg::Burn {
                    lower_tick: lower,
                    upper_tick: upper,
                    amount: Uint128::new(liq),
                },
            )
            .unwrap();
            let out0: u128 = attr(&burn, "amount0_burned").parse().unwrap();
            let out1: u128 = attr(&burn, "amount1_burned").parse().unwrap();

            assert!(
                out0 <= din0,
                "token0 extraction: deposited {} < withdrawn {} (range {}..{}, L {})",
                din0,
                out0,
                lower,
                upper,
                liq
            );
            assert!(
                out1 <= din1,
                "token1 extraction: deposited {} < withdrawn {} (range {}..{}, L {})",
                din1,
                out1,
                lower,
                upper,
                liq
            );

            // Collect to clear owed before next iteration.
            let _ = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[]),
                ExecuteMsg::Collect {
                    recipient: lp.to_string(),
                    lower_tick: lower,
                    upper_tick: upper,
                    amount0_requested: Uint128::MAX,
                    amount1_requested: Uint128::MAX,
                },
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
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: 0,
                upper_tick: 10,
                amount: Uint128::new(cap),
            },
        )
        .expect("minting exactly the cap must succeed");

        // One more unit referencing tick 0 (as lower) must exceed the cap.
        let over = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: 0,
                upper_tick: 20,
                amount: Uint128::new(1),
            },
        );
        assert!(
            over.is_err(),
            "minting one over MAX_LIQUIDITY_PER_TICK must revert"
        );
        let msg = format!("{:?}", over.unwrap_err());
        assert!(
            msg.contains("MAX_LIQUIDITY_PER_TICK"),
            "wrong error: {}",
            msg
        );
    }

    // ---------------------------------------------------------------------
    // REGIME 5b (Item 2): the per-tick cap is computed from the spacing, so it
    // is a DIFFERENT value at each fee tier's spacing. Re-prove "mint exactly the
    // cap succeeds, one over reverts" at spacings 1 / 60 / 200 — the spacing-10
    // test above masks a cap formula that ignores spacing (the cap at spacing 1
    // is ~20x smaller than at spacing 10, so a spacing-blind formula would let an
    // over-cap mint through at spacing 1 and per-tick u128 math could overflow).
    // ---------------------------------------------------------------------
    #[test]
    fn max_liquidity_per_tick_enforced_per_spacing() {
        let max_tick = 887272i32;
        for spacing in [1u32, 60, 200] {
            let s = spacing as i32;
            // Exact contract formula: 2*floor(MAX_TICK/spacing)+1 (symmetric).
            let num_ticks = 2 * ((max_tick / s) as u128) + 1;
            let cap = u128::MAX / num_ticks;

            let (mut deps, env, lp) = setup(spacing, 3000, 0);
            // Range [0, spacing] (one spacing wide, above current price tick 0).
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: 0,
                    upper_tick: s,
                    amount: Uint128::new(cap),
                },
            )
            .unwrap_or_else(|e| {
                panic!(
                    "spacing {}: minting exactly the cap must succeed: {:?}",
                    spacing, e
                )
            });

            // One more unit referencing tick 0 as lower must exceed the cap.
            let over = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: 0,
                    upper_tick: 2 * s,
                    amount: Uint128::new(1),
                },
            );
            assert!(
                over.is_err(),
                "spacing {}: minting one over the cap must revert",
                spacing
            );
            let msg = format!("{:?}", over.unwrap_err());
            assert!(
                msg.contains("MAX_LIQUIDITY_PER_TICK"),
                "spacing {}: wrong error: {}",
                spacing,
                msg
            );
        }
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
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: -5120,
                upper_tick: 5120,
                amount: Uint128::new(500_000_000_000),
            },
        )
        .unwrap();
        // Position whose LOWER edge sits exactly on the word boundary, above price.
        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: boundary,
                upper_tick: boundary + 1280,
                amount: Uint128::new(700_000_000_000),
            },
        )
        .unwrap();

        // Walk price UP in small one_for_zero steps. After EACH step assert the
        // exact active-L-from-tick formula (INV-ACTL). The decisive case: at any
        // landing tick inside [2560, 3840) the boundary position's +L MUST be
        // included — proving the word-boundary tick crossing applied its delta
        // (the historical bug skipped exactly word-aligned ticks).
        let mut tested_boundary = false;
        for _ in 0..40 {
            let res = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[Coin::new(Uint128::new(3_000_000_000), T1)]),
                ExecuteMsg::SwapExactInput {
                    minimum_amount_out: Uint128::zero(),
                    recipient: None,
                    deadline: None,
                },
            )
            .unwrap();
            let final_tick: i32 = attr(&res, "final_tick").parse().unwrap();
            let slot0: choice_clmm_common::pool::PoolState =
                from_json(query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap())
                    .unwrap();

            let mut expected = 0u128;
            if (-5120..5120).contains(&final_tick) {
                expected += 500_000_000_000;
            }
            if boundary <= final_tick && final_tick < boundary + 1280 {
                expected += 700_000_000_000;
            }
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
        assert!(
            tested_boundary,
            "price never landed in the boundary position's range; resize the walk"
        );
    }

    #[test]
    fn full_range_and_min_width_lifecycle() {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // Full-range position (max ticks divisible by spacing).
        let lo = -887270;
        let hi = 887270;
        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: lo,
                upper_tick: hi,
                amount: Uint128::new(1_000_000),
            },
        )
        .unwrap();
        // One-spacing-wide position straddling price.
        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: 0,
                upper_tick: 10,
                amount: Uint128::new(1_000_000_000),
            },
        )
        .unwrap();

        // Swap both directions hard. Each leg must actually FILL (non-zero output)
        // and move the price in the correct direction — not merely "not error".
        // A bug that mis-applies full-range liquidity and silently no-ops the swap
        // would pass a bare `let _ = execute(...)` but is caught here.
        for (denom, amt, zero_for_one) in [(T0, 5_000_000u128, true), (T1, 9_000_000u128, false)] {
            let before: choice_clmm_common::pool::PoolState =
                from_json(query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap())
                    .unwrap();
            let res = execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[Coin::new(Uint128::new(amt), denom)]),
                ExecuteMsg::SwapExactInput {
                    minimum_amount_out: Uint128::zero(),
                    recipient: None,
                    deadline: None,
                },
            )
            .unwrap();
            let out = res
                .attributes
                .iter()
                .find(|a| a.key == "amount_out")
                .expect("swap must emit amount_out")
                .value
                .parse::<u128>()
                .unwrap();
            assert!(out > 0, "swap of {} {} produced no output", amt, denom);
            let after: choice_clmm_common::pool::PoolState =
                from_json(query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap())
                    .unwrap();
            if zero_for_one {
                assert!(
                    after.sqrt_price < before.sqrt_price,
                    "zero_for_one swap must push the price down ({} -> {})",
                    before.sqrt_price,
                    after.sqrt_price
                );
            } else {
                assert!(
                    after.sqrt_price > before.sqrt_price,
                    "one_for_zero swap must push the price up ({} -> {})",
                    before.sqrt_price,
                    after.sqrt_price
                );
            }
        }
        // Burn both fully — must succeed and zero out.
        for (l, u, q) in [(lo, hi, 1_000_000u128), (0, 10, 1_000_000_000)] {
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&lp, &[]),
                ExecuteMsg::Burn {
                    lower_tick: l,
                    upper_tick: u,
                    amount: Uint128::new(q),
                },
            )
            .unwrap();
        }
        let slot0: choice_clmm_common::pool::PoolState =
            from_json(query(deps.as_ref(), env.clone(), QueryMsg::GetSlot0 {}).unwrap()).unwrap();
        assert_eq!(
            slot0.liquidity.u128(),
            0,
            "active L not zero after full burn"
        );
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
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: lower,
                upper_tick: upper,
                amount: Uint128::new(liq),
            },
        )
        .unwrap();

        // One small zero_for_one swap (single step, no crossing) so the floor is
        // applied exactly once.
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &[Coin::new(Uint128::new(5_000_000), T0)]),
            ExecuteMsg::SwapExactInput {
                minimum_amount_out: Uint128::zero(),
                recipient: None,
                deadline: None,
            },
        )
        .unwrap();
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
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &[]),
            ExecuteMsg::Burn {
                lower_tick: lower,
                upper_tick: upper,
                amount: Uint128::zero(),
            },
        )
        .unwrap();
        let owed = POSITIONS
            .load(&deps.storage, (lp.as_str(), lower, upper))
            .unwrap()
            .tokens_owed_0
            .u128();
        let lp_share = fee - protocol;
        assert!(
            owed <= lp_share,
            "LP over-credited: {} > {}",
            owed,
            lp_share
        );
        assert!(
            lp_share - owed <= 2,
            "LP short-changed by {} (> rounding)",
            lp_share - owed
        );
        assert!(
            protocol + owed <= fee,
            "protocol+LP {} exceeds fee charged {}",
            protocol + owed,
            fee
        );
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
                SubMsgResponse {
                    events: vec![],
                    data: None,
                    msg_responses: vec![],
                },
            ),
        }
    }

    const FLASH_HELD: u128 = 1_000_000;
    const FLASH_BORROW: u128 = 400_000;
    /// Active in-range liquidity minted by `flashed_pool` (used to derive the
    /// exact LP fee-growth delta in `flash_exact_repay_distributes_fee_and_clears_lock`).
    const FLASH_LIQUIDITY: u128 = 1_000_000_000_000;

    /// Build a pool mid-flash: mint in-range liquidity, set the pool's bank
    /// balance to `FLASH_HELD` of each token, then execute Flash (borrowing
    /// token0). Returns the deps/env/pool plus the loan fee. After this, the
    /// reentrancy lock is held and PENDING_FLASH is set — exactly the state a
    /// `reply` observes. (Each call is its own pool, so a failed `reply_flash`
    /// in one scenario can't bleed into another.)
    fn flashed_pool() -> (
        OwnedDeps<MemoryStorage, MockApi, MockQuerier>,
        Env,
        Addr,
        u128,
    ) {
        let (mut deps, env, lp) = setup(10, 3000, 0);
        // This helper's callers assert the WHOLE flash fee goes to LPs.
        disable_protocol_carve(&mut deps.storage);
        let pool = env.contract.address.clone();
        // `execute_flash` live-queries the factory's flash-borrower allowlist;
        // answer authorized so this test exercises the loan path.
        deps.querier.update_wasm(|q| {
            use cosmwasm_std::{ContractResult as CR, SystemResult as SR};
            match q {
                cosmwasm_std::WasmQuery::Smart { msg, .. } => {
                    use choice_clmm_common::factory::QueryMsg as FQ;
                    match cosmwasm_std::from_json::<FQ>(msg) {
                        Ok(FQ::IsFlashBorrower { .. }) => {
                            let resp = choice_clmm_common::factory::IsFlashBorrowerResponse {
                                authorized: true,
                            };
                            SR::Ok(CR::Ok(cosmwasm_std::to_json_binary(&resp).unwrap()))
                        }
                        _ => SR::Ok(CR::Ok(Default::default())),
                    }
                }
                _ => SR::Ok(CR::Ok(Default::default())),
            }
        });
        execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: -20000,
                upper_tick: 20000,
                amount: Uint128::new(FLASH_LIQUIDITY),
            },
        )
        .unwrap();
        deps.querier.bank.update_balance(
            pool.clone(),
            vec![
                Coin::new(Uint128::new(FLASH_HELD), T0),
                Coin::new(Uint128::new(FLASH_HELD), T1),
            ],
        );
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&lp, &[]),
            ExecuteMsg::Flash {
                recipient: lp.to_string(),
                amount0: Uint128::new(FLASH_BORROW),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();
        let fee0: u128 = attr(&res, "fee0").parse().unwrap();
        (deps, env, pool, fee0)
    }

    #[test]
    fn flash_lock_blocks_reentrancy() {
        let (mut fdeps, fenv, _pool, fee0) = flashed_pool();
        let lp = fdeps.api.addr_make("lp");
        assert!(fee0 > 0, "flash fee zero (test ineffective)");
        // Lock held mid-flash: every mutator entrypoint must reject.
        assert!(
            is_locked(&fdeps.storage),
            "reentrancy lock not held during flash"
        );
        for msg in [
            ExecuteMsg::Mint {
                lower_tick: -100,
                upper_tick: 100,
                amount: Uint128::new(1),
            },
            ExecuteMsg::Burn {
                lower_tick: -20000,
                upper_tick: 20000,
                amount: Uint128::new(1),
            },
            ExecuteMsg::Collect {
                recipient: lp.to_string(),
                lower_tick: -20000,
                upper_tick: 20000,
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
            ExecuteMsg::Flash {
                recipient: lp.to_string(),
                amount0: Uint128::new(1),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
            ExecuteMsg::SwapExactInput {
                minimum_amount_out: Uint128::zero(),
                recipient: None,
                deadline: None,
            },
        ] {
            let r = execute(
                fdeps.as_mut(),
                fenv.clone(),
                message_info(&lp, &[Coin::new(Uint128::new(1), T0)]),
                msg,
            );
            assert!(
                matches!(r, Err(crate::error::ContractError::Reentrancy {})),
                "mutator not blocked mid-flash: {:?}",
                r
            );
        }
    }

    #[test]
    fn flash_underpay_one_wei_reverts() {
        let (mut deps, env, pool, fee0) = flashed_pool();
        deps.querier.bank.update_balance(
            pool,
            vec![
                Coin::new(Uint128::new(FLASH_HELD + fee0 - 1), T0),
                Coin::new(Uint128::new(FLASH_HELD), T1),
            ],
        );
        let bad = reply_flash(deps.as_mut(), env, ok_reply());
        assert!(
            matches!(bad, Err(crate::error::ContractError::FlashNotRepaid { .. })),
            "1-wei underpay accepted: {:?}",
            bad
        );
    }

    #[test]
    fn flash_exact_repay_distributes_fee_and_clears_lock() {
        let (mut deps, env, pool, fee0) = flashed_pool();
        let fg_before = FEE_GROWTH_GLOBAL_0
            .may_load(&deps.storage)
            .unwrap()
            .unwrap_or_default();
        deps.querier.bank.update_balance(
            pool,
            vec![
                Coin::new(Uint128::new(FLASH_HELD + fee0), T0),
                Coin::new(Uint128::new(FLASH_HELD), T1),
            ],
        );
        let protocol_before = PROTOCOL_FEES_0
            .may_load(&deps.storage)
            .unwrap()
            .unwrap_or_default();
        reply_flash(deps.as_mut(), env, ok_reply()).expect("exact repayment must succeed");
        assert!(
            !is_locked(&deps.storage),
            "lock not released after repayment"
        );
        let fg_after = FEE_GROWTH_GLOBAL_0.load(&deps.storage).unwrap();
        // The whole fee is the LP share (no protocol carve configured here), and
        // `accrue_flash_fee` adds `mul_div(fee0, 2^128, liquidity)` to the global
        // accumulator. Assert that EXACT delta — a bug that distributes a wrong
        // fraction (1 wei, or routes most to the protocol bucket) would still
        // satisfy a bare `fg_after > fg_before` but is caught here.
        let q128 = Uint256::one() << 128u32;
        let expected_delta = Uint256::from(fee0) * q128 / Uint256::from(FLASH_LIQUIDITY);
        assert_eq!(
            fg_after.wrapping_sub(fg_before),
            expected_delta,
            "flash LP fee delta mismatch: fee0={} L={}",
            fee0,
            FLASH_LIQUIDITY,
        );
        // And with no carve, the protocol bucket must be untouched.
        let protocol_after = PROTOCOL_FEES_0
            .may_load(&deps.storage)
            .unwrap()
            .unwrap_or_default();
        assert_eq!(
            protocol_after, protocol_before,
            "no protocol carve configured, yet the protocol bucket changed"
        );
    }

    // ---------------------------------------------------------------------
    // MIS-ATTRIBUTION: two LPs sharing one range, both minted before any swap,
    // must be credited fees in EXACT proportion to their liquidity — one LP's
    // fees can never leak to another. Equal L ⇒ byte-equal fees.
    // ---------------------------------------------------------------------
    #[test]
    fn fee_attribution_proportional_between_lps() {
        for (la, lb) in [
            (1_000_000_000u128, 1_000_000_000u128),
            (1_000_000_000, 3_000_000_000),
        ] {
            let (mut deps, env, _lp) = setup(10, 3000, 0);
            let a = deps.api.addr_make("alice");
            let b = deps.api.addr_make("bob");
            let (lower, upper) = (-20000i32, 20000i32);
            // Both mint the SAME range before any swap (inside_last == 0 for both).
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&a, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: lower,
                    upper_tick: upper,
                    amount: Uint128::new(la),
                },
            )
            .unwrap();
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&b, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: lower,
                    upper_tick: upper,
                    amount: Uint128::new(lb),
                },
            )
            .unwrap();

            // Swaps both directions, staying within the shared range.
            for (denom, amt) in [(T0, 7_000_000u128), (T1, 11_000_000), (T0, 3_000_000)] {
                execute(
                    deps.as_mut(),
                    env.clone(),
                    message_info(&a, &[Coin::new(Uint128::new(amt), denom)]),
                    ExecuteMsg::SwapExactInput {
                        minimum_amount_out: Uint128::zero(),
                        recipient: None,
                        deadline: None,
                    },
                )
                .unwrap();
            }

            // Roll fees onto both positions.
            for who in [&a, &b] {
                execute(
                    deps.as_mut(),
                    env.clone(),
                    message_info(who, &[]),
                    ExecuteMsg::Burn {
                        lower_tick: lower,
                        upper_tick: upper,
                        amount: Uint128::zero(),
                    },
                )
                .unwrap();
            }
            let pa = POSITIONS
                .load(&deps.storage, (a.as_str(), lower, upper))
                .unwrap();
            let pb = POSITIONS
                .load(&deps.storage, (b.as_str(), lower, upper))
                .unwrap();

            // owed_a / la == owed_b / lb  ⇒  owed_a * lb == owed_b * la (± rounding).
            for (oa, ob) in [
                (pa.tokens_owed_0.u128(), pb.tokens_owed_0.u128()),
                (pa.tokens_owed_1.u128(), pb.tokens_owed_1.u128()),
            ] {
                assert!(oa > 0 && ob > 0, "no fees credited (la={}, lb={})", la, lb);
                if la == lb {
                    assert_eq!(oa, ob, "equal-L LPs credited unequal fees: {oa} vs {ob}");
                } else {
                    // Cross-multiply; allow a small slack for independent flooring.
                    let lhs = oa * lb;
                    let rhs = ob * la;
                    let diff = lhs.abs_diff(rhs);
                    let slack = la.max(lb) * 2;
                    assert!(
                        diff <= slack,
                        "fee attribution not proportional: {}/{} vs {}/{} (diff {} > {})",
                        oa,
                        la,
                        ob,
                        lb,
                        diff,
                        slack
                    );
                }
            }
        }
    }

    // =====================================================================
    // Item 4: dynamic-fee / EMA rate-limit dynamics across multiple blocks.
    //
    // The existing oracle unit tests pin the *pure* `compute_raw_dynamic_fee`.
    // What they do NOT exercise is the stateful, time-advancing path through
    // `update_oracle_and_fee`: the same-block fee pin, the per-second change
    // clamp across varying `block.time`, and the [base, max] envelope under an
    // adversary trying to jerk the next victim's fee with one manipulated swap.
    // We drive real swaps through `execute`, advance `env.block.time` between
    // them, and read the actually-charged `fee_ppm` attribute, reconstructing
    // the rate-limit bound independently.
    // =====================================================================

    fn next(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Instantiate a pool with FULL `FeeConfig` control (the shared `setup`
    /// hardcodes `max_fee_change_per_second_ppm = 0`).
    #[allow(clippy::too_many_arguments)]
    fn setup_dyn(
        base_fee: u32,
        max_fee: u32,
        vol_mult: u32,
        halflife: u64,
        rate_ppm: u32,
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
                tick_spacing: 10,
                fee_config: FeeConfig {
                    base_fee_ppm: base_fee,
                    max_fee_ppm: max_fee,
                    volatility_multiplier: vol_mult,
                    ema_halflife_seconds: halflife,
                    max_fee_change_per_second_ppm: rate_ppm,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();
        let lp = deps.api.addr_make("lp");
        (deps, mock_env(), lp)
    }

    fn swap_read_fee(
        deps: &mut OwnedDeps<MemoryStorage, MockApi, MockQuerier>,
        env: &Env,
        who: &Addr,
        zero_for_one: bool,
        amt: u128,
    ) -> Option<u32> {
        let denom = if zero_for_one { T0 } else { T1 };
        let funds = vec![Coin::new(Uint128::new(amt), denom)];
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(who, &funds),
            ExecuteMsg::SwapExactInput {
                minimum_amount_out: Uint128::zero(),
                recipient: None,
                deadline: None,
            },
        )
        .ok()?;
        Some(attr(&res, "fee_ppm").parse().unwrap())
    }

    /// The time-advancing fuzzer. Reconstructs the rate-limit / same-block-pin /
    /// envelope bounds from an independent model of the oracle's committed fee
    /// and commit-time, and asserts the contract's charged fee obeys all three
    /// after every swap.
    #[test]
    fn dynamic_fee_rate_limit_fuzz() {
        const BASE: u32 = 3_000;
        const MAX: u32 = 30_000;
        const VOL: u32 = 1_000_000; // high volatility amplification
        const HALFLIFE: u64 = 600;
        const RATE: u32 = 50; // ppm per second
        const SEEDS: u64 = 40;
        const STEPS: usize = 120;

        for sd in 0..SEEDS {
            let (mut deps, base_env, lp) = setup_dyn(BASE, MAX, VOL, HALFLIFE, RATE);
            // Wide in-range position so swaps always succeed and price can drift.
            execute(
                deps.as_mut(),
                base_env.clone(),
                message_info(&lp, &big_coins()),
                ExecuteMsg::Mint {
                    lower_tick: -50_000,
                    upper_tick: 50_000,
                    amount: Uint128::new(1_000_000_000_000),
                },
            )
            .unwrap();
            let trader = deps.api.addr_make("trader");

            let t0 = base_env.block.time.seconds();
            // Independent model of the oracle's committed state.
            let mut committed_fee: u32 = BASE;
            let mut committed_t: u64 = t0;
            let mut now: u64 = t0;

            let mut st = 0xF7E5_0000u64.wrapping_add(sd.wrapping_mul(0x9E37_79B9));
            for step in 0..STEPS {
                // Δt biased to include 0 (same block), small, ~halflife, and large.
                let dt = match next(&mut st) % 5 {
                    0 => 0u64,
                    1 => 1 + next(&mut st) % 5,
                    2 => 1 + next(&mut st) % 60,
                    3 => 1 + next(&mut st) % 700,
                    _ => 1 + next(&mut st) % 5000,
                };
                now += dt;
                let mut env = base_env.clone();
                env.block.time = Timestamp::from_seconds(now);

                let zero_for_one = (next(&mut st) & 1) == 0;
                let amt = 1_000_000 + next(&mut st) % 800_000_000;
                let fee = match swap_read_fee(&mut deps, &env, &trader, zero_for_one, amt as u128) {
                    Some(f) => f,
                    None => continue, // swap rejected (e.g. price limit) — no commit
                };

                // (1) Envelope: the charged fee never escapes [base, max].
                assert!(
                    (BASE..=MAX).contains(&fee),
                    "sd {} step {}: fee {} out of [{}, {}]",
                    sd,
                    step,
                    fee,
                    BASE,
                    MAX
                );

                let delta = now - committed_t;
                if delta == 0 {
                    // (2) Same-block pin: every swap in a block pays the fee the
                    // first swap of that block committed. No re-derivation.
                    assert_eq!(
                        fee, committed_fee,
                        "sd {} step {}: same-block fee {} != committed {} (anti-manipulation pin broken)",
                        sd, step, fee, committed_fee
                    );
                    // No commit happens on a same-block swap; model unchanged.
                } else {
                    // (3) Rate limit: the committed fee moved at most RATE·Δt from
                    // the previously committed fee. A single swap cannot jerk the
                    // fee further than the elapsed-time budget allows.
                    let max_change = RATE.saturating_mul(delta.min(u32::MAX as u64) as u32);
                    let moved = fee.abs_diff(committed_fee);
                    assert!(
                        moved <= max_change,
                        "sd {} step {}: fee moved {} ppm in {}s (> RATE·Δt = {}); committed {} -> {}",
                        sd, step, moved, delta, max_change, committed_fee, fee
                    );
                    committed_fee = fee;
                    committed_t = now;
                }
            }
        }
    }

    /// Focused same-block pin: many swaps at the SAME `block.time` (after one
    /// time-advancing swap commits a fee) all pay that identical fee, even as
    /// price moves within the block. A mid-block re-derivation would let an
    /// attacker dodge the rate limit by trading within one block.
    #[test]
    fn dynamic_fee_same_block_pin() {
        let (mut deps, base_env, lp) = setup_dyn(3_000, 30_000, 1_000_000, 600, 50);
        execute(
            deps.as_mut(),
            base_env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: -50_000,
                upper_tick: 50_000,
                amount: Uint128::new(1_000_000_000_000),
            },
        )
        .unwrap();
        let trader = deps.api.addr_make("trader");

        // Advance time and commit a fee at block time t1.
        let t1 = base_env.block.time.seconds() + 300;
        let mut env = base_env.clone();
        env.block.time = Timestamp::from_seconds(t1);
        let committed = swap_read_fee(&mut deps, &env, &trader, true, 500_000_000).unwrap();

        // Now hammer many swaps at the SAME block time, alternating direction to
        // move price hard within the block. Every one must pay `committed`.
        for i in 0..20 {
            let zfo = i % 2 == 0;
            let fee = swap_read_fee(&mut deps, &env, &trader, zfo, 300_000_000).unwrap();
            assert_eq!(
                fee, committed,
                "same-block swap #{} charged {} != committed {}",
                i, fee, committed
            );
        }
    }

    /// Focused manipulation resistance: an attacker slams price (huge swap) to
    /// drive the raw volatility fee toward `max`, then a victim swaps one second
    /// later. The victim's fee must be clamped to `committed + RATE·1`, not the
    /// jerked raw fee — the rate limit defangs single-swap fee manipulation.
    #[test]
    fn dynamic_fee_manipulation_cannot_jerk_victim() {
        const BASE: u32 = 3_000;
        const MAX: u32 = 50_000;
        const RATE: u32 = 50;
        let (mut deps, base_env, lp) = setup_dyn(BASE, MAX, 1_000_000, 600, RATE);
        execute(
            deps.as_mut(),
            base_env.clone(),
            message_info(&lp, &big_coins()),
            ExecuteMsg::Mint {
                lower_tick: -100_000,
                upper_tick: 100_000,
                amount: Uint128::new(1_000_000_000_000),
            },
        )
        .unwrap();
        let attacker = deps.api.addr_make("attacker");
        let victim = deps.api.addr_make("victim");

        // Establish a committed baseline fee at t1.
        let t1 = base_env.block.time.seconds() + 100;
        let mut env1 = base_env.clone();
        env1.block.time = Timestamp::from_seconds(t1);
        let baseline = swap_read_fee(&mut deps, &env1, &attacker, true, 10_000_000).unwrap();

        // Attacker slams a huge swap one second later to spike volatility.
        let mut env2 = base_env.clone();
        env2.block.time = Timestamp::from_seconds(t1 + 1);
        let attacker_fee =
            swap_read_fee(&mut deps, &env2, &attacker, true, 50_000_000_000).unwrap();

        // The attacker's own fee is rate-limited to baseline + RATE·1.
        assert!(
            attacker_fee.abs_diff(baseline) <= RATE,
            "attacker fee jerked {} ppm in 1s (> RATE {}): {} -> {}",
            attacker_fee.abs_diff(baseline),
            RATE,
            baseline,
            attacker_fee
        );

        // Victim swaps one more second later: still clamped to attacker_fee ±RATE.
        let mut env3 = base_env.clone();
        env3.block.time = Timestamp::from_seconds(t1 + 2);
        let victim_fee = swap_read_fee(&mut deps, &env3, &victim, true, 10_000_000).unwrap();
        assert!(
            victim_fee.abs_diff(attacker_fee) <= RATE,
            "victim fee jerked {} ppm in 1s (> RATE {}): {} -> {}",
            victim_fee.abs_diff(attacker_fee),
            RATE,
            attacker_fee,
            victim_fee
        );
        // Net: two seconds after baseline, the fee cannot have moved more than 2·RATE.
        assert!(
            victim_fee.abs_diff(baseline) <= 2 * RATE,
            "fee moved {} ppm over 2s (> 2·RATE {})",
            victim_fee.abs_diff(baseline),
            2 * RATE
        );
    }

    // =====================================================================
    // Item 1 (mock-level): CW20 refund-MESSAGE exactness. The test-tube
    // `cw20_accounting_fuzz` suite proves end-to-end real-balance behavior, but
    // that runs against the prebuilt wasm artifact, so a contract mutation is
    // invisible without a (slow) docker rebuild. These mock tests exercise the
    // SAME refund branches in `apply_swap` directly (`mock_dependencies`,
    // calling `execute_swap_*` and inspecting the emitted transfer messages), so
    // the refund logic is mutation-validated at cargo speed — no wasm needed.
    // =====================================================================

    const CW20A: &str = "cw20token0000000000000000000000000000000000";

    /// Native(T0) / CW20(token1) pool at price 1.0, with one in-range position
    /// holding `liq` liquidity (CW20 leg pulled via TransferFrom — a no-op move
    /// in the mock, but the pool state is updated identically to a real pull).
    fn setup_native_cw20(
        liq: u128,
    ) -> (
        OwnedDeps<MemoryStorage, MockApi, MockQuerier>,
        Env,
        Addr,
        Addr,
    ) {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        let cw20 = deps.api.addr_make(CW20A);
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&factory, &[]),
            InstantiateMsg {
                token0: native(T0),
                token1: AssetInfo::Token {
                    contract_addr: cw20.to_string(),
                },
                tick_spacing: 10,
                fee_config: FeeConfig {
                    base_fee_ppm: 3000,
                    max_fee_ppm: 6000,
                    volatility_multiplier: 0,
                    ema_halflife_seconds: 600,
                    max_fee_change_per_second_ppm: 0,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();
        let lp = deps.api.addr_make("lp");
        // Mint in-range; attach generous T0 (native), CW20 leg via allowance.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&lp, &[Coin::new(Uint128::MAX, T0)]),
            ExecuteMsg::Mint {
                lower_tick: -100,
                upper_tick: 100,
                amount: Uint128::new(liq),
            },
        )
        .unwrap();
        (deps, mock_env(), lp, cw20)
    }

    /// Find a Cw20 `Transfer` message to `to` and return its amount (if any).
    fn find_cw20_transfer(res: &Response, cw20: &str, to: &str) -> Option<u128> {
        res.messages.iter().find_map(|m| {
            if let CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) = &m.msg
            {
                if contract_addr == cw20 {
                    if let Ok(cw20::Cw20ExecuteMsg::Transfer { recipient, amount }) = from_json(msg)
                    {
                        if recipient == to {
                            return Some(amount.u128());
                        }
                    }
                }
            }
            None
        })
    }

    /// Sum of native `BankMsg::Send` amounts of `denom` to `to`.
    fn find_native_refund(res: &Response, to: &str, denom: &str) -> u128 {
        res.messages
            .iter()
            .filter_map(|m| {
                if let CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &m.msg {
                    if to_address == to {
                        return Some(
                            amount
                                .iter()
                                .filter(|c| c.denom == denom)
                                .map(|c| c.amount.u128())
                                .sum::<u128>(),
                        );
                    }
                }
                None
            })
            .sum()
    }

    fn attr_u128(res: &Response, key: &str) -> u128 {
        attr(res, key).parse().unwrap()
    }

    /// Receive-hook (`Cw20AlreadySent`) partial fill: a huge CW20-in swap against
    /// thin liquidity must refund the unused remainder via a Cw20 Transfer back
    /// to the sender of EXACTLY `sent - amount_in`.
    #[test]
    fn cw20_receive_hook_partial_fill_refund_message_exact() {
        let (mut deps, env, _lp, cw20) = setup_native_cw20(50_000_000);
        let trader = deps.api.addr_make("trader");
        let sent = 100_000_000_000u128; // dwarfs the thin liquidity → partial fill

        // CW20 is token1 ⇒ swapping it in is one_for_zero (zero_for_one = false).
        let res = crate::actions::swap::execute_swap_exact_input_cw20(
            deps.as_mut(),
            env,
            trader.clone(),
            Uint128::new(sent),
            /*zero_for_one*/ false,
            Uint128::zero(),
            None,
            None,
            vec![],
        )
        .unwrap();

        let amount_in = attr_u128(&res, "amount_in");
        assert!(
            amount_in < sent,
            "expected partial fill: consumed {} of {}",
            amount_in,
            sent
        );
        let refund = find_cw20_transfer(&res, cw20.as_str(), trader.as_str())
            .expect("partial fill must emit a CW20 refund Transfer to sender");
        assert_eq!(
            refund,
            sent - amount_in,
            "Receive-hook refund {} != sent - amount_in ({} - {})",
            refund,
            sent,
            amount_in
        );
    }

    /// `Cw20Allowance` path: a CW20-input low-level `Swap` with wrongly-attached
    /// native coins must refund the FULL attached amount to the sender. We attach
    /// a non-pool denom so the refund can't be confused with swap output.
    #[test]
    fn cw20_allowance_swap_refunds_attached_native_message() {
        let (mut deps, env, _lp, _cw20) = setup_native_cw20(500_000_000);
        let trader = deps.api.addr_make("trader");
        let recipient = deps.api.addr_make("recipient");
        let stray = 9_999u128;

        // zero_for_one = false ⇒ token1 (CW20) is the input (Cw20Allowance path).
        let res = crate::actions::swap::execute_swap(
            deps.as_mut(),
            env,
            message_info(&trader, &[Coin::new(Uint128::new(stray), "uxxx")]),
            recipient.to_string(),
            /*zero_for_one*/ false,
            Uint128::new(2_000_000),
            choice_clmm_math::tick_math::max_sqrt_ratio() - Uint256::one(),
        )
        .unwrap();

        let refunded = find_native_refund(&res, trader.as_str(), "uxxx");
        assert_eq!(
            refunded, stray,
            "wrongly-attached native not fully refunded: got {} of {}",
            refunded, stray
        );
    }
}

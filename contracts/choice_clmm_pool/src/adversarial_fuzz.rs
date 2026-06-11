//! Adversarial model-based invariant fuzzer (mock backend, no wasm).
//!
//! Threat model & method: drive randomized interleavings of every fund-affecting
//! pool entrypoint (mint / swap-exact-in / low-level swap with price limits /
//! swap-exact-out / burn / collect) against a native/native pool, and after EVERY
//! op assert a battery of *independent* accounting invariants reconstructed from
//! an off-chain model — not just "did it not crash". The model knows each LP's
//! minted liquidity, so it can recompute the quantities the contract maintains
//! incrementally and demand they match exactly:
//!
//!   INV-SOLV   The pool never `Send`s more of a token than it holds (exact
//!              native ledger; surplus is pool-favored dust).
//!   INV-ACTL   slot0.liquidity == Σ position.liquidity over positions whose
//!              [lower, upper) straddles the *current* slot0.tick. The contract
//!              maintains this incrementally through mint/burn/cross; the model
//!              recomputes it from scratch. A mismatch is liquidity-accounting
//!              drift (the root of most CLMM insolvencies).
//!   INV-NET    Σ over all initialized ticks of liquidity_delta == 0 (the net
//!              deltas must telescope; a non-zero sum means a crossing will
//!              over/under-apply liquidity somewhere).
//!   INV-GROSS  For every tick, TickInfo.active_positions_count == Σ liquidity of
//!              positions referencing that tick as lower OR upper (gross L).
//!   INV-DELTA  For every tick, TickInfo.liquidity_delta ==
//!              Σ(+L for positions with lower==t) + Σ(-L for upper==t).
//!   INV-BMAP   The tick bitmap bit for a tick is set IFF a TICKS entry exists
//!              for it (we delete entries on last-position-exit), checked in both
//!              directions (entry⇒bit and bit⇒entry).
//!   INV-DRAIN  After burning + collecting every position: zero active L, the
//!              TICKS map is empty, the bitmap is empty, and no position retains
//!              liquidity or owed tokens. Complete accounting cleanup.
//!
//! On any failure we print the seed, op index, and op so it is a one-line repro,
//! then a binary-search shrink narrows the failing prefix.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod adversarial_fuzz {
    use crate::actions::swap::MAX_SWAP_ITERATIONS;
    use crate::contract::{execute, instantiate};
    use crate::core::bitmap::flip_tick;
    use crate::error::ContractError;
    use crate::state::{
        FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, POOL_STATE, POSITIONS, PROTOCOL_FEES_0,
        PROTOCOL_FEES_1, TICKS, TICK_BITMAP,
    };
    use choice_clmm_common::pool::{ExecuteMsg, FeeConfig, InstantiateMsg, TickInfo};
    use choice_clmm_common::types::AssetInfo;
    use choice_clmm_math::tick_math::{MAX_TICK, MIN_TICK};
    use choice_clmm_math::utils::{to_u256, U256};
    use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env, MockApi};
    use cosmwasm_std::{
        Addr, BankMsg, Coin, CosmosMsg, Order, Response, Storage, Uint128, Uint256,
    };
    use std::collections::BTreeMap;

    const T0: &str = "uaaa";
    const T1: &str = "ubbb";
    const TICK_SPACING: i32 = 10;

    /// Every tick spacing the factory can mint (fee tiers 100/500/3000/10000).
    /// Spacing 1 packs compressed ticks densely (word boundaries every 256
    /// ticks); spacing 200 makes them sparse. Each has a *different*
    /// `MAX_LIQUIDITY_PER_TICK` and a different bitmap compression, so re-running
    /// the full invariant battery per spacing exercises the per-tick cap and the
    /// `tick/spacing` ↔ compressed mapping the single-spacing run never touches.
    const SPACINGS: [i32; 4] = [1, 10, 60, 200];

    // Crank these for heavier runs. The full battery has been exercised at
    // 1500 x 450 (~675k ops) clean; these CI defaults run in a few seconds.
    const SEEDS: u64 = 200;
    const STEPS: usize = 400;
    /// Per-spacing seed budget for the multi-spacing sweep (kept smaller so the
    /// 4-spacing product still runs in CI time; crank for a deep hunt).
    const SPACING_SEEDS: u64 = 60;
    const SPACING_STEPS: usize = 300;

    fn native(d: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: d.to_string(),
        }
    }
    fn price_one() -> Uint256 {
        Uint256::from_u128(1) << 96
    }

    fn next(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// One fuzz operation (recorded so failures can be replayed / shrunk).
    #[derive(Clone, Debug)]
    enum Op {
        Mint {
            oi: usize,
            lower: i32,
            upper: i32,
            liq: u128,
        },
        SwapIn {
            oi: usize,
            zero_for_one: bool,
            amt: u128,
        },
        SwapLimited {
            oi: usize,
            zero_for_one: bool,
            amt: u128,
            num: u32,
            den: u32,
        },
        SwapOut {
            oi: usize,
            zero_for_one: bool,
            amt_out: u128,
        },
        Burn {
            oi: usize,
            lower: i32,
            upper: i32,
            frac: u128,
        },
        Collect {
            oi: usize,
            lower: i32,
            upper: i32,
        },
    }

    /// Generate a single op from the PRNG given the current model.
    ///
    /// `spacing` shapes mint tick generation: lower/upper are produced in
    /// *compressed* units then multiplied by `spacing`, so every minted boundary
    /// is a valid multiple of the spacing and the compressed range (≈ ±600)
    /// straddles bitmap word boundaries (compressed ≡ 0 mod 256: -512,-256,0,
    /// 256,512) for every spacing — keeping the word-boundary code under load
    /// regardless of how sparse the absolute ticks become at spacing 200.
    fn gen_op(st: &mut u64, positions: &BTreeMap<(usize, i32, i32), u128>, spacing: i32) -> Op {
        match next(st) % 12 {
            0..=3 => {
                let oi = (next(st) % 4) as usize;
                let lo_comp = -600 + (next(st) % 1200) as i32;
                let width_comp = 1 + (next(st) % 60) as i32;
                let lower = lo_comp * spacing;
                let upper = (lo_comp + width_comp) * spacing;
                let liq = 1 + next(st) % 1_000_000_000;
                Op::Mint {
                    oi,
                    lower,
                    upper,
                    liq: liq as u128,
                }
            }
            4..=5 => {
                let oi = (next(st) % 4) as usize;
                let zero_for_one = (next(st) & 1) == 0;
                let amt = 1 + next(st) % 50_000_000;
                Op::SwapIn {
                    oi,
                    zero_for_one,
                    amt: amt as u128,
                }
            }
            6 => {
                let oi = (next(st) % 4) as usize;
                let zero_for_one = (next(st) & 1) == 0;
                let amt = 1 + next(st) % 50_000_000;
                // Price-limit numerator/denominator: pull the limit partway so the
                // swap can stop on the limit (reached_limit path).
                let (num, den) = if zero_for_one {
                    (1 + next(st) % 99, 100)
                } else {
                    (101 + next(st) % 99, 100)
                };
                Op::SwapLimited {
                    oi,
                    zero_for_one,
                    amt: amt as u128,
                    num: num as u32,
                    den: den as u32,
                }
            }
            7 => {
                let oi = (next(st) % 4) as usize;
                let zero_for_one = (next(st) & 1) == 0;
                let amt_out = 1 + next(st) % 20_000_000;
                Op::SwapOut {
                    oi,
                    zero_for_one,
                    amt_out: amt_out as u128,
                }
            }
            8..=9 => {
                if let Some((oi, l, u, _)) = pick(positions, st) {
                    let frac = 1 + next(st) % 4; // burn 1/4..1/1 of position
                    Op::Burn {
                        oi,
                        lower: l,
                        upper: u,
                        frac: frac as u128,
                    }
                } else {
                    Op::SwapIn {
                        oi: 0,
                        zero_for_one: true,
                        amt: 1,
                    }
                }
            }
            _ => {
                if let Some((oi, l, u, _)) = pick(positions, st) {
                    Op::Collect {
                        oi,
                        lower: l,
                        upper: u,
                    }
                } else {
                    Op::SwapIn {
                        oi: 0,
                        zero_for_one: false,
                        amt: 1,
                    }
                }
            }
        }
    }

    fn pick(
        m: &BTreeMap<(usize, i32, i32), u128>,
        st: &mut u64,
    ) -> Option<(usize, i32, i32, u128)> {
        if m.is_empty() {
            return None;
        }
        let idx = (next(st) as usize) % m.len();
        m.iter().nth(idx).map(|((o, l, u), liq)| (*o, *l, *u, *liq))
    }

    /// Classify an op result. `Some(res)` on success, `None` on an acceptable
    /// business revert (zero amount / slippage / iteration cap / nothing owed /
    /// stale burn target). Errors that can only mean a logic bug PANIC — so the
    /// shrink harness narrows and reports them instead of the old `if let Ok`
    /// which silently skipped every revert (blinding the fuzzer to spurious-revert
    /// / DoS-by-revert regressions). The per-op invariant battery still runs after
    /// the (possibly unchanged) state regardless.
    ///
    /// (Protocol-fee-carve solvency is exercised by `solvency_fuzz`; these
    /// accounting invariants — INV-ACTL/NET/GROSS/DELTA/BMAP — are independent of
    /// the carve, so this fuzzer leaves the carve at its default.)
    fn classify(r: Result<Response, ContractError>, ctx: &str) -> Option<Response> {
        match r {
            Ok(res) => Some(res),
            Err(e) => {
                match &e {
                    ContractError::Reentrancy {}
                    | ContractError::Unauthorized {}
                    | ContractError::FlashNotRepaid { .. }
                    | ContractError::FlashWithoutLiquidity {}
                    | ContractError::InvalidTokenOrder {} => {
                        panic!("[{}] unexpected logic-bug error: {:?}", ctx, e);
                    }
                    ContractError::Std(s) => {
                        // The pool's own defensive rounding/solvency guard firing is
                        // a bug signal, not a normal revert.
                        if format!("{:?}", s).contains("invariant violated") {
                            panic!("[{}] rounding invariant guard tripped: {:?}", ctx, s);
                        }
                        None
                    }
                    // ZeroAmount / InsufficientOutput / ExcessiveInput /
                    // DeadlineExceeded / PositionNotFound / InvalidFunds /
                    // InvalidConfig / SwapIterationLimit are legitimate for random
                    // inputs (deep crossings can hit the iteration cap, etc.).
                    _ => None,
                }
            }
        }
    }

    fn settle(bal0: &mut u128, bal1: &mut u128, attached: &[Coin], res: &Response, ctx: &str) {
        for c in attached {
            if c.denom == T0 {
                *bal0 += c.amount.u128();
            } else if c.denom == T1 {
                *bal1 += c.amount.u128();
            } else {
                panic!("test attached a non-pool denom: {}", c.denom);
            }
        }
        for m in &res.messages {
            if let CosmosMsg::Bank(BankMsg::Send { amount, .. }) = &m.msg {
                for coin in amount {
                    let amt = coin.amount.u128();
                    let bal = if coin.denom == T0 {
                        &mut *bal0
                    } else {
                        &mut *bal1
                    };
                    assert!(
                        *bal >= amt,
                        "INV-SOLV [{}]: pool Sends {} {} but holds {}",
                        ctx,
                        amt,
                        coin.denom,
                        *bal
                    );
                    *bal -= amt;
                }
            }
        }
    }

    fn bit_set(word: Uint256, b: u32) -> bool {
        let w = to_u256(word);
        !((w >> (b as usize)) & U256::one()).is_zero()
    }

    /// Compressed-tick mapping identical to `core::bitmap::position` for ticks
    /// that are exact multiples of the spacing (always true for stored ticks).
    fn word_bit(tick: i32, spacing: i32) -> (i16, u8) {
        let compressed = tick / spacing; // exact: spacing divides tick
        let word = (compressed >> 8) as i16;
        let bit = (compressed & 0xff) as u8;
        (word, bit)
    }

    /// The load-bearing assertion battery. Reconstructs every incrementally
    /// maintained quantity from the model and demands an exact match.
    fn check_invariants(
        storage: &dyn Storage,
        positions: &BTreeMap<(usize, i32, i32), u128>,
        api: &MockApi,
        spacing: i32,
        ctx: &str,
    ) {
        let slot0 = POOL_STATE.load(storage).unwrap();
        let cur = slot0.tick;

        // --- INV-ACTL: active liquidity == Σ straddling positions. ---
        let mut expected_active: u128 = 0;
        for ((_, l, u), liq) in positions.iter() {
            if *l <= cur && cur < *u {
                expected_active = expected_active
                    .checked_add(*liq)
                    .expect("model active L overflow");
            }
        }
        assert_eq!(
            slot0.liquidity.u128(),
            expected_active,
            "INV-ACTL [{}]: contract active L {} != model {} (tick {})",
            ctx,
            slot0.liquidity.u128(),
            expected_active,
            cur
        );

        // --- Model expectations for per-tick gross/delta. ---
        let mut exp_gross: BTreeMap<i32, u128> = BTreeMap::new();
        let mut exp_delta: BTreeMap<i32, i128> = BTreeMap::new();
        for ((oi, l, u), liq) in positions.iter() {
            let _ = oi;
            *exp_gross.entry(*l).or_insert(0) += *liq;
            *exp_gross.entry(*u).or_insert(0) += *liq;
            *exp_delta.entry(*l).or_insert(0) += *liq as i128;
            *exp_delta.entry(*u).or_insert(0) -= *liq as i128;
        }
        // Ticks whose net liq is zero are deleted by the contract; drop them.
        exp_gross.retain(|_, v| *v != 0);

        // --- Read all contract ticks. ---
        let contract_ticks: BTreeMap<i32, choice_clmm_common::pool::TickInfo> = TICKS
            .range(storage, None, None, Order::Ascending)
            .map(|r| r.unwrap())
            .collect();

        // INV-GROSS / INV-DELTA: every model boundary present with right values.
        for (t, g) in exp_gross.iter() {
            let ti = contract_ticks.get(t).unwrap_or_else(|| {
                panic!(
                    "INV-GROSS [{}]: model expects tick {} but contract has none",
                    ctx, t
                )
            });
            assert_eq!(
                ti.active_positions_count, *g,
                "INV-GROSS [{}]: tick {} gross {} != model {}",
                ctx, t, ti.active_positions_count, g
            );
            let ed = *exp_delta.get(t).unwrap();
            assert_eq!(
                ti.liquidity_delta, ed,
                "INV-DELTA [{}]: tick {} delta {} != model {}",
                ctx, t, ti.liquidity_delta, ed
            );
            assert!(
                ti.initialized,
                "INV-GROSS [{}]: tick {} present but not initialized",
                ctx, t
            );
        }
        // No EXTRA contract ticks beyond the model's set.
        for (t, ti) in contract_ticks.iter() {
            assert!(
                exp_gross.contains_key(t),
                "INV-GROSS [{}]: contract has stray tick {} (gross {}) not in model",
                ctx,
                t,
                ti.active_positions_count
            );
        }

        // --- INV-NET: Σ liquidity_delta over all ticks telescopes to 0. ---
        let net: i128 = contract_ticks.values().map(|t| t.liquidity_delta).sum();
        assert_eq!(
            net, 0,
            "INV-NET [{}]: Σ liquidity_delta = {} (must be 0)",
            ctx, net
        );

        // --- INV-BMAP: bit set IFF a TICKS entry exists. ---
        // entry ⇒ bit
        for t in contract_ticks.keys() {
            let (word, bit) = word_bit(*t, spacing);
            let w = TICK_BITMAP
                .may_load(storage, word)
                .unwrap()
                .unwrap_or_default();
            assert!(
                bit_set(w, bit as u32),
                "INV-BMAP [{}]: tick {} has a TICKS entry but bitmap bit (word {},bit {}) is clear",
                ctx,
                t,
                word,
                bit
            );
        }
        // bit ⇒ entry
        let words: Vec<(i16, Uint256)> = TICK_BITMAP
            .range(storage, None, None, Order::Ascending)
            .map(|r| r.unwrap())
            .collect();
        for (word, w) in words.iter() {
            assert!(
                !w.is_zero(),
                "INV-BMAP [{}]: empty bitmap word {} not pruned",
                ctx,
                word
            );
            for b in 0u32..256 {
                if bit_set(*w, b) {
                    let compressed = (*word as i32) * 256 + b as i32;
                    let tick = compressed * spacing;
                    assert!(
                        contract_ticks.contains_key(&tick),
                        "INV-BMAP [{}]: bitmap bit (word {},bit {}) -> tick {} has no TICKS entry",
                        ctx,
                        word,
                        b,
                        tick
                    );
                }
            }
        }

        let _ = api;
    }

    fn instantiate_pool(
        deps: &mut cosmwasm_std::OwnedDeps<impl Storage, MockApi, impl cosmwasm_std::Querier>,
        factory: &Addr,
        spacing: i32,
    ) {
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(factory, &[]),
            InstantiateMsg {
                token0: native(T0),
                token1: native(T1),
                tick_spacing: spacing as u32,
                // Factory mainnet defaults for the 0.30% tier (multiplier
                // 100_000, 100 ppm/s rate limit) — combined with the per-op
                // time cadence in the runners, the fee genuinely swings
                // 3000..6000 during fuzzing instead of sitting frozen at base.
                fee_config: FeeConfig {
                    base_fee_ppm: 3000,
                    max_fee_ppm: 6000,
                    variable_fee_control: 8_800,
                    max_volatility_accumulator: 2_000,
                    volatility_decay_seconds: 600,
                    max_fee_change_per_second_ppm: 100,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();
    }

    /// Apply one op against the contract; update the model on success. Returns
    /// whether the op succeeded (for shrink bookkeeping). `check` toggles the
    /// per-step invariant battery (off during shrink replay for speed).
    #[allow(clippy::too_many_arguments)]
    fn apply_op(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            MockApi,
            cosmwasm_std::testing::MockQuerier,
        >,
        env: &cosmwasm_std::Env,
        owners: &[Addr],
        bal0: &mut u128,
        bal1: &mut u128,
        positions: &mut BTreeMap<(usize, i32, i32), u128>,
        op: &Op,
        spacing: i32,
        ctx: &str,
        check: bool,
    ) {
        let big = u64::MAX as u128;
        match op {
            Op::Mint {
                oi,
                lower,
                upper,
                liq,
            } => {
                let funds = vec![
                    Coin::new(Uint128::new(big), T0),
                    Coin::new(Uint128::new(big), T1),
                ];
                let msg = ExecuteMsg::Mint {
                    lower_tick: *lower,
                    upper_tick: *upper,
                    amount: Uint128::new(*liq),
                };
                if let Some(res) = classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[*oi], &funds),
                        msg,
                    ),
                    ctx,
                ) {
                    settle(bal0, bal1, &funds, &res, ctx);
                    *positions.entry((*oi, *lower, *upper)).or_insert(0) += *liq;
                }
            }
            Op::SwapIn {
                oi,
                zero_for_one,
                amt,
            } => {
                let denom = if *zero_for_one { T0 } else { T1 };
                let funds = vec![Coin::new(Uint128::new(*amt), denom)];
                let msg = ExecuteMsg::SwapExactInput {
                    minimum_amount_out: Uint128::zero(),
                    recipient: None,
                    deadline: None,
                };
                if let Some(res) = classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[*oi], &funds),
                        msg,
                    ),
                    ctx,
                ) {
                    settle(bal0, bal1, &funds, &res, ctx);
                }
            }
            Op::SwapLimited {
                oi,
                zero_for_one,
                amt,
                num,
                den,
            } => {
                let slot0 = POOL_STATE.load(&deps.storage).unwrap();
                let limit = slot0.sqrt_price * Uint256::from(*num) / Uint256::from(*den);
                let denom = if *zero_for_one { T0 } else { T1 };
                let funds = vec![Coin::new(Uint128::new(*amt), denom)];
                let msg = ExecuteMsg::Swap {
                    recipient: owners[*oi].to_string(),
                    zero_for_one: *zero_for_one,
                    amount_specified: Uint128::new(*amt),
                    sqrt_price_limit_x96: limit,
                };
                if let Some(res) = classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[*oi], &funds),
                        msg,
                    ),
                    ctx,
                ) {
                    settle(bal0, bal1, &funds, &res, ctx);
                }
            }
            Op::SwapOut {
                oi,
                zero_for_one,
                amt_out,
            } => {
                // Over-attach the input token; surplus refunded.
                let denom = if *zero_for_one { T0 } else { T1 };
                let funds = vec![Coin::new(Uint128::new(big), denom)];
                let msg = ExecuteMsg::SwapExactOutput {
                    zero_for_one: *zero_for_one,
                    amount_out: Uint128::new(*amt_out),
                    maximum_amount_in: Uint128::new(big),
                    recipient: None,
                    deadline: None,
                };
                if let Some(res) = classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[*oi], &funds),
                        msg,
                    ),
                    ctx,
                ) {
                    settle(bal0, bal1, &funds, &res, ctx);
                }
            }
            Op::Burn {
                oi,
                lower,
                upper,
                frac,
            } => {
                if let Some(liq) = positions.get(&(*oi, *lower, *upper)).copied() {
                    if liq > 0 {
                        let burn = (liq / *frac).max(1).min(liq);
                        let msg = ExecuteMsg::Burn {
                            lower_tick: *lower,
                            upper_tick: *upper,
                            amount: Uint128::new(burn),
                        };
                        if let Some(res) = classify(
                            execute(
                                deps.as_mut(),
                                env.clone(),
                                message_info(&owners[*oi], &[]),
                                msg,
                            ),
                            ctx,
                        ) {
                            settle(bal0, bal1, &[], &res, ctx);
                            *positions.get_mut(&(*oi, *lower, *upper)).unwrap() -= burn;
                        }
                    }
                }
            }
            Op::Collect { oi, lower, upper } => {
                let msg = ExecuteMsg::Collect {
                    recipient: owners[*oi].to_string(),
                    lower_tick: *lower,
                    upper_tick: *upper,
                    amount0_requested: Uint128::MAX,
                    amount1_requested: Uint128::MAX,
                };
                if let Some(res) = classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[*oi], &[]),
                        msg,
                    ),
                    ctx,
                ) {
                    settle(bal0, bal1, &[], &res, ctx);
                }
            }
        }
        if check {
            check_invariants(&deps.storage, positions, &deps.api, spacing, ctx);
        }
    }

    /// Run `ops` from a fresh pool. Used both for live fuzzing (record=Some) and
    /// shrink replay. Panics propagate (that's the failure signal).
    fn replay(ops: &[Op], spacing: i32, check: bool) {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        instantiate_pool(&mut deps, &factory, spacing);
        let owners: Vec<Addr> = (0..4)
            .map(|i| deps.api.addr_make(&format!("lp{i}")))
            .collect();
        let mut env = mock_env();
        let mut bal0 = 0u128;
        let mut bal1 = 0u128;
        let mut positions: BTreeMap<(usize, i32, i32), u128> = BTreeMap::new();
        for (i, op) in ops.iter().enumerate() {
            // Same-second / next-second / mid-gap / past-halflife cadence. A
            // pure function of the op index keeps prefix-shrink replay exact:
            // op #i always runs at the same timestamp regardless of prefix
            // length (the live runner below uses the identical cadence).
            env.block.time = env.block.time.plus_seconds([0, 1, 30, 700][i % 4]);
            let ctx = format!("spacing{spacing}:op#{i}:{op:?}");
            apply_op(
                &mut deps,
                &env,
                &owners,
                &mut bal0,
                &mut bal1,
                &mut positions,
                op,
                spacing,
                &ctx,
                check,
            );
        }
    }

    /// Binary-search shrink: find the shortest prefix of `ops` that still panics.
    fn shrink_and_report(seed: u64, spacing: i32, ops: &[Op]) -> ! {
        // Find minimal failing prefix length via linear scan from the front
        // (prefixes are monotone: if prefix k passes, the failure is later).
        let mut lo = 1usize;
        let mut hi = ops.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let prefix = ops[..mid].to_vec();
            let panicked = std::panic::catch_unwind(|| replay(&prefix, spacing, true)).is_err();
            if panicked {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        eprintln!("\n==== ADVERSARIAL FUZZ FAILURE (seed {seed}, spacing {spacing}) ====");
        eprintln!("Minimal failing prefix is {lo} ops:");
        for (i, op) in ops[..lo].iter().enumerate() {
            eprintln!("  op#{i}: {op:?}");
        }
        eprintln!("Reproduce: replay(&MINIMAL_PREFIX, {spacing}, true)");
        // Capture and print the actual assertion message (the test's silent hook
        // would otherwise swallow it), then re-panic to fail the test.
        let prefix = ops[..lo].to_vec();
        let payload = std::panic::catch_unwind(move || replay(&prefix, spacing, true)).unwrap_err();
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string());
        eprintln!("FAILING ASSERTION: {msg}");
        std::panic::resume_unwind(payload);
    }

    fn run(seed: u64, spacing: i32, steps: usize) {
        // Pre-generate the full op list so we can shrink deterministically.
        let mut st = seed;
        let mut model: BTreeMap<(usize, i32, i32), u128> = BTreeMap::new();
        let mut ops: Vec<Op> = Vec::with_capacity(steps);
        // We must mirror the *successful* model to generate realistic burn/collect
        // targets, but op application may differ (some execute() calls fail). To
        // keep generation cheap and deterministic we approximate the model by
        // assuming mints succeed (burns target whatever the gen picked); apply_op
        // guards against stale targets, so a missed target is just a no-op.
        for _ in 0..steps {
            let op = gen_op(&mut st, &model, spacing);
            if let Op::Mint {
                oi,
                lower,
                upper,
                liq,
            } = &op
            {
                *model.entry((*oi, *lower, *upper)).or_insert(0) += *liq;
            }
            ops.push(op);
        }

        // Live run with full invariant checking; shrink on panic.
        let result = std::panic::catch_unwind(|| replay(&ops, spacing, true));
        if result.is_err() {
            shrink_and_report(seed, spacing, &ops);
        }
    }

    #[test]
    fn fuzz_adversarial_invariants_many_seeds() {
        // Silence the default panic hook during the catch_unwind probe so shrink
        // output isn't drowned in backtraces; restore for the final re-panic.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        for s in 0..SEEDS {
            let seed = 0xA5A5_0000u64.wrapping_add(s.wrapping_mul(0x9E37_79B9));
            run(seed, TICK_SPACING, STEPS);
        }
        std::panic::set_hook(prev);
    }

    /// Item 2 (residual-risk): re-run the FULL invariant battery across every
    /// factory-mintable tick spacing (1/10/60/200). Each spacing has a distinct
    /// `MAX_LIQUIDITY_PER_TICK` and a distinct `tick/spacing` ↔ compressed-bitmap
    /// mapping; the original sweep only ever ran spacing 10. A bug in the
    /// compression (e.g. negative-tick floor division) or the per-tick cap that
    /// the spacing-10 run masks would surface here.
    #[test]
    fn fuzz_adversarial_invariants_all_spacings() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        for &spacing in SPACINGS.iter() {
            for s in 0..SPACING_SEEDS {
                // Vary the seed by spacing so each spacing explores a different
                // stream (and a per-spacing failure repros deterministically).
                let seed = 0x5AC1_0000u64
                    .wrapping_add((spacing as u64).wrapping_mul(0xD1B5_4A32))
                    .wrapping_add(s.wrapping_mul(0x9E37_79B9));
                run(seed, spacing, SPACING_STEPS);
            }
        }
        std::panic::set_hook(prev);
    }

    /// Final-drain invariant (INV-DRAIN): after a fuzz run, burn + collect every
    /// position and assert complete accounting cleanup.
    #[test]
    fn fuzz_full_drain_cleanup() {
        // Drain over every spacing so INV-DRAIN (empty ticks/bitmap, no residual
        // owed) is proven for each compression/cap regime, not just spacing 10.
        for &spacing in SPACINGS.iter() {
            for s in 0..20u64 {
                let seed = 0xDEAD_0000u64
                    .wrapping_add((spacing as u64).wrapping_mul(0xD1B5_4A32))
                    .wrapping_add(s.wrapping_mul(0x9E37_79B9));
                let mut deps = mock_dependencies();
                let factory = deps.api.addr_make("factory");
                instantiate_pool(&mut deps, &factory, spacing);
                let owners: Vec<Addr> = (0..4)
                    .map(|i| deps.api.addr_make(&format!("lp{i}")))
                    .collect();
                let mut env = mock_env();
                let mut bal0 = 0u128;
                let mut bal1 = 0u128;
                let mut positions: BTreeMap<(usize, i32, i32), u128> = BTreeMap::new();
                let mut st = seed;
                for i in 0..STEPS {
                    // MUST match the cadence in `replay` so shrunk repros run
                    // each op at the same timestamp as the live run.
                    env.block.time = env.block.time.plus_seconds([0, 1, 30, 700][i % 4]);
                    let op = gen_op(&mut st, &positions, spacing);
                    let ctx = format!("spacing{spacing}:seed{seed}:op#{i}");
                    apply_op(
                        &mut deps,
                        &env,
                        &owners,
                        &mut bal0,
                        &mut bal1,
                        &mut positions,
                        &op,
                        spacing,
                        &ctx,
                        false,
                    );
                }

                // Drain: burn all then collect all.
                let keys: Vec<(usize, i32, i32)> = positions.keys().cloned().collect();
                for &(oi, l, u) in &keys {
                    let liq = positions[&(oi, l, u)];
                    if liq > 0 {
                        let res = execute(
                            deps.as_mut(),
                            env.clone(),
                            message_info(&owners[oi], &[]),
                            ExecuteMsg::Burn {
                                lower_tick: l,
                                upper_tick: u,
                                amount: Uint128::new(liq),
                            },
                        )
                        .unwrap();
                        settle(&mut bal0, &mut bal1, &[], &res, "drain-burn");
                        *positions.get_mut(&(oi, l, u)).unwrap() = 0;
                    }
                }
                for &(oi, l, u) in &keys {
                    let res = execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[oi], &[]),
                        ExecuteMsg::Collect {
                            recipient: owners[oi].to_string(),
                            lower_tick: l,
                            upper_tick: u,
                            amount0_requested: Uint128::MAX,
                            amount1_requested: Uint128::MAX,
                        },
                    )
                    .unwrap();
                    settle(&mut bal0, &mut bal1, &[], &res, "drain-collect");
                }

                // INV-DRAIN assertions.
                let slot0 = POOL_STATE.load(&deps.storage).unwrap();
                assert_eq!(
                    slot0.liquidity.u128(),
                    0,
                    "seed {seed}: residual active L after drain"
                );
                let remaining_ticks = TICKS
                    .range(&deps.storage, None, None, Order::Ascending)
                    .count();
                assert_eq!(
                    remaining_ticks, 0,
                    "seed {seed}: {remaining_ticks} ticks remain after full drain"
                );
                let remaining_words = TICK_BITMAP
                    .range(&deps.storage, None, None, Order::Ascending)
                    .count();
                assert_eq!(
                    remaining_words, 0,
                    "seed {seed}: {remaining_words} bitmap words remain after full drain"
                );
                for &(oi, l, u) in &keys {
                    let p = POSITIONS
                        .may_load(&deps.storage, (owners[oi].as_str(), l, u))
                        .unwrap();
                    if let Some(p) = p {
                        assert_eq!(
                            p.liquidity, 0,
                            "seed {seed}: position {oi},{l},{u} retains L"
                        );
                        assert_eq!(
                            p.tokens_owed_0,
                            Uint128::zero(),
                            "seed {seed}: owed0 not drained"
                        );
                        assert_eq!(
                            p.tokens_owed_1,
                            Uint128::zero(),
                            "seed {seed}: owed1 not drained"
                        );
                    }
                }
                // After every position is burned to zero and collected, the only
                // native value the pool may still hold is pool-favored *rounding
                // dust*: mint rounds deposits up, burn rounds withdrawals down, and
                // fee-growth distribution truncates a sub-unit remainder per swap.
                // That accumulates to at most O(ops) units. Asserting a tight bound
                // (rather than the old vacuous `>= 0`) catches a refund/accounting
                // leak that would strand a position's principal or un-distributed
                // fees — which would be orders of magnitude larger than this cap.
                // Protocol fees default ON (v3 table), so after the LP drain
                // the pool legitimately still holds the treasury's carve —
                // that bucket is owed, not stranded. It must be fully BACKED
                // by the remaining balance, and only the excess above it is
                // held to the dust cap.
                let protocol_0 = PROTOCOL_FEES_0
                    .may_load(&deps.storage)
                    .unwrap()
                    .unwrap_or_default()
                    .u128();
                let protocol_1 = PROTOCOL_FEES_1
                    .may_load(&deps.storage)
                    .unwrap()
                    .unwrap_or_default()
                    .u128();
                assert!(
                    bal0 >= protocol_0 && bal1 >= protocol_1,
                    "seed {}: protocol fee bucket not backed after drain: \
                     bal0={} owed0={} bal1={} owed1={}",
                    seed,
                    bal0,
                    protocol_0,
                    bal1,
                    protocol_1,
                );
                let dust_cap: u128 = (STEPS as u128) * 10_000;
                assert!(
                    bal0 - protocol_0 <= dust_cap && bal1 - protocol_1 <= dust_cap,
                    "seed {}: pool stranded more than rounding dust after full drain: \
                     bal0={} bal1={} protocol=({},{}) cap={}",
                    seed,
                    bal0,
                    bal1,
                    protocol_0,
                    protocol_1,
                    dust_cap,
                );
            }
        }
    }

    // =====================================================================
    // Item 3: extreme-tick density / deep swap crossings.
    //
    // The random sweep keeps positions within ±~600 compressed ticks. Here we
    // deliberately pile positions at MIN_TICK / MAX_TICK / full-range AND lay
    // down dense bands of adjacent initialized ticks, then drive large swaps
    // that cross hundreds of ticks in a single call — stressing the u128↔i128
    // liquidity_net conversions and the add/sub-at-crossing sign logic at the
    // tick extremes, where a misparsed cast would corrupt active liquidity. The
    // full invariant battery runs after every op.
    // =====================================================================

    /// Generate an extreme-regime op stream (spacing 1): a mix of full-range,
    /// near-MIN/MAX, and dense-adjacent-band mints interleaved with big swaps.
    fn gen_extreme_ops(seed: u64, n: usize) -> Vec<Op> {
        let mut st = seed;
        let mut ops = Vec::with_capacity(n);
        // Seed the book with structural positions: full-range + extreme edges +
        // an adjacent dense band straddling price 0 (so swaps cross many ticks).
        ops.push(Op::Mint {
            oi: 0,
            lower: MIN_TICK,
            upper: MAX_TICK,
            liq: 800_000_000_000,
        });
        // Dense band of ~300 adjacent unit-width positions across [-150,150].
        for k in 0..300i32 {
            let lower = -150 + k;
            ops.push(Op::Mint {
                oi: (k as usize) % 4,
                lower,
                upper: lower + 1,
                liq: 1 + (next(&mut st) % 5_000_000) as u128,
            });
        }
        // A clutch of positions hugging the extremes.
        for j in 0..8i32 {
            ops.push(Op::Mint {
                oi: 1,
                lower: MIN_TICK + j,
                upper: MIN_TICK + j + 1,
                liq: 1 + (next(&mut st) % 1_000) as u128,
            });
            ops.push(Op::Mint {
                oi: 2,
                lower: MAX_TICK - j - 1,
                upper: MAX_TICK - j,
                liq: 1 + (next(&mut st) % 1_000) as u128,
            });
        }
        // Now interleave big deep-crossing swaps with occasional mints/burns.
        while ops.len() < n {
            match next(&mut st) % 8 {
                0..=4 => {
                    let zero_for_one = (next(&mut st) & 1) == 0;
                    // Large amounts to force deep multi-tick crossings.
                    let amt = 1 + next(&mut st) % 5_000_000_000;
                    ops.push(Op::SwapIn {
                        oi: (next(&mut st) % 4) as usize,
                        zero_for_one,
                        amt: amt as u128,
                    });
                }
                5 => {
                    let zero_for_one = (next(&mut st) & 1) == 0;
                    let amt_out = 1 + next(&mut st) % 2_000_000_000;
                    ops.push(Op::SwapOut {
                        oi: (next(&mut st) % 4) as usize,
                        zero_for_one,
                        amt_out: amt_out as u128,
                    });
                }
                6 => {
                    // Re-mint another adjacent unit position somewhere in the band.
                    let lower = -150 + (next(&mut st) % 300) as i32;
                    ops.push(Op::Mint {
                        oi: (next(&mut st) % 4) as usize,
                        lower,
                        upper: lower + 1,
                        liq: 1 + (next(&mut st) % 1_000_000) as u128,
                    });
                }
                _ => {
                    // Burn part of the dense-band position at a random tick.
                    let lower = -150 + (next(&mut st) % 300) as i32;
                    ops.push(Op::Burn {
                        oi: (next(&mut st) % 4) as usize,
                        lower,
                        upper: lower + 1,
                        frac: 1 + (next(&mut st) % 3) as u128,
                    });
                }
            }
        }
        ops
    }

    #[test]
    fn fuzz_extreme_tick_density() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        for s in 0..15u64 {
            let seed = 0xE47E_0000u64.wrapping_add(s.wrapping_mul(0x9E37_79B9));
            let ops = gen_extreme_ops(seed, 400);
            let res = std::panic::catch_unwind(|| replay(&ops, 1, true));
            if res.is_err() {
                shrink_and_report(seed, 1, &ops);
            }
        }
        std::panic::set_hook(prev);
    }

    /// (slot0, ticks, bitmap, fee_growth_global_0/1, protocol_fees_0/1) — the
    /// full set of swap-relevant stores compared across the iteration-limit revert.
    type SwapStateSnapshot = (
        choice_clmm_common::pool::PoolState,
        Vec<(i32, TickInfo)>,
        Vec<(i16, Uint256)>,
        Uint256,
        Uint256,
        Option<Uint128>,
        Option<Uint128>,
    );

    /// Snapshot the swap-relevant stores (everything `apply_swap` could write).
    /// ORACLE is intentionally excluded: `update_oracle_and_fee` writes it
    /// *before* `compute_swap` runs, so it is mutated even on the revert path —
    /// in production wasmd rolls the whole message back, but a direct mock
    /// `execute()` call has no transaction, so we compare only the stores the
    /// swap body itself touches.
    fn swap_state_snapshot(storage: &dyn Storage) -> SwapStateSnapshot {
        let slot0 = POOL_STATE.load(storage).unwrap();
        let ticks: Vec<(i32, TickInfo)> = TICKS
            .range(storage, None, None, Order::Ascending)
            .map(|r| r.unwrap())
            .collect();
        let bitmap: Vec<(i16, Uint256)> = TICK_BITMAP
            .range(storage, None, None, Order::Ascending)
            .map(|r| r.unwrap())
            .collect();
        let fg0 = FEE_GROWTH_GLOBAL_0
            .may_load(storage)
            .unwrap()
            .unwrap_or_default();
        let fg1 = FEE_GROWTH_GLOBAL_1
            .may_load(storage)
            .unwrap()
            .unwrap_or_default();
        let pf0 = PROTOCOL_FEES_0.may_load(storage).unwrap();
        let pf1 = PROTOCOL_FEES_1.may_load(storage).unwrap();
        (slot0, ticks, bitmap, fg0, fg1, pf0, pf1)
    }

    /// Item 3 — clean-revert proof: a swap that exceeds `MAX_SWAP_ITERATIONS`
    /// must error with `SwapIterationLimit` and leave the swap-relevant state
    /// BYTE-IDENTICAL (no partially-applied crossing). `compute_swap` is a pure
    /// read; `apply_swap` is the only writer and runs only on `Ok`, so the error
    /// path must not have mutated ticks, bitmap, slot0, or fee accumulators.
    #[test]
    fn swap_iteration_limit_reverts_without_partial_state() {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        instantiate_pool(&mut deps, &factory, 1);
        let trader = deps.api.addr_make("trader");

        // Lay down an initialized tick at EVERY spacing-1 tick from 1..=cap+5,
        // each adding +1 L on the way up. A one_for_zero swap then has to cross
        // one initialized tick per loop iteration and blows past the cap.
        let n = (MAX_SWAP_ITERATIONS as i32) + 5;
        for tick in 1..=n {
            let info = TickInfo {
                active_positions_count: 2,
                liquidity_delta: 1,
                fee_growth_outside_0: Uint256::zero(),
                fee_growth_outside_1: Uint256::zero(),
                initialized: true,
            };
            TICKS.save(&mut deps.storage, tick, &info).unwrap();
            flip_tick(&mut deps.storage, tick, 1).unwrap();
        }
        // Establish active liquidity and keep slot0 at tick 0 / price 1.0.
        POOL_STATE
            .save(
                &mut deps.storage,
                &choice_clmm_common::pool::PoolState {
                    sqrt_price: price_one(),
                    tick: 0,
                    liquidity: Uint128::new(1_000),
                },
            )
            .unwrap();

        let before = swap_state_snapshot(&deps.storage);

        // one_for_zero (token1 in) with a huge budget: price rises through the
        // dense ticks and must hit the iteration limit before the budget drains.
        let big = u64::MAX as u128;
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&trader, &[Coin::new(Uint128::new(big), T1)]),
            ExecuteMsg::SwapExactInput {
                minimum_amount_out: Uint128::zero(),
                recipient: None,
                deadline: None,
            },
        )
        .unwrap_err();

        match err {
            ContractError::SwapIterationLimit { iterations } => {
                assert_eq!(
                    iterations,
                    MAX_SWAP_ITERATIONS + 1,
                    "expected limit at MAX+1, got {}",
                    iterations
                );
            }
            other => panic!("expected SwapIterationLimit, got {:?}", other),
        }

        let after = swap_state_snapshot(&deps.storage);
        assert!(
            before.0 == after.0,
            "POOL_STATE mutated on iteration-limit revert: {:?} -> {:?}",
            before.0,
            after.0
        );
        assert!(
            before.1 == after.1,
            "TICKS mutated on iteration-limit revert ({} -> {} entries)",
            before.1.len(),
            after.1.len()
        );
        assert!(
            before.2 == after.2,
            "TICK_BITMAP mutated on iteration-limit revert"
        );
        assert!(
            before.3 == after.3,
            "FEE_GROWTH_GLOBAL_0 mutated on iteration-limit revert"
        );
        assert!(
            before.4 == after.4,
            "FEE_GROWTH_GLOBAL_1 mutated on iteration-limit revert"
        );
        assert!(
            before.5 == after.5,
            "PROTOCOL_FEES_0 mutated on iteration-limit revert"
        );
        assert!(
            before.6 == after.6,
            "PROTOCOL_FEES_1 mutated on iteration-limit revert"
        );
    }
}

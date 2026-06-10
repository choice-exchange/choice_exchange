//! Stateful solvency / accounting invariant fuzzer.
//!
//! Threat model: an attacker with the source drives an arbitrary interleaving of
//! `mint` / `swap` / `burn` / `collect` against a pool, hunting for any sequence
//! that makes the pool promise out more tokens than it physically holds —
//! i.e. value creation via rounding-in-their-favor, fee over-crediting,
//! double-withdrawal, or liquidity/tick accounting drift.
//!
//! Strategy: drive random op sequences against a native/native pool on the mock
//! backend and maintain an EXACT off-chain ledger of the pool's two reserves.
//! The pool can only move native value two ways: funds attached to a (successful)
//! message flow IN, and `BankMsg::Send` flows OUT. So the ledger is exact. The
//! load-bearing assertion: **before applying any `BankMsg::Send`, the pool must
//! already hold at least that amount.** A violation means the pool tried to pay
//! out tokens it never received — insolvency. Finally we drain every position
//! (burn-to-zero + collect-all) AND sweep accrued protocol fees; that the drain
//! itself never underflows proves every LP and the protocol can be fully paid out
//! of real reserves.
//!
//! The protocol-fee carve is toggled on/off mid-run (`SetFeeProtocol`). With the
//! carve ON, the swapper's fee is split: the LP share enters `fee_growth_global`
//! while the protocol share is parked in a *separate* `PROTOCOL_FEES` bucket that
//! the LP drain must not touch. A double-count bug (carve credited to both the LP
//! growth and the protocol bucket) would make the LP drain or the protocol sweep
//! try to `Send` more than the pool holds — caught by the ledger assertion.
//!
//! Reverts are NOT silently swallowed: every op result is classified, errors that
//! can only indicate a logic bug abort the test, and the per-op success counts are
//! asserted at the end so a regression that bricks an op path (spurious reverts)
//! surfaces as "0 successes" rather than passing vacuously.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod solvency_fuzz {
    use crate::contract::{execute, instantiate};
    use crate::error::ContractError;
    use choice_clmm_common::pool::{ExecuteMsg, FeeConfig, InstantiateMsg};
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{
        message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{
        to_json_binary, Addr, BankMsg, Coin, ContractResult, CosmosMsg, OwnedDeps, Response,
        SystemResult, Uint128, Uint256, WasmQuery,
    };
    use std::collections::BTreeMap;

    // token0 < token1 (native ordering is lexicographic on the denom).
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

    /// Deterministic splitmix64 — reproducible failures.
    fn next(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Per-op success tally — used to assert the fuzzer actually exercises each
    /// path (liveness), so a regression that makes an op always revert is caught.
    #[derive(Default)]
    struct Counts {
        mint: u64,
        swap: u64,
        burn: u64,
        collect: u64,
        set_fee_protocol: u64,
        collect_protocol: u64,
        protocol_sent_0: u128,
        protocol_sent_1: u128,
    }

    /// Classify an op result. Returns `Some(res)` on success and `None` on an
    /// acceptable business revert (zero amount, slippage, iteration cap, nothing
    /// owed, …). Errors that can only mean a logic bug abort the test rather than
    /// being silently skipped.
    fn classify(r: Result<Response, ContractError>, ctx: &str) -> Option<Response> {
        match r {
            Ok(res) => Some(res),
            Err(e) => {
                match &e {
                    // The fuzzer always uses correct senders and never flash-loans,
                    // so these are unreachable unless the contract logic is broken.
                    ContractError::Reentrancy {}
                    | ContractError::Unauthorized {}
                    | ContractError::FlashNotRepaid { .. }
                    | ContractError::FlashWithoutLiquidity {}
                    | ContractError::InvalidTokenOrder {} => {
                        panic!("[{}] unexpected logic-bug error: {:?}", ctx, e);
                    }
                    ContractError::Std(s) => {
                        let m = format!("{s:?}");
                        // The pool's own defensive rounding/solvency guards firing
                        // is a real bug signal, not an acceptable revert.
                        if m.contains("invariant violated")
                            || m.contains("overflow")
                            || m.contains("underflow")
                        {
                            panic!("[{}] arithmetic/invariant guard tripped: {}", ctx, m);
                        }
                        None
                    }
                    // ZeroAmount / InsufficientOutput / ExcessiveInput /
                    // DeadlineExceeded / PositionNotFound / InvalidFunds /
                    // InvalidConfig / SwapIterationLimit are all legitimate
                    // business reverts for random inputs.
                    _ => None,
                }
            }
        }
    }

    /// Apply a SUCCESSFUL response to the native ledger, asserting the pool never
    /// instructs a transfer it cannot cover. `attached` is what the caller sent
    /// with this (successful) message — on a real chain it is credited to the
    /// pool before the handler's own `Send`s execute. Returns (sent0, sent1) so
    /// callers can attribute protocol-fee outflows.
    fn settle(
        bal0: &mut u128,
        bal1: &mut u128,
        attached: &[Coin],
        res: &Response,
        ctx: &str,
    ) -> (u128, u128) {
        for c in attached {
            if c.denom == T0 {
                *bal0 += c.amount.u128();
            } else if c.denom == T1 {
                *bal1 += c.amount.u128();
            } else {
                panic!("test attached a non-pool denom: {}", c.denom);
            }
        }
        let (mut sent0, mut sent1) = (0u128, 0u128);
        for m in &res.messages {
            // Protocol-fee sweeps to the treasury are BankMsg::Send (no burn
            // auction is configured), so the ledger remains exact. A WasmMsg
            // outflow would slip past this and is asserted against below.
            if let CosmosMsg::Wasm(_) = &m.msg {
                panic!("[{}] unexpected WasmMsg outflow — ledger would desync", ctx);
            }
            if let CosmosMsg::Bank(BankMsg::Send { amount, .. }) = &m.msg {
                for coin in amount {
                    let amt = coin.amount.u128();
                    let (bal, sent) = if coin.denom == T0 {
                        (&mut *bal0, &mut sent0)
                    } else if coin.denom == T1 {
                        (&mut *bal1, &mut sent1)
                    } else {
                        panic!("pool sent a non-pool denom: {}", coin.denom);
                    };
                    assert!(
                        *bal >= amt,
                        "INSOLVENCY [{}]: pool instructs Send of {} {} but only holds {}",
                        ctx,
                        amt,
                        coin.denom,
                        *bal
                    );
                    *bal -= amt;
                    *sent += amt;
                }
            }
        }
        (sent0, sent1)
    }

    /// Pick a pseudo-random entry from the position map.
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

    /// Build a mock backend whose querier answers the factory's `GetConfig` with
    /// `owner = factory`, so `SetFeeProtocol` / `CollectProtocol` authorize.
    fn setup() -> (OwnedDeps<MockStorage, MockApi, MockQuerier>, Addr) {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        let owner = factory.to_string();
        deps.querier.update_wasm(move |q| match q {
            WasmQuery::Smart { .. } => {
                let resp = choice_clmm_common::factory::ConfigResponse {
                    owner: owner.clone(),
                    pool_code_id: 1,
                };
                SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
            }
            _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
        });
        (deps, factory)
    }

    // `x % n == 0` is kept over `.is_multiple_of(n)` deliberately: this fuzzer
    // is host-only test code, but we don't want to impose a Rust-version floor
    // (is_multiple_of is recent-stable) on running the suite.
    #[allow(clippy::manual_is_multiple_of)]
    fn run(seed: u64, steps: usize) -> Counts {
        let (mut deps, factory) = setup();

        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&factory, &[]),
            InstantiateMsg {
                token0: native(T0),
                token1: native(T1),
                tick_spacing: 10,
                // Factory mainnet defaults for the 0.30% tier. The fuzzer used
                // to pin `volatility_multiplier: 100` / rate limit 0, which
                // (with the time cadence below) froze the fee at base — so
                // solvency was never exercised while fee_pips actually swings
                // 3000..6000 under rate limiting, exactly how mainnet runs.
                fee_config: FeeConfig {
                    base_fee_ppm: 3000,
                    max_fee_ppm: 6000,
                    volatility_multiplier: 100_000,
                    ema_halflife_seconds: 600,
                    max_fee_change_per_second_ppm: 100,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();

        let owners: Vec<Addr> = (0..4)
            .map(|i| deps.api.addr_make(&format!("lp{i}")))
            .collect();
        let treasury = deps.api.addr_make("protocol_treasury");
        let mut env = mock_env();

        // Route protocol fees entirely to the treasury via BankMsg::Send (no burn
        // auction) so the off-chain ledger stays exact.
        classify(
            execute(
                deps.as_mut(),
                env.clone(),
                message_info(&factory, &[]),
                ExecuteMsg::UpdateProtocolFeeConfig {
                    treasury: Some(treasury.to_string()),
                    burn_auction: None,
                    burn_share_bps: Some(0),
                    clear_burn_auction: true,
                },
            ),
            "update_protocol_fee_config",
        )
        .expect("protocol fee config setup must succeed");

        let mut bal0: u128 = 0;
        let mut bal1: u128 = 0;
        // (owner_idx, lower, upper) -> liquidity currently minted by that owner.
        let mut positions: BTreeMap<(usize, i32, i32), u128> = BTreeMap::new();
        let mut counts = Counts::default();

        let mut st = seed;

        for step in 0..steps {
            // Advance block time on a fixed cadence so the dynamic fee actually
            // moves: same-second (frozen fee), next-second, mid-gap, and
            // past-halflife (EMA snap) steps. A pure function of the step index
            // keeps seed-replay reproducible.
            env.block.time = env.block.time.plus_seconds([0, 1, 30, 700][step % 4]);
            // Periodically toggle the protocol-fee carve on/off so the carve path
            // (separate PROTOCOL_FEES bucket, excluded from fee_growth) is fuzzed,
            // not just the divisor==0 configuration.
            if next(&mut st) % 7 == 0 {
                let div = |s: &mut u64| -> u8 {
                    match next(s) % 3 {
                        0 => 0,                       // off
                        1 => 4,                       // 25%
                        _ => 4 + (next(s) % 7) as u8, // 4..=10
                    }
                };
                let fp0 = div(&mut st);
                let fp1 = div(&mut st);
                if classify(
                    execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&factory, &[]),
                        ExecuteMsg::SetFeeProtocol {
                            fee_protocol_0: fp0,
                            fee_protocol_1: fp1,
                        },
                    ),
                    "set_fee_protocol",
                )
                .is_some()
                {
                    counts.set_fee_protocol += 1;
                }
            }

            match next(&mut st) % 10 {
                // ---- MINT (40%) ----
                0..=3 => {
                    let oi = (next(&mut st) % 4) as usize;
                    let lower = -5000 + 10 * (next(&mut st) % 1000) as i32; // -5000..=4990
                    let width = 10 * (1 + next(&mut st) % 400) as i32; // 10..=4000
                    let upper = lower + width;
                    let liq = 1 + next(&mut st) % 1_000_000_000; // 1..=1e9
                                                                 // Over-attach both tokens; the pool consumes what it needs
                                                                 // (rounded up) and refunds the surplus.
                    let funds = vec![
                        Coin::new(Uint128::new(u64::MAX as u128), T0),
                        Coin::new(Uint128::new(u64::MAX as u128), T1),
                    ];
                    let msg = ExecuteMsg::Mint {
                        lower_tick: lower,
                        upper_tick: upper,
                        amount: Uint128::new(liq as u128),
                    };
                    if let Some(res) = classify(
                        execute(
                            deps.as_mut(),
                            env.clone(),
                            message_info(&owners[oi], &funds),
                            msg,
                        ),
                        "mint",
                    ) {
                        settle(&mut bal0, &mut bal1, &funds, &res, "mint");
                        *positions.entry((oi, lower, upper)).or_insert(0) += liq as u128;
                        counts.mint += 1;
                    }
                }
                // ---- SWAP (30%) ----
                4..=6 => {
                    let zero_for_one = (next(&mut st) & 1) == 0;
                    let amt = 1 + next(&mut st) % 50_000_000; // up to 5e7
                    let denom = if zero_for_one { T0 } else { T1 };
                    let funds = vec![Coin::new(Uint128::new(amt as u128), denom)];
                    let oi = (next(&mut st) % 4) as usize;
                    let msg = ExecuteMsg::SwapExactInput {
                        minimum_amount_out: Uint128::zero(),
                        recipient: None,
                        deadline: None,
                    };
                    if let Some(res) = classify(
                        execute(
                            deps.as_mut(),
                            env.clone(),
                            message_info(&owners[oi], &funds),
                            msg,
                        ),
                        "swap",
                    ) {
                        settle(&mut bal0, &mut bal1, &funds, &res, "swap");
                        counts.swap += 1;
                    }
                }
                // ---- BURN part of a random position (20%) ----
                7..=8 => {
                    if let Some((oi, l, u, liq)) = pick(&positions, &mut st) {
                        if liq > 0 {
                            let burn: u128 = 1 + (next(&mut st) as u128) % liq;
                            let msg = ExecuteMsg::Burn {
                                lower_tick: l,
                                upper_tick: u,
                                amount: Uint128::new(burn),
                            };
                            if let Some(res) = classify(
                                execute(
                                    deps.as_mut(),
                                    env.clone(),
                                    message_info(&owners[oi], &[]),
                                    msg,
                                ),
                                "burn",
                            ) {
                                settle(&mut bal0, &mut bal1, &[], &res, "burn");
                                *positions.get_mut(&(oi, l, u)).unwrap() -= burn;
                                counts.burn += 1;
                            }
                        }
                    }
                }
                // ---- COLLECT a random position (10%) ----
                _ => {
                    if let Some((oi, l, u, _)) = pick(&positions, &mut st) {
                        let msg = ExecuteMsg::Collect {
                            recipient: owners[oi].to_string(),
                            lower_tick: l,
                            upper_tick: u,
                            amount0_requested: Uint128::MAX,
                            amount1_requested: Uint128::MAX,
                        };
                        if let Some(res) = classify(
                            execute(
                                deps.as_mut(),
                                env.clone(),
                                message_info(&owners[oi], &[]),
                                msg,
                            ),
                            "collect",
                        ) {
                            settle(&mut bal0, &mut bal1, &[], &res, "collect");
                            counts.collect += 1;
                        }
                    }
                }
            }
        }

        // ---- FINAL DRAIN: prove every position can be fully realized. ----
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

        // ---- SWEEP PROTOCOL FEES: the carve bucket must also be fully backed. ----
        // After every LP has drained, the only value the pool may still owe is the
        // accrued protocol fee. Collecting it must not underflow the reserves —
        // proving the carve was held in real tokens and never double-spent against
        // the LP withdrawals above.
        let res = execute(
            deps.as_mut(),
            env.clone(),
            message_info(&factory, &[]),
            ExecuteMsg::CollectProtocol {
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
        )
        .unwrap();
        let (p0, p1) = settle(&mut bal0, &mut bal1, &[], &res, "drain-protocol");
        counts.collect_protocol += 1;
        counts.protocol_sent_0 += p0;
        counts.protocol_sent_1 += p1;

        // Solvency held at every step (no Send ever underflowed the reserves).
        // Whatever remains is pool-favored rounding dust — never negative.
        counts
    }

    #[test]
    fn fuzz_pool_solvency_many_seeds() {
        let mut total = Counts::default();
        for seed in [
            1u64,
            42,
            7_777,
            0x1234_5678,
            0xDEAD_BEEF,
            0xC0FF_EE00,
            0xA5A5_5A5A,
            123_456_789,
            0xFACE_FEED,
            999_999,
        ] {
            let c = run(seed, 400);
            total.mint += c.mint;
            total.swap += c.swap;
            total.burn += c.burn;
            total.collect += c.collect;
            total.set_fee_protocol += c.set_fee_protocol;
            total.collect_protocol += c.collect_protocol;
            total.protocol_sent_0 += c.protocol_sent_0;
            total.protocol_sent_1 += c.protocol_sent_1;
        }

        // Liveness: every op path must succeed a meaningful number of times across
        // the sweep, so a regression that bricks one path (spurious reverts) shows
        // up as a count near zero rather than the test passing vacuously.
        assert!(total.mint > 200, "too few successful mints: {}", total.mint);
        assert!(total.swap > 200, "too few successful swaps: {}", total.swap);
        assert!(total.burn > 50, "too few successful burns: {}", total.burn);
        assert!(
            total.collect > 20,
            "too few successful collects: {}",
            total.collect
        );
        assert!(
            total.set_fee_protocol > 50,
            "protocol-fee carve barely toggled: {}",
            total.set_fee_protocol
        );
        // The carve must actually accrue and be swept, or the protocol-fee path is
        // not really being exercised under the solvency ledger.
        assert!(
            total.protocol_sent_0 > 0 || total.protocol_sent_1 > 0,
            "no protocol fees were ever accrued+swept — carve path untested"
        );
    }
}

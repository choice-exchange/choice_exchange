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
//! (burn-to-zero + collect-all); that the drain itself never underflows proves
//! every LP can be fully paid out of real reserves.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod solvency_fuzz {
    use crate::contract::{execute, instantiate};
    use choice_clmm_common::pool::{ExecuteMsg, FeeConfig, InstantiateMsg};
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
    use cosmwasm_std::{Addr, BankMsg, Coin, CosmosMsg, Response, Uint128, Uint256};
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

    /// Apply a SUCCESSFUL response to the native ledger, asserting the pool never
    /// instructs a transfer it cannot cover. `attached` is what the caller sent
    /// with this (successful) message — on a real chain it is credited to the
    /// pool before the handler's own `Send`s execute.
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
                    } else if coin.denom == T1 {
                        &mut *bal1
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
                }
            }
        }
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

    fn run(seed: u64, steps: usize) {
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
                    base_fee_ppm: 3000,
                    max_fee_ppm: 6000,
                    volatility_multiplier: 100,
                    ema_halflife_seconds: 600,
                    max_fee_change_per_second_ppm: 0,
                },
                initial_sqrt_price: price_one(),
            },
        )
        .unwrap();

        let owners: Vec<Addr> = (0..4)
            .map(|i| deps.api.addr_make(&format!("lp{i}")))
            .collect();
        let env = mock_env();

        let mut bal0: u128 = 0;
        let mut bal1: u128 = 0;
        // (owner_idx, lower, upper) -> liquidity currently minted by that owner.
        let mut positions: BTreeMap<(usize, i32, i32), u128> = BTreeMap::new();

        let mut st = seed;

        for _ in 0..steps {
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
                    if let Ok(res) = execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[oi], &funds),
                        msg,
                    ) {
                        settle(&mut bal0, &mut bal1, &funds, &res, "mint");
                        *positions.entry((oi, lower, upper)).or_insert(0) += liq as u128;
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
                    if let Ok(res) = execute(
                        deps.as_mut(),
                        env.clone(),
                        message_info(&owners[oi], &funds),
                        msg,
                    ) {
                        settle(&mut bal0, &mut bal1, &funds, &res, "swap");
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
                            if let Ok(res) = execute(
                                deps.as_mut(),
                                env.clone(),
                                message_info(&owners[oi], &[]),
                                msg,
                            ) {
                                settle(&mut bal0, &mut bal1, &[], &res, "burn");
                                *positions.get_mut(&(oi, l, u)).unwrap() -= burn;
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
                        if let Ok(res) = execute(
                            deps.as_mut(),
                            env.clone(),
                            message_info(&owners[oi], &[]),
                            msg,
                        ) {
                            settle(&mut bal0, &mut bal1, &[], &res, "collect");
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

        // Solvency held at every step (no Send ever underflowed the reserves).
        // Whatever remains is pool-favored rounding dust — never negative.
    }

    #[test]
    fn fuzz_pool_solvency_many_seeds() {
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
            run(seed, 400);
        }
    }
}

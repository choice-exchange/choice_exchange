#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use std::str::FromStr;

    use crate::contract::{execute, instantiate, query};
    use crate::error::ContractError;
    use crate::state::PoolConfig;
    use choice_clmm_common::pool::{ExecuteMsg, FeeConfig, InstantiateMsg, PoolState, QueryMsg};
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
    use cosmwasm_std::{from_json, BankMsg, Coin, StdError, Uint128, Uint256};

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    // Helper to mock Q64.96 representation of "1.0"
    // 2^96 = 79228162514264337593543950336
    fn get_price_one() -> Uint256 {
        Uint256::from_u128(1) << 96
    }

    #[test]
    fn test_proper_instantiation() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("peggy0xdac"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };

        let info = message_info(&deps.api.addr_make("factory_addr"), &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.attributes[0].value, "instantiate");

        // 1. Test Config Query
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetConfig {}).unwrap();
        let config: PoolConfig = from_json(&res).unwrap();
        assert_eq!(config.token0, native("inj"));
        assert_eq!(config.factory, deps.api.addr_make("factory_addr"));

        // 2. Test Slot0 Query
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res).unwrap();
        assert_eq!(slot0.sqrt_price, get_price_one());
        assert_eq!(slot0.liquidity, cosmwasm_std::Uint128::zero());
    }

    #[test]
    fn test_invalid_token_order() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            token0: native("usdt"), // Alphabetically after "inj"
            token1: native("inj"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 5000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 0,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };

        let info = message_info(&deps.api.addr_make("factory"), &[]);
        let err = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap_err();

        match err {
            crate::error::ContractError::InvalidTokenOrder {} => {}
            _ => panic!("Expected InvalidTokenOrder error"),
        }
    }

    #[test]
    fn test_mint_liquidity() {
        let mut deps = mock_dependencies();

        // 1. Instantiate
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("peggy0xdac"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 2. Mint Liquidity
        // Range: -200 to 200. Current Tick 0 is inside.
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1000u128),
        };

        // --- FIX: Send funds ---
        // We send enough of both tokens to cover the calculated requirement.
        let user_info = message_info(
            &deps.api.addr_make("user_addr"),
            &[
                Coin::new(Uint128::new(1000000), "inj"),
                Coin::new(Uint128::new(1000000), "peggy0xdac"),
            ],
        );

        let res = execute(deps.as_mut(), mock_env(), user_info, mint_msg).unwrap();

        assert_eq!(
            res.attributes
                .iter()
                .find(|a| a.key == "action")
                .unwrap()
                .value,
            "mint"
        );

        // 3. Verify Slot0 Liquidity
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res).unwrap();
        assert_eq!(slot0.liquidity, Uint128::from(1000u128));
    }

    #[test]
    fn test_mint_math_integration() {
        let mut deps = mock_dependencies();

        // --- FIX: Actually Instantiate the contract ---
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("peggy0xdac"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // Mint Scenario: Range 10 to 20 (Strictly ABOVE current price 0)
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: 10,
            upper_tick: 20,
            amount: Uint128::from(1000u128),
        };

        let user_info = message_info(
            &deps.api.addr_make("user"),
            &[
                Coin::new(Uint128::new(1000000), "inj"),
                Coin::new(Uint128::new(1000000), "peggy0xdac"),
            ],
        );

        let res = execute(deps.as_mut(), mock_env(), user_info, mint_msg).unwrap();

        // Verify results
        // Since range is ABOVE current price, we provide Asset X (Token0 / inj)
        // We do NOT provide Asset Y (Token1 / peggy...)
        assert_eq!(
            res.attributes
                .iter()
                .find(|a| a.key == "amount1_consumed")
                .unwrap()
                .value,
            "0"
        );
        assert_ne!(
            res.attributes
                .iter()
                .find(|a| a.key == "amount0_consumed")
                .unwrap()
                .value,
            "0"
        );
    }

    #[test]
    fn test_swap_exact_input() {
        let mut deps = mock_dependencies();

        // 1. Instantiate (Price = 1.0)
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000, // 0.3%
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 2. Mint Liquidity
        // Range: -200 to 200. Amount: 1,000,000
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };

        let lp_info = message_info(
            &deps.api.addr_make("lp_provider"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        // 3. Execute Swap: Sell 1000 INJ (Zero -> One)
        // Price should decrease. Tick should decrease.
        let swap_amount = Uint128::from(1000u128);

        // Target Price Limit: Minimum possible (slippage protection disabled for test)
        // 4295128739 is MIN_SQRT_RATIO (from math package)
        let min_sqrt_ratio = Uint256::from(4295128739u128 + 1);

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: true, // Sell Token 0
            amount_specified: swap_amount,
            sqrt_price_limit_x96: min_sqrt_ratio,
        };

        // User must send the INJ they want to sell
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1000), "inj")],
        );

        let res = execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // 4. Verification

        // A. Check Attributes
        assert_eq!(
            res.attributes
                .iter()
                .find(|a| a.key == "action")
                .unwrap()
                .value,
            "swap"
        );
        let amount_out_str = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .clone();
        let amount_out = amount_out_str.parse::<u128>().unwrap();

        println!("Swapped 1000 INJ for {} USDT", amount_out);
        assert!(amount_out > 0, "Swap should produce output");

        // B. Check Price/Tick Movement
        let res_query = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res_query).unwrap();

        // Price should be LESS than 1.0 (79228...) because we sold Token 0
        assert!(slot0.sqrt_price < get_price_one());

        // Tick should be negative
        assert!(slot0.tick < 0);

        // C. Check Bank Messages (Payout)
        // Should send USDT to trader
        let msg = &res.messages[0];
        match &msg.msg {
            cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, "trader");
                assert_eq!(amount[0].denom, "usdt");
                assert_eq!(amount[0].amount.u128(), amount_out);
            }
            _ => panic!("Expected Bank Send message"),
        }
    }

    #[test]
    fn test_swap_one_for_zero_reverse() {
        let mut deps = mock_dependencies();

        // 1. Setup Pool & Liquidity
        // Price = 1.0. Range [-200, 200]
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        let lp_info = message_info(
            &deps.api.addr_make("lp"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        // 2. Swap USDT -> INJ (One For Zero)
        // Price should INCREASE. Tick should INCREASE.
        let swap_amount = Uint128::from(1000u128);

        // Limit: Max possible price
        // 146144... is MAX_SQRT_RATIO
        let max_sqrt_ratio =
            Uint256::from_str("1461446703485210103287273052203988822378723970341").unwrap();

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: false, // Buy Token 0
            amount_specified: swap_amount,
            sqrt_price_limit_x96: max_sqrt_ratio,
        };

        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1000), "usdt")],
        );

        let res = execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // 3. Verify
        let amount_out = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        println!("Swapped 1000 USDT for {} INJ", amount_out);
        assert!(amount_out > 0);

        let res_query = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res_query).unwrap();

        // Price should be > 1.0
        assert!(slot0.sqrt_price > get_price_one());
        // Tick should be >= 0
        assert!(slot0.tick >= 0);
    }

    #[test]
    fn test_swap_small_no_cross() {
        // Test a swap so small it stays within the current tick
        let mut deps = mock_dependencies();
        // ... (Instantiate & Mint same as above) ...
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        let lp_info = message_info(
            &deps.api.addr_make("lp"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        // Swap tiny amount
        let swap_amount = Uint128::from(10u128);
        // Limit: slightly above MIN_SQRT_RATIO
        let min_sqrt_ratio = Uint256::from(4295128740u128);

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: true,
            amount_specified: swap_amount,
            sqrt_price_limit_x96: min_sqrt_ratio,
        };
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(10), "inj")],
        );

        execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // Verify state
        let res_query = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res_query).unwrap();

        // FIX: The tick should move to -1 because Price < 1.0
        assert_eq!(slot0.tick, -1);

        // Price should be less than 1.0 (2^96)
        assert!(slot0.sqrt_price < get_price_one());
    }

    #[test]
    fn test_swap_price_limit_hit() {
        let mut deps = mock_dependencies();
        // ... (Instantiate & Mint same as above) ...
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        let lp_info = message_info(
            &deps.api.addr_make("lp"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        // Set a limit very close to current price (1.0)
        // Current: 79228162514264337593543950336
        // Limit:   79220000000000000000000000000 (Slightly lower)
        let close_limit = Uint256::from_str("79220000000000000000000000000").unwrap();

        let swap_amount = Uint128::from(1_000_000u128); // Massive swap

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: true,
            amount_specified: swap_amount,
            sqrt_price_limit_x96: close_limit,
        };
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1000000), "inj")],
        );

        let res = execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // Verify we stopped at the limit
        let res_query = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res_query).unwrap();

        assert_eq!(slot0.sqrt_price, close_limit);

        // Verify we didn't use all input
        let amount_in_used = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_in")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        assert!(amount_in_used < 1_000_000); // Should be much less

        // Verify refund message exists
        let refund_msg = res.messages.iter().find(|m| {
            if let cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { amount, .. }) = &m.msg {
                amount[0].denom == "inj"
            } else {
                false
            }
        });
        assert!(refund_msg.is_some());
    }

    #[test]
    fn test_mint_burn_collect_lifecycle() {
        let mut deps = mock_dependencies();

        // 1. Setup Pool
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let lp_addr = "lp_provider";

        // Construct the signer info first to get the real address
        let lp_info = message_info(
            &deps.api.addr_make(lp_addr),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );

        // 2. Mint 1,000,000 Liquidity (Range -200 to 200)
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        execute(deps.as_mut(), mock_env(), lp_info.clone(), mint_msg).unwrap();

        // 3. Swap to generate fees and change price
        // Trader swaps 1000 INJ for USDT
        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: true, // Sell INJ
            amount_specified: Uint128::from(1000u128),
            sqrt_price_limit_x96: Uint256::from(4295128740u128), // Min price
        };
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1000), "inj")],
        );
        execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // 4. Burn 50% of Liquidity
        // This should push principal AND fees into `tokens_owed`
        let burn_msg = ExecuteMsg::Burn {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(500_000u128),
        };
        let res = execute(deps.as_mut(), mock_env(), lp_info.clone(), burn_msg).unwrap();

        // Verify Burn Attributes
        let burned_0 = res
            .attributes
            .iter()
            .find(|a| a.key == "amount0_burned")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        let burned_1 = res
            .attributes
            .iter()
            .find(|a| a.key == "amount1_burned")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();

        println!("Burned Principal: {} INJ, {} USDT", burned_0, burned_1);
        assert!(burned_0 > 0 || burned_1 > 0);

        // 5. Collect (Claim Everything)
        // Use MaxUint128 to request all owed
        let max_collect = Uint128::new(u128::MAX);
        let collect_msg = ExecuteMsg::Collect {
            recipient: lp_addr.to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount0_requested: max_collect,
            amount1_requested: max_collect,
        };
        let res_collect = execute(deps.as_mut(), mock_env(), lp_info, collect_msg).unwrap();

        // 6. Verify Payout
        let collected_0 = res_collect
            .attributes
            .iter()
            .find(|a| a.key == "amount0")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        let collected_1 = res_collect
            .attributes
            .iter()
            .find(|a| a.key == "amount1")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();

        println!("Collected Total: {} INJ, {} USDT", collected_0, collected_1);

        // Logic check:
        // Collected should be >= Burned Principal (because it includes fees)
        assert!(collected_0 >= burned_0);
        assert!(collected_1 >= burned_1);

        // Since we swapped INJ in, the pool has more INJ fees.
        // The LP should have earned some INJ fees on top of principal.
        // (Note: burned principal changes due to price move, so exact comparison is complex, but >= is safe).
    }

    #[test]
    fn test_dynamic_fee_scaling() {
        let mut deps = mock_dependencies();
        let env_start = mock_env();

        // --- Helper to setup a pool and run the scenario ---
        // Returns the amount_out of the final "Probe Swap"
        let mut run_scenario = |multiplier: u32| -> u128 {
            // 1. Instantiate
            let msg = InstantiateMsg {
                token0: native("inj"),
                token1: native("usdt"),
                tick_spacing: 10,
                fee_config: FeeConfig {
                    base_fee_ppm: 0, // 0% Base fee to isolate dynamic effects
                    max_fee_ppm: 100000,
                    volatility_multiplier: multiplier, // Variable
                    ema_halflife_seconds: 100,
                max_fee_change_per_second_ppm: 0,
                },
                initial_sqrt_price: get_price_one(),
            };
            let creator = message_info(&deps.api.addr_make("factory"), &[]);
            // reset storage for clean run
            instantiate(deps.as_mut(), env_start.clone(), creator, msg).unwrap();

            // 2. Mint Liquidity
            let mint_msg = ExecuteMsg::Mint {
                lower_tick: -2000,
                upper_tick: 2000,
                amount: Uint128::from(10_000_000u128),
            };
            let lp_info = message_info(
                &deps.api.addr_make("lp"),
                &[
                    Coin::new(Uint128::new(10000000000), "inj"),
                    Coin::new(Uint128::new(10000000000), "usdt"),
                ],
            );
            execute(deps.as_mut(), env_start.clone(), lp_info, mint_msg).unwrap();

            // 3. Advance time (Ensure EMA is stable)
            let mut env_swap = env_start.clone();
            env_swap.block.time = env_swap.block.time.plus_seconds(100);

            // 4. "The Setup Swap" (High Volume)
            // This moves the Spot Price away from the EMA
            let setup_swap_msg = ExecuteMsg::Swap {
                recipient: "whale".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128), // <-- INCREASED from 50,000
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            };
            let whale_info = message_info(
                &deps.api.addr_make("whale"),
                &[Coin::new(Uint128::new(500_000), "inj")],
            );
            execute(deps.as_mut(), env_swap.clone(), whale_info, setup_swap_msg).unwrap();

            // 5. "The Probe Swap" (Small Volume)
            // This happens immediately after (same block time), so Spot != EMA.
            // Volatility is high.
            let probe_swap_msg = ExecuteMsg::Swap {
                recipient: "trader".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            };
            let trader_info = message_info(
                &deps.api.addr_make("trader"),
                &[Coin::new(Uint128::new(1000), "inj")],
            );

            // Advance one second so the probe swap is a DIFFERENT block from
            // the setup swap. Phase 4 hardening freezes the dynamic fee
            // within a block (see oracle::update_oracle_and_fee) so that
            // attackers cannot sandwich-tax victims in the same block by
            // moving the price between swaps. Fees only evolve across
            // blocks, subject to `max_fee_change_per_second_ppm`. For this
            // test we set that cap to 0 (unlimited) so the probe's fee
            // reflects the full raw dynamic fee at the new price.
            let mut env_probe = env_swap.clone();
            env_probe.block.time = env_probe.block.time.plus_seconds(1);
            let res = execute(deps.as_mut(), env_probe, trader_info, probe_swap_msg).unwrap();

            // Extract amount_out
            res.attributes
                .iter()
                .find(|a| a.key == "amount_out")
                .unwrap()
                .value
                .parse::<u128>()
                .unwrap()
        };

        // --- Run Control Group (Static Fee) ---
        // Multiplier 0 means fee is always 0 (Base 0 + 0)
        let output_static = run_scenario(0);
        println!("Output (Static Fee): {}", output_static);

        // --- Run Test Group (Dynamic Fee) ---
        // Multiplier 500,000 means fee should spike
        let output_dynamic = run_scenario(500_000);
        println!("Output (Dynamic Fee): {}", output_dynamic);

        // --- Verification ---
        // The dynamic pool should return FEWER tokens because a fee was taken.
        assert!(
            output_dynamic < output_static,
            "Dynamic fee did not reduce output! Fee logic failed."
        );

        // Optional: specific check
        let diff = output_static - output_dynamic;
        println!("Fee Collected (Approx): {}", diff);
        assert!(diff > 10, "Fee collected was too small to be significant");
    }

    #[test]
    fn test_liquidity_gap_crossing() {
        // Scenario:
        // Range 1: [-200, -100] (Liquidity active)
        // Range 2: [100, 200] (Liquidity waiting)
        // Gap: [-100, 100] (No liquidity)
        // Current Tick: -150 (Inside Range 1)
        // Action: Swap to push price UP through the gap into Range 2.

        let mut deps = mock_dependencies();

        // 1. Setup Price at Tick -150 (approx 0.985)
        // We can't set exact tick in instantiate, so we set price that maps to -150.
        // Tick -150 -> SqrtPrice ... calculate or approximate.
        // easier way: Instantiate at 1.0 (Tick 0), Swap down to -150, then Mint, then Swap UP.
        // OR: Just instantiate at 1.0 (Tick 0) which is IN THE GAP.

        // Let's try: Instantiate at Tick 0. Mint [-200, -100] and [100, 200].
        // Current L should be 0.
        // Swap should immediately jump to -100 (if selling) or 100 (if buying).

        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(), // Tick 0
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 2. Mint ranges with GAP
        let lp_info = message_info(
            &deps.api.addr_make("lp"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );

        // Range A (Below)
        let mint_a = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: -100,
            amount: Uint128::from(1_000_000u128),
        };
        execute(deps.as_mut(), mock_env(), lp_info.clone(), mint_a).unwrap();

        // Range B (Above)
        let mint_b = ExecuteMsg::Mint {
            lower_tick: 100,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        execute(deps.as_mut(), mock_env(), lp_info.clone(), mint_b).unwrap();

        // 3. Verify Current State (L should be 0 because Tick 0 is in the gap)
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res).unwrap();
        assert_eq!(slot0.liquidity, Uint128::zero());
        assert_eq!(slot0.tick, 0);

        // 4. Swap UP (Buy Token 0)
        // We are at 0. Next active tick is 100.
        // The swap should jump from 0 -> 100 instantly consuming 0 input (because L=0).
        // Then it should consume input to move from 100 -> 200+.

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: false,                       // Buy Token 0 (Price Up)
            amount_specified: Uint128::from(1000u128), // Small amount
            sqrt_price_limit_x96: Uint256::from_str(
                "1461446703485210103287273052203988822378723970341",
            )
            .unwrap(), // Max
        };
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1000), "usdt")],
        );

        execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        // 5. Verify
        // The final tick should be > 100.
        let res_query = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res_query).unwrap();

        println!("Gap Test Final Tick: {}", slot0.tick);
        assert!(
            slot0.tick >= 100,
            "Should have crossed the gap to the next range"
        );
        assert!(
            slot0.liquidity > Uint128::zero(),
            "Should have picked up liquidity from Range B"
        );
    }

    #[test]
    fn test_unauthorized_burn() {
        let mut deps = mock_dependencies();
        // Setup ...
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 1. User A Mints
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::from(1_000_000u128),
        };
        let info_a = message_info(
            &deps.api.addr_make("user_a"),
            &[
                Coin::new(Uint128::new(1000000), "inj"),
                Coin::new(Uint128::new(1000000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), info_a, mint_msg).unwrap();

        // 2. User B Tries to Burn "User A's Range"
        // The contract interprets this as "Burn User B's Position at [-100, 100]".
        // Since User B has no position there, it should basically do nothing (burn 0) or fail if we enforced it.
        // In our current implementation logic, it loads default (empty) position, sees 0 liquidity, returns OK with 0 burned.

        let burn_msg = ExecuteMsg::Burn {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::from(500_000u128), // Try to burn
        };
        let info_b = message_info(&deps.api.addr_make("user_b"), &[]);

        // FIX: We expect an error here, not success.
        let err = execute(deps.as_mut(), mock_env(), info_b, burn_msg).unwrap_err();

        // 3. Verify Error
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert_eq!(msg, "Liquidity underflow");
            }
            _ => panic!("Expected Liquidity underflow error, got {:?}", err),
        }
    }

    #[test]
    fn test_overlapping_liquidity_math() {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let factory = deps.api.addr_make("factory");
        instantiate(deps.as_mut(), mock_env(), message_info(&factory, &[]), msg).unwrap();
        let lp_info = message_info(
            &deps.api.addr_make("lp"),
            &[
                Coin::new(Uint128::new(1000000000), "inj"),
                Coin::new(Uint128::new(1000000000), "usdt"),
            ],
        );

        let _lp = deps.api.addr_make("lp");

        // 1. Mint Range A: [-100, 100] -> L = 10,000,000
        execute(
            deps.as_mut(),
            mock_env(),
            lp_info.clone(),
            ExecuteMsg::Mint {
                lower_tick: -100,
                upper_tick: 100,
                amount: Uint128::from(10_000_000u128),
        },
        )
        .unwrap();

        // 2. Mint Range B: [0, 200] -> L = 5,000,000
        execute(
            deps.as_mut(),
            mock_env(),
            lp_info.clone(),
            ExecuteMsg::Mint {
                lower_tick: 0,
                upper_tick: 200,
                amount: Uint128::from(5_000_000u128),
        },
        )
        .unwrap();

        // 3. Verify Active Liquidity at Tick 0
        // Current Tick is 0. Both ranges active.
        // Total L should be 15,000,000
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res).unwrap();
        assert_eq!(slot0.liquidity, Uint128::from(15_000_000u128));

        // 4. Swap UP
        // Goal: Cross Tick 100 (where Range A ends), but stop before Tick 200.
        // Math:
        // - To reach Tick 100 (~0.5% price move): Need ~0.5% of L=15M ~= 75,000 input.
        // - To reach Tick 200 (another 0.5% move): Need ~0.5% of L=5M ~= 25,000 input.
        // - Total to reach 200 ~= 100,000.
        // Action: Swap 85,000. This should clear the first 75k (Cross 100) and use 10k into the second range.

        let swap_msg = ExecuteMsg::Swap {
            recipient: "trader".to_string(),
            zero_for_one: false, // Buy Token 0 (Price Up)
            amount_specified: Uint128::from(85_000u128),
            sqrt_price_limit_x96: Uint256::from_str(
                "1461446703485210103287273052203988822378723970341",
            )
            .unwrap(),
        };
        let trader = deps.api.addr_make("trader");
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&trader, &[Coin::new(Uint128::new(85000), "usdt")]),
            swap_msg,
        )
        .unwrap();

        // 5. Verify Final Liquidity
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetSlot0 {}).unwrap();
        let slot0: PoolState = from_json(&res).unwrap();

        println!("Final Tick: {}", slot0.tick);

        // Ensure we crossed into the [100, 200] bucket
        assert!(slot0.tick >= 100, "Should have crossed 100");
        assert!(slot0.tick < 200, "Should NOT have crossed 200");

        // Liquidity should now only be Range B (5,000,000)
        assert_eq!(
            slot0.liquidity,
            Uint128::from(5_000_000u128),
            "Should have dropped Alice's liquidity (10M) after crossing 100"
        );
    }

    // -----------------------------------------------------------------
    // Phase 2 security-fix regression tests
    // -----------------------------------------------------------------

    fn standard_pool_setup(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            cosmwasm_std::testing::MockQuerier,
        >,
    ) {
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();
    }

    #[test]
    fn phase2_mint_is_always_keyed_by_info_sender() {
        // Regression for CRIT-2 (audit): pool `Mint` previously accepted a
        // caller-chosen `recipient`, letting attackers credit or orphan
        // liquidity on arbitrary keys. The schema removed the field entirely.
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        let funds = vec![
            Coin::new(Uint128::new(10_000), "inj"),
            Coin::new(Uint128::new(10_000), "usdt"),
        ];

        // Alice and Bob mint into the same tick range.
        let alice = deps.api.addr_make("alice");
        let bob = deps.api.addr_make("bob");
        let mint = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000u128),
        };
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&alice, &funds),
            mint.clone(),
        )
        .unwrap();
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&bob, &funds),
            mint,
        )
        .unwrap();

        // Each position must exist ONLY under its own caller's key.
        let resp_alice = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::GetPosition {
                owner: alice.to_string(),
                tick_lower: -200,
                tick_upper: 200,
            },
        )
        .unwrap();
        let pos_alice: choice_clmm_common::pool::PositionInfoResponse =
            from_json(&resp_alice).unwrap();
        assert_eq!(pos_alice.liquidity, Uint128::from(1_000u128));

        let resp_bob = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::GetPosition {
                owner: bob.to_string(),
                tick_lower: -200,
                tick_upper: 200,
            },
        )
        .unwrap();
        let pos_bob: choice_clmm_common::pool::PositionInfoResponse =
            from_json(&resp_bob).unwrap();
        assert_eq!(pos_bob.liquidity, Uint128::from(1_000u128));

        // Bob cannot burn Alice's liquidity even though they share the range.
        let burn = ExecuteMsg::Burn {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(2_000u128),
        };
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&bob, &[]),
            burn,
        )
        .unwrap_err();
        // Bob only has 1000; trying to burn 2000 must fail per-position.
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(msg.contains("Liquidity underflow"), "got: {}", msg);
            }
            _ => panic!("expected Liquidity underflow, got {:?}", err),
        }
    }

    #[test]
    fn phase2_reject_mint_amount_above_i128_max() {
        // Regression for CRIT-7 (audit): pre-fix, `u128 as i128` silently
        // wrapped to a negative delta for amounts > i128::MAX and corrupted
        // tick accounting. The fix rejects such amounts up front.
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        let too_big = Uint128::new(u128::MAX);
        let lp = deps.api.addr_make("lp");
        let info = message_info(
            &lp,
            &[
                Coin::new(Uint128::new(u128::MAX / 2), "inj"),
                Coin::new(Uint128::new(u128::MAX / 2), "usdt"),
            ],
        );
        let err = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::Mint {
                lower_tick: -200,
                upper_tick: 200,
                amount: too_big,
            },
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(msg.contains("i128::MAX"), "got: {}", msg);
            }
            _ => panic!("expected i128::MAX overflow error, got {:?}", err),
        }
    }

    #[test]
    fn phase2_reject_mint_exceeding_max_liquidity_per_tick() {
        // V3 parity: every pool enforces MAX_LIQUIDITY_PER_TICK =
        // u128::MAX / num_ticks. Pre-fix Choice had no cap, letting a single
        // position cause per-tick liquidity math to overflow or become
        // non-traversable.
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        // tick_spacing = 10, so num_ticks ~= (2 * 887270 / 10) + 1 = 177455.
        // MAX_LIQUIDITY_PER_TICK = u128::MAX / 177455 ≈ 1.917e33.
        // We ask for one more than that to provoke the cap check.
        let num_ticks: u128 =
            (((choice_clmm_math::tick_math::MAX_TICK / 10) * 2) as u128) + 1;
        let cap = u128::MAX / num_ticks;
        let over = cap.saturating_add(1);

        let lp = deps.api.addr_make("lp_whale");
        let info = message_info(
            &lp,
            &[
                Coin::new(Uint128::new(u128::MAX / 2), "inj"),
                Coin::new(Uint128::new(u128::MAX / 2), "usdt"),
            ],
        );

        // `over` still fits in i128 (< i128::MAX), so we should hit the
        // MAX_LIQUIDITY_PER_TICK check specifically, not the i128 cap.
        assert!(over <= i128::MAX as u128);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::Mint {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::new(over),
            },
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(
                    msg.contains("MAX_LIQUIDITY_PER_TICK"),
                    "got: {}", msg
                );
            }
            _ => panic!("expected MAX_LIQUIDITY_PER_TICK error, got {:?}", err),
        }
    }

    #[test]
    fn phase2_zero_mint_amount_rejected() {
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);
        let lp = deps.api.addr_make("lp");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&lp, &[]),
            ExecuteMsg::Mint {
                lower_tick: -10,
                upper_tick: 10,
                amount: Uint128::zero(),
            },
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(msg.contains("must be > 0"), "got: {}", msg);
            }
            _ => panic!("expected zero-amount rejection, got {:?}", err),
        }
    }

    #[test]
    fn phase2_cw20_receive_refund_on_liquidity_exhaustion() {
        // Regression for HIGH-7 (audit): the CW20 Receive path used to
        // silently strand unconsumed input in the pool on partial fills. The
        // fix appends a CW20 `Transfer` back to the original sender for the
        // delta `total_sent - amount_in`.
        //
        // Setup: native token0 + CW20 token1 (honors AssetInfo ordering:
        // Native < Token). Zero pool liquidity so the swap must exit with
        // amount_in == 0 and refund 100% of the input.
        use crate::actions::swap::execute_swap_exact_input_cw20;

        let mut deps = mock_dependencies();
        let cw20_addr = deps.api.addr_make("cw20_usdt");
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: AssetInfo::Token {
                contract_addr: cw20_addr.to_string(),
            },
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // Trader sends CW20 token1 into the pool via Receive hook (selling
        // token1, so `zero_for_one = false`). With zero pool liquidity the
        // swap loop makes no progress and `amount_in` ends up at 0.
        let trader = deps.api.addr_make("trader");
        let sent = Uint128::new(100_000);
        let res = execute_swap_exact_input_cw20(
            deps.as_mut(),
            mock_env(),
            trader.clone(),
            sent,
            false, // one_for_zero: selling token1 (CW20) for token0
            Uint128::zero(),
            None,
            None,
        )
        .unwrap();

        // Expect exactly one message: a CW20 Transfer refund of the full amount.
        assert_eq!(res.messages.len(), 1, "expected one refund message");
        let msg = &res.messages[0].msg;
        let cosmwasm_std::CosmosMsg::Wasm(cosmwasm_std::WasmMsg::Execute {
            contract_addr,
            msg: inner,
            funds,
        }) = msg
        else {
            panic!("expected WasmMsg::Execute, got {:?}", msg);
        };
        assert_eq!(contract_addr, &cw20_addr.to_string());
        assert!(funds.is_empty());
        let cw20_msg: cw20::Cw20ExecuteMsg = from_json(inner).unwrap();
        match cw20_msg {
            cw20::Cw20ExecuteMsg::Transfer { recipient, amount } => {
                assert_eq!(recipient, trader.to_string());
                assert_eq!(amount, sent);
            }
            other => panic!("expected CW20 Transfer refund, got {:?}", other),
        }
    }

    // ------------------------------------------------------------
    // Phase 4 — dynamic fee oracle hardening regression tests
    // ------------------------------------------------------------

    /// Set up a pool whose base fee is 0 and max fee is 100_000 ppm (10%),
    /// with the volatility multiplier high enough that `raw_dynamic_fee`
    /// will naturally saturate at `max_fee_ppm` once the price moves.
    /// `max_fee_change_per_second_ppm` is caller-controlled.
    fn setup_oracle_pool(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            cosmwasm_std::testing::MockQuerier,
        >,
        max_change_per_sec: u32,
    ) -> cosmwasm_std::Env {
        let env = mock_env();
        let factory = deps.api.addr_make("factory");
        let lp = deps.api.addr_make("lp");
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 0,
                max_fee_ppm: 100_000, // 10%
                volatility_multiplier: 500_000,
                ema_halflife_seconds: 100,
                max_fee_change_per_second_ppm: max_change_per_sec,
            },
            initial_sqrt_price: get_price_one(),
        };
        instantiate(
            deps.as_mut(),
            env.clone(),
            message_info(&factory, &[]),
            msg,
        )
        .unwrap();

        let lp_info = message_info(
            &lp,
            &[
                Coin::new(Uint128::new(10_000_000_000), "inj"),
                Coin::new(Uint128::new(10_000_000_000), "usdt"),
            ],
        );
        execute(
            deps.as_mut(),
            env.clone(),
            lp_info,
            ExecuteMsg::Mint {
                lower_tick: -2000,
                upper_tick: 2000,
                amount: Uint128::from(10_000_000u128),
            },
        )
        .unwrap();
        env
    }

    /// Runs a large whale swap then a probe swap `probe_delay_sec` later and
    /// returns the probe's output and the effective fee from the response.
    fn whale_then_probe(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            cosmwasm_std::testing::MockQuerier,
        >,
        env_start: cosmwasm_std::Env,
        probe_delay_sec: u64,
    ) -> (u128, u128) {
        // Whale moves the price.
        let mut env_whale = env_start.clone();
        env_whale.block.time = env_whale.block.time.plus_seconds(200);
        let whale = deps.api.addr_make("whale");
        let whale_info = message_info(
            &whale,
            &[Coin::new(Uint128::new(500_000), "inj")],
        );
        execute(
            deps.as_mut(),
            env_whale.clone(),
            whale_info,
            ExecuteMsg::Swap {
                recipient: "whale".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let mut env_probe = env_whale.clone();
        env_probe.block.time = env_probe.block.time.plus_seconds(probe_delay_sec);

        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1_000), "inj")],
        );
        let res = execute(
            deps.as_mut(),
            env_probe,
            trader_info,
            ExecuteMsg::Swap {
                recipient: "trader".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let out = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        let fee_in = res
            .attributes
            .iter()
            .find(|a| a.key == "final_price")
            .map(|_| 0u128)
            .unwrap_or(0);
        let _ = fee_in;
        // Rough fee recovery: fee = 1000 - amount_out - (amount_out's fair at 0 fee).
        // For this regression we just return `amount_out` and let the caller compare.
        (out, 1_000u128)
    }

    #[test]
    fn phase4_same_block_swaps_pay_same_frozen_fee() {
        // Sandwich-in-same-block protection: once the first swap of a block
        // has committed a fee, subsequent swaps in the same block must pay
        // that SAME fee. Previously the second swap re-derived the fee
        // using post-first-swap slot0 vs. stale EMA and got slammed.
        let mut deps = mock_dependencies();
        // Rate limit doesn't matter here — same block freezes the fee
        // regardless of rate.
        let env_start = setup_oracle_pool(&mut deps, 1_000_000);

        // Advance to some block time.
        let mut env = env_start.clone();
        env.block.time = env.block.time.plus_seconds(200);

        // Attacker swap A (same block).
        let whale_info = message_info(
            &deps.api.addr_make("whale"),
            &[Coin::new(Uint128::new(300_000), "inj")],
        );
        let res_a = execute(
            deps.as_mut(),
            env.clone(),
            whale_info,
            ExecuteMsg::Swap {
                recipient: "whale".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(300_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        // Victim swap B (SAME block).
        let victim = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1_000), "inj")],
        );
        let res_b = execute(
            deps.as_mut(),
            env.clone(),
            victim,
            ExecuteMsg::Swap {
                recipient: "trader".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let fee_a = res_a
            .attributes
            .iter()
            .find(|a| a.key == "fee_pips")
            .map(|a| a.value.parse::<u128>().unwrap())
            .unwrap_or(0);
        let fee_b = res_b
            .attributes
            .iter()
            .find(|a| a.key == "fee_pips")
            .map(|a| a.value.parse::<u128>().unwrap())
            .unwrap_or(0);
        // fee_pips isn't emitted as an attribute today, but this test still
        // asserts the outcome: same-block victim pays NO more than whale.
        // Instead we check amount_out ratios (both are zero_for_one with same
        // liquidity window, so proportional inputs should yield proportional
        // outputs iff fee is constant).
        let _ = (fee_a, fee_b);
        let out_a = res_a
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        let out_b = res_b
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        // Heuristic: victim's per-token output should be similar to whale's.
        // Under the old vulnerable behavior, victim's output was dramatically
        // lower due to inflated fee. We use a conservative bound: victim gets
        // at least 90% of the whale's per-token rate.
        let whale_rate = out_a * 1000 / 300_000;
        let victim_rate = out_b * 1000 / 1_000;
        assert!(
            victim_rate * 10 >= whale_rate * 9,
            "victim swapped at far worse rate than whale in same block: \
             whale_rate={} victim_rate={}",
            whale_rate,
            victim_rate,
        );
    }

    #[test]
    fn phase4_rate_limit_caps_cross_block_fee_rise() {
        // Cross-block sandwich-tax protection: fee can only rise by
        // `max_fee_change_per_second_ppm * delta` per block.
        //
        // With rate limit 100 ppm/sec and a 1-second block, the probe in the
        // block following the whale must pay AT MOST base_fee + 100 ppm = 100 ppm.
        let mut deps = mock_dependencies();
        let env_start = setup_oracle_pool(&mut deps, 100);
        let (out_with_limit, sent) = whale_then_probe(&mut deps, env_start, 1);

        // Re-run the scenario with rate limiting DISABLED for a baseline.
        let mut deps2 = mock_dependencies();
        let env_start2 = setup_oracle_pool(&mut deps2, 0);
        let (out_no_limit, _) = whale_then_probe(&mut deps2, env_start2, 1);

        // With rate limiting, the probe gets MORE output (less fee taken)
        // than without rate limiting. This is the exact behavior that
        // defeats the cross-block sandwich-tax attack: the victim in the
        // block after the attacker's swap cannot be slammed with a near-max
        // fee, because the fee can only climb by `rate * delta` per block.
        assert!(
            out_with_limit > out_no_limit,
            "rate limit should reduce fee, giving the probe more output: \
             limited={} unlimited={} sent={}",
            out_with_limit,
            out_no_limit,
            sent,
        );
    }

    #[test]
    fn phase4_rate_limit_still_allows_gradual_climb() {
        // Given enough elapsed time (many seconds), the rate-limited fee
        // should eventually catch up to the raw dynamic fee. This ensures
        // the rate limit doesn't PERMANENTLY cap the fee below its natural
        // level for sustained volatility.
        let mut deps = mock_dependencies();
        let env_start = setup_oracle_pool(&mut deps, 100);

        // Whale manipulates.
        let mut env_whale = env_start.clone();
        env_whale.block.time = env_whale.block.time.plus_seconds(200);
        let whale1 = deps.api.addr_make("whale");
        execute(
            deps.as_mut(),
            env_whale.clone(),
            message_info(&whale1, &[Coin::new(Uint128::new(500_000), "inj")]),
            ExecuteMsg::Swap {
                recipient: "whale".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        // After 1 sec: fee capped at base + 100 ppm
        let (out_fast, _) = whale_then_probe_from(&mut deps, &env_whale, 1);
        // Wait much longer: fee catches up to the raw dynamic fee.
        let mut deps2 = mock_dependencies();
        let env_start2 = setup_oracle_pool(&mut deps2, 100);
        let mut env_whale2 = env_start2.clone();
        env_whale2.block.time = env_whale2.block.time.plus_seconds(200);
        let whale2 = deps2.api.addr_make("whale");
        execute(
            deps2.as_mut(),
            env_whale2.clone(),
            message_info(&whale2, &[Coin::new(Uint128::new(500_000), "inj")]),
            ExecuteMsg::Swap {
                recipient: "whale".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();
        let (out_slow, _) = whale_then_probe_from(&mut deps2, &env_whale2, 10_000);

        // After 10k seconds, the cap (100 ppm/sec * 10000 = 1e6 ppm) exceeds
        // the natural raw fee (capped by max_fee_ppm = 100k ppm), so fee
        // reaches its natural level. The probe therefore pays a HIGHER fee
        // than the 1-second probe did.
        assert!(
            out_slow <= out_fast,
            "waiting longer should let the fee catch up (lower output): fast={} slow={}", out_fast, out_slow
        );
    }

    /// Variant that takes a pre-advanced env (already at the whale's block)
    /// and runs the probe after `probe_delay_sec` additional seconds.
    fn whale_then_probe_from(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            cosmwasm_std::testing::MockQuerier,
        >,
        env_whale: &cosmwasm_std::Env,
        probe_delay_sec: u64,
    ) -> (u128, u128) {
        let mut env_probe = env_whale.clone();
        env_probe.block.time = env_probe.block.time.plus_seconds(probe_delay_sec);
        let trader_info = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(1_000), "inj")],
        );
        let res = execute(
            deps.as_mut(),
            env_probe,
            trader_info,
            ExecuteMsg::Swap {
                recipient: "trader".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();
        let out = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_out")
            .unwrap()
            .value
            .parse::<u128>()
            .unwrap();
        (out, 1_000u128)
    }

    #[test]
    fn phase4_stale_oracle_falls_back_to_base_fee() {
        // If the oracle hasn't been touched in > 1 hour, get_dynamic_fee
        // returns base_fee_ppm. This protects quote/simulation paths from
        // using arbitrarily-stale EMA data.
        use crate::core::oracle::get_dynamic_fee;

        let mut deps = mock_dependencies();
        let env_start = setup_oracle_pool(&mut deps, 100);

        // Fast-forward 2 hours (> MAX_ORACLE_AGE_SECONDS).
        let mut env_later = env_start.clone();
        env_later.block.time = env_later.block.time.plus_seconds(7200);

        let fee = get_dynamic_fee(&deps.storage, &env_later, get_price_one()).unwrap();
        // base_fee_ppm in setup_oracle_pool is 0.
        assert_eq!(fee, 0);
    }
}

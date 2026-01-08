#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use crate::contract::{execute, instantiate, query};
    use crate::error::ContractError;
    use crate::state::PoolConfig;
    use choice_clmm_common::pool::{ExecuteMsg, FeeConfig, InstantiateMsg, PoolState, QueryMsg};
    use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
    use cosmwasm_std::{from_json, BankMsg, Coin, StdError, Uint128, Uint256};

    // Helper to mock Q64.96 representation of "1.0"
    // 2^96 = 79228162514264337593543950336
    fn get_price_one() -> Uint256 {
        Uint256::from_u128(1) << 96
    }

    #[test]
    fn test_proper_instantiation() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            token0: "inj".to_string(),
            token1: "peggy0xdac".to_string(), // USDT
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };

        let info = message_info(&deps.api.addr_make("factory_addr"), &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.attributes[0].value, "instantiate");

        // 1. Test Config Query
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetConfig {}).unwrap();
        let config: PoolConfig = from_json(&res).unwrap();
        assert_eq!(config.token0, "inj");
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
            token0: "usdt".to_string(), // Alphabetically after "inj"
            token1: "inj".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 5000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 0,
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
            token0: "inj".to_string(),
            token1: "peggy0xdac".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 2. Mint Liquidity
        // Range: -200 to 200. Current Tick 0 is inside.
        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("user_addr").to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "peggy0xdac".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // Mint Scenario: Range 10 to 20 (Strictly ABOVE current price 0)
        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("user").to_string(),
            lower_tick: 10,
            upper_tick: 20,
            amount: Uint128::from(1000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000, // 0.3%
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 2. Mint Liquidity
        // Range: -200 to 200. Amount: 1,000,000
        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("lp_provider").to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("lp").to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("lp").to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("lp").to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
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
            recipient: lp_info.sender.to_string(), // <--- FIX: Use the actual address string
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
                token0: "inj".to_string(),
                token1: "usdt".to_string(),
                tick_spacing: 10,
                fee_config: FeeConfig {
                    base_fee_ppm: 0, // 0% Base fee to isolate dynamic effects
                    max_fee_ppm: 100000,
                    volatility_multiplier: multiplier, // Variable
                    ema_halflife_seconds: 100,
                },
                initial_sqrt_price: get_price_one(),
            };
            let creator = message_info(&deps.api.addr_make("factory"), &[]);
            // reset storage for clean run
            instantiate(deps.as_mut(), env_start.clone(), creator, msg).unwrap();

            // 2. Mint Liquidity
            let mint_msg = ExecuteMsg::Mint {
                recipient: deps.api.addr_make("lp").to_string(),
                lower_tick: -2000,
                upper_tick: 2000,
                amount: Uint128::from(10_000_000u128),
                data: None,
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

            let res = execute(deps.as_mut(), env_swap, trader_info, probe_swap_msg).unwrap();

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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 600,
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
            recipient: deps.api.addr_make("lp").to_string(),
            lower_tick: -200,
            upper_tick: -100,
            amount: Uint128::from(1_000_000u128),
            data: None,
        };
        execute(deps.as_mut(), mock_env(), lp_info.clone(), mint_a).unwrap();

        // Range B (Above)
        let mint_b = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("lp").to_string(),
            lower_tick: 100,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 100,
                ema_halflife_seconds: 600,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // 1. User A Mints
        let mint_msg = ExecuteMsg::Mint {
            recipient: deps.api.addr_make("user_a").to_string(),
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::from(1_000_000u128),
            data: None,
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
            token0: "inj".to_string(),
            token1: "usdt".to_string(),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                volatility_multiplier: 0,
                ema_halflife_seconds: 600,
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

        let lp = deps.api.addr_make("lp");

        // 1. Mint Range A: [-100, 100] -> L = 10,000,000
        execute(
            deps.as_mut(),
            mock_env(),
            lp_info.clone(),
            ExecuteMsg::Mint {
                recipient: lp.to_string(),
                lower_tick: -100,
                upper_tick: 100,
                amount: Uint128::from(10_000_000u128),
                data: None,
            },
        )
        .unwrap();

        // 2. Mint Range B: [0, 200] -> L = 5,000,000
        execute(
            deps.as_mut(),
            mock_env(),
            lp_info.clone(),
            ExecuteMsg::Mint {
                recipient: lp.to_string(),
                lower_tick: 0,
                upper_tick: 200,
                amount: Uint128::from(5_000_000u128),
                data: None,
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
}

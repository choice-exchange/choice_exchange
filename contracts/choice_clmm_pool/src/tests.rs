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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
                variable_fee_control: 0,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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

    /// Regression: mint must refund any surplus native funds. Previously the
    /// pool would silently absorb excess into its reserves, and coins of an
    /// unrelated denom sent with the tx were stranded forever.
    #[test]
    fn mint_refunds_excess_native_funds() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("peggy0xdac"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let user = deps.api.addr_make("user_addr");
        let user_info = message_info(
            &user,
            &[
                // Pool needs ~10 of each for L=1000 at [-200, 200]. Send 1M of
                // each plus 500 of an unrelated denom — all three surpluses
                // should refund.
                Coin::new(Uint128::new(1_000_000), "inj"),
                Coin::new(Uint128::new(1_000_000), "peggy0xdac"),
                Coin::new(Uint128::new(500), "stranded"),
            ],
        );
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1000u128),
        };
        let res = execute(deps.as_mut(), mock_env(), user_info, mint_msg).unwrap();

        let consumed0: Uint128 = res
            .attributes
            .iter()
            .find(|a| a.key == "amount0_consumed")
            .unwrap()
            .value
            .parse()
            .unwrap();
        let consumed1: Uint128 = res
            .attributes
            .iter()
            .find(|a| a.key == "amount1_consumed")
            .unwrap()
            .value
            .parse()
            .unwrap();

        let refund = res
            .messages
            .iter()
            .find_map(|m| match &m.msg {
                cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount })
                    if to_address == user.as_str() =>
                {
                    Some(amount.clone())
                }
                _ => None,
            })
            .expect("expected a refund BankMsg::Send");

        let find = |denom: &str| {
            refund
                .iter()
                .find(|c| c.denom == denom)
                .map(|c| c.amount)
                .unwrap_or_default()
        };
        assert_eq!(find("inj"), Uint128::new(1_000_000) - consumed0);
        assert_eq!(find("peggy0xdac"), Uint128::new(1_000_000) - consumed1);
        assert_eq!(find("stranded"), Uint128::new(500));
    }

    /// Regression: a swap must refund the surplus of the input denom AND the
    /// full amount of any other denom attached — including the pool's *other*
    /// token. Previously `apply_swap` only refunded the input-denom surplus, so
    /// any extra coins sent with the swap were silently absorbed into reserves
    /// and stranded forever (mint already refunded them; swap did not).
    #[test]
    fn swap_refunds_excess_and_unrelated_native_funds() {
        let mut deps = mock_dependencies();

        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // Deep liquidity so the small swap fully consumes its ~1000 input.
        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        let lp_info = message_info(
            &deps.api.addr_make("lp_provider"),
            &[
                Coin::new(Uint128::new(1_000_000_000), "inj"),
                Coin::new(Uint128::new(1_000_000_000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        let trader = deps.api.addr_make("trader");
        let recipient = deps.api.addr_make("recipient");
        let min_sqrt_ratio = Uint256::from(4295128739u128 + 1);

        let swap_msg = ExecuteMsg::Swap {
            recipient: recipient.to_string(),
            zero_for_one: true, // selling token0 (inj) for token1 (usdt)
            amount_specified: Uint128::from(1000u128),
            sqrt_price_limit_x96: min_sqrt_ratio,
        };

        // Attach: input inj with a surplus, the *other* pool token (usdt), and
        // an unrelated denom. All non-consumed coins must be refunded to sender.
        let trader_info = message_info(
            &trader,
            &[
                Coin::new(Uint128::new(1500), "inj"),
                Coin::new(Uint128::new(700), "usdt"),
                Coin::new(Uint128::new(500), "stranded"),
            ],
        );
        let res = execute(deps.as_mut(), mock_env(), trader_info, swap_msg).unwrap();

        let amount_in: Uint128 = res
            .attributes
            .iter()
            .find(|a| a.key == "amount_in")
            .unwrap()
            .value
            .parse()
            .unwrap();
        assert!(!amount_in.is_zero(), "swap should consume some input");

        // The refund BankMsg goes to the SENDER (trader); the swap output (usdt)
        // goes to the distinct `recipient`, so we can disambiguate by address.
        let refund = res
            .messages
            .iter()
            .find_map(|m| match &m.msg {
                cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount })
                    if to_address == trader.as_str() =>
                {
                    Some(amount.clone())
                }
                _ => None,
            })
            .expect("expected a refund BankMsg::Send to the trader");

        let find = |denom: &str| {
            refund
                .iter()
                .find(|c| c.denom == denom)
                .map(|c| c.amount)
                .unwrap_or_default()
        };
        // Input-denom surplus, plus the other pool token and the unrelated denom
        // in full.
        assert_eq!(find("inj"), Uint128::new(1500) - amount_in);
        assert_eq!(find("usdt"), Uint128::new(700));
        assert_eq!(find("stranded"), Uint128::new(500));

        // Sanity: the swap output (usdt) is a *separate* message to `recipient`.
        let out_to_recipient = res.messages.iter().any(|m| {
            matches!(
                &m.msg,
                cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, .. })
                    if to_address == recipient.as_str()
            )
        });
        assert!(out_to_recipient, "swap output should go to recipient");
    }

    /// Regression: entrypoints that don't consume funds must REJECT attached
    /// native coins rather than silently absorbing them into the pool. Only
    /// Mint / Swap* are payable.
    #[test]
    fn nonpayable_entrypoints_reject_attached_funds() {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        let user = deps.api.addr_make("user");
        let funds = [Coin::new(Uint128::new(1), "inj")];

        let cases = vec![
            ExecuteMsg::Burn {
                lower_tick: -10,
                upper_tick: 10,
                amount: Uint128::zero(),
            },
            ExecuteMsg::Collect {
                recipient: user.to_string(),
                lower_tick: -10,
                upper_tick: 10,
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
            ExecuteMsg::Flash {
                recipient: user.to_string(),
                amount0: Uint128::new(1),
                amount1: Uint128::zero(),
                data: cosmwasm_std::Binary::default(),
            },
            ExecuteMsg::CollectProtocol {
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        ];

        for m in cases {
            let label = format!("{:?}", m);
            let err = match execute(deps.as_mut(), mock_env(), message_info(&user, &funds), m) {
                Err(e) => e,
                Ok(_) => panic!("{} must reject attached funds", label),
            };
            assert!(
                err.to_string().contains("no funds"),
                "{}: unexpected error {}",
                label,
                err
            );
        }
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                assert_eq!(to_address, &deps.api.addr_make("trader").to_string());
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
        // Burn/Collect take no funds — use an empty-funds info for the same LP.
        let lp_nopay = message_info(&deps.api.addr_make(lp_addr), &[]);
        let res = execute(deps.as_mut(), mock_env(), lp_nopay.clone(), burn_msg).unwrap();

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
            recipient: deps.api.addr_make(lp_addr).to_string(),
            lower_tick: -200,
            upper_tick: 200,
            amount0_requested: max_collect,
            amount1_requested: max_collect,
        };
        let res_collect = execute(deps.as_mut(), mock_env(), lp_nopay, collect_msg).unwrap();

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
                    variable_fee_control: multiplier, // Variable
                    max_volatility_accumulator: 2_000,
                    volatility_decay_seconds: 100,
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
                recipient: deps.api.addr_make("whale").to_string(),
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
                recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 0,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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

        // 3. Verify Error — user B has no position at this tick range.
        match err {
            ContractError::PositionNotFound {} => {}
            _ => panic!("Expected PositionNotFound, got {:?}", err),
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
                variable_fee_control: 0,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            recipient: deps.api.addr_make("trader").to_string(),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
        execute(deps.as_mut(), mock_env(), message_info(&bob, &funds), mint).unwrap();

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
        let pos_bob: choice_clmm_common::pool::PositionInfoResponse = from_json(&resp_bob).unwrap();
        assert_eq!(pos_bob.liquidity, Uint128::from(1_000u128));

        // Bob cannot burn Alice's liquidity even though they share the range.
        let burn = ExecuteMsg::Burn {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(2_000u128),
        };
        let err = execute(deps.as_mut(), mock_env(), message_info(&bob, &[]), burn).unwrap_err();
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
        let num_ticks: u128 = (((choice_clmm_math::tick_math::MAX_TICK / 10) * 2) as u128) + 1;
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
                assert!(msg.contains("MAX_LIQUIDITY_PER_TICK"), "got: {}", msg);
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
    fn burn_zero_rolls_fees_into_tokens_owed() {
        // Regression: pool.Burn(amount=0) previously short-circuited to
        // Response::default(), skipping update_position. The NFT manager's
        // Collect flow fires Burn(0) to roll accrued fees into
        // position.tokens_owed (V3 pattern) before calling Collect. Without
        // update_position running, Collect paid out zero and the manager
        // decremented the NFT's local owed balance anyway — stranding fees.
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        let lp = deps.api.addr_make("lp");
        let lp_info = message_info(
            &lp,
            &[
                Coin::new(Uint128::new(1_000_000_000), "inj"),
                Coin::new(Uint128::new(1_000_000_000), "usdt"),
            ],
        );
        execute(
            deps.as_mut(),
            mock_env(),
            lp_info.clone(),
            ExecuteMsg::Mint {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::from(1_000_000u128),
            },
        )
        .unwrap();

        let trader = deps.api.addr_make("trader");
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&trader, &[Coin::new(Uint128::new(1000), "inj")]),
            ExecuteMsg::Swap {
                recipient: trader.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        // Burn(0) must succeed and advance the fee accumulator — principal is 0.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&lp, &[]),
            ExecuteMsg::Burn {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::zero(),
            },
        )
        .unwrap();
        let get = |key: &str| {
            res.attributes
                .iter()
                .find(|a| a.key == key)
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(get("action"), "burn");
        assert_eq!(get("liquidity_burned"), "0");
        assert_eq!(get("amount0_burned"), "0");
        assert_eq!(get("amount1_burned"), "0");

        // Collect should now drain the fees that Burn(0) rolled into tokens_owed.
        let collect_res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&lp, &[]),
            ExecuteMsg::Collect {
                recipient: lp.to_string(),
                lower_tick: -200,
                upper_tick: 200,
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
        )
        .unwrap();
        let collected_0: u128 = collect_res
            .attributes
            .iter()
            .find(|a| a.key == "amount0")
            .unwrap()
            .value
            .parse()
            .unwrap();
        let collected_1: u128 = collect_res
            .attributes
            .iter()
            .find(|a| a.key == "amount1")
            .unwrap()
            .value
            .parse()
            .unwrap();
        // Swap was zero_for_one, so fees accrue in token0.
        assert!(
            collected_0 > 0,
            "Burn(0) should have credited fees; got amount0=0"
        );
        assert_eq!(collected_1, 0);
    }

    #[test]
    fn burn_clears_ticks_on_final_exit() {
        // V3 parity: when the last position referencing a tick leaves, the tick
        // entry should be deleted and the bitmap bit flipped off. Prior code
        // kept `initialized=true` forever, bloating state and leaking stale
        // `fee_growth_outside` snapshots into any future re-mint at the same
        // ticks. Fee math stays consistent because `get_fee_growth_inside`
        // now treats missing ticks as default (fee_growth_outside = 0).
        use crate::state::{TICKS, TICK_BITMAP};

        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        let lp = deps.api.addr_make("lp");
        let info = message_info(
            &lp,
            &[
                Coin::new(Uint128::new(10_000_000), "inj"),
                Coin::new(Uint128::new(10_000_000), "usdt"),
            ],
        );

        execute(
            deps.as_mut(),
            mock_env(),
            info.clone(),
            ExecuteMsg::Mint {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::from(1_000_000u128),
            },
        )
        .unwrap();

        // Both ticks present and bitmap bit set.
        assert!(TICKS.may_load(&deps.storage, -200).unwrap().is_some());
        assert!(TICKS.may_load(&deps.storage, 200).unwrap().is_some());

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&lp, &[]),
            ExecuteMsg::Burn {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::from(1_000_000u128),
            },
        )
        .unwrap();

        // After full burn the tick entries should be gone.
        assert!(
            TICKS.may_load(&deps.storage, -200).unwrap().is_none(),
            "lower tick should be cleared"
        );
        assert!(
            TICKS.may_load(&deps.storage, 200).unwrap().is_none(),
            "upper tick should be cleared"
        );

        // Bitmap: flipping twice (init on mint, clear on burn) returns the word
        // to 0, so the word either doesn't exist or is zero.
        for word_pos in [-1i16, 0, 1] {
            let word = TICK_BITMAP.may_load(&deps.storage, word_pos).unwrap();
            if let Some(w) = word {
                assert_eq!(
                    w,
                    cosmwasm_std::Uint256::zero(),
                    "bitmap word {} should be zero after clear",
                    word_pos
                );
            }
        }
    }

    #[test]
    fn burn_zero_rejects_unknown_position() {
        // Without this guard, update_position would create an empty POSITIONS
        // entry for any caller+tick triple — storage bloat vector.
        let mut deps = mock_dependencies();
        standard_pool_setup(&mut deps);

        let stranger = deps.api.addr_make("stranger");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&stranger, &[]),
            ExecuteMsg::Burn {
                lower_tick: -200,
                upper_tick: 200,
                amount: Uint128::zero(),
            },
        )
        .unwrap_err();
        match err {
            ContractError::PositionNotFound {} => {}
            other => panic!("expected PositionNotFound, got {:?}", other),
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
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
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
            vec![], // no native funds attached to the Receive envelope
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
                variable_fee_control: 500_000,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 100,
                max_fee_change_per_second_ppm: max_change_per_sec,
            },
            initial_sqrt_price: get_price_one(),
        };
        instantiate(deps.as_mut(), env.clone(), message_info(&factory, &[]), msg).unwrap();

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

    /// A swap whose `recipient` is not a valid bech32 address is rejected up
    /// front (parity with flash/collect), rather than failing opaquely inside
    /// the output `BankMsg`/`Cw20` transfer.
    #[test]
    fn swap_rejects_invalid_recipient() {
        let mut deps = mock_dependencies();
        let env = setup_oracle_pool(&mut deps, 0);

        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(1_000), "inj")]);

        let err = execute(
            deps.as_mut(),
            env,
            trader_info,
            ExecuteMsg::Swap {
                recipient: "not-a-valid-bech32".to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128739u128 + 1),
            },
        )
        .unwrap_err();

        // Comes from `deps.api.addr_validate` in `apply_swap`.
        assert!(
            matches!(err, ContractError::Std(_)),
            "expected an address-validation Std error, got {:?}",
            err
        );

        // Positive control: the SAME swap with a VALID recipient succeeds. Since
        // the recipient is the only thing that changed, this proves the error
        // above is attributable to recipient validation — not an unrelated Std
        // failure (insufficient funds, price limit, …) that `matches!(Std(_))`
        // would otherwise accept for the wrong reason.
        let mut deps_ok = mock_dependencies();
        let env_ok = setup_oracle_pool(&mut deps_ok, 0);
        let good_recipient = deps_ok.api.addr_make("good_recipient").to_string();
        let trader2 = deps_ok.api.addr_make("trader");
        execute(
            deps_ok.as_mut(),
            env_ok,
            message_info(&trader2, &[Coin::new(Uint128::new(1_000), "inj")]),
            ExecuteMsg::Swap {
                recipient: good_recipient,
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128739u128 + 1),
            },
        )
        .expect("swap with a valid recipient must succeed");
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
        let whale_info = message_info(&whale, &[Coin::new(Uint128::new(500_000), "inj")]);
        execute(
            deps.as_mut(),
            env_whale.clone(),
            whale_info,
            ExecuteMsg::Swap {
                recipient: whale.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let mut env_probe = env_whale.clone();
        env_probe.block.time = env_probe.block.time.plus_seconds(probe_delay_sec);

        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(1_000), "inj")]);
        let res = execute(
            deps.as_mut(),
            env_probe,
            trader_info,
            ExecuteMsg::Swap {
                recipient: trader.to_string(),
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
        // The actual fee charged this swap, emitted by `apply_swap`. Returning the
        // exact `fee_ppm` lets callers assert the rate-limit cap directly instead
        // of inferring it from output rate.
        let fee_ppm = res
            .attributes
            .iter()
            .find(|a| a.key == "fee_ppm")
            .expect("swap response must carry a fee_ppm attribute")
            .value
            .parse::<u128>()
            .unwrap();
        (out, fee_ppm)
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
        let whale = deps.api.addr_make("whale");
        let whale_info = message_info(&whale, &[Coin::new(Uint128::new(300_000), "inj")]);
        let res_a = execute(
            deps.as_mut(),
            env.clone(),
            whale_info,
            ExecuteMsg::Swap {
                recipient: whale.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(300_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        // Victim swap B (SAME block).
        let trader = deps.api.addr_make("trader");
        let victim = message_info(&trader, &[Coin::new(Uint128::new(1_000), "inj")]);
        let res_b = execute(
            deps.as_mut(),
            env.clone(),
            victim,
            ExecuteMsg::Swap {
                recipient: trader.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let fee_a = res_a
            .attributes
            .iter()
            .find(|a| a.key == "fee_ppm")
            .expect("swap A must carry a fee_ppm attribute")
            .value
            .parse::<u128>()
            .unwrap();
        let fee_b = res_b
            .attributes
            .iter()
            .find(|a| a.key == "fee_ppm")
            .expect("swap B must carry a fee_ppm attribute")
            .value
            .parse::<u128>()
            .unwrap();
        // The load-bearing invariant: the second swap in the same block pays the
        // EXACT fee the first swap committed. Without the freeze, swap B would
        // re-derive the fee against post-A (moved) slot0 vs. stale EMA and be
        // slammed with a higher value — so `fee_a == fee_b` is precisely what
        // distinguishes the fixed behavior from the vulnerable one.
        assert_eq!(
            fee_a, fee_b,
            "same-block fee not frozen: swap A paid {} ppm, swap B paid {} ppm",
            fee_a, fee_b
        );
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
        let (out_with_limit, fee_with_limit) = whale_then_probe(&mut deps, env_start, 1);

        // Re-run the scenario with rate limiting DISABLED for a baseline.
        let mut deps2 = mock_dependencies();
        let env_start2 = setup_oracle_pool(&mut deps2, 0);
        let (out_no_limit, fee_no_limit) = whale_then_probe(&mut deps2, env_start2, 1);

        // Direct invariant: with rate limit 100 ppm/sec and a 1-second gap, the
        // probe's fee can rise by AT MOST base(0) + 100 = 100 ppm. The unlimited
        // baseline pays the full (much higher) volatility fee, proving the cap is
        // what's biting — not that the scenario simply produced a small fee.
        assert!(
            fee_with_limit <= 100,
            "rate-limited probe fee {} exceeds base+100 ppm cap",
            fee_with_limit,
        );
        assert!(
            fee_no_limit > fee_with_limit,
            "baseline (no rate limit) should pay a strictly higher fee than the \
             capped probe: capped={} uncapped={}",
            fee_with_limit,
            fee_no_limit,
        );
        // And the lower fee means the capped probe receives strictly more output.
        assert!(
            out_with_limit > out_no_limit,
            "rate limit should reduce fee, giving the probe more output: \
             limited={} unlimited={}",
            out_with_limit,
            out_no_limit,
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
                recipient: whale1.to_string(),
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
                recipient: whale2.to_string(),
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
            "waiting longer should let the fee catch up (lower output): fast={} slow={}",
            out_fast,
            out_slow
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
        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(1_000), "inj")]);
        let res = execute(
            deps.as_mut(),
            env_probe,
            trader_info,
            ExecuteMsg::Swap {
                recipient: trader.to_string(),
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
        let fee_ppm = res
            .attributes
            .iter()
            .find(|a| a.key == "fee_ppm")
            .expect("swap response must carry a fee_ppm attribute")
            .value
            .parse::<u128>()
            .unwrap();
        (out, fee_ppm)
    }

    #[test]
    fn phase4_gap_move_still_charged_after_quiet_period() {
        // v2 accumulator analogue of the old EMA self-dilution regression: a
        // price move observed after a quiet gap >= volatility_decay_seconds
        // (here 100s, so the accumulator has fully decayed to 0) must STILL be
        // charged, because the fee at the probe's entry accumulates the realized
        // move `|current_tick - last_tick|` from the last-observed tick — and the
        // last tick stored was the whale's PRE-move entry tick, not the moved
        // price. The full excursion therefore registers regardless of how much
        // idle time elapsed, so the followers of any move pay even on an
        // infrequently-traded pool (the v1 EMA snapped the reference to price
        // before the deviation was measured, zeroing the signal for these
        // followers).
        let mut deps = mock_dependencies();
        // Rate limiting OFF so the elevated volatility fee is stored verbatim.
        let env_start = setup_oracle_pool(&mut deps, 0);

        // Whale moves the price (it pays base itself: its pre-swap price still
        // equals the EMA — the oracle can only ever charge the followers).
        let mut env_whale = env_start.clone();
        env_whale.block.time = env_whale.block.time.plus_seconds(200);
        let whale = deps.api.addr_make("whale");
        execute(
            deps.as_mut(),
            env_whale.clone(),
            message_info(&whale, &[Coin::new(Uint128::new(500_000), "inj")]),
            ExecuteMsg::Swap {
                recipient: whale.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(500_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        // Probe AFTER a quiet gap of 1.5x the decay window: its entry tick (the
        // whale's post-move price) diverges from the stored last_tick, and full
        // accumulator decay must not erase that realized move. The fee the probe
        // paid is the value it committed to the oracle.
        let mut env_probe = env_whale.clone();
        env_probe.block.time = env_probe.block.time.plus_seconds(150);
        let trader = deps.api.addr_make("trader");
        execute(
            deps.as_mut(),
            env_probe.clone(),
            message_info(&trader, &[Coin::new(Uint128::new(1_000), "inj")]),
            ExecuteMsg::Swap {
                recipient: trader.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::from(1_000u128),
                sqrt_price_limit_x96: Uint256::from(4295128740u128),
            },
        )
        .unwrap();

        let committed = crate::state::ORACLE
            .load(&deps.storage)
            .unwrap()
            .last_fee_ppm;
        assert!(
            committed > 0,
            "a move observed after a quiet gap >= decay window must charge above base (0), got {}",
            committed
        );
    }

    // ----------------------------------------------------------------------
    // Phase 1: protocol fees
    // ----------------------------------------------------------------------

    use cosmwasm_std::testing::{MockApi, MockQuerier, MockStorage};
    use cosmwasm_std::{to_json_binary, ContractResult, OwnedDeps, SystemResult, WasmQuery};

    /// Instantiate an INJ/USDT pool with liquidity and a querier that answers the
    /// factory's `GetConfig` with `owner`. Returns (deps, owner_addr_string).
    fn setup_protocol_pool() -> (OwnedDeps<MockStorage, MockApi, MockQuerier>, String) {
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        let owner = deps.api.addr_make("factory_owner").to_string();

        // Wire the querier so `assert_factory_owner` resolves the owner and
        // `assert_flash_borrower` sees every caller as authorized (the default
        // for these tests; the gate itself is exercised separately).
        let owner_for_querier = owner.clone();
        deps.querier.update_wasm(move |q| match q {
            WasmQuery::Smart { msg, .. } => {
                use choice_clmm_common::factory::QueryMsg as FQ;
                match cosmwasm_std::from_json::<FQ>(msg) {
                    Ok(FQ::IsFlashBorrower { .. }) => {
                        let resp = choice_clmm_common::factory::IsFlashBorrowerResponse {
                            authorized: true,
                        };
                        SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
                    }
                    // GetConfig (and any other factory query) → resolve the owner.
                    _ => {
                        let resp = choice_clmm_common::factory::ConfigResponse {
                            owner: owner_for_querier.clone(),
                            pool_code_id: 1,
                        };
                        SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
                    }
                }
            }
            _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
        });

        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 0,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        instantiate(deps.as_mut(), mock_env(), message_info(&factory, &[]), msg).unwrap();

        let mint_msg = ExecuteMsg::Mint {
            lower_tick: -200,
            upper_tick: 200,
            amount: Uint128::from(1_000_000u128),
        };
        let lp_info = message_info(
            &deps.api.addr_make("lp_provider"),
            &[
                Coin::new(Uint128::new(1_000_000_000), "inj"),
                Coin::new(Uint128::new(1_000_000_000), "usdt"),
            ],
        );
        execute(deps.as_mut(), mock_env(), lp_info, mint_msg).unwrap();

        (deps, owner)
    }

    fn do_swap_zero_for_one(
        deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier>,
        amount: u128,
    ) -> cosmwasm_std::Response {
        let swap_msg = ExecuteMsg::Swap {
            recipient: deps.api.addr_make("trader").to_string(),
            zero_for_one: true,
            amount_specified: Uint128::from(amount),
            sqrt_price_limit_x96: Uint256::from(4295128739u128 + 1),
        };
        let trader = message_info(
            &deps.api.addr_make("trader"),
            &[Coin::new(Uint128::new(amount), "inj")],
        );
        execute(deps.as_mut(), mock_env(), trader, swap_msg).unwrap()
    }

    fn attr(res: &cosmwasm_std::Response, key: &str) -> String {
        res.attributes
            .iter()
            .find(|a| a.key == key)
            .unwrap_or_else(|| panic!("missing attr {}", key))
            .value
            .clone()
    }

    #[test]
    fn protocol_fee_on_by_default_per_v3_table() {
        // Defaults follow the Uniswap v3 deployment table: divisor 6 (~16.7%
        // of fees) for tiers above 0.05%, divisor 4 (25%) at or below it.
        // setup_protocol_pool uses base 3000 -> 6.
        let (mut deps, _owner) = setup_protocol_pool();
        let cfg = crate::state::PROTOCOL_FEE_CONFIG
            .load(&deps.storage)
            .unwrap();
        assert_eq!(cfg.fee_protocol_0, 6, "0.30% tier defaults to 1/6 carve");
        assert_eq!(cfg.fee_protocol_1, 6);

        // A swap big enough that floor(fee/6) is non-zero accrues protocol
        // fees with NO SetFeeProtocol call: 12_000 in -> fee 36 -> carve 6.
        let res = do_swap_zero_for_one(&mut deps, 12_000);
        assert_ne!(
            attr(&res, "protocol_fee"),
            "0",
            "carve must be on by default"
        );

        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        assert!(!fees.protocol_fees_0.is_zero());

        // Tier rule, low side: a base <= 500 ppm pool defaults to 1/4.
        let mut deps2 = mock_dependencies();
        let factory = deps2.api.addr_make("factory");
        instantiate(
            deps2.as_mut(),
            mock_env(),
            message_info(&factory, &[]),
            InstantiateMsg {
                token0: native("inj"),
                token1: native("usdt"),
                tick_spacing: 10,
                fee_config: FeeConfig {
                    base_fee_ppm: 500,
                    max_fee_ppm: 1000,
                    variable_fee_control: 8_800,
                    max_volatility_accumulator: 2_000,
                    volatility_decay_seconds: 600,
                    max_fee_change_per_second_ppm: 100,
                },
                initial_sqrt_price: get_price_one(),
            },
        )
        .unwrap();
        let cfg2 = crate::state::PROTOCOL_FEE_CONFIG
            .load(&deps2.storage)
            .unwrap();
        assert_eq!(cfg2.fee_protocol_0, 4, "0.05% tier defaults to 1/4 carve");
    }

    #[test]
    fn protocol_fee_carve_accrues_quarter_and_reduces_lp() {
        let (mut deps, owner) = setup_protocol_pool();
        let owner_addr = cosmwasm_std::Addr::unchecked(&owner);

        // Owner enables a 1/4 (25%) carve on both directions.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        )
        .unwrap();

        // Single-step swap (no tick cross): protocol_fee == floor(fee_amount / 4).
        // The input is large enough (4000) that the 0.3% fee (~12) yields a
        // NON-ZERO quarter-carve (3) — a 1000-input swap would round the carve to
        // floor(3/4) == 0, leaving nothing to observe. It stays single-step: ~10k
        // of token0 is needed to reach the tick -200 boundary, so 4000 stops short.
        let fg0_before = crate::state::FEE_GROWTH_GLOBAL_0
            .may_load(&deps.storage)
            .unwrap()
            .unwrap_or_default();
        let res = do_swap_zero_for_one(&mut deps, 4000);
        let fee_amount: u128 = attr(&res, "fee_amount").parse().unwrap();
        let protocol_fee: u128 = attr(&res, "protocol_fee").parse().unwrap();
        assert!(
            protocol_fee > 0,
            "carve must actually be non-zero to be meaningful"
        );
        assert_eq!(protocol_fee, fee_amount / 4);

        // Accrued on the token0 side (zero_for_one input is token0 = inj).
        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        assert_eq!(fees.protocol_fees_0.u128(), protocol_fee);
        assert!(fees.protocol_fees_1.is_zero());

        // "...and_reduces_lp": the LP fee-growth must reflect ONLY the LP share
        // (fee_amount - protocol_fee), not the whole fee. The single in-range
        // position holds L = 1_000_000, and the accumulator gains
        // mul_div(lp_part, 2^128, L). Asserting the exact delta — and that it is
        // strictly below what the full fee would have produced — is what actually
        // backs the "reduces_lp" half of the test name.
        let fg0_after = crate::state::FEE_GROWTH_GLOBAL_0
            .load(&deps.storage)
            .unwrap();
        let l = Uint256::from(1_000_000u128);
        let q128 = Uint256::one() << 128u32;
        let lp_part = fee_amount - protocol_fee;
        let expected_lp_delta = Uint256::from(lp_part) * q128 / l;
        assert_eq!(
            fg0_after.wrapping_sub(fg0_before),
            expected_lp_delta,
            "LP fee-growth must reflect only the LP share, not the full fee"
        );
        let full_fee_delta = Uint256::from(fee_amount) * q128 / l;
        assert!(
            expected_lp_delta < full_fee_delta,
            "carve must reduce the LP share below the full-fee growth"
        );
    }

    #[test]
    fn set_fee_protocol_rejects_non_owner() {
        let (mut deps, _owner) = setup_protocol_pool();
        let intruder = deps.api.addr_make("intruder");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&intruder, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }

    #[test]
    fn set_fee_protocol_rejects_invalid_divisor() {
        let (mut deps, owner) = setup_protocol_pool();
        let owner_addr = cosmwasm_std::Addr::unchecked(&owner);
        // 3 is out of the valid 0 || 4..=10 range.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 3,
                fee_protocol_1: 0,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::InvalidConfig { .. }));
    }

    #[test]
    fn collect_protocol_splits_burn_and_treasury() {
        let (mut deps, owner) = setup_protocol_pool();
        let owner_addr = cosmwasm_std::Addr::unchecked(&owner);
        let treasury = deps.api.addr_make("treasury").to_string();
        let auction = deps.api.addr_make("burn_auction").to_string();

        // Configure: 50% to burn auction, rest to treasury; 25% protocol carve.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::UpdateProtocolFeeConfig {
                treasury: Some(treasury.clone()),
                burn_auction: Some(auction.clone()),
                burn_share_bps: Some(5000),
                clear_burn_auction: false,
            },
        )
        .unwrap();
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        )
        .unwrap();

        // Accrue some token0 protocol fees.
        do_swap_zero_for_one(&mut deps, 100_000);
        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        let accrued = fees.protocol_fees_0.u128();
        assert!(accrued > 1, "need a non-dust protocol balance to split");

        // Collect all.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::CollectProtocol {
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
        )
        .unwrap();

        let burn_amt = accrued * 5000 / 10000;
        let treasury_amt = accrued - burn_amt;

        // Expect a SendNative to the auction (funds == burn share) and a bank
        // send to the treasury (remainder). token0 = native "inj".
        let mut saw_auction = false;
        let mut saw_treasury = false;
        for m in &res.messages {
            if let cosmwasm_std::CosmosMsg::Wasm(cosmwasm_std::WasmMsg::Execute {
                contract_addr,
                funds,
                ..
            }) = &m.msg
            {
                if contract_addr == &auction {
                    saw_auction = true;
                    assert_eq!(funds[0].denom, "inj");
                    assert_eq!(funds[0].amount.u128(), burn_amt);
                }
            }
            if let cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &m.msg {
                if to_address == &treasury {
                    saw_treasury = true;
                    assert_eq!(amount[0].amount.u128(), treasury_amt);
                }
            }
        }
        assert!(saw_auction, "expected a burn-auction SendNative message");
        assert!(saw_treasury, "expected a treasury bank send");

        // Accrued balance is now drained.
        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        assert!(fees.protocol_fees_0.is_zero());
    }

    #[test]
    fn collect_protocol_rejects_non_owner() {
        let (mut deps, _owner) = setup_protocol_pool();
        let intruder = deps.api.addr_make("intruder");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&intruder, &[]),
            ExecuteMsg::CollectProtocol {
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }

    // ----------------------------------------------------------------------
    // Phase 2: flash loans + reentrancy lock
    // ----------------------------------------------------------------------

    use crate::actions::flash::REPLY_FLASH;
    use crate::contract::reply as contract_reply;
    use crate::state::{is_locked, PENDING_FLASH};
    use cosmwasm_std::{Binary, Reply, SubMsgResponse, SubMsgResult};

    fn pool_addr() -> String {
        mock_env().contract.address.to_string()
    }

    fn set_pool_balance(
        deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier>,
        inj: u128,
        usdt: u128,
    ) {
        deps.querier.bank.update_balance(
            pool_addr(),
            vec![
                Coin::new(Uint128::new(inj), "inj"),
                Coin::new(Uint128::new(usdt), "usdt"),
            ],
        );
    }

    fn flash_reply_msg() -> Reply {
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

    #[test]
    fn flash_lends_and_locks() {
        let (mut deps, _owner) = setup_protocol_pool();
        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");
        let caller = deps.api.addr_make("anyone");
        let caller_info = message_info(&caller, &[]);

        let res = execute(
            deps.as_mut(),
            mock_env(),
            caller_info,
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();

        // Loan transfer (bank send) + callback submessage.
        let loan = res
            .messages
            .iter()
            .find(|m| matches!(&m.msg, cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { .. })))
            .expect("loan bank send");
        if let cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &loan.msg {
            assert_eq!(to_address, &borrower.to_string());
            assert_eq!(amount[0].denom, "inj");
            assert_eq!(amount[0].amount.u128(), 1_000_000);
        }
        let callback = res
            .messages
            .iter()
            .find(|m| m.id == REPLY_FLASH)
            .expect("callback submessage with flash reply id");
        assert!(matches!(
            &callback.msg,
            cosmwasm_std::CosmosMsg::Wasm(cosmwasm_std::WasmMsg::Execute { .. })
        ));

        // Lock held; pending context recorded. fee0 = ceil(1e6 * 3000 / 1e6) = 3000.
        assert!(is_locked(&deps.storage));
        let pending = PENDING_FLASH.load(&deps.storage).unwrap();
        assert_eq!(pending.amount0.u128(), 1_000_000);
        assert_eq!(pending.fee0.u128(), 3000);
        assert_eq!(pending.snapshot0.u128(), 5_000_000);
    }

    #[test]
    fn flash_rejects_unauthorized_borrower() {
        // Same pool, but the factory reports the caller is NOT an allowlisted
        // flash borrower → the pool must reject before lending or locking.
        let (mut deps, _owner) = setup_protocol_pool();
        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        // Repoint the querier so `IsFlashBorrower` answers `false`.
        deps.querier.update_wasm(|q| match q {
            WasmQuery::Smart { msg, .. } => {
                use choice_clmm_common::factory::QueryMsg as FQ;
                match cosmwasm_std::from_json::<FQ>(msg) {
                    Ok(FQ::IsFlashBorrower { .. }) => {
                        let resp = choice_clmm_common::factory::IsFlashBorrowerResponse {
                            authorized: false,
                        };
                        SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
                    }
                    _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
                }
            }
            _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
        });

        let borrower = deps.api.addr_make("borrower");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
        // No lock was taken — the gate runs before any state change.
        assert!(!is_locked(&deps.storage));
    }

    #[test]
    fn flash_blocks_reentrant_mutator() {
        let (mut deps, _owner) = setup_protocol_pool();
        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();

        // While the lock is held, any fund-affecting mutator must revert.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[Coin::new(Uint128::new(100), "inj")]),
            ExecuteMsg::Swap {
                recipient: borrower.to_string(),
                zero_for_one: true,
                amount_specified: Uint128::new(100),
                sqrt_price_limit_x96: Uint256::from(4295128739u128 + 1),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Reentrancy {}));

        // A nested flash is likewise rejected.
        let err2 = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap_err();
        assert!(matches!(err2, ContractError::Reentrancy {}));

        // Config setters are now ALSO guarded during a flash (previously exempt).
        // `reply_flash` re-reads PROTOCOL_FEE_CONFIG after the callback, so a
        // mid-flash carve change is rejected. The lock check precedes the owner
        // check, so this reverts with Reentrancy regardless of sender.
        let err3 = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        )
        .unwrap_err();
        assert!(matches!(err3, ContractError::Reentrancy {}));

        let err4 = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::UpdateProtocolFeeConfig {
                treasury: None,
                burn_auction: None,
                burn_share_bps: None,
                clear_burn_auction: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err4, ContractError::Reentrancy {}));
    }

    #[test]
    fn flash_reply_accrues_lp_fee_and_unlocks() {
        let (mut deps, owner) = setup_protocol_pool();
        // This test asserts the WHOLE flash fee lands in LP fee growth, so
        // switch the (default-on) protocol carve off through the real
        // owner-gated path first.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&cosmwasm_std::Addr::unchecked(&owner), &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 0,
                fee_protocol_1: 0,
            },
        )
        .unwrap();
        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();

        // Borrower repays loan + fee: balance returns to snapshot + fee.
        set_pool_balance(&mut deps, 5_003_000, 5_000_000);

        contract_reply(deps.as_mut(), mock_env(), flash_reply_msg()).unwrap();

        // Lock released, pending cleared.
        assert!(!is_locked(&deps.storage));
        assert!(PENDING_FLASH.may_load(&deps.storage).unwrap().is_none());

        // LP fee growth bumped by fee0 (protocol off). L = 1_000_000 (minted).
        let fg0 = crate::state::FEE_GROWTH_GLOBAL_0
            .load(&deps.storage)
            .unwrap();
        let expected = (Uint256::from(3000u128) << 128u32) / Uint256::from(1_000_000u128);
        assert_eq!(fg0, expected);

        // No protocol fee accrued.
        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        assert!(fees.protocol_fees_0.is_zero());
    }

    #[test]
    fn flash_reply_rejects_underpayment() {
        let (mut deps, _owner) = setup_protocol_pool();
        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();

        // One unit short of snapshot + fee.
        set_pool_balance(&mut deps, 5_002_999, 5_000_000);
        let err = contract_reply(deps.as_mut(), mock_env(), flash_reply_msg()).unwrap_err();
        assert!(matches!(
            err,
            ContractError::FlashNotRepaid { token: 0, .. }
        ));
    }

    #[test]
    fn flash_fee_carves_to_protocol() {
        let (mut deps, owner) = setup_protocol_pool();
        let owner_addr = cosmwasm_std::Addr::unchecked(&owner);
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::SetFeeProtocol {
                fee_protocol_0: 4,
                fee_protocol_1: 4,
            },
        )
        .unwrap();

        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap();
        set_pool_balance(&mut deps, 5_003_000, 5_000_000);
        contract_reply(deps.as_mut(), mock_env(), flash_reply_msg()).unwrap();

        // fee0 = 3000; protocol takes floor(3000/4) = 750, LPs get 2250.
        let q = query(deps.as_ref(), mock_env(), QueryMsg::GetProtocolFees {}).unwrap();
        let fees: choice_clmm_common::pool::ProtocolFeesResponse = from_json(&q).unwrap();
        assert_eq!(fees.protocol_fees_0.u128(), 750);

        let fg0 = crate::state::FEE_GROWTH_GLOBAL_0
            .load(&deps.storage)
            .unwrap();
        let expected = (Uint256::from(2250u128) << 128u32) / Uint256::from(1_000_000u128);
        assert_eq!(fg0, expected);
    }

    #[test]
    fn flash_rejected_when_no_liquidity() {
        // Fresh pool with NO minted liquidity (L = 0). Flash must be rejected
        // (matching Uniswap v3's `require(L > 0)`) so the LP fee share is never
        // silently diverted to the protocol bucket. The pool holds idle balance
        // but no in-range liquidity.
        let mut deps = mock_dependencies();
        let factory = deps.api.addr_make("factory");
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 0,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        instantiate(deps.as_mut(), mock_env(), message_info(&factory, &[]), msg).unwrap();

        // Authorize the borrower so the flash gate passes and we actually reach
        // the no-liquidity check this test targets.
        deps.querier.update_wasm(|q| match q {
            WasmQuery::Smart { msg, .. } => {
                use choice_clmm_common::factory::QueryMsg as FQ;
                match cosmwasm_std::from_json::<FQ>(msg) {
                    Ok(FQ::IsFlashBorrower { .. }) => {
                        let resp = choice_clmm_common::factory::IsFlashBorrowerResponse {
                            authorized: true,
                        };
                        SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
                    }
                    _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
                }
            }
            _ => SystemResult::Ok(ContractResult::Ok(Default::default())),
        });

        set_pool_balance(&mut deps, 5_000_000, 5_000_000);
        let borrower = deps.api.addr_make("borrower");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&borrower, &[]),
            ExecuteMsg::Flash {
                recipient: borrower.to_string(),
                amount0: Uint128::new(1_000_000),
                amount1: Uint128::zero(),
                data: Binary::default(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::FlashWithoutLiquidity {}));
    }

    // ----------------------------------------------------------------------
    // Phase 3: exact-output swaps
    // ----------------------------------------------------------------------

    #[test]
    fn exact_output_delivers_requested_and_refunds() {
        let (mut deps, _owner) = setup_protocol_pool();
        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(10_000), "inj")]);

        let res = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::SwapExactOutput {
                zero_for_one: true,
                amount_out: Uint128::new(500),
                maximum_amount_in: Uint128::new(10_000),
                recipient: Some(trader.to_string()),
                deadline: None,
            },
        )
        .unwrap();

        // Exactly the requested output is delivered.
        let out_amt: u128 = attr(&res, "amount_out").parse().unwrap();
        assert_eq!(out_amt, 500);
        let in_amt: u128 = attr(&res, "amount_in").parse().unwrap();
        // Cost is output + fee, comfortably under the 10_000 max.
        assert!((500..10_000).contains(&in_amt));

        // Output payout = 500 usdt to trader; surplus inj refunded.
        let mut paid_out = false;
        let mut refunded = false;
        for m in &res.messages {
            if let cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &m.msg {
                if to_address == &trader.to_string() && amount[0].denom == "usdt" {
                    paid_out = true;
                    assert_eq!(amount[0].amount.u128(), 500);
                }
                if to_address == &trader.to_string() && amount[0].denom == "inj" {
                    refunded = true;
                    assert_eq!(amount[0].amount.u128(), 10_000 - in_amt);
                }
            }
        }
        assert!(paid_out, "expected 500 usdt payout");
        assert!(refunded, "expected inj surplus refund");
    }

    #[test]
    fn exact_output_quote_matches_execution() {
        let (mut deps, _owner) = setup_protocol_pool();

        let q = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::QuoteExactOutput {
                token_out: native("usdt"),
                amount_out: Uint128::new(500),
            },
        )
        .unwrap();
        let quote: choice_clmm_common::pool::QuoteResponse = from_json(&q).unwrap();
        assert_eq!(quote.amount_out.u128(), 500);

        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(10_000), "inj")]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::SwapExactOutput {
                zero_for_one: true,
                amount_out: Uint128::new(500),
                maximum_amount_in: Uint128::new(10_000),
                recipient: Some(trader.to_string()),
                deadline: None,
            },
        )
        .unwrap();
        let in_amt: u128 = attr(&res, "amount_in").parse().unwrap();
        assert_eq!(quote.amount_in_consumed.u128(), in_amt);
    }

    #[test]
    fn exact_output_excessive_input_reverts() {
        let (mut deps, _owner) = setup_protocol_pool();
        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(100), "inj")]);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::SwapExactOutput {
                zero_for_one: true,
                amount_out: Uint128::new(500),
                maximum_amount_in: Uint128::new(100), // less than 500 + fee
                recipient: Some(trader.to_string()),
                deadline: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::ExcessiveInput { .. }));
    }

    #[test]
    fn exact_output_insufficient_liquidity_reverts() {
        let (mut deps, _owner) = setup_protocol_pool();
        let trader = deps.api.addr_make("trader");
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(100_000_000), "inj")]);

        // Far more token1 than the [-200, 200] range can deliver.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::SwapExactOutput {
                zero_for_one: true,
                amount_out: Uint128::new(100_000_000),
                maximum_amount_in: Uint128::new(100_000_000),
                recipient: Some(trader.to_string()),
                deadline: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::InsufficientOutput { .. }));
    }

    #[test]
    fn exact_output_reverse_direction() {
        let (mut deps, _owner) = setup_protocol_pool();
        let trader = deps.api.addr_make("trader");
        // Pay usdt (token1), receive inj (token0): zero_for_one = false.
        let trader_info = message_info(&trader, &[Coin::new(Uint128::new(10_000), "usdt")]);

        let res = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::SwapExactOutput {
                zero_for_one: false,
                amount_out: Uint128::new(500),
                maximum_amount_in: Uint128::new(10_000),
                recipient: Some(trader.to_string()),
                deadline: None,
            },
        )
        .unwrap();

        assert_eq!(attr(&res, "amount_out").parse::<u128>().unwrap(), 500);
        let mut paid_inj = false;
        for m in &res.messages {
            if let cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &m.msg {
                if to_address == &trader.to_string() && amount[0].denom == "inj" {
                    paid_inj = true;
                    assert_eq!(amount[0].amount.u128(), 500);
                }
            }
        }
        assert!(paid_inj, "expected 500 inj payout");
    }

    // ----------------------------------------------------------------------
    // Phase 4: hook seam (reserved, inert)
    // ----------------------------------------------------------------------

    #[test]
    fn hook_seam_defaults_to_none() {
        let (deps, _owner) = setup_protocol_pool();
        let res = query(deps.as_ref(), mock_env(), QueryMsg::GetConfig {}).unwrap();
        let config: PoolConfig = from_json(&res).unwrap();
        assert_eq!(config.hook, None);
        assert_eq!(config.hook_permissions, 0);
    }

    // ----------------------------------------------------------------------
    // C-L1: native funds attached to a CW20-input swap must be refunded
    // ----------------------------------------------------------------------

    /// Regression for C-L1 (audit 2026-06-06): `Swap`/`SwapExactInput`/
    /// `SwapExactOutput` are classified payable, so a caller can attach native
    /// coins. When the in-token is a CW20 (pulled via `TransferFrom`), the swap
    /// consumes NO native coins — they used to be silently absorbed into
    /// reserves. The fix appends a `BankMsg::Send` returning the full attached
    /// `info.funds` to the caller.
    #[test]
    fn swap_cw20_input_refunds_attached_native_funds() {
        let mut deps = mock_dependencies();
        let cw20_addr = deps.api.addr_make("cw20_usdt");

        // token0 = native (inj), token1 = CW20 (Native < Token ordering).
        let msg = InstantiateMsg {
            token0: native("inj"),
            token1: AssetInfo::Token {
                contract_addr: cw20_addr.to_string(),
            },
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: get_price_one(),
        };
        let creator = message_info(&deps.api.addr_make("factory"), &[]);
        instantiate(deps.as_mut(), mock_env(), creator, msg).unwrap();

        // Sell the CW20 (token1) for native token0 via the allowance path
        // (`zero_for_one = false`), wrongly attaching native coins. Those coins
        // are not the consumed input (the CW20 is), so all of them must refund.
        let trader = deps.api.addr_make("trader");
        let recipient = deps.api.addr_make("recipient");
        let max_sqrt_ratio = Uint256::from_str("1461446703485210103287273052203988822378723970341")
            .unwrap()
            - Uint256::one();

        let trader_info = message_info(
            &trader,
            &[
                Coin::new(Uint128::new(1234), "inj"),
                Coin::new(Uint128::new(500), "stranded"),
            ],
        );
        let res = execute(
            deps.as_mut(),
            mock_env(),
            trader_info,
            ExecuteMsg::Swap {
                recipient: recipient.to_string(),
                zero_for_one: false, // selling token1 (CW20) for token0
                amount_specified: Uint128::from(1000u128),
                sqrt_price_limit_x96: max_sqrt_ratio,
            },
        )
        .unwrap();

        // The attached native coins must be refunded in FULL to the trader.
        let refund = res
            .messages
            .iter()
            .find_map(|m| match &m.msg {
                cosmwasm_std::CosmosMsg::Bank(BankMsg::Send { to_address, amount })
                    if to_address == trader.as_str() =>
                {
                    Some(amount.clone())
                }
                _ => None,
            })
            .expect("expected a native refund BankMsg::Send to the trader");

        let find = |denom: &str| {
            refund
                .iter()
                .find(|c| c.denom == denom)
                .map(|c| c.amount)
                .unwrap_or_default()
        };
        assert_eq!(find("inj"), Uint128::new(1234));
        assert_eq!(find("stranded"), Uint128::new(500));
    }

    // ----------------------------------------------------------------------
    // C-L7 (pool-side): reject zero / out-of-range init_sqrt_price on instantiate
    // ----------------------------------------------------------------------

    /// Regression for C-L7 (audit 2026-06-06): the factory forwards a
    /// caller-supplied `init_sqrt_price` unvalidated, so the pool must reject a
    /// zero or out-of-Q64.96-range opening price on instantiate (defense in
    /// depth) rather than seed slot0 with a nonsensical price.
    #[test]
    fn instantiate_rejects_zero_or_out_of_range_init_sqrt_price() {
        use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

        let base_msg = |sqrt_price: Uint256| InstantiateMsg {
            token0: native("inj"),
            token1: native("usdt"),
            tick_spacing: 10,
            fee_config: FeeConfig {
                base_fee_ppm: 3000,
                max_fee_ppm: 10000,
                variable_fee_control: 8_800,
                max_volatility_accumulator: 2_000,
                volatility_decay_seconds: 600,
                max_fee_change_per_second_ppm: 0,
            },
            initial_sqrt_price: sqrt_price,
        };

        // Zero is rejected.
        {
            let mut deps = mock_dependencies();
            let info = message_info(&deps.api.addr_make("factory"), &[]);
            let err = instantiate(deps.as_mut(), mock_env(), info, base_msg(Uint256::zero()))
                .unwrap_err();
            assert!(matches!(err, ContractError::InvalidConfig { .. }));
        }

        // Below MIN_SQRT_RATIO is rejected.
        {
            let mut deps = mock_dependencies();
            let info = message_info(&deps.api.addr_make("factory"), &[]);
            let below = Uint256::from(MIN_SQRT_RATIO) - Uint256::one();
            let err = instantiate(deps.as_mut(), mock_env(), info, base_msg(below)).unwrap_err();
            assert!(matches!(err, ContractError::InvalidConfig { .. }));
        }

        // At/above max_sqrt_ratio() is rejected (the upper bound is exclusive).
        {
            let mut deps = mock_dependencies();
            let info = message_info(&deps.api.addr_make("factory"), &[]);
            let err = instantiate(deps.as_mut(), mock_env(), info, base_msg(max_sqrt_ratio()))
                .unwrap_err();
            assert!(matches!(err, ContractError::InvalidConfig { .. }));
        }

        // A valid in-range price (1.0) still instantiates successfully.
        {
            let mut deps = mock_dependencies();
            let info = message_info(&deps.api.addr_make("factory"), &[]);
            instantiate(deps.as_mut(), mock_env(), info, base_msg(get_price_one()))
                .expect("valid init_sqrt_price should instantiate");
        }
    }
}

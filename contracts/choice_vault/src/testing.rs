#[cfg(test)]
mod tests {
    use crate::contract::{
        execute, instantiate, query, reply, HARVEST_REPLY_ID, PROVIDE_LIQUIDITY_REPLY_ID,
        SWAP_REPLY_ID,
    };
    use crate::error::ContractError;
    use crate::mock_querier::mock_dependencies;
    use crate::msg::{CompoundPayload, Cw20HookMsg, UserInfoResponse};
    use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use crate::state::{Config, UserInfo, TOTAL_SHARES, USERS};
    use choice::asset::AssetInfo;
    use choice::staking::{
        Cw20HookMsg as FarmCw20HookMsg, ExecuteMsg as FarmExecuteMsg, StakerInfoResponse,
    };
    use cosmwasm_std::testing::{message_info, mock_env};
    use cosmwasm_std::{
        from_json, to_json_binary, Binary, CosmosMsg, Decimal, StdError, SubMsg, Uint128, WasmMsg,
    };
    use cosmwasm_std::{Reply, SubMsgResponse, SubMsgResult};
    use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

    #[test]
    fn proper_initialization() {
        // Use standard mock dependencies
        let mut deps = mock_dependencies();

        // --- Define addresses using addr_make ---
        let owner_addr = deps.api.addr_make("owner");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let token_a_addr = deps.api.addr_make("token_a0000");
        let creator_addr = deps.api.addr_make("creator");

        // Define native token denoms
        let token_b_denom = "uinj";
        let reward_denom = "reward_denom";

        // --- Construct the instantiation message ---
        let msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: reward_denom.to_string(),
            },
            asset_infos: [
                AssetInfo::Token {
                    contract_addr: token_a_addr.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };

        let info = message_info(&creator_addr, &[]);

        // Call instantiate, .unwrap() will panic if there's an error
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.messages.len(), 0); // Instantiate should not send any messages

        // --- Verify State ---

        // 1. Query the configuration
        let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
        let config: Config = from_json(&res).unwrap();

        // Assert that all fields in the config match the addresses we created
        assert_eq!(config.owner, owner_addr);
        assert_eq!(config.pair_contract, pair_contract_addr);
        assert_eq!(config.farm_contract, farm_contract_addr);
        assert_eq!(
            config.lp_token,
            AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            }
        );
        assert_eq!(
            config.reward_token,
            AssetInfo::NativeToken {
                denom: reward_denom.to_string()
            }
        );
        assert_eq!(
            config.asset_infos,
            [
                AssetInfo::Token {
                    contract_addr: token_a_addr.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ]
        );

        // 2. Query the total shares
        let res = query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap();
        let total_shares: Uint128 = from_json(&res).unwrap();

        // Assert that total shares are initialized to zero
        assert_eq!(total_shares, Uint128::zero());
    }

    #[test]
    fn test_deposit_first_user() {
        // --- Arrange ---
        // 1. Setup the environment with our custom querier
        let mut deps = mock_dependencies();

        // 2. Instantiate the contract
        let owner_addr = deps.api.addr_make("owner");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let info = message_info(&creator_addr, &[]);
        instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();

        // 3. Setup the mock querier response
        // The contract will query StakerInfo for itself from the farm.
        // Since this is the first deposit, the bond_amount should be zero.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(), // This field doesn't matter for the test
                reward_index: Decimal::zero(),
                bond_amount: Uint128::zero(),
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. Simulate a user sending 100 LP tokens to the vault
        let user1_addr = deps.api.addr_make("user1");
        let deposit_amount = Uint128::new(100);

        let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user1_addr.to_string(), // The user who is depositing
            amount: deposit_amount,
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });

        // The `info.sender` for a Receive hook is always the token contract
        let info = message_info(&lp_token_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the state changes
        // User1 should now have 100 shares
        let user_info_res: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user_info_res.shares, deposit_amount);

        // Total shares should be 100
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, deposit_amount);

        // 6. Verify the returned message
        // The vault must send a message to stake the received LP tokens in the farm
        assert_eq!(res.messages.len(), 1);
        let expected_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: farm_contract_addr.to_string(),
                amount: deposit_amount,
                msg: to_json_binary(&FarmCw20HookMsg::Bond {}).unwrap(),
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_msg);
    }

    #[test]
    fn test_deposit_second_user_proportional_shares() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Simulate First User's Deposit ---
        // We need to manually set the state as if the first user has already deposited.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(100);
        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                },
            )
            .unwrap();

        // 2. Setup Mock Querier: The key part of this test.
        // The vault now has 100 shares, but let's say the underlying LP tokens
        // have grown to 120 due to compounding.
        let total_lp_staked = Uint128::new(120);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(), // Some non-zero value
                bond_amount: total_lp_staked,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 3. A second user (user2) deposits 60 LP tokens.
        let user2_addr = deps.api.addr_make("user2");
        let user2_deposit_amount = Uint128::new(60);

        let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user2_addr.to_string(),
            amount: user2_deposit_amount,
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });

        let info = message_info(&lp_token_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 4. Verify the new share calculation
        // Expected shares for user2 = (amount * total_shares) / total_lp_staked
        //                         = (60 * 100) / 120 = 50
        let expected_new_shares = Uint128::new(50);

        let user2_info: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user2_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user2_info.shares, expected_new_shares);

        // 5. Verify total shares have been updated correctly
        // New total shares = 100 (from user1) + 50 (from user2) = 150
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, initial_shares + expected_new_shares);
    }

    #[test]
    fn test_withdraw_simple() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let pair_contract_addr = deps.api.addr_make("pair0000");
        let creator_addr = deps.api.addr_make("creator");
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Simulate a pre-existing deposit for user1
        let user1_addr = deps.api.addr_make("user1");
        let user1_shares = Uint128::new(100);
        TOTAL_SHARES.save(&mut deps.storage, &user1_shares).unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier
        // For a simple withdrawal, the amount of LP tokens in the farm equals the total shares.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(100), // Vault has 100 LP tokens staked
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. User1 withdraws their entire balance of 100 shares.
        let msg = ExecuteMsg::Withdraw {
            shares: user1_shares,
        };
        let info = message_info(&user1_addr, &[]); // The user themselves sends this message
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes
        // User1's shares should now be zero (and their UserInfo removed from storage)
        let user_info: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user_info.shares, Uint128::zero());

        // Total shares in the contract should be zero
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, Uint128::zero());

        // 6. Verify the returned messages
        // The response should contain two messages: Unbond from farm, then Transfer to user.
        assert_eq!(res.messages.len(), 2);

        // Message 1: Unbond from the farm
        let expected_unbond_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: user1_shares,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_unbond_msg);

        // Message 2: Transfer LP tokens to the user
        let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: user1_addr.to_string(),
                amount: user1_shares,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[1], expected_transfer_msg);
    }

    #[test]
    fn test_withdraw_proportional() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let pair_contract_addr = deps.api.addr_make("pair0000");
        let creator_addr = deps.api.addr_make("creator");
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Simulate a pre-existing state where user1 has 100 shares.
        let user1_addr = deps.api.addr_make("user1");
        let user1_shares = Uint128::new(100);
        TOTAL_SHARES.save(&mut deps.storage, &user1_shares).unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier: THIS IS THE KEY PART OF THE TEST.
        // The vault holds 100 total shares, but due to compounding, the underlying
        // staked LP token balance has grown to 120.
        let lp_staked_after_growth = Uint128::new(120);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: lp_staked_after_growth,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. User1 withdraws their entire balance of 100 shares.
        let msg = ExecuteMsg::Withdraw {
            shares: user1_shares,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the amount of LP tokens returned.
        // The user should receive the proportional amount of the grown assets, not their initial deposit.
        // lp_to_withdraw = (shares * total_lp) / total_shares = (100 * 120) / 100 = 120.
        let expected_lp_to_receive = Uint128::new(120);

        // 6. Verify the messages.
        assert_eq!(res.messages.len(), 2);

        let expected_unbond_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: expected_lp_to_receive,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_unbond_msg);

        let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: user1_addr.to_string(),
                amount: expected_lp_to_receive,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[1], expected_transfer_msg);
    }

    #[test]
    fn test_withdraw_partial() {
        // --- Arrange ---
        // 1. Setup and instantiate
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let pair_contract_addr = deps.api.addr_make("pair0000");
        let creator_addr = deps.api.addr_make("creator");
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Simulate a pre-existing deposit for user1 with 100 shares.
        let user1_addr = deps.api.addr_make("user1");
        let initial_user_shares = Uint128::new(100);
        TOTAL_SHARES
            .save(&mut deps.storage, &initial_user_shares)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_user_shares,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier: Use a simple 1:1 ratio for this test.
        let total_lp_staked = Uint128::new(100);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_lp_staked,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. User1 withdraws 40 of their 100 shares.
        let shares_to_withdraw = Uint128::new(40);
        let msg = ExecuteMsg::Withdraw {
            shares: shares_to_withdraw,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes.
        let expected_remaining_shares = Uint128::new(60);

        // User's balance should be 60
        let user_info: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user_info.shares, expected_remaining_shares);

        // Total shares should now be 60
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, expected_remaining_shares);

        // 6. Verify returned messages.
        // The amount of LP tokens to unbond and transfer should be 40.
        let lp_to_withdraw = shares_to_withdraw;
        assert_eq!(res.messages.len(), 2);

        let expected_unbond_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: lp_to_withdraw,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_unbond_msg);

        let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: user1_addr.to_string(),
                amount: lp_to_withdraw,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[1], expected_transfer_msg);
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_happy_path() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let vault_addr = deps.api.addr_make("vault_contract"); // Mock our own address

        let reward_denom = "uinj";
        let token_a_addr = deps.api.addr_make("token_a0000"); // CW20
        let token_b_denom = "uusd"; // Native

        let creator_addr = deps.api.addr_make("creator");
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: reward_denom.to_string(),
            },
            asset_infos: [
                AssetInfo::Token {
                    contract_addr: token_a_addr.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        let pending_rewards = Uint128::new(20);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000), // Vault has 1000 LP staked
                pending_reward: pending_rewards,
            },
        );

        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[
                cosmwasm_std::Coin {
                    denom: reward_denom.to_string(),
                    amount: pending_rewards,
                },
                cosmwasm_std::Coin {
                    denom: token_b_denom.to_string(),
                    amount: Uint128::new(10),
                },
            ],
        )]);

        // This is the balance of the CW20 asset after the swap
        let cw20_balance_after_swap = Uint128::new(8);
        deps.querier.with_token_balance(
            &token_a_addr.to_string(),
            &vault_addr.to_string(),
            cw20_balance_after_swap,
        );
        deps.querier.with_token_balance(
            &lp_token_addr.to_string(),
            &vault_addr.to_string(),
            Uint128::new(9),
        );

        let mut env = mock_env();
        env.contract.address = vault_addr;

        // ==> STEP 1: Execute Compound
        let info = message_info(&owner_addr, &[]);
        let res = execute(
            deps.as_mut(),
            env.clone(),
            info,
            ExecuteMsg::Compound {
                belief_price: Decimal::one(),
            },
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // ==> STEP 2: Handle Harvest Reply
        let payload = CompoundPayload {
            belief_price: Decimal::one(),
        };
        let reply_msg = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(), // <-- THIS IS THE FIX
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, SWAP_REPLY_ID);

        // ==> STEP 3: Handle Swap Reply
        let reply_msg = Reply {
            id: SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: Binary::default(),
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // +++ FIX IS HERE +++
        // We now expect TWO messages: The IncreaseAllowance and the ProvideLiquidity SubMsg
        assert_eq!(res.messages.len(), 2);

        // Verify the first message is IncreaseAllowance for the CW20 token
        let expected_allowance_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: token_a_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                spender: pair_contract_addr.to_string(),
                amount: cw20_balance_after_swap,
                expires: None,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[0].msg, expected_allowance_msg);

        // Verify the second message is the submessage to Provide Liquidity
        assert_eq!(res.messages[1].id, PROVIDE_LIQUIDITY_REPLY_ID);

        // ==> STEP 4: Handle Provide Liquidity Reply (Final Step)
        let reply_msg = Reply {
            id: PROVIDE_LIQUIDITY_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: Binary::default(),
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        assert_eq!(res.messages.len(), 1);
        let final_bond_amount = Uint128::new(9);
        let expected_final_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: farm_contract_addr.to_string(),
                amount: final_bond_amount,
                msg: to_json_binary(&FarmCw20HookMsg::Bond {}).unwrap(),
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_final_msg);

        // Check final attributes
        assert!(res
            .attributes
            .contains(&cosmwasm_std::attr("action", "compound")));
        assert!(res
            .attributes
            .contains(&cosmwasm_std::attr("status", "step_4_complete")));
        assert!(res
            .attributes
            .contains(&cosmwasm_std::attr("lp_tokens_staked", "9")));
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_happy_path_native_assets() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let vault_addr = deps.api.addr_make("vault_contract");

        let reward_denom = "uinj";
        let token_a_denom = "uatom"; // Native
        let token_b_denom = "uusd"; // Native

        let creator_addr = deps.api.addr_make("creator");
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: reward_denom.to_string(),
            },
            // Using two native tokens for the pair
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: token_a_denom.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        let pending_rewards = Uint128::new(20);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: pending_rewards,
            },
        );

        // Mock balances for all native tokens involved
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[
                cosmwasm_std::Coin {
                    denom: reward_denom.to_string(),
                    amount: pending_rewards,
                },
                // This is the balance of asset A after the swap
                cosmwasm_std::Coin {
                    denom: token_a_denom.to_string(),
                    amount: Uint128::new(8),
                },
                // This is the balance of asset B after the swap
                cosmwasm_std::Coin {
                    denom: token_b_denom.to_string(),
                    amount: Uint128::new(10),
                },
            ],
        )]);

        // We only need to mock the LP token balance, as it's a CW20
        deps.querier.with_token_balance(
            &lp_token_addr.to_string(),
            &vault_addr.to_string(),
            Uint128::new(9),
        );

        let mut env = mock_env();
        env.contract.address = vault_addr;

        // Execute the full compound flow
        let info = message_info(&owner_addr, &[]);
        execute(
            deps.as_mut(),
            env.clone(),
            info,
            ExecuteMsg::Compound {
                belief_price: Decimal::one(),
            },
        )
        .unwrap();

        let payload = CompoundPayload {
            belief_price: Decimal::one(),
        };
        let reply_msg = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(), // <-- THIS IS THE FIX
        };
        reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // ==> Check STEP 3: Handle Swap Reply <==
        let reply_msg = Reply {
            id: SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: Binary::default(),
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // With two native assets, we expect ONLY ONE message: the ProvideLiquidity SubMsg
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, PROVIDE_LIQUIDITY_REPLY_ID);

        // Finish the flow
        let reply_msg = Reply {
            id: PROVIDE_LIQUIDITY_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: Binary::default(),
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // Assert the final message is correct
        assert_eq!(res.messages.len(), 1);
        assert!(res
            .attributes
            .contains(&cosmwasm_std::attr("status", "step_4_complete")));
    }

    #[test]
    fn test_withdraw_insufficient_shares() {
        // --- Arrange ---
        // 1. Setup and instantiate
        let mut deps = mock_dependencies();
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let owner_addr = deps.api.addr_make("owner");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Simulate a user with 100 shares
        let user1_addr = deps.api.addr_make("user1");
        let user1_shares = Uint128::new(100);
        TOTAL_SHARES.save(&mut deps.storage, &user1_shares).unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                },
            )
            .unwrap();

        // --- Act ---
        // 3. User1 attempts to withdraw 101 shares, which is more than they have.
        let shares_to_withdraw = Uint128::new(101);
        let msg = ExecuteMsg::Withdraw {
            shares: shares_to_withdraw,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // --- Assert ---
        // 4. Verify the operation failed with the correct error.
        assert!(matches!(res, Err(ContractError::InsufficientShares {})));
    }

    #[test]
    fn test_deposit_incorrect_token() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let lp_token_addr = deps.api.addr_make("the_real_lp_token");
        let fake_lp_token_addr = deps.api.addr_make("some_other_token");
        let owner_addr = deps.api.addr_make("owner");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: deps.api.addr_make("farm0000").to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Act ---
        // A different, incorrect token contract tries to send a deposit message
        let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: deps.api.addr_make("user1").to_string(),
            amount: Uint128::new(100),
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });

        // The info.sender is the token contract, which is NOT the one we configured
        let info = message_info(&fake_lp_token_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // --- Assert ---
        assert!(matches!(res, Err(ContractError::Unauthorized {})));
    }

    #[test]
    fn test_compound_zero_rewards() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Setup Mock Querier to return zero pending rewards
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: deps.api.addr_make("vault_contract").to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::zero(), // The key part of this test
            },
        );

        // --- Act ---
        let msg = ExecuteMsg::Compound {
            belief_price: Decimal::one(),
        };
        let info = message_info(&owner_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // The execution should succeed but generate no messages
        assert_eq!(res.messages.len(), 0);

        // Check for the specific attribute indicating why no action was taken
        assert_eq!(
            res.attributes,
            vec![
                cosmwasm_std::attr("action", "compound"),
                cosmwasm_std::attr("status", "no_rewards"),
            ]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_with_fees() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let vault_addr = deps.api.addr_make("vault_contract");
        let fee_recipient_addr = deps.api.addr_make("fee_recipient");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let reward_denom = "uinj";

        // Instantiate with a 10% fee
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: reward_denom.to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: Some(fee_recipient_addr.to_string()),
            fee_percentage: Some(Decimal::percent(10)), // 10% fee
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");

        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Mock pending rewards in the farm contract
        let total_rewards = Uint128::new(1000);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: total_rewards,
            },
        );

        // Mock the vault's balance *after* the harvest has occurred
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[cosmwasm_std::Coin {
                denom: reward_denom.to_string(),
                amount: total_rewards,
            }],
        )]);

        // --- Act ---
        // Trigger the harvest reply, which is where the fee logic lives
        let mut env = mock_env();
        env.contract.address = vault_addr;

        let payload = CompoundPayload {
            belief_price: Decimal::one(), // A default belief_price is fine for this test
        };
        let reply_msg = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(),
        };
        let res = reply(deps.as_mut(), env, reply_msg).unwrap();

        // --- Assert ---
        // The response should now have TWO messages: one for the fee, one submessage for the swap.
        assert_eq!(res.messages.len(), 2);

        // 1. Verify the Fee Message
        let expected_fee_amount = Uint128::new(100); // 10% of 1000
        let fee_message = res.messages.get(0).unwrap().clone().msg;
        let expected_fee_message = CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: fee_recipient_addr.to_string(),
            amount: vec![cosmwasm_std::Coin {
                denom: reward_denom.to_string(),
                amount: expected_fee_amount,
            }],
        });
        assert_eq!(fee_message, expected_fee_message);

        // 2. Verify the Swap Submessage
        let remaining_rewards = Uint128::new(900); // 1000 - 100
        let expected_swap_amount = remaining_rewards.multiply_ratio(1u128, 2u128); // Half of remaining

        let swap_submessage = res.messages.get(1).unwrap();
        assert_eq!(swap_submessage.id, SWAP_REPLY_ID);

        // To verify the amount, we need to decode the inner message
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &swap_submessage.msg {
            if let Ok(choice::pair::ExecuteMsg::Swap { offer_asset, .. }) = from_json(msg) {
                assert_eq!(offer_asset.amount, expected_swap_amount);
            } else {
                panic!("Could not decode inner Swap message");
            }
        } else {
            panic!("Expected a Wasm message for the swap");
        }
    }

    #[test]
    fn test_compound_minimum_reward_threshold() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let vault_addr = deps.api.addr_make("vault_contract");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let minimum_rewards = Uint128::new(100);

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: minimum_rewards,
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        let mut env = mock_env();
        env.contract.address = vault_addr.clone();

        // --- Scenario 1: Rewards BELOW Threshold ---
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::new(99),
            },
        );

        let msg = ExecuteMsg::Compound {
            belief_price: Decimal::one(),
        };
        let info = message_info(&compounder_addr, &[]); // Correct compounder calls
        let res = execute(deps.as_mut(), env.clone(), info, msg.clone());

        // Assert that it fails because rewards are too low
        assert!(
            matches!(res, Err(ContractError::Std(StdError::GenericErr { msg, .. })) if msg == "Pending rewards are below the minimum threshold to compound")
        );

        // --- Scenario 2: Rewards EQUAL to Threshold ---
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::new(100),
            },
        );

        let info = message_info(&compounder_addr, &[]);
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);
    }

    #[test]
    fn test_ownership_transfer_happy_path() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let new_owner_addr = deps.api.addr_make("new_owner");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        // Instantiate the contract
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: deps.api.addr_make("farm0000").to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::new(100),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");

        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Act 1: Propose New Owner ---
        let msg = ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner_addr.to_string(),
        };
        let info = message_info(&owner_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert 1: Proposal is stored correctly ---
        let config: Config =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(config.owner, owner_addr); // Owner has not changed yet
        assert_eq!(config.proposed_owner, Some(new_owner_addr.clone()));

        // --- Act 2: Accept Ownership ---
        let msg = ExecuteMsg::AcceptOwnership {};
        let info = message_info(&new_owner_addr, &[]); // The new owner accepts
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert 2: Ownership has been transferred ---
        let config: Config =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(config.owner, new_owner_addr); // Owner is now the new owner
        assert_eq!(config.proposed_owner, None); // Proposal has been cleared

        // --- Verification: Old owner can no longer execute owner actions ---
        let msg = ExecuteMsg::ProposeNewOwner {
            new_owner: owner_addr.to_string(),
        };
        let info = message_info(&owner_addr, &[]); // Old owner tries again
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(matches!(res, Err(ContractError::Unauthorized {})));
    }

    #[test]
    fn test_ownership_transfer_unauthorized_actions() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let new_owner_addr = deps.api.addr_make("new_owner");
        let random_user_addr = deps.api.addr_make("random_user");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        // Instantiate the contract
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            // ... other fields can be defaults
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: deps.api.addr_make("farm0000").to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Test 1: Random user cannot propose a new owner ---
        let msg = ExecuteMsg::ProposeNewOwner {
            new_owner: random_user_addr.to_string(),
        };
        let info = message_info(&random_user_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(matches!(res, Err(ContractError::Unauthorized {})));

        // --- Setup for next tests: Owner makes a valid proposal ---
        let msg = ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner_addr.to_string(),
        };
        let info = message_info(&owner_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Test 2: Random user cannot accept the proposal ---
        let msg = ExecuteMsg::AcceptOwnership {};
        let info = message_info(&random_user_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(
            matches!(res, Err(ContractError::Std( StdError::GenericErr { msg, .. })) if msg == "No ownership proposal for this address to accept")
        );

        // --- Test 3: Old owner cannot accept the proposal ---
        let msg = ExecuteMsg::AcceptOwnership {};
        let info = message_info(&owner_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(
            matches!(res, Err(ContractError::Std( StdError::GenericErr { msg, .. })) if msg == "No ownership proposal for this address to accept")
        );
    }

    #[test]
    fn test_ownership_transfer_cancel_proposal() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let new_owner_addr = deps.api.addr_make("new_owner");
        let random_user_addr = deps.api.addr_make("random_user");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        // Instantiate the contract
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: deps.api.addr_make("farm0000").to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Owner makes a valid proposal
        let msg = ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner_addr.to_string(),
        };
        let info = message_info(&owner_addr, &[]);
        execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap();

        // --- Act 1: Unauthorized user tries to cancel ---
        let msg = ExecuteMsg::CancelOwnershipProposal {};
        let unauthorized_info = message_info(&random_user_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), unauthorized_info, msg.clone());
        assert!(matches!(res, Err(ContractError::Unauthorized {})));

        // --- Act 2: Owner successfully cancels ---
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(res.is_ok());

        // --- Assert 2: Proposal is cleared ---
        let config: Config =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(config.proposed_owner, None);

        // --- Act 3: Proposed owner can no longer accept ---
        let msg = ExecuteMsg::AcceptOwnership {};
        let info = message_info(&new_owner_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(
            matches!(res, Err(ContractError::Std( StdError::GenericErr { msg, .. })) if msg == "No ownership proposal for this address to accept")
        );
    }

    #[test]
    fn test_compound_unauthorized_compounder() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let random_caller = deps.api.addr_make("random_caller");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: deps.api.addr_make("farm0000").to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Mock rewards so the call wouldn't fail for that reason
        deps.querier.with_staker_info(
            "farm0000".to_string(),
            StakerInfoResponse {
                staker: "vault_contract".to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::new(1000),
            },
        );

        // A random, non-compounder user calls Compound
        let msg = ExecuteMsg::Compound {
            belief_price: Decimal::one(),
        };
        let info = message_info(&random_caller, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // Assert that it fails with Unauthorized
        assert!(matches!(res, Err(ContractError::Unauthorized {})));
    }

    #[test]
    fn test_deposit_and_withdraw_native_lp() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let user1_addr = deps.api.addr_make("user1");

        // Define the native LP token denomination
        let native_lp_denom = "factory/inj1paircontract/lp";

        // Instantiate the contract with the native LP token configuration
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "reward".to_string(),
            },
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "b".to_string(),
                },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg.clone(),
        )
        .unwrap();

        // --- 1. TEST NATIVE DEPOSIT ---

        // Arrange for deposit
        let deposit_amount = Uint128::new(100);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::zero(),
                bond_amount: Uint128::zero(), // First depositor
                pending_reward: Uint128::zero(),
            },
        );

        // Act: Execute the native deposit
        let msg = ExecuteMsg::DepositNativeLp {};
        let info = message_info(
            &user1_addr,
            &[cosmwasm_std::coin(deposit_amount.u128(), native_lp_denom)], // Attach funds
        );
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // Assert deposit success
        // 1a. Check state: User should have shares
        let user_info: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user_info.shares, deposit_amount);

        // 1b. Check message: The contract must send a `Bond` message to the farm
        assert_eq!(res.messages.len(), 1);
        let expected_bond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Bond {
                amount: deposit_amount,
            })
            .unwrap(),
            funds: vec![cosmwasm_std::coin(deposit_amount.u128(), native_lp_denom)],
        });
        assert_eq!(res.messages[0].msg, expected_bond_msg);

        // --- 2. TEST NATIVE WITHDRAWAL ---

        // Arrange for withdrawal
        // We'll reset the state to simulate the user already having deposited.
        let mut deps = mock_dependencies(); // Fresh dependencies
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap(); // Re-instantiate

        TOTAL_SHARES
            .save(&mut deps.storage, &deposit_amount)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: deposit_amount,
                },
            )
            .unwrap();

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::one(),
                bond_amount: deposit_amount, // Vault has 100 native LP tokens staked
                pending_reward: Uint128::zero(),
            },
        );

        // Act: Execute the withdrawal
        let msg = ExecuteMsg::Withdraw {
            shares: deposit_amount,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // Assert withdrawal success
        // 2a. Check state: User's shares should be gone
        let user_info: UserInfoResponse = from_json(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user_info.shares, Uint128::zero());

        // 2b. Check messages: Should be an `Unbond` WasmMsg and a `BankMsg::Send`
        assert_eq!(res.messages.len(), 2);

        // Message 1 should be `Unbond`
        let expected_unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: deposit_amount,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[0].msg, expected_unbond_msg);

        // Message 2 should be `BankMsg::Send` to give the native tokens back
        let expected_send_msg = CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: user1_addr.to_string(),
            amount: vec![cosmwasm_std::coin(deposit_amount.u128(), native_lp_denom)],
        });
        assert_eq!(res.messages[1].msg, expected_send_msg);
    }
}

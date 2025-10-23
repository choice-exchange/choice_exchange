#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::contract::{
        execute, instantiate, query, reply, FINAL_SWAP_REPLY_ID, HARVEST_REPLY_ID,
        PROVIDE_LIQUIDITY_REPLY_ID, ROUTE_SWAP_REPLY_ID,
    };
    use crate::error::ContractError;
    use crate::mock_querier::mock_dependencies;
    use crate::msg::{
        CompoundRoutePayload, Cw20HookMsg, HarvestReplyPayload, PendingDepositsResponse,
        UserInfoResponse,
    };
    use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use crate::state::{
        CompoundingInfo, Config, UserInfo, TOTAL_PENDING_DEPOSITS, TOTAL_SHARES, USERS,
    };
    use choice::asset::AssetInfo;
    use choice::staking::{
        Cw20HookMsg as FarmCw20HookMsg, ExecuteMsg as FarmExecuteMsg, StakerInfoResponse,
    };
    use cosmwasm_std::testing::{message_info, mock_env};
    use cosmwasm_std::{
        from_json, to_json_binary, BankMsg, Coin, CosmosMsg, Decimal, StdError, SubMsg, Uint128,
        WasmMsg,
    };
    use cosmwasm_std::{Reply, SubMsgResponse, SubMsgResult};
    use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

    #[test]
    fn proper_initialization() {
        let mut deps = mock_dependencies();

        let owner_addr = deps.api.addr_make("owner");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let token_a_addr = deps.api.addr_make("token_a0000");
        let creator_addr = deps.api.addr_make("creator");

        let token_b_denom = "uinj";
        let reward_denom = "reward_denom";

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
            reward_to_lp_token_route: vec![],
        };

        let info = message_info(&creator_addr, &[]);

        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.messages.len(), 0);

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
            reward_to_lp_token_route: vec![],
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
            sender: user1_addr.to_string(),
            amount: deposit_amount,
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });
        let info = message_info(&lp_token_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the state changes
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

        assert_eq!(user_info_res.shares, Uint128::zero()); // User has NO shares yet.
        assert_eq!(user_info_res.pending_deposit, deposit_amount); // Deposit is pending.

        // Total shares should still be zero
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, Uint128::zero());

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
            reward_to_lp_token_route: vec![],
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Simulate User1 already having 100 shares.
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
                    pending_deposit: Uint128::zero(), // User1 has no pending deposits
                },
            )
            .unwrap();

        // --- Act ---
        // This part is the same: a second user (user2) deposits 60 LP tokens.
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
        // The assertions are now completely different. We check for pending deposits, not shares.
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

        // Verify that user2 has NO new shares yet.
        assert_eq!(user2_info.shares, Uint128::zero());
        // Verify that the deposited amount is in the pending_deposit field.
        assert_eq!(user2_info.pending_deposit, user2_deposit_amount);

        // Verify that total shares has NOT changed.
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, initial_shares); // Still 100
    }

    #[test]
    fn test_activate_deposit_calculates_proportional_shares() {
        // --- Arrange ---
        // 1. Setup the environment and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner"); // This will also be the compounder
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        // The instantiation message defines the contract's configuration.
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
            compounder: owner_addr.to_string(), // The owner is the keeper in this test
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator_addr = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Manually set up the state to simulate a vault that already has users and value.
        //    - User1 has 100 active shares.
        //    - User2 has deposited 60 LP tokens, which are currently pending.
        let user1_addr = deps.api.addr_make("user1");
        let user2_addr = deps.api.addr_make("user2");
        let initial_total_shares = Uint128::new(100);
        let user2_pending_amount = Uint128::new(60);

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_total_shares)
            .unwrap();

        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &user2_pending_amount)
            .unwrap();

        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_total_shares,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user2_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: user2_pending_amount,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier: This is the crucial part of the test.
        // We simulate that due to compounding, User1's original 100 shares are now worth 120 LP tokens.
        // Since User2's 60 pending LP tokens have already been staked by the deposit function,
        // the total amount of LP tokens in the farm (`bond_amount`) is 120 + 60 = 180.
        let value_of_existing_shares = Uint128::new(120);
        let total_lp_staked_in_farm = value_of_existing_shares + user2_pending_amount; // 120 + 60 = 180

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_lp_staked_in_farm,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The keeper (the owner in this case) calls the function to activate User2's pending deposit.
        let msg = ExecuteMsg::ActivatePendingDeposits {
            users: vec![user2_addr.to_string()],
        };
        let info = message_info(&owner_addr, &[]); // Message is from the authorized compounder
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the proportional share calculation.
        // The formula is: shares_to_mint = amount_to_activate * total_shares / total_lp_staked
        // In our scenario: 60 * 100 / 180 = 33.33... which truncates to 33 shares.
        // This is CORRECT. User2 gets fewer shares because each share is now worth more (1.2 LP tokens).
        let expected_new_shares =
            user2_pending_amount.multiply_ratio(initial_total_shares, total_lp_staked_in_farm);
        assert_eq!(expected_new_shares, Uint128::new(33));

        // 6. Query User2's state to confirm the changes.
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

        // User2's pending deposit should now be converted to the correct number of shares.
        assert_eq!(user2_info.shares, expected_new_shares);
        assert_eq!(user2_info.pending_deposit, Uint128::zero());

        // 7. Verify the contract's total shares have been updated correctly.
        // New total shares = 100 (from user1) + 33 (from user2) = 133
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, initial_total_shares + expected_new_shares);
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
            reward_to_lp_token_route: vec![],
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

        // We must now provide the full UserInfo struct, including the new field.
        // In this simple case, the user has no pending deposits.
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                    pending_deposit: Uint128::zero(),
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
        // 4. User1 withdraws their entire balance of 100 shares
        let msg = ExecuteMsg::Withdraw {
            shares: user1_shares,
        };
        let info = message_info(&user1_addr, &[]); // The user themselves sends this message
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes
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

        // The user's shares and pending deposits should both be zero after withdrawal.
        assert_eq!(user_info.shares, Uint128::zero());
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        // Total shares in the contract should be zero
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, Uint128::zero());

        // 6. Verify the returned messages
        // The logic for this specific case (no pending deposits) results in the same messages.
        assert_eq!(res.messages.len(), 2);

        // Message 1: Unbond from the farm
        // The amount to unbond should be 100 (from shares) + 0 (from pending) = 100.
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
            reward_to_lp_token_route: vec![],
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

        // We must provide the full UserInfo struct, including the new field.
        // In this proportional test, the user has no pending deposits.
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                    pending_deposit: Uint128::zero(),
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
        // 4. User1 withdraws their entire balance of 100 shares
        let msg = ExecuteMsg::Withdraw {
            shares: user1_shares,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the amount of LP tokens returned
        // The user should receive the proportional amount of the grown assets.
        // lp_from_shares = (shares * total_lp) / total_shares = (100 * 120) / 100 = 120.
        // pending_to_withdraw = 0.
        // total_lp_to_withdraw = 120 + 0 = 120.
        let expected_lp_to_receive = Uint128::new(120);

        // 6. Verify the messages
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
            reward_to_lp_token_route: vec![],
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

        // Update the UserInfo struct to include the new `pending_deposit` field.
        // For this test, the user has no pending funds.
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_user_shares,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();

        // 3. Setup Mock Querier: Use a simple 1:1 ratio for this test
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
        // 4. User1 withdraws 40 of their 100 shares
        let shares_to_withdraw = Uint128::new(40);
        let msg = ExecuteMsg::Withdraw {
            shares: shares_to_withdraw,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes.
        let expected_remaining_shares = Uint128::new(60);

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

        // Check both shares and pending deposits.
        assert_eq!(user_info.shares, expected_remaining_shares);
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        // Total shares should now be 60
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, expected_remaining_shares);

        // 6. Verify returned messages
        // The amount of LP tokens to unbond and transfer should be 40.
        // lp_from_shares = 40 * 100 / 100 = 40. pending = 0. total = 40.
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
        let pending_rewards = Uint128::new(20);
        let total_lp_staked = Uint128::new(1000);

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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

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
        let res = execute(deps.as_mut(), env.clone(), info, ExecuteMsg::Compound {}).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // ==> STEP 2: Handle Harvest Reply
        let payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
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
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, FINAL_SWAP_REPLY_ID);

        // ==> STEP 3: Handle Swap Reply
        let reply_msg = Reply {
            id: FINAL_SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(),
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

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
            payload: to_json_binary(&payload).unwrap(),
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

        let compounding_info: CompoundingInfo =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::CompoundingInfo {}).unwrap())
                .unwrap();
        assert_eq!(
            compounding_info.last_reward_amount_compounded,
            pending_rewards
        );
        assert_eq!(
            compounding_info.total_lp_staked_at_last_compound,
            total_lp_staked
        );
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

        let pending_rewards = Uint128::new(20);
        let total_lp_staked = Uint128::new(1000);

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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

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
        execute(deps.as_mut(), env.clone(), info, ExecuteMsg::Compound {}).unwrap();

        let payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
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
        reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // ==> Check STEP 3: Handle Swap Reply <==
        let reply_msg = Reply {
            id: FINAL_SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(),
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
            payload: to_json_binary(&payload).unwrap(),
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
        let creator_addr = deps.api.addr_make("creator");

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
            reward_to_lp_token_route: vec![],
        };
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

        // Update the UserInfo struct to include the new `pending_deposit` field.
        // For this test, the user has no pending funds.
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();

        // --- Act ---
        // 3. User1 attempts to withdraw 101 shares, which is more than they have
        let shares_to_withdraw = Uint128::new(101);
        let msg = ExecuteMsg::Withdraw {
            shares: shares_to_withdraw,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // --- Assert ---
        // 4. Verify the operation failed with the correct error
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
            reward_to_lp_token_route: vec![],
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
            reward_to_lp_token_route: vec![],
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
        let msg = ExecuteMsg::Compound {};
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
            reward_to_lp_token_route: vec![],
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

        let payload = HarvestReplyPayload {
            reward_amount_to_compound: total_rewards, // Use the actual reward amount
            tvl_before_compound: Uint128::new(1000),
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
        assert_eq!(swap_submessage.id, FINAL_SWAP_REPLY_ID);

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
            reward_to_lp_token_route: vec![],
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

        let msg = ExecuteMsg::Compound {};
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
            reward_to_lp_token_route: vec![],
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
            reward_to_lp_token_route: vec![],
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
            reward_to_lp_token_route: vec![],
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
            reward_to_lp_token_route: vec![],
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
        let msg = ExecuteMsg::Compound {};
        let info = message_info(&random_caller, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // Assert that it fails with Unauthorized
        assert!(matches!(res, Err(ContractError::Unauthorized {})));
    }

    #[test]
    fn test_deposit_native_lp_creates_pending_deposit() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let user1_addr = deps.api.addr_make("user1");
        let native_lp_denom = "factory/inj1paircontract/lp";
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            // ... other fields can be defaults for this test
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Act ---
        let deposit_amount = Uint128::new(100);
        let msg = ExecuteMsg::Deposit {};
        let info = message_info(
            &user1_addr,
            &[cosmwasm_std::coin(deposit_amount.u128(), native_lp_denom)],
        );
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 1a. Check state: User should have a pending deposit, but no shares.
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
        assert_eq!(user_info.pending_deposit, deposit_amount);

        // 1b. Check message: The contract must still send a `Bond` message to the farm.
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
    }

    // --- Test 2: Verify that the keeper can activate a pending native deposit ---
    #[test]
    fn test_activate_native_lp_deposit() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let user1_addr = deps.api.addr_make("user1");
        let native_lp_denom = "factory/inj1paircontract/lp";
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            // ... other fields can be defaults for this test
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Manually set up state: user has a pending deposit of 100 native LP tokens.
        let pending_amount = Uint128::new(100);
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &pending_amount)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: pending_amount,
                },
            )
            .unwrap();

        // Mock querier: the farm has the 100 native LP tokens staked.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::zero(),
                bond_amount: pending_amount,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        let msg = ExecuteMsg::ActivatePendingDeposits {
            users: vec![user1_addr.to_string()],
        };
        let info = message_info(&compounder_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
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
        assert_eq!(user_info.shares, pending_amount); // User now has 100 shares
        assert_eq!(user_info.pending_deposit, Uint128::zero()); // Pending is cleared
    }

    // --- Test 3: Verify that a user can withdraw their active native LP shares ---
    #[test]
    fn test_withdraw_native_lp_shares() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let user1_addr = deps.api.addr_make("user1");
        let native_lp_denom = "factory/inj1paircontract/lp";
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            // ... other fields can be defaults for this test
            slippage_tolerance: Decimal::percent(1),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Manually set up state: user has 100 active shares and no pending deposits.
        let shares_amount = Uint128::new(100);
        TOTAL_SHARES
            .save(&mut deps.storage, &shares_amount)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: shares_amount,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();

        // Mock querier: the farm has 100 native LP tokens staked, representing the 100 shares.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::one(),
                bond_amount: shares_amount,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        let msg = ExecuteMsg::Withdraw {
            shares: shares_amount,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 2a. Check state: User's shares should be gone.
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
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        // 2b. Check messages: Should be an `Unbond` WasmMsg and a `BankMsg::Send`.
        assert_eq!(res.messages.len(), 2);

        let expected_unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: shares_amount,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[0].msg, expected_unbond_msg);

        let expected_send_msg = CosmosMsg::Bank(BankMsg::Send {
            to_address: user1_addr.to_string(),
            amount: vec![cosmwasm_std::coin(shares_amount.u128(), native_lp_denom)],
        });
        assert_eq!(res.messages[1].msg, expected_send_msg);
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_with_multi_hop_route() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let compounder_addr = deps.api.addr_make("compounder");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let vault_addr = deps.api.addr_make("vault_contract");

        // --- Define all assets and pairs for the route ---
        let reward_token_sai = deps.api.addr_make("reward_sai_token");
        let intermediate_token_shroom = deps.api.addr_make("intermediate_shroom_token");
        let final_token_inj = "uinj"; // native
        let final_lp_token = deps.api.addr_make("shroom_inj_lp_token");

        let pending_rewards = Uint128::new(1000);
        let total_lp_staked = Uint128::new(5000);

        // Pair for the route hop (SAI -> SHROOM)
        let route_pair_sai_shroom = deps.api.addr_make("pair_sai_shroom");
        // Final pair for the LP (SHROOM/INJ)
        let final_pair_shroom_inj = deps.api.addr_make("pair_shroom_inj");

        // --- Instantiate with a 1-hop route ---
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: compounder_addr.to_string(),
            pair_contract: final_pair_shroom_inj.to_string(), // Final LP pair
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: final_lp_token.to_string(),
            },
            reward_token: AssetInfo::Token {
                contract_addr: reward_token_sai.to_string(),
            },
            asset_infos: [
                AssetInfo::Token {
                    contract_addr: intermediate_token_shroom.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: final_token_inj.to_string(),
                },
            ],
            reward_to_lp_token_route: vec![crate::msg::SwapHop {
                pair_contract: route_pair_sai_shroom.to_string(),
                to_asset_info: AssetInfo::Token {
                    contract_addr: intermediate_token_shroom.to_string(),
                },
            }],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            slippage_tolerance: Decimal::percent(1),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Mock the entire chain of events ---
        let mut env = mock_env();
        env.contract.address = vault_addr.clone();

        // 1. Farm has 1000 SAI as pending rewards
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(5000),
                pending_reward: Uint128::new(1000),
            },
        );
        // 2. After harvest, vault has 1000 SAI
        deps.querier.with_token_balance(
            &reward_token_sai.to_string(),
            &vault_addr.to_string(),
            Uint128::new(1000),
        );
        // 3. After first swap (SAI->SHROOM), vault has 500 SHROOM
        deps.querier.with_token_balance(
            &intermediate_token_shroom.to_string(),
            &vault_addr.to_string(),
            Uint128::new(500),
        );
        // 4. After second swap (50% of SHROOM -> INJ), vault has 250 SHROOM and 100 INJ
        deps.querier.with_token_balance(
            &intermediate_token_shroom.to_string(),
            &vault_addr.to_string(),
            Uint128::new(250),
        );
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[Coin::new(Uint128::new(100), final_token_inj)],
        )]);
        // 5. After providing liquidity, vault receives 150 new LP tokens
        deps.querier.with_token_balance(
            &final_lp_token.to_string(),
            &vault_addr.to_string(),
            Uint128::new(150),
        );

        // --- Execute the compound flow step-by-step ---

        // ==> STEP 1: Execute Compound
        // We need 2 belief prices: one for SAI->SHROOM, one for SHROOM->INJ

        let info = message_info(&compounder_addr, &[]);
        let res = execute(deps.as_mut(), env.clone(), info, ExecuteMsg::Compound {}).unwrap();
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // ==> STEP 2: Handle Harvest Reply -> Should start the route
        let harvest_payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
        };
        let reply_msg = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
            payload: to_json_binary(&harvest_payload).unwrap(),
            gas_used: 0,
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, ROUTE_SWAP_REPLY_ID); // It correctly starts the route

        // ==> STEP 3: Handle Route Swap Reply -> Route is now complete, should start final swap
        let route_payload = CompoundRoutePayload {
            hop_index: 1,
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
        };
        let reply_msg = Reply {
            id: ROUTE_SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
            payload: to_json_binary(&route_payload).unwrap(),
            gas_used: 0,
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, FINAL_SWAP_REPLY_ID); // It correctly transitions to the final swap

        // ==> STEP 4: Handle Final Swap Reply
        let final_swap_payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
        };
        let reply_msg = Reply {
            id: FINAL_SWAP_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
            payload: to_json_binary(&final_swap_payload).unwrap(),
            gas_used: 0,
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 2); // Allowance + Provide Liquidity
        assert_eq!(res.messages[1].id, PROVIDE_LIQUIDITY_REPLY_ID);

        // ==> STEP 5: Handle Provide Liquidity Reply
        let reply_msg = Reply {
            id: PROVIDE_LIQUIDITY_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
            payload: to_json_binary(&final_swap_payload).unwrap(),
            gas_used: 0,
        };
        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();
        assert_eq!(res.messages.len(), 1);
        assert!(res
            .attributes
            .iter()
            .any(|attr| attr.key == "status" && attr.value == "step_4_complete"));
        assert!(res
            .attributes
            .iter()
            .any(|attr| attr.key == "lp_tokens_staked" && attr.value == "150"));
    }

    #[test]
    fn test_withdraw_clears_both_shares_and_pending_deposits_correctly() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Set up a user with a mix of active shares and a new pending deposit.
        let user1_addr = deps.api.addr_make("user1");
        let user1_shares = Uint128::new(100);
        let user1_pending = Uint128::new(50);

        // This user is the only one, so they own 100% of the shares.
        TOTAL_SHARES.save(&mut deps.storage, &user1_shares).unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &user1_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: user1_shares,
                    pending_deposit: user1_pending,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier to define the vault's value.
        // Let's say due to compounding, the 100 active shares have grown to be worth 120 LP tokens.
        let value_of_active_shares = Uint128::new(120);
        // The total amount of LP staked in the farm is the value of the active shares (120)
        // plus the pending LPs that have also been staked (50).
        let total_lp_staked = value_of_active_shares + user1_pending; // 120 + 50 = 170

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
        // 4. User withdraws all 100 of their shares. This action should also automatically
        //    claim their 50 pending LP tokens.
        let msg = ExecuteMsg::Withdraw {
            shares: user1_shares,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. This section defines the CORRECT behavior the contract should have.

        // The user should receive the current value of their shares PLUS their pending deposit.
        // Value of Shares = 120 LP
        // Pending Deposit = 50 LP
        // Correct Total Withdrawal = 170 LP
        let expected_lp_to_receive = value_of_active_shares + user1_pending;
        assert_eq!(expected_lp_to_receive, Uint128::new(170));

        // 6. Verify the messages sent by the contract.
        assert_eq!(res.messages.len(), 2);

        // The contract should unbond the CORRECT total amount from the farm.
        let expected_unbond_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: expected_lp_to_receive,
            })
            .unwrap(),
            funds: vec![],
        }));
        assert_eq!(res.messages[0], expected_unbond_msg);

        // The contract should then transfer the CORRECT total amount to the user.
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

        // 7. Verify the user's state is completely cleared.
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
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        // 8. Verify the total shares of the contract is now zero.
        let total_shares: Uint128 =
            from_json(&query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, Uint128::zero());
    }

    #[test]
    fn test_withdraw_pending_only() {
        // --- Arrange ---
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // 2. Set up a user with ONLY a pending deposit.
        let user1_addr = deps.api.addr_make("user1");
        let user1_pending = Uint128::new(75);

        // The user has no shares, so TOTAL_SHARES is zero.
        // TOTAL_PENDING_DEPOSITS must reflect the user's pending amount.
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &user1_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: user1_pending,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier.
        // The total amount staked in the farm is only the pending LPs.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: user1_pending,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user calls `withdraw` with 0 shares to claim their pending funds.
        let msg = ExecuteMsg::Withdraw {
            shares: Uint128::zero(),
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Calculate the expected LP amount.
        // lp_from_shares should be 0.
        // lp_to_withdraw should be 0 + pending_to_withdraw = 75.
        let expected_lp_to_receive = user1_pending;

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

        // 7. Verify the user's state is completely removed from storage.
        let user_info_raw = USERS.may_load(&deps.storage, &user1_addr).unwrap();
        assert!(user_info_raw.is_none());

        // 8. Verify the totals are now zero.
        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();
        assert_eq!(total_pending, Uint128::zero());
    }

    #[test]
    fn test_query_pending_users_pagination() {
        // --- Arrange ---
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
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        // We create the Addr objects first, so we can reference them later.
        let user1_addr = deps.api.addr_make("user1"); // has pending
        let user2_addr = deps.api.addr_make("user2"); // has NO pending, should be skipped
        let user3_addr = deps.api.addr_make("user3"); // has pending
        let user4_addr = deps.api.addr_make("user4"); // has pending
        let user5_addr = deps.api.addr_make("user5"); // has pending

        // Save user info to storage
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(10),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user2_addr,
                &UserInfo {
                    shares: Uint128::new(100),
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user3_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(30),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user4_addr,
                &UserInfo {
                    shares: Uint128::new(50),
                    pending_deposit: Uint128::new(40),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user5_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(50),
                },
            )
            .unwrap();

        // --- Act & Assert: SCENARIO 1 - Get the first page ---
        println!("Testing first page...");
        let query_msg = QueryMsg::PendingDeposits {
            start_after: None,
            limit: Some(2),
        };
        let res = query(deps.as_ref(), mock_env(), query_msg).unwrap();
        let page1: PendingDepositsResponse = from_json(&res).unwrap();

        assert_eq!(page1.users.len(), 2, "Page 1 should return exactly 2 users");

        let all_pending_users: HashSet<String> = [
            user1_addr.to_string(),
            user3_addr.to_string(),
            user4_addr.to_string(),
            user5_addr.to_string(),
        ]
        .iter()
        .cloned()
        .collect();

        // Verify that the users returned are indeed in our master list of pending users.
        assert!(all_pending_users.contains(&page1.users[0]));
        assert!(all_pending_users.contains(&page1.users[1]));
        assert_ne!(
            page1.users[0], page1.users[1],
            "Returned users should be unique"
        ); // Sanity check

        // Dynamically get the last user from the *actual* response to use as the next cursor.
        let last_user_from_page1 = page1.last_user.clone().unwrap();

        // --- Act & Assert: SCENARIO 2 - Get the second page ---
        println!("Testing second page...");
        let query_msg_2 = QueryMsg::PendingDeposits {
            start_after: Some(last_user_from_page1.clone()), // Use the dynamic cursor
            limit: Some(2),
        };
        let res2 = query(deps.as_ref(), mock_env(), query_msg_2).unwrap();
        let page2: PendingDepositsResponse = from_json(&res2).unwrap();

        assert_eq!(
            page2.users.len(),
            2,
            "Page 2 should return the remaining 2 users"
        );
        assert!(all_pending_users.contains(&page2.users[0]));
        assert!(all_pending_users.contains(&page2.users[1]));

        // Ensure page 2 users are different from page 1 users
        assert!(!page1.users.contains(&page2.users[0]));
        assert!(!page1.users.contains(&page2.users[1]));

        let last_user_from_page2 = page2.last_user.clone().unwrap();

        // --- Act & Assert: SCENARIO 3 - Get the (non-existent) third page ---
        println!("Testing third (empty) page...");
        let query_msg_3 = QueryMsg::PendingDeposits {
            start_after: Some(last_user_from_page2),
            limit: Some(2),
        };
        let res3 = query(deps.as_ref(), mock_env(), query_msg_3).unwrap();
        let page3: PendingDepositsResponse = from_json(&res3).unwrap();

        assert!(page3.users.is_empty(), "Page 3 should be empty");
        assert!(
            page3.last_user.is_none(),
            "last_user should be None when the page is empty"
        );
    }
}

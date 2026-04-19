#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::contract::{
        execute, instantiate, migrate, query, reply, FINAL_SWAP_REPLY_ID, HARVEST_REPLY_ID,
        PROVIDE_LIQUIDITY_REPLY_ID, ROUTE_SWAP_REPLY_ID,
    };
    use crate::msg::MigrateMsg;
    use crate::error::ContractError;
    use crate::mock_querier::mock_dependencies;
    use crate::msg::{
        CompoundRoutePayload, Cw20HookMsg, HarvestReplyPayload, PendingDepositsResponse,
        UserInfoResponse,
    };
    use choice::asset::Asset;
    use choice::pair::PoolResponse;

    /// Build a `PoolResponse` with big reserves so the optimal-zap formula matches the legacy
    /// 50/50 split to within rounding — lets pre-H-3 assertions keep working without math churn.
    fn big_pool_response(a: AssetInfo, b: AssetInfo) -> PoolResponse {
        PoolResponse {
            assets: [
                Asset {
                    info: a,
                    amount: Uint128::new(1_000_000_000_000u128),
                },
                Asset {
                    info: b,
                    amount: Uint128::new(1_000_000_000_000u128),
                },
            ],
            total_share: Uint128::new(1_000_000_000_000u128),
        }
    }
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
        // reward_token is set equal to token_b so the compound path terminates on a pair asset
        // (see H-2 init-time validation in contract.rs).
        let reward_denom = token_b_denom;

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
                denom: "token_a".to_string(),
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
            query(
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
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
                denom: "token_a".to_string(),
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
            query(
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
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
                denom: "token_a".to_string(),
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
            user2_pending_amount.multiply_ratio(initial_total_shares, value_of_existing_shares); // Use the correct denominator
        assert_eq!(expected_new_shares, Uint128::new(50)); // Expect the fair amount of 50 shares

        // 6. Query User2's state to confirm the changes.
        let user2_info: UserInfoResponse = from_json(
            query(
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
        assert_eq!(user2_info.pending_deposit, Uint128::zero());

        // 7. Verify the contract's total shares have been updated correctly.
        // New total shares = 100 (from user1) + 50 (from user2) = 150
        let total_shares: Uint128 =
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
                .unwrap();
        assert_eq!(total_shares, initial_total_shares + expected_new_shares);
    }

    #[test]
    fn test_activate_pending_deposits_rejects_uncompounded_rewards() {
        // C-2 regression: activating while farm has unharvested rewards would silently dilute
        // existing shareholders (share price denominator excludes pending_reward). Verify the
        // guard blocks activation and unblocks it once the farm reports no pending reward.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");

        let minimum_reward = Uint128::new(1_000);
        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            minimum_reward_to_compound: minimum_reward,
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

        // Farm reports pending_reward at the compound threshold — activation must be rejected.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(180),
                pending_reward: minimum_reward,
            },
        );

        let msg = ExecuteMsg::ActivatePendingDeposits {
            users: vec![user2_addr.to_string()],
        };
        let info = message_info(&owner_addr, &[]);
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg.clone()).unwrap_err();
        match err {
            ContractError::PendingRewardsMustBeCompounded { pending } => {
                assert_eq!(pending, minimum_reward);
            }
            other => panic!("expected PendingRewardsMustBeCompounded, got {:?}", other),
        }

        // State must be untouched by the rejected call.
        let user2_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user2_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user2_info.shares, Uint128::zero());
        assert_eq!(user2_info.pending_deposit, user2_pending_amount);

        // Sub-threshold rewards are allowed (they can't be harvested anyway).
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(180),
                pending_reward: minimum_reward - Uint128::new(1),
            },
        );
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        let user2_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user2_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(user2_info.pending_deposit, Uint128::zero());
        assert!(user2_info.shares > Uint128::zero());
    }

    #[test]
    fn test_activate_pending_deposits_rejects_any_pending_when_minimum_is_zero() {
        // C-2 regression: when minimum_reward_to_compound is 0 the guard clamps to 1, so any
        // nonzero pending_reward still blocks activation.
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
                denom: "token_a".to_string(),
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

        let user_addr = deps.api.addr_make("user_pending");
        TOTAL_SHARES
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(50))
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(50),
                },
            )
            .unwrap();

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(150),
                pending_reward: Uint128::new(1),
            },
        );

        let msg = ExecuteMsg::ActivatePendingDeposits {
            users: vec![user_addr.to_string()],
        };
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            msg,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ContractError::PendingRewardsMustBeCompounded { .. }
        ));
    }

    /// Shared helper for C-3 tests: spins up a vault at initial_compounder with the given
    /// farm snapshot and a single pending-deposit user, so tests only contain the assertions
    /// that matter for the behavior under test.
    fn setup_vault_for_c3(
        initial_compounder: &cosmwasm_std::Addr,
        pending_reward: Uint128,
    ) -> (
        cosmwasm_std::OwnedDeps<
            cosmwasm_std::testing::MockStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
    ) {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner_c3");
        let farm_contract_addr = deps.api.addr_make("farm_c3");
        let lp_token_addr = deps.api.addr_make("lp_token_c3");
        let user_addr = deps.api.addr_make("user_c3");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_c3").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: initial_compounder.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator_addr = deps.api.addr_make("creator_c3");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator_addr, &[]),
            instantiate_msg,
        )
        .unwrap();

        TOTAL_SHARES
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(50))
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(50),
                },
            )
            .unwrap();

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(150),
                pending_reward,
            },
        );

        (deps, owner_addr, user_addr, farm_contract_addr)
    }

    #[test]
    fn test_compounder_rotation_rejects_apply_before_timelock() {
        // C-3 regression: proposing a new compounder must not take effect until the timelock
        // expires. ApplyCompounderRotation before the delay must error.
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, owner_addr, _, _) =
            setup_vault_for_c3(&initial_compounder, Uint128::zero());
        let new_compounder = deps.api.addr_make("keeper1");

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::ProposeCompounder {
                new_compounder: new_compounder.to_string(),
            },
        )
        .unwrap();

        // Same block — timelock has not elapsed.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::ApplyCompounderRotation,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::CompounderRotationNotReady {}));

        // And any stale batch-activation call by the *new* compounder must fail — they aren't
        // the active compounder yet. (C-3 made `Compound` permissionless, but
        // `ActivatePendingDeposits` still routes through the compounder field.)
        let someone = deps.api.addr_make("someone");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&new_compounder, &[]),
            ExecuteMsg::ActivatePendingDeposits {
                users: vec![someone.to_string()],
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }

    #[test]
    fn test_compounder_rotation_applies_after_timelock() {
        // C-3 regression: once the timelock has elapsed, the owner can finalize the rotation
        // and the new compounder replaces the old. The old compounder loses access.
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, owner_addr, _, _) =
            setup_vault_for_c3(&initial_compounder, Uint128::zero());
        let new_compounder = deps.api.addr_make("keeper1");

        let propose_env = mock_env();
        execute(
            deps.as_mut(),
            propose_env.clone(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::ProposeCompounder {
                new_compounder: new_compounder.to_string(),
            },
        )
        .unwrap();

        // Advance past the timelock.
        let mut applied_env = mock_env();
        applied_env.block.time = propose_env
            .block
            .time
            .plus_seconds(crate::state::COMPOUNDER_ROTATION_DELAY_SECONDS + 1);

        execute(
            deps.as_mut(),
            applied_env.clone(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::ApplyCompounderRotation,
        )
        .unwrap();

        // Old compounder is now unauthorized.
        let err_old = execute(
            deps.as_mut(),
            applied_env.clone(),
            message_info(&initial_compounder, &[]),
            ExecuteMsg::ActivatePendingDeposits { users: vec![] },
        )
        .unwrap_err();
        assert!(matches!(err_old, ContractError::Unauthorized {}));

        // New compounder is authorized (empty users vec just exercises the permission check).
        execute(
            deps.as_mut(),
            applied_env,
            message_info(&new_compounder, &[]),
            ExecuteMsg::ActivatePendingDeposits { users: vec![] },
        )
        .unwrap();
    }

    #[test]
    fn test_propose_compounder_rejects_non_owner() {
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, _, _, _) = setup_vault_for_c3(&initial_compounder, Uint128::zero());
        let attacker = deps.api.addr_make("attacker");

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&attacker, &[]),
            ExecuteMsg::ProposeCompounder {
                new_compounder: attacker.to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }

    #[test]
    fn test_cancel_compounder_proposal_clears_pending() {
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, owner_addr, _, _) =
            setup_vault_for_c3(&initial_compounder, Uint128::zero());
        let proposed = deps.api.addr_make("keeper1");

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::ProposeCompounder {
                new_compounder: proposed.to_string(),
            },
        )
        .unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner_addr, &[]),
            ExecuteMsg::CancelCompounderProposal,
        )
        .unwrap();

        // Subsequent apply — even after a long wait — errors with NoPending, not Timelock.
        let mut future = mock_env();
        future.block.time = future
            .block
            .time
            .plus_seconds(crate::state::COMPOUNDER_ROTATION_DELAY_SECONDS * 10);
        let err = execute(
            deps.as_mut(),
            future,
            message_info(&owner_addr, &[]),
            ExecuteMsg::ApplyCompounderRotation,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::NoPendingCompounderRotation {}));
    }

    #[test]
    fn test_activate_my_deposit_works_without_keeper() {
        // C-3 regression: users can self-activate their pending deposits without waiting on
        // the keeper, so a dead keeper cannot permanently strand their capital.
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, _, user_addr, _) =
            setup_vault_for_c3(&initial_compounder, Uint128::zero());

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user_addr, &[]),
            ExecuteMsg::ActivateMyDeposit {},
        )
        .unwrap();

        let info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(info.pending_deposit, Uint128::zero());
        assert!(info.shares > Uint128::zero());
    }

    #[test]
    fn test_activate_my_deposit_respects_dilution_guard() {
        // C-3 must not bypass the C-2 guard: self-activation still errors when pending farm
        // rewards would dilute existing holders.
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, _, user_addr, _) = setup_vault_for_c3(&initial_compounder, Uint128::new(5));

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user_addr, &[]),
            ExecuteMsg::ActivateMyDeposit {},
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ContractError::PendingRewardsMustBeCompounded { .. }
        ));
    }

    #[test]
    fn test_activate_my_deposit_errors_without_pending() {
        let initial_compounder = cosmwasm_std::testing::MockApi::default().addr_make("keeper0");
        let (mut deps, _, _, _) = setup_vault_for_c3(&initial_compounder, Uint128::zero());
        let random = deps.api.addr_make("no_deposit_user");

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::ActivateMyDeposit {},
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::NoPendingDeposit {}));
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: user1_shares,
        };
        let info = message_info(&user1_addr, &[]); // The user themselves sends this message
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes
        let user_info: UserInfoResponse = from_json(
            query(
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: user1_shares,
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: shares_to_withdraw,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify state changes.
        let expected_remaining_shares = Uint128::new(60);

        let user_info: UserInfoResponse = from_json(
            query(
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::TotalShares {}).unwrap())
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

        let token_a_addr = deps.api.addr_make("token_a0000"); // CW20
        let token_b_denom = "uusd"; // Native
        // reward_token must be one of the pair's assets (H-2) — pick token_b (native).
        let reward_denom = token_b_denom;

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

        // H-3 mock: pool has to return reserves for the optimal-zap query. Large reserves so
        // the new formula rounds to ~50/50 and existing downstream assertions keep holding.
        deps.querier.with_pool(
            pair_contract_addr.to_string(),
            big_pool_response(
                AssetInfo::Token {
                    contract_addr: token_a_addr.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ),
        );

        // reward_denom == token_b_denom, so both legs live under the same denom entry.
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[cosmwasm_std::Coin {
                denom: token_b_denom.to_string(),
                amount: pending_rewards + Uint128::new(10),
            }],
        )]);

        // This is the balance of the CW20 asset after the swap
        let cw20_balance_after_swap = Uint128::new(8);
        deps.querier.with_token_balance(
            token_a_addr.as_ref(),
            vault_addr.as_ref(),
            cw20_balance_after_swap,
        );
        deps.querier.with_token_balance(
            lp_token_addr.as_ref(),
            vault_addr.as_ref(),
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
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // ==> STEP 2: Handle Harvest Reply
        let payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::CompoundingInfo {}).unwrap())
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

        let token_a_denom = "uatom"; // Native
        let token_b_denom = "uusd"; // Native
        // reward_token must be one of the pair's assets (H-2) — pick token_a.
        let reward_denom = token_a_denom;

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

        // H-3: mock the pool query that optimal_zap_amount_xyk consults.
        deps.querier.with_pool(
            pair_contract_addr.to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: token_a_denom.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ),
        );

        // reward_denom == token_a_denom, so the reward and "asset A after swap" balances
        // live under the same entry.
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[
                cosmwasm_std::Coin {
                    denom: token_a_denom.to_string(),
                    amount: pending_rewards + Uint128::new(8),
                },
                cosmwasm_std::Coin {
                    denom: token_b_denom.to_string(),
                    amount: Uint128::new(10),
                },
            ],
        )]);

        // We only need to mock the LP token balance, as it's a CW20
        deps.querier.with_token_balance(
            lp_token_addr.as_ref(),
            vault_addr.as_ref(),
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
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap();

        let payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
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
    fn test_compound_rejects_wrong_belief_prices_length() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // No route configured -> expect exactly 1 belief price. Pass 0 and 2 and expect rejection.
        let info = message_info(&owner_addr, &[]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info.clone(),
            ExecuteMsg::Compound {
                belief_prices: vec![],
                minimum_lp_to_receive: Uint128::new(1),
            },
        );
        assert!(matches!(
            res,
            Err(ContractError::InvalidBeliefPrices {
                expected: 1,
                got: 0
            })
        ));

        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one(), Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        );
        assert!(matches!(
            res,
            Err(ContractError::InvalidBeliefPrices {
                expected: 1,
                got: 2
            })
        ));
    }

    #[test]
    fn test_compound_rejects_zero_belief_price() {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        let info = message_info(&owner_addr, &[]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::zero()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        );
        assert!(matches!(res, Err(ContractError::ZeroBeliefPrice {})));
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_swap_submsg_sets_belief_price_and_slippage() {
        // Verifies C-1 fix: every swap carries the caller-supplied belief_price,
        // and the ProvideLiquidity call carries a non-None slippage_tolerance.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let vault_addr = deps.api.addr_make("vault_contract");
        let creator_addr = deps.api.addr_make("creator");

        let token_a_denom = "uatom";
        let token_b_denom = "uusd";
        // reward_token must be one of the pair's assets (H-2) — pick token_a.
        let reward_denom = token_a_denom;

        let total_rewards = Uint128::new(1000);
        let slippage = Decimal::permille(5); // 0.5%
        let belief_price = Decimal::percent(200); // 2.0

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
            slippage_tolerance: slippage,
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
                pending_reward: total_rewards,
            },
        );
        deps.querier.with_pool(
            pair_contract_addr.to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: token_a_denom.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: token_b_denom.to_string(),
                },
            ),
        );
        // reward_denom == token_a_denom, merged into one entry.
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[
                cosmwasm_std::Coin {
                    denom: token_a_denom.to_string(),
                    amount: total_rewards + Uint128::new(500),
                },
                cosmwasm_std::Coin {
                    denom: token_b_denom.to_string(),
                    amount: Uint128::new(500),
                },
            ],
        )]);

        let mut env = mock_env();
        env.contract.address = vault_addr;

        // Drive the harvest reply, which builds the swap submsg.
        let payload = HarvestReplyPayload {
            reward_amount_to_compound: total_rewards,
            tvl_before_compound: Uint128::new(1000),
            belief_prices: vec![belief_price],
            minimum_lp_to_receive: Uint128::new(1),
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

        let swap_submsg = res.messages.last().unwrap();
        assert_eq!(swap_submsg.id, FINAL_SWAP_REPLY_ID);
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &swap_submsg.msg {
            if let Ok(choice::pair::ExecuteMsg::Swap {
                belief_price: bp,
                max_spread,
                ..
            }) = from_json(msg)
            {
                assert_eq!(bp, Some(belief_price));
                assert_eq!(max_spread, Some(slippage));
            } else {
                panic!("inner msg is not a Swap");
            }
        } else {
            panic!("outer msg is not a Wasm execute");
        }

        // Drive the final swap reply and verify ProvideLiquidity carries slippage_tolerance.
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
        let provide_submsg = res.messages.last().unwrap();
        assert_eq!(provide_submsg.id, PROVIDE_LIQUIDITY_REPLY_ID);
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &provide_submsg.msg {
            if let Ok(choice::pair::ExecuteMsg::ProvideLiquidity {
                slippage_tolerance, ..
            }) = from_json(msg)
            {
                assert_eq!(slippage_tolerance, Some(slippage));
            } else {
                panic!("inner msg is not ProvideLiquidity");
            }
        } else {
            panic!("outer msg is not a Wasm execute");
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_compound_rejects_insufficient_lp_received() {
        // Verifies C-1 fix: minimum_lp_to_receive reverts the compound when the
        // provide step mints fewer LP than the caller specified.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let pair_contract_addr = deps.api.addr_make("pair0000");
        let vault_addr = deps.api.addr_make("vault_contract");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: pair_contract_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        let minted_lp = Uint128::new(50);
        let minimum_required = Uint128::new(100);
        deps.querier.with_token_balance(
            lp_token_addr.as_ref(),
            vault_addr.as_ref(),
            minted_lp,
        );

        let mut env = mock_env();
        env.contract.address = vault_addr;

        let payload = HarvestReplyPayload {
            reward_amount_to_compound: Uint128::new(1000),
            tvl_before_compound: Uint128::new(1000),
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: minimum_required,
        };
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
        let res = reply(deps.as_mut(), env, reply_msg);
        assert!(matches!(
            res,
            Err(ContractError::InsufficientLpReceived {
                minimum,
                got,
            }) if minimum == minimum_required && got == minted_lp
        ));
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: shares_to_withdraw,
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
                denom: "token_a".to_string(),
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
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
        // reward_token must be one of the pair's assets (H-2) — pick token_a.
        let reward_denom = "token_a";

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
        deps.querier.with_pool(
            pair_contract_addr.to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ),
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
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
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
        let fee_message = res.messages.first().unwrap().clone().msg;
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
                denom: "token_a".to_string(),
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

        // B-6: heuristic runs after the threshold check. Mock the pool so Scenario 2
        // gets past the query; Scenario 1 short-circuits before the heuristic.
        deps.querier.with_pool(
            deps.api.addr_make("pair0000").to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ),
        );

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

        // B-6: min_lp = 5 clears the heuristic floor (≈4 at pending_reward=100 against big_pool)
        // in Scenario 2 below; Scenario 1's threshold-reject runs before the heuristic so this
        // value is immaterial there.
        let msg = ExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(5),
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
                denom: "token_a".to_string(),
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(config.owner, owner_addr); // Owner has not changed yet
        assert_eq!(config.proposed_owner, Some(new_owner_addr.clone()));

        // --- Act 2: Accept Ownership ---
        let msg = ExecuteMsg::AcceptOwnership {};
        let info = message_info(&new_owner_addr, &[]); // The new owner accepts
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert 2: Ownership has been transferred ---
        let config: Config =
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
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
                denom: "token_a".to_string(),
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
                denom: "token_a".to_string(),
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
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
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
    fn test_compound_rejects_zero_minimum_lp_to_receive() {
        // C-3: Compound is now permissionless, but every caller MUST commit to a
        // non-zero `minimum_lp_to_receive` so the vault's downside is bounded if the
        // caller is lax or gets sandwiched.
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
                denom: "token_a".to_string(),
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

        // A random, non-compounder user calls Compound with zero min_lp — must reject.
        let msg = ExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::zero(),
        };
        let info = message_info(&random_caller, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        assert!(matches!(res, Err(ContractError::MinimumLpToReceiveZero {})));

        // Same caller with non-zero minimum_lp_to_receive must NOT be Unauthorized — C-3 made
        // the call permissionless. (The call may still fail downstream on reward queries in
        // this minimal harness; that's not the property under test here.)
        let msg = ExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
        };
        let info = message_info(&random_caller, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);
        match res {
            Err(ContractError::Unauthorized {}) => {
                panic!("Compound should be permissionless post-C-3")
            }
            _ => { /* any other outcome is fine for this test */ }
        }
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
                denom: "token_a".to_string(),
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
            query(
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
                denom: "token_a".to_string(),
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
            query(
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
                denom: "token_a".to_string(),
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
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: shares_amount,
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 2a. Check state: User's shares should be gone.
        let user_info: UserInfoResponse = from_json(
            query(
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
            reward_token_sai.as_ref(),
            vault_addr.as_ref(),
            Uint128::new(1000),
        );
        // 3. After first swap (SAI->SHROOM), vault has 500 SHROOM
        deps.querier.with_token_balance(
            intermediate_token_shroom.as_ref(),
            vault_addr.as_ref(),
            Uint128::new(500),
        );
        // 4. After second swap (50% of SHROOM -> INJ), vault has 250 SHROOM and 100 INJ
        deps.querier.with_token_balance(
            intermediate_token_shroom.as_ref(),
            vault_addr.as_ref(),
            Uint128::new(250),
        );
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[Coin::new(Uint128::new(100), final_token_inj)],
        )]);
        // 5. After providing liquidity, vault receives 150 new LP tokens
        deps.querier.with_token_balance(
            final_lp_token.as_ref(),
            vault_addr.as_ref(),
            Uint128::new(150),
        );
        // H-3: end-of-route path queries the final pair's Pool for the optimal zap split.
        deps.querier.with_pool(
            final_pair_shroom_inj.to_string(),
            big_pool_response(
                AssetInfo::Token {
                    contract_addr: intermediate_token_shroom.to_string(),
                },
                AssetInfo::NativeToken {
                    denom: final_token_inj.to_string(),
                },
            ),
        );
        // B-6: heuristic walks each route hop via Simulation. Mock SAI→SHROOM →
        // a tiny return so the terminal-asset amount is below `optimal_zap`'s
        // resolution against big reserves; expected_lp rounds to zero and the
        // heuristic fails open. The downstream mocks (token balances) still
        // drive the actual reply-chain assertions.
        deps.querier.with_simulation(
            route_pair_sai_shroom.to_string(),
            AssetInfo::Token {
                contract_addr: reward_token_sai.to_string(),
            },
            choice::pair::SimulationResponse {
                return_amount: Uint128::new(1),
                spread_amount: Uint128::zero(),
                commission_amount: Uint128::zero(),
            },
        );

        // --- Execute the compound flow step-by-step ---

        // ==> STEP 1: Execute Compound
        // We need 2 belief prices: one for SAI->SHROOM, one for SHROOM->INJ

        let belief_prices = vec![Decimal::one(), Decimal::one()];
        let info = message_info(&compounder_addr, &[]);
        let res = execute(
            deps.as_mut(),
            env.clone(),
            info,
            ExecuteMsg::Compound {
                belief_prices: belief_prices.clone(),
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap();
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // ==> STEP 2: Handle Harvest Reply -> Should start the route
        let harvest_payload = HarvestReplyPayload {
            reward_amount_to_compound: pending_rewards,
            tvl_before_compound: total_lp_staked,
            belief_prices: belief_prices.clone(),
            minimum_lp_to_receive: Uint128::new(1),
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
            belief_prices: belief_prices.clone(),
            minimum_lp_to_receive: Uint128::new(1),
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
            belief_prices: belief_prices.clone(),
            minimum_lp_to_receive: Uint128::new(1),
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
    fn test_withdraw_shares_with_large_numbers() {
        // --- Arrange ---
        // This test verifies that withdrawing shares using very large numbers
        // (to simulate high-decimal tokens) works correctly and does not affect
        // the user's separate pending deposit.

        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let user1_addr = deps.api.addr_make("user1");
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // Define a huge number that mimics a token with 18 decimals and a large supply.
        const HUGE_NUMBER: u128 = 60_000_000_000_000_000_000_000_000_000;

        // 2. Set up state with these huge numbers.
        let initial_user_shares = Uint128::new(HUGE_NUMBER * 2); // User has 2H shares
        let initial_user_pending = Uint128::new(HUGE_NUMBER); // User has 1H pending
        let total_shares_before = initial_user_shares;

        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_user_shares,
                    pending_deposit: initial_user_pending,
                },
            )
            .unwrap();
        TOTAL_SHARES
            .save(&mut deps.storage, &total_shares_before)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &initial_user_pending)
            .unwrap();

        // 3. Set up the mock querier.
        // The value of the 2H active shares has grown to be worth 3H LP tokens.
        let value_of_active_shares = Uint128::new(HUGE_NUMBER * 3);
        // The total value in the farm is the value of shares (3H) + pending deposits (1H).
        let total_lp_staked = value_of_active_shares + initial_user_pending;
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_lp_staked,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user decides to withdraw/burn half of their shares (1H shares).
        let shares_to_burn = Uint128::new(HUGE_NUMBER);

        let msg = ExecuteMsg::WithdrawShares { shares_to_burn };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the state changes are correct and isolated.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        // ASSERT 1: The pending deposit MUST be unchanged.
        assert_eq!(user_info.pending_deposit, initial_user_pending);

        // ASSERT 2: The user's share balance should be correctly reduced.
        let expected_remaining_shares = initial_user_shares - shares_to_burn;
        assert_eq!(user_info.shares, expected_remaining_shares);
        assert_eq!(expected_remaining_shares, Uint128::new(HUGE_NUMBER)); // Sanity check

        // ASSERT 3: The correct proportional amount of LP tokens should be withdrawn.
        // Share price = 3H LP / 2H shares = 1.5 LP/share.
        // LP to receive = 1H shares * 1.5 = 1.5H LP.
        let expected_lp_to_receive =
            shares_to_burn.multiply_ratio(value_of_active_shares, total_shares_before);
        assert_eq!(
            expected_lp_to_receive,
            Uint128::new(HUGE_NUMBER + HUGE_NUMBER / 2)
        );

        // ASSERT 4: Verify the messages sent for unbonding and transfer.
        assert_eq!(res.messages.len(), 2);

        let expected_unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: expected_lp_to_receive,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[0].msg, expected_unbond_msg);

        let expected_transfer_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: user1_addr.to_string(),
                amount: expected_lp_to_receive,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[1].msg, expected_transfer_msg);
    }

    #[test]
    fn test_activate_pending_deposits_fair_pricing() {
        // --- Arrange ---
        // This test verifies that the activation logic correctly subtracts ALL pending
        // deposits to find the fair share price, ensuring no dilution for new depositors.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let user1_addr = deps.api.addr_make("user1");
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: owner_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: deps.api.addr_make("lp_token").to_string(),
            },
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        const HUGE_NUMBER: u128 = 60_000_000_000_000_000_000_000; // 6e22

        // 2. Set up existing state
        let total_shares_before = Uint128::new(HUGE_NUMBER * 10); // 10H
        let value_of_existing_shares = Uint128::new(HUGE_NUMBER * 10); // 10H (Fair price is 1:1)

        // 3. Set up TWO users with pending deposits
        let user1_pending = Uint128::new(HUGE_NUMBER * 2); // 2H
        let user2_pending = Uint128::new(HUGE_NUMBER * 3); // 3H
        let total_pending = user1_pending + user2_pending; // 5H

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
        let user2_addr = deps.api.addr_make("user2");
        USERS
            .save(
                &mut deps.storage,
                &user2_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: user2_pending,
                },
            )
            .unwrap();

        TOTAL_SHARES
            .save(&mut deps.storage, &total_shares_before)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &total_pending)
            .unwrap();

        // 4. Farm has EVERYTHING staked (active + all pending) = 15H
        let total_lp_staked = value_of_existing_shares + total_pending;
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_lp_staked,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        let msg = ExecuteMsg::ActivatePendingDeposits {
            users: vec![user1_addr.to_string()],
        };
        let info = message_info(&owner_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        let user1_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        let expected_shares = Uint128::new(HUGE_NUMBER * 2);
        assert_eq!(user1_info.shares, expected_shares);
        assert_eq!(user1_info.pending_deposit, Uint128::zero());
    }

    #[test]
    fn test_activate_first_deposit_ever() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            compounder: owner_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_token".to_string(),
            },
            // ... other fields can be defaults
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with a pending deposit, but TOTAL_SHARES is zero.
        let user1_addr = deps.api.addr_make("user1");
        let pending_amount = Uint128::new(150);
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
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &pending_amount)
            .unwrap();

        // 3. The farm has the 150 LP staked.
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
        let info = message_info(&owner_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        // In the initial case, shares minted should be 1:1 with the deposit amount.
        assert_eq!(user_info.shares, pending_amount);
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        let total_shares = TOTAL_SHARES.load(&deps.storage).unwrap();
        assert_eq!(total_shares, pending_amount);
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
                denom: "token_a".to_string(),
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

        assert!(page3.users.is_empty(), "Page 3 should have no pending users");

        // The cursor must still advance past any non-pending users that were
        // iterated on this page, so paginate again to confirm termination.
        if let Some(cursor) = page3.last_user {
            let query_msg_4 = QueryMsg::PendingDeposits {
                start_after: Some(cursor),
                limit: Some(2),
            };
            let res4 = query(deps.as_ref(), mock_env(), query_msg_4).unwrap();
            let page4: PendingDepositsResponse = from_json(&res4).unwrap();
            assert!(page4.users.is_empty());
            assert!(
                page4.last_user.is_none(),
                "last_user should be None once the iterator is exhausted"
            );
        }
    }

    #[test]
    fn test_query_pending_users_pagination_advances_past_non_pending_page() {
        // Regression: if a full page of iterated users has no pending deposits,
        // the cursor must still advance. Otherwise the keeper stops paginating
        // and pending deposits beyond the page are silently missed.
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
                denom: "token_a".to_string(),
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

        // Create 10 users, only 1 with a pending deposit. With limit=2, at
        // least one page will contain zero pending users regardless of the
        // sort order produced by addr_make.
        let mut expected_pending: HashSet<String> = HashSet::new();
        for i in 0..10 {
            let addr = deps.api.addr_make(&format!("user{i}"));
            let pending = if i == 7 {
                expected_pending.insert(addr.to_string());
                Uint128::new(42)
            } else {
                Uint128::zero()
            };
            USERS
                .save(
                    &mut deps.storage,
                    &addr,
                    &UserInfo {
                        shares: Uint128::new(100),
                        pending_deposit: pending,
                    },
                )
                .unwrap();
        }

        // Paginate to exhaustion, collecting every user returned as pending.
        let mut collected: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;
        loop {
            let res = query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::PendingDeposits {
                    start_after: cursor.clone(),
                    limit: Some(2),
                },
            )
            .unwrap();
            let page: PendingDepositsResponse = from_json(&res).unwrap();
            for u in &page.users {
                collected.insert(u.clone());
            }
            match page.last_user {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        assert_eq!(
            collected, expected_pending,
            "pagination must return every user with a pending deposit"
        );
    }

    #[test]
    fn test_multiple_pending_deposits_accumulate() {
        // --- Arrange ---
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let user1_addr = deps.api.addr_make("user1");
        let creator = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields can be defaults
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap();

        // --- Act ---
        // User deposits 100 LP tokens.
        let msg1 = ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user1_addr.to_string(),
            amount: Uint128::new(100),
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });
        let info1 = message_info(&lp_token_addr, &[]);
        execute(deps.as_mut(), mock_env(), info1, msg1).unwrap();

        // Before activation, user deposits another 50 LP tokens.
        let msg2 = ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user1_addr.to_string(),
            amount: Uint128::new(50),
            msg: to_json_binary(&Cw20HookMsg::Deposit {}).unwrap(),
        });
        let info2 = message_info(&lp_token_addr, &[]);
        execute(deps.as_mut(), mock_env(), info2, msg2).unwrap();

        // --- Assert ---
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        // User should have NO shares yet.
        assert_eq!(user_info.shares, Uint128::zero());
        // The pending deposit should be the sum of both deposits.
        assert_eq!(user_info.pending_deposit, Uint128::new(150));

        let total_pending: Uint128 = from_json(
            query(deps.as_ref(), mock_env(), QueryMsg::TotalPendingDeposits {}).unwrap(),
        )
        .unwrap();
        assert_eq!(total_pending, Uint128::new(150));
    }

    #[test]
    fn test_withdraw_pending_full_amount() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with ONLY a pending deposit and no active shares.
        let user1_addr = deps.api.addr_make("user1");
        let user1_pending = Uint128::new(75);

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

        // 3. Setup Mock Querier. The farm's staked balance is equal to the pending deposit.
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
        // 4. The user calls `WithdrawPending` with `amount: None` to claim their entire pending balance.
        let msg = ExecuteMsg::WithdrawPending { amount: None };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the correct messages are sent.
        assert_eq!(res.messages.len(), 2);

        let expected_unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm_contract_addr.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Unbond {
                amount: user1_pending,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[0].msg, expected_unbond_msg);

        let expected_transfer_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: lp_token_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: user1_addr.to_string(),
                amount: user1_pending,
            })
            .unwrap(),
            funds: vec![],
        });
        assert_eq!(res.messages[1].msg, expected_transfer_msg);

        // 6. Verify the user's state is completely removed from storage for gas efficiency.
        let user_info_raw = USERS.may_load(&deps.storage, &user1_addr).unwrap();
        assert!(
            user_info_raw.is_none(),
            "User info should be removed after full withdrawal with no other assets"
        );

        // 7. Verify the global pending deposit total is now zero.
        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();
        assert_eq!(total_pending, Uint128::zero());

        // 8. Verify attributes are correct
        assert_eq!(
            res.attributes[1],
            cosmwasm_std::attr("withdrawer", user1_addr.to_string())
        );
        assert_eq!(res.attributes[2], cosmwasm_std::attr("shares_burnt", "0"));
        assert_eq!(
            res.attributes[3],
            cosmwasm_std::attr("pending_lp_withdrawn", "75")
        );
        assert_eq!(
            res.attributes[4],
            cosmwasm_std::attr("total_lp_withdrawn", "75")
        );
    }

    #[test]
    fn test_withdraw_pending_partial_amount() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default for this test
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with an existing share balance and a pending deposit.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(50);
        let initial_pending = Uint128::new(100);

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &initial_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                    pending_deposit: initial_pending,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier. Farm's value is shares + pending.
        // Assuming 1:1 share price for simplicity.
        let total_staked = initial_shares + initial_pending;
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_staked, // 50 + 100 = 150
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user calls `WithdrawPending` to withdraw 40 of their 100 pending LP tokens.
        let amount_to_withdraw = Uint128::new(40);
        let msg = ExecuteMsg::WithdrawPending {
            amount: Some(amount_to_withdraw),
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the user's state has been updated correctly.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        // Shares should be unchanged.
        assert_eq!(user_info.shares, initial_shares);
        // Pending deposit should be reduced.
        assert_eq!(
            user_info.pending_deposit,
            initial_pending - amount_to_withdraw
        );
        assert_eq!(user_info.pending_deposit, Uint128::new(60));

        // 6. Verify global state has been updated.
        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();
        assert_eq!(total_pending, initial_pending - amount_to_withdraw);

        // 7. Verify messages are correct.
        assert_eq!(res.messages.len(), 2);
        // Unbond message should be for the withdrawn amount.
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &res.messages[0].msg {
            if let Ok(FarmExecuteMsg::Unbond { amount }) = from_json(msg) {
                assert_eq!(amount, amount_to_withdraw);
            } else {
                panic!("Incorrect Wasm message type for unbond");
            }
        } else {
            panic!("Expected Wasm message for unbond");
        }
        // Transfer message should also be for the withdrawn amount.
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &res.messages[1].msg {
            if let Ok(Cw20ExecuteMsg::Transfer { amount, .. }) = from_json(msg) {
                assert_eq!(amount, amount_to_withdraw);
            } else {
                panic!("Incorrect Wasm message type for transfer");
            }
        } else {
            panic!("Expected Wasm message for transfer");
        }
    }

    #[test]
    fn test_withdraw_pending_does_not_affect_shares() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with a significant balance of both active shares and pending deposits.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(100);
        let initial_pending = Uint128::new(50);

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &initial_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                    pending_deposit: initial_pending,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier. Farm's value is shares + pending.
        let total_staked = Uint128::new(150); // For simplicity, assume 1:1 share value
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_staked,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user calls `WithdrawPending` to withdraw their ENTIRE pending balance.
        let msg = ExecuteMsg::WithdrawPending {
            amount: Some(initial_pending),
        };
        let info = message_info(&user1_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the user's state.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        // CRITICAL ASSERTION: The user's share balance must be completely unchanged.
        assert_eq!(
            user_info.shares, initial_shares,
            "Shares should not be affected by withdrawing pending deposits"
        );

        // The pending deposit should now be zero.
        assert_eq!(user_info.pending_deposit, Uint128::zero());

        // 6. Verify global state.
        let total_shares = TOTAL_SHARES.load(&deps.storage).unwrap();
        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();

        // CRITICAL ASSERTION: Total shares in the contract must be unchanged.
        assert_eq!(
            total_shares, initial_shares,
            "Total shares should not be affected"
        );

        // Total pending should now be zero.
        assert_eq!(total_pending, Uint128::zero());
    }

    #[test]
    fn test_withdraw_pending_error_insufficient_funds() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default for this test
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with a pending deposit of 50 LP.
        let user1_addr = deps.api.addr_make("user1");
        let initial_pending = Uint128::new(50);

        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &initial_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: initial_pending,
                },
            )
            .unwrap();

        // --- Act ---
        // 3. The user attempts to withdraw 51 LP, which is more than they have pending.
        let amount_to_withdraw = Uint128::new(51);
        let msg = ExecuteMsg::WithdrawPending {
            amount: Some(amount_to_withdraw),
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // --- Assert ---
        // 4. Verify that the execution failed with the correct error.
        match res {
            Err(ContractError::Std(StdError::GenericErr { msg, .. })) => {
                assert_eq!(msg, "Insufficient pending deposit.");
            }
            _ => panic!("Expected a generic StdError for insufficient pending deposit."),
        }

        // --- Verification ---
        // 5. Verify that the user's state and global state were NOT changed.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            user_info.pending_deposit, initial_pending,
            "Pending deposit should not have changed on failed withdrawal"
        );

        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();
        assert_eq!(
            total_pending, initial_pending,
            "Total pending deposits should not have changed"
        );
    }

    #[test]
    fn test_withdraw_shares_proportional_lp() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up state.
        // A user has 100 shares. Another user has a pending deposit of 30 LP.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(100);
        let total_pending = Uint128::new(30);

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &total_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                    pending_deposit: Uint128::zero(), // This user has no pending funds
                },
            )
            .unwrap();
        // (We don't need to save the other user with a pending deposit for this test)

        // 3. Setup Mock Querier: This is the key part of the test.
        // Due to compounding, the 100 active shares have grown in value to be worth 120 LP tokens.
        let value_of_active_shares = Uint128::new(120);
        // The total amount in the farm is the value of active shares + all pending deposits.
        let total_lp_staked = value_of_active_shares + total_pending; // 120 + 30 = 150

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
        // 4. The user burns 50 of their 100 shares.
        let shares_to_burn = Uint128::new(50);
        let msg = ExecuteMsg::WithdrawShares { shares_to_burn };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Calculate the expected LP to receive.
        // The share price is 120 LP / 100 shares = 1.2 LP/share.
        // Withdrawing 50 shares should yield 50 * 1.2 = 60 LP tokens.
        let expected_lp_to_receive =
            shares_to_burn.multiply_ratio(value_of_active_shares, initial_shares);
        assert_eq!(expected_lp_to_receive, Uint128::new(60));

        // 6. Verify state changes.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(user_info.shares, initial_shares - shares_to_burn); // 100 - 50 = 50
        assert_eq!(user_info.pending_deposit, Uint128::zero()); // Unchanged

        let total_shares = TOTAL_SHARES.load(&deps.storage).unwrap();
        assert_eq!(total_shares, initial_shares - shares_to_burn);

        // 7. Verify the correct LP amount was unbonded and transferred.
        assert_eq!(res.messages.len(), 2);
        if let CosmosMsg::Wasm(WasmMsg::Execute { msg, .. }) = &res.messages[1].msg {
            if let Ok(Cw20ExecuteMsg::Transfer { amount, .. }) = from_json(msg) {
                assert_eq!(amount, expected_lp_to_receive);
            } else {
                panic!("Incorrect Wasm message type for transfer");
            }
        } else {
            panic!("Expected Wasm message for transfer");
        }
    }

    #[test]
    fn test_withdraw_shares_does_not_affect_pending() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with both active shares and a pending deposit.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(100);
        let initial_pending = Uint128::new(50);

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &initial_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                    pending_deposit: initial_pending,
                },
            )
            .unwrap();

        // 3. Setup Mock Querier. The value of active shares is 120 LP.
        let value_of_active_shares = Uint128::new(120);
        let total_lp_staked = value_of_active_shares + initial_pending; // 120 + 50 = 170

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
        // 4. The user burns a portion of their active shares.
        let shares_to_burn = Uint128::new(25);
        let msg = ExecuteMsg::WithdrawShares { shares_to_burn };
        let info = message_info(&user1_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. Verify the user's state.
        let user_info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user1_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();

        // The user's share balance should be reduced.
        assert_eq!(user_info.shares, initial_shares - shares_to_burn); // 100 - 25 = 75

        // CRITICAL ASSERTION: The user's pending deposit must be completely unchanged.
        assert_eq!(
            user_info.pending_deposit, initial_pending,
            "Pending deposit should not be affected by withdrawing shares"
        );

        // 6. Verify global state.
        let total_shares = TOTAL_SHARES.load(&deps.storage).unwrap();
        let total_pending = TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap();

        assert_eq!(
            total_shares,
            initial_shares - shares_to_burn,
            "Total shares should be reduced"
        );

        // CRITICAL ASSERTION: Total pending deposits must be unchanged.
        assert_eq!(
            total_pending, initial_pending,
            "Total pending deposits should not be affected"
        );
    }

    #[test]
    fn test_withdraw_shares_full_withdrawal_removes_user() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with active shares but NO pending deposit.
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
                    pending_deposit: Uint128::zero(), // No pending deposit
                },
            )
            .unwrap();

        // 3. Setup Mock Querier. Assume 1:1 share value for simplicity.
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: initial_shares,
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user burns ALL of their active shares.
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: initial_shares,
        };
        let info = message_info(&user1_addr, &[]);
        execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // --- Assert ---
        // 5. CRITICAL ASSERTION: The user's record should be completely gone from the USERS map.
        let user_info_raw = USERS.may_load(&deps.storage, &user1_addr).unwrap();
        assert!(
            user_info_raw.is_none(),
            "User info should be removed from storage after withdrawing all assets"
        );

        // 6. Verify global state is updated.
        let total_shares = TOTAL_SHARES.load(&deps.storage).unwrap();
        assert_eq!(total_shares, Uint128::zero());
    }

    #[test]
    fn test_withdraw_shares_error_if_farm_value_less_than_pending() {
        // --- Arrange ---
        // 1. Setup and instantiate the contract.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner");
        let farm_contract_addr = deps.api.addr_make("farm0000");
        let lp_token_addr = deps.api.addr_make("lp_token0000");
        let creator_addr = deps.api.addr_make("creator");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair0000").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token_addr.to_string(),
            },
            // ... other fields are default
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        // 2. Set up a user with some active shares.
        let user1_addr = deps.api.addr_make("user1");
        let initial_shares = Uint128::new(100);
        let total_pending = Uint128::new(500); // There is a large amount of pending deposits.

        TOTAL_SHARES
            .save(&mut deps.storage, &initial_shares)
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &total_pending)
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user1_addr,
                &UserInfo {
                    shares: initial_shares,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();

        // 3. CRITICAL: Mock the farm querier to report a total value (bond_amount)
        // that is LESS than the total pending deposits. This simulates a loss of funds
        // or a major state inconsistency.
        let inconsistent_farm_balance = Uint128::new(400);
        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: "any".to_string(),
                reward_index: Decimal::one(),
                bond_amount: inconsistent_farm_balance, // 400 is less than the 500 pending
                pending_reward: Uint128::zero(),
            },
        );

        // --- Act ---
        // 4. The user with active shares attempts to withdraw. The contract will try to calculate
        // `lp_value_of_all_shares = 400 - 500`, which should trigger an overflow error.
        let msg = ExecuteMsg::WithdrawShares {
            shares_to_burn: Uint128::new(10),
        };
        let info = message_info(&user1_addr, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg);

        // --- Assert ---
        // 5. The execution must fail with a StdError::Overflow, not a panic.
        // This proves the `checked_sub` is working as a safety rail.
        assert!(matches!(
            res,
            Err(ContractError::Std(StdError::Overflow { .. }))
        ));
    }

    /// Spins up a single-user native-LP/native-reward vault ready for withdraw-shares tests.
    /// Returns deps, the farm addr, user addr, user's shares, and the total bonded LP.
    fn setup_vault_for_withdraw_shares(
        pending_reward: Uint128,
    ) -> (
        cosmwasm_std::OwnedDeps<
            cosmwasm_std::testing::MockStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
        Uint128,
        Uint128,
    ) {
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner_h1");
        let farm_contract_addr = deps.api.addr_make("farm_h1");
        let user_addr = deps.api.addr_make("exiter_h1");
        let creator_addr = deps.api.addr_make("creator_h1");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_h1").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_denom".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        let user_shares = Uint128::new(100);
        let total_bonded = Uint128::new(100);
        TOTAL_SHARES.save(&mut deps.storage, &user_shares).unwrap();
        USERS
            .save(
                &mut deps.storage,
                &user_addr,
                &UserInfo {
                    shares: user_shares,
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: total_bonded,
                pending_reward,
            },
        );

        (deps, farm_contract_addr, user_addr, user_shares, total_bonded)
    }

    #[test]
    fn test_withdraw_shares_emits_harvest_submsg_when_rewards_pending() {
        // H-1/H-5 regression: when the farm has unharvested rewards, withdraw must route
        // through the farm Withdraw reply chain instead of directly unbonding — otherwise the
        // exiter forfeits their slice of the pending reward.
        let (mut deps, farm_addr, user_addr, shares, _) =
            setup_vault_for_withdraw_shares(Uint128::new(999));

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user_addr, &[]),
            ExecuteMsg::WithdrawShares {
                shares_to_burn: shares,
            },
        )
        .unwrap();

        // Exactly one submessage: farm.Withdraw. The unbond + transfer happen in the reply.
        assert_eq!(res.messages.len(), 1);
        let sub = &res.messages[0];
        assert_eq!(sub.id, crate::contract::WITHDRAW_SHARES_REPLY_ID);
        match &sub.msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                assert_eq!(contract_addr, &farm_addr.to_string());
                let decoded: FarmExecuteMsg = from_json(msg).unwrap();
                assert!(matches!(decoded, FarmExecuteMsg::Withdraw {}));
            }
            other => panic!("unexpected submsg: {:?}", other),
        }

        // User bookkeeping already updated — the reply only emits transfers.
        let info: UserInfoResponse = from_json(
            query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::UserInfo {
                    user: user_addr.to_string(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(info.shares, Uint128::zero());
    }

    #[test]
    #[allow(deprecated)] // SubMsgResponse::data — still required for cw 2.x compat
    fn test_withdraw_shares_reply_sends_proportional_reward() {
        // Sole shareholder exits. Reply sees the full reward_token balance and must forward
        // all of it to the exiter (100% of shares → 100% of harvested rewards).
        let (mut deps, farm_addr, user_addr, shares, bonded) =
            setup_vault_for_withdraw_shares(Uint128::new(999));

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user_addr, &[]),
            ExecuteMsg::WithdrawShares {
                shares_to_burn: shares,
            },
        )
        .unwrap();
        let payload = res.messages[0].payload.clone();

        // Simulate farm handing the reward tokens to the vault. The reward_token in
        // `setup_vault_for_withdraw_shares` is "token_a" (must be a pair asset per H-2).
        let harvested = Uint128::new(999);
        let env = mock_env();
        deps.querier.with_balance(&[(
            env.contract.address.to_string(),
            &[cosmwasm_std::Coin {
                denom: "token_a".to_string(),
                amount: harvested,
            }],
        )]);

        let reply_msg = Reply {
            id: crate::contract::WITHDRAW_SHARES_REPLY_ID,
            payload,
            gas_used: 0,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
        };

        let res = reply(deps.as_mut(), env.clone(), reply_msg).unwrap();

        // Expect: unbond, LP transfer, reward transfer.
        assert_eq!(res.messages.len(), 3);

        let unbond = &res.messages[0].msg;
        match unbond {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                assert_eq!(contract_addr, &farm_addr.to_string());
                let decoded: FarmExecuteMsg = from_json(msg).unwrap();
                assert!(matches!(
                    decoded,
                    FarmExecuteMsg::Unbond { amount } if amount == bonded
                ));
            }
            other => panic!("expected unbond, got {:?}", other),
        }

        let lp_transfer = &res.messages[1].msg;
        match lp_transfer {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, &user_addr.to_string());
                assert_eq!(amount.len(), 1);
                assert_eq!(amount[0].denom, "lp_denom");
                assert_eq!(amount[0].amount, bonded);
            }
            other => panic!("expected LP bank send, got {:?}", other),
        }

        let reward_transfer = &res.messages[2].msg;
        match reward_transfer {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, &user_addr.to_string());
                assert_eq!(amount.len(), 1);
                assert_eq!(amount[0].denom, "token_a");
                assert_eq!(amount[0].amount, harvested);
            }
            other => panic!("expected reward bank send, got {:?}", other),
        }
    }

    #[test]
    #[allow(deprecated)] // SubMsgResponse::data — still required for cw 2.x compat
    fn test_withdraw_shares_reply_proportional_split_with_multiple_holders() {
        // Two shareholders of equal stake. Exiter burns half the shares, must receive exactly
        // half of whatever reward balance is in the vault when the reply fires.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner_h1b");
        let farm_contract_addr = deps.api.addr_make("farm_h1b");
        let exiter_addr = deps.api.addr_make("exiter_h1b");
        let stayer_addr = deps.api.addr_make("stayer_h1b");
        let creator_addr = deps.api.addr_make("creator_h1b");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_h1b").to_string(),
            farm_contract: farm_contract_addr.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_denom".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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

        let shares_each = Uint128::new(100);
        TOTAL_SHARES
            .save(&mut deps.storage, &(shares_each + shares_each))
            .unwrap();
        for addr in [&exiter_addr, &stayer_addr] {
            USERS
                .save(
                    &mut deps.storage,
                    addr,
                    &UserInfo {
                        shares: shares_each,
                        pending_deposit: Uint128::zero(),
                    },
                )
                .unwrap();
        }

        deps.querier.with_staker_info(
            farm_contract_addr.to_string(),
            StakerInfoResponse {
                staker: owner_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(200),
                pending_reward: Uint128::new(500),
            },
        );

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&exiter_addr, &[]),
            ExecuteMsg::WithdrawShares {
                shares_to_burn: shares_each,
            },
        )
        .unwrap();
        let payload = res.messages[0].payload.clone();

        let env = mock_env();
        deps.querier.with_balance(&[(
            env.contract.address.to_string(),
            &[cosmwasm_std::Coin {
                denom: "token_a".to_string(),
                amount: Uint128::new(500),
            }],
        )]);

        let reply_msg = Reply {
            id: crate::contract::WITHDRAW_SHARES_REPLY_ID,
            payload,
            gas_used: 0,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None,
                msg_responses: vec![],
            }),
        };
        let res = reply(deps.as_mut(), env, reply_msg).unwrap();

        let reward_transfer = &res.messages[2].msg;
        match reward_transfer {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, &exiter_addr.to_string());
                // Exiter burns 100 of 200 pre-burn shares → 500 * 100 / 200 = 250.
                assert_eq!(amount[0].amount, Uint128::new(250));
            }
            other => panic!("expected reward bank send, got {:?}", other),
        }
    }

    #[test]
    fn test_withdraw_shares_fast_path_when_no_pending_reward() {
        // Regression guard: when pending_reward is zero the vault should still emit the old
        // direct unbond + transfer (no reply chain), keeping the happy path simple.
        let (mut deps, farm_addr, user_addr, shares, bonded) =
            setup_vault_for_withdraw_shares(Uint128::zero());

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user_addr, &[]),
            ExecuteMsg::WithdrawShares {
                shares_to_burn: shares,
            },
        )
        .unwrap();

        // Two direct messages, no submessages keyed on WITHDRAW_SHARES_REPLY_ID.
        assert_eq!(res.messages.len(), 2);
        assert!(res
            .messages
            .iter()
            .all(|m| m.id != crate::contract::WITHDRAW_SHARES_REPLY_ID));

        let unbond = &res.messages[0].msg;
        match unbond {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                assert_eq!(contract_addr, &farm_addr.to_string());
                let decoded: FarmExecuteMsg = from_json(msg).unwrap();
                assert!(matches!(
                    decoded,
                    FarmExecuteMsg::Unbond { amount } if amount == bonded
                ));
            }
            other => panic!("expected unbond, got {:?}", other),
        }
    }

    #[test]
    fn test_instantiate_rejects_route_that_does_not_end_on_pair_asset() {
        // H-2 regression: a route that terminates on a non-pair asset would leave the final
        // 50/50 swap trying to offer a token the configured pair_contract doesn't trade,
        // stranding every compound. Instantiate must reject at deploy time.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner_h2");
        let orphan_denom = "orphan";

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_h2").to_string(),
            farm_contract: deps.api.addr_make("farm_h2").to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_denom".to_string(),
            },
            // reward is one of the pair assets, which alone would be fine — but the route
            // redirects to an orphan terminal asset. That must still be rejected.
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            reward_to_lp_token_route: vec![crate::msg::SwapHop {
                pair_contract: deps.api.addr_make("hop_pair_h2").to_string(),
                to_asset_info: AssetInfo::NativeToken {
                    denom: orphan_denom.to_string(),
                },
            }],
        };

        let creator = deps.api.addr_make("creator_h2");
        let err = instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::CompoundPathMustEndOnPairAsset {}));
    }

    #[test]
    fn test_instantiate_rejects_empty_route_when_reward_is_not_pair_asset() {
        // H-2 regression: empty route + reward_token that isn't one of the pair's assets is
        // the same silent-burn config, just with no intermediate hops.
        let mut deps = mock_dependencies();
        let owner_addr = deps.api.addr_make("owner_h2b");

        let instantiate_msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_h2b").to_string(),
            farm_contract: deps.api.addr_make("farm_h2b").to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_denom".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "orphan".to_string(),
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

        let creator = deps.api.addr_make("creator_h2b");
        let err = instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            instantiate_msg,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::CompoundPathMustEndOnPairAsset {}));
    }

    // -------------------------------------------------------------------------
    // Batch 1 — instantiate & config hygiene (M-5, M-6, M-7)
    // -------------------------------------------------------------------------

    fn make_valid_instantiate_msg(deps: &cosmwasm_std::OwnedDeps<
        cosmwasm_std::MemoryStorage,
        cosmwasm_std::testing::MockApi,
        crate::mock_querier::WasmMockQuerier,
    >) -> (InstantiateMsg, cosmwasm_std::Addr) {
        let owner_addr = deps.api.addr_make("owner_batch1");
        let msg = InstantiateMsg {
            owner: owner_addr.to_string(),
            pair_contract: deps.api.addr_make("pair_batch1").to_string(),
            farm_contract: deps.api.addr_make("farm_batch1").to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp_denom".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
        (msg, owner_addr)
    }

    #[test]
    fn test_instantiate_rejects_slippage_above_max() {
        // M-5: slippage_tolerance is the fallback MEV guard on ProvideLiquidity /
        // assert_max_spread. A too-permissive value silently disables those checks.
        let mut deps = mock_dependencies();
        let (mut msg, _) = make_valid_instantiate_msg(&deps);
        msg.slippage_tolerance = Decimal::percent(26); // just over the 25% cap

        let creator = deps.api.addr_make("creator");
        let err = instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap_err();
        match err {
            ContractError::SlippageToleranceAboveMax { got, max } => {
                assert_eq!(got, Decimal::percent(26));
                assert_eq!(max, crate::contract::DEFAULT_MAX_SLIPPAGE_TOLERANCE);
            }
            other => panic!("expected SlippageToleranceAboveMax, got {:?}", other),
        }
    }

    #[test]
    fn test_instantiate_accepts_slippage_at_max() {
        // Boundary: exactly equal to the cap must be accepted.
        let mut deps = mock_dependencies();
        let (mut msg, _) = make_valid_instantiate_msg(&deps);
        msg.slippage_tolerance = crate::contract::DEFAULT_MAX_SLIPPAGE_TOLERANCE;

        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();
    }

    #[test]
    fn test_update_config_rejects_slippage_above_max() {
        // M-5: the same cap must apply on update; otherwise the owner could relax the
        // guard post-deploy.
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::UpdateConfig {
                slippage_tolerance: Some(Decimal::percent(30)),
                fee_recipient: None,
                fee_percentage: None,
                minimum_reward_to_compound: None,
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ContractError::SlippageToleranceAboveMax { .. }
        ));
    }

    #[test]
    fn test_clear_fee_recipient_owner_only() {
        // M-7: the recipient can be set via UpdateConfig but had no way to be cleared
        // (short of re-deploying). ClearFeeRecipient is owner-only.
        let mut deps = mock_dependencies();
        let (mut msg, owner) = make_valid_instantiate_msg(&deps);
        let initial_recipient = deps.api.addr_make("initial_fee_recipient");
        msg.fee_recipient = Some(initial_recipient.to_string());
        msg.fee_percentage = Some(Decimal::percent(5));

        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();

        // Non-owner cannot clear.
        let rando = deps.api.addr_make("rando");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&rando, &[]),
            ExecuteMsg::ClearFeeRecipient,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));

        // Before: recipient still set.
        let cfg_bytes = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
        let cfg: Config = from_json(&cfg_bytes).unwrap();
        assert_eq!(cfg.fee_recipient.as_ref(), Some(&initial_recipient));

        // Owner clears.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ClearFeeRecipient,
        )
        .unwrap();

        // After: recipient is None, but fee_percentage is untouched (audit note:
        // percentage alone will not trigger a fee transfer because both must be Some).
        let cfg_bytes = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
        let cfg: Config = from_json(&cfg_bytes).unwrap();
        assert_eq!(cfg.fee_recipient, None);
        assert_eq!(cfg.fee_percentage, Some(Decimal::percent(5)));
    }

    #[test]
    fn test_fee_amount_uses_decimal_mul_floor() {
        // M-6: the previous fee calc hand-unrolled Decimal's internal 10^18 scale.
        // Verify mul_floor produces the expected amount at 5% of 1_000_000.
        let balance = Uint128::new(1_000_000);
        let pct = Decimal::percent(5);
        assert_eq!(balance.mul_floor(pct), Uint128::new(50_000));

        // And rounds down, not up, on non-exact fractions.
        let balance = Uint128::new(9_999);
        let pct = Decimal::percent(5);
        assert_eq!(balance.mul_floor(pct), Uint128::new(499));
    }

    // -------------------------------------------------------------------------
    // Batch 2 — deposit/withdraw hygiene (M-1, M-8, L-15)
    // -------------------------------------------------------------------------

    fn setup_native_lp_vault(
        native_lp_denom: &str,
    ) -> (
        cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
    ) {
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_batch2");
        let farm = deps.api.addr_make("farm_batch2");
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: deps.api.addr_make("pair_batch2").to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_batch2");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();
        (deps, owner, farm)
    }

    #[test]
    fn test_deposit_native_rejects_extra_coins() {
        // M-1: previously the deposit path found the LP denom and silently retained any other
        // coins sent alongside. Exactly one coin is now required.
        let native_lp_denom = "factory/pair_batch2/lp";
        let (mut deps, _, _) = setup_native_lp_vault(native_lp_denom);
        let user = deps.api.addr_make("user_extra");

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(
                &user,
                &[
                    cosmwasm_std::coin(100, native_lp_denom),
                    cosmwasm_std::coin(50, "uinj"),
                ],
            ),
            ExecuteMsg::Deposit {},
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(
                    msg.contains("expected exactly one coin"),
                    "unexpected error: {}",
                    msg
                );
            }
            other => panic!("expected GenericErr, got {:?}", other),
        }
    }

    #[test]
    fn test_deposit_native_rejects_wrong_denom() {
        // M-1: the single coin must match the configured LP denom.
        let native_lp_denom = "factory/pair_batch2/lp";
        let (mut deps, _, _) = setup_native_lp_vault(native_lp_denom);
        let user = deps.api.addr_make("user_wrong");

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[cosmwasm_std::coin(100, "uinj")]),
            ExecuteMsg::Deposit {},
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(msg.contains("expected"), "unexpected error: {}", msg);
            }
            other => panic!("expected GenericErr, got {:?}", other),
        }
    }

    #[test]
    fn test_deposit_native_accepts_exact_single_coin() {
        // M-1 positive case: the happy path still works.
        let native_lp_denom = "factory/pair_batch2/lp";
        let (mut deps, _, farm) = setup_native_lp_vault(native_lp_denom);
        let user = deps.api.addr_make("user_ok");

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[cosmwasm_std::coin(100, native_lp_denom)]),
            ExecuteMsg::Deposit {},
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);
        match &res.messages[0].msg {
            CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, .. }) => {
                assert_eq!(contract_addr, &farm.to_string());
            }
            other => panic!("expected Wasm Execute to farm, got {:?}", other),
        }
    }

    #[test]
    fn test_withdraw_shares_errors_on_total_shares_invariant() {
        // M-8: previously a dead branch returned zero when total_shares was zero while the
        // user held shares, which surfaced as a misleading "too small" error. Now it
        // rejects with an explicit invariant message.
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_m8");
        let farm = deps.api.addr_make("farm_m8");
        let lp_token = deps.api.addr_make("lp_m8");
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: deps.api.addr_make("pair_m8").to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_m8");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();

        // Corrupt state to the impossible shape the dead branch guarded against:
        // user has shares, but total_shares is zero.
        let user = deps.api.addr_make("user_m8");
        USERS
            .save(
                &mut deps.storage,
                &user,
                &UserInfo {
                    shares: Uint128::new(10),
                    pending_deposit: Uint128::zero(),
                },
            )
            .unwrap();
        TOTAL_SHARES
            .save(&mut deps.storage, &Uint128::zero())
            .unwrap();
        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: owner.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::zero(),
            },
        );

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::WithdrawShares {
                shares_to_burn: Uint128::new(1),
            },
        )
        .unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(
                    msg.contains("invariant violation"),
                    "unexpected error: {}",
                    msg
                );
            }
            other => panic!("expected invariant violation, got {:?}", other),
        }
    }

    #[test]
    fn test_activate_my_deposit_auto_refunds_dust() {
        // L-15: pending deposits that are too small to mint a share used to be left in limbo —
        // the user had to know about `WithdrawPending` to reclaim. Self-activation now
        // auto-refunds dust instead.
        let native_lp_denom = "factory/pair_batch2/lp";
        let (mut deps, _, farm) = setup_native_lp_vault(native_lp_denom);
        let user = deps.api.addr_make("dust_user");

        // Setup: existing large share pool so 1 LP rounds to 0 shares.
        USERS
            .save(
                &mut deps.storage,
                &user,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(1),
                },
            )
            .unwrap();
        TOTAL_SHARES
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(1))
            .unwrap();
        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: "ignored".to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(10_001), // lp_value_of_all_shares = 10_000
                pending_reward: Uint128::zero(),
            },
        );

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::ActivateMyDeposit {},
        )
        .unwrap();

        // Expect the two-message unbond + BankSend refund that send_withdrawal_messages emits
        // for native LP.
        assert_eq!(res.messages.len(), 2);
        match &res.messages[0].msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                assert_eq!(contract_addr, &farm.to_string());
                let decoded: FarmExecuteMsg = from_json(msg).unwrap();
                assert!(matches!(
                    decoded,
                    FarmExecuteMsg::Unbond { amount } if amount == Uint128::new(1)
                ));
            }
            other => panic!("expected farm Unbond, got {:?}", other),
        }
        match &res.messages[1].msg {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(to_address, &user.to_string());
                assert_eq!(amount.len(), 1);
                assert_eq!(amount[0].denom, native_lp_denom);
                assert_eq!(amount[0].amount, Uint128::new(1));
            }
            other => panic!("expected BankMsg::Send refund, got {:?}", other),
        }

        // State assertions.
        let user_after: Option<UserInfo> = USERS.may_load(&deps.storage, &user).unwrap();
        assert!(user_after.is_none(), "user record should be purged");
        assert_eq!(
            TOTAL_PENDING_DEPOSITS.load(&deps.storage).unwrap(),
            Uint128::zero()
        );
        // total_shares unchanged — no shares were minted.
        assert_eq!(TOTAL_SHARES.load(&deps.storage).unwrap(), Uint128::new(100));
    }

    // -------------------------------------------------------------------------
    // Batch 3 — compound flow & MEV (C-3, H-3, M-2, M-3)
    // -------------------------------------------------------------------------

    #[test]
    fn test_compound_permissionless_nonzero_min_lp_from_random_caller() {
        // C-3: anyone can call Compound as long as they commit to a non-zero min_lp.
        // A previous test asserted Unauthorized for a random caller — we now assert
        // absence of that error regardless of downstream behavior.
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_c3perm");
        let farm = deps.api.addr_make("farm_c3perm");
        let pair = deps.api.addr_make("pair_c3perm");
        let vault_addr = mock_env().contract.address;
        let random = deps.api.addr_make("random_caller_c3perm");

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();
        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1000),
                pending_reward: Uint128::new(100),
            },
        );
        // B-6: heuristic needs the pair's pool state.
        deps.querier.with_pool(
            pair.to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ),
        );

        // Non-compounder caller + min_lp above the B-6 heuristic floor — must emit the
        // harvest submsg (never Unauthorized). Floor at pending_reward=100 against
        // big_pool is ~4; pass 5 to clear it without entangling this C-3 test with
        // heuristic-boundary behaviour.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(5),
            },
        )
        .unwrap();
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);

        // With zero min_lp, explicit reject.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::zero(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MinimumLpToReceiveZero {}));
    }

    #[test]
    fn test_optimal_zap_amount_matches_fee_derivation() {
        // H-3: spot-check the closed-form zap against the derivation. For (Rx=10_000, A=30)
        // with fee=0.3%, the optimal swap is ~15 (very slightly over A/2 due to fee).
        let s = crate::contract::optimal_zap_amount_xyk(
            Uint128::new(30),
            Uint128::new(10_000),
        )
        .unwrap();
        // The closed-form yields 15 for these parameters (naive 50/50 would also be 15, but
        // this test-pins the formula against a known-good manual computation).
        assert_eq!(s, Uint128::new(15));

        // For a smaller pool (A closer to Rx), fee impact is more visible and s should be
        // well below A/2. For (Rx=1_000, A=1_000) the naive 50/50 is 500; the optimal is less.
        let s = crate::contract::optimal_zap_amount_xyk(
            Uint128::new(1_000),
            Uint128::new(1_000),
        )
        .unwrap();
        assert!(
            s < Uint128::new(500),
            "for A=Rx the optimal zap is strictly below A/2; got {}",
            s
        );
        assert!(s > Uint128::new(400), "but not absurdly below; got {}", s);
    }

    #[test]
    fn test_optimal_zap_amount_zero_amount() {
        let s = crate::contract::optimal_zap_amount_xyk(
            Uint128::zero(),
            Uint128::new(10_000),
        )
        .unwrap();
        assert_eq!(s, Uint128::zero());
    }

    #[test]
    fn test_optimal_zap_amount_zero_reserve_errors() {
        let err = crate::contract::optimal_zap_amount_xyk(
            Uint128::new(100),
            Uint128::zero(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("zero offer-side reserve"));
    }

    #[test]
    #[allow(deprecated)] // SubMsgResponse::data — still required for cw 2.x compat
    fn test_m2_payload_carries_actual_post_harvest_balance() {
        // M-2: after harvest_reply runs, the payload threaded to the final swap must carry
        // the actual queried reward balance, not the pre-withdraw prediction from
        // staker_info.pending_reward. We assert this by observing the CompoundingInfo
        // written on the provide_liquidity reply.
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_m2");
        let farm = deps.api.addr_make("farm_m2");
        let pair = deps.api.addr_make("pair_m2");
        let lp_token = deps.api.addr_make("lp_m2");
        let vault_addr = deps.api.addr_make("vault_m2");

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::Token {
                contract_addr: lp_token.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();

        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(500),
                pending_reward: Uint128::new(1000), // Pre-execute prediction
            },
        );
        deps.querier.with_pool(
            pair.to_string(),
            big_pool_response(
                AssetInfo::NativeToken {
                    denom: "token_a".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "token_b".to_string(),
                },
            ),
        );
        // Actual post-harvest balance is *higher* than the prediction (could reflect
        // leftover dust from a previous cycle too). M-2 says we record the actual.
        let actual_balance = Uint128::new(1_200);
        deps.querier.with_balance(&[(
            vault_addr.to_string(),
            &[cosmwasm_std::Coin {
                denom: "token_a".to_string(),
                amount: actual_balance,
            }],
        )]);
        // Needed for the provide_liquidity_reply path.
        deps.querier.with_token_balance(
            lp_token.as_ref(),
            vault_addr.as_ref(),
            Uint128::new(50),
        );

        let mut env = mock_env();
        env.contract.address = vault_addr.clone();

        let harvest_payload = HarvestReplyPayload {
            reward_amount_to_compound: Uint128::new(1000), // stale prediction
            tvl_before_compound: Uint128::new(500),
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
        };
        let harvest_reply = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                msg_responses: vec![],
                data: None,
            }),
            gas_used: 0,
            payload: to_json_binary(&harvest_payload).unwrap(),
        };
        let res = reply(deps.as_mut(), env.clone(), harvest_reply).unwrap();

        // The next payload (attached to FINAL_SWAP_REPLY_ID) should carry actual_balance.
        let final_submsg = &res.messages[0];
        let forwarded: HarvestReplyPayload = from_json(&final_submsg.payload).unwrap();
        assert_eq!(
            forwarded.reward_amount_to_compound, actual_balance,
            "M-2: payload should carry queried balance, not pre-execute prediction"
        );
    }

    #[test]
    fn test_m3_harvest_reply_on_always_propagates_error_with_attr() {
        // M-3: when farm.Withdraw fails, handle_harvest_reply still fires (ReplyOn::Always)
        // and returns a structured error. Observable behavior: an Err result whose message
        // mentions the compound step that failed.
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_m3");
        let farm = deps.api.addr_make("farm_m3");
        let pair = deps.api.addr_make("pair_m3");

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: "lp".to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();

        let payload = HarvestReplyPayload {
            reward_amount_to_compound: Uint128::new(100),
            tvl_before_compound: Uint128::new(1000),
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(1),
        };
        let err_reply = Reply {
            id: HARVEST_REPLY_ID,
            result: SubMsgResult::Err("simulated farm withdraw failure".to_string()),
            gas_used: 0,
            payload: to_json_binary(&payload).unwrap(),
        };
        let err = reply(deps.as_mut(), mock_env(), err_reply).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("step 1") && msg.contains("simulated farm withdraw failure"),
            "expected structured step-1 error, got: {}",
            msg
        );
    }

    #[test]
    fn test_activate_batch_skips_dust_and_reports_count() {
        // L-15: batch path does NOT auto-refund (gas-envelope risk with adversarial input) —
        // it skips dust entries but surfaces the count in the response attributes so keepers
        // can notify users.
        let native_lp_denom = "factory/pair_batch2/lp";
        let (mut deps, _, farm) = setup_native_lp_vault(native_lp_denom);
        let compounder = deps.api.addr_make("owner_batch2"); // owner == compounder in helper
        let dust_user = deps.api.addr_make("dust_user_batch");
        let normal_user = deps.api.addr_make("normal_user_batch");

        USERS
            .save(
                &mut deps.storage,
                &dust_user,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(1),
                },
            )
            .unwrap();
        USERS
            .save(
                &mut deps.storage,
                &normal_user,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(1_000),
                },
            )
            .unwrap();
        TOTAL_SHARES
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(1_001))
            .unwrap();
        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: "ignored".to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(11_001), // lp_value_of_all_shares = 10_000
                pending_reward: Uint128::zero(),
            },
        );

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&compounder, &[]),
            ExecuteMsg::ActivatePendingDeposits {
                users: vec![dust_user.to_string(), normal_user.to_string()],
            },
        )
        .unwrap();

        let attrs: std::collections::HashMap<_, _> = res
            .attributes
            .iter()
            .map(|a| (a.key.clone(), a.value.clone()))
            .collect();
        assert_eq!(attrs.get("activated_user_count"), Some(&"1".to_string()));
        assert_eq!(attrs.get("skipped_dust_count"), Some(&"1".to_string()));

        // Dust user untouched; can still self-rescue.
        let dust_after: UserInfo = USERS.load(&deps.storage, &dust_user).unwrap();
        assert_eq!(dust_after.pending_deposit, Uint128::new(1));
        assert_eq!(dust_after.shares, Uint128::zero());

        // Normal user activated.
        let normal_after: UserInfo = USERS.load(&deps.storage, &normal_user).unwrap();
        assert_eq!(normal_after.pending_deposit, Uint128::zero());
        assert!(normal_after.shares > Uint128::zero());
    }

    // -------------------------------------------------------------------------
    // Batch 4 — operational safety (H-4 pause, L-11 migrate)
    // -------------------------------------------------------------------------

    /// Instantiates a vault with native LP, an active compounder, and returns the key addrs.
    fn setup_paused_vault_env(
        native_lp_denom: &str,
    ) -> (
        cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
    ) {
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_pause");
        let compounder = deps.api.addr_make("compounder_pause");
        let farm = deps.api.addr_make("farm_pause");
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: deps.api.addr_make("pair_pause").to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken {
                denom: native_lp_denom.to_string(),
            },
            reward_token: AssetInfo::NativeToken {
                denom: "token_a".to_string(),
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
            compounder: compounder.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_pause");
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            msg,
        )
        .unwrap();
        (deps, owner, compounder)
    }

    fn pause(
        deps: &mut cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        owner: &cosmwasm_std::Addr,
    ) {
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(owner, &[]),
            ExecuteMsg::Pause,
        )
        .unwrap();
    }

    #[test]
    fn test_pause_unpause_owner_only() {
        // H-4: only the owner can pause/unpause. Non-owner callers must be rejected.
        let native_lp_denom = "factory/pair_pause/lp";
        let (mut deps, owner, _) = setup_paused_vault_env(native_lp_denom);
        let rando = deps.api.addr_make("rando_pause");

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&rando, &[]),
            ExecuteMsg::Pause,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Pause,
        )
        .unwrap();
        let cfg_bytes = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
        let cfg: Config = from_json(&cfg_bytes).unwrap();
        assert!(cfg.paused);

        // Unpause — same authorization requirement.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&rando, &[]),
            ExecuteMsg::Unpause,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Unpause,
        )
        .unwrap();
        let cfg: Config =
            from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert!(!cfg.paused);
    }

    #[test]
    fn test_pause_blocks_deposit() {
        // H-4: native deposit must reject when paused.
        let native_lp_denom = "factory/pair_pause/lp";
        let (mut deps, owner, _) = setup_paused_vault_env(native_lp_denom);
        let user = deps.api.addr_make("depositor_paused");
        pause(&mut deps, &owner);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[cosmwasm_std::coin(100, native_lp_denom)]),
            ExecuteMsg::Deposit {},
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::VaultPaused {}));
    }

    #[test]
    fn test_pause_blocks_compound() {
        // H-4: Compound rejects when paused, even with correct bounds.
        let (mut deps, owner, _) = setup_paused_vault_env("factory/pair_pause/lp");
        pause(&mut deps, &owner);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::VaultPaused {}));
    }

    #[test]
    fn test_pause_blocks_activate_paths() {
        // H-4: both batch and self-activation must reject when paused.
        let (mut deps, owner, compounder) = setup_paused_vault_env("factory/pair_pause/lp");
        let user = deps.api.addr_make("user_activate_paused");
        USERS
            .save(
                &mut deps.storage,
                &user,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(100),
                },
            )
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        pause(&mut deps, &owner);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&compounder, &[]),
            ExecuteMsg::ActivatePendingDeposits {
                users: vec![user.to_string()],
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::VaultPaused {}));

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::ActivateMyDeposit {},
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::VaultPaused {}));
    }

    #[test]
    fn test_pause_leaves_withdraw_paths_open() {
        // H-4: the whole point of the pause mechanism is that users can always unwind.
        // WithdrawPending for a user with pending_deposit must succeed even when paused.
        let native_lp_denom = "factory/pair_pause/lp";
        let (mut deps, owner, _) = setup_paused_vault_env(native_lp_denom);
        let user = deps.api.addr_make("user_exit_paused");
        USERS
            .save(
                &mut deps.storage,
                &user,
                &UserInfo {
                    shares: Uint128::zero(),
                    pending_deposit: Uint128::new(100),
                },
            )
            .unwrap();
        TOTAL_PENDING_DEPOSITS
            .save(&mut deps.storage, &Uint128::new(100))
            .unwrap();
        pause(&mut deps, &owner);

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::WithdrawPending { amount: None },
        )
        .unwrap();
        // Unbond + transfer LP back — the expected two-message exit.
        assert_eq!(res.messages.len(), 2);
    }

    #[test]
    fn test_migrate_rejects_wrong_contract_name() {
        // L-11: the migrate entry point guards against being invoked against a binary
        // deployed under a different contract name (defense against wiring accidents).
        let native_lp_denom = "factory/pair_migrate/lp";
        let (mut deps, _, _) = setup_paused_vault_env(native_lp_denom);
        // Corrupt the stored contract name, then verify migrate rejects.
        cw2::set_contract_version(&mut deps.storage, "crates.io:something-else", "9.9.9").unwrap();
        let err = migrate(deps.as_mut(), mock_env(), MigrateMsg {}).unwrap_err();
        match err {
            ContractError::Std(StdError::GenericErr { msg, .. }) => {
                assert!(msg.contains("cannot migrate"));
            }
            other => panic!("expected GenericErr, got {:?}", other),
        }
    }

    #[test]
    fn test_migrate_bumps_version_when_name_matches() {
        // L-11: same-contract migration succeeds and bumps the stored version string.
        let (mut deps, _, _) = setup_paused_vault_env("factory/pair_migrate/lp");
        // Stomp the stored version to something old.
        cw2::set_contract_version(&mut deps.storage, "crates.io:choice-vault", "0.0.1").unwrap();

        let res = migrate(deps.as_mut(), mock_env(), MigrateMsg {}).unwrap();
        let attrs: std::collections::HashMap<_, _> = res
            .attributes
            .iter()
            .map(|a| (a.key.clone(), a.value.clone()))
            .collect();
        assert_eq!(attrs.get("action"), Some(&"migrate".to_string()));
        assert_eq!(attrs.get("from_version"), Some(&"0.0.1".to_string()));
        // The new version matches the crate's Cargo.toml version; we don't pin it in-test
        // (would break on version bumps) — just assert it's present and different.
        assert!(attrs.contains_key("to_version"));
        assert_ne!(
            attrs.get("to_version"),
            Some(&"0.0.1".to_string()),
            "migrate should have advanced the version"
        );
    }

    // --- B-6: minimum_lp_to_receive heuristic ----------------------------------

    /// Builds a vault instance with a tight pool (small reserves + total_share) so
    /// the B-6 heuristic computes a non-zero floor even for modest rewards. Returns
    /// (deps, owner, pair, farm, vault_addr) wired up and ready to fire a Compound.
    ///
    /// Pool setup: both reserves = 10_000, total_share = 10_000, same-decimals.
    /// pending_reward = 100 → optimal_zap s = 49 → expected_lp = 50 (10_000 * 51 / 10_049).
    /// At k = 10%, floor = 5. Tests below assume that scale.
    fn setup_heuristic_vault() -> (
        cosmwasm_std::OwnedDeps<
            cosmwasm_std::MemoryStorage,
            cosmwasm_std::testing::MockApi,
            crate::mock_querier::WasmMockQuerier,
        >,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
        cosmwasm_std::Addr,
    ) {
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_b6");
        let pair = deps.api.addr_make("pair_b6");
        let farm = deps.api.addr_make("farm_b6");
        let vault_addr = mock_env().contract.address;

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken { denom: "lp".to_string() },
            reward_token: AssetInfo::NativeToken { denom: "token_a".to_string() },
            asset_infos: [
                AssetInfo::NativeToken { denom: "token_a".to_string() },
                AssetInfo::NativeToken { denom: "token_b".to_string() },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_b6");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(10_000),
                pending_reward: Uint128::new(100),
            },
        );
        // Tight pool: reserves == total_share == 10_000. See helper doc for numbers.
        deps.querier.with_pool(
            pair.to_string(),
            PoolResponse {
                assets: [
                    Asset {
                        info: AssetInfo::NativeToken { denom: "token_a".to_string() },
                        amount: Uint128::new(10_000),
                    },
                    Asset {
                        info: AssetInfo::NativeToken { denom: "token_b".to_string() },
                        amount: Uint128::new(10_000),
                    },
                ],
                total_share: Uint128::new(10_000),
            },
        );

        (deps, owner, pair, farm, vault_addr)
    }

    #[test]
    fn test_compound_heuristic_rejects_min_lp_below_floor() {
        // B-6 negative case. Floor at the test scale is ~4 (= 10% of ~49 expected LP).
        // min_lp = 1 is below that floor and must be rejected before the harvest fires.
        let (mut deps, owner, _pair, _farm, _vault) = setup_heuristic_vault();

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap_err();
        match err {
            ContractError::MinimumLpBelowHeuristic { minimum, floor } => {
                assert_eq!(minimum, Uint128::new(1));
                assert!(!floor.is_zero(), "floor must be non-zero for this pool scale");
                assert!(minimum < floor);
            }
            other => panic!("expected MinimumLpBelowHeuristic, got {:?}", other),
        }
    }

    #[test]
    fn test_compound_heuristic_accepts_realistic_min_lp() {
        // B-6 positive case. Setting min_lp to ~80% of expected (≈40 for a ~49-LP
        // expected mint) clears the 10% floor with plenty of headroom — the happy
        // path proceeds through harvest.
        let (mut deps, owner, _pair, _farm, _vault) = setup_heuristic_vault();

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(40),
            },
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);
    }

    #[test]
    fn test_compound_heuristic_accepts_min_lp_at_floor() {
        // Boundary: min_lp exactly equal to the computed floor must pass (the check is
        // strictly less-than). Concrete floor is 5 for the helper's pool + reward scale.
        let (mut deps, owner, _pair, _farm, _vault) = setup_heuristic_vault();

        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(5),
            },
        )
        .unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);
    }

    #[test]
    fn test_compound_heuristic_skips_when_expected_lp_rounds_to_zero() {
        // When the reward is dust against huge reserves (big_pool = 10^12 reserves),
        // expected_lp times k = 10% floors to zero and the heuristic fails open.
        // This keeps the heuristic from false-positive-blocking honest keepers of
        // huge pools with tiny reward tails.
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_b6_dust");
        let pair = deps.api.addr_make("pair_b6_dust");
        let farm = deps.api.addr_make("farm_b6_dust");
        let vault_addr = mock_env().contract.address;

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken { denom: "lp".to_string() },
            reward_token: AssetInfo::NativeToken { denom: "token_a".to_string() },
            asset_infos: [
                AssetInfo::NativeToken { denom: "token_a".to_string() },
                AssetInfo::NativeToken { denom: "token_b".to_string() },
            ],
            fee_recipient: None,
            fee_percentage: None,
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_b6_dust");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(1_000_000_000_000),
                pending_reward: Uint128::new(20),
            },
        );
        deps.querier.with_pool(
            pair.to_string(),
            big_pool_response(
                AssetInfo::NativeToken { denom: "token_a".to_string() },
                AssetInfo::NativeToken { denom: "token_b".to_string() },
            ),
        );

        // min_lp = 1 is usually rejected by the heuristic, but expected_lp * 10% rounds
        // to zero here so the floor short-circuits and min_lp = 1 (non-zero) is admitted.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap();
        assert_eq!(res.messages[0].id, HARVEST_REPLY_ID);
    }

    #[test]
    fn test_compound_heuristic_accounts_for_compound_fee() {
        // With a 50% compound fee, only half the reward goes to LP — the heuristic's
        // `reward_after_fee` must reflect this. Configure a tight pool and a 50% fee,
        // then verify the floor is roughly half of the no-fee floor. Using the same
        // pool scale as setup_heuristic_vault() but with fee = 50%: expected_lp ≈ 24,
        // floor ≈ 2 (down from ~4 without fee).
        let mut deps = mock_dependencies();
        let owner = deps.api.addr_make("owner_b6_fee");
        let pair = deps.api.addr_make("pair_b6_fee");
        let farm = deps.api.addr_make("farm_b6_fee");
        let fee_recipient = deps.api.addr_make("fees_b6");
        let vault_addr = mock_env().contract.address;

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            pair_contract: pair.to_string(),
            farm_contract: farm.to_string(),
            lp_token: AssetInfo::NativeToken { denom: "lp".to_string() },
            reward_token: AssetInfo::NativeToken { denom: "token_a".to_string() },
            asset_infos: [
                AssetInfo::NativeToken { denom: "token_a".to_string() },
                AssetInfo::NativeToken { denom: "token_b".to_string() },
            ],
            fee_recipient: Some(fee_recipient.to_string()),
            fee_percentage: Some(Decimal::percent(50)),
            minimum_reward_to_compound: Uint128::zero(),
            compounder: owner.to_string(),
            slippage_tolerance: Decimal::percent(1),
            reward_to_lp_token_route: vec![],
        };
        let creator = deps.api.addr_make("creator_b6_fee");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        deps.querier.with_staker_info(
            farm.to_string(),
            StakerInfoResponse {
                staker: vault_addr.to_string(),
                reward_index: Decimal::one(),
                bond_amount: Uint128::new(10_000),
                pending_reward: Uint128::new(100),
            },
        );
        deps.querier.with_pool(
            pair.to_string(),
            PoolResponse {
                assets: [
                    Asset {
                        info: AssetInfo::NativeToken { denom: "token_a".to_string() },
                        amount: Uint128::new(10_000),
                    },
                    Asset {
                        info: AssetInfo::NativeToken { denom: "token_b".to_string() },
                        amount: Uint128::new(10_000),
                    },
                ],
                total_share: Uint128::new(10_000),
            },
        );

        // With fee = 50%, reward_after_fee = 50. Heuristic floor drops below the
        // no-fee case; min_lp = 1 is still below floor, so still rejects — but the
        // floor itself must be lower than the no-fee version (5).
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
        )
        .unwrap_err();
        match err {
            ContractError::MinimumLpBelowHeuristic { floor, .. } => {
                // Concrete floor at this scale: 2 (vs no-fee 5).
                assert!(
                    floor < Uint128::new(5),
                    "fee-adjusted floor {} should be below the no-fee floor (5)",
                    floor
                );
            }
            other => panic!("expected MinimumLpBelowHeuristic, got {:?}", other),
        }
    }

    // --- MAX_SLIPPAGE_TOLERANCE timelocked raise -------------------------------

    #[test]
    fn test_tighten_max_slippage_applies_instantly() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        // Initially 25% (DEFAULT_MAX_SLIPPAGE_TOLERANCE). Tighten to 10%.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::TightenMaxSlippage { new_max: Decimal::percent(10) },
        )
        .unwrap();
        assert!(res
            .attributes
            .iter()
            .any(|a| a.key == "action" && a.value == "tighten_max_slippage"));

        let cfg: Config = from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(cfg.max_slippage_tolerance, Decimal::percent(10));
    }

    #[test]
    fn test_tighten_max_slippage_clamps_operational_slippage() {
        // Invariant: slippage_tolerance must always be <= max_slippage_tolerance. When
        // the owner tightens below the current operational value, clamp rather than
        // leave an inconsistent config that UpdateConfig would subsequently reject.
        let mut deps = mock_dependencies();
        let (mut msg, owner) = make_valid_instantiate_msg(&deps);
        msg.slippage_tolerance = Decimal::percent(20); // high but within the 25% default cap
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::TightenMaxSlippage { new_max: Decimal::percent(5) },
        )
        .unwrap();

        let cfg: Config = from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(cfg.max_slippage_tolerance, Decimal::percent(5));
        assert_eq!(cfg.slippage_tolerance, Decimal::percent(5));
    }

    #[test]
    fn test_tighten_max_slippage_rejects_raise_attempt() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        // Default cap is 25%. Attempting to "tighten" to 30% must reject.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::TightenMaxSlippage { new_max: Decimal::percent(30) },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MaxSlippageMustNotRaise { .. }));
    }

    #[test]
    fn test_tighten_max_slippage_rejects_non_owner() {
        let mut deps = mock_dependencies();
        let (msg, _owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        let stranger = deps.api.addr_make("stranger");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&stranger, &[]),
            ExecuteMsg::TightenMaxSlippage { new_max: Decimal::percent(10) },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::Unauthorized {}));
    }

    #[test]
    fn test_propose_max_slippage_raise_rejects_below_current() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        // Current cap = 25%. Proposing 25% or below rejects — ProposeRaise is strict >.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(25) },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MaxSlippageMustBeHigher { .. }));
    }

    #[test]
    fn test_propose_max_slippage_raise_rejects_above_ceiling() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        // Ceiling = 50%. Proposing 60% rejects even with the timelock.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(60) },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MaxSlippageAboveCeiling { .. }));
    }

    #[test]
    fn test_apply_max_slippage_raise_rejects_before_delay() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(40) },
        )
        .unwrap();

        // Apply without waiting — must fail.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ApplyMaxSlippageRaise,
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MaxSlippageRaiseNotReady {}));

        // Cap is unchanged.
        let cfg: Config = from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(cfg.max_slippage_tolerance, Decimal::percent(25));
    }

    #[test]
    fn test_apply_max_slippage_raise_succeeds_after_delay() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(40) },
        )
        .unwrap();

        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(
            crate::state::MAX_SLIPPAGE_RAISE_DELAY_SECONDS + 1,
        );
        execute(
            deps.as_mut(),
            env,
            message_info(&owner, &[]),
            ExecuteMsg::ApplyMaxSlippageRaise,
        )
        .unwrap();

        let cfg: Config = from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
        assert_eq!(cfg.max_slippage_tolerance, Decimal::percent(40));
        assert!(cfg.pending_max_slippage.is_none());
        assert!(cfg.pending_max_slippage_effective_at.is_none());
    }

    #[test]
    fn test_propose_max_slippage_raise_rejects_when_already_pending() {
        // Only one raise can be in flight. Second propose must cancel first.
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(40) },
        )
        .unwrap();

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(45) },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::MaxSlippageRaiseAlreadyPending {}));
    }

    #[test]
    fn test_cancel_max_slippage_proposal_clears_pending() {
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeMaxSlippageRaise { new_max: Decimal::percent(40) },
        )
        .unwrap();
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::CancelMaxSlippageProposal,
        )
        .unwrap();

        let pending: crate::msg::PendingMaxSlippageRaiseResponse = from_json(
            query(deps.as_ref(), mock_env(), QueryMsg::PendingMaxSlippageRaise {}).unwrap(),
        )
        .unwrap();
        assert!(pending.pending_max_slippage.is_none());
        assert!(pending.effective_at.is_none());
    }

    #[test]
    fn test_update_config_respects_live_max_slippage_cap() {
        // After a tighten, UpdateConfig must reject a slippage_tolerance above the new
        // (live) cap — not the original 25% constant. This guards against the owner
        // sidestepping their own tighten by immediately re-raising `slippage_tolerance`
        // via UpdateConfig.
        let mut deps = mock_dependencies();
        let (msg, owner) = make_valid_instantiate_msg(&deps);
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::TightenMaxSlippage { new_max: Decimal::percent(5) },
        )
        .unwrap();

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::UpdateConfig {
                slippage_tolerance: Some(Decimal::percent(10)),
                fee_recipient: None,
                fee_percentage: None,
                minimum_reward_to_compound: None,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContractError::SlippageToleranceAboveMax { .. }));
    }
}

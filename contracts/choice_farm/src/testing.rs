use crate::contract::{execute, instantiate, query};
use crate::mock_querier::{mock_dependencies, WasmMockQuerier};
use choice::asset::AssetInfo;
use choice::staking::ExecuteMsg::UpdateConfig;
use choice::staking::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg, StakerInfoResponse,
    StateResponse,
};
use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockStorage};
use cosmwasm_std::{
    attr, coins, from_json, to_json_binary, BankMsg, Coin, CosmosMsg, Decimal, OwnedDeps, StdError,
    SubMsg, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

type TestDeps = OwnedDeps<MockStorage, MockApi, WasmMockQuerier>;

/// Fund the farm's reward pool via a Cw20 Send hook from the configured reward
/// token "reward0000". Tests that use a CW20 reward token share this helper.
fn fund_via_reward_cw20(deps: &mut TestDeps, amount: Uint128) {
    let reward_addr = deps.api.addr_make("reward0000");
    let funder = deps.api.addr_make("funder").to_string();
    let info = message_info(&reward_addr, &[]);
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: funder,
        amount,
        msg: to_json_binary(&Cw20HookMsg::Fund {}).unwrap(),
    });
    execute(deps.as_mut(), mock_env(), info, msg).unwrap();
}

/// Fund the farm's reward pool with a native reward token (e.g. "inj").
fn fund_via_reward_native(deps: &mut TestDeps, denom: &str, amount: Uint128) {
    let funder = deps.api.addr_make("funder");
    let info = message_info(
        &funder,
        &[Coin {
            denom: denom.to_string(),
            amount,
        }],
    );
    execute(deps.as_mut(), mock_env(), info, ExecuteMsg::Fund {}).unwrap();
}

#[test]
fn proper_initialization() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![(100, 200, Uint128::from(1000000u128))],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // it worked, let's query the state
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config,
        ConfigResponse {
            reward_token: deps.api.addr_make("reward0000").to_string(),
            staking_token: deps.api.addr_make("staking0000").to_string(),
            distribution_schedule: vec![(100, 200, Uint128::from(1000000u128))],
        }
    );

    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::State { block_time: None },
    )
    .unwrap();
    let state: StateResponse = from_json(&res).unwrap();
    assert_eq!(
        state,
        StateResponse {
            last_distributed: mock_env().block.time.seconds(),
            total_bond_amount: Uint128::zero(),
            global_reward_index: Decimal::zero(),
            undistributed_rewards: Uint128::zero(),
        }
    );
}

#[test]
fn test_bond_tokens() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Fund the distribution pool so rewards can actually be distributed.
    fund_via_reward_cw20(&mut deps, Uint128::from(11_000_000u128));

    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });

    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    let _res = execute(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

    assert_eq!(
        from_json::<StakerInfoResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::StakerInfo {
                    staker: deps.api.addr_make("addr0000").to_string(),
                    block_time: None,
                },
            )
            .unwrap(),
        )
        .unwrap(),
        StakerInfoResponse {
            staker: deps.api.addr_make("addr0000").to_string(),
            reward_index: Decimal::zero(),
            pending_reward: Uint128::zero(),
            bond_amount: Uint128::from(100u128),
        }
    );

    assert_eq!(
        from_json::<StateResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::State { block_time: None }
            )
            .unwrap()
        )
        .unwrap(),
        StateResponse {
            total_bond_amount: Uint128::from(100u128),
            global_reward_index: Decimal::zero(),
            last_distributed: mock_env().block.time.seconds(),
            undistributed_rewards: Uint128::from(11_000_000u128),
        }
    );

    // bond 100 more tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    env.block.time = env.block.time.plus_seconds(10);

    let _res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        from_json::<StakerInfoResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::StakerInfo {
                    staker: deps.api.addr_make("addr0000").to_string(),
                    block_time: None,
                },
            )
            .unwrap(),
        )
        .unwrap(),
        StakerInfoResponse {
            staker: deps.api.addr_make("addr0000").to_string(),
            reward_index: Decimal::from_ratio(1000u128, 1u128),
            pending_reward: Uint128::from(100000u128),
            bond_amount: Uint128::from(200u128),
        }
    );

    assert_eq!(
        from_json::<StateResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::State { block_time: None }
            )
            .unwrap()
        )
        .unwrap(),
        StateResponse {
            total_bond_amount: Uint128::from(200u128),
            global_reward_index: Decimal::from_ratio(1000u128, 1u128),
            last_distributed: mock_env().block.time.seconds() + 10,
            undistributed_rewards: Uint128::from(10_900_000u128),
        }
    );

    // failed with unauthorized
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });

    let info = message_info(&deps.api.addr_make("staking0001"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("Must return unauthorized error"),
    }
}

#[test]
fn test_unbond() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (12345, 12345 + 100, Uint128::from(1000000u128)),
            (12345 + 100, 12345 + 200, Uint128::from(10000000u128)),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let _res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    // unbond 150 tokens; failed
    let msg = ExecuteMsg::Unbond {
        amount: Uint128::from(150u128),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    match res {
        StdError::GenericErr { msg, .. } => {
            assert_eq!(msg, "Cannot unbond more than bond amount");
        }
        _ => panic!("Must return generic error"),
    };

    // normal unbond
    let msg = ExecuteMsg::Unbond {
        amount: Uint128::from(100u128),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(100u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );
}

#[test]
fn test_compute_reward() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    fund_via_reward_cw20(&mut deps, Uint128::from(11_000_000u128));

    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    let _res = execute(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

    // 100 seconds passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(100);

    // bond 100 more tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let _res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    assert_eq!(
        from_json::<StakerInfoResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::StakerInfo {
                    staker: deps.api.addr_make("addr0000").to_string(),
                    block_time: None,
                },
            )
            .unwrap()
        )
        .unwrap(),
        StakerInfoResponse {
            staker: deps.api.addr_make("addr0000").to_string(),
            reward_index: Decimal::from_ratio(10000u128, 1u128),
            pending_reward: Uint128::from(1000000u128),
            bond_amount: Uint128::from(200u128),
        }
    );

    // 100 seconds passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(10);
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // unbond
    let msg = ExecuteMsg::Unbond {
        amount: Uint128::from(100u128),
    };
    let _res = execute(deps.as_mut(), env, info, msg).unwrap();
    assert_eq!(
        from_json::<StakerInfoResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::StakerInfo {
                    staker: deps.api.addr_make("addr0000").to_string(),
                    block_time: None,
                },
            )
            .unwrap()
        )
        .unwrap(),
        StakerInfoResponse {
            staker: deps.api.addr_make("addr0000").to_string(),
            reward_index: Decimal::from_ratio(15000u64, 1u64),
            pending_reward: Uint128::from(2000000u128),
            bond_amount: Uint128::from(100u128),
        }
    );

    // query future block
    assert_eq!(
        from_json::<StakerInfoResponse>(
            &query(
                deps.as_ref(),
                mock_env(),
                QueryMsg::StakerInfo {
                    staker: deps.api.addr_make("addr0000").to_string(),
                    block_time: Some(mock_env().block.time.plus_seconds(120).seconds()),
                },
            )
            .unwrap()
        )
        .unwrap(),
        StakerInfoResponse {
            staker: deps.api.addr_make("addr0000").to_string(),
            reward_index: Decimal::from_ratio(25000u64, 1u64),
            pending_reward: Uint128::from(3000000u128),
            bond_amount: Uint128::from(100u128),
        }
    );
}

#[test]
fn test_withdraw() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    fund_via_reward_cw20(&mut deps, Uint128::from(11_000_000u128));

    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    let _res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    // 100 seconds passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(100);

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    let msg = ExecuteMsg::Withdraw {};
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(1000000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );
}

#[test]
fn test_migrate_staking() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    fund_via_reward_cw20(&mut deps, Uint128::from(11_000_000u128));

    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    let _res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    // 100 seconds is passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(100);
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    let msg = ExecuteMsg::Withdraw {};
    let res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(1000000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );

    // execute migration after 50 seconds
    env.block.time = env.block.time.plus_seconds(50);

    let msg = ExecuteMsg::MigrateStaking {
        new_staking_contract: deps.api.addr_make("newstaking0000").to_string(),
    };

    // unauthorized attempt
    let info = message_info(&deps.api.addr_make("notaddr0000"), &[]);
    let res = execute(deps.as_mut(), env.clone(), info, msg.clone());
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("Must return unauthorized error"),
    }

    // successful attempt
    //
    // Funded 11M; 1M was credited on the withdraw (slot 0 fully); another 5M
    // credited on compute_reward inside migrate_staking (half of slot 1).
    // Remaining undistributed: 11M - 6M = 5M, which is what's forwarded.
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        res.attributes,
        vec![
            attr("action", "migrate_staking"),
            attr("remaining_amount", "5000000"),
        ]
    );

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("newstaking0000").to_string(),
                amount: Uint128::from(5000000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );

    // The distribution schedule is untouched by migrate_staking — the source
    // of truth for what's left to distribute is `undistributed_rewards`, not
    // the schedule.
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config,
        ConfigResponse {
            reward_token: deps.api.addr_make("reward0000").to_string(),
            staking_token: deps.api.addr_make("staking0000").to_string(),
            distribution_schedule: vec![
                (
                    mock_env().block.time.seconds(),
                    mock_env().block.time.seconds() + 100,
                    Uint128::from(1000000u128)
                ),
                (
                    mock_env().block.time.seconds() + 100,
                    mock_env().block.time.seconds() + 200,
                    Uint128::from(10000000u128)
                ),
            ]
        }
    );

    // State's undistributed_rewards is zeroed after migrate.
    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::State { block_time: None },
    )
    .unwrap();
    let state: StateResponse = from_json(&res).unwrap();
    assert_eq!(state.undistributed_rewards, Uint128::zero());
}

#[test]
fn test_update_config() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Fund with the full schedule total so distribution math matches the
    // original expectations of this test.
    fund_via_reward_cw20(&mut deps, Uint128::from(41_000_000u128));

    let update_config = UpdateConfig {
        distribution_schedule: vec![(
            mock_env().block.time.seconds() + 300,
            mock_env().block.time.seconds() + 400,
            Uint128::from(10000000u128),
        )],
    };

    let info = message_info(&deps.api.addr_make("notgov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("Must return unauthorized error"),
    }

    // do some bond and update rewards
    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    let _res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    // 100 seconds is passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(100);
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    let msg = ExecuteMsg::Withdraw {};
    let res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(1000000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );

    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(5000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config);
    match res {
        Err(StdError::GenericErr { msg, .. }) => {
            assert_eq!(msg, "new schedule removes already started distribution")
        }
        _ => panic!("Must return unauthorized error"),
    }

    // do some bond and update rewards
    // bond 100 tokens
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let _res = execute(deps.as_mut(), env.clone(), info, msg).unwrap();

    // 100 seconds is passed
    // 1,000,000 rewards distributed
    env.block.time = env.block.time.plus_seconds(100);

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    let msg = ExecuteMsg::Withdraw {};
    let _res = execute(deps.as_mut(), env, info, msg).unwrap();

    //cannot update previous schedule
    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(5000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(10000000u128),
            ),
        ],
    };

    deps.querier
        .with_anc_minter(deps.api.addr_make("gov0000").to_string());

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config);
    match res {
        Err(StdError::GenericErr { msg, .. }) => {
            assert_eq!(msg, "new schedule removes already started distribution")
        }
        _ => panic!("Must return unauthorized error"),
    }

    //successful one
    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(20000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(10000000u128),
            ),
        ],
    };

    deps.querier
        .with_anc_minter(deps.api.addr_make("gov0000").to_string());

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config).unwrap();

    assert_eq!(res.attributes, vec![("action", "update_config")]);

    // query config
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config.distribution_schedule,
        vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(20000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(10000000u128),
            ),
        ]
    );

    //successful one
    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(20000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(50000000u128),
            ),
        ],
    };

    deps.querier
        .with_anc_minter(deps.api.addr_make("gov0000").to_string());

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config).unwrap();

    assert_eq!(res.attributes, vec![("action", "update_config")]);

    // query config
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config.distribution_schedule,
        vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(20000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(50000000u128),
            ),
        ]
    );

    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(90000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(80000000u128),
            ),
        ],
    };

    deps.querier
        .with_anc_minter(deps.api.addr_make("gov0000").to_string());

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config).unwrap();

    assert_eq!(res.attributes, vec![("action", "update_config")]);

    // query config
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config.distribution_schedule,
        vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(90000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(80000000u128),
            ),
        ]
    );

    let update_config = UpdateConfig {
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(90000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(80000000u128),
            ),
            (
                mock_env().block.time.seconds() + 500,
                mock_env().block.time.seconds() + 600,
                Uint128::from(60000000u128),
            ),
        ],
    };

    deps.querier
        .with_anc_minter(deps.api.addr_make("gov0000").to_string());

    let info = message_info(&deps.api.addr_make("gov0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, update_config).unwrap();

    assert_eq!(res.attributes, vec![("action", "update_config")]);

    // query config
    let res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(
        config.distribution_schedule,
        vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1000000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 200,
                mock_env().block.time.seconds() + 300,
                Uint128::from(10000000u128),
            ),
            (
                mock_env().block.time.seconds() + 300,
                mock_env().block.time.seconds() + 400,
                Uint128::from(90000000u128),
            ),
            (
                mock_env().block.time.seconds() + 400,
                mock_env().block.time.seconds() + 500,
                Uint128::from(80000000u128),
            ),
            (
                mock_env().block.time.seconds() + 500,
                mock_env().block.time.seconds() + 600,
                Uint128::from(60000000u128),
            )
        ]
    );
}

#[test]
fn test_instantiate_and_query_native_reward_token() {
    let mut deps = mock_dependencies(&[]);

    // Set current time for reproducibility.
    let env = mock_env();
    let current_time = env.block.time.seconds();

    // Instantiate the contract with a native reward token (e.g., "inj")
    let msg = InstantiateMsg {
        reward_token: AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (current_time, current_time + 100, Uint128::from(1000000u128)),
            (
                current_time + 100,
                current_time + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    // Use "addr0000" as the instantiator (owner)
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

    // Query config and verify that the reward token returns the native denom.
    let res = query(deps.as_ref(), env.clone(), QueryMsg::Config {}).unwrap();
    let config: ConfigResponse = from_json(&res).unwrap();
    assert_eq!(config.reward_token, "inj".to_string());
    assert_eq!(
        config.staking_token,
        deps.api.addr_make("staking0000").to_string()
    );
}

#[test]
fn test_withdraw_native_reward_token() {
    let mut deps = mock_dependencies(&[]);
    let mut env = mock_env();
    let current_time = env.block.time.seconds();

    // Instantiate with a native reward token ("inj")
    let msg = InstantiateMsg {
        reward_token: AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![
            (current_time, current_time + 100, Uint128::from(1000000u128)),
            (
                current_time + 100,
                current_time + 200,
                Uint128::from(10000000u128),
            ),
        ],
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

    // Fund the reward pool with native "inj".
    fund_via_reward_native(&mut deps, "inj", Uint128::from(11_000_000u128));

    // Bond 100 tokens.
    let bond_msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    // The staking token is expected to be CW20, so we use "staking0000" as sender.
    let bond_info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let _res = execute(deps.as_mut(), env.clone(), bond_info, bond_msg).unwrap();

    // Advance time by 100 seconds so that rewards can accumulate.
    env.block.time = env.block.time.plus_seconds(100);

    // Withdraw rewards.
    let withdraw_msg = ExecuteMsg::Withdraw {};
    let withdraw_info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), env.clone(), withdraw_info, withdraw_msg).unwrap();

    // The expected reward (from the first slot) is 1,000,000.
    // Check that the message is a BankMsg::Send with denom "inj"
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: deps.api.addr_make("addr0000").to_string(),
            amount: coins(1000000, "inj"),
        }))]
    );
}

#[test]
fn test_bond_native() {
    let mut deps = mock_dependencies(&[]);

    let instantiate_msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::NativeToken {
            denom: "uusd".to_string(),
        },
        distribution_schedule: vec![
            (
                mock_env().block.time.seconds(),
                mock_env().block.time.seconds() + 100,
                Uint128::from(1_000_000u128),
            ),
            (
                mock_env().block.time.seconds() + 100,
                mock_env().block.time.seconds() + 200,
                Uint128::from(10_000_000u128),
            ),
        ],
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), env.clone(), info, instantiate_msg).unwrap();

    // Simulate bonding by a user sending native funds.
    let bond_amount = Uint128::from(100u128);
    let bond_info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: bond_amount,
        }],
    );

    // Use the new Bond message variant (for native bonding).
    let bond_msg = ExecuteMsg::Bond {
        amount: bond_amount,
    };

    execute(deps.as_mut(), env.clone(), bond_info, bond_msg).unwrap();

    // Query staker info.
    let staker_query = QueryMsg::StakerInfo {
        staker: deps.api.addr_make("addr0000").to_string(),
        block_time: None,
    };
    let staker_info_bin = query(deps.as_ref(), env.clone(), staker_query).unwrap();
    let staker_info: StakerInfoResponse = from_json(&staker_info_bin).unwrap();

    // The staker's bond amount should now equal the bond_amount.
    assert_eq!(staker_info.bond_amount, bond_amount);

    // Query global state.
    let state_query = QueryMsg::State { block_time: None };
    let state_bin = query(deps.as_ref(), env.clone(), state_query).unwrap();
    let state: StateResponse = from_json(&state_bin).unwrap();

    // The total bond amount in state should equal the bond_amount.
    assert_eq!(state.total_bond_amount, bond_amount);
}

#[test]
fn test_unbond_native() {
    let mut deps = mock_dependencies(&[]);

    // Instantiate contract with a native staking token.
    let instantiate_msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::NativeToken {
            denom: "uusd".to_string(),
        },
        distribution_schedule: vec![
            (12345, 12345 + 100, Uint128::from(1_000_000u128)),
            (12345 + 100, 12345 + 200, Uint128::from(10_000_000u128)),
        ],
    };

    // Create an environment with contract address equal to MOCK_CONTRACT_ADDR.
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _inst_res = instantiate(deps.as_mut(), env.clone(), info, instantiate_msg).unwrap();

    // Simulate bonding native tokens.
    let bond_amount = Uint128::from(100u128);
    // For native bonding, assume we have an ExecuteMsg::Bond variant.
    let bond_msg = ExecuteMsg::Bond {
        amount: bond_amount,
    };
    // The user sends the native tokens in funds.
    let bond_info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: bond_amount,
        }],
    );
    let _bond_res = execute(deps.as_mut(), env.clone(), bond_info, bond_msg).unwrap();

    // Try to unbond too many tokens: should fail.
    let unbond_msg = ExecuteMsg::Unbond {
        amount: Uint128::from(150u128),
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res_err = execute(deps.as_mut(), env.clone(), info, unbond_msg).unwrap_err();
    match res_err {
        StdError::GenericErr { msg, .. } => {
            assert_eq!(msg, "Cannot unbond more than bond amount");
        }
        _ => panic!("Expected generic error"),
    };

    // Normal unbond: unbond exactly 100 tokens.
    let unbond_msg = ExecuteMsg::Unbond {
        amount: Uint128::from(100u128),
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res_unbond = execute(deps.as_mut(), env.clone(), info, unbond_msg).unwrap();

    // For native staking tokens, the unbond operation should result in a BankMsg::Send
    // that sends the unbonded amount back to the user.
    let expected_msg = SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
        to_address: deps.api.addr_make("addr0000").to_string(),
        amount: coins(100, "uusd"),
    }));

    assert_eq!(res_unbond.messages, vec![expected_msg]);
}

/// Fund via the CW20 reward token's `Receive` hook succeeds and increments
/// `undistributed_rewards`. Fund via a different CW20 is rejected.
#[test]
fn test_fund_cw20() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![(
            mock_env().block.time.seconds(),
            mock_env().block.time.seconds() + 100,
            Uint128::from(1_000_000u128),
        )],
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Someone else's CW20 is not accepted as the reward token.
    let wrong_info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let wrong_msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("funder").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Fund {}).unwrap(),
    });
    let err = execute(deps.as_mut(), mock_env(), wrong_info, wrong_msg).unwrap_err();
    match err {
        StdError::GenericErr { msg, .. } => assert_eq!(msg, "unauthorized"),
        _ => panic!("expected unauthorized"),
    };

    // Native Fund is rejected when reward_token is CW20.
    let native_info = message_info(
        &deps.api.addr_make("funder"),
        &coins(100, "inj"),
    );
    let err = execute(deps.as_mut(), mock_env(), native_info, ExecuteMsg::Fund {}).unwrap_err();
    match err {
        StdError::GenericErr { msg, .. } => assert!(msg.contains("reward token is cw20")),
        _ => panic!("expected cw20 rejection"),
    };

    // Correct CW20 funding.
    fund_via_reward_cw20(&mut deps, Uint128::from(500_000u128));
    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::State { block_time: None },
    )
    .unwrap();
    let state: StateResponse = from_json(&res).unwrap();
    assert_eq!(state.undistributed_rewards, Uint128::from(500_000u128));
}

/// Native Fund succeeds with the correct denom; wrong denom and CW20 hook
/// against a native reward are rejected.
#[test]
fn test_fund_native() {
    let mut deps = mock_dependencies(&[]);
    let msg = InstantiateMsg {
        reward_token: AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![(
            mock_env().block.time.seconds(),
            mock_env().block.time.seconds() + 100,
            Uint128::from(1_000_000u128),
        )],
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Wrong denom rejected.
    let wrong_info = message_info(&deps.api.addr_make("funder"), &coins(100, "usdc"));
    let err = execute(deps.as_mut(), mock_env(), wrong_info, ExecuteMsg::Fund {}).unwrap_err();
    match err {
        StdError::GenericErr { msg, .. } => assert!(msg.contains("Incorrect fund denom")),
        _ => panic!("expected denom rejection"),
    };

    // CW20 Fund hook rejected when reward is native.
    let cw20_info = message_info(&deps.api.addr_make("reward0000"), &[]);
    let cw20_msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("funder").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Fund {}).unwrap(),
    });
    let err = execute(deps.as_mut(), mock_env(), cw20_info, cw20_msg).unwrap_err();
    match err {
        StdError::GenericErr { msg, .. } => assert!(msg.contains("reward token is native")),
        _ => panic!("expected native rejection"),
    };

    // Correct native funding.
    fund_via_reward_native(&mut deps, "inj", Uint128::from(777u128));
    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::State { block_time: None },
    )
    .unwrap();
    let state: StateResponse = from_json(&res).unwrap();
    assert_eq!(state.undistributed_rewards, Uint128::from(777u128));
}

/// If the contract is underfunded, distribution is capped to what's available
/// — the schedule cannot promise rewards the contract can't pay.
#[test]
fn test_compute_reward_capped_by_funding() {
    let mut deps = mock_dependencies(&[]);
    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        // Schedule promises 1,000,000 over 100 seconds.
        distribution_schedule: vec![(
            mock_env().block.time.seconds(),
            mock_env().block.time.seconds() + 100,
            Uint128::from(1_000_000u128),
        )],
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Only fund 300_000 — schedule promises 1_000_000.
    fund_via_reward_cw20(&mut deps, Uint128::from(300_000u128));

    // Bond.
    let bond = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(100u128),
        msg: to_json_binary(&Cw20HookMsg::Bond {}).unwrap(),
    });
    let bond_info = message_info(&deps.api.addr_make("staking0000"), &[]);
    let mut env = mock_env();
    execute(deps.as_mut(), env.clone(), bond_info, bond).unwrap();

    // Fast-forward past the end of the schedule. Theoretical = 1M, but only
    // 300k was funded — user's pending_reward is capped at 300k.
    env.block.time = env.block.time.plus_seconds(200);
    let withdraw_info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), env, withdraw_info, ExecuteMsg::Withdraw {}).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(300_000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );

    // undistributed is drained to zero.
    let state: StateResponse = from_json(
        &query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::State { block_time: None },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state.undistributed_rewards, Uint128::zero());
}

/// Rewards accrued during a period when nobody is staked stay in the pool and
/// are forwarded on migrate — they're not trapped forever (L-3).
#[test]
fn test_zero_bond_period_preserves_funds_through_migrate() {
    let mut deps = mock_dependencies(&[]);
    let start = mock_env().block.time.seconds();
    let msg = InstantiateMsg {
        reward_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: deps.api.addr_make("staking0000").to_string(),
        },
        distribution_schedule: vec![(start, start + 100, Uint128::from(1_000_000u128))],
    };
    let owner = deps.api.addr_make("addr0000");
    let info = message_info(&owner, &[]);
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    fund_via_reward_cw20(&mut deps, Uint128::from(1_000_000u128));

    // Advance past the end of the schedule without anyone bonding. The whole
    // distribution period happens with total_bond_amount == 0 — in the old
    // design this 1M would be stranded; now it stays in undistributed_rewards.
    let mut env = mock_env();
    env.block.time = env.block.time.plus_seconds(200);

    let migrate = ExecuteMsg::MigrateStaking {
        new_staking_contract: deps.api.addr_make("newstaking").to_string(),
    };
    let res = execute(deps.as_mut(), env, message_info(&owner, &[]), migrate).unwrap();

    // Full 1M forwarded to the new contract.
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("reward0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("newstaking").to_string(),
                amount: Uint128::from(1_000_000u128),
            })
            .unwrap(),
            funds: vec![],
        }))]
    );
}

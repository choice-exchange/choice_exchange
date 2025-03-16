use crate::contract::{execute, instantiate, query};
use crate::mock_querier::mock_dependencies;
use choice::asset::AssetInfo;
use choice::staking::ExecuteMsg::UpdateConfig;
use choice::staking::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg, StakerInfoResponse,
    StateResponse,
};
use cosmwasm_std::testing::{message_info, mock_env};
use cosmwasm_std::{
    attr, coins, from_json, to_json_binary, BankMsg, Coin, CosmosMsg, Decimal, StdError, SubMsg,
    Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

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
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        res.attributes,
        vec![
            attr("action", "migrate_staking"),
            attr("distributed_amount", "6000000"), // 1000000 + (10000000 / 2)
            attr("remaining_amount", "5000000")    // 11,000,000 - 6000000
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

    // query config
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
                    mock_env().block.time.seconds() + 150,
                    Uint128::from(5000000u128)
                ), // slot was modified
            ]
        }
    );
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

    let res = execute(deps.as_mut(), env.clone(), bond_info, bond_msg).unwrap();
    println!("Bond response: {:?}", res);

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

use cosmwasm_std::testing::{message_info, mock_env, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    coin, from_json, to_json_binary, Addr, Api, Coin, CosmosMsg, StdError, SubMsg, Uint128, WasmMsg,
};

use crate::contract::{execute, instantiate, query};
use crate::operations::asset_into_swap_msg;
use choice::mock_querier::mock_dependencies;

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use choice::router::{
    ConfigResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg,
    SimulateSwapOperationsResponse, SwapOperation,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

#[test]
fn proper_initialization() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // it worked, let's query the state
    let config: ConfigResponse =
        from_json(&query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(
        deps.api.addr_make("choicefactory").to_string(),
        config.choice_factory.as_str()
    );
}

#[test]
fn execute_swap_operations() {
    let mut deps = mock_dependencies(&[]);
    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0002").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(1000000u128))],
    )]);

    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    let msg = ExecuteMsg::ExecuteSwapOperations {
        operations: vec![],
        minimum_receive: None,
        to: None,
        deadline: None,
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "must provide operations"),
        _ => panic!("DO NOT ENTER HERE"),
    }

    let msg = ExecuteMsg::ExecuteSwapOperations {
        operations: vec![
            SwapOperation::Choice {
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "ukrw".to_string(),
                },
                ask_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0001").to_string(),
                },
            },
            SwapOperation::Choice {
                offer_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0001").to_string(),
                },
                ask_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            },
            SwapOperation::Choice {
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                ask_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0002").to_string(),
                },
            },
        ],
        minimum_receive: Some(Uint128::from(1000000u128)),
        to: None,
        deadline: None,
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "ukrw".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0001").to_string(),
                        },
                    },
                    to: None,
                    deadline: None,
                })
                .unwrap(),
            })),
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0001").to_string(),
                        },
                        ask_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    },
                    to: None,
                    deadline: None,
                })
                .unwrap(),
            })),
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0002").to_string(),
                        },
                    },
                    to: Some(deps.api.addr_make("addr0000").to_string()),
                    deadline: None,
                })
                .unwrap(),
            })),
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::AssertMinimumReceive {
                    asset_info: AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0002").to_string(),
                    },
                    prev_balance: Uint128::zero(),
                    minimum_receive: Uint128::from(1000000u128),
                    receiver: deps.api.addr_make("addr0000").to_string(),
                })
                .unwrap(),
            })),
        ]
    );

    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: Uint128::from(1000000u128),
        msg: to_json_binary(&Cw20HookMsg::ExecuteSwapOperations {
            operations: vec![
                SwapOperation::Choice {
                    offer_asset_info: AssetInfo::NativeToken {
                        denom: "ukrw".to_string(),
                    },
                    ask_asset_info: AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0001").to_string(),
                    },
                },
                SwapOperation::Choice {
                    offer_asset_info: AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0001").to_string(),
                    },
                    ask_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                },
                SwapOperation::Choice {
                    offer_asset_info: AssetInfo::NativeToken {
                        denom: "inj".to_string(),
                    },
                    ask_asset_info: AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0002").to_string(),
                    },
                },
            ],
            minimum_receive: None,
            to: Some(deps.api.addr_make("addr0002").to_string()),
            deadline: None,
        })
        .unwrap(),
    });

    let info = message_info(&deps.api.addr_make("asset0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "ukrw".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0001").to_string(),
                        },
                    },
                    to: None,
                    deadline: None,
                })
                .unwrap(),
            })),
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0001").to_string(),
                        },
                        ask_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    },
                    to: None,
                    deadline: None,
                })
                .unwrap(),
            })),
            SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: MOCK_CONTRACT_ADDR.to_string(),
                funds: vec![],
                msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                    operation: SwapOperation::Choice {
                        offer_asset_info: AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                        ask_asset_info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0002").to_string(),
                        },
                    },
                    to: Some(deps.api.addr_make("addr0002").to_string()),
                    deadline: None,
                })
                .unwrap(),
            }))
        ]
    );
}

#[test]
fn execute_swap_operation() {
    let mut deps = mock_dependencies(&[]);
    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    deps.querier.with_choice_factory(
        &[(
            &("uusd".to_string() + deps.api.addr_make("asset0000").as_str()),
            &PairInfo {
                asset_infos: [
                    AssetInfo::NativeToken {
                        denom: "uusd".to_string(),
                    },
                    AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0000").to_string(),
                    },
                ],
                contract_addr: deps.api.addr_make("pair0000").to_string(),
                liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                asset_decimals: [6u8, 6u8],
                burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
            },
        )],
        &[("uusd".to_string(), 6u8)],
    );
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        [Coin {
            amount: Uint128::from(1000000u128),
            denom: "uusd".to_string(),
        }]
        .to_vec(),
    )]);

    let msg = ExecuteMsg::ExecuteSwapOperation {
        operation: SwapOperation::Choice {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        },
        to: None,
        deadline: None,
    };
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg.clone());
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("DO NOT ENTER HERE"),
    }

    let sender = deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap();
    let info = message_info(&sender, &[]);

    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(
            asset_into_swap_msg(
                deps.as_ref(),
                Addr::unchecked(deps.api.addr_make("pair0000")),
                Asset {
                    info: AssetInfo::NativeToken {
                        denom: "uusd".to_string(),
                    },
                    amount: Uint128::from(1000000u128)
                },
                None,
                None,
                None,
            )
            .unwrap()
        )],
    );

    // optional to address
    // swap_send
    let msg = ExecuteMsg::ExecuteSwapOperation {
        operation: SwapOperation::Choice {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        },
        to: Some(deps.api.addr_make("addr0000").to_string()),
        deadline: None,
    };

    let info = message_info(&deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![SubMsg::new(
            asset_into_swap_msg(
                deps.as_ref(),
                Addr::unchecked(deps.api.addr_make("pair0000")),
                Asset {
                    info: AssetInfo::NativeToken {
                        denom: "uusd".to_string(),
                    },
                    amount: Uint128::from(1000000u128)
                },
                None,
                Some(deps.api.addr_make("addr0000").to_string()),
                None,
            )
            .unwrap()
        )],
    );
    deps.querier.with_choice_factory(
        &[(
            &format!("uusd{}", deps.api.addr_make("asset")),
            &PairInfo {
                asset_infos: [
                    AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset").to_string(),
                    },
                    AssetInfo::NativeToken {
                        denom: "uusd".to_string(),
                    },
                ],
                contract_addr: deps.api.addr_make("pair0000").to_string(),
                liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                asset_decimals: [6u8, 6u8],
                burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
            },
        )],
        &[("uusd".to_string(), 6u8)],
    );
    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(1000000u128))],
    )]);

    let msg = ExecuteMsg::ExecuteSwapOperation {
        operation: SwapOperation::Choice {
            offer_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset").to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
        },
        to: Some(deps.api.addr_make("addr0000").to_string()),
        deadline: None,
    };

    let info = message_info(&deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("asset").to_string(),
            funds: vec![],
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: deps.api.addr_make("pair0000").to_string(),
                amount: Uint128::from(1000000u128),
                msg: to_json_binary(&PairExecuteMsg::Swap {
                    offer_asset: Asset {
                        info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset").to_string(),
                        },
                        amount: Uint128::from(1000000u128),
                    },
                    belief_price: None,
                    max_spread: None,
                    to: Some(deps.api.addr_make("addr0000").to_string()),
                    deadline: None,
                })
                .unwrap()
            })
            .unwrap()
        }))]
    );
}

#[test]
fn query_buy_with_routes() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    let msg = QueryMsg::SimulateSwapOperations {
        offer_amount: Uint128::from(1000000u128),
        operations: vec![
            SwapOperation::Choice {
                offer_asset_info: AssetInfo::NativeToken {
                    denom: "ukrw".to_string(),
                },
                ask_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
            },
            SwapOperation::Choice {
                offer_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                ask_asset_info: AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
            },
        ],
    };

    deps.querier.with_choice_factory(
        &[
            (
                &format!("ukrw{}", deps.api.addr_make("asset0000")),
                &PairInfo {
                    asset_infos: [
                        AssetInfo::NativeToken {
                            denom: "ukrw".to_string(),
                        },
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                    ],
                    contract_addr: deps.api.addr_make("pair0000").to_string(),
                    liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                    asset_decimals: [6u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
            (
                &format!("{}inj", deps.api.addr_make("asset0000")),
                &PairInfo {
                    asset_infos: [
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    ],
                    contract_addr: deps.api.addr_make("pair0001").to_string(),
                    liquidity_token: deps.api.addr_make("liquidity0001").to_string(),
                    asset_decimals: [6u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
        ],
        &[("ukrw".to_string(), 6u8), ("inj".to_string(), 6u8)],
    );

    let res: SimulateSwapOperationsResponse =
        from_json(&query(deps.as_ref(), mock_env(), msg).unwrap()).unwrap();
    assert_eq!(
        res,
        SimulateSwapOperationsResponse {
            amount: Uint128::from(1000000u128)
        }
    );
}

#[test]
fn query_reverse_routes_with_from_native() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let target_amount = 1000000u128;

    let info = message_info(&deps.api.addr_make("addr0000"), &[coin(10000000, "ukrw")]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        [Coin {
            amount: Uint128::from(1000000u128),
            denom: "ukrw".to_string(),
        }]
        .to_vec(),
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0001").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(1000000u128))],
    )]);

    let msg = QueryMsg::ReverseSimulateSwapOperations {
        ask_amount: Uint128::from(target_amount),
        operations: vec![SwapOperation::Choice {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        }],
    };

    deps.querier.with_choice_factory(
        &[
            (
                &format!("ukrw{}", deps.api.addr_make("asset0000")),
                &PairInfo {
                    contract_addr: deps.api.addr_make("pair0000").to_string(),
                    liquidity_token: "liquidity0000".to_string(),
                    asset_infos: [
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "ukrw".to_string(),
                        },
                    ],
                    asset_decimals: [8u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
            (
                &format!("{}inj", deps.api.addr_make("asset0000")),
                &PairInfo {
                    contract_addr: deps.api.addr_make("pair0001").to_string(),
                    liquidity_token: deps.api.addr_make("liquidity0001").to_string(),
                    asset_infos: [
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    ],
                    asset_decimals: [8u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
        ],
        &[("ukrw".to_string(), 6u8), ("inj".to_string(), 6u8)],
    );

    let res: SimulateSwapOperationsResponse =
        from_json(&query(deps.as_ref(), mock_env(), msg).unwrap()).unwrap();

    assert_eq!(
        res,
        SimulateSwapOperationsResponse {
            amount: Uint128::from(1000000u128),
        }
    );

    let offer_amount = res.amount;

    let msg = ExecuteMsg::ExecuteSwapOperation {
        operation: SwapOperation::Choice {
            offer_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            ask_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        },
        to: None,
        deadline: None,
    };
    let info = message_info(
        &deps.api.addr_make("addr0"),
        &[coin(offer_amount.u128(), "ukrw")],
    );
    let res = execute(deps.as_mut(), mock_env(), info, msg.clone());
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("DO NOT ENTER HERE"),
    }

    let info = message_info(&deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("pair0000").to_string(),
            funds: vec![coin(target_amount, "ukrw")],
            msg: to_json_binary(&PairExecuteMsg::Swap {
                offer_asset: Asset {
                    info: AssetInfo::NativeToken {
                        denom: "ukrw".to_string(),
                    },
                    amount: Uint128::from(target_amount),
                },
                belief_price: None,
                max_spread: None,
                to: None,
                deadline: None,
            })
            .unwrap(),
        })),],
    );
}

#[test]
fn query_reverse_routes_with_to_native() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        choice_factory: deps.api.addr_make("choicefactory").to_string(),
    };

    let target_amount = 1000000u128;

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    deps.querier.with_token_balances(&[
        (
            &deps.api.addr_make("asset0000").to_string(),
            &[(
                &deps.api.addr_make("pair0000").to_string(),
                &Uint128::from(1000000u128),
            )],
        ),
        (
            &deps.api.addr_make("asset0000").to_string(),
            &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(1000000u128))],
        ),
    ]);

    let msg = QueryMsg::ReverseSimulateSwapOperations {
        ask_amount: Uint128::from(target_amount),
        operations: vec![SwapOperation::Choice {
            offer_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
        }],
    };

    deps.querier.with_choice_factory(
        &[
            (
                &format!("ukrw{}", deps.api.addr_make("asset0000")),
                &PairInfo {
                    contract_addr: deps.api.addr_make("pair0000").to_string(),
                    liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                    asset_infos: [
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "ukrw".to_string(),
                        },
                    ],
                    asset_decimals: [8u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
            (
                &format!("{}inj", deps.api.addr_make("asset0000")),
                &PairInfo {
                    contract_addr: deps.api.addr_make("pair0001").to_string(),
                    liquidity_token: deps.api.addr_make("liquidity0001").to_string(),
                    asset_infos: [
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "inj".to_string(),
                        },
                    ],
                    asset_decimals: [8u8, 6u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
                },
            ),
        ],
        &[("ukrw".to_string(), 6u8), ("inj".to_string(), 6u8)],
    );

    let res: SimulateSwapOperationsResponse =
        from_json(&query(deps.as_ref(), mock_env(), msg).unwrap()).unwrap();

    assert_eq!(
        res,
        SimulateSwapOperationsResponse {
            amount: Uint128::from(target_amount),
        }
    );

    let offer_amount = res.amount;

    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0").to_string(),
        amount: offer_amount,
        msg: to_json_binary(&Cw20HookMsg::ExecuteSwapOperations {
            operations: vec![SwapOperation::Choice {
                offer_asset_info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                ask_asset_info: AssetInfo::NativeToken {
                    denom: "ukrw".to_string(),
                },
            }],
            minimum_receive: None,
            to: None,
            deadline: None,
        })
        .unwrap(),
    });
    let info = message_info(&deps.api.addr_make("addr0"), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: MOCK_CONTRACT_ADDR.to_string(),
            funds: vec![],
            msg: to_json_binary(&ExecuteMsg::ExecuteSwapOperation {
                operation: SwapOperation::Choice {
                    offer_asset_info: AssetInfo::Token {
                        contract_addr: deps.api.addr_make("asset0000").to_string(),
                    },
                    ask_asset_info: AssetInfo::NativeToken {
                        denom: "ukrw".to_string(),
                    },
                },
                to: Some(deps.api.addr_make("addr0").to_string()),
                deadline: None,
            })
            .unwrap(),
        })),],
    );

    let msg = ExecuteMsg::ExecuteSwapOperation {
        operation: SwapOperation::Choice {
            offer_asset_info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            ask_asset_info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
        },
        to: None,
        deadline: None,
    };

    let info = message_info(&deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), &[]);
    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("asset0000").to_string(),
            funds: vec![],
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: deps.api.addr_make("pair0000").to_string(),
                amount: Uint128::from(target_amount),
                msg: to_json_binary(&PairExecuteMsg::Swap {
                    offer_asset: Asset {
                        info: AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0000").to_string(),
                        },
                        amount: Uint128::from(target_amount),
                    },
                    belief_price: None,
                    max_spread: None,
                    to: None,
                    deadline: None,
                })
                .unwrap(),
            })
            .unwrap(),
        }))],
    );
}

#[test]
fn assert_minimum_receive_native_token() {
    let mut deps = mock_dependencies(&[]);
    deps.querier.with_balance(&[(
        &deps.api.addr_make("addr0000").to_string(),
        [Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1000000u128),
        }]
        .to_vec(),
    )]);

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // success
    let msg = ExecuteMsg::AssertMinimumReceive {
        asset_info: AssetInfo::NativeToken {
            denom: "uusd".to_string(),
        },
        prev_balance: Uint128::zero(),
        minimum_receive: Uint128::from(1000000u128),
        receiver: deps.api.addr_make("addr0000").to_string(),
    };
    let _res = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap();

    // assertion failed; native token
    let msg = ExecuteMsg::AssertMinimumReceive {
        asset_info: AssetInfo::NativeToken {
            denom: "uusd".to_string(),
        },
        prev_balance: Uint128::zero(),
        minimum_receive: Uint128::from(1000001u128),
        receiver: deps.api.addr_make("addr0000").to_string(),
    };
    let res = execute(deps.as_mut(), mock_env(), info, msg);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(
            msg,
            "assertion failed; minimum receive amount: 1000001, swap amount: 1000000"
        ),
        _ => panic!("DO NOT ENTER HERE"),
    }
}

#[test]
fn assert_minimum_receive_token() {
    let mut deps = mock_dependencies(&[]);
    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("token0000").to_string(),
        &[(
            &deps.api.addr_make("addr0000").to_string(),
            &Uint128::from(1000000u128),
        )],
    )]);

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // success
    let msg = ExecuteMsg::AssertMinimumReceive {
        asset_info: AssetInfo::Token {
            contract_addr: deps.api.addr_make("token0000").to_string(),
        },
        prev_balance: Uint128::zero(),
        minimum_receive: Uint128::from(1000000u128),
        receiver: deps.api.addr_make("addr0000").to_string(),
    };
    let _res = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap();

    // assertion failed; native token
    let msg = ExecuteMsg::AssertMinimumReceive {
        asset_info: AssetInfo::Token {
            contract_addr: deps.api.addr_make("token0000").to_string(),
        },
        prev_balance: Uint128::zero(),
        minimum_receive: Uint128::from(1000001u128),
        receiver: deps.api.addr_make("addr0000").to_string(),
    };
    let res = execute(deps.as_mut(), mock_env(), info, msg);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(
            msg,
            "assertion failed; minimum receive amount: 1000001, swap amount: 1000000"
        ),
        _ => panic!("DO NOT ENTER HERE"),
    }
}

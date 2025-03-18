use crate::contract::{execute, execute_add_native_token_decimals, instantiate, query, reply};
use choice::mock_querier::{mock_dependencies, WasmMockQuerier};
use injective_cosmwasm::InjectiveQueryWrapper;

use crate::state::{pair_key, Config, TmpPairInfo, CONFIG, TMP_PAIR_INFO};

use crate::response::MsgInstantiateContractResponse;
use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, NativeTokenDecimalsResponse, QueryMsg,
    UpdateConfigParams,
};
use choice::pair::{
    ExecuteMsg as PairExecuteMsg, InstantiateMsg as PairInstantiateMsg,
    MigrateMsg as PairMigrateMsg,
};
use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockStorage, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    attr, coin, coins, from_json, to_json_binary, Api, Binary, Coin, CosmosMsg, MsgResponse,
    OwnedDeps, Reply, ReplyOn, Response, StdError, SubMsg, SubMsgResponse, SubMsgResult, Uint128,
    WasmMsg,
};
use cw20::Cw20ExecuteMsg;
use protobuf::Message;

#[test]
fn proper_initialization() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        pair_code_id: 321u64,
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    let query_res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config_res: ConfigResponse = from_json(&query_res).unwrap();
    assert_eq!(321u64, config_res.pair_code_id);
    assert_eq!(deps.api.addr_make("addr0000").to_string(), config_res.owner);
    assert_eq!(
        deps.api.addr_make("burnaddr0000").to_string(),
        config_res.burn_address
    ); // New assertion
    assert_eq!(
        deps.api.addr_make("feeaddr0000").to_string(),
        config_res.fee_wallet_address
    ); // New assertion
}

#[test]
fn update_config() {
    let mut deps = mock_dependencies(&[]);

    // Instantiate the contract with initial config
    let msg = InstantiateMsg {
        pair_code_id: 321u64,
        burn_address: deps.api.addr_make("burnaddr0000").to_string(),
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();

    // Update owner using UpdateConfigParams
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let update_params = UpdateConfigParams {
        owner: Some(deps.api.addr_make("addr0001").to_string()),
        pair_code_id: None,
        burn_address: None,
        fee_wallet_address: None,
    };
    let msg = ExecuteMsg::UpdateConfig {
        params: update_params,
    };

    let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
    assert_eq!(0, res.messages.len());

    // Query and check updated state
    let query_res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config_res: ConfigResponse = from_json(&query_res).unwrap();
    assert_eq!(321u64, config_res.pair_code_id);
    assert_eq!(deps.api.addr_make("addr0001").to_string(), config_res.owner);

    // Update other config fields
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0001"), &[]);
    let update_params = UpdateConfigParams {
        owner: None,
        pair_code_id: Some(100u64),
        burn_address: None,
        fee_wallet_address: None,
    };
    let msg = ExecuteMsg::UpdateConfig {
        params: update_params,
    };

    let res = execute(deps.as_mut(), env, info, msg).unwrap();
    assert_eq!(0, res.messages.len());

    // Query and check updated state
    let query_res = query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap();
    let config_res: ConfigResponse = from_json(&query_res).unwrap();
    assert_eq!(100u64, config_res.pair_code_id);
    assert_eq!(deps.api.addr_make("addr0001").to_string(), config_res.owner);

    // Try an update with unauthorized user
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let update_params = UpdateConfigParams {
        owner: None,
        pair_code_id: None,
        burn_address: None,
        fee_wallet_address: None,
    };
    let msg = ExecuteMsg::UpdateConfig {
        params: update_params,
    };

    let res = execute(deps.as_mut(), env, info, msg);
    match res {
        Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "unauthorized"),
        _ => panic!("Must return unauthorized error"),
    }
}

fn init(
    mut deps: OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper>,
) -> OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper> {
    let mock_api = MockApi::default();

    let msg = InstantiateMsg {
        pair_code_id: 321u64,
        burn_address: mock_api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: mock_api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();
    let info = message_info(&mock_api.addr_make("addr0000"), &[]);

    deps.querier.with_token_balances(&[(
        &mock_api.addr_make("asset0001").to_string(),
        &[(&env.contract.address.to_string(), &Uint128::zero())],
    )]);

    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    deps
}

#[test]
fn create_pair() {
    let mut deps = mock_dependencies(&[coin(10u128, "uusd".to_string())]);
    deps = init(deps);
    deps.querier
        .with_choice_factory(&[], &[("uusd".to_string(), 6u8)]);

    deps.querier.with_token_factory_denom_create_fee(&[(
        &"inj",
        Uint128::from(1_000_000_000_000_000_000u128),
    )]);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0001").to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let msg = ExecuteMsg::CreatePair {
        assets: assets.clone(),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &vec![Coin {
            denom: "inj".to_string(),
            amount: Uint128::from(1_000_000_000_000_000_000u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        res.attributes,
        vec![
            attr("action", "create_pair"),
            attr(
                "pair",
                "uusd-".to_string() + deps.api.addr_make("asset0001").as_str()
            )
        ]
    );

    assert_eq!(
        res.messages,
        vec![SubMsg {
            id: 1,
            payload: Binary::default(),
            gas_limit: None,
            reply_on: ReplyOn::Success,
            msg: WasmMsg::Instantiate {
                msg: to_json_binary(&PairInstantiateMsg {
                    asset_infos: [
                        AssetInfo::NativeToken {
                            denom: "uusd".to_string(),
                        },
                        AssetInfo::Token {
                            contract_addr: deps.api.addr_make("asset0001").to_string(),
                        }
                    ],
                    asset_decimals: [6u8, 8u8],
                    burn_address: deps.api.addr_make("burnaddr0000").to_string(), // Add burn address
                    fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // Add fee wallet address
                })
                .unwrap(),
                code_id: 321u64,
                funds: vec![Coin {
                    denom: "inj".to_string(),
                    amount: Uint128::from(1_000_000_000_000_000_000u128)
                }],
                label: "pair".to_string(),
                admin: Some(MOCK_CONTRACT_ADDR.to_string()),
            }
            .into()
        },]
    );

    let raw_assets = [
        assets[0].to_raw(deps.as_ref().api).unwrap(),
        assets[1].to_raw(deps.as_ref().api).unwrap(),
    ];

    let raw_infos = [
        assets[0].info.to_raw(deps.as_ref().api).unwrap(),
        assets[1].info.to_raw(deps.as_ref().api).unwrap(),
    ];

    assert_eq!(
        TMP_PAIR_INFO.load(&deps.storage).unwrap(),
        TmpPairInfo {
            assets: raw_assets,
            pair_key: pair_key(&raw_infos),
            sender: deps.api.addr_make("addr0000"),
            asset_decimals: [6u8, 8u8]
        }
    );
}

#[test]
fn create_pair_native_token_and_ibc_token() {
    let mock_api = MockApi::default();
    let mut deps = mock_dependencies(&[
        coin(10u128, "uusd".to_string()),
        coin(10u128, "ibc/HASH".to_string()),
    ]);
    deps = init(deps);
    deps.querier.with_choice_factory(
        &[],
        &[("uusd".to_string(), 6u8), ("ibc/HASH".to_string(), 6u8)],
    );

    deps.querier.with_token_factory_denom_create_fee(&[(
        &"inj",
        Uint128::from(1_000_000_000_000_000_000u128),
    )]);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "ibc/HASH".to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let msg = ExecuteMsg::CreatePair {
        assets: assets.clone(),
    };

    let env = mock_env();
    let info = message_info(
        &mock_api.addr_make("addr0000"),
        &vec![Coin {
            denom: "inj".to_string(),
            amount: Uint128::from(1_000_000_000_000_000_000u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    assert_eq!(
        res.attributes,
        vec![attr("action", "create_pair"), attr("pair", "uusd-ibc/HASH")]
    );

    assert_eq!(
        res.messages,
        vec![SubMsg {
            id: 1,
            payload: Binary::default(),
            gas_limit: None,
            reply_on: ReplyOn::Success,
            msg: WasmMsg::Instantiate {
                msg: to_json_binary(&PairInstantiateMsg {
                    asset_infos: [
                        AssetInfo::NativeToken {
                            denom: "uusd".to_string(),
                        },
                        AssetInfo::NativeToken {
                            denom: "ibc/HASH".to_string(),
                        }
                    ],
                    asset_decimals: [6u8, 6u8],
                    burn_address: mock_api.addr_make("burnaddr0000").to_string(), // Add burn address
                    fee_wallet_address: mock_api.addr_make("feeaddr0000").to_string(), // Add fee wallet address
                })
                .unwrap(),
                code_id: 321u64,
                funds: vec![Coin {
                    denom: "inj".to_string(),
                    amount: Uint128::from(1_000_000_000_000_000_000u128)
                }],
                label: "pair".to_string(),
                admin: Some(MOCK_CONTRACT_ADDR.to_string()),
            }
            .into()
        }]
    );

    let raw_assets = [
        assets[0].to_raw(deps.as_ref().api).unwrap(),
        assets[1].to_raw(deps.as_ref().api).unwrap(),
    ];

    let raw_infos = [
        assets[0].info.to_raw(deps.as_ref().api).unwrap(),
        assets[1].info.to_raw(deps.as_ref().api).unwrap(),
    ];

    assert_eq!(
        TMP_PAIR_INFO.load(&deps.storage).unwrap(),
        TmpPairInfo {
            assets: raw_assets,
            pair_key: pair_key(&raw_infos),
            sender: mock_api.addr_make("addr0000"),
            asset_decimals: [6u8, 6u8]
        }
    );
}

#[test]
fn fail_to_create_same_pair() {
    let mut deps = mock_dependencies(&[coin(10u128, "uusd".to_string())]);
    deps = init(deps);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let msg = ExecuteMsg::CreatePair { assets };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    match execute(deps.as_mut(), env, info, msg).unwrap_err() {
        StdError::GenericErr { msg, .. } => assert_eq!(msg, "same asset".to_string()),
        _ => panic!("Must return generic error"),
    }
}

#[test]
fn fail_to_create_pair_with_unknown_denom() {
    let mut deps = mock_dependencies(&[coin(10u128, "uusd".to_string())]);
    deps = init(deps);

    deps.querier
        .with_choice_factory(&[], &[("uusd".to_string(), 6u8)]);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "token".to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let msg = ExecuteMsg::CreatePair { assets };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    match execute(deps.as_mut(), env, info, msg).unwrap_err() {
        StdError::GenericErr { msg, .. } => assert_eq!(msg, "asset1 is invalid".to_string()),
        _ => panic!("Must return generic error"),
    }
}

#[test]
fn fail_to_create_pair_with_unknown_token() {
    let mut deps = mock_dependencies(&[coin(10u128, "uusd".to_string())]);
    deps = init(deps);

    deps.querier
        .with_choice_factory(&[], &[("inj".to_string(), 6u8)]);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::Token {
                contract_addr: "inj123".to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let msg = ExecuteMsg::CreatePair { assets };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    match execute(deps.as_mut(), env, info, msg).unwrap_err() {
        StdError::GenericErr { msg, .. } => assert_eq!(msg, "asset2 is invalid".to_string()),
        _ => panic!("Must return generic error"),
    }
}

#[test]
fn reply_only_create_pair() {
    let mut deps = mock_dependencies(&[]);

    CONFIG
        .save(
            &mut deps.storage,
            &Config {
                owner: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("owner0000").to_string())
                    .unwrap(),
                pair_code_id: 321u64,
                burn_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("burnaddr0000").to_string())
                    .unwrap(),
                fee_wallet_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("feeaddr0000").to_string())
                    .unwrap(),
            },
        )
        .unwrap();

    deps.querier.with_token_balances(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        &[
            (
                &deps.api.addr_make("asset0000").to_string(),
                &Uint128::from(100u128),
            ),
            (
                &deps.api.addr_make("asset0001").to_string(),
                &Uint128::from(100u128),
            ),
        ],
    )]);

    let assets = [
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            amount: Uint128::zero(),
        },
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0001").to_string(),
            },
            amount: Uint128::zero(),
        },
    ];

    let raw_assets = [
        assets[0].to_raw(deps.as_ref().api).unwrap(),
        assets[1].to_raw(deps.as_ref().api).unwrap(),
    ];

    let raw_infos = [
        assets[0].info.to_raw(deps.as_ref().api).unwrap(),
        assets[1].info.to_raw(deps.as_ref().api).unwrap(),
    ];

    let pair_key = pair_key(&raw_infos);
    TMP_PAIR_INFO
        .save(
            &mut deps.storage,
            &TmpPairInfo {
                assets: raw_assets,
                pair_key,
                sender: deps.api.addr_make("addr0000"),
                asset_decimals: [8u8, 8u8],
            },
        )
        .unwrap();

    let expected = MsgInstantiateContractResponse {
        address: deps.api.addr_make("pair0000").to_string(),
        data: vec![],
        special_fields: Default::default(),
    };
    let expected_bytes = expected.write_to_bytes().unwrap();
    println!("Expected bytes: {}", hex::encode(expected_bytes.clone()));

    #[allow(deprecated)]
    let reply_msg = Reply {
        id: 1,
        payload: Binary::default(),
        gas_used: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None, // deprecated, so leave it as None
            msg_responses: vec![MsgResponse {
                type_url: "".to_string(), // or some appropriate type_url if needed
                value: Binary::from(expected_bytes.clone()),
            }],
        }),
    };

    let asset_infos = [
        AssetInfo::Token {
            contract_addr: deps.api.addr_make("asset0000").to_string(),
        },
        AssetInfo::Token {
            contract_addr: deps.api.addr_make("asset0001").to_string(),
        },
    ];

    // register choice pair querier
    deps.querier.with_choice_factory(
        &[(
            &deps.api.addr_make("pair0000").to_string(),
            &PairInfo {
                asset_infos,
                contract_addr: deps.api.addr_make("pair0000").to_string(),
                liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                asset_decimals: [8u8, 8u8],
                burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
            },
        )],
        &[],
    );

    let res = reply(deps.as_mut(), mock_env(), reply_msg).unwrap();

    assert_eq!(res.messages.len(), 0);
    assert_eq!(
        res.attributes[0],
        attr("pair_contract_addr", deps.api.addr_make("pair0000"))
    );
    assert_eq!(
        res.attributes[1],
        attr("liquidity_token_addr", deps.api.addr_make("liquidity0000"))
    );
}

#[test]
fn reply_create_pair_with_provide() {
    let mut deps = mock_dependencies(&[]);

    CONFIG
        .save(
            &mut deps.storage,
            &Config {
                owner: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("owner0000").to_string())
                    .unwrap(),
                pair_code_id: 321u64,
                burn_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("burnaddr0000").to_string())
                    .unwrap(),
                fee_wallet_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("feeaddr0000").to_string())
                    .unwrap(),
            },
        )
        .unwrap();

    deps.querier
        .with_balance(&[(&MOCK_CONTRACT_ADDR.to_string(), coins(100u128, "inj"))]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("pair0000").to_string(),
        &[(
            &deps.api.addr_make("asset0000").to_string(),
            &Uint128::from(100u128),
        )],
    )]);

    let assets = [
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(100u128),
        },
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            amount: Uint128::from(100u128),
        },
    ];

    let raw_assets = [
        assets[0].to_raw(deps.as_ref().api).unwrap(),
        assets[1].to_raw(deps.as_ref().api).unwrap(),
    ];

    let raw_infos = [
        assets[0].info.to_raw(deps.as_ref().api).unwrap(),
        assets[1].info.to_raw(deps.as_ref().api).unwrap(),
    ];

    let pair_key = pair_key(&raw_infos);
    TMP_PAIR_INFO
        .save(
            &mut deps.storage,
            &TmpPairInfo {
                assets: raw_assets,
                pair_key,
                sender: deps.api.addr_make("addr0000"),
                asset_decimals: [18u8, 8u8],
            },
        )
        .unwrap();

    let expected = MsgInstantiateContractResponse {
        address: deps.api.addr_make("pair0000").to_string(),
        data: vec![],
        special_fields: Default::default(),
    };
    let expected_bytes = expected.write_to_bytes().unwrap();
    println!("Expected bytes: {}", hex::encode(expected_bytes.clone()));

    #[allow(deprecated)]
    let reply_msg = Reply {
        id: 1,
        payload: Binary::default(),
        gas_used: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None, // deprecated, so leave it as None
            msg_responses: vec![MsgResponse {
                type_url: "".to_string(), // or some appropriate type_url if needed
                value: Binary::from(expected_bytes.clone()),
            }],
        }),
    };

    let asset_infos = [
        AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
        AssetInfo::Token {
            contract_addr: deps.api.addr_make("asset0000").to_string(),
        },
    ];

    // register choice pair querier
    deps.querier.with_choice_factory(
        &[(
            &deps.api.addr_make("pair0000").to_string(),
            &PairInfo {
                asset_infos,
                contract_addr: deps.api.addr_make("pair0000").to_string(),
                liquidity_token: deps.api.addr_make("liquidity0000").to_string(),
                asset_decimals: [18u8, 8u8],
                burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
                fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
            },
        )],
        &[("inj".to_string(), 18u8)],
    );

    let res = reply(deps.as_mut(), mock_env(), reply_msg).unwrap();

    assert_eq!(res.messages.len(), 3);
    assert_eq!(
        res.messages[0],
        SubMsg {
            id: 0,
            payload: Binary::default(),
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                    spender: deps.api.addr_make("pair0000").to_string(),
                    amount: Uint128::from(100u128),
                    expires: None,
                })
                .unwrap(),
                funds: vec![],
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never,
        }
    );
    assert_eq!(
        res.messages[1],
        SubMsg {
            id: 0,
            payload: Binary::default(),
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                    owner: deps.api.addr_make("addr0000").to_string(),
                    amount: Uint128::from(100u128),
                    recipient: MOCK_CONTRACT_ADDR.to_string(),
                })
                .unwrap(),
                funds: vec![],
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never,
        }
    );
    assert_eq!(
        res.messages[2],
        SubMsg {
            id: 0,
            payload: Binary::default(),
            msg: CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: deps.api.addr_make("pair0000").to_string(),
                msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
                    assets,
                    receiver: Some(deps.api.addr_make("addr0000").to_string()),
                    deadline: None,
                    slippage_tolerance: None,
                })
                .unwrap(),
                funds: coins(100u128, "inj".to_string()),
            }),
            gas_limit: None,
            reply_on: ReplyOn::Never,
        }
    );
    assert_eq!(
        res.attributes[0],
        attr("pair_contract_addr", deps.api.addr_make("pair0000"))
    );
    assert_eq!(
        res.attributes[1],
        attr("liquidity_token_addr", deps.api.addr_make("liquidity0000"))
    );
}

#[test]
fn failed_reply_with_unknown_id() {
    let mut deps = mock_dependencies(&[]);

    #[allow(deprecated)]
    let res = reply(
        deps.as_mut(),
        mock_env(),
        Reply {
            id: 9,
            payload: Binary::default(),
            gas_used: 0,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![],
                data: None, // deprecated, so leave it as None
                msg_responses: vec![MsgResponse {
                    type_url: "".to_string(), // or some appropriate type_url if needed
                    value: Binary::from(vec![]),
                }],
            }),
        },
    );

    assert_eq!(res, Err(StdError::generic_err("invalid reply msg")))
}

#[test]
fn normal_add_allow_native_token() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::AddNativeTokenDecimals {
        denom: "inj".to_string(),
        decimals: 6u8,
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg).unwrap(),
        Response::new().add_attributes(vec![
            ("action", "add_allow_native_token"),
            ("denom", "inj"),
            ("decimals", "6"),
        ])
    );

    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::NativeTokenDecimals {
            denom: "inj".to_string(),
        },
    )
    .unwrap();
    let res: NativeTokenDecimalsResponse = from_json(&res).unwrap();
    assert_eq!(6u8, res.decimals)
}

#[test]
fn failed_add_allow_native_token_with_non_admin() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::AddNativeTokenDecimals {
        denom: "inj".to_string(),
        decimals: 6u8,
    };

    let info = message_info(&deps.api.addr_make("noadmin"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg),
        Err(StdError::generic_err("unauthorized"))
    );
}

#[test]
fn failed_add_allow_native_token_with_zero_factory_balance() {
    let mut deps = mock_dependencies(&[coin(0u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::AddNativeTokenDecimals {
        denom: "inj".to_string(),
        decimals: 6u8,
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg),
        Err(StdError::generic_err(
            "a balance greater than zero is required by the factory for verification",
        ))
    );
}

#[test]
fn append_add_allow_native_token_with_already_exist_token() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::AddNativeTokenDecimals {
        denom: "inj".to_string(),

        decimals: 6u8,
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap();

    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::NativeTokenDecimals {
            denom: "inj".to_string(),
        },
    )
    .unwrap();
    let res: NativeTokenDecimalsResponse = from_json(&res).unwrap();
    assert_eq!(6u8, res.decimals);

    let msg = ExecuteMsg::AddNativeTokenDecimals {
        denom: "inj".to_string(),
        decimals: 7u8,
    };

    execute(deps.as_mut(), mock_env(), info, msg).unwrap();

    let res = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::NativeTokenDecimals {
            denom: "inj".to_string(),
        },
    )
    .unwrap();
    let res: NativeTokenDecimalsResponse = from_json(&res).unwrap();
    assert_eq!(7u8, res.decimals)
}

#[test]
fn normal_migrate_pair() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::MigratePair {
        code_id: Some(123u64),
        contract: "contract0000".to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg).unwrap(),
        Response::new().add_message(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: "contract0000".to_string(),
            new_code_id: 123u64,
            msg: to_json_binary(&PairMigrateMsg {}).unwrap(),
        })),
    );
}

#[test]
fn normal_migrate_pair_with_none_code_id_will_config_code_id() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::MigratePair {
        code_id: None,
        contract: "contract0000".to_string(),
    };

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg).unwrap(),
        Response::new().add_message(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: "contract0000".to_string(),
            new_code_id: 321u64,
            msg: to_json_binary(&PairMigrateMsg {}).unwrap(),
        })),
    );
}

#[test]
fn failed_migrate_pair_with_no_admin() {
    let mut deps = mock_dependencies(&[coin(1u128, "inj".to_string())]);
    deps = init(deps);

    let msg = ExecuteMsg::MigratePair {
        code_id: None,
        contract: "contract0000".to_string(),
    };

    let info = message_info(&deps.api.addr_make("noadmin"), &[]);

    assert_eq!(
        execute(deps.as_mut(), mock_env(), info, msg),
        Err(StdError::generic_err("unauthorized")),
    );
}

#[test]
fn test_execute_add_native_token_decimals_factory() {
    // Set up dependencies with a bank balance for the factory token denom.
    // We want the bank to have a nonzero balance for the denom "factory/cosmwasm1owneraddr/lp"
    let mut deps = mock_dependencies(&[coin(
        1000u128,
        format!("factory/{}/{}", MOCK_CONTRACT_ADDR, "lp"),
    )]);

    CONFIG
        .save(
            &mut deps.storage,
            &Config {
                owner: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("owner0000").to_string())
                    .unwrap(),
                pair_code_id: 321u64,
                burn_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("burnaddr0000").to_string())
                    .unwrap(),
                fee_wallet_address: deps
                    .api
                    .addr_canonicalize(&deps.api.addr_make("feeaddr0000").to_string())
                    .unwrap(),
            },
        )
        .unwrap();

    // We'll test with a factory denom.
    let valid_denom = format!("factory/{}/{}", MOCK_CONTRACT_ADDR, "lp");
    let decimals = 6u8;

    // Create an environment where the contract address is MOCK_CONTRACT_ADDR,
    // so that bank queries will return the correct balance.
    let env = mock_env();

    // Test case 1: Authorized sender (matches owner in denom)
    let info = message_info(&deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), &[]);
    let res = execute_add_native_token_decimals(
        deps.as_mut(),
        env.clone(),
        info.clone(),
        valid_denom.clone(),
        decimals,
    )
    .unwrap();
    println!("Response attributes: {:?}", res.attributes);
    // Check that the response has the expected attributes.
    assert!(res
        .attributes
        .iter()
        .any(|attr| attr.key == "action" && attr.value == "add_allow_native_token"));
    assert!(res
        .attributes
        .iter()
        .any(|attr| attr.key == "denom" && attr.value == valid_denom));
    assert!(res
        .attributes
        .iter()
        .any(|attr| attr.key == "decimals" && &attr.value == &decimals.to_string()));

    // Test case 2: Unauthorized sender (does not match owner in denom)
    let bad_info = message_info(&deps.api.addr_make("cosmwasm1otheraddr"), &[]);
    let res_err = execute_add_native_token_decimals(
        deps.as_mut(),
        env.clone(),
        bad_info,
        valid_denom.clone(),
        decimals,
    );
    match res_err {
        Err(StdError::GenericErr { msg, .. }) => {
            assert_eq!(msg, "unauthorized: sender does not match owner in denom")
        }
        _ => panic!("Expected unauthorized error"),
    }
}

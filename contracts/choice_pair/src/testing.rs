use crate::contract::{
    assert_deadline, assert_max_spread, assert_minimum_assets, execute, instantiate,
    query_pair_info, query_pool, query_reverse_simulation, query_simulation,
};
use crate::error::ContractError;
use std::str::FromStr;

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::mock_querier::mock_dependencies;
use choice::pair::{
    Cw20HookMsg, ExecuteMsg, InstantiateMsg, PoolResponse, ReverseSimulationResponse,
    SimulationResponse,
};
use cosmwasm_std::testing::{message_info, mock_env, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    attr, coins, to_json_binary, Addr, Api, BankMsg, Binary, Coin, CosmosMsg, Decimal, Decimal256, ReplyOn, Response, StdError, SubMsg, Uint128, Uint256, WasmMsg
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use injective_cosmwasm::msg::{create_new_denom_msg, create_set_token_metadata_msg};
use injective_cosmwasm::InjectiveMsgWrapper;
use injective_cosmwasm::{create_burn_tokens_msg, create_mint_tokens_msg};
use std::convert::TryInto;

#[test]
fn proper_initialization() {
    let mut deps = mock_dependencies(&[]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    // we can just call .unwrap() to assert this was a success
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

    assert_eq!(
        res.messages,
        vec![
            SubMsg {
                id: 0,
                payload: Binary::default(),
                gas_limit: None,
                reply_on: ReplyOn::Never,
                msg: create_new_denom_msg(env.contract.address.to_string(), "lp".to_string())
            },
            SubMsg {
                id: 0,
                payload: Binary::default(),
                gas_limit: None,
                reply_on: ReplyOn::Never,
                msg: create_set_token_metadata_msg(
                    format!("factory/{}/{}", env.contract.address, "lp"),
                    "choice liquidity token".to_string(),
                    "uLP".to_string(),
                    6,
                )
            }
        ]
    );

    // it worked, let's query the state
    let pair_info: PairInfo = query_pair_info(deps.as_ref()).unwrap();

    // Compute the expected LP denom.
    let expected_lp_denom = format!("factory/{}/{}", env.contract.address, "lp");

    // Assert the liquidity_token in state matches the expected LP denom.
    assert_eq!(expected_lp_denom, pair_info.liquidity_token);

    // Other assertions remain the same.
    assert_eq!(
        pair_info.asset_infos,
        [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string()
            }
        ]
    );
    assert_eq!(
        deps.api.addr_make("burnaddr0000").to_string(),
        pair_info.burn_address.as_str()
    );
    assert_eq!(
        deps.api.addr_make("feeaddr0000").to_string(),
        pair_info.fee_wallet_address.as_str()
    );
}

#[test]
fn provide_liquidity() {
    let mut deps = mock_dependencies(&[]);

    deps.querier
        .with_token_balances(&[(&deps.api.addr_make("asset0000").to_string(), &[])]);

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1_100u128),
        }],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::zero(),
    )]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();

    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    // should raise MinimumLiquidityAmountError with insufficient initial liquidity
    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(1u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(1u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: None,
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1u128),
        }],
    );

    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();

    match res {
        ContractError::MinimumLiquidityAmountError {
            min_lp_token,
            given_lp,
        } => {
            assert_eq!(min_lp_token, "1000");
            assert_eq!(given_lp, "1");
        }
        _ => panic!("Must return MinimumLiquidityAmountError"),
    }

    // successfully provide liquidity for the exist pool
    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(1_100u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(1_100u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: None,
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1_100u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap();
    let liquidity_to_contract_msg = res.messages.get(0).expect("no message");
    let transfer_from_msg = res.messages.get(1).expect("no message");
    let mint_msg = res.messages.get(2).expect("no message");

    // Build the expected liquidity-to-contract message
    let expected_liquidity_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender
        Coin {
            // amount minted is 1_000 with the LP denom as defined in your state.
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(1_000u128),
        },
        MOCK_CONTRACT_ADDR.to_string(), // mint_to
    ));

    assert_eq!(liquidity_to_contract_msg, &expected_liquidity_msg);

    let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_make("asset0000").to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
            owner: deps.api.addr_make("addr0000").to_string(),
            recipient: MOCK_CONTRACT_ADDR.to_string(),
            amount: Uint128::from(1_100u128),
        })
        .unwrap(),
        funds: vec![],
    }));

    assert_eq!(transfer_from_msg, &expected_transfer_msg);

    let expected_mint_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender for minting
        Coin {
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(100u128),
        },
        deps.api.addr_make("addr0000").to_string(), // mint_to recipient
    ));

    assert_eq!(mint_msg, &expected_mint_msg);

    // providing liquidity with a ratio exceeding the specified slippage tolerance
    // should return MaxSlippageAssertion
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: Uint128::from(200u128 + 200u128),
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: Uint128::from(1_100u128),
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(200u128))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::from(1_100u128),
    )]);

    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(100u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(200u128),
            },
        ],
        receiver: Some(deps.api.addr_make("staking0000").to_string()), // try changing receiver
        deadline: None,
        slippage_tolerance: Some(Decimal::from_str("0.005").unwrap()),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(200u128),
        }],
    );

    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();

    match res {
        ContractError::MaxSlippageAssertion { .. } => (),
        _ => panic!("MaxSlippageAssertion should be raised"),
    }

    // providing liquidity at a rate that is not equal to the existing one
    // should refund the remained amount of one side's assets
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: Uint128::from(
                    100u128 + 200u128, /* user deposit must be pre-applied */
                ),
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: Uint128::from(100u128),
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(200u128))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::from(100u128),
    )]);

    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(100u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(100u128),
            },
        ],
        receiver: Some(deps.api.addr_make("staking0000").to_string()), // try changing receiver
        deadline: None,
        slippage_tolerance: Some(Decimal::from_str("0.05").unwrap()),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(100u128),
        }],
    );

    let res: Response<InjectiveMsgWrapper> = execute(deps.as_mut(), env, info, msg).unwrap();
    let transfer_from_msg = res.messages.get(0).expect("no message");
    let mint_msg = res.messages.get(1).expect("no message");

    let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_make("asset0000").to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
            owner: deps.api.addr_make("addr0000").to_string(),
            recipient: MOCK_CONTRACT_ADDR.to_string(),
            amount: Uint128::from(100u128),
        })
        .unwrap(),
        funds: vec![],
    }));

    assert_eq!(transfer_from_msg, &expected_transfer_msg);

    let expected_mint_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender for minting
        Coin {
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(50u128),
        },
        deps.api.addr_make("staking0000").to_string(), // mint_to recipient
    ));

    assert_eq!(mint_msg, &expected_mint_msg);

    // check wrong argument
    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(100u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(50u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: Some(Decimal::from_str("0.005").unwrap()),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(100u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();
    match res {
        ContractError::Std(StdError::GenericErr { msg, .. }) => assert_eq!(
            msg,
            "Native token balance mismatch between the argument and the transferred".to_string()
        ),
        _ => panic!("Must return generic error"),
    }

    // initialize token balance to 1:1
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: Uint128::from(100u128 + 98u128 /* user deposit must be pre-applied */),
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: Uint128::from(100u128),
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(100u128))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::from(100u128),
    )]);

    // successfully provide liquidity, and refund remain asset
    let msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(100u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(98u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: Some(Decimal::from_str("0.05").unwrap()),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0001"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(98u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    let transfer_from_msg = res.messages.get(0).expect("no message");
    let mint_msg = res.messages.get(1).expect("no message");

    let expected_transfer_msg = SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_make("asset0000").to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
            owner: deps.api.addr_make("addr0001").to_string(),
            recipient: MOCK_CONTRACT_ADDR.to_string(),
            amount: Uint128::from(98u128),
        })
        .unwrap(),
        funds: vec![],
    }));

    assert_eq!(transfer_from_msg, &expected_transfer_msg);

    let expected_mint_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender for minting
        Coin {
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(98u128),
        },
        deps.api.addr_make("addr0001").to_string(), // mint_to recipient
    ));

    assert_eq!(mint_msg, &expected_mint_msg);
}

#[test]
fn withdraw_liquidity() {
    let mut deps = mock_dependencies(&[]);

    deps.querier.with_balance(&[
        (
            &deps.api.addr_make("addr0000").to_string(),
            vec![Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: Uint128::from(100u128),
            }],
        ),
        (
            &MOCK_CONTRACT_ADDR.to_string(),
            vec![Coin {
                denom: "uusd".to_string(),
                amount: Uint128::from(100u128),
            }],
        ),
    ]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &Uint128::from(100u128))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::from(100u128),
    )]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    // failed to withdraw liquidity, did not pass funds
    let msg = ExecuteMsg::WithdrawLiquidity {
        min_assets: None,
        deadline: None,
        amount: Uint128::from(100u128),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[], // empty funds
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();

    assert_eq!(res, ContractError::InvalidLiquidityFunds {});

    // withdraw liquidity, passing lp funds
    let msg = ExecuteMsg::WithdrawLiquidity {
        min_assets: None,
        deadline: None,
        amount: Uint128::from(100u128),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            // pass lp denom with exact amount to burn
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(100u128),
        }],
    );

    let res = execute(deps.as_mut(), env, info, msg).unwrap();

    let log_withdrawn_share = res.attributes.get(2).expect("no log");
    let log_refund_assets = res.attributes.get(3).expect("no log");
    let msg_refund_0 = res.messages.get(0).expect("no message");
    let msg_refund_1 = res.messages.get(1).expect("no message");
    let msg_burn_liquidity = res.messages.get(2).expect("no message");

    assert_eq!(
        msg_refund_0,
        &SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: deps.api.addr_make("addr0000").to_string(),
            amount: vec![Coin {
                denom: "uusd".to_string(),
                amount: Uint128::from(100u128),
            }],
        }))
    );

    assert_eq!(
        msg_refund_1,
        &SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("asset0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: Uint128::from(100u128),
            })
            .unwrap(),
            funds: vec![],
        }))
    );

    let expected_burn_msg = SubMsg::new(create_burn_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender for burning
        Coin {
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(100u128),
        },
    ));

    assert_eq!(msg_burn_liquidity, &expected_burn_msg);

    assert_eq!(
        log_withdrawn_share,
        &attr("withdrawn_share", 100u128.to_string())
    );
    assert_eq!(
        log_refund_assets,
        &attr(
            "refund_assets",
            format!(
                "100uusd, 100{}",
                deps.api.addr_make("asset0000").to_string()
            )
        )
    );

    // withdraw liquidity with assert min_assets
    let msg = ExecuteMsg::WithdrawLiquidity {
        min_assets: Some([
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(1000u128),
            },
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::zero(),
            },
        ]),
        deadline: None,
        amount: Uint128::from(100u128),
    };

    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
            amount: Uint128::from(100u128),
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();

    assert_eq!(
        res,
        ContractError::MinAmountAssertion {
            min_asset: "1000uusd".to_string(),
            asset: "100uusd".to_string()
        }
    );

    // failed to withdraw liquidity due to deadline
    let msg = ExecuteMsg::WithdrawLiquidity {
        min_assets: None,
        deadline: Some(100u64),
        amount: Uint128::from(100u128),
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let err = execute(deps.as_mut(), env, info, msg).unwrap_err();
    assert_eq!(err, ContractError::ExpiredDeadline {})
}

#[test]
fn try_native_to_token() {
    let total_share = Uint128::from(30000000000u128);
    let asset_pool_amount = Uint128::from(20000000000u128);
    let collateral_pool_amount = Uint128::from(30000000000u128);
    let exchange_rate: Decimal = Decimal::from_ratio(asset_pool_amount, collateral_pool_amount);
    let offer_amount = Uint128::from(1500000000u128);

    let mut deps = mock_dependencies(&[]);

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: collateral_pool_amount + offer_amount,
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: total_share,
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &asset_pool_amount)],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        total_share,
    )]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    // normal swap
    let msg = ExecuteMsg::Swap {
        offer_asset: Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: offer_amount,
        },
        belief_price: None,
        max_spread: None,
        to: None,
        deadline: None,
    };
    let env = mock_env();
    let info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: offer_amount,
        }],
    );
    let res = execute(deps.as_mut(), env, info, msg).unwrap();
    let msg_transfer = res.messages.get(0).expect("no message");

    // current price is 1.5, so expected return without spread is 1000
    // 952.380952 = 20000 - 20000 * 30000 / (30000 + 1500)
    let expected_ret_amount = Uint128::from(952_380_952u128);
    let expected_spread_amount = (offer_amount.mul_floor(exchange_rate))
        .checked_sub(expected_ret_amount)
        .unwrap();
    let expected_commission_amount =
        expected_ret_amount.multiply_ratio(3u128, 1000u128) + Uint128::from(1u8); // 0.3%, round up

    let expected_fee_wallet_amount = expected_commission_amount.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let expected_burn_amount = expected_commission_amount.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let expected_lp_amount = expected_commission_amount
        .checked_sub(expected_fee_wallet_amount)
        .unwrap()
        .checked_sub(expected_burn_amount)
        .unwrap();

    let expected_return_amount = expected_ret_amount
        .checked_sub(expected_commission_amount)
        .unwrap();
    // check simulation res
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: "uusd".to_string(),
            amount: collateral_pool_amount, /* user deposit must be pre-applied */
        }],
    )]);

    let simulation_res: SimulationResponse = query_simulation(
        deps.as_ref(),
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: offer_amount,
        },
    )
    .unwrap();
    assert_eq!(expected_return_amount, simulation_res.return_amount);
    assert_eq!(expected_commission_amount, simulation_res.commission_amount);
    assert_eq!(expected_spread_amount, simulation_res.spread_amount);

    // check reverse simulation res
    let reverse_simulation_res: ReverseSimulationResponse = query_reverse_simulation(
        deps.as_ref(),
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            amount: expected_return_amount,
        },
    )
    .unwrap();

    assert!(
        (offer_amount.u128() as i128 - reverse_simulation_res.offer_amount.u128() as i128).abs()
            < 3i128
    );
    assert!(
        (expected_commission_amount.u128() as i128
            - reverse_simulation_res.commission_amount.u128() as i128)
            .abs()
            < 3i128
    );
    assert!(
        (expected_spread_amount.u128() as i128
            - reverse_simulation_res.spread_amount.u128() as i128)
            .abs()
            < 3i128
    );

    let expected_offer_pool_post = collateral_pool_amount + offer_amount;
    let expected_ask_pool_post = asset_pool_amount
        - expected_return_amount
        - expected_fee_wallet_amount
        - expected_burn_amount;

    assert_eq!(
        res.attributes,
        vec![
            attr("action", "swap"),
            attr("sender", deps.api.addr_make("addr0000")),
            attr("receiver", deps.api.addr_make("addr0000")),
            attr("offer_asset", "uusd"),
            attr("ask_asset", deps.api.addr_make("asset0000")),
            attr("offer_amount", offer_amount.to_string()),
            attr("return_amount", expected_return_amount.to_string()),
            attr("spread_amount", expected_spread_amount.to_string()),
            attr("commission_amount", expected_commission_amount.to_string()),
            attr("burn_amount", expected_burn_amount.to_string()),
            attr("fee_wallet_amount", expected_fee_wallet_amount.to_string()),
            attr("pool_amount", expected_lp_amount.to_string()),
            attr("offer_pool_balance", expected_offer_pool_post.to_string()),
            attr("ask_pool_balance", expected_ask_pool_post.to_string()),
        ]
    );

    assert_eq!(
        &SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: deps.api.addr_make("asset0000").to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: deps.api.addr_make("addr0000").to_string(),
                amount: expected_return_amount,
            })
            .unwrap(),
            funds: vec![],
        })),
        msg_transfer,
    );
}

#[test]
fn try_token_to_native() {
    let total_share = Uint128::from(20000000000u128);
    let asset_pool_amount = Uint128::from(30000000000u128);
    let collateral_pool_amount = Uint128::from(20000000000u128);
    let exchange_rate = Decimal::from_ratio(collateral_pool_amount, asset_pool_amount);
    let offer_amount = Uint128::from(1500000000u128);

    let mut deps = mock_dependencies(&[]);

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: collateral_pool_amount,
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: total_share,
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(
            &MOCK_CONTRACT_ADDR.to_string(),
            &(asset_pool_amount + offer_amount),
        )],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        total_share,
    )]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [8u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    // unauthorized access; can not execute swap directly for token swap
    let msg = ExecuteMsg::Swap {
        offer_asset: Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
            amount: offer_amount,
        },
        belief_price: None,
        max_spread: None,
        to: None,
        deadline: None,
    };
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();

    match res {
        ContractError::Unauthorized {} => (),
        _ => panic!("DO NOT ENTER HERE"),
    }

    // normal sell
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: offer_amount,
        msg: to_json_binary(&Cw20HookMsg::Swap {
            belief_price: None,
            max_spread: None,
            to: None,
            deadline: None,
        })
        .unwrap(),
    });
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("asset0000"), &[]);

    let res = execute(deps.as_mut(), env, info, msg).unwrap();
    let msg_transfer = res.messages.get(0).expect("no message");

    // current price is 1.5, so expected return without spread is 1000
    // 952.380952 = 20000 - 20000 * 30000 / (30000 + 1500)
    let expected_ret_amount = Uint128::from(952_380_952u128);
    let expected_spread_amount = (offer_amount.mul_floor(exchange_rate))
        .checked_sub(expected_ret_amount)
        .unwrap();
    let expected_commission_amount =
        expected_ret_amount.multiply_ratio(3u128, 1000u128) + Uint128::from(1u8); // 0.3%, round up
    let expected_return_amount = expected_ret_amount
        .checked_sub(expected_commission_amount)
        .unwrap();

    let expected_fee_wallet_amount = expected_commission_amount.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let expected_burn_amount = expected_commission_amount.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let expected_lp_amount = expected_commission_amount
        .checked_sub(expected_fee_wallet_amount)
        .unwrap()
        .checked_sub(expected_burn_amount)
        .unwrap();

    // check simulation res
    // return asset token balance as normal

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: collateral_pool_amount,
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: total_share,
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &(asset_pool_amount))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        total_share,
    )]);

    let simulation_res: SimulationResponse = query_simulation(
        deps.as_ref(),
        Asset {
            amount: offer_amount,
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        },
    )
    .unwrap();
    assert_eq!(expected_return_amount, simulation_res.return_amount);
    assert_eq!(expected_commission_amount, simulation_res.commission_amount);
    assert_eq!(expected_spread_amount, simulation_res.spread_amount);

    // check reverse simulation res
    let reverse_simulation_res: ReverseSimulationResponse = query_reverse_simulation(
        deps.as_ref(),
        Asset {
            amount: expected_return_amount,
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
        },
    )
    .unwrap();
    assert!(
        (offer_amount.u128() as i128 - reverse_simulation_res.offer_amount.u128() as i128).abs()
            < 3i128
    );
    assert!(
        (expected_commission_amount.u128() as i128
            - reverse_simulation_res.commission_amount.u128() as i128)
            .abs()
            < 3i128
    );
    assert!(
        (expected_spread_amount.u128() as i128
            - reverse_simulation_res.spread_amount.u128() as i128)
            .abs()
            < 3i128
    );

    let expected_offer_pool_post = asset_pool_amount + offer_amount;
    let expected_ask_pool_post = collateral_pool_amount
        - expected_return_amount
        - expected_fee_wallet_amount
        - expected_burn_amount;

    assert_eq!(
        res.attributes,
        vec![
            attr("action", "swap"),
            attr("sender", deps.api.addr_make("addr0000")),
            attr("receiver", deps.api.addr_make("addr0000")),
            attr("offer_asset", deps.api.addr_make("asset0000")),
            attr("ask_asset", "uusd"),
            attr("offer_amount", offer_amount.to_string()),
            attr("return_amount", expected_return_amount.to_string()),
            attr("spread_amount", expected_spread_amount.to_string()),
            attr("commission_amount", expected_commission_amount.to_string()),
            attr("burn_amount", expected_burn_amount.to_string()),
            attr("fee_wallet_amount", expected_fee_wallet_amount.to_string()),
            attr("pool_amount", expected_lp_amount.to_string()),
            attr("offer_pool_balance", expected_offer_pool_post.to_string()),
            attr("ask_pool_balance", expected_ask_pool_post.to_string()),
        ]
    );

    assert_eq!(
        &SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: deps.api.addr_make("addr0000").to_string(),
            amount: vec![Coin {
                denom: "uusd".to_string(),
                amount: expected_return_amount
            }],
        })),
        msg_transfer,
    );

    // failed due to non asset token contract try to execute sell
    let msg = ExecuteMsg::Receive(Cw20ReceiveMsg {
        sender: deps.api.addr_make("addr0000").to_string(),
        amount: offer_amount,
        msg: to_json_binary(&Cw20HookMsg::Swap {
            belief_price: None,
            max_spread: None,
            to: None,
            deadline: None,
        })
        .unwrap(),
    });
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("liquidity0000"), &[]);
    let res = execute(deps.as_mut(), env, info, msg).unwrap_err();
    match res {
        ContractError::Unauthorized {} => (),
        _ => panic!("DO NOT ENTER HERE"),
    }
}

#[test]
fn test_max_spread() {
    let offer_asset_info = AssetInfo::NativeToken {
        denom: "offer_asset".to_string(),
    };
    let ask_asset_info = AssetInfo::NativeToken {
        denom: "ask_asset_info".to_string(),
    };

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info.clone(),
            amount: Uint128::from(1200000000u128),
        },
        Asset {
            info: ask_asset_info.clone(),
            amount: Uint128::from(989999u128),
        },
        Uint128::zero(),
        6u8,
        6u8,
    )
    .unwrap_err();

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info.clone(),
            amount: Uint128::from(1200000000u128),
        },
        Asset {
            info: ask_asset_info.clone(),
            amount: Uint128::from(990000u128),
        },
        Uint128::zero(),
        6u8,
        6u8,
    )
    .unwrap();

    assert_max_spread(
        None,
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info.clone(),
            amount: Uint128::zero(),
        },
        Asset {
            info: ask_asset_info.clone(),
            amount: Uint128::from(989999u128),
        },
        Uint128::from(10001u128),
        6u8,
        6u8,
    )
    .unwrap_err();

    assert_max_spread(
        None,
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info,
            amount: Uint128::zero(),
        },
        Asset {
            info: ask_asset_info,
            amount: Uint128::from(990000u128),
        },
        Uint128::from(10000u128),
        6u8,
        6u8,
    )
    .unwrap();
}

#[test]
fn test_max_spread_with_diff_decimal() {
    let token_addr = "ask_asset_info".to_string();

    let mut deps = mock_dependencies(&[]);
    deps.querier.with_token_balances(&[(
        &token_addr,
        &[(
            &MOCK_CONTRACT_ADDR.to_string(),
            &Uint128::from(10000000000u64),
        )],
    )]);
    let offer_asset_info = AssetInfo::NativeToken {
        denom: "offer_asset".to_string(),
    };
    let ask_asset_info = AssetInfo::Token {
        contract_addr: token_addr.to_string(),
    };

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info.clone(),
            amount: Uint128::from(1200000000u128),
        },
        Asset {
            info: ask_asset_info.clone(),
            amount: Uint128::from(100000000u128),
        },
        Uint128::zero(),
        6u8,
        8u8,
    )
    .unwrap();

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info,
            amount: Uint128::from(1200000000u128),
        },
        Asset {
            info: ask_asset_info,
            amount: Uint128::from(98999999u128),
        },
        Uint128::zero(),
        6u8,
        8u8,
    )
    .unwrap_err();

    let offer_asset_info = AssetInfo::Token {
        contract_addr: token_addr,
    };
    let ask_asset_info = AssetInfo::NativeToken {
        denom: "offer_asset".to_string(),
    };

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info.clone(),
            amount: Uint128::from(120000000000u128),
        },
        Asset {
            info: ask_asset_info.clone(),
            amount: Uint128::from(1000000u128),
        },
        Uint128::zero(),
        8u8,
        6u8,
    )
    .unwrap();

    assert_max_spread(
        Some(Decimal::from_ratio(1200u128, 1u128)),
        Some(Decimal::percent(1)),
        Asset {
            info: offer_asset_info,
            amount: Uint128::from(120000000000u128),
        },
        Asset {
            info: ask_asset_info,
            amount: Uint128::from(989999u128),
        },
        Uint128::zero(),
        8u8,
        6u8,
    )
    .unwrap_err();
}

#[test]
fn test_query_pool() {
    let total_share_amount = Uint128::from(111u128);
    let asset_0_amount = Uint128::from(222u128);
    let asset_1_amount = Uint128::from(333u128);

    let mut deps = mock_dependencies(&[]);

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "uusd".to_string(),
                amount: asset_0_amount,
            },
            Coin {
                denom: format!("factory/{}/{}", MOCK_CONTRACT_ADDR.to_string(), "lp"),
                amount: total_share_amount,
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(&MOCK_CONTRACT_ADDR.to_string(), &(asset_1_amount))],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        total_share_amount,
    )]);

    let msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(), // New field
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(), // New field
    };

    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    // we can just call .unwrap() to assert this was a success
    let _res = instantiate(deps.as_mut(), env, info, msg).unwrap();

    let res: PoolResponse = query_pool(deps.as_ref()).unwrap();

    assert_eq!(
        res.assets,
        [
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: asset_0_amount
            },
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: asset_1_amount
            }
        ]
    );
    assert_eq!(res.total_share, total_share_amount);
}

#[test]
fn test_assert_minimum_assets_with_equals() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ]);

    assert_minimum_assets(assets, minimum_assets).unwrap();
}

#[test]
fn test_assert_minimum_assets_with_normal() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(2u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(2u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ]);

    assert_minimum_assets(assets, minimum_assets).unwrap();
}

#[test]
fn test_assert_minimum_assets_with_less_all() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(2u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(2u128),
        },
    ]);

    let err = assert_minimum_assets(assets, minimum_assets).unwrap_err();
    assert_eq!(
        err,
        ContractError::MinAmountAssertion {
            min_asset: "2inj".to_string(),
            asset: "1inj".to_string()
        }
    )
}

#[test]
fn test_assert_minimum_assets_with_less_second_asset() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(2u128),
        },
    ]);

    let err = assert_minimum_assets(assets, minimum_assets).unwrap_err();
    assert_eq!(
        err,
        ContractError::MinAmountAssertion {
            min_asset: "2uusd".to_string(),
            asset: "1uusd".to_string()
        }
    )
}

#[test]
fn test_assert_minimum_assets_with_less_first_asset() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(2u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ]);

    let err = assert_minimum_assets(assets, minimum_assets).unwrap_err();
    assert_eq!(
        err,
        ContractError::MinAmountAssertion {
            min_asset: "2inj".to_string(),
            asset: "1inj".to_string()
        }
    )
}

#[test]
fn test_assert_minimum_assets_with_unsorted_less_first_asset() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(2u128),
        },
    ]);

    let err = assert_minimum_assets(assets, minimum_assets).unwrap_err();
    assert_eq!(
        err,
        ContractError::MinAmountAssertion {
            min_asset: "2inj".to_string(),
            asset: "1inj".to_string()
        }
    )
}

#[test]
fn test_assert_minimum_assets_with_unknown_asset() {
    let assets = vec![
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(2u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            amount: Uint128::from(2u128),
        },
    ];

    let minimum_assets = Some([
        Asset {
            info: AssetInfo::NativeToken {
                denom: "ukrw".to_string(),
            },
            amount: Uint128::from(1u128),
        },
        Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: Uint128::from(1u128),
        },
    ]);

    let err = assert_minimum_assets(assets, minimum_assets).unwrap_err();
    assert_eq!(
        err,
        ContractError::MinAmountAssertion {
            min_asset: "1ukrw".to_string(),
            asset: "0ukrw".to_string()
        }
    )
}

#[test]
fn test_assert_deadline_with_normal() {
    assert_deadline(5u64, Some(10u64)).unwrap();
}

#[test]
fn test_assert_deadline_with_expired() {
    let err = assert_deadline(10u64, Some(5u64)).unwrap_err();
    assert_eq!(err, ContractError::ExpiredDeadline {})
}

#[test]
fn test_assert_deadline_with_same() {
    let err = assert_deadline(10u64, Some(10u64)).unwrap_err();
    assert_eq!(err, ContractError::ExpiredDeadline {})
}

#[test]
fn test_assert_deadline_with_none() {
    assert_deadline(5u64, None).unwrap();
}

#[test]
fn test_initial_liquidity_provide() {
    use cosmwasm_std::{Coin, SubMsg, Uint128, Uint256};
    // Use your own instantiation functions/types.
    let mut deps = mock_dependencies(&[]);

    // Set up the token factory supply to zero for initial liquidity.
    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::zero(),
    )]);

    // Set up a native balance for the contract.
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1_000_000u128),
        }],
    )]);

    // Set up cw20 token balance for asset0000.
    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("asset0000").to_string(),
        &[(
            &MOCK_CONTRACT_ADDR.to_string(),
            &Uint128::from(1_000_000_000u128),
        )],
    )]);

    // Instantiate the contract with two assets: a native token and a cw20 token.
    let instantiate_msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "uusd".to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("asset0000").to_string(),
            },
        ],
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(),
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(),
    };
    let env = mock_env();
    let info = message_info(&deps.api.addr_make("addr0000"), &[]);
    let _ = instantiate(deps.as_mut(), env.clone(), info, instantiate_msg).unwrap();

    // Now, execute ProvideLiquidity with the deposits:
    // - cw20 token: 1,000,000,000 units
    // - native token: 1,000,000 units
    let provide_liquidity_msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: deps.api.addr_make("asset0000").to_string(),
                },
                amount: Uint128::from(1_000_000_000u128),
            },
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
                amount: Uint128::from(1_000_000u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: None,
    };

    // For native token deposit, the funds are passed along.
    let exec_env = mock_env();
    let exec_info = message_info(
        &deps.api.addr_make("addr0000"),
        &[Coin {
            denom: "uusd".to_string(),
            amount: Uint128::from(1_000_000u128),
        }],
    );
    let res = execute(deps.as_mut(), exec_env, exec_info, provide_liquidity_msg).unwrap();

    // --- Compute the expected LP token amount ---
    // The original code computes share as:
    //    share = sqrt( deposit0 * deposit1 )
    // where deposit0 = 1,000,000,000 and deposit1 = 1,000,000.
    // Note: Decimal256 uses fixed-point arithmetic (with 18 decimals).
    // Thus, we mimic that computation:
    let deposit0 = Uint256::from(1_000_000_000u128);
    let deposit1 = Uint256::from(1_000_000u128);
    let product = deposit0 * deposit1; // 1e15 in Uint256
                                       // Compute the square root as a Decimal256.
    let sqrt_decimal = Decimal256::from_ratio(product, Uint256::one()).sqrt();
    // Get the underlying scaled integer (typically 1e18 represents 1).
    let scaled_share = sqrt_decimal.atomics();
    // Remove the scaling factor (10^18) to get the plain integer share.
    let scaling_factor = Uint256::from(1_000_000_000_000_000_000u128);
    let plain_share: Uint128 = (scaled_share / scaling_factor).try_into().unwrap();
    // The contract then subtracts MINIMUM_LIQUIDITY_AMOUNT (assumed to be 1000).
    let expected_provider_lp = plain_share.checked_sub(Uint128::from(1000u128)).unwrap();

    // --- Verify the response messages ---
    // We expect three messages:
    // 1. A mint message that mints MINIMUM_LIQUIDITY_AMOUNT LP tokens to the contract (locking them forever).
    // 2. A transfer message to transfer cw20 tokens from the user to the contract.
    // 3. A mint message that mints (computed share - MINIMUM_LIQUIDITY_AMOUNT) LP tokens to the user.
    assert_eq!(res.messages.len(), 3);

    // Check that the third message is a mint message with the expected LP token amount.
    let mint_msg = res.messages.get(2).expect("no mint msg").clone();
    let expected_mint_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // sender (contract address)
        Coin {
            denom: format!("factory/{}/lp", MOCK_CONTRACT_ADDR.to_string()),
            amount: expected_provider_lp,
        },
        deps.api.addr_make("addr0000").to_string(), // mint_to (user)
    ));
    assert_eq!(mint_msg, expected_mint_msg);
}

#[test]
fn test_create_pair_simulated() {
    let mut deps = mock_dependencies(&[]);

    // Set the token factory supply to zero for the LP token.
    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        Uint128::zero(),
    )]);

    // Set up the native balance for the contract for the asset.
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: "token".to_string(),
            amount: Uint128::from(200000000000u128),
        }],
    )]);

    // The CW20 token asset – use the provided contract address.
    let cw20_addr = deps.api.addr_make("contr0000").to_string();
    deps.querier.with_token_balances(&[(
        &cw20_addr.to_string(),
        &[(
            &MOCK_CONTRACT_ADDR.to_string(),
            &Uint128::from(10000000000000000000000000u128),
        )],
    )]);

    // Instantiate the contract with two assets:
    // - Asset 0: Native token
    // - Asset 1: CW20 token.
    let instantiate_msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "token".to_string(),
            },
            AssetInfo::Token {
                contract_addr: cw20_addr.to_string(),
            },
        ],
        // Provide appropriate decimals. (Here we assume asset0 has 6 decimals and asset1 has 8.)
        asset_decimals: [6u8, 8u8],
        burn_address: deps.api.addr_make("burnaddr0000").to_string(),
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(),
    };
    let env = mock_env();
    let creator = deps.api.addr_make("creator");
    let info = message_info(&creator, &[]);
    let _ = instantiate(deps.as_mut(), env.clone(), info, instantiate_msg).unwrap();

    // Create the pair by executing a CreatePair message.
    // Assets:
    // - Native: 200000000000 
    // - CW20: 10000000000000000000000000
    let create_pair_msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info: AssetInfo::NativeToken {
                    denom: "token".to_string(),
                },
                amount: Uint128::from(200000000000u128),
            },
            Asset {
                info: AssetInfo::Token {
                    contract_addr: cw20_addr.to_string(),
                },
                amount: Uint128::from(10000000000000000000000000u128),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: None,
    };

    // For the native asset deposit, the funds are passed along.
    let exec_env = mock_env();
    let exec_info = message_info(
        &creator,
        &[Coin {
            denom: "token".to_string(),
            amount: Uint128::from(200000000000u128),
        }],
    );
    let res = execute(deps.as_mut(), exec_env, exec_info, create_pair_msg).unwrap();

    // --- Compute the expected LP token amount ---
    // The contract computes share as:
    //     share = sqrt( deposit_cw20 * deposit_native )
    // Where:
    //     deposit_cw20 = 10000000000000000000000000
    //     deposit_native = 200000000000
    let deposit0 = Uint256::from(10000000000000000000000000u128);
    let deposit1 = Uint256::from(200000000000u128);
    let product = deposit0 * deposit1;
    let sqrt_decimal = Decimal256::from_ratio(product, Uint256::one()).sqrt();
    let scaled_share = sqrt_decimal.atomics();
    // Remove the fixed-point scaling (assume 18 decimals, so divide by 1e18)
    let scaling_factor = Uint256::from(1_000_000_000_000_000_000u128);
    let plain_share: Uint128 = (scaled_share / scaling_factor).try_into().unwrap();
    // The contract subtracts a minimum liquidity amount (assumed here to be 1000).
    let expected_provider_lp = plain_share.checked_sub(Uint128::from(1000u128)).unwrap();

    // --- Verify the response messages ---
    // We expect three messages:
    // 1. A mint message for MINIMUM_LIQUIDITY_AMOUNT LP tokens to the contract.
    // 2. A transfer message moving CW20 tokens from the creator to the contract.
    // 3. A mint message that mints (computed share - MINIMUM_LIQUIDITY_AMOUNT) LP tokens to the creator.
    assert_eq!(res.messages.len(), 3);

    // Check that the third message is the mint message with the expected LP token amount.
    let mint_msg = res.messages.get(2).expect("no mint msg").clone();
    let expected_mint_msg = SubMsg::new(create_mint_tokens_msg(
        deps.api.addr_validate(MOCK_CONTRACT_ADDR).unwrap(), // contract address
        Coin {
            denom: format!("factory/{}/lp", MOCK_CONTRACT_ADDR.to_string()),
            amount: expected_provider_lp,
        },
        creator.to_string(), // mint to creator
    ));
    assert_eq!(mint_msg, expected_mint_msg);
}


#[test]
fn simulate_token_to_native_underflow() {
    let total_share = Uint128::new(2_624_880_949_681_337_452u128);

    let ask_pool_native = Uint128::new(112_671_819_035u128);

    let offer_pool_token =
        Uint128::new(61_169_543_951_810_129_262_735_575u128);

        let offer_amount = Uint128::new(100_000_000_000_000_000_000u128);

    let mut deps = mock_dependencies(&[]);

    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![
            Coin {
                denom: "token"
                    .to_string(),
                amount: ask_pool_native,
            },
            Coin {
                denom: format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
                amount: total_share,
            },
        ],
    )]);

    deps.querier.with_token_balances(&[(
        &deps.api.addr_make("cw20token").to_string(),
        &[(
            &MOCK_CONTRACT_ADDR.to_string(),
            &offer_pool_token,
        )],
    )]);

    deps.querier.with_token_factory_denom_supply(&[(
        &format!("factory/{}/lp", MOCK_CONTRACT_ADDR),
        total_share,
    )]);

    let creator = deps.api.addr_make("creator");

    let init_msg = InstantiateMsg {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: "token"
                    .to_string(),
            },
            AssetInfo::Token {
                contract_addr: deps.api.addr_make("cw20token").to_string(),
            },
        ],
        asset_decimals: [6u8, 18u8], 
        burn_address: deps.api.addr_make("burnaddr0000").to_string(),
        fee_wallet_address: deps.api.addr_make("feeaddr0000").to_string(),
    };
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&creator, &[]),
        init_msg,
    )
    .unwrap();

    let _ = query_simulation(
        deps.as_ref(),
        Asset {
            info: AssetInfo::Token {
                contract_addr: deps.api.addr_make("cw20token").to_string(),
            },
            amount: offer_amount,
        },
    )
    .unwrap();
}


#[test]
fn provide_liquidity_lp_overflow() {


    let mut deps = mock_dependencies(&[]);
    let pair_addr = Addr::unchecked(MOCK_CONTRACT_ADDR);
    let lp_denom  = format!("factory/{}/lp", MOCK_CONTRACT_ADDR);

    deps.querier.with_balance(&[(
        &pair_addr.to_string(),
        vec![
            Coin { denom: "uusd".into(),  amount: Uint128::new(1_000) },
            Coin { denom: "uluna".into(), amount: Uint128::new(1_000) },
        ],
    )]);
    deps.querier
        .with_token_factory_denom_supply(&[(&lp_denom, Uint128::MAX)]);

    let creator = deps.api.addr_make("creator");
    let burn = deps.api.addr_make("burn");              
    let fees = deps.api.addr_make("fees");              

    let inst_info = message_info(&creator, &[]);             

    instantiate(
        deps.as_mut(),                                       
        mock_env(),
        inst_info,                                            
        InstantiateMsg {
            asset_infos: [
                AssetInfo::NativeToken { denom: "uusd".into() },
                AssetInfo::NativeToken { denom: "uluna".into() },
            ],
            asset_decimals: [6, 6],
            burn_address:   burn.to_string(),
            fee_wallet_address: fees.to_string(),
        },
    )
    .unwrap();

    let exec_msg = ExecuteMsg::ProvideLiquidity {
        assets: [
            Asset {
                info:   AssetInfo::NativeToken { denom: "uusd".into() },
                amount: Uint128::new(1),
            },
            Asset {
                info:   AssetInfo::NativeToken { denom: "uluna".into() },
                amount: Uint128::new(1),
            },
        ],
        receiver: None,
        deadline: None,
        slippage_tolerance: None,
    };

    let lp_addr   = deps.api.addr_make("liquidity_provider");  // pre-compute
    let exec_info = message_info(
        &lp_addr,
        &coins(1, "uusd")
            .into_iter()
            .chain(coins(1, "uluna"))
            .collect::<Vec<_>>(),
    );

    let res = execute(deps.as_mut(), mock_env(), exec_info, exec_msg);

    match res {
        Err(ContractError::LpSupplyOverflow{}) => (),          
        other => panic!("expected LpSupplyOverflow, got {:?}", other),
    }
}

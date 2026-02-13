#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::contract::{execute, instantiate, query};
    use crate::state::POSITIONS;
    use choice_clmm_common::factory::QueryMsg as FactoryQueryMsg;
    use choice_clmm_common::manager::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use choice_clmm_common::pool::{PoolState, QueryMsg as PoolQueryMsg};
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{
        message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{
        from_json, to_json_binary, Coin, ContractResult, OwnedDeps, Querier, QuerierResult,
        QueryRequest, SystemError, SystemResult, Uint128, Uint256, WasmQuery,
    };
    use cw721::msg::OwnerOfResponse;

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    // --- 1. MOCK QUERIER SETUP ---

    pub struct CustomMockQuerier {
        base: MockQuerier,
        factory_addr: String,
        pool_addr: String,
    }

    impl CustomMockQuerier {
        pub fn new(base: MockQuerier, factory_addr: String, pool_addr: String) -> Self {
            Self {
                base,
                factory_addr,
                pool_addr,
            }
        }
    }

    impl Querier for CustomMockQuerier {
        fn raw_query(&self, bin_request: &[u8]) -> QuerierResult {
            let request: QueryRequest<cosmwasm_std::Empty> = match from_json(bin_request) {
                Ok(v) => v,
                Err(e) => {
                    return SystemResult::Err(SystemError::InvalidRequest {
                        error: format!("Parsing query request: {}", e),
                        request: bin_request.into(),
                    })
                }
            };

            match request {
                QueryRequest::Wasm(WasmQuery::Smart { contract_addr, msg }) => {
                    if contract_addr == self.factory_addr {
                        // Mock Factory Response
                        let q: FactoryQueryMsg = from_json(&msg).unwrap();
                        match q {
                            FactoryQueryMsg::GetPool { .. } => SystemResult::Ok(
                                ContractResult::Ok(to_json_binary(&self.pool_addr).unwrap()),
                            ),
                        }
                    } else if contract_addr == self.pool_addr {
                        // Mock Pool Response (GetSlot0)
                        let q: PoolQueryMsg = from_json(&msg).unwrap();
                        match q {
                            PoolQueryMsg::GetSlot0 {} => {
                                let slot0 = PoolState {
                                    sqrt_price: Uint256::from(79228162514264337593543950336u128), // 2^96 (Price = 1.0)
                                    tick: 0,
                                    liquidity: Uint128::zero(),
                                };
                                SystemResult::Ok(ContractResult::Ok(
                                    to_json_binary(&slot0).unwrap(),
                                ))
                            }
                            _ => SystemResult::Err(SystemError::UnsupportedRequest {
                                kind: "Unknown Pool Msg".to_string(),
                            }),
                        }
                    } else {
                        let req = QueryRequest::Wasm(WasmQuery::Smart { contract_addr, msg });
                        self.base.handle_query(&req)
                    }
                }
                _ => self.base.handle_query(&request),
            }
        }
    }

    fn setup_deps() -> OwnedDeps<MockStorage, MockApi, CustomMockQuerier> {
        let deps = mock_dependencies();
        let factory_bech32 = deps.api.addr_make("factory_addr").to_string();

        let querier = CustomMockQuerier::new(
            deps.querier,
            factory_bech32, // Pass the full Bech32 address here
            "pool_addr".to_string(),
        );
        OwnedDeps {
            storage: deps.storage,
            api: deps.api,
            querier,
            custom_query_type: std::marker::PhantomData,
        }
    }

    // --- 2. TESTS ---

    #[test]
    fn test_instantiate() {
        let mut deps = setup_deps();
        let msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };

        // Style: message_info + addr_make
        let creator = deps.api.addr_make("creator");
        let info = message_info(&creator, &[]);

        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.attributes[0].value, "instantiate");
    }

    #[test]
    fn test_mint_position() {
        let mut deps = setup_deps();
        let creator = deps.api.addr_make("creator");
        let user = deps.api.addr_make("user");

        // 1. Instantiate
        let init_msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            init_msg,
        )
        .unwrap();

        // 2. Mint Position
        let mint_msg = ExecuteMsg::MintPosition {
            token0: native("ATOM"),
            token1: native("OSMO"),
            fee: 500,
            tick_lower: -100,
            tick_upper: 100,
            amount0_desired: Uint128::new(1000),
            amount1_desired: Uint128::new(1000),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            recipient: None,
            deadline: 9999999999,
        };

        // Style: Explicit Coins with Uint128
        let user_info = message_info(
            &user,
            &[
                Coin {
                    denom: "ATOM".to_string(),
                    amount: Uint128::new(1000),
                },
                Coin {
                    denom: "OSMO".to_string(),
                    amount: Uint128::new(1000),
                },
            ],
        );

        let res = execute(deps.as_mut(), mock_env(), user_info, mint_msg).unwrap();

        // 3. Verify Response
        assert_eq!(res.attributes[0].value, "mint_position");
        assert_eq!(res.attributes[1].value, "1"); // Token ID 1

        // 4. Verify Sidecar State (Logic Data)
        let pos = POSITIONS.load(&deps.storage, "1").unwrap();
        assert_eq!(pos.pool_address, "pool_addr");
        assert_eq!(pos.tick_lower, -100);

        // 5. Verify NFT State (Ownership)
        let query_msg = QueryMsg::OwnerOf {
            token_id: "1".to_string(),
            include_expired: None,
        };
        let res_bin = query(deps.as_ref(), mock_env(), query_msg).unwrap();
        let owner_res: OwnerOfResponse = from_json(&res_bin).unwrap();
        assert_eq!(owner_res.owner, user.to_string());
    }

    #[test]
    fn test_decrease_liquidity_permissions() {
        let mut deps = setup_deps();
        let creator = deps.api.addr_make("creator");
        let user = deps.api.addr_make("user");
        let hacker = deps.api.addr_make("hacker");

        // Setup
        let init_msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            init_msg,
        )
        .unwrap();

        // Mint ID #1 to "user"
        let mint_msg = ExecuteMsg::MintPosition {
            token0: native("ATOM"),
            token1: native("OSMO"),
            fee: 500,
            tick_lower: -100,
            tick_upper: 100,
            amount0_desired: Uint128::new(100),
            amount1_desired: Uint128::new(100),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            recipient: None,
            deadline: 0,
        };

        let mint_info = message_info(
            &user,
            &[
                Coin {
                    denom: "ATOM".to_string(),
                    amount: Uint128::new(100),
                },
                Coin {
                    denom: "OSMO".to_string(),
                    amount: Uint128::new(100),
                },
            ],
        );

        execute(deps.as_mut(), mock_env(), mint_info, mint_msg).unwrap();

        // Test 1: Hacker tries to decrease liquidity
        let hack_msg = ExecuteMsg::DecreaseLiquidity {
            token_id: "1".to_string(),
            liquidity: Uint128::new(50),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: 0,
        };

        let hack_info = message_info(&hacker, &[]);
        let err = execute(deps.as_mut(), mock_env(), hack_info, hack_msg.clone()).unwrap_err();
        assert_eq!(err.to_string(), "Unauthorized");

        // Test 2: Owner succeeds
        let owner_info = message_info(&user, &[]);
        let res = execute(deps.as_mut(), mock_env(), owner_info, hack_msg).unwrap();
        assert_eq!(res.attributes[0].value, "decrease_liquidity");
    }

    #[test]
    fn test_increase_liquidity() {
        let mut deps = setup_deps();
        let creator = deps.api.addr_make("creator");
        let user = deps.api.addr_make("user");

        // Setup & Mint
        let init_msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };
        instantiate(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            init_msg,
        )
        .unwrap();

        let mint_info = message_info(
            &user,
            &[
                Coin {
                    denom: "ATOM".to_string(),
                    amount: Uint128::new(100),
                },
                Coin {
                    denom: "OSMO".to_string(),
                    amount: Uint128::new(100),
                },
            ],
        );

        execute(
            deps.as_mut(),
            mock_env(),
            mint_info,
            ExecuteMsg::MintPosition {
                token0: native("ATOM"),
                token1: native("OSMO"),
                fee: 500,
                tick_lower: -100,
                tick_upper: 100,
                amount0_desired: Uint128::new(100),
                amount1_desired: Uint128::new(100),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();

        // Increase Liquidity Msg
        let inc_msg = ExecuteMsg::IncreaseLiquidity {
            token_id: "1".to_string(),
            amount0_desired: Uint128::new(50),
            amount1_desired: Uint128::new(50),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: 0,
        };

        // Fail: insufficient funds sent
        // User sends 49 ATOM (missing 1)
        let fail_info = message_info(
            &user,
            &[Coin {
                denom: "ATOM".to_string(),
                amount: Uint128::new(49),
            }],
        );
        let err = execute(deps.as_mut(), mock_env(), fail_info, inc_msg.clone()).unwrap_err();
        assert!(err.to_string().contains("Insufficient funds"));

        // Success: User sends correct funds
        let success_info = message_info(
            &user,
            &[
                Coin {
                    denom: "ATOM".to_string(),
                    amount: Uint128::new(50),
                },
                Coin {
                    denom: "OSMO".to_string(),
                    amount: Uint128::new(50),
                },
            ],
        );
        let res = execute(deps.as_mut(), mock_env(), success_info, inc_msg).unwrap();
        assert_eq!(res.attributes[0].value, "increase_liquidity");
    }
}

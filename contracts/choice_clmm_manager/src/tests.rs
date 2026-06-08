#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::contract::{execute, instantiate, query, reply};
    use crate::state::{
        PositionState, PENDING_INCREASE, PENDING_MINT, POSITIONS, REPLY_COLLECT,
        REPLY_DECREASE_LIQUIDITY, REPLY_INCREASE_LIQUIDITY, REPLY_MINT_POSITION,
    };
    use choice_clmm_common::factory::QueryMsg as FactoryQueryMsg;
    use choice_clmm_common::manager::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use choice_clmm_common::pool::{FeeGrowthInsideResponse, PoolState, QueryMsg as PoolQueryMsg};
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{
        message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{
        from_json, to_json_binary, Coin, ContractResult, CosmosMsg, Event, OwnedDeps, Querier,
        QuerierResult, QueryRequest, Reply, SubMsgResponse, SubMsgResult, SystemError,
        SystemResult, Uint128, Uint256, WasmMsg, WasmQuery,
    };
    use cw721::msg::OwnerOfResponse;
    use std::cell::RefCell;

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    // ------------------------------------------------------------
    // Mock pool/factory querier.
    //
    // Serves:
    //   factory: GetPool -> pool_addr
    //   pool:    GetSlot0 -> PoolState { sqrt_price = 2^96, tick = 0, liquidity = 0 }
    //   pool:    GetFeeGrowthInside -> values from an interior cell so each
    //            test can independently simulate fee accrual
    // ------------------------------------------------------------

    pub struct CustomMockQuerier {
        base: MockQuerier,
        factory_addr: String,
        pool_addr: String,
        fee_growth_inside: RefCell<(Uint256, Uint256)>,
    }

    impl CustomMockQuerier {
        pub fn new(base: MockQuerier, factory_addr: String, pool_addr: String) -> Self {
            Self {
                base,
                factory_addr,
                pool_addr,
                fee_growth_inside: RefCell::new((Uint256::zero(), Uint256::zero())),
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
                        let q: FactoryQueryMsg = from_json(&msg).unwrap();
                        match q {
                            FactoryQueryMsg::GetPool { .. } => SystemResult::Ok(
                                ContractResult::Ok(to_json_binary(&self.pool_addr).unwrap()),
                            ),
                            _ => SystemResult::Ok(ContractResult::Ok(to_json_binary(&"").unwrap())),
                        }
                    } else if contract_addr == self.pool_addr {
                        let q: PoolQueryMsg = from_json(&msg).unwrap();
                        match q {
                            PoolQueryMsg::GetSlot0 {} => {
                                let slot0 = PoolState {
                                    sqrt_price: Uint256::from(79228162514264337593543950336u128), // 2^96
                                    tick: 0,
                                    liquidity: Uint128::zero(),
                                };
                                SystemResult::Ok(ContractResult::Ok(
                                    to_json_binary(&slot0).unwrap(),
                                ))
                            }
                            PoolQueryMsg::GetFeeGrowthInside { .. } => {
                                let (f0, f1) = *self.fee_growth_inside.borrow();
                                let resp = FeeGrowthInsideResponse {
                                    fee_growth_inside_0_x128: f0,
                                    fee_growth_inside_1_x128: f1,
                                };
                                SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
                            }
                            _ => SystemResult::Err(SystemError::UnsupportedRequest {
                                kind: format!("Unknown Pool Msg: {:?}", q),
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

    fn set_fee_growth_inside(
        deps: &mut OwnedDeps<MockStorage, MockApi, CustomMockQuerier>,
        v0: Uint256,
        v1: Uint256,
    ) {
        *deps.querier.fee_growth_inside.borrow_mut() = (v0, v1);
    }

    fn setup_deps() -> OwnedDeps<MockStorage, MockApi, CustomMockQuerier> {
        let deps = mock_dependencies();
        let factory_bech32 = deps.api.addr_make("factory_addr").to_string();
        let pool_bech32 = deps.api.addr_make("pool_addr").to_string();

        let querier = CustomMockQuerier::new(deps.querier, factory_bech32, pool_bech32);
        OwnedDeps {
            storage: deps.storage,
            api: deps.api,
            querier,
            custom_query_type: std::marker::PhantomData,
        }
    }

    /// Encode a `token_id` (decimal string) into the 8-byte big-endian reply
    /// payload the contract tags every SubMsg with (C-L5).
    fn payload_for(token_id: &str) -> cosmwasm_std::Binary {
        let key: u64 = token_id.parse().unwrap();
        cosmwasm_std::Binary::from(key.to_be_bytes().to_vec())
    }

    fn ok_reply_with_burn_attrs(
        id: u64,
        token_id: &str,
        amount0: u128,
        amount1: u128,
        emitter: &str,
    ) -> Reply {
        ok_reply_with_burn_attrs_and_fg(
            id,
            token_id,
            amount0,
            amount1,
            Uint256::zero(),
            Uint256::zero(),
            emitter,
        )
    }

    /// Build a reply that mimics the pool's `Collect` response — carries
    /// `amount0` / `amount1` (actual paid amounts) keyed to the emitter.
    fn ok_reply_with_collect_attrs(
        id: u64,
        token_id: &str,
        amount0: u128,
        amount1: u128,
        emitter: &str,
    ) -> Reply {
        let ev = Event::new("wasm")
            .add_attribute("_contract_address", emitter)
            .add_attribute("amount0", Uint128::new(amount0).to_string())
            .add_attribute("amount1", Uint128::new(amount1).to_string());
        Reply {
            id,
            payload: payload_for(token_id),
            gas_used: 0,
            result: SubMsgResult::Ok(ok_submsg_response(vec![ev])),
        }
    }

    fn ok_reply_with_burn_attrs_and_fg(
        id: u64,
        token_id: &str,
        amount0: u128,
        amount1: u128,
        fg_inside_0: Uint256,
        fg_inside_1: Uint256,
        emitter: &str,
    ) -> Reply {
        let ev = Event::new("wasm")
            .add_attribute("_contract_address", emitter)
            .add_attribute("amount0_burned", Uint128::new(amount0).to_string())
            .add_attribute("amount1_burned", Uint128::new(amount1).to_string())
            .add_attribute("fee_growth_inside_0_x128", fg_inside_0.to_string())
            .add_attribute("fee_growth_inside_1_x128", fg_inside_1.to_string());
        Reply {
            id,
            payload: payload_for(token_id),
            gas_used: 0,
            result: SubMsgResult::Ok(ok_submsg_response(vec![ev])),
        }
    }

    /// Build a reply that mimics the pool's `Mint` response — carries
    /// `amount0_consumed` / `amount1_consumed` attributes keyed to the emitter,
    /// matching whatever the manager parked in PENDING_{MINT,INCREASE}. Loads
    /// from storage so call sites don't need to duplicate the amounts.
    fn ok_reply_consumed(
        deps: &OwnedDeps<MockStorage, MockApi, CustomMockQuerier>,
        id: u64,
        token_id: &str,
        emitter: &str,
    ) -> Reply {
        let key: u64 = token_id.parse().unwrap();
        let (amount0, amount1) = if id == REPLY_MINT_POSITION {
            let p = PENDING_MINT.load(&deps.storage, key).unwrap();
            (p.amount0_sent_to_pool, p.amount1_sent_to_pool)
        } else if id == REPLY_INCREASE_LIQUIDITY {
            let p = PENDING_INCREASE.load(&deps.storage, key).unwrap();
            (p.amount0_sent_to_pool, p.amount1_sent_to_pool)
        } else {
            panic!("ok_reply_consumed only supports mint/increase reply ids");
        };
        let ev = Event::new("wasm")
            .add_attribute("_contract_address", emitter)
            .add_attribute("amount0_consumed", amount0.to_string())
            .add_attribute("amount1_consumed", amount1.to_string());
        Reply {
            id,
            payload: payload_for(token_id),
            gas_used: 0,
            result: SubMsgResult::Ok(ok_submsg_response(vec![ev])),
        }
    }

    /// Build a `SubMsgResponse` with `events` populated. The `data` field is
    /// deprecated post-CosmWasm 2.0 but still required by the struct; we
    /// suppress the deprecation warning at the one allocation site instead
    /// of polluting every call site.
    #[allow(deprecated)]
    fn ok_submsg_response(events: Vec<Event>) -> SubMsgResponse {
        SubMsgResponse {
            events,
            data: None,
            msg_responses: vec![],
        }
    }

    // ------------------------------------------------------------
    // Basic setup tests
    // ------------------------------------------------------------

    #[test]
    fn test_instantiate() {
        let mut deps = setup_deps();
        let msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };

        let creator = deps.api.addr_make("creator");
        let info = message_info(&creator, &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.attributes[0].value, "instantiate");
    }

    /// Regression: only MintPosition / IncreaseLiquidity forward funds to the
    /// pool. Every other entrypoint must REJECT attached native coins rather
    /// than stranding them in the manager contract. The guard fires before any
    /// position/ownership lookup, so no NFT setup is needed.
    #[test]
    fn nonpayable_entrypoints_reject_attached_funds() {
        let mut deps = setup_deps();
        let msg = InstantiateMsg {
            name: "Choice Positions".to_string(),
            symbol: "CH-POS".to_string(),
            factory_addr: deps.api.addr_make("factory_addr").to_string(),
        };
        let creator = deps.api.addr_make("creator");
        instantiate(deps.as_mut(), mock_env(), message_info(&creator, &[]), msg).unwrap();

        let user = deps.api.addr_make("user");
        let funds = [Coin::new(Uint128::new(1), "inj")];
        let cases = vec![
            ExecuteMsg::DecreaseLiquidity {
                token_id: "1".to_string(),
                liquidity: Uint128::new(1),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
            ExecuteMsg::Collect {
                token_id: "1".to_string(),
                recipient: None,
            },
            ExecuteMsg::Burn {
                token_id: "1".to_string(),
            },
            ExecuteMsg::TransferNft {
                recipient: user.to_string(),
                token_id: "1".to_string(),
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

    fn instantiate_manager(deps: &mut OwnedDeps<MockStorage, MockApi, CustomMockQuerier>) {
        let creator = deps.api.addr_make("creator");
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
    }

    fn mint_nft(
        deps: &mut OwnedDeps<MockStorage, MockApi, CustomMockQuerier>,
        user: &cosmwasm_std::Addr,
        amount0: u128,
        amount1: u128,
        tick_lower: i32,
        tick_upper: i32,
    ) -> String {
        // 1. execute_mint_position
        let info = message_info(
            user,
            &[
                Coin {
                    denom: "ATOM".to_string(),
                    amount: Uint128::new(amount0),
                },
                Coin {
                    denom: "OSMO".to_string(),
                    amount: Uint128::new(amount1),
                },
            ],
        );
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("ATOM"),
                token1: native("OSMO"),
                fee: 500,
                tick_lower,
                tick_upper,
                amount0_desired: Uint128::new(amount0),
                amount1_desired: Uint128::new(amount1),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();
        let token_id = res
            .attributes
            .iter()
            .find(|a| a.key == "token_id")
            .unwrap()
            .value
            .clone();

        // 2. simulate the pool submsg having succeeded -> run reply
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let r = ok_reply_consumed(deps, REPLY_MINT_POSITION, &token_id, &pool_addr);
        reply(deps.as_mut(), mock_env(), r).unwrap();

        token_id
    }

    // ------------------------------------------------------------
    // Phase 3 tests
    // ------------------------------------------------------------

    #[test]
    fn mint_position_records_nft_state_and_liquidity() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);

        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        assert_eq!(token_id, "1");

        // Sidecar state reflects NFT-level liquidity.
        let pos = POSITIONS.load(&deps.storage, &token_id).unwrap();
        assert_eq!(pos.tick_lower, -100);
        assert_eq!(pos.tick_upper, 100);
        assert!(!pos.liquidity.is_zero(), "liquidity should be set");
        assert_eq!(pos.tokens_owed_0, Uint128::zero());
        assert_eq!(pos.tokens_owed_1, Uint128::zero());
        assert_eq!(pos.fee_growth_inside_0_last_x128, Uint256::zero());
        assert_eq!(pos.fee_growth_inside_1_last_x128, Uint256::zero());

        // NFT ownership.
        let owner_bin = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::OwnerOf {
                token_id: token_id.clone(),
                include_expired: None,
            },
        )
        .unwrap();
        let owner_res: OwnerOfResponse = from_json(&owner_bin).unwrap();
        assert_eq!(owner_res.owner, user.to_string());
    }

    #[test]
    fn mint_refunds_native_excess_to_sender() {
        // Regression (audit HIGH-8): prior versions forwarded the full
        // attached native amount to the pool regardless of actual consumption;
        // excess was stranded. Fix: manager sends exactly `amount*_actual` and
        // refunds the rest via BankMsg::Send in the same response.

        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");

        // Mint a RANGE-ABOVE-CURRENT position so only token0 is consumed.
        // User sends BOTH tokens; token1 (OSMO) should be fully refunded.
        let info = message_info(
            &user,
            &[
                Coin::new(Uint128::new(1_000), "ATOM"),
                Coin::new(Uint128::new(1_000), "OSMO"), // extra
            ],
        );
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("ATOM"),
                token1: native("OSMO"),
                fee: 500,
                // Range entirely ABOVE current price 0.
                tick_lower: 10,
                tick_upper: 100,
                amount0_desired: Uint128::new(1_000),
                amount1_desired: Uint128::new(1_000),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();

        // Find the BankMsg::Send refund.
        let refund = res
            .messages
            .iter()
            .find_map(|m| match &m.msg {
                CosmosMsg::Bank(cosmwasm_std::BankMsg::Send { to_address, amount })
                    if to_address == &user.to_string() =>
                {
                    Some(amount.clone())
                }
                _ => None,
            })
            .expect("should refund excess to user");

        // Token1 not consumed at all for an entirely-above-current range.
        let osmo = refund.iter().find(|c| c.denom == "OSMO");
        assert!(osmo.is_some(), "expected OSMO refund");
        assert_eq!(osmo.unwrap().amount, Uint128::new(1_000));
    }

    #[test]
    fn increase_liquidity_accrues_existing_fees_to_nft() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");

        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        // Pre: no tokens_owed.
        let before = POSITIONS.load(&deps.storage, &token_id).unwrap();
        assert_eq!(before.tokens_owed_0, Uint128::zero());

        // Simulate the pool accruing some fee_growth_inside_0.
        // 100 * Q128 of growth over L=<from mint> -> fees = 100 * L.
        let q128 = Uint256::one() << 128u32;
        set_fee_growth_inside(&mut deps, q128, Uint256::zero());

        // User adds more liquidity.
        let info = message_info(
            &user,
            &[
                Coin::new(Uint128::new(500), "ATOM"),
                Coin::new(Uint128::new(500), "OSMO"),
            ],
        );
        execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::IncreaseLiquidity {
                token_id: token_id.clone(),
                amount0_desired: Uint128::new(500),
                amount1_desired: Uint128::new(500),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap();

        // Reply fires after pool mint.
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let r = ok_reply_consumed(&deps, REPLY_INCREASE_LIQUIDITY, &token_id, &pool_addr);
        reply(deps.as_mut(), mock_env(), r).unwrap();

        let after = POSITIONS.load(&deps.storage, &token_id).unwrap();
        // fee delta = Q128 - 0 = Q128
        // accrued_0 = L * Q128 / Q128 = L (before the increase)
        assert_eq!(after.tokens_owed_0, Uint128::new(before.liquidity.u128()));
        assert!(
            after.liquidity > before.liquidity,
            "liquidity should have increased"
        );
        assert_eq!(after.fee_growth_inside_0_last_x128, q128);
    }

    #[test]
    fn decrease_liquidity_credits_principal_and_fees_to_tokens_owed() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        // Swap accrues Q128 of fee_growth_inside_0.
        let q128 = Uint256::one() << 128u32;
        set_fee_growth_inside(&mut deps, q128, Uint256::zero());

        // Request decrease half the liquidity.
        let before = POSITIONS.load(&deps.storage, &token_id).unwrap();
        let half = Uint128::new(before.liquidity.u128() / 2);

        let info = message_info(&user, &[]);
        execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::DecreaseLiquidity {
                token_id: token_id.clone(),
                liquidity: half,
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap();

        // Simulate pool.Burn result: amount0_burned=400, amount1_burned=600 (arbitrary).
        // Pool emits the post-update fee_growth_inside used when crediting its
        // own position.tokens_owed — the manager now parses these from the
        // reply instead of re-querying (which would read zeros if the burn
        // cleared the ticks).
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        reply(
            deps.as_mut(),
            mock_env(),
            ok_reply_with_burn_attrs_and_fg(
                REPLY_DECREASE_LIQUIDITY,
                &token_id,
                400,
                600,
                q128,
                Uint256::zero(),
                &pool_addr,
            ),
        )
        .unwrap();

        let after = POSITIONS.load(&deps.storage, &token_id).unwrap();
        // principal credited
        assert_eq!(
            after.tokens_owed_0,
            Uint128::new(400 + before.liquidity.u128())
        );
        assert_eq!(after.tokens_owed_1, Uint128::new(600));
        // liquidity decreased by exactly `half`.
        assert_eq!(
            after.liquidity.u128(),
            before.liquidity.u128() - half.u128()
        );
        assert_eq!(after.fee_growth_inside_0_last_x128, q128);
    }

    #[test]
    fn decrease_liquidity_slippage_revert() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);
        let before = POSITIONS.load(&deps.storage, &token_id).unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::DecreaseLiquidity {
                token_id: token_id.clone(),
                liquidity: Uint128::new(before.liquidity.u128() / 2),
                // Require more than the pool will actually return.
                amount0_min: Uint128::new(10_000_000),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap();

        // Pool reports only 100 burned — below the 10M minimum we asked for.
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let err = reply(
            deps.as_mut(),
            mock_env(),
            ok_reply_with_burn_attrs(REPLY_DECREASE_LIQUIDITY, &token_id, 100, 100, &pool_addr),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Slippage"), "got: {}", err);
    }

    #[test]
    fn collect_sends_only_this_nfts_share() {
        // Headline Phase 3 exploit regression (audit CRIT-1): two NFTs share
        // the same (manager, tickL, tickU) pool position. After fee accrual,
        // each NFT's collect must pay ONLY its own share, not drain the
        // whole pool-level tokens_owed.

        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let alice = deps.api.addr_make("alice");
        let bob = deps.api.addr_make("bob");

        // Alice mints first.
        let alice_id = mint_nft(&mut deps, &alice, 1_000, 1_000, -100, 100);
        let alice_before = POSITIONS.load(&deps.storage, &alice_id).unwrap();

        // Between Alice and Bob's mints, half the total fee accrual happens.
        let q128 = Uint256::one() << 128u32;
        set_fee_growth_inside(&mut deps, q128, Uint256::zero());

        // Bob mints (snapshots fee_growth_inside_last = Q128; no fees accrue to him yet).
        let bob_id = mint_nft(&mut deps, &bob, 1_000, 1_000, -100, 100);
        let bob_before = POSITIONS.load(&deps.storage, &bob_id).unwrap();
        assert_eq!(bob_before.fee_growth_inside_0_last_x128, q128);

        // More fee accrual after Bob's entry.
        set_fee_growth_inside(&mut deps, q128 * Uint256::from(2u128), Uint256::zero());

        // Alice collects. Her share = L_A * (2Q - 0) / Q = 2 * L_A.
        // Bob's hypothetical share (if he collected now) = L_B * (2Q - Q) / Q = L_B.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&alice, &[]),
            ExecuteMsg::Collect {
                token_id: alice_id.clone(),
                recipient: None,
            },
        )
        .unwrap();

        // Inspect the pool.Collect submsg — must request exactly Alice's share.
        let collect_amount_0 = res
            .messages
            .iter()
            .find_map(|m| match &m.msg {
                CosmosMsg::Wasm(WasmMsg::Execute { msg: bin, .. }) => {
                    let pe: Result<choice_clmm_common::pool::ExecuteMsg, _> = from_json(bin);
                    if let Ok(choice_clmm_common::pool::ExecuteMsg::Collect {
                        amount0_requested,
                        ..
                    }) = pe
                    {
                        Some(amount0_requested)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .expect("expected pool.Collect submsg");

        let alice_share = 2 * alice_before.liquidity.u128();
        assert_eq!(
            collect_amount_0.u128(),
            alice_share,
            "Alice should collect exactly her share, not Bob's"
        );

        // Simulate the pool paying out the full requested amount. Reply
        // decrements tokens_owed by exactly what was paid.
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        reply(
            deps.as_mut(),
            mock_env(),
            ok_reply_with_collect_attrs(REPLY_COLLECT, &alice_id, alice_share, 0, &pool_addr),
        )
        .unwrap();

        // Alice's NFT tokens_owed was zeroed after reply.
        let alice_after = POSITIONS.load(&deps.storage, &alice_id).unwrap();
        assert_eq!(alice_after.tokens_owed_0, Uint128::zero());
        assert_eq!(
            alice_after.fee_growth_inside_0_last_x128,
            q128 * Uint256::from(2u128)
        );

        // Bob's NFT is untouched (still snapshot = Q128, no tokens_owed).
        let bob_still = POSITIONS.load(&deps.storage, &bob_id).unwrap();
        assert_eq!(bob_still.tokens_owed_0, Uint128::zero());
        assert_eq!(bob_still.fee_growth_inside_0_last_x128, q128);
    }

    #[test]
    fn collect_shortfall_leaves_residue_on_nft() {
        // If the pool pays less than requested (e.g. because its
        // position.tokens_owed got out of sync), the manager must leave the
        // unpaid amount on the NFT for a later retry — not silently zero
        // tokens_owed as the pre-fix code did.
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 10_000, 10_000, -100, 100);

        // Accrue Q128 of fee_growth_inside_0 so the NFT's share is nonzero.
        let q128 = Uint256::one() << 128u32;
        set_fee_growth_inside(&mut deps, q128, Uint256::zero());

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::Collect {
                token_id: token_id.clone(),
                recipient: None,
            },
        )
        .unwrap();

        let after_execute = POSITIONS.load(&deps.storage, &token_id).unwrap();
        let requested = after_execute.tokens_owed_0;
        assert!(requested > Uint128::zero(), "expected non-zero request");
        // Pool pays only half of what was requested.
        let half = Uint128::new(requested.u128() / 2);

        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        reply(
            deps.as_mut(),
            mock_env(),
            ok_reply_with_collect_attrs(REPLY_COLLECT, &token_id, half.u128(), 0, &pool_addr),
        )
        .unwrap();

        let after_reply = POSITIONS.load(&deps.storage, &token_id).unwrap();
        // Residue equals the unpaid portion, available for a later collect.
        assert_eq!(
            after_reply.tokens_owed_0,
            requested.checked_sub(half).unwrap()
        );
    }

    #[test]
    fn burn_rejected_while_liquidity_nonzero() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::Burn { token_id },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not cleared"), "got: {}", err);
    }

    #[test]
    fn burn_rejected_while_tokens_owed_nonzero() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        // Manually clear liquidity but leave tokens_owed — simulate a state
        // where user decreased to zero but hasn't collected yet.
        let mut state = POSITIONS.load(&deps.storage, &token_id).unwrap();
        state.liquidity = Uint128::zero();
        state.tokens_owed_0 = Uint128::new(42);
        POSITIONS
            .save(&mut deps.storage, &token_id, &state)
            .unwrap();

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::Burn { token_id },
        )
        .unwrap_err();
        assert!(err.to_string().contains("not cleared"), "got: {}", err);
    }

    #[test]
    fn burn_succeeds_when_fully_cleared() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        // Fully clear position state in storage (simulating decrease + collect).
        let mut state = POSITIONS.load(&deps.storage, &token_id).unwrap();
        state.liquidity = Uint128::zero();
        state.tokens_owed_0 = Uint128::zero();
        state.tokens_owed_1 = Uint128::zero();
        POSITIONS
            .save(&mut deps.storage, &token_id, &state)
            .unwrap();

        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::Burn {
                token_id: token_id.clone(),
            },
        )
        .unwrap();
        // POSITIONS entry removed.
        assert!(POSITIONS
            .may_load(&deps.storage, &token_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn hacker_cannot_decrease_or_burn_another_users_nft() {
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let hacker = deps.api.addr_make("hacker");

        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&hacker, &[]),
            ExecuteMsg::DecreaseLiquidity {
                token_id: token_id.clone(),
                liquidity: Uint128::new(50),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Unauthorized");

        // Also cannot burn (even though burn also hits the not-cleared check).
        let err2 = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&hacker, &[]),
            ExecuteMsg::Burn { token_id },
        )
        .unwrap_err();
        assert_eq!(err2.to_string(), "Unauthorized");
    }

    #[test]
    fn approve_all_operator_can_decrease() {
        // Regression (audit HIGH-6): `assert_owner_or_approved` previously
        // ignored cw721 operator (ApproveAll) approvals, breaking standard
        // cw721 delegation semantics.
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");
        let operator = deps.api.addr_make("operator");

        let token_id = mint_nft(&mut deps, &user, 1_000, 1_000, -100, 100);

        // User grants ApproveAll to operator.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &[]),
            ExecuteMsg::ApproveAll {
                operator: operator.to_string(),
                expires: None,
            },
        )
        .unwrap();

        // Operator can decrease.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&operator, &[]),
            ExecuteMsg::DecreaseLiquidity {
                token_id,
                liquidity: Uint128::new(10),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap();
    }

    // ------------------------------------------------------------
    // C-M1: token0/token1 canonicalization in mint
    // ------------------------------------------------------------

    /// Find the pool.Mint submsg funds in a mint response.
    fn pool_mint_funds(res: &cosmwasm_std::Response) -> Vec<Coin> {
        res.messages
            .iter()
            .find_map(|m| match &m.msg {
                CosmosMsg::Wasm(WasmMsg::Execute {
                    funds, msg: bin, ..
                }) => {
                    let pe: Result<choice_clmm_common::pool::ExecuteMsg, _> = from_json(bin);
                    if matches!(pe, Ok(choice_clmm_common::pool::ExecuteMsg::Mint { .. })) {
                        Some(funds.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .expect("expected pool.Mint submsg")
    }

    #[test]
    fn mint_with_reversed_token_order_is_canonicalized() {
        // Pass tokens in NON-canonical order (token0=OSMO, token1=ATOM; ATOM <
        // OSMO so this is reversed) with ASYMMETRIC amounts. The manager must
        // canonicalize to (token0=ATOM, token1=OSMO) and re-map the amounts so
        // the stored PositionState is canonical and funds route correctly.
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");

        // Range entirely ABOVE current price (tick 0): only canonical token0
        // (ATOM) is consumed. Caller passes token0=OSMO, so amount0_desired
        // is tied to OSMO; after canonicalization it must move to amount1.
        let info = message_info(
            &user,
            &[
                Coin::new(Uint128::new(7_000), "ATOM"),
                Coin::new(Uint128::new(3_000), "OSMO"),
            ],
        );
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("OSMO"),
                token1: native("ATOM"),
                fee: 500,
                tick_lower: 10,
                tick_upper: 100,
                // amounts are in CALLER order: amount0 pairs with OSMO.
                amount0_desired: Uint128::new(3_000),
                amount1_desired: Uint128::new(7_000),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();

        let token_id = res
            .attributes
            .iter()
            .find(|a| a.key == "token_id")
            .unwrap()
            .value
            .clone();

        // Pending mint must record CANONICAL order: token0 == ATOM.
        let key: u64 = token_id.parse().unwrap();
        let pending = PENDING_MINT.load(&deps.storage, key).unwrap();
        assert_eq!(
            pending.token0,
            native("ATOM"),
            "token0 must canonicalize to ATOM"
        );
        assert_eq!(
            pending.token1,
            native("OSMO"),
            "token1 must canonicalize to OSMO"
        );

        // Range above current => only canonical token0 (ATOM) consumed.
        assert!(
            !pending.amount0_sent_to_pool.is_zero(),
            "canonical token0 (ATOM) must be consumed"
        );
        assert!(
            pending.amount1_sent_to_pool.is_zero(),
            "canonical token1 (OSMO) must NOT be consumed for above-range position"
        );

        // Funds forwarded to the pool must be ATOM only (denom routed correctly).
        let funds = pool_mint_funds(&res);
        assert_eq!(funds.len(), 1, "only one denom forwarded");
        assert_eq!(
            funds[0].denom, "ATOM",
            "forwarded denom must be canonical token0 ATOM"
        );

        // Complete the reply and assert the persisted PositionState is canonical.
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let r = ok_reply_consumed(&deps, REPLY_MINT_POSITION, &token_id, &pool_addr);
        reply(deps.as_mut(), mock_env(), r).unwrap();

        let pos = POSITIONS.load(&deps.storage, &token_id).unwrap();
        assert_eq!(pos.token0, native("ATOM"));
        assert_eq!(pos.token1, native("OSMO"));
    }

    #[test]
    fn increase_liquidity_routes_correct_denom_after_reversed_mint() {
        // After a reversed-order mint canonicalizes to (ATOM, OSMO), a later
        // IncreaseLiquidity (which reads token order from stored PositionState)
        // must forward the correct denom.
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");

        // Reversed-order mint, range above current => token0 (ATOM) only.
        let info = message_info(&user, &[Coin::new(Uint128::new(5_000), "ATOM")]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("OSMO"),
                token1: native("ATOM"),
                fee: 500,
                tick_lower: 10,
                tick_upper: 100,
                amount0_desired: Uint128::zero(), // OSMO (caller order)
                amount1_desired: Uint128::new(5_000), // ATOM (caller order)
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();
        let token_id = res
            .attributes
            .iter()
            .find(|a| a.key == "token_id")
            .unwrap()
            .value
            .clone();
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let r = ok_reply_consumed(&deps, REPLY_MINT_POSITION, &token_id, &pool_addr);
        reply(deps.as_mut(), mock_env(), r).unwrap();

        // IncreaseLiquidity uses stored (canonical) order: amount0 == ATOM.
        let info = message_info(&user, &[Coin::new(Uint128::new(2_000), "ATOM")]);
        let res = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::IncreaseLiquidity {
                token_id: token_id.clone(),
                amount0_desired: Uint128::new(2_000),
                amount1_desired: Uint128::zero(),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: 0,
            },
        )
        .unwrap();

        let funds = pool_mint_funds(&res);
        assert_eq!(funds.len(), 1, "only one denom forwarded on increase");
        assert_eq!(
            funds[0].denom, "ATOM",
            "increase must forward canonical token0 (ATOM)"
        );
    }

    // ------------------------------------------------------------
    // C-L5: pending reply state keyed by token_id
    // ------------------------------------------------------------

    #[test]
    fn pending_mint_state_is_keyed_by_token_id() {
        // Two concurrent in-flight mints must each park their pending context
        // under their own token_id key, so a reply for one cannot clobber or
        // load the other's state.
        let mut deps = setup_deps();
        instantiate_manager(&mut deps);
        let user = deps.api.addr_make("user");

        // First mint (token_id 1) — DO NOT run its reply, leaving it pending.
        let info = message_info(&user, &[Coin::new(Uint128::new(1_000), "ATOM")]);
        let res1 = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("ATOM"),
                token1: native("OSMO"),
                fee: 500,
                tick_lower: 10, // above current => token0 (ATOM) only
                tick_upper: 100,
                amount0_desired: Uint128::new(1_000),
                amount1_desired: Uint128::zero(),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();
        let id1 = res1
            .attributes
            .iter()
            .find(|a| a.key == "token_id")
            .unwrap()
            .value
            .clone();

        // Second mint (token_id 2), also left pending, with a DIFFERENT amount.
        let info = message_info(&user, &[Coin::new(Uint128::new(2_000), "ATOM")]);
        let res2 = execute(
            deps.as_mut(),
            mock_env(),
            info,
            ExecuteMsg::MintPosition {
                token0: native("ATOM"),
                token1: native("OSMO"),
                fee: 500,
                tick_lower: 10,
                tick_upper: 100,
                amount0_desired: Uint128::new(2_000),
                amount1_desired: Uint128::zero(),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                recipient: None,
                deadline: 0,
            },
        )
        .unwrap();
        let id2 = res2
            .attributes
            .iter()
            .find(|a| a.key == "token_id")
            .unwrap()
            .value
            .clone();

        assert_ne!(id1, id2, "ids must differ");
        let k1: u64 = id1.parse().unwrap();
        let k2: u64 = id2.parse().unwrap();

        // Both pending entries coexist under distinct keys.
        let p1 = PENDING_MINT.load(&deps.storage, k1).unwrap();
        let p2 = PENDING_MINT.load(&deps.storage, k2).unwrap();
        assert_eq!(p1.token_id, id1);
        assert_eq!(p2.token_id, id2);
        assert_eq!(p1.amount0_sent_to_pool, Uint128::new(1_000));
        assert_eq!(p2.amount0_sent_to_pool, Uint128::new(2_000));

        // Run the reply for id1 (payload carries k1). It must consume ONLY
        // id1's entry and leave id2's untouched.
        let pool_addr = deps.api.addr_make("pool_addr").to_string();
        let r1 = ok_reply_consumed(&deps, REPLY_MINT_POSITION, &id1, &pool_addr);
        reply(deps.as_mut(), mock_env(), r1).unwrap();

        assert!(
            PENDING_MINT.may_load(&deps.storage, k1).unwrap().is_none(),
            "id1 pending entry must be removed after its reply"
        );
        let p2_still = PENDING_MINT
            .may_load(&deps.storage, k2)
            .unwrap()
            .expect("id2 pending entry must survive id1's reply");
        assert_eq!(
            p2_still.amount0_sent_to_pool,
            Uint128::new(2_000),
            "id2 pending state must be intact (not clobbered)"
        );

        // id1's position is now persisted; id2's is not yet.
        assert!(POSITIONS.may_load(&deps.storage, &id1).unwrap().is_some());
        assert!(POSITIONS.may_load(&deps.storage, &id2).unwrap().is_none());
    }

    fn _ensure_position_default_construction_compiles() {
        // Type sanity check: PositionState must not accidentally require Default.
        let _ = |s: PositionState| s.liquidity;
    }
}

#[cfg(test)]
mod tests {
    use crate::contract::{execute, instantiate, reply};
    use crate::state::{CONFIG, FEE_TIERS, POOLS};
    use choice_clmm_common::factory::{ExecuteMsg, InstantiateMsg};
    use choice_clmm_common::pool::InstantiateMsg as PoolInstantiateMsg;
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{
        message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{
        attr, from_json, Addr, Binary, CosmosMsg, Event, OwnedDeps, Reply, SubMsgResponse,
        SubMsgResult, Uint256, WasmMsg,
    };
    use sha2::{Digest, Sha256};

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    // Helper to setup the factory
    fn setup_factory() -> OwnedDeps<MockStorage, MockApi, MockQuerier> {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg { pool_code_id: 123 };
        let info = message_info(&deps.api.addr_make("creator"), &[]);
        let res = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(0, res.messages.len());
        deps
    }

    #[test]
    fn test_instantiate() {
        let deps = setup_factory();

        // Check Config
        let config = CONFIG.load(&deps.storage).unwrap();
        assert_eq!(config.owner, Addr::unchecked(deps.api.addr_make("creator")));
        assert_eq!(config.pool_code_id, 123);

        // Check Default Fee Tiers
        assert_eq!(FEE_TIERS.load(&deps.storage, 100).unwrap(), 1); // 0.01%
        assert_eq!(FEE_TIERS.load(&deps.storage, 500).unwrap(), 10); // 0.05%
        assert_eq!(FEE_TIERS.load(&deps.storage, 3000).unwrap(), 60); // 0.30%
        assert_eq!(FEE_TIERS.load(&deps.storage, 10000).unwrap(), 200); // 1.00%
    }

    #[test]
    fn test_enable_fee_amount() {
        let mut deps = setup_factory();

        // 1. Unauthorized attempt
        let msg = ExecuteMsg::EnableFeeAmount {
            fee: 250,
            tick_spacing: 5,
        };
        let info = message_info(&deps.api.addr_make("hacker"), &[]);
        let err = execute(deps.as_mut(), mock_env(), info, msg.clone()).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // 2. Owner success
        let info = message_info(&deps.api.addr_make("creator"), &[]);
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(
            res.attributes,
            vec![
                attr("action", "enable_fee_amount"),
                attr("fee", "250"),
                attr("tick_spacing", "5"),
            ]
        );

        // Verify storage
        assert_eq!(FEE_TIERS.load(&deps.storage, 250).unwrap(), 5);
    }

    #[test]
    fn test_create_pool_validation() {
        let mut deps = setup_factory();
        let info = message_info(&deps.api.addr_make("user"), &[]);
        let init_price = Uint256::from(79228162514264337593543950336u128); // SqrtPrice for 1.0

        // 1. Same tokens
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("ATOM"),
            fee: 500,
            init_sqrt_price: init_price,
        };
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Same tokens");

        // 2. Invalid Fee Tier
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 999999, // Doesn't exist
            init_sqrt_price: init_price,
        };
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Fee tier not supported");
    }

    #[test]
    fn test_create_pool_instantiate2() {
        let mut deps = setup_factory();
        let info = message_info(&deps.api.addr_make("user"), &[]);
        let init_price = Uint256::from(79228162514264337593543950336u128);

        // We use unsorted tokens to test the sorting logic
        // "OSMO" > "ATOM", so Token0 should be ATOM, Token1 OSMO
        let msg = ExecuteMsg::CreatePool {
            token_a: native("OSMO"),
            token_b: native("ATOM"),
            fee: 500,
            init_sqrt_price: init_price,
        };

        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();

        // 1. Verify Submessage
        assert_eq!(res.messages.len(), 1);
        let sub_msg = &res.messages[0];
        assert_eq!(sub_msg.id, 1); // Reply ID

        match &sub_msg.msg {
            CosmosMsg::Wasm(WasmMsg::Instantiate2 {
                code_id,
                msg,
                funds,
                label,
                salt,
                ..
            }) => {
                assert_eq!(*code_id, 123);
                assert_eq!(funds.len(), 0);
                assert_eq!(label, "Choice CLMM Pool ATOM/OSMO");

                let pool_msg: PoolInstantiateMsg = from_json(msg).unwrap();
                assert_eq!(pool_msg.token0, native("ATOM"));
                assert_eq!(pool_msg.token1, native("OSMO"));
                assert_eq!(pool_msg.fee_config.base_fee_ppm, 500);
                assert_eq!(pool_msg.tick_spacing, 10);

                let mut hasher = Sha256::new();
                hasher.update("ATOM".as_bytes());
                hasher.update("OSMO".as_bytes());
                hasher.update(500u32.to_le_bytes());
                let expected_salt = hasher.finalize().to_vec();
                assert_eq!(salt.as_slice(), expected_salt.as_slice());
            }
            _ => panic!("Unexpected message type"),
        }
    }

    #[test]
    fn test_create_pool_reply() {
        let mut deps = setup_factory();

        // 1. Setup State manually (simulate what execute_create_pool does)
        // Token0: ATOM, Token1: OSMO, Fee: 500
        crate::contract::TMP_POOL_INFO
            .save(
                &mut deps.storage,
                &("ATOM".to_string(), "OSMO".to_string(), 500),
            )
            .unwrap();

        // 2. Mock the Reply from Instantiate2
        let pool_addr = deps.api.addr_make("osmo1pooladdress");
        #[allow(deprecated)]
        let reply_msg = Reply {
            id: 1,
            result: SubMsgResult::Ok(SubMsgResponse {
                events: vec![
                    Event::new("instantiate").add_attribute("_contract_address", &pool_addr)
                ],
                data: None,            // Still needed even if deprecated
                msg_responses: vec![], // FIX: New field in 2.0
            }),
            payload: Binary::default(), // FIX: New field in 2.0
            gas_used: 0,                // FIX: New field in 2.0
        };

        let res = reply(deps.as_mut(), mock_env(), reply_msg).unwrap();

        // 3. Verify Response
        assert_eq!(
            res.attributes,
            vec![attr("pool_address", pool_addr.to_string())]
        );

        // 4. Verify Registry Update
        let stored_addr = POOLS.load(&deps.storage, ("ATOM", "OSMO", 500)).unwrap();
        assert_eq!(stored_addr, Addr::unchecked(pool_addr));

        // 5. Verify Temp storage cleanup
        assert!(crate::contract::TMP_POOL_INFO.load(&deps.storage).is_err());
    }

    #[test]
    fn test_create_pool_duplicate() {
        let mut deps = setup_factory();

        // Manually save a pool in the registry
        POOLS
            .save(
                &mut deps.storage,
                ("ATOM", "OSMO", 500),
                &Addr::unchecked("existing_pool"),
            )
            .unwrap();

        let info = message_info(&deps.api.addr_make("user"), &[]);
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: Uint256::zero(),
        };

        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Pool already exists");
    }
}

#[cfg(test)]
mod tests {
    use crate::contract::{execute, instantiate, query, reply};
    use crate::state::{CONFIG, FEE_TIERS, POOLS, POOL_CREATION_AUTH};
    use choice_clmm_common::factory::{
        CreationAuthResponse, ExecuteMsg, FlashBorrowersResponse, InstantiateMsg,
        IsFlashBorrowerResponse, QueryMsg,
    };
    use choice_clmm_common::pool::InstantiateMsg as PoolInstantiateMsg;
    use choice_clmm_common::types::AssetInfo;
    use cosmwasm_std::testing::{
        message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
    };
    use cosmwasm_std::{
        attr, from_json, Addr, CosmosMsg, Event, OwnedDeps, Reply, SubMsgResponse, SubMsgResult,
        Uint256, WasmMsg,
    };
    use sha2::{Digest, Sha256};
    use std::str::FromStr;

    fn native(denom: &str) -> AssetInfo {
        AssetInfo::NativeToken {
            denom: denom.to_string(),
        }
    }

    /// A `factory/{owner}/{sub}` native denom owned by `owner`.
    fn factory_denom(owner: &Addr, sub: &str) -> AssetInfo {
        native(&format!("factory/{}/{}", owner, sub))
    }

    /// 2^96 — a valid (price == 1.0) init sqrt price.
    fn init_price() -> Uint256 {
        Uint256::from(79228162514264337593543950336u128)
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

        // 3. Parameter-validation REJECT side. All from the OWNER, so each failure
        //    is attributable to the param (not auth), and each asserts the EXACT
        //    error so it cannot pass for an unrelated reason. These guard the
        //    `tick_spacing as i32` cast and the fee bounds the pool relies on.
        let creator = deps.api.addr_make("creator");
        let cases: &[(u32, u32, &str)] = &[
            (0, 5, "Generic error: Fee must be > 0 and < 1_000_000"),
            (
                1_000_000,
                5,
                "Generic error: Fee must be > 0 and < 1_000_000",
            ),
            (300, 0, "Generic error: Tick spacing must be in 1..=16384"),
            (
                300,
                16385,
                "Generic error: Tick spacing must be in 1..=16384",
            ),
        ];
        for (fee, tick_spacing, want) in cases {
            let err = execute(
                deps.as_mut(),
                mock_env(),
                message_info(&creator, &[]),
                ExecuteMsg::EnableFeeAmount {
                    fee: *fee,
                    tick_spacing: *tick_spacing,
                },
            )
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                *want,
                "fee={} tick_spacing={}",
                fee,
                tick_spacing
            );
        }

        // 4. Duplicate fee tier (250 was just enabled) is rejected as an overwrite,
        //    and the original spacing is left untouched.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&creator, &[]),
            ExecuteMsg::EnableFeeAmount {
                fee: 250,
                tick_spacing: 7,
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Fee tier already enabled");
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

                // Salt is derived from the variant-qualified registry keys
                // (`n:` native / `c:` cw20) so a native denom can't collide
                // with a CW20 address of the same string.
                let mut hasher = Sha256::new();
                hasher.update("n:ATOM".as_bytes());
                hasher.update("n:OSMO".as_bytes());
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

        // 1. The registry key now rides in the SubMsg payload. Build it the SAME
        // way `execute_create_pool` does — via `registry_key()` (the `n:` native
        // prefix) — so this reply test stays consistent with the producer instead
        // of hand-writing raw, un-prefixed keys the producer never emits.
        let key0 = native("ATOM").registry_key();
        let key1 = native("OSMO").registry_key();
        assert_eq!(key0, "n:ATOM");
        assert_eq!(key1, "n:OSMO");
        let payload = cosmwasm_std::to_json_binary(&(key0.clone(), key1.clone(), 500u32)).unwrap();

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
            payload, // registry key carried by the submessage
            gas_used: 0,
        };

        let res = reply(deps.as_mut(), mock_env(), reply_msg).unwrap();

        // 3. Verify Response
        assert_eq!(
            res.attributes,
            vec![attr("pool_address", pool_addr.to_string())]
        );

        // 4. Verify Registry Update under the producer's variant-qualified key.
        let stored_addr = POOLS
            .load(&deps.storage, (key0.as_str(), key1.as_str(), 500))
            .unwrap();
        assert_eq!(stored_addr, Addr::unchecked(pool_addr));
        // No temp storage to clean up — the key rode in the SubMsg payload.
    }

    #[test]
    fn test_create_pool_duplicate() {
        let mut deps = setup_factory();

        // Manually save a pool in the registry, under the variant-qualified
        // keys the contract uses (`n:` native prefix).
        POOLS
            .save(
                &mut deps.storage,
                ("n:ATOM", "n:OSMO", 500),
                &Addr::unchecked("existing_pool"),
            )
            .unwrap();

        let info = message_info(&deps.api.addr_make("user"), &[]);
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: init_price(),
        };

        let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Pool already exists");
    }

    // ----------------------------------------------------------------------
    // Anti-squat creation gate
    // ----------------------------------------------------------------------

    fn authorize(
        deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier>,
        sender: &Addr,
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
        creator: &Addr,
        ttl_seconds: u64,
    ) -> Result<cosmwasm_std::Response, cosmwasm_std::StdError> {
        let msg = ExecuteMsg::AuthorizeCreation {
            token_a,
            token_b,
            fee,
            creator: creator.to_string(),
            ttl_seconds,
        };
        execute(deps.as_mut(), mock_env(), message_info(sender, &[]), msg)
    }

    fn create_pool(
        deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier>,
        env: cosmwasm_std::Env,
        sender: &Addr,
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    ) -> Result<cosmwasm_std::Response, cosmwasm_std::StdError> {
        let msg = ExecuteMsg::CreatePool {
            token_a,
            token_b,
            fee,
            init_sqrt_price: init_price(),
        };
        execute(deps.as_mut(), env, message_info(sender, &[]), msg)
    }

    #[test]
    fn test_open_creation_unchanged_without_auth() {
        // No reservation → anyone can create, exactly as before.
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let stranger = deps.api.addr_make("stranger");
        let res = create_pool(
            &mut deps,
            mock_env(),
            &stranger,
            factory_denom(&issuer, "shroom_1"),
            native("inj"),
            500,
        )
        .unwrap();
        // Pool instantiate submsg emitted (gate is a no-op).
        assert_eq!(res.messages.len(), 1);
    }

    #[test]
    fn test_authorize_by_namespace_owner_and_query() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let denom = factory_denom(&issuer, "shroom_1");

        // ttl == 0 → no expiry.
        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            0,
        )
        .unwrap();

        // Query reflects it (and tolerates swapped token order).
        let bin = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::GetCreationAuth {
                token_a: native("inj"),
                token_b: denom,
                fee: 500,
            },
        )
        .unwrap();
        let auth: Option<CreationAuthResponse> = from_json(bin).unwrap();
        let auth = auth.expect("reservation present");
        assert_eq!(auth.creator, sink.to_string());
        assert_eq!(auth.expires_at, u64::MAX);
    }

    #[test]
    fn test_authorize_rejected_for_non_owner() {
        // An attacker who doesn't own the namespace can't reserve the slot.
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let attacker = deps.api.addr_make("attacker");
        let err = authorize(
            &mut deps,
            &attacker,
            factory_denom(&issuer, "shroom_1"),
            native("inj"),
            500,
            &attacker,
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);
    }

    #[test]
    fn test_authorize_rejected_for_foreign_blue_chip_pair() {
        // No `factory/{attacker}/…` side ⇒ nothing the attacker can claim.
        let mut deps = setup_factory();
        let attacker = deps.api.addr_make("attacker");
        let err = authorize(
            &mut deps,
            &attacker,
            native("uusdc"),
            native("inj"),
            500,
            &attacker,
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);
    }

    #[test]
    fn test_authorize_by_factory_owner() {
        // Governance (factory owner) can reserve any slot, incl. blue-chip pairs.
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator"); // setup_factory's instantiator
        let chosen = deps.api.addr_make("chosen");
        authorize(
            &mut deps,
            &owner,
            native("uusdc"),
            native("inj"),
            500,
            &chosen,
            0,
        )
        .unwrap();
        assert!(POOL_CREATION_AUTH
            .may_load(&deps.storage, ("n:inj", "n:uusdc", 500))
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_gate_blocks_unauthorized_creator_then_allows_and_consumes() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let squatter = deps.api.addr_make("squatter");
        let denom = factory_denom(&issuer, "shroom_1");

        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            0,
        )
        .unwrap();

        // Squatter is blocked (note: passes tokens in the opposite order).
        let err = create_pool(
            &mut deps,
            mock_env(),
            &squatter,
            native("inj"),
            denom.clone(),
            500,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Generic error: pool slot reserved for another creator"
        );

        // Authorized sink succeeds and the reservation is consumed.
        let res = create_pool(&mut deps, mock_env(), &sink, denom, native("inj"), 500).unwrap();
        assert_eq!(res.messages.len(), 1);
        let denom_str = format!("factory/{}/shroom_1", issuer);
        assert!(POOL_CREATION_AUTH
            .may_load(&deps.storage, (denom_str.as_str(), "inj", 500))
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_finite_ttl_expiry_reopens_slot() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let squatter = deps.api.addr_make("squatter");
        let denom = factory_denom(&issuer, "shroom_1");

        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            100,
        )
        .unwrap();

        // Past expiry, the slot reopens to anyone and the stale entry is swept.
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(101);
        let res = create_pool(&mut deps, env, &squatter, denom, native("inj"), 500).unwrap();
        assert_eq!(res.messages.len(), 1);
        let denom_str = format!("factory/{}/shroom_1", issuer);
        assert!(POOL_CREATION_AUTH
            .may_load(&deps.storage, (denom_str.as_str(), "inj", 500))
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_ttl_zero_never_expires() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let squatter = deps.api.addr_make("squatter");
        let denom = factory_denom(&issuer, "shroom_1");

        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            0,
        )
        .unwrap();

        // Even far in the future, a non-creator is still blocked.
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(10_000_000_000);
        let err = create_pool(&mut deps, env, &squatter, denom, native("inj"), 500).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Generic error: pool slot reserved for another creator"
        );
    }

    #[test]
    fn test_authorize_unsupported_fee_tier_rejected() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let err = authorize(
            &mut deps,
            &issuer,
            factory_denom(&issuer, "shroom_1"),
            native("inj"),
            999_999, // not an enabled tier
            &sink,
            0,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Fee tier not supported");
    }

    #[test]
    fn test_cancel_creation_auth() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let denom = factory_denom(&issuer, "shroom_1");

        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            0,
        )
        .unwrap();

        // Cancel by the namespace owner releases the slot.
        let msg = ExecuteMsg::CancelCreationAuth {
            token_a: denom.clone(),
            token_b: native("inj"),
            fee: 500,
        };
        execute(deps.as_mut(), mock_env(), message_info(&issuer, &[]), msg).unwrap();
        let denom_str = format!("factory/{}/shroom_1", issuer);
        assert!(POOL_CREATION_AUTH
            .may_load(&deps.storage, (denom_str.as_str(), "inj", 500))
            .unwrap()
            .is_none());

        // Cancelling a non-existent reservation errors.
        let msg = ExecuteMsg::CancelCreationAuth {
            token_a: denom,
            token_b: native("inj"),
            fee: 500,
        };
        let err = execute(deps.as_mut(), mock_env(), message_info(&issuer, &[]), msg).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Generic error: No reservation for this slot"
        );
    }

    #[test]
    fn test_cancel_rejected_for_non_owner() {
        let mut deps = setup_factory();
        let issuer = deps.api.addr_make("issuer");
        let sink = deps.api.addr_make("sink");
        let attacker = deps.api.addr_make("attacker");
        let denom = factory_denom(&issuer, "shroom_1");

        authorize(
            &mut deps,
            &issuer,
            denom.clone(),
            native("inj"),
            500,
            &sink,
            0,
        )
        .unwrap();

        let msg = ExecuteMsg::CancelCreationAuth {
            token_a: denom,
            token_b: native("inj"),
            fee: 500,
        };
        let err =
            execute(deps.as_mut(), mock_env(), message_info(&attacker, &[]), msg).unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);
    }

    // ----------------------------------------------------------------------
    // C-L2 — two-step owner transfer
    // ----------------------------------------------------------------------

    #[test]
    fn owner_transfer_requires_propose_then_accept() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let new_owner = deps.api.addr_make("new_owner");
        let random = deps.api.addr_make("random");

        // UpdateConfig no longer carries owner/pool_code_id and changes nothing,
        // but is still owner-gated.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::UpdateConfig {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // A non-owner cannot propose.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::ProposeOwner {
                new_owner: new_owner.to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // Owner proposes; ownership has NOT yet moved.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeOwner {
                new_owner: new_owner.to_string(),
            },
        )
        .unwrap();
        let config = CONFIG.load(&deps.storage).unwrap();
        assert_eq!(config.owner, owner);
        assert_eq!(config.pending_owner, Some(new_owner.clone()));

        // A random caller (not the pending owner) cannot accept.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::AcceptOwner {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);

        // The pending owner accepts; ownership moves and pending clears.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&new_owner, &[]),
            ExecuteMsg::AcceptOwner {},
        )
        .unwrap();
        let config = CONFIG.load(&deps.storage).unwrap();
        assert_eq!(config.owner, new_owner);
        assert_eq!(config.pending_owner, None);

        // The OLD owner can no longer act (e.g. enable a fee tier).
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::EnableFeeAmount {
                fee: 250,
                tick_spacing: 5,
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // The NEW owner can act.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&new_owner, &[]),
            ExecuteMsg::EnableFeeAmount {
                fee: 250,
                tick_spacing: 5,
            },
        )
        .unwrap();
    }

    #[test]
    fn accept_owner_without_proposal_fails() {
        let mut deps = setup_factory();
        let random = deps.api.addr_make("random");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::AcceptOwner {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: No pending owner");
    }

    #[test]
    fn propose_owner_rejects_dead_address() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposeOwner {
                new_owner: "inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqe2hm49".to_string(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("dead burn address"), "{}", err);
    }

    // ----------------------------------------------------------------------
    // C-L3 — two-step pool_code_id repoint
    // ----------------------------------------------------------------------

    #[test]
    fn pool_code_id_change_requires_propose_then_accept() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let random = deps.api.addr_make("random");

        // Sanity: starts at 123 (setup_factory).
        assert_eq!(CONFIG.load(&deps.storage).unwrap().pool_code_id, 123);

        // Non-owner cannot propose.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::ProposePoolCodeId { code_id: 456 },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // Owner proposes; live code id unchanged, pending set.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::ProposePoolCodeId { code_id: 456 },
        )
        .unwrap();
        let config = CONFIG.load(&deps.storage).unwrap();
        assert_eq!(config.pool_code_id, 123);
        assert_eq!(config.pending_pool_code_id, Some(456));

        // Non-owner cannot accept.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&random, &[]),
            ExecuteMsg::AcceptPoolCodeId {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // Owner accepts; live code id repointed, pending cleared, event emitted.
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::AcceptPoolCodeId {},
        )
        .unwrap();
        assert!(res.attributes.contains(&attr("old_pool_code_id", "123")));
        assert!(res.attributes.contains(&attr("new_pool_code_id", "456")));
        let config = CONFIG.load(&deps.storage).unwrap();
        assert_eq!(config.pool_code_id, 456);
        assert_eq!(config.pending_pool_code_id, None);
    }

    #[test]
    fn accept_pool_code_id_without_proposal_fails() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::AcceptPoolCodeId {},
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Generic error: No pending pool code id");
    }

    // ----------------------------------------------------------------------
    // C-L7 — init_sqrt_price validation on CreatePool
    // ----------------------------------------------------------------------

    #[test]
    fn create_pool_rejects_zero_and_out_of_range_init_sqrt_price() {
        let mut deps = setup_factory();
        let info = message_info(&deps.api.addr_make("user"), &[]);

        // 1. Zero is rejected.
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: Uint256::zero(),
        };
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap_err();
        assert!(
            err.to_string().contains("init_sqrt_price out of range"),
            "{}",
            err
        );

        // 2. Just below MIN_SQRT_RATIO (4295128739) is rejected.
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: Uint256::from(4295128738u128),
        };
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap_err();
        assert!(
            err.to_string().contains("init_sqrt_price out of range"),
            "{}",
            err
        );

        // 3. At/above the exclusive upper bound is rejected.
        let max = Uint256::from_str("1461446703485210103287273052203988822378723970342").unwrap();
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: max,
        };
        let err = execute(deps.as_mut(), mock_env(), info.clone(), msg).unwrap_err();
        assert!(
            err.to_string().contains("init_sqrt_price out of range"),
            "{}",
            err
        );

        // 4. The inclusive lower bound MIN_SQRT_RATIO is accepted (proceeds to
        //    instantiate submsg).
        let msg = ExecuteMsg::CreatePool {
            token_a: native("ATOM"),
            token_b: native("OSMO"),
            fee: 500,
            init_sqrt_price: Uint256::from(4295128739u128),
        };
        let res = execute(deps.as_mut(), mock_env(), info, msg).unwrap();
        assert_eq!(res.messages.len(), 1);
    }

    fn is_flash_borrower(
        deps: &OwnedDeps<MockStorage, MockApi, MockQuerier>,
        borrower: &Addr,
    ) -> bool {
        let bin = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::IsFlashBorrower {
                borrower: borrower.to_string(),
            },
        )
        .unwrap();
        from_json::<IsFlashBorrowerResponse>(&bin)
            .unwrap()
            .authorized
    }

    #[test]
    fn test_flash_borrower_allowlist_owner_gated() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let borrower = deps.api.addr_make("aggregator");
        let stranger = deps.api.addr_make("stranger");

        // Default: deny-all.
        assert!(!is_flash_borrower(&deps, &borrower));

        // Non-owner cannot authorize.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&stranger, &[]),
            ExecuteMsg::AuthorizeFlashBorrower {
                borrower: borrower.to_string(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);

        // Owner authorizes → allowlisted.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::AuthorizeFlashBorrower {
                borrower: borrower.to_string(),
            },
        )
        .unwrap();
        assert!(is_flash_borrower(&deps, &borrower));
        assert!(!is_flash_borrower(&deps, &stranger));

        // Owner revokes → denied again.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::RevokeFlashBorrower {
                borrower: borrower.to_string(),
            },
        )
        .unwrap();
        assert!(!is_flash_borrower(&deps, &borrower));
    }

    #[test]
    fn test_flash_unrestricted_opens_gate_for_all() {
        let mut deps = setup_factory();
        let owner = deps.api.addr_make("creator");
        let anyone = deps.api.addr_make("anyone");

        assert!(!is_flash_borrower(&deps, &anyone));

        // Non-owner cannot toggle.
        let err = execute(
            deps.as_mut(),
            mock_env(),
            message_info(&anyone, &[]),
            ExecuteMsg::SetFlashUnrestricted { open: true },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Unauthorized"), "{}", err);

        // Owner opens flash globally → anyone is authorized without being listed.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::SetFlashUnrestricted { open: true },
        )
        .unwrap();
        assert!(is_flash_borrower(&deps, &anyone));

        // The listing query reports the flag and (still-empty) explicit set.
        let bin = query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::FlashBorrowers {
                start_after: None,
                limit: None,
            },
        )
        .unwrap();
        let resp = from_json::<FlashBorrowersResponse>(&bin).unwrap();
        assert!(resp.unrestricted);
        assert!(resp.borrowers.is_empty());

        // Closing it again restores deny-all.
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&owner, &[]),
            ExecuteMsg::SetFlashUnrestricted { open: false },
        )
        .unwrap();
        assert!(!is_flash_borrower(&deps, &anyone));
    }
}

#![cfg(test)]

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use choice::asset::{Asset, AssetInfo};
    use choice::send_to_auction::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockQuerier, MockStorage};
    use cosmwasm_std::{
        from_json, Addr, Api, Binary, Coin, Empty, MessageInfo, OwnedDeps, StdError,
    };
    use cw20::Cw20ReceiveMsg;

    use crate::contract::{execute, query};
    use crate::instantiate;
    use crate::state::{load_config, Config};

    pub fn mock_dependencies() -> OwnedDeps<MockStorage, MockApi, MockQuerier, Empty> {
        OwnedDeps {
            storage: MockStorage::default(),
            api: MockApi::default().with_prefix("inj"),
            querier: MockQuerier::default(),
            custom_query_type: PhantomData,
        }
    }

    #[test]
    fn test_instantiate_contract() {
        let mut deps = mock_dependencies();

        let env = mock_env();

        // Use addr_make to generate valid Injective addresses
        let admin_addr = deps.api.addr_make("admin-seed");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let info = MessageInfo {
            sender: admin_addr.clone(),
            funds: vec![],
        };

        let msg = InstantiateMsg {
            owner: admin_addr.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };

        // Call the instantiate function
        let res = instantiate(deps.as_mut(), env, info, msg).unwrap();

        // Assert that the response is successful
        assert_eq!(res.messages.len(), 0); // No messages expected on instantiation

        // Load the stored config
        let config = load_config(deps.as_ref()).unwrap();

        // Assert the stored values are correct
        assert_eq!(
            config.owner,
            deps.api.addr_canonicalize(&admin_addr.to_string()).unwrap()
        );
        assert_eq!(config.adapter_contract, adapter_contract_addr.to_string());
        assert_eq!(
            config.burn_auction_subaccount,
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn test_send_native_via_execute() {
        let mut deps = mock_dependencies();

        let mut env = mock_env();
        env.contract.address = Addr::unchecked("inj1l2gcrfr6aenjyt5jddk79j7w5v0twskw6n70y8");

        let admin_addr = deps.api.addr_make("admin-seed");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let admin_info = MessageInfo {
            sender: admin_addr.clone(),
            funds: vec![Coin {
                denom: "inj".to_string(),
                amount: 1000u128.into(),
            }],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            owner: admin_addr.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), admin_info.clone(), msg).unwrap();

        // Prepare the ExecuteMsg::SendNative message
        let asset = Asset {
            info: AssetInfo::NativeToken {
                denom: "inj".to_string(),
            },
            amount: 1000u128.into(),
        };
        let execute_msg = ExecuteMsg::SendNative { asset };

        // Call the execute function
        let res = execute(deps.as_mut(), env.clone(), admin_info, execute_msg).unwrap();

        // Assert the response attributes
        assert_eq!(res.attributes, vec![("action", "send_native")]);

        // Assert that the appropriate messages were created
        assert_eq!(res.messages.len(), 2); // Deposit and Transfer messages
    }

    #[test]
    fn test_receive_cw20_via_execute() {
        let mut deps = mock_dependencies();

        let contract_address = "inj1l2gcrfr6aenjyt5jddk79j7w5v0twskw6n70y8";
        let mut env = mock_env();
        env.contract.address = Addr::unchecked(contract_address);

        let admin_info = MessageInfo {
            sender: Addr::unchecked("inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"),
            funds: vec![],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            owner: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
            adapter_contract: "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk".to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), admin_info.clone(), msg).unwrap();

        // Prepare the ExecuteMsg::Receive message
        let cw20_sender = "inj1sendercw20address0000000000000000000000000000000";
        let cw20_contract = "inj1cw20contractaddress000000000000000000000000000";
        let cw20_amount = 1000u128;

        let receive_msg = Cw20ReceiveMsg {
            sender: cw20_sender.to_string(),
            amount: cw20_amount.into(),
            msg: Binary::default(), // Use an empty Binary
        };

        let execute_msg = ExecuteMsg::Receive(receive_msg);

        let cw20_info = MessageInfo {
            sender: Addr::unchecked(cw20_contract),
            funds: vec![], // No native funds should be sent in a CW20 message
        };

        // Call the execute function
        let res = execute(deps.as_mut(), env.clone(), cw20_info, execute_msg).unwrap();

        // Assert the response attributes
        assert_eq!(
            res.attributes,
            vec![
                ("action", "receive_cw20"),
                ("sender", cw20_sender),
                ("amount", cw20_amount.to_string().as_str())
            ]
        );

        // Assert that the appropriate messages were created
        assert_eq!(res.messages.len(), 3); // Deposit and Transfer messages for converted CW20 tokens
    }

    #[test]
    fn test_query_config() {
        let mut deps = mock_dependencies();

        let env = mock_env();

        // Generate proper mocked Injective addresses
        let admin_addr = deps.api.addr_make("admin-seed");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let info = MessageInfo {
            sender: admin_addr.clone(),
            funds: vec![],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            owner: admin_addr.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(), // 0x + 64 hex chars = valid Injective subaccount
        };
        instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

        // Query the configuration
        let query_msg = QueryMsg::GetConfig {};
        let res = query(deps.as_ref(), env.clone(), query_msg).unwrap();

        // Assert the configuration is correct
        let config: Config = from_json(&res).unwrap();
        assert_eq!(
            config.owner,
            deps.api.addr_canonicalize(&admin_addr.to_string()).unwrap()
        );
        assert_eq!(config.adapter_contract, adapter_contract_addr.to_string());
        assert_eq!(
            config.burn_auction_subaccount,
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn test_invalid_send_native_via_execute() {
        let mut deps = mock_dependencies();

        let contract_address = "inj1l2gcrfr6aenjyt5jddk79j7w5v0twskw6n70y8";
        let mut env = mock_env();
        env.contract.address = Addr::unchecked(contract_address);

        let admin_info = MessageInfo {
            sender: Addr::unchecked("user"),
            funds: vec![],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            owner: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
            adapter_contract: "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk".to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), admin_info.clone(), msg).unwrap();

        // Prepare an invalid asset
        let invalid_asset = Asset {
            info: AssetInfo::Token {
                contract_addr: "invalid_token_address".to_string(),
            },
            amount: 1000u128.into(),
        };

        // Prepare the ExecuteMsg::SendNative message
        let execute_msg = ExecuteMsg::SendNative {
            asset: invalid_asset,
        };

        // Call the execute function and expect an error
        let err = execute(deps.as_mut(), env, admin_info, execute_msg).unwrap_err();

        // Assert the error message
        assert_eq!(
            err.to_string(),
            "Generic error: Invalid asset: Expected a native token"
        );
    }

    #[test]
    fn propose_and_accept_ownership() {
        let mut deps = mock_dependencies();

        let mut env = mock_env();
        env.contract.address = Addr::unchecked("inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq9w87h3"); // Important for send_native tests

        let owner = deps.api.addr_make("owner0000");
        let proposed_owner = deps.api.addr_make("newowner0000");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let info = message_info(&owner, &[]);

        let msg = InstantiateMsg {
            owner: owner.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };

        instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

        // Propose a new owner
        let propose_msg = ExecuteMsg::ProposeNewOwner {
            new_owner: proposed_owner.to_string(),
        };

        let res = execute(deps.as_mut(), env.clone(), info.clone(), propose_msg).unwrap();
        assert_eq!(
            res.attributes,
            vec![
                ("action", "propose_new_owner"),
                ("proposed_owner", proposed_owner.as_str())
            ]
        );

        // Make sure proposed_owner is set
        let config = load_config(deps.as_ref()).unwrap();
        assert_eq!(
            config.proposed_owner.unwrap().to_string(),
            proposed_owner.to_string()
        );

        // Accept ownership by the proposed owner
        let accept_info = message_info(&proposed_owner, &[]);
        let accept_msg = ExecuteMsg::AcceptOwnership;
        let res = execute(deps.as_mut(), env.clone(), accept_info.clone(), accept_msg).unwrap();
        assert_eq!(
            res.attributes,
            vec![
                ("action", "accept_ownership"),
                ("new_owner", proposed_owner.as_str())
            ]
        );

        // Verify ownership is updated
        let config = load_config(deps.as_ref()).unwrap();
        assert_eq!(
            config.owner,
            deps.api.addr_canonicalize(proposed_owner.as_str()).unwrap()
        );
        assert_eq!(config.proposed_owner, None);
    }

    #[test]
    fn unauthorized_propose_new_owner() {
        let mut deps = mock_dependencies();

        let mut env = mock_env();
        env.contract.address = Addr::unchecked("inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq9w87h3");

        let owner = deps.api.addr_make("owner0000");
        let unauthorized = deps.api.addr_make("badactor0000");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let info = message_info(&owner, &[]);
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

        // Try to propose a new owner from a non-owner
        let propose_msg = ExecuteMsg::ProposeNewOwner {
            new_owner: deps.api.addr_make("newowner0000").to_string(),
        };

        let bad_info = message_info(&unauthorized, &[]);
        let res = execute(deps.as_mut(), env.clone(), bad_info, propose_msg);

        match res {
            Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "Unauthorized"),
            _ => panic!("Must return unauthorized error"),
        }
    }

    #[test]
    fn unauthorized_accept_ownership() {
        let mut deps = mock_dependencies();

        let mut env = mock_env();
        env.contract.address = Addr::unchecked("inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq9w87h3");

        let owner = deps.api.addr_make("owner0000");
        let proposed_owner = deps.api.addr_make("newowner0000");
        let bad_actor = deps.api.addr_make("badactor0000");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let info = message_info(&owner, &[]);
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

        // Owner proposes a new owner
        let propose_msg = ExecuteMsg::ProposeNewOwner {
            new_owner: proposed_owner.to_string(),
        };
        execute(deps.as_mut(), env.clone(), info.clone(), propose_msg).unwrap();

        // Try to accept ownership with a wrong account
        let bad_info = message_info(&bad_actor, &[]);
        let accept_msg = ExecuteMsg::AcceptOwnership;
        let res = execute(deps.as_mut(), env.clone(), bad_info, accept_msg);

        match res {
            Err(StdError::GenericErr { msg, .. }) => {
                assert_eq!(msg, "No ownership proposal for you")
            }
            _ => panic!("Must fail with 'No ownership proposal for you'"),
        }
    }

    #[test]
    fn test_cancel_ownership_proposal() {
        let mut deps = mock_dependencies();

        let owner = deps.api.addr_make("owner0000");
        let proposed_owner = deps.api.addr_make("newowner0000");
        let adapter_contract_addr = deps.api.addr_make("adapter-seed");

        let env = mock_env();
        let info = message_info(&owner, &[]);

        // Instantiate
        let msg = InstantiateMsg {
            owner: owner.to_string(),
            adapter_contract: adapter_contract_addr.to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

        // Propose new owner
        let propose_msg = ExecuteMsg::ProposeNewOwner {
            new_owner: proposed_owner.to_string(),
        };
        execute(deps.as_mut(), env.clone(), info.clone(), propose_msg).unwrap();

        // Cancel proposal
        let cancel_msg = ExecuteMsg::CancelOwnershipProposal;
        let res = execute(deps.as_mut(), env.clone(), info.clone(), cancel_msg).unwrap();
        assert_eq!(
            res.attributes,
            vec![
                ("action", "cancel_ownership_proposal"),
                ("owner", owner.as_str()),
            ]
        );

        // Verify proposal is cleared
        let config = load_config(deps.as_ref()).unwrap();
        assert_eq!(config.proposed_owner, None);
    }

    #[test]
    fn test_update_config_fields() {
        let mut deps = mock_dependencies();

        // Prepare initial addresses
        let owner = deps.api.addr_make("owner0000");
        let initial_adapter = deps.api.addr_make("adapter-old");
        let initial_subaccount =
            "0x1111111111111111111111111111111111111111111111111111111111111111";

        // Instantiate the contract
        let env = mock_env();
        let info = message_info(&owner, &[]);
        let instantiate_msg = InstantiateMsg {
            owner: owner.to_string(),
            adapter_contract: initial_adapter.to_string(),
            burn_auction_subaccount: initial_subaccount.to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), instantiate_msg).unwrap();

        // Prepare new values
        let new_adapter = deps.api.addr_make("adapter-new");
        let new_subaccount = "0x2222222222222222222222222222222222222222222222222222222222222222";

        // Owner calls UpdateConfig
        let update_msg = ExecuteMsg::UpdateConfig {
            adapter_contract: Some(new_adapter.to_string()),
            burn_auction_subaccount: Some(new_subaccount.to_string()),
        };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), update_msg).unwrap();
        // Check that attributes reflect the update
        assert_eq!(
            res.attributes,
            vec![
                ("action", "update_config"),
                ("adapter_contract", new_adapter.as_str()),
                ("burn_auction_subaccount", new_subaccount),
            ]
        );

        // Verify state was updated
        let cfg = load_config(deps.as_ref()).unwrap();
        assert_eq!(cfg.adapter_contract, new_adapter.to_string());
        assert_eq!(cfg.burn_auction_subaccount, new_subaccount.to_string());

        // Unauthorized caller should fail
        let bad_actor = deps.api.addr_make("badactor0000");
        let bad_info = message_info(&bad_actor, &[]);
        let fail_msg = ExecuteMsg::UpdateConfig {
            adapter_contract: Some(deps.api.addr_make("wont-take").to_string()),
            burn_auction_subaccount: None,
        };
        match execute(deps.as_mut(), env, bad_info, fail_msg) {
            Err(StdError::GenericErr { msg, .. }) => assert_eq!(msg, "Unauthorized"),
            _ => panic!("Expected unauthorized error"),
        }
    }
}

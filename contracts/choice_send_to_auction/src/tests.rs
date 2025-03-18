#![cfg(test)]

#[cfg(test)]
mod tests {
    use choice::asset::{Asset, AssetInfo};
    use choice::send_to_auction::{ExecuteMsg, InstantiateMsg, QueryMsg};
    use cosmwasm_std::testing::{mock_dependencies, mock_env};
    use cosmwasm_std::{from_json, Addr, Binary, Coin, MessageInfo};
    use cw20::Cw20ReceiveMsg;

    use crate::contract::{execute, query};
    use crate::instantiate;
    use crate::state::{load_config, Config};

    #[test]
    fn test_instantiate_contract() {
        let mut deps = mock_dependencies();

        let env = mock_env();
        let info = MessageInfo {
            sender: Addr::unchecked("inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"),
            funds: vec![],
        };

        let msg = InstantiateMsg {
            admin: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
            adapter_contract: "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk".to_string(),
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
        assert_eq!(config.admin, "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz");
        assert_eq!(
            config.adapter_contract,
            "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk"
        );
        assert_eq!(
            config.burn_auction_subaccount,
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn test_update_admin_via_execute() {
        let mut deps = mock_dependencies();

        let env = mock_env();
        let initial_admin = "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz";
        let new_admin = "inj1a2b3c4d5e6f7g8h9i0jklmnopqrstuvwx0yz";
        let non_admin = "inj1notallowedtochange";

        // Instantiate the contract with the initial admin
        let info = MessageInfo {
            sender: Addr::unchecked(initial_admin),
            funds: vec![],
        };
        let msg = InstantiateMsg {
            admin: initial_admin.to_string(),
            adapter_contract: "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk".to_string(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), msg).unwrap();

        // Attempt to update the admin as a non-admin (should fail)
        let non_admin_info = MessageInfo {
            sender: Addr::unchecked(non_admin),
            funds: vec![],
        };
        let update_msg = ExecuteMsg::UpdateAdmin {
            admin: new_admin.to_string(),
        };
        let err = execute(deps.as_mut(), env.clone(), non_admin_info, update_msg).unwrap_err();
        assert_eq!(err.to_string(), "Generic error: Unauthorized");

        // Attempt to update the admin as the current admin (should succeed)
        let update_msg = ExecuteMsg::UpdateAdmin {
            admin: new_admin.to_string(),
        };
        let res = execute(deps.as_mut(), env.clone(), info, update_msg).unwrap();
        assert_eq!(res.attributes, vec![("action", "update_admin")]);

        // Verify the admin was updated
        let config = load_config(deps.as_ref()).unwrap();
        assert_eq!(config.admin, new_admin);
    }

    #[test]
    fn test_send_native_via_execute() {
        let mut deps = mock_dependencies();

        let contract_address = "inj1l2gcrfr6aenjyt5jddk79j7w5v0twskw6n70y8";
        let mut env = mock_env();
        env.contract.address = Addr::unchecked(contract_address);

        let admin_info = MessageInfo {
            sender: Addr::unchecked("inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"),
            funds: vec![Coin {
                denom: "inj".to_string(),
                amount: 1000u128.into(),
            }],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            admin: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
            adapter_contract: "inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk".to_string(),
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

        // for (i, msg) in res.messages.iter().enumerate() {
        //     println!("Message {}: {:?}", i + 1, msg);
        // }
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
            admin: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
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

        for (i, msg) in res.messages.iter().enumerate() {
            println!("Message {}: {:?}", i + 1, msg);
        }
    }

    #[test]
    fn test_query_config() {
        let mut deps = mock_dependencies();

        let env = mock_env();
        let info = MessageInfo {
            sender: Addr::unchecked("admin"),
            funds: vec![],
        };

        // Instantiate the contract
        let msg = InstantiateMsg {
            admin: "admin_address".to_string(),
            adapter_contract: "adapter_contract_address".to_string(),
            burn_auction_subaccount: "burn_subaccount".to_string(),
        };
        instantiate(deps.as_mut(), env.clone(), info, msg).unwrap();

        // Query the configuration
        let query_msg = QueryMsg::GetConfig {};
        let res = query(deps.as_ref(), env.clone(), query_msg).unwrap();

        // Assert the configuration is correct
        let config: Config = from_json(&res).unwrap();
        assert_eq!(config.admin, "admin_address");
        assert_eq!(config.adapter_contract, "adapter_contract_address");
        assert_eq!(config.burn_auction_subaccount, "burn_subaccount");
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
            admin: "inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz".to_string(),
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
}

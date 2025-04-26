use schemars::JsonSchema;

use crate::state::{load_config, save_config, Config};
use choice::asset::{Asset, AssetInfo};
use choice::send_to_auction::{ExecuteMsg, InstantiateMsg, QueryMsg};
use cosmwasm_std::{
    entry_point, to_json_binary, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Response, StdError, StdResult, Uint128, WasmMsg,
};
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use injective_cosmwasm::exchange::subaccount::checked_address_to_subaccount_id;
use injective_cosmwasm::exchange::types::SubaccountId;
use injective_cosmwasm::{InjectiveMsg, InjectiveMsgWrapper, InjectiveRoute};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub enum AdapterExecuteMsg {
    Receive {
        sender: String,
        amount: Uint128,
        msg: Option<Binary>,
    },
}

pub fn get_burn_auction_subaccount(deps: Deps) -> StdResult<SubaccountId> {
    let config = load_config(deps)?;
    let burn_auction_subaccount = config.burn_auction_subaccount;

    SubaccountId::new(burn_auction_subaccount)
        .map_err(|_| StdError::generic_err("Invalid burn auction sub_account ID"))
}

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let admin = deps.api.addr_validate(&msg.admin)?;
    let adapter_contract = deps.api.addr_validate(&msg.adapter_contract)?;
    let sub_account = SubaccountId::new(msg.burn_auction_subaccount)?;

    let config = Config {
        admin: admin.to_string(),
        adapter_contract: adapter_contract.to_string(),
        burn_auction_subaccount: sub_account.to_string(),
    };
    save_config(deps, &config)?;
    Ok(Response::default())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::SendNative { asset } => send_native(deps, env, info, asset),
        ExecuteMsg::UpdateAdmin { admin } => update_admin(deps, info, admin),
    }
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetConfig {} => {
            let config = load_config(deps)?;
            to_json_binary(&config)
        }
    }
}

fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: Cw20ReceiveMsg,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    let contract_addr = info.sender.clone();
    let burn_amount = msg.amount;

    // Call send_to_burn_auction with the CW20 token info
    send_to_burn_auction(
        deps,
        env,
        info,
        Asset {
            info: AssetInfo::Token {
                contract_addr: contract_addr.to_string(),
            },
            amount: burn_amount,
        },
        &mut messages,
    )?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "receive_cw20")
        .add_attribute("sender", msg.sender)
        .add_attribute("amount", burn_amount.to_string()))
}

pub fn send_native(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    asset: Asset,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];

    if !asset.info.is_native_token() {
        return Err(StdError::generic_err(
            "Invalid asset: Expected a native token",
        ));
    }

    send_to_burn_auction(deps, env, info, asset, &mut messages)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "send_native"))
}

fn update_admin(
    deps: DepsMut,
    info: MessageInfo,
    admin: String,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let config = load_config(deps.as_ref())?;

    // Only the current admin can update
    if info.sender.to_string() != config.admin {
        return Err(StdError::generic_err("Unauthorized"));
    }

    let admin = deps.api.addr_validate(&admin)?;
    let new_config = Config { admin: admin.to_string(), ..config };
    save_config(deps, &new_config)?;

    Ok(Response::new().add_attribute("action", "update_admin"))
}

pub fn send_to_burn_auction(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    asset: Asset,
    messages: &mut Vec<CosmosMsg<InjectiveMsgWrapper>>,
) -> StdResult<()> {
    let burn_auction_subaccount_obj = get_burn_auction_subaccount(deps.as_ref())?;

    let config = load_config(deps.as_ref())?;
    let cw20_adapter_address = config.adapter_contract.clone();

    let burn_amount = asset.amount;
    let asset_info = asset.info;

    if asset_info.is_native_token() {
        if info.funds.is_empty() {
            return Err(StdError::generic_err("No funds provided"));
        }

        let provided_funds = info
            .funds
            .iter()
            .find(|coin| coin.denom == asset_info.to_string());
        match provided_funds {
            Some(coin) => {
                if coin.amount != burn_amount {
                    return Err(StdError::generic_err(format!(
                        "Mismatched fund amount: expected {}, provided {}",
                        burn_amount, coin.amount
                    )));
                }
            }
            None => {
                return Err(StdError::generic_err(format!(
                    "Mismatched denomination: expected {}, but no matching funds provided",
                    asset_info
                )));
            }
        }

        // Native token handling
        let subaccount_id = checked_address_to_subaccount_id(&env.contract.address, 1);
        let deposit_msg = CosmosMsg::Custom(InjectiveMsgWrapper {
            route: InjectiveRoute::Exchange,
            msg_data: InjectiveMsg::Deposit {
                sender: env.contract.address.clone(),
                subaccount_id: subaccount_id.clone(),
                amount: Coin {
                    denom: asset_info.to_string(),
                    amount: burn_amount,
                },
            },
        });
        messages.push(deposit_msg);

        let transfer_msg = CosmosMsg::Custom(InjectiveMsgWrapper {
            route: InjectiveRoute::Exchange,
            msg_data: InjectiveMsg::ExternalTransfer {
                sender: env.contract.address,
                source_subaccount_id: subaccount_id,
                destination_subaccount_id: burn_auction_subaccount_obj,
                amount: Coin {
                    denom: asset_info.to_string(),
                    amount: burn_amount,
                },
            },
        });
        messages.push(transfer_msg);
    } else {
        // CW20 token handling
        let cw20_address = match &asset_info {
            AssetInfo::Token { contract_addr } => contract_addr.to_string(),
            AssetInfo::NativeToken { .. } => {
                return Err(StdError::generic_err("Expected token address"))
            }
        };

        let subaccount_id = checked_address_to_subaccount_id(&env.contract.address, 1);
        let converted_native_denom = format!("factory/{}/{}", cw20_adapter_address, cw20_address);

        let adapter_msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: cw20_address.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: cw20_adapter_address.to_string(),
                amount: burn_amount,
                msg: Binary::default(),
            })?,
            funds: vec![],
        });
        messages.push(adapter_msg);

        // After conversion, prepare the deposit message
        let deposit_msg = CosmosMsg::Custom(InjectiveMsgWrapper {
            route: InjectiveRoute::Exchange,
            msg_data: InjectiveMsg::Deposit {
                sender: env.contract.address.clone(),
                subaccount_id: subaccount_id.clone(),
                amount: Coin {
                    denom: converted_native_denom.clone(),
                    amount: burn_amount,
                },
            },
        });
        messages.push(deposit_msg);

        // Transfer to the burn auction sub account
        let transfer_msg = CosmosMsg::Custom(InjectiveMsgWrapper {
            route: InjectiveRoute::Exchange,
            msg_data: InjectiveMsg::ExternalTransfer {
                sender: env.contract.address,
                source_subaccount_id: subaccount_id,
                destination_subaccount_id: burn_auction_subaccount_obj,
                amount: Coin {
                    denom: converted_native_denom,
                    amount: burn_amount,
                },
            },
        });
        messages.push(transfer_msg);
    }

    Ok(())
}

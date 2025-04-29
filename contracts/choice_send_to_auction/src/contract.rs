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
        .map_err(|_| StdError::generic_err("Invalid burn auction subaccount ID"))
}

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let owner = deps.api.addr_validate(&msg.owner)?;
    let adapter_contract = deps.api.addr_validate(&msg.adapter_contract)?;
    let sub_account = SubaccountId::new(msg.burn_auction_subaccount)?;

    let config = Config {
        owner: deps.api.addr_canonicalize(owner.as_str())?,
        adapter_contract: adapter_contract.to_string(),
        burn_auction_subaccount: sub_account.to_string(),
        proposed_owner: None,
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
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, info, new_owner)
        }
        ExecuteMsg::AcceptOwnership => execute_accept_ownership(deps, info),
        ExecuteMsg::CancelOwnershipProposal => execute_cancel_ownership_proposal(deps, info),
        ExecuteMsg::UpdateConfig {
            adapter_contract,
            burn_auction_subaccount,
        } => execute_update_config(deps, info, adapter_contract, burn_auction_subaccount),
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

pub fn execute_propose_new_owner(
    deps: DepsMut,
    info: MessageInfo,
    new_owner: String,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let mut config = load_config(deps.as_ref())?;

    // Only current owner can propose
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    let validated = deps.api.addr_validate(&new_owner)?;
    config.proposed_owner = Some(validated);
    save_config(deps, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_new_owner")
        .add_attribute("proposed_owner", new_owner))
}

pub fn execute_accept_ownership(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let mut config = load_config(deps.as_ref())?;

    match config.proposed_owner {
        Some(proposed) if proposed == info.sender => {
            config.owner = deps.api.addr_canonicalize(info.sender.as_str())?;
            config.proposed_owner = None; // clear proposed owner
            save_config(deps, &config)?;

            Ok(Response::new()
                .add_attribute("action", "accept_ownership")
                .add_attribute("new_owner", info.sender.to_string()))
        }
        _ => Err(StdError::generic_err("No ownership proposal for you")),
    }
}

pub fn execute_cancel_ownership_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    let mut config = load_config(deps.as_ref())?;

    // Only current owner can cancel
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    // Clear the proposed owner
    config.proposed_owner = None;

    save_config(deps, &config)?;

    Ok(Response::new()
        .add_attribute("action", "cancel_ownership_proposal")
        .add_attribute("owner", info.sender))
}

pub fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    adapter_contract: Option<String>,
    burn_auction_subaccount: Option<String>,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    // load and validate owner
    let mut cfg = load_config(deps.as_ref())?;
    let caller = deps.api.addr_canonicalize(info.sender.as_ref())?;
    if caller != cfg.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    // apply updates
    if let Some(addr) = adapter_contract {
        // validate new CW20 adapter address
        deps.api.addr_validate(&addr)?;
        cfg.adapter_contract = addr;
    }
    if let Some(sa) = burn_auction_subaccount {
        // validate new subaccount ID (66-char 0x-prefixed)
        SubaccountId::new(sa.clone()).map_err(|_| StdError::generic_err("Invalid subaccount"))?;
        cfg.burn_auction_subaccount = sa;
    }

    // persist
    save_config(deps, &cfg)?;
    Ok(Response::new()
        .add_attribute("action", "update_config")
        .add_attribute("adapter_contract", cfg.adapter_contract.clone())
        .add_attribute(
            "burn_auction_subaccount",
            cfg.burn_auction_subaccount.clone(),
        ))
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

    let subaccount_id = checked_address_to_subaccount_id(&env.contract.address, 1);

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

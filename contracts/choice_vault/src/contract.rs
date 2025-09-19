use choice::asset::{Asset, AssetInfo};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Binary, CosmosMsg, Deps, DepsMut, Env, MessageInfo, Reply, ReplyOn,
    Response, StdResult, SubMsg, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

use choice::pair::{Cw20HookMsg as PairCw20HookMsg, ExecuteMsg as PairExecuteMsg};
use choice::querier::{query_balance, query_token_balance};
use choice::staking::{ExecuteMsg as FarmExecuteMsg, QueryMsg as FarmQueryMsg, StakerInfoResponse};

use crate::error::ContractError;
use crate::msg::{Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg, UserInfoResponse};
use crate::state::{Config, UserInfo, CONFIG, TOTAL_SHARES, USERS};

const CONTRACT_NAME: &str = "crates.io:choice-vault";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HARVEST_REPLY_ID: u64 = 1;
pub const SWAP_REPLY_ID: u64 = 2;
pub const PROVIDE_LIQUIDITY_REPLY_ID: u64 = 3;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let owner_addr = deps.api.addr_validate(&msg.owner)?;
    let pair_contract_addr = deps.api.addr_validate(&msg.pair_contract)?;
    let farm_contract_addr = deps.api.addr_validate(&msg.farm_contract)?;
    let lp_token_addr = deps.api.addr_validate(&msg.lp_token)?;

    let config = Config {
        owner: owner_addr,
        pair_contract: pair_contract_addr,
        farm_contract: farm_contract_addr,
        lp_token: lp_token_addr,
        reward_token: msg.reward_token,
        asset_infos: msg.asset_infos,
    };

    CONFIG.save(deps.storage, &config)?;
    TOTAL_SHARES.save(deps.storage, &Uint128::zero())?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", msg.owner)
        .add_attribute("pair_contract", msg.pair_contract)
        .add_attribute("farm_contract", msg.farm_contract))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::Withdraw { shares } => execute_withdraw(deps, env, info, shares),
        ExecuteMsg::Compound {} => execute_compound(deps, env, info),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> Result<Response, ContractError> {
    match msg.id {
        HARVEST_REPLY_ID => handle_harvest_reply(deps, env),
        SWAP_REPLY_ID => handle_swap_reply(deps, env),
        PROVIDE_LIQUIDITY_REPLY_ID => handle_provide_liquidity_reply(deps, env),
        _ => Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Unknown reply id",
        ))),
    }
}

pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    if info.sender != config.lp_token {
        return Err(ContractError::Unauthorized {});
    }

    match from_json(&cw20_msg.msg)? {
        Cw20HookMsg::Deposit {} => execute_deposit(deps, env, cw20_msg.sender, cw20_msg.amount),
    }
}

pub fn execute_deposit(
    deps: DepsMut,
    env: Env,
    sender: String,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let mut total_shares = TOTAL_SHARES.load(deps.storage)?;
    let sender_addr = deps.api.addr_validate(&sender)?;

    // Query the farm contract to find out the total amount of LP tokens
    // our vault currently has staked.
    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: None, // Use current block time
        },
    )?;
    let total_lp_staked = staker_info.bond_amount;

    // Calculate the number of shares to mint.
    // This is proportional to the current ratio of shares to LP tokens.
    let shares_to_mint = if total_shares.is_zero() || total_lp_staked.is_zero() {
        // If we are the first depositor, 1 LP token = 1 share.
        amount
    } else {
        // Otherwise, shares = (amount * total_shares) / total_lp_staked
        amount.multiply_ratio(total_shares, total_lp_staked)
    };

    if shares_to_mint.is_zero() {
        // This can happen due to rounding if a very small amount is deposited.
        // We should reject it to prevent dust deposits.
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Received zero shares for the deposit",
        )));
    }

    // Update the user's share count and the total shares.
    USERS.update(
        deps.storage,
        &sender_addr,
        |user_info| -> StdResult<UserInfo> {
            let mut info = user_info.unwrap_or_default();
            info.shares += shares_to_mint;
            Ok(info)
        },
    )?;
    total_shares += shares_to_mint;
    TOTAL_SHARES.save(deps.storage, &total_shares)?;

    // Now that the shares are minted, create a message to stake the received
    // LP tokens into the farm contract.
    // The message is a Cw20 Send message to the LP token contract, which in turn
    // calls the farm contract's Receive hook with a "Bond" message.
    let bond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.lp_token.to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::Send {
            contract: config.farm_contract.to_string(),
            amount,
            msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_message(bond_msg)
        .add_attribute("action", "deposit")
        .add_attribute("depositor", sender)
        .add_attribute("lp_amount", amount.to_string())
        .add_attribute("shares_minted", shares_to_mint.to_string()))
}

pub fn execute_withdraw(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    shares: Uint128,
) -> Result<Response, ContractError> {
    if shares.is_zero() {
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Cannot withdraw zero shares",
        )));
    }

    let config = CONFIG.load(deps.storage)?;
    let sender_addr = info.sender;

    // Decrease the user's share balance, checking for sufficient funds
    let user_info = USERS.update(
        deps.storage,
        &sender_addr,
        |user| -> Result<_, ContractError> {
            match user {
                Some(mut user_info) => {
                    user_info.shares = user_info
                        .shares
                        .checked_sub(shares)
                        .map_err(|_| ContractError::InsufficientShares {})?;
                    Ok(user_info)
                }
                None => Err(ContractError::InsufficientShares {}),
            }
        },
    )?;

    // If the user's shares are now zero, remove them from storage to save gas.
    if user_info.shares.is_zero() {
        USERS.remove(deps.storage, &sender_addr);
    }

    // Update total shares by subtracting the burnt shares
    let total_shares_before_burn = TOTAL_SHARES.load(deps.storage)?;
    TOTAL_SHARES.save(deps.storage, &(total_shares_before_burn - shares))?;

    // Query the farm contract to get the vault's total LP balance
    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: None,
        },
    )?;
    let total_lp_staked = staker_info.bond_amount;

    // Calculate the amount of LP tokens to redeem
    // lp_to_withdraw = (shares_to_burn * total_lp_staked) / total_shares_before_burn
    let lp_to_withdraw = shares.multiply_ratio(total_lp_staked, total_shares_before_burn);

    // --- Message Generation ---
    // The withdrawal is a two-step process executed atomically:
    // 1. Vault tells the Farm to `unbond`. The Farm sends LP tokens to the Vault.
    // 2. Vault immediately sends those newly received LP tokens to the user.

    // Message 1: Unbond from the farm contract.
    let unbond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.farm_contract.to_string(),
        msg: to_json_binary(&FarmExecuteMsg::Unbond {
            amount: lp_to_withdraw,
        })?,
        funds: vec![],
    });

    // Message 2: Transfer the withdrawn LP tokens from our vault to the user.
    let transfer_lp_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.lp_token.to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
            recipient: sender_addr.to_string(),
            amount: lp_to_withdraw,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_messages(vec![unbond_msg, transfer_lp_msg])
        .add_attribute("action", "withdraw")
        .add_attribute("withdrawer", sender_addr.to_string())
        .add_attribute("shares_burnt", shares.to_string())
        .add_attribute("lp_amount_withdrawn", lp_to_withdraw.to_string()))
}

pub fn execute_compound(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: None,
        },
    )?;

    if staker_info.pending_reward.is_zero() {
        return Ok(Response::new()
            .add_attribute("action", "compound")
            .add_attribute("status", "no_rewards"));
    }

    let harvest_msg = SubMsg {
        id: HARVEST_REPLY_ID,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.farm_contract.to_string(),
            msg: to_json_binary(&FarmExecuteMsg::Withdraw {})?,
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload: Binary::default(),
    };

    Ok(Response::new()
        .add_submessage(harvest_msg)
        .add_attribute("action", "compound")
        .add_attribute("status", "step_1_harvest_initiated"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::TotalShares {} => to_json_binary(&query_total_shares(deps)?),
        QueryMsg::UserInfo { user } => to_json_binary(&query_user_info(deps, user)?),
    }
}

fn query_config(deps: Deps) -> StdResult<Config> {
    CONFIG.load(deps.storage)
}

fn query_total_shares(deps: Deps) -> StdResult<Uint128> {
    TOTAL_SHARES.load(deps.storage)
}

fn query_user_info(deps: Deps, user: String) -> StdResult<UserInfoResponse> {
    let user_addr = deps.api.addr_validate(&user)?;
    let user_info = USERS
        .may_load(deps.storage, &user_addr)?
        .unwrap_or_default();
    Ok(UserInfoResponse {
        shares: user_info.shares,
    })
}

// This function is called after the HARVEST is successful
pub fn handle_harvest_reply(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    let reward_asset_info = config.reward_token.clone();

    // Use the querier functions from the 'choice' library to get the balance
    let reward_balance = match reward_asset_info.clone() {
        AssetInfo::Token { contract_addr } => query_token_balance(
            &deps.querier,
            deps.api.addr_validate(&contract_addr)?,
            env.contract.address.clone(),
        )?,
        AssetInfo::NativeToken { denom } => {
            query_balance(&deps.querier, env.contract.address.clone(), denom)?
        }
    };

    if reward_balance.is_zero() {
        return Ok(Response::new().add_attribute("status", "no_rewards_after_harvest"));
    }

    let amount_to_swap = reward_balance.multiply_ratio(1u128, 2u128);
    let offer_asset = Asset {
        info: reward_asset_info,
        amount: amount_to_swap,
    };

    let swap_cosmos_msg = match &offer_asset.info {
        AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.pair_contract.to_string(),
            msg: to_json_binary(&PairExecuteMsg::Swap {
                offer_asset: offer_asset.clone(),
                belief_price: None,
                max_spread: None,
                to: None,
                deadline: None,
            })?,
            funds: vec![cosmwasm_std::Coin {
                denom: denom.clone(),
                amount: offer_asset.amount,
            }],
        }),
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: config.pair_contract.to_string(),
                amount: offer_asset.amount,
                msg: to_json_binary(&PairCw20HookMsg::Swap {
                    belief_price: None,
                    max_spread: None,
                    to: None,
                    deadline: None,
                })?,
            })?,
            funds: vec![],
        }),
    };

    let swap_sub_msg = SubMsg {
        id: SWAP_REPLY_ID,
        msg: swap_cosmos_msg,
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload: Binary::default(),
    };

    Ok(Response::new()
        .add_submessage(swap_sub_msg)
        .add_attribute("status", "step_2_swap_initiated")
        .add_attribute("amount_to_swap", amount_to_swap))
}

pub fn handle_swap_reply(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // Query balances for both assets in the pair using the provided library functions
    let mut assets_to_provide: [Asset; 2] = [
        Asset {
            info: config.asset_infos[0].clone(),
            amount: Uint128::zero(),
        },
        Asset {
            info: config.asset_infos[1].clone(),
            amount: Uint128::zero(),
        },
    ];

    for asset in &mut assets_to_provide {
        asset.amount = match &asset.info {
            AssetInfo::Token { contract_addr } => query_token_balance(
                &deps.querier,
                deps.api.addr_validate(contract_addr)?,
                env.contract.address.clone(),
            )?,
            AssetInfo::NativeToken { denom } => {
                query_balance(&deps.querier, env.contract.address.clone(), denom.clone())?
            }
        };
    }

    // Collect native token funds to send with the ProvideLiquidity message
    let funds: Vec<_> = assets_to_provide
        .iter()
        .filter_map(|a| {
            if let AssetInfo::NativeToken { denom } = &a.info {
                if !a.amount.is_zero() {
                    return Some(cosmwasm_std::Coin {
                        denom: denom.clone(),
                        amount: a.amount,
                    });
                }
            }
            None
        })
        .collect();

    let provide_liquidity_msg = SubMsg {
        id: PROVIDE_LIQUIDITY_REPLY_ID,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: config.pair_contract.to_string(),
            msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
                assets: assets_to_provide,
                receiver: None,
                deadline: None,
                slippage_tolerance: None,
            })?,
            funds,
        }),
        gas_limit: None,
        reply_on: ReplyOn::Success,
        payload: Binary::default(),
    };

    Ok(Response::new()
        .add_submessage(provide_liquidity_msg)
        .add_attribute("status", "step_3_provide_liquidity_initiated"))
}

pub fn handle_provide_liquidity_reply(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // LP tokens are always CW20, so we use query_token_balance
    let new_lp_balance =
        query_token_balance(&deps.querier, config.lp_token.clone(), env.contract.address)?;

    if new_lp_balance.is_zero() {
        return Ok(Response::new().add_attribute("status", "no_lp_tokens_received"));
    }

    let bond_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: config.lp_token.to_string(),
        msg: to_json_binary(&Cw20ExecuteMsg::Send {
            contract: config.farm_contract.to_string(),
            amount: new_lp_balance,
            msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_message(bond_msg)
        .add_attribute("action", "compound")
        .add_attribute("status", "step_4_complete")
        .add_attribute("lp_tokens_staked", new_lp_balance))
}

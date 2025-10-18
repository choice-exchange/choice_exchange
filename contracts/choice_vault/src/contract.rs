use choice::asset::{Asset, AssetInfo};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Binary, CosmosMsg, Decimal, Deps, DepsMut, Env, MessageInfo, Reply,
    ReplyOn, Response, StdError, StdResult, SubMsg, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

use choice::pair::{Cw20HookMsg as PairCw20HookMsg, ExecuteMsg as PairExecuteMsg};
use choice::querier::{query_balance, query_token_balance};
use choice::staking::{ExecuteMsg as FarmExecuteMsg, QueryMsg as FarmQueryMsg, StakerInfoResponse};

use crate::error::ContractError;
use crate::msg::{
    CompoundPayload, Cw20HookMsg, ExecuteMsg, InstantiateMsg, QueryMsg, UserInfoResponse,
};
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

    let fee_recipient_addr = if let Some(fee_recipient) = &msg.fee_recipient {
        Some(deps.api.addr_validate(fee_recipient)?)
    } else {
        None
    };

    if let Some(fee_percentage) = msg.fee_percentage {
        if fee_percentage > Decimal::one() {
            return Err(ContractError::InvalidFeePercentage {});
        }
    }

    let compounder_addr = deps.api.addr_validate(&msg.compounder)?;

    let config = Config {
        owner: owner_addr,
        pair_contract: pair_contract_addr,
        farm_contract: farm_contract_addr,
        lp_token: msg.lp_token,
        reward_token: msg.reward_token,
        asset_infos: msg.asset_infos,
        fee_recipient: fee_recipient_addr,
        fee_percentage: msg.fee_percentage,
        minimum_reward_to_compound: msg.minimum_reward_to_compound,
        proposed_owner: None,
        compounder: compounder_addr,
        slippage_tolerance: msg.slippage_tolerance,
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
        ExecuteMsg::DepositNativeLp {} => execute_deposit_native_lp(deps, env, info),
        ExecuteMsg::Withdraw { shares } => execute_withdraw(deps, env, info, shares),
        ExecuteMsg::Compound { belief_price } => execute_compound(deps, env, info, belief_price),
        ExecuteMsg::UpdateConfig {
            compounder,
            slippage_tolerance,
            fee_recipient,
            fee_percentage,
            minimum_reward_to_compound,
        } => execute_update_config(
            deps,
            info,
            compounder,
            slippage_tolerance,
            fee_recipient,
            fee_percentage,
            minimum_reward_to_compound,
        ),
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, info, new_owner)
        }
        ExecuteMsg::AcceptOwnership => execute_accept_ownership(deps, info),
        ExecuteMsg::CancelOwnershipProposal => execute_cancel_ownership_proposal(deps, info),
    }
}

pub fn execute_update_config(
    deps: DepsMut,
    info: MessageInfo,
    compounder: Option<String>,
    slippage_tolerance: Option<Decimal>,
    fee_recipient: Option<String>,
    fee_percentage: Option<Decimal>,
    minimum_reward_to_compound: Option<Uint128>,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Only the owner can update the config
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    if let Some(compounder) = compounder {
        config.compounder = deps.api.addr_validate(&compounder)?;
    }

    if let Some(slippage) = slippage_tolerance {
        config.slippage_tolerance = slippage;
    }

    if let Some(fee_recipient) = fee_recipient {
        config.fee_recipient = Some(deps.api.addr_validate(&fee_recipient)?);
    }

    if let Some(fee_percentage) = fee_percentage {
        // Validate that the fee percentage is not greater than 100%
        if fee_percentage > Decimal::one() {
            return Err(ContractError::InvalidFeePercentage {});
        }
        config.fee_percentage = Some(fee_percentage);
    }

    if let Some(minimum_reward) = minimum_reward_to_compound {
        config.minimum_reward_to_compound = minimum_reward;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "update_config"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, msg: Reply) -> Result<Response, ContractError> {
    match msg.id {
        HARVEST_REPLY_ID => handle_harvest_reply(deps, env, msg),
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

    // Check that the configured LP token is the one sending the message
    match config.lp_token {
        AssetInfo::Token { contract_addr } => {
            if info.sender.to_string() != contract_addr {
                return Err(ContractError::Unauthorized {});
            }
        }
        AssetInfo::NativeToken { .. } => {
            // This hook should not be called for native LP tokens
            return Err(ContractError::Std(StdError::generic_err(
                "Receive hook called for a native LP token vault",
            )));
        }
    }

    match from_json(&cw20_msg.msg)? {
        Cw20HookMsg::Deposit {} => execute_deposit(deps, env, cw20_msg.sender, cw20_msg.amount),
    }
}

pub fn execute_deposit_native_lp(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    let native_lp_denom = match config.lp_token {
        AssetInfo::NativeToken { denom } => denom,
        AssetInfo::Token { .. } => {
            return Err(ContractError::Std(StdError::generic_err(
                "Native deposit called for a CW20 LP token vault",
            )));
        }
    };

    // Find the sent native token in the message funds
    let amount = info
        .funds
        .iter()
        .find(|c| c.denom == native_lp_denom)
        .map(|c| c.amount)
        .unwrap_or_else(Uint128::zero);

    if amount.is_zero() {
        return Err(ContractError::Std(StdError::generic_err(
            "No native LP tokens sent for deposit",
        )));
    }

    // Pass the sender's address (the actual depositor) and amount to the common deposit logic
    execute_deposit(deps, env, info.sender.to_string(), amount)
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
    let shares_to_mint = if total_shares.is_zero() || total_lp_staked.is_zero() {
        amount
    } else {
        amount.multiply_ratio(total_shares, total_lp_staked)
    };

    if shares_to_mint.is_zero() {
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

    // After minting shares, create the correct message to stake the received LP tokens.
    let bond_msg = match config.lp_token {
        AssetInfo::Token { contract_addr } => {
            // For CW20 tokens, use the `Send` hook pattern.
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::Send {
                    contract: config.farm_contract.to_string(),
                    amount,
                    msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
                })?,
                funds: vec![],
            })
        }
        AssetInfo::NativeToken { denom } => {
            // For native tokens, call the farm's `Bond` message directly
            // and attach the native coins in the `funds` array.
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: config.farm_contract.to_string(),
                msg: to_json_binary(&FarmExecuteMsg::Bond { amount })?,
                funds: vec![cosmwasm_std::coin(amount.u128(), denom)],
            })
        }
    };

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

    let env_time = Some(env.block.time.seconds());

    // Query the farm contract to get the vault's total LP balance
    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: env_time,
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

    // Create the message to transfer LP tokens back to the user
    let transfer_lp_msg = match config.lp_token {
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: sender_addr.to_string(),
                amount: lp_to_withdraw,
            })?,
            funds: vec![],
        }),
        AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address: sender_addr.to_string(),
            amount: vec![cosmwasm_std::Coin {
                denom,
                amount: lp_to_withdraw,
            }],
        }),
    };

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
    belief_price: Decimal,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    if info.sender != config.compounder {
        return Err(ContractError::Unauthorized {});
    }

    let env_time = Some(env.block.time.seconds());

    let staker_info: StakerInfoResponse = deps.querier.query_wasm_smart(
        config.farm_contract.clone(),
        &FarmQueryMsg::StakerInfo {
            staker: env.contract.address.to_string(),
            block_time: env_time,
        },
    )?;

    if staker_info.pending_reward.is_zero() {
        return Ok(Response::new()
            .add_attribute("action", "compound")
            .add_attribute("status", "no_rewards"));
    }

    let payload = CompoundPayload { belief_price };

    // Ensure rewards are above the minimum threshold.
    // This prevents unprofitable compounding.
    if staker_info.pending_reward < config.minimum_reward_to_compound {
        return Err(ContractError::Std(cosmwasm_std::StdError::generic_err(
            "Pending rewards are below the minimum threshold to compound",
        )));
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
        payload: to_json_binary(&payload)?,
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
pub fn handle_harvest_reply(
    deps: DepsMut,
    env: Env,
    msg: Reply,
) -> Result<Response, ContractError> {
    let payload: CompoundPayload = from_json(&msg.payload)?;
    let belief_price = payload.belief_price;

    let config = CONFIG.load(deps.storage)?;
    let slippage_tolerance = config.slippage_tolerance;
    let reward_asset_info = config.reward_token.clone();

    // Use the querier functions from the 'choice' library to get the balance
    let mut reward_balance = match reward_asset_info.clone() {
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

    let mut messages: Vec<CosmosMsg> = vec![];
    let fee_recipient = config.fee_recipient;
    let fee_percentage = config.fee_percentage;

    // If a fee is configured, calculate it and create a message to send it.
    if let (Some(recipient), Some(percentage)) = (fee_recipient, fee_percentage) {
        let fee_amount = reward_balance.multiply_ratio(
            percentage.atomics(),
            Uint128::new(1_000_000_000_000_000_000u128),
        );

        if !fee_amount.is_zero() {
            let fee_msg = match reward_asset_info.clone() {
                AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr,
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: recipient.to_string(),
                        amount: fee_amount,
                    })?,
                    funds: vec![],
                }),
                AssetInfo::NativeToken { denom } => CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
                    to_address: recipient.to_string(),
                    amount: vec![cosmwasm_std::Coin {
                        denom,
                        amount: fee_amount,
                    }],
                }),
            };

            messages.push(fee_msg);

            reward_balance = reward_balance
                .checked_sub(fee_amount)
                .map_err(StdError::from)?;
        }
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
                belief_price: Some(belief_price),
                max_spread: Some(slippage_tolerance),
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
                   belief_price: Some(belief_price),
                    max_spread: Some(slippage_tolerance),
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
        .add_messages(messages)
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

    let mut messages: Vec<CosmosMsg> = vec![];

    for asset in &assets_to_provide {
        // If the asset is a CW20 token and we have a balance, grant an allowance to the pair contract
        if let AssetInfo::Token { contract_addr } = &asset.info {
            if !asset.amount.is_zero() {
                let allowance_msg = CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                        spender: config.pair_contract.to_string(),
                        amount: asset.amount,
                        expires: None,
                    })?,
                    funds: vec![],
                });
                messages.push(allowance_msg);
            }
        }
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
        .add_messages(messages) // Add the allowance messages before the submessage
        .add_submessage(provide_liquidity_msg)
        .add_attribute("status", "step_3_provide_liquidity_initiated"))
}

pub fn handle_provide_liquidity_reply(deps: DepsMut, env: Env) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // Query the balance of the new LP tokens and create the correct bond message
    let (new_lp_balance, bond_msg) = match config.lp_token {
        // Case 1: The LP token is a CW20 token
        AssetInfo::Token { contract_addr } => {
            // We must validate the string address from the config into an Addr type before querying.
            let lp_token_addr = deps.api.addr_validate(&contract_addr)?;

            let balance = query_token_balance(&deps.querier, lp_token_addr, env.contract.address)?;

            // For CW20, we use the `Send` hook, which calls the farm's `Receive` entry point
            let msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::Send {
                    contract: config.farm_contract.to_string(),
                    amount: balance,
                    msg: to_json_binary(&choice::staking::Cw20HookMsg::Bond {})?,
                })?,
                funds: vec![],
            });
            (balance, msg)
        }
        // Case 2: The LP token is a native token
        AssetInfo::NativeToken { denom } => {
            let balance = query_balance(&deps.querier, env.contract.address, denom.clone())?;

            // For native tokens, we call the farm's `Bond` execute message directly,
            // passing the amount in the message body and the actual coins in the `funds` array.
            let msg = CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: config.farm_contract.to_string(),
                msg: to_json_binary(&FarmExecuteMsg::Bond { amount: balance })?,
                funds: vec![cosmwasm_std::Coin {
                    denom,
                    amount: balance,
                }],
            });
            (balance, msg)
        }
    };

    if new_lp_balance.is_zero() {
        return Ok(Response::new().add_attribute("status", "no_lp_tokens_received"));
    }

    Ok(Response::new()
        .add_message(bond_msg)
        .add_attribute("action", "compound")
        .add_attribute("status", "step_4_complete")
        .add_attribute("lp_tokens_staked", new_lp_balance))
}

/// Creates a proposal to transfer ownership of the contract.
/// Only the current owner can call this.
pub fn execute_propose_new_owner(
    deps: DepsMut,
    info: MessageInfo,
    new_owner: String,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if the sender is the current owner
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    // Validate and store the proposed new owner address
    let new_owner_addr = deps.api.addr_validate(&new_owner)?;
    config.proposed_owner = Some(new_owner_addr);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_new_owner")
        .add_attribute("proposed_owner", new_owner))
}

/// Accepts an ownership transfer proposal.
/// Only the proposed new owner can call this.
pub fn execute_accept_ownership(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if there is a proposal and if the sender is the proposed owner
    match config.proposed_owner {
        Some(proposed) if proposed == info.sender => {
            config.owner = proposed; // The proposed owner is now the new owner
            config.proposed_owner = None; // Clear the proposal
            CONFIG.save(deps.storage, &config)?;

            Ok(Response::new()
                .add_attribute("action", "accept_ownership")
                .add_attribute("new_owner", info.sender.to_string()))
        }
        _ => Err(ContractError::Std(StdError::generic_err(
            "No ownership proposal for this address to accept",
        ))),
    }
}

/// Cancels an ownership transfer proposal.
/// Only the current owner can call this.
pub fn execute_cancel_ownership_proposal(
    deps: DepsMut,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;

    // Check if the sender is the current owner
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized {});
    }

    // Clear the proposed owner
    config.proposed_owner = None;
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "cancel_ownership_proposal")
        .add_attribute("owner", info.sender.to_string()))
}

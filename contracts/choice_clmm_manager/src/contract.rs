#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    ensure, to_json_binary, Binary, Coin, Deps, DepsMut, Env, MessageInfo, Response, StdError,
    Uint128, WasmMsg,
};
use cw2::set_contract_version;

use cw721::msg::NftExtensionMsg;
use cw721::traits::Cw721Query;
use cw721_base::traits::Cw721Execute;
use cw721_metadata_onchain::Cw721MetadataContract;

use choice_clmm_common::factory::QueryMsg as FactoryQueryMsg;
use choice_clmm_common::manager::{ExecuteMsg, InstantiateMsg, Position, QueryMsg};
use choice_clmm_common::pool::{ExecuteMsg as PoolExecuteMsg, PoolState, QueryMsg as PoolQueryMsg};

use choice_clmm_math::liquidity_math::get_liquidity_for_amounts;
use choice_clmm_math::tick_math::get_sqrt_ratio_at_tick;

use crate::error::ContractError;
use crate::state::{Config, CONFIG, POSITIONS, TOKEN_ID_COUNTER};

const CONTRACT_NAME: &str = "crates.io:choice-clmm-manager";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

// Helper structs to fix clippy::too_many_arguments
pub struct MintParams {
    pub token0: String,
    pub token1: String,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub amount0_desired: Uint128,
    pub amount1_desired: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub recipient: Option<String>,
    pub deadline: u64,
}

pub struct IncreaseParams {
    pub token_id: String,
    pub amount0_desired: Uint128,
    pub amount1_desired: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub deadline: u64,
}

pub struct DecreaseParams {
    pub token_id: String,
    pub liquidity: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub deadline: u64,
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let config = Config {
        factory: deps.api.addr_validate(&msg.factory_addr)?,
    };
    CONFIG.save(deps.storage, &config)?;
    TOKEN_ID_COUNTER.save(deps.storage, &1u64)?;

    let cw721_msg = cw721_base::msg::InstantiateMsg {
        name: msg.name,
        symbol: msg.symbol,
        minter: Some(env.contract.address.to_string()), // Manager controls minting
        withdraw_address: None,
        collection_info_extension: None,
        creator: None,
    };

    // Use the simple default contract
    Cw721MetadataContract::default().instantiate(deps, &env, &info, cw721_msg)?;

    Ok(Response::new().add_attribute("action", "instantiate"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    // Helper like in your example
    let base_contract = Cw721MetadataContract::default();

    match msg {
        // --- CUSTOM LOGIC ---
        ExecuteMsg::MintPosition {
            token0,
            token1,
            fee,
            tick_lower,
            tick_upper,
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
            recipient,
            deadline,
        } => execute_mint_position(
            deps,
            env,
            info,
            MintParams {
                token0,
                token1,
                fee,
                tick_lower,
                tick_upper,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
                recipient,
                deadline,
            },
        ),

        ExecuteMsg::IncreaseLiquidity {
            token_id,
            amount0_desired,
            amount1_desired,
            amount0_min,
            amount1_min,
            deadline,
        } => execute_increase_liquidity(
            deps,
            env,
            info,
            IncreaseParams {
                token_id,
                amount0_desired,
                amount1_desired,
                amount0_min,
                amount1_min,
                deadline,
            },
        ),

        ExecuteMsg::DecreaseLiquidity {
            token_id,
            liquidity,
            amount0_min,
            amount1_min,
            deadline,
        } => execute_decrease_liquidity(
            deps,
            env,
            info,
            DecreaseParams {
                token_id,
                liquidity,
                amount0_min,
                amount1_min,
                deadline,
            },
        ),

        ExecuteMsg::Collect {
            token_id,
            recipient,
        } => execute_collect(deps, env, info, token_id, recipient),

        ExecuteMsg::Burn { token_id } => {
            // TODO: Ensure liquidity is zero
            // Remove from our sidecar map
            POSITIONS.remove(deps.storage, &token_id);
            // Delegate burn to NFT contract
            Ok(base_contract.burn_nft(deps, &env, &info, token_id)?)
        }

        // --- STANDARD DELEGATION ---
        // Just forward these to the base contract
        ExecuteMsg::TransferNft {
            recipient,
            token_id,
        } => Ok(base_contract.transfer_nft(deps, &env, &info, recipient, token_id)?),
        ExecuteMsg::SendNft {
            contract,
            token_id,
            msg,
        } => Ok(base_contract.send_nft(deps, &env, &info, contract, token_id, msg)?),
        ExecuteMsg::Approve {
            spender,
            token_id,
            expires,
        } => Ok(base_contract.approve(deps, &env, &info, spender, token_id, expires)?),
        ExecuteMsg::Revoke { spender, token_id } => {
            Ok(base_contract.revoke(deps, &env, &info, spender, token_id)?)
        }
        ExecuteMsg::ApproveAll { operator, expires } => {
            Ok(base_contract.approve_all(deps, &env, &info, operator, expires)?)
        }
        ExecuteMsg::RevokeAll { operator } => {
            Ok(base_contract.revoke_all(deps, &env, &info, operator)?)
        }
    }
}

fn execute_mint_position(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: MintParams, // Changed
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // Resolve Pool
    let pool_addr: String = deps.querier.query_wasm_smart(
        config.factory,
        &FactoryQueryMsg::GetPool {
            token_a: params.token0.clone(), // Use params.field
            token_b: params.token1.clone(),
            fee: params.fee,
        },
    )?;

    // Generate ID
    let token_id_int = TOKEN_ID_COUNTER.load(deps.storage)?;
    TOKEN_ID_COUNTER.save(deps.storage, &(token_id_int + 1))?;
    let token_id = token_id_int.to_string();

    // Funds Check
    let mut funds_to_pool = vec![];
    if !params.amount0_desired.is_zero() {
        funds_to_pool.push(Coin {
            denom: params.token0.clone(),
            amount: params.amount0_desired,
        });
    }
    if !params.amount1_desired.is_zero() {
        funds_to_pool.push(Coin {
            denom: params.token1.clone(),
            amount: params.amount1_desired,
        });
    }
    for req in &funds_to_pool {
        let sent = info
            .funds
            .iter()
            .find(|c| c.denom == req.denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        ensure!(
            sent >= req.amount,
            ContractError::Std(StdError::generic_err("Insufficient funds sent"))
        );
    }

    // Get Slot0
    let slot0: PoolState = deps
        .querier
        .query_wasm_smart(pool_addr.clone(), &PoolQueryMsg::GetSlot0 {})?;

    // Calculate Liquidity
    let sqrt_price_lower = get_sqrt_ratio_at_tick(params.tick_lower)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(params.tick_upper)?;

    let liquidity = get_liquidity_for_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        params.amount0_desired,
        params.amount1_desired,
    )?;

    // Call Pool Mint
    let pool_mint_msg = PoolExecuteMsg::Mint {
        recipient: env.contract.address.to_string(),
        lower_tick: params.tick_lower,
        upper_tick: params.tick_upper,
        amount: liquidity,
        data: None,
    };

    let wasm_msg = WasmMsg::Execute {
        contract_addr: pool_addr.clone(),
        msg: to_json_binary(&pool_mint_msg)?,
        funds: funds_to_pool,
    };

    // Store Logic Data
    let position = Position {
        token0: params.token0,
        token1: params.token1,
        fee: params.fee,
        tick_lower: params.tick_lower,
        tick_upper: params.tick_upper,
        pool_address: pool_addr,
    };
    POSITIONS.save(deps.storage, &token_id, &position)?;

    // Mint NFT
    let metadata: NftExtensionMsg = position.into();
    let self_info = MessageInfo {
        sender: env.contract.address.clone(),
        funds: vec![],
    };

    Cw721MetadataContract::default().mint(
        deps,
        &env,
        &self_info,
        token_id.clone(),
        params.recipient.unwrap_or(info.sender.to_string()),
        None,
        Some(metadata),
    )?;

    Ok(Response::new()
        .add_message(wasm_msg)
        .add_attribute("action", "mint_position")
        .add_attribute("token_id", token_id)
        .add_attribute("liquidity", liquidity))
}

fn execute_increase_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: IncreaseParams, // Changed
) -> Result<Response, ContractError> {
    let position = POSITIONS
        .load(deps.storage, &params.token_id)
        .map_err(|_| ContractError::Std(StdError::generic_err("Position not found")))?;

    // Funds Check
    let mut funds_to_pool = vec![];
    if !params.amount0_desired.is_zero() {
        funds_to_pool.push(Coin {
            denom: position.token0.clone(),
            amount: params.amount0_desired,
        });
    }
    if !params.amount1_desired.is_zero() {
        funds_to_pool.push(Coin {
            denom: position.token1.clone(),
            amount: params.amount1_desired,
        });
    }

    for req in &funds_to_pool {
        let sent = info
            .funds
            .iter()
            .find(|c| c.denom == req.denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        ensure!(
            sent >= req.amount,
            ContractError::Std(StdError::generic_err(
                "Insufficient funds sent for increase"
            ))
        );
    }

    let slot0: PoolState = deps
        .querier
        .query_wasm_smart(position.pool_address.clone(), &PoolQueryMsg::GetSlot0 {})?;

    let sqrt_price_lower = get_sqrt_ratio_at_tick(position.tick_lower)?;
    let sqrt_price_upper = get_sqrt_ratio_at_tick(position.tick_upper)?;

    let liquidity = get_liquidity_for_amounts(
        slot0.sqrt_price,
        sqrt_price_lower,
        sqrt_price_upper,
        params.amount0_desired,
        params.amount1_desired,
    )?;

    let pool_mint_msg = PoolExecuteMsg::Mint {
        recipient: env.contract.address.to_string(),
        lower_tick: position.tick_lower,
        upper_tick: position.tick_upper,
        amount: liquidity,
        data: None,
    };

    let wasm_msg = WasmMsg::Execute {
        contract_addr: position.pool_address,
        msg: to_json_binary(&pool_mint_msg)?,
        funds: funds_to_pool,
    };

    Ok(Response::new()
        .add_message(wasm_msg)
        .add_attribute("action", "increase_liquidity")
        .add_attribute("token_id", params.token_id))
}

fn execute_decrease_liquidity(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    params: DecreaseParams, // Changed
) -> Result<Response, ContractError> {
    let base = Cw721MetadataContract::default();

    let owner_res =
        base.query_owner_of(deps.as_ref(), &env.clone(), params.token_id.clone(), false)?;
    if info.sender.to_string() != owner_res.owner {
        return Err(ContractError::Std(StdError::generic_err("Unauthorized")));
    }

    let position = POSITIONS.load(deps.storage, &params.token_id)?;

    let burn_msg = PoolExecuteMsg::Burn {
        lower_tick: position.tick_lower,
        upper_tick: position.tick_upper,
        amount: params.liquidity, // Use params.liquidity
    };

    let wasm_msg = WasmMsg::Execute {
        contract_addr: position.pool_address,
        msg: to_json_binary(&burn_msg)?,
        funds: vec![],
    };

    Ok(Response::new()
        .add_message(wasm_msg)
        .add_attribute("action", "decrease_liquidity"))
}

fn execute_collect(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_id: String,
    recipient: Option<String>,
) -> Result<Response, ContractError> {
    let base = Cw721MetadataContract::default();

    let owner_res = base.query_owner_of(deps.as_ref(), &env.clone(), token_id.clone(), false)?;
    let owner = owner_res.owner;

    let dest = recipient.unwrap_or(owner.clone());
    if dest != owner && info.sender.to_string() != owner {
        return Err(ContractError::Std(StdError::generic_err("Unauthorized")));
    }

    // Load from sidecar
    let position = POSITIONS.load(deps.storage, &token_id)?;

    let collect_msg = PoolExecuteMsg::Collect {
        recipient: dest,
        lower_tick: position.tick_lower,
        upper_tick: position.tick_upper,
        amount0_requested: Uint128::MAX,
        amount1_requested: Uint128::MAX,
    };

    let wasm_msg = WasmMsg::Execute {
        contract_addr: position.pool_address,
        msg: to_json_binary(&collect_msg)?,
        funds: vec![],
    };

    Ok(Response::new()
        .add_message(wasm_msg)
        .add_attribute("action", "collect"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> Result<Binary, ContractError> {
    let base_contract = Cw721MetadataContract::default();

    match msg {
        // Custom Query for Logic
        QueryMsg::Position { token_id } => {
            let position = POSITIONS.load(deps.storage, &token_id)?;
            Ok(to_json_binary(&position)?)
        }

        QueryMsg::OwnerOf {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::OwnerOf {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Approval {
            token_id,
            spender,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Approval {
                token_id,
                spender,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Approvals {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Approvals {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }

        QueryMsg::NumTokens {} => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::NumTokens {};
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::NftInfo { token_id } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::NftInfo { token_id };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::AllNftInfo {
            token_id,
            include_expired,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::AllNftInfo {
                token_id,
                include_expired,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
        QueryMsg::Tokens {
            owner,
            start_after,
            limit,
        } => {
            let cw721_msg = cw721_metadata_onchain::msg::QueryMsg::Tokens {
                owner,
                start_after,
                limit,
            };
            Ok(base_contract.query(deps, &env, cw721_msg)?)
        }
    }
}

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Reply, Response, StdError, StdResult,
    SubMsg, Uint256, WasmMsg,
};
use cw_storage_plus::Item;
use sha2::{Digest, Sha256}; // Import sha2 for hashing

use crate::state::{Config, CONFIG, FEE_TIERS, POOLS};

// Import the Pool Message definitions
// NOTE: Make sure these imports match your actual package structure
use choice_clmm_common::factory::{ExecuteMsg, InstantiateMsg, QueryMsg};
use choice_clmm_common::pool::{FeeConfig, InstantiateMsg as PoolInstantiateMsg};

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let config = Config {
        owner: info.sender.clone(),
        pool_code_id: msg.pool_code_id,
    };
    CONFIG.save(deps.storage, &config)?;

    // Initialize default fee tiers
    FEE_TIERS.save(deps.storage, 100, &1)?; // 0.01%
    FEE_TIERS.save(deps.storage, 500, &10)?; // 0.05%
    FEE_TIERS.save(deps.storage, 3000, &60)?; // 0.30%
    FEE_TIERS.save(deps.storage, 10000, &200)?; // 1.00%

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("owner", info.sender))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, StdError> {
    match msg {
        // Updated signature to include init_sqrt_price
        ExecuteMsg::CreatePool {
            token_a,
            token_b,
            fee,
            init_sqrt_price,
        } => execute_create_pool(deps, env, token_a, token_b, fee, init_sqrt_price),
        ExecuteMsg::EnableFeeAmount { fee, tick_spacing } => {
            let config = CONFIG.load(deps.storage)?;
            if info.sender != config.owner {
                return Err(StdError::generic_err("Unauthorized"));
            }
            FEE_TIERS.save(deps.storage, fee, &tick_spacing)?;
            Ok(Response::new().add_attribute("action", "enable_fee_amount"))
        }
        ExecuteMsg::UpdateConfig { .. } => unimplemented!(),
    }
}

fn execute_create_pool(
    deps: DepsMut,
    _env: Env,
    token_a: String,
    token_b: String,
    fee: u32,
    init_sqrt_price: Uint256,
) -> Result<Response, StdError> {
    // 1. Sort Tokens
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }
    let (token0, token1) = if token_a < token_b {
        (token_a.clone(), token_b.clone())
    } else {
        (token_b.clone(), token_a.clone())
    };

    // 2. Check existence
    let config = CONFIG.load(deps.storage)?;
    if POOLS.has(deps.storage, (&token0, &token1, fee)) {
        return Err(StdError::generic_err("Pool already exists"));
    }

    let tick_spacing = FEE_TIERS
        .load(deps.storage, fee)
        .map_err(|_| StdError::generic_err("Fee tier not supported"))?;

    // 3. Generate Salt (FIXED: Using Sha256 crate)
    let mut hasher = Sha256::new();
    hasher.update(token0.as_bytes());
    hasher.update(token1.as_bytes());
    hasher.update(fee.to_le_bytes()); // fee is u32
    let salt = Binary::from(hasher.finalize().to_vec());

    // 4. Create FeeConfig
    // We Map the simple fee (u32) to the complex FeeConfig required by the pool
    let fee_config = FeeConfig {
        base_fee_ppm: fee,
        max_fee_ppm: fee * 2,       // Default logic: max is double base
        volatility_multiplier: 100, // Default: 1.0x (no boost initially)
        ema_halflife_seconds: 600,  // Default: 10 minutes
    };

    // 5. Prepare Instantiate Msg (FIXED: Matching your struct definition)
    let pool_instantiate_msg = PoolInstantiateMsg {
        token0: token0.clone(),
        token1: token1.clone(),
        tick_spacing,
        fee_config,
        initial_sqrt_price_x96: init_sqrt_price,
    };

    // 6. Create WasmMsg with Instantiate2
    let wasm_msg = WasmMsg::Instantiate2 {
        admin: None,
        code_id: config.pool_code_id,
        msg: to_json_binary(&pool_instantiate_msg)?,
        funds: vec![],
        label: format!("Choice CLMM Pool {}/{}", token0, token1),
        salt,
    };

    let sub_msg = SubMsg::reply_on_success(wasm_msg, 1);

    TMP_POOL_INFO.save(deps.storage, &(token0.clone(), token1.clone(), fee))?;

    Ok(Response::new()
        .add_submessage(sub_msg)
        .add_attribute("action", "create_pool")
        .add_attribute("token0", token0)
        .add_attribute("token1", token1)
        .add_attribute("fee", fee.to_string()))
}

pub const TMP_POOL_INFO: Item<(String, String, u32)> = Item::new("tmp_pool_info");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id == 1 {
        let res = msg.result.into_result().map_err(StdError::generic_err)?;

        // Find the address. In Wasmd 0.29+, standard event is "instantiate" -> "_contract_address"
        let address_str = res
            .events
            .iter()
            .find(|e| e.ty == "instantiate")
            .and_then(|e| e.attributes.iter().find(|a| a.key == "_contract_address"))
            .map(|a| &a.value)
            .ok_or_else(|| StdError::generic_err("No contract address found in reply"))?;

        let pool_address = deps.api.addr_validate(address_str)?;

        let (token0, token1, fee) = TMP_POOL_INFO.load(deps.storage)?;

        POOLS.save(deps.storage, (&token0, &token1, fee), &pool_address)?;
        TMP_POOL_INFO.remove(deps.storage);

        Ok(Response::new().add_attribute("pool_address", pool_address))
    } else {
        Err(StdError::generic_err("Unknown reply id"))
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::GetPool {
            token_a,
            token_b,
            fee,
        } => {
            let (token0, token1) = if token_a < token_b {
                (token_a, token_b)
            } else {
                (token_b, token_a)
            };

            let pool_address = POOLS.load(deps.storage, (&token0, &token1, fee))?;

            to_json_binary(&pool_address)
        }
    }
}

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Binary, Deps, DepsMut, Env, MessageInfo, Order, Reply, Response, StdError,
    StdResult, SubMsg, Uint256, WasmMsg,
};
use cw_storage_plus::{Bound, Item};
use sha2::{Digest, Sha256};

use crate::state::{Config, CONFIG, FEE_TIERS, POOLS};

use choice_clmm_common::factory::{
    ConfigResponse, ExecuteMsg, FeeTierEntry, InstantiateMsg, MigrateMsg, PoolInfo, QueryMsg,
};
use choice_clmm_common::pool::{FeeConfig, InstantiateMsg as PoolInstantiateMsg};
use choice_clmm_common::types::AssetInfo;

const CONTRACT_NAME: &str = "crates.io:choice-clmm-factory";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

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
            if fee == 0 || fee >= 1_000_000 {
                return Err(StdError::generic_err("Fee must be > 0 and < 1_000_000"));
            }
            if tick_spacing == 0 {
                return Err(StdError::generic_err("Tick spacing must be > 0"));
            }
            FEE_TIERS.save(deps.storage, fee, &tick_spacing)?;
            Ok(Response::new().add_attribute("action", "enable_fee_amount"))
        }
        ExecuteMsg::UpdateConfig {
            owner,
            pool_code_id,
        } => {
            let mut config = CONFIG.load(deps.storage)?;
            if info.sender != config.owner {
                return Err(StdError::generic_err("Unauthorized"));
            }
            if let Some(new_owner) = owner {
                config.owner = deps.api.addr_validate(&new_owner)?;
            }
            if let Some(new_code_id) = pool_code_id {
                config.pool_code_id = new_code_id;
            }
            CONFIG.save(deps.storage, &config)?;
            Ok(Response::new().add_attribute("action", "update_config"))
        }
    }
}

fn execute_create_pool(
    deps: DepsMut,
    _env: Env,
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,
    init_sqrt_price: Uint256,
) -> Result<Response, StdError> {
    // 1. Sort Tokens
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }

    // Validate CW20 addresses
    if let AssetInfo::Token { contract_addr } = &token_a {
        deps.api.addr_validate(contract_addr)?;
    }
    if let AssetInfo::Token { contract_addr } = &token_b {
        deps.api.addr_validate(contract_addr)?;
    }

    let (token0, token1) = if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };

    let key0 = token0.key().to_string();
    let key1 = token1.key().to_string();

    // 2. Check existence
    let config = CONFIG.load(deps.storage)?;
    if POOLS.has(deps.storage, (&key0, &key1, fee)) {
        return Err(StdError::generic_err("Pool already exists"));
    }

    let tick_spacing = FEE_TIERS
        .load(deps.storage, fee)
        .map_err(|_| StdError::generic_err("Fee tier not supported"))?;

    // 3. Generate Salt
    let mut hasher = Sha256::new();
    hasher.update(key0.as_bytes());
    hasher.update(key1.as_bytes());
    hasher.update(fee.to_le_bytes());
    let salt = Binary::from(hasher.finalize().to_vec());

    // 4. Create FeeConfig. `max_fee_change_per_second_ppm` rate-limits how
    // fast the dynamic fee can move across blocks, dampening single-block
    // price-manipulation attacks (see audit MED-14). 100 ppm/sec = 600 ppm
    // per 6-second block, so reaching the max fee from base takes roughly a
    // minute of sustained volatility — short enough to react to real
    // volatility, long enough to make griefing unprofitable.
    let fee_config = FeeConfig {
        base_fee_ppm: fee,
        max_fee_ppm: fee.saturating_mul(2),
        volatility_multiplier: 100,
        ema_halflife_seconds: 600,
        max_fee_change_per_second_ppm: 100,
    };

    // 5. Prepare Instantiate Msg
    let pool_instantiate_msg = PoolInstantiateMsg {
        token0: token0.clone(),
        token1: token1.clone(),
        tick_spacing,
        fee_config,
        initial_sqrt_price: init_sqrt_price,
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

    TMP_POOL_INFO.save(deps.storage, &(key0.clone(), key1.clone(), fee))?;

    Ok(Response::new()
        .add_submessage(sub_msg)
        .add_attribute("action", "create_pool")
        .add_attribute("token0", token0.to_string())
        .add_attribute("token1", token1.to_string())
        .add_attribute("fee", fee.to_string()))
}

pub const TMP_POOL_INFO: Item<(String, String, u32)> = Item::new("tmp_pool_info");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id == 1 {
        let res = msg.result.into_result().map_err(StdError::generic_err)?;

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
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    let version = cw2::get_contract_version(deps.storage)?;
    if version.contract != CONTRACT_NAME {
        return Err(StdError::generic_err(
            "Cannot migrate from different contract",
        ));
    }
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from_version", version.version)
        .add_attribute("to_version", CONTRACT_VERSION))
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

            let key0 = token0.key().to_string();
            let key1 = token1.key().to_string();
            let pool_address = POOLS.load(deps.storage, (&key0, &key1, fee))?;

            to_json_binary(&pool_address)
        }
        QueryMsg::GetAllPools { start_after, limit } => {
            let limit = limit.unwrap_or(30).min(100) as usize;
            let start = start_after
                .as_ref()
                .map(|(t0, t1, f)| Bound::exclusive((t0.as_str(), t1.as_str(), *f)));

            let pools: Vec<PoolInfo> = POOLS
                .range(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|item| {
                    let ((token0, token1, fee), addr) = item?;
                    Ok(PoolInfo {
                        pool_address: addr.to_string(),
                        token0,
                        token1,
                        fee,
                    })
                })
                .collect::<StdResult<_>>()?;

            to_json_binary(&pools)
        }
        QueryMsg::GetConfig {} => {
            let config = CONFIG.load(deps.storage)?;
            to_json_binary(&ConfigResponse {
                owner: config.owner.to_string(),
                pool_code_id: config.pool_code_id,
            })
        }
        QueryMsg::GetFeeTiers { start_after, limit } => {
            let limit = limit.unwrap_or(30).min(100) as usize;
            let start = start_after.map(Bound::exclusive);
            let tiers: Vec<FeeTierEntry> = FEE_TIERS
                .range(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|item| {
                    let (fee, tick_spacing) = item?;
                    Ok(FeeTierEntry { fee, tick_spacing })
                })
                .collect::<StdResult<_>>()?;
            to_json_binary(&tiers)
        }
    }
}

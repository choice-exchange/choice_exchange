#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_json_binary, Addr, Api, Binary, Deps, DepsMut, Env, MessageInfo, Order, Reply, Response,
    StdError, StdResult, SubMsg, Uint256, WasmMsg,
};
use cw_storage_plus::{Bound, Item};
use sha2::{Digest, Sha256};

use crate::state::{Config, PoolCreationAuth, CONFIG, FEE_TIERS, POOLS, POOL_CREATION_AUTH};

use choice_clmm_common::factory::{
    ConfigResponse, CreationAuthResponse, ExecuteMsg, FeeTierEntry, InstantiateMsg, MigrateMsg,
    PoolInfo, QueryMsg,
};
use choice_clmm_common::pool::{FeeConfig, InstantiateMsg as PoolInstantiateMsg};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

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
        pending_owner: None,
        pool_code_id: msg.pool_code_id,
        pending_pool_code_id: None,
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
        } => execute_create_pool(deps, env, info, token_a, token_b, fee, init_sqrt_price),
        ExecuteMsg::AuthorizeCreation {
            token_a,
            token_b,
            fee,
            creator,
            ttl_seconds,
        } => execute_authorize_creation(deps, env, info, token_a, token_b, fee, creator, ttl_seconds),
        ExecuteMsg::CancelCreationAuth {
            token_a,
            token_b,
            fee,
        } => execute_cancel_creation_auth(deps, info, token_a, token_b, fee),
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
            // Reject silent overwrite: existing pools captured `tick_spacing`
            // at instantiate time so they survive unchanged, but indexers and
            // integrators observing the factory would misinterpret historical
            // pools' spacing after a repoint. Fee tiers are meant to be stable.
            if FEE_TIERS.has(deps.storage, fee) {
                return Err(StdError::generic_err("Fee tier already enabled"));
            }
            FEE_TIERS.save(deps.storage, fee, &tick_spacing)?;
            Ok(Response::new()
                .add_attribute("action", "enable_fee_amount")
                .add_attribute("fee", fee.to_string())
                .add_attribute("tick_spacing", tick_spacing.to_string()))
        }
        ExecuteMsg::UpdateConfig {} => {
            // `owner` and `pool_code_id` are intentionally NOT settable here —
            // they move only through the two-step propose/accept handshakes
            // (audit C-L2 / C-L3). This arm currently mutates nothing but keeps
            // the entrypoint + owner gate for future mutable fields.
            let config = CONFIG.load(deps.storage)?;
            if info.sender != config.owner {
                return Err(StdError::generic_err("Unauthorized"));
            }
            Ok(Response::new()
                .add_attribute("action", "update_config")
                .add_attribute("factory_action", "update_config")
                .add_attribute("owner", config.owner.as_str())
                .add_attribute("pool_code_id", config.pool_code_id.to_string()))
        }
        ExecuteMsg::ProposeOwner { new_owner } => execute_propose_owner(deps, info, new_owner),
        ExecuteMsg::AcceptOwner {} => execute_accept_owner(deps, info),
        ExecuteMsg::ProposePoolCodeId { code_id } => {
            execute_propose_pool_code_id(deps, info, code_id)
        }
        ExecuteMsg::AcceptPoolCodeId {} => execute_accept_pool_code_id(deps, info),
    }
}

/// `inj1qqqq…e2hm49` — the 20-zero-byte burn address. Proposing it as the new
/// owner is almost certainly a mistake (it would brick the factory the same way
/// the single-step transfer used to), so reject it up front. See memory
/// `feedback_inj_tokenfactory_admin_revoke_burn_bech32`.
const DEAD_OWNER_ADDRESS: &str = "inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqe2hm49";

/// C-L2 step 1 — owner-only. Records `pending_owner`; no power changes hands
/// until `AcceptOwner`.
fn execute_propose_owner(
    deps: DepsMut,
    info: MessageInfo,
    new_owner: String,
) -> Result<Response, StdError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    if new_owner == DEAD_OWNER_ADDRESS {
        return Err(StdError::generic_err(
            "Refusing to propose the dead burn address as owner",
        ));
    }
    let new_owner = deps.api.addr_validate(&new_owner)?;
    config.pending_owner = Some(new_owner.clone());
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "propose_owner")
        .add_attribute("current_owner", config.owner.as_str())
        .add_attribute("pending_owner", new_owner.as_str()))
}

/// C-L2 step 2 — callable ONLY by `pending_owner`. Promotes it to `owner` and
/// clears the pending slot.
fn execute_accept_owner(deps: DepsMut, info: MessageInfo) -> Result<Response, StdError> {
    let mut config = CONFIG.load(deps.storage)?;
    let pending = config
        .pending_owner
        .clone()
        .ok_or_else(|| StdError::generic_err("No pending owner"))?;
    if info.sender != pending {
        return Err(StdError::generic_err(
            "Unauthorized: only the pending owner may accept",
        ));
    }
    let previous_owner = config.owner.clone();
    config.owner = pending.clone();
    config.pending_owner = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "accept_owner")
        .add_attribute("previous_owner", previous_owner.as_str())
        .add_attribute("new_owner", pending.as_str()))
}

/// C-L3 step 1 — owner-only. Records `pending_pool_code_id`; the live
/// `pool_code_id` is untouched until `AcceptPoolCodeId`.
fn execute_propose_pool_code_id(
    deps: DepsMut,
    info: MessageInfo,
    code_id: u64,
) -> Result<Response, StdError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    config.pending_pool_code_id = Some(code_id);
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "propose_pool_code_id")
        .add_attribute("current_pool_code_id", config.pool_code_id.to_string())
        .add_attribute("pending_pool_code_id", code_id.to_string()))
}

/// C-L3 step 2 — owner-only. Applies the pending pool code id, emitting old→new.
fn execute_accept_pool_code_id(deps: DepsMut, info: MessageInfo) -> Result<Response, StdError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    let new_code_id = config
        .pending_pool_code_id
        .ok_or_else(|| StdError::generic_err("No pending pool code id"))?;
    let old_code_id = config.pool_code_id;
    config.pool_code_id = new_code_id;
    config.pending_pool_code_id = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "accept_pool_code_id")
        .add_attribute("old_pool_code_id", old_code_id.to_string())
        .add_attribute("new_pool_code_id", new_code_id.to_string()))
}

fn execute_create_pool(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,
    init_sqrt_price: Uint256,
) -> Result<Response, StdError> {
    // 1. Sort Tokens
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }

    // C-L7: validate the caller-supplied opening price BEFORE instantiating the
    // pool. Bounds mirror the pool/tick-math invariant: a valid Q64.96 sqrt
    // price is in `[MIN_SQRT_RATIO, max_sqrt_ratio())` (upper bound exclusive,
    // matching `get_tick_at_sqrt_ratio`). Rejecting 0 / out-of-range here turns
    // an opaque nested instantiate revert into a clear factory-level error.
    if init_sqrt_price < Uint256::from(MIN_SQRT_RATIO) || init_sqrt_price >= max_sqrt_ratio() {
        return Err(StdError::generic_err(
            "init_sqrt_price out of range: must be in [MIN_SQRT_RATIO, MAX_SQRT_RATIO)",
        ));
    }

    // Validate CW20 addresses
    if let AssetInfo::Token { contract_addr } = &token_a {
        deps.api.addr_validate(contract_addr)?;
    }
    if let AssetInfo::Token { contract_addr } = &token_b {
        deps.api.addr_validate(contract_addr)?;
    }

    let (token0, token1) = sort_tokens(token_a, token_b);
    let key0 = token0.registry_key();
    let key1 = token1.registry_key();

    // 2. Anti-squat gate. If this slot is reserved (and unexpired), only the
    //    authorized creator may proceed; otherwise creation is fully
    //    permissionless, exactly as before. Either way the entry is consumed
    //    (a matching create) or swept (expired) so the slot doesn't linger.
    if let Some(auth) = POOL_CREATION_AUTH.may_load(deps.storage, (&key0, &key1, fee))? {
        if env.block.time.seconds() <= auth.expires_at && info.sender != auth.creator {
            return Err(StdError::generic_err(
                "pool slot reserved for another creator",
            ));
        }
        POOL_CREATION_AUTH.remove(deps.storage, (&key0, &key1, fee));
    }

    // 3. Check existence
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
        // Clamp below 1_000_000: the pool's instantiate validation rejects
        // `max_fee_ppm >= 1_000_000`, so an un-clamped `fee * 2` would make
        // `create_pool` revert (with a confusing nested error) for any fee tier
        // >= 500_000. `min(999_999)` keeps `base <= max` (they meet at the cap)
        // and guarantees the pool accepts the config for every valid tier.
        max_fee_ppm: fee.saturating_mul(2).min(999_999),
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
        .add_attribute("fee", fee.to_string())
        .add_attribute("tick_spacing", tick_spacing.to_string())
        .add_attribute("init_sqrt_price", init_sqrt_price.to_string()))
}

/// Sort two assets into `(token0, token1)` using `AssetInfo`'s ordering — the
/// single source of truth shared by `CreatePool`, `AuthorizeCreation`, and
/// `CancelCreationAuth` so all three derive byte-identical storage keys.
fn sort_tokens(token_a: AssetInfo, token_b: AssetInfo) -> (AssetInfo, AssetInfo) {
    if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

/// True iff `sender` owns the tokenfactory namespace of `asset`, i.e. `asset`
/// is a native `factory/{owner}/{sub}` denom with `owner == sender`
/// (canonicalized). Pure string parse — no tokenfactory query, so it works
/// before the denom is even minted. Non-`factory/` denoms and CW20s return
/// false.
fn sender_owns_namespace(api: &dyn Api, sender: &Addr, asset: &AssetInfo) -> bool {
    let denom = match asset {
        AssetInfo::NativeToken { denom } => denom,
        AssetInfo::Token { .. } => return false,
    };
    // Expect "factory/{owner}/{subdenom}".
    let mut parts = denom.split('/');
    if parts.next() != Some("factory") {
        return false;
    }
    let owner = match parts.next() {
        Some(o) if !o.is_empty() => o,
        _ => return false,
    };
    // Require a non-empty subdenom segment so a bare "factory/{owner}" can't
    // be passed off as a namespace claim.
    if !matches!(parts.next(), Some(s) if !s.is_empty()) {
        return false;
    }
    match (api.addr_canonicalize(sender.as_str()), api.addr_canonicalize(owner)) {
        (Ok(s), Ok(o)) => s == o,
        _ => false,
    }
}

/// Shared authorizer check for `AuthorizeCreation` / `CancelCreationAuth`:
/// `sender` must own the namespace of one of the (already validated) sides, or
/// be the factory owner.
fn ensure_can_authorize(
    api: &dyn Api,
    config: &Config,
    sender: &Addr,
    token0: &AssetInfo,
    token1: &AssetInfo,
) -> Result<(), StdError> {
    if *sender == config.owner
        || sender_owns_namespace(api, sender, token0)
        || sender_owns_namespace(api, sender, token1)
    {
        Ok(())
    } else {
        Err(StdError::generic_err(
            "Unauthorized: must own a token's factory namespace or be the factory owner",
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_authorize_creation(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,
    creator: String,
    ttl_seconds: u64,
) -> Result<Response, StdError> {
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }
    if let AssetInfo::Token { contract_addr } = &token_a {
        deps.api.addr_validate(contract_addr)?;
    }
    if let AssetInfo::Token { contract_addr } = &token_b {
        deps.api.addr_validate(contract_addr)?;
    }
    // Reserving a fee tier that can never host a pool is meaningless — fail
    // early rather than store a dead entry.
    if !FEE_TIERS.has(deps.storage, fee) {
        return Err(StdError::generic_err("Fee tier not supported"));
    }
    let creator = deps.api.addr_validate(&creator)?;

    let (token0, token1) = sort_tokens(token_a, token_b);
    let config = CONFIG.load(deps.storage)?;
    ensure_can_authorize(deps.api, &config, &info.sender, &token0, &token1)?;

    let key0 = token0.registry_key();
    let key1 = token1.registry_key();

    // `ttl_seconds == 0` ⇒ no expiry (the graduation default: the launch denom
    // is unique, so an indefinite reservation harms nobody and can never lapse
    // before Settle).
    let expires_at = if ttl_seconds == 0 {
        u64::MAX
    } else {
        env.block.time.seconds().saturating_add(ttl_seconds)
    };

    POOL_CREATION_AUTH.save(
        deps.storage,
        (&key0, &key1, fee),
        &PoolCreationAuth {
            creator: creator.clone(),
            expires_at,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "authorize_creation")
        .add_attribute("token0", token0.to_string())
        .add_attribute("token1", token1.to_string())
        .add_attribute("fee", fee.to_string())
        .add_attribute("creator", creator)
        .add_attribute("expires_at", expires_at.to_string()))
}

fn execute_cancel_creation_auth(
    deps: DepsMut,
    info: MessageInfo,
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,
) -> Result<Response, StdError> {
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }
    let (token0, token1) = sort_tokens(token_a, token_b);
    let config = CONFIG.load(deps.storage)?;
    ensure_can_authorize(deps.api, &config, &info.sender, &token0, &token1)?;

    let key0 = token0.registry_key();
    let key1 = token1.registry_key();
    if !POOL_CREATION_AUTH.has(deps.storage, (&key0, &key1, fee)) {
        return Err(StdError::generic_err("No reservation for this slot"));
    }
    POOL_CREATION_AUTH.remove(deps.storage, (&key0, &key1, fee));

    Ok(Response::new()
        .add_attribute("action", "cancel_creation_auth")
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

            let key0 = token0.registry_key();
            let key1 = token1.registry_key();
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
        QueryMsg::GetCreationAuth {
            token_a,
            token_b,
            fee,
        } => {
            let (token0, token1) = sort_tokens(token_a, token_b);
            let key0 = token0.registry_key();
            let key1 = token1.registry_key();
            let auth = POOL_CREATION_AUTH
                .may_load(deps.storage, (&key0, &key1, fee))?
                .map(|a| CreationAuthResponse {
                    creator: a.creator.to_string(),
                    expires_at: a.expires_at,
                });
            to_json_binary(&auth)
        }
    }
}

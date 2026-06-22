#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    from_json, to_json_binary, Addr, Api, Binary, Deps, DepsMut, Env, MessageInfo, Order, Reply,
    Response, StdError, StdResult, SubMsg, Uint256, WasmMsg,
};
use cw_storage_plus::Bound;
use sha2::{Digest, Sha256};

use crate::state::{
    Config, PoolCreationAuth, CONFIG, FEE_TIERS, FLASH_BORROWERS, FLASH_UNRESTRICTED, POOLS,
    POOL_CREATION_AUTH,
};

use choice_clmm_common::factory::{
    ConfigResponse, CreationAuthResponse, ExecuteMsg, FeeTierEntry, FlashBorrowersResponse,
    InstantiateMsg, IsFlashBorrowerResponse, MigrateMsg, PoolInfo, QueryMsg,
};
use choice_clmm_common::pool::{FeeConfig, InstantiateMsg as PoolInstantiateMsg};
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

const CONTRACT_NAME: &str = "crates.io:choice-clmm-factory";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Upper bound on a fee tier's `tick_spacing` (matches Uniswap v3's maximum).
/// Keeps the pool's i32 tick-bitmap / swap arithmetic from overflowing and the
/// `tick_spacing as i32` cast from wrapping negative. See `EnableFeeAmount`.
const MAX_TICK_SPACING: u32 = 16384;

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
            max_fee_multiple,
        } => execute_create_pool(
            deps,
            env,
            info,
            token_a,
            token_b,
            fee,
            init_sqrt_price,
            max_fee_multiple,
        ),
        ExecuteMsg::AuthorizeCreation {
            token_a,
            token_b,
            fee,
            creator,
            ttl_seconds,
        } => {
            execute_authorize_creation(deps, env, info, token_a, token_b, fee, creator, ttl_seconds)
        }
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
            // Bound tick_spacing on BOTH ends. The upper bound (16384, matching
            // Uniswap v3's maximum) keeps every `tick * tick_spacing` and
            // `word_pos * 256 * tick_spacing` computation in the pool's tick
            // bitmap / swap loop comfortably inside i32, and crucially prevents
            // the `config.tick_spacing as i32` cast from wrapping negative for a
            // u32 above i32::MAX — either of which would corrupt tick traversal
            // or brick swaps for the whole fee tier. Admin-gated, so this is a
            // governance footgun guard rather than an attacker-reachable path.
            if tick_spacing == 0 || tick_spacing > MAX_TICK_SPACING {
                return Err(StdError::generic_err(format!(
                    "Tick spacing must be in 1..={}",
                    MAX_TICK_SPACING
                )));
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
        ExecuteMsg::AuthorizeFlashBorrower { borrower } => {
            execute_authorize_flash_borrower(deps, info, borrower)
        }
        ExecuteMsg::RevokeFlashBorrower { borrower } => {
            execute_revoke_flash_borrower(deps, info, borrower)
        }
        ExecuteMsg::SetFlashUnrestricted { open } => {
            execute_set_flash_unrestricted(deps, info, open)
        }
    }
}

/// Owner-only: add `borrower` to the flash-loan allowlist that every pool created
/// by this factory live-queries during `Flash {}`.
fn execute_authorize_flash_borrower(
    deps: DepsMut,
    info: MessageInfo,
    borrower: String,
) -> Result<Response, StdError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    let borrower_addr = deps.api.addr_validate(&borrower)?;
    FLASH_BORROWERS.save(deps.storage, &borrower_addr, &())?;

    Ok(Response::new()
        .add_attribute("action", "authorize_flash_borrower")
        .add_attribute("borrower", borrower_addr.to_string()))
}

/// Owner-only: remove `borrower` from the flash-loan allowlist.
fn execute_revoke_flash_borrower(
    deps: DepsMut,
    info: MessageInfo,
    borrower: String,
) -> Result<Response, StdError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    let borrower_addr = deps.api.addr_validate(&borrower)?;
    FLASH_BORROWERS.remove(deps.storage, &borrower_addr);

    Ok(Response::new()
        .add_attribute("action", "revoke_flash_borrower")
        .add_attribute("borrower", borrower_addr.to_string()))
}

/// Owner-only: toggle the pool flash-borrower gate. When `open` is true, every
/// pool's `Flash {}` is permissionless.
fn execute_set_flash_unrestricted(
    deps: DepsMut,
    info: MessageInfo,
    open: bool,
) -> Result<Response, StdError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }
    FLASH_UNRESTRICTED.save(deps.storage, &open)?;

    Ok(Response::new()
        .add_attribute("action", "set_flash_unrestricted")
        .add_attribute("open", open.to_string()))
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

#[allow(clippy::too_many_arguments)]
fn execute_create_pool(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,
    init_sqrt_price: Uint256,
    max_fee_multiple: Option<u32>,
) -> Result<Response, StdError> {
    // 1. Sort Tokens
    if token_a == token_b {
        return Err(StdError::generic_err("Same tokens"));
    }

    // Dynamic-fee ceiling = `fee * multiple`. Default 2x; launchpad
    // graduations pass up to 10x so the volatility fee can price a
    // post-graduation frenzy. Lower bound 2 keeps the dynamic mechanism
    // meaningful (1x would pin the fee at base); upper bound 10 bounds the
    // worst-case trader fee at 10x the advertised tier.
    let max_fee_multiple = max_fee_multiple.unwrap_or(2);
    if !(2..=10).contains(&max_fee_multiple) {
        return Err(StdError::generic_err("max_fee_multiple must be in 2..=10"));
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
    // price-manipulation attacks (see audit MED-14). 100 ppm/sec ≈ 65-90 ppm
    // per ~0.65-0.9s Injective block, so ramping base -> 2x-base cap takes
    // `base/100` seconds of sustained volatility (30s for the 0.30% tier) —
    // short enough to react to real volatility, long enough to make
    // single-block griefing unprofitable.
    let fee_config = FeeConfig {
        base_fee_ppm: fee,
        // Clamp below 1_000_000: the pool's instantiate validation rejects
        // `max_fee_ppm >= 1_000_000`, so an un-clamped `fee * multiple` would
        // make `create_pool` revert (with a confusing nested error) for large
        // fee tiers. `min(999_999)` keeps `base <= max` (they meet at the cap)
        // and guarantees the pool accepts the config for every valid tier.
        max_fee_ppm: fee.saturating_mul(max_fee_multiple).min(999_999),
        // v2 dynamic fee: a decaying accumulator of realized tick-movement fed
        // through a convex (squared) fee — `variable_ppm = control * v_a^2 / 1e6`,
        // where `v_a` (in ticks) decays linearly to 0 over `volatility_decay_seconds`.
        // The signal is raw ticks (1 tick ≈ 1 bp of price), so these constants are
        // tier-independent (unlike v1's sqrt-space multiplier).
        //
        // - `variable_fee_control = 8800`: a single ~6% move (~583 ticks) adds
        //   ~2990 ppm, doubling the 0.30% tier to its 2x cap; smaller moves add
        //   convexly less, sustained/larger moves saturate the cap. Validated on
        //   real NONJA candle data (plan §3).
        // - `max_volatility_accumulator = 2000` ticks (~22% move): bounds v_a^2
        //   well inside the u128 intermediate, far past where the fee saturates.
        // - `volatility_decay_seconds = 600`: a 10-minute full-forget window — an
        //   elevated fee persists across a move then relaxes to base as trading
        //   calms, regardless of the new price level (the v1 gap-then-calm fix).
        //
        // Must stay <= the pool's 1_000_000 instantiate bound (control) and
        // <= 2*MAX_TICK (accumulator); decay in 60..=86400.
        variable_fee_control: 8_800,
        max_volatility_accumulator: 2_000,
        volatility_decay_seconds: 600,
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
    // The instantiate label is capped at 128 chars by wasmd. Two long
    // tokenfactory denoms (e.g. a `factory/<issuer>/<sub>` launch token paired
    // with `factory/<admin>/SAI`) blow past that, so truncate defensively —
    // the label is cosmetic and the canonical pair identity lives in state.
    let mut label = format!("Choice CLMM Pool {}/{}", token0, token1);
    if label.len() > 128 {
        label.truncate(128);
    }
    let wasm_msg = WasmMsg::Instantiate2 {
        admin: None,
        code_id: config.pool_code_id,
        msg: to_json_binary(&pool_instantiate_msg)?,
        funds: vec![],
        label,
        salt,
    };

    // Carry the registry key in the SubMsg payload rather than a global
    // `Item`. The payload is bound to THIS submessage and delivered verbatim to
    // `reply`, so it cannot be cross-contaminated by a concurrent/abandoned
    // create the way a shared `Item` could if the reply mode ever changed away
    // from `reply_on_success`. Self-documenting and race-proof by construction.
    let sub_msg = SubMsg::reply_on_success(wasm_msg, 1).with_payload(to_json_binary(&(
        key0.clone(),
        key1.clone(),
        fee,
    ))?);

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
    match (
        api.addr_canonicalize(sender.as_str()),
        api.addr_canonicalize(owner),
    ) {
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

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id == 1 {
        // The registry key travels in the submessage payload (see CreatePool),
        // not in shared storage, so it is impossible to associate this reply
        // with the wrong pending pool.
        let (token0, token1, fee): (String, String, u32) = from_json(&msg.payload)?;

        let res = msg.result.into_result().map_err(StdError::generic_err)?;

        let address_str = res
            .events
            .iter()
            .find(|e| e.ty == "instantiate")
            .and_then(|e| e.attributes.iter().find(|a| a.key == "_contract_address"))
            .map(|a| &a.value)
            .ok_or_else(|| StdError::generic_err("No contract address found in reply"))?;

        let pool_address = deps.api.addr_validate(address_str)?;

        POOLS.save(deps.storage, (&token0, &token1, fee), &pool_address)?;

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
        QueryMsg::IsFlashBorrower { borrower } => {
            let borrower_addr = deps.api.addr_validate(&borrower)?;
            let unrestricted = FLASH_UNRESTRICTED.may_load(deps.storage)?.unwrap_or(false);
            let authorized = unrestricted || FLASH_BORROWERS.has(deps.storage, &borrower_addr);
            to_json_binary(&IsFlashBorrowerResponse { authorized })
        }
        QueryMsg::FlashBorrowers { start_after, limit } => {
            let limit = limit.unwrap_or(30).min(100) as usize;
            let start = start_after
                .map(|addr| deps.api.addr_validate(&addr))
                .transpose()?;
            let borrowers: Vec<String> = FLASH_BORROWERS
                .range(
                    deps.storage,
                    start.as_ref().map(Bound::exclusive),
                    None,
                    Order::Ascending,
                )
                .take(limit)
                .map(|item| {
                    let (borrower_addr, _) = item?;
                    Ok(borrower_addr.to_string())
                })
                .collect::<StdResult<_>>()?;
            let unrestricted = FLASH_UNRESTRICTED.may_load(deps.storage)?.unwrap_or(false);
            to_json_binary(&FlashBorrowersResponse {
                borrowers,
                unrestricted,
            })
        }
    }
}

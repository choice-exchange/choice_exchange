use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::Uint256;

use crate::types::AssetInfo;

#[cw_serde]
pub struct InstantiateMsg {
    pub pool_code_id: u64,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Creates a new pool using Instantiate2 (Deterministic address)
    CreatePool {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
        init_sqrt_price: Uint256,
    },
    /// Updates the owner of the factory
    UpdateConfig {
        owner: Option<String>,
        pool_code_id: Option<u64>,
    },
    /// Enable a new fee tier (e.g. 100 pips, spacing 1)
    EnableFeeAmount { fee: u32, tick_spacing: u32 },
    /// Reserve the canonical pool slot `(token0, token1, fee)` for `creator`.
    /// Until consumed (or expired), only `creator` may `CreatePool` for that
    /// slot; every other slot stays permissionless. Authorizer must own the
    /// tokenfactory namespace of one side (`factory/{sender}/…`) or be the
    /// factory owner. `ttl_seconds == 0` means no expiry. Anti-squat gate for
    /// the graduation on-ramp; see `docs/graduation_antisquat_plan.md`.
    AuthorizeCreation {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
        creator: String,
        ttl_seconds: u64,
    },
    /// Release a reservation early. Same authorizer rule as `AuthorizeCreation`.
    CancelCreationAuth {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
}

#[cw_serde]
pub struct PoolInfo {
    pub pool_address: String,
    pub token0: String,
    pub token1: String,
    pub fee: u32,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(String)]
    GetPool {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
    #[returns(Vec<PoolInfo>)]
    GetAllPools {
        start_after: Option<(String, String, u32)>,
        limit: Option<u32>,
    },
    #[returns(ConfigResponse)]
    GetConfig {},
    #[returns(Vec<FeeTierEntry>)]
    GetFeeTiers {
        start_after: Option<u32>,
        limit: Option<u32>,
    },
    /// Returns the active creation reservation for a slot, if any.
    #[returns(Option<CreationAuthResponse>)]
    GetCreationAuth {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
}

#[cw_serde]
pub struct ConfigResponse {
    pub owner: String,
    pub pool_code_id: u64,
}

#[cw_serde]
pub struct FeeTierEntry {
    pub fee: u32,
    pub tick_spacing: u32,
}

#[cw_serde]
pub struct CreationAuthResponse {
    /// The only address allowed to `CreatePool` for this slot until expiry.
    pub creator: String,
    /// Unix seconds; `u64::MAX` means no expiry.
    pub expires_at: u64,
}

#[cw_serde]
pub struct MigrateMsg {}

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
pub struct MigrateMsg {}

use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, Uint256};

#[cw_serde]
pub struct InstantiateMsg {
    pub pool_code_id: u64,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Creates a new pool using Instantiate2 (Deterministic address)
    CreatePool {
        token_a: String,
        token_b: String,
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
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(Addr)]
    GetPool {
        token_a: String,
        token_b: String,
        fee: u32,
    },
    // ... other queries like GetFeeTier
}

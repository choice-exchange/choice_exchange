use cosmwasm_schema::cw_serde;
use cosmwasm_std::Addr;
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub struct FeeTier {
    pub tick_spacing: u32,
    // Default fee rate in pips (e.g., 500 = 0.05%)
    pub default_fee: u32,
}

// Config for the factory itself
#[cw_serde]
pub struct Config {
    pub owner: Addr,
    pub pool_code_id: u64, // The uploaded Code ID of the Choice CLMM Pool
}

pub const CONFIG: Item<Config> = Item::new("config");

// Allowed fee tiers (e.g. 100->1, 500->10, 3000->60)
// Key: Fee (pips) -> Value: Tick Spacing
pub const FEE_TIERS: Map<u32, u32> = Map::new("fee_tiers");

// Registry of created pools
// Key: (Token0, Token1, FeeTier) -> Pool Address
pub const POOLS: Map<(&str, &str, u32), Addr> = Map::new("pools");

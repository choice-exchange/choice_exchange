use choice_clmm_common::pool::{FeeConfig, Slot0, TickInfo};
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Uint128, Uint256};
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub struct PoolConfig {
    pub factory: Addr,
    pub token0: String,
    pub token1: String,
    pub tick_spacing: u32,
    pub fee_config: FeeConfig,
}

#[cw_serde]
#[derive(Default)]
pub struct PositionInfo {
    pub liquidity: u128,
    pub fee_growth_inside_0_last: Uint256,
    pub fee_growth_inside_1_last: Uint256,
    pub tokens_owed_0: Uint128,
    pub tokens_owed_1: Uint128,
}

#[cw_serde]
pub struct OracleData {
    pub price_ema_x96: Uint256, // Exponential Moving Average of SqrtPrice
    pub last_block_time: u64,   // Seconds
}

// Define Storage Keys
pub const CONFIG: Item<PoolConfig> = Item::new("config");
pub const SLOT0: Item<Slot0> = Item::new("slot0");

// Maps Tick Index (i32) -> Info
pub const TICKS: Map<i32, TickInfo> = Map::new("ticks");

// Maps (Owner, LowerTick, UpperTick) -> Info
pub const POSITIONS: Map<(&str, i32, i32), PositionInfo> = Map::new("positions");

pub const TICK_BITMAP: Map<i16, Uint256> = Map::new("tick_bitmap");

pub const FEE_GROWTH_GLOBAL_0: Item<Uint256> = Item::new("fg0");
pub const FEE_GROWTH_GLOBAL_1: Item<Uint256> = Item::new("fg1");

pub const ORACLE: Item<OracleData> = Item::new("oracle");

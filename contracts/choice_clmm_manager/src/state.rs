use choice_clmm_common::manager::Position;
use cosmwasm_schema::cw_serde;
use cosmwasm_std::Addr;
use cw_storage_plus::{Item, Map};

// Global config
#[cw_serde]
pub struct Config {
    pub factory: Addr,
}

pub const CONFIG: Item<Config> = Item::new("config");

// Auto-incrementing ID for positions
pub const TOKEN_ID_COUNTER: Item<u64> = Item::new("token_id_counter");

pub const POSITIONS: Map<&str, Position> = Map::new("positions");

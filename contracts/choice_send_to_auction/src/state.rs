use cosmwasm_std::{Addr, CanonicalAddr, Deps, DepsMut, StdResult};

use cw_storage_plus::Item;
use serde::{Deserialize, Serialize};

pub const CONFIG: Item<Config> = Item::new("config");

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Config {
    pub owner: CanonicalAddr,
    pub adapter_contract: String,
    pub burn_auction_subaccount: String,

    pub proposed_owner: Option<Addr>,
}

pub fn load_config(deps: Deps) -> StdResult<Config> {
    CONFIG.load(deps.storage)
}

pub fn save_config(deps: DepsMut, config: &Config) -> StdResult<()> {
    CONFIG.save(deps.storage, config)
}

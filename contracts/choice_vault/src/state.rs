use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use choice::asset::AssetInfo; // Using the shared library
use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct Config {
    /// The address that can trigger the compound function (keeper)
    pub owner: Addr,
    /// The address of the AMM pair contract.
    pub pair_contract: Addr,
    /// The address of the farm/staking contract.
    pub farm_contract: Addr,
    /// The contract address of the LP token.
    pub lp_token: Addr,
    /// The address of the farm's reward token.
    pub reward_token: AssetInfo,

    pub asset_infos: [AssetInfo; 2],
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct UserInfo {
    /// The number of shares the user owns in the vault.
    pub shares: Uint128,
}

/// The contract's configuration.
pub const CONFIG: Item<Config> = Item::new("config");

/// The total number of shares issued to all users.
pub const TOTAL_SHARES: Item<Uint128> = Item::new("total_shares");

/// Maps a user's address to their share information.
pub const USERS: Map<&Addr, UserInfo> = Map::new("users");

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use choice::asset::AssetInfo; // Using the shared library
use cosmwasm_std::{Addr, Decimal, Uint128};
use cw_storage_plus::{Item, Map};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct Config {
    /// The contract's owner, who can change configuration.
    pub owner: Addr,
    /// The trusted address allowed to call the Compound function.
    pub compounder: Addr,
    /// The slippage tolerance to use for swaps during compounding.
    pub slippage_tolerance: Decimal,
    /// The address of the AMM pair contract.
    pub pair_contract: Addr,
    /// The address of the farm/staking contract.
    pub farm_contract: Addr,
    /// The contract address of the LP token.
    pub lp_token: Addr,
    /// The address of the farm's reward token.
    pub reward_token: AssetInfo,
    pub asset_infos: [AssetInfo; 2],
    /// The address that will receive the compounding fees.
    pub fee_recipient: Option<Addr>,
    /// The percentage of rewards to take as a fee.
    pub fee_percentage: Option<Decimal>,
    pub minimum_reward_to_compound: Uint128,
    /// A new owner address that has been proposed but not yet accepted.
    pub proposed_owner: Option<Addr>,
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

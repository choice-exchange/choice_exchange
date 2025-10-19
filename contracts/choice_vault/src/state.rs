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
    pub lp_token: AssetInfo,
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

    /// A list of swap operations to perform to convert the reward token
    /// into one of the two underlying assets of the `pair_contract`.
    /// If this route is empty, the contract assumes the reward token
    /// is already one of the LP's assets.
    pub reward_to_lp_token_route: Vec<SwapHop>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SwapHop {
    /// The address of the pair contract for this specific hop.
    pub pair_contract: Addr,
    /// The asset we expect to receive from this swap.
    pub to_asset_info: AssetInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct UserInfo {
    /// The number of shares the user owns in the vault.
    pub shares: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema, Default)]
pub struct CompoundingInfo {
    /// The block time (in seconds) of the last successful compound.
    pub last_compound_time: u64,
    /// The amount of the `reward_token` harvested and compounded in the last run. (Profit)
    pub last_reward_amount_compounded: Uint128,
    /// The total LP tokens staked by the vault *before* the last compound. (Principal)
    pub total_lp_staked_at_last_compound: Uint128,
}

// Add this new Item to your list of constants
pub const COMPOUNDING_INFO: Item<CompoundingInfo> = Item::new("compounding_info");

/// The contract's configuration.
pub const CONFIG: Item<Config> = Item::new("config");

/// The total number of shares issued to all users.
pub const TOTAL_SHARES: Item<Uint128> = Item::new("total_shares");

/// Maps a user's address to their share information.
pub const USERS: Map<&Addr, UserInfo> = Map::new("users");

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{CanonicalAddr, Decimal, StdResult, Storage, Uint128};
use cw_storage_plus::{Item, Map};

use choice::asset::AssetInfo;

/// Store the configuration under the key "config"
pub const CONFIG: Item<Config> = Item::new("config");

/// Store the state under the key "state"
pub const STATE: Item<State> = Item::new("state");

/// Store staker info using the prefix "reward"
pub const STAKER_INFO: Map<&[u8], StakerInfo> = Map::new("reward");

/// Pending migrate_staking proposal. Absent when no migration is queued.
/// H-4 follow-through: migration is now two-step — `ProposeMigrateStaking`
/// sets this item, `ApplyMigrateStaking` clears it and forwards funds once
/// the timelock has elapsed.
pub const PENDING_MIGRATION: Item<PendingMigration> = Item::new("pending_migration");

/// Delay between proposing and applying either an owner rotation or a
/// `migrate_staking`. 48 hours gives stakers a window to unbond + withdraw
/// if they distrust the proposal.
pub const TIMELOCK_DELAY_SECONDS: u64 = 48 * 60 * 60;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub owner: CanonicalAddr,
    pub reward_token: AssetInfo,
    pub staking_token: AssetInfo,
    pub distribution_schedule: Vec<(u64, u64, Uint128)>,
    /// Owner rotation that has been proposed but cannot take effect until the
    /// timelock expires. `#[serde(default)]` so pre-hardening stored configs
    /// read as "no pending rotation" without a migration.
    #[serde(default)]
    pub pending_owner: Option<CanonicalAddr>,
    /// Unix timestamp (seconds) at which `pending_owner` may be applied.
    #[serde(default)]
    pub pending_owner_effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingMigration {
    pub new_staking_contract: CanonicalAddr,
    /// Unix timestamp (seconds) at which the migration may be applied.
    pub effective_at: u64,
}

/// Save the configuration into storage.
pub fn store_config(storage: &mut dyn Storage, config: &Config) -> StdResult<()> {
    CONFIG.save(storage, config)
}

/// Load the configuration from storage.
pub fn read_config(storage: &dyn Storage) -> StdResult<Config> {
    CONFIG.load(storage)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct State {
    pub last_distributed: u64,
    pub total_bond_amount: Uint128,
    pub global_reward_index: Decimal,
    /// Reward tokens held by the contract that have not yet been credited to
    /// any staker. Incremented by `Fund` messages and decremented by
    /// `compute_reward` when it credits rewards to the global index. The
    /// schedule defines a cap on the rate; this field defines a cap on the
    /// total. Distribution is `min(schedule, undistributed_rewards)`.
    pub undistributed_rewards: Uint128,
}

/// Save the state into storage.
pub fn store_state(storage: &mut dyn Storage, state: &State) -> StdResult<()> {
    STATE.save(storage, state)
}

/// Load the state from storage.
pub fn read_state(storage: &dyn Storage) -> StdResult<State> {
    STATE.load(storage)
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct StakerInfo {
    pub reward_index: Decimal,
    pub bond_amount: Uint128,
    pub pending_reward: Uint128,
}

/// Save the staker info for a given owner.
///
/// The key used in storage is derived from the owner's canonical address.
pub fn store_staker_info(
    storage: &mut dyn Storage,
    owner: &CanonicalAddr,
    staker_info: &StakerInfo,
) -> StdResult<()> {
    STAKER_INFO.save(storage, owner.as_slice(), staker_info)
}

/// Remove the staker info for a given owner.
pub fn remove_staker_info(storage: &mut dyn Storage, owner: &CanonicalAddr) {
    STAKER_INFO.remove(storage, owner.as_slice())
}

/// Read the staker info for a given owner.
///
/// If no info is found, returns a default `StakerInfo` with all values set to zero.
pub fn read_staker_info(storage: &dyn Storage, owner: &CanonicalAddr) -> StdResult<StakerInfo> {
    match STAKER_INFO.may_load(storage, owner.as_slice())? {
        Some(info) => Ok(info),
        None => Ok(StakerInfo {
            reward_index: Decimal::zero(),
            bond_amount: Uint128::zero(),
            pending_reward: Uint128::zero(),
        }),
    }
}

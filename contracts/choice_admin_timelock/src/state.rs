use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{Binary, CanonicalAddr};
use cw_storage_plus::Item;

/// Minimum allowed timelock delay. A short delay defeats the contract's
/// purpose; 1 hour is a permissive floor that still gives users time to
/// react. Tests may instantiate with values close to this floor.
pub const MIN_TIMELOCK_SECONDS: u64 = 60 * 60;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    /// Owner that can propose migrations and owner rotations. Should itself
    /// be a multisig.
    pub owner: CanonicalAddr,
    /// Delay in seconds between propose and apply for both migrations and
    /// owner rotation. Set at instantiate and immutable thereafter (changing
    /// it would defeat the contract's purpose; ship a new contract with a
    /// different value if needed).
    pub timelock_seconds: u64,
    /// Owner rotation queued. The new owner takes effect after
    /// `pending_owner_effective_at` has elapsed.
    pub pending_owner: Option<CanonicalAddr>,
    pub pending_owner_effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingMigration {
    /// Bech32 address of the contract to migrate.
    pub contract: String,
    /// New code id to migrate to.
    pub code_id: u64,
    /// `MigrateMsg` to pass to the target contract's migrate handler.
    pub msg: Binary,
    /// Unix timestamp (seconds) at which the migration may be applied.
    pub effective_at: u64,
}

pub const CONFIG: Item<Config> = Item::new("config");
pub const PENDING_MIGRATION: Item<PendingMigration> = Item::new("pending_migration");

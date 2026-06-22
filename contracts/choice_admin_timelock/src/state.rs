use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{Binary, CanonicalAddr, Coin};
use cw_storage_plus::Item;

/// Minimum allowed timelock delay. A short delay defeats the contract's
/// purpose; 1 hour is a permissive floor that still gives users time to
/// react. Tests may instantiate with values close to this floor.
pub const MIN_TIMELOCK_SECONDS: u64 = 60 * 60;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    /// Owner that can propose actions and owner rotations. Should itself
    /// be a multisig.
    pub owner: CanonicalAddr,
    /// Delay in seconds between propose and apply for both actions and
    /// owner rotation. Set at instantiate and immutable thereafter (changing
    /// it would defeat the contract's purpose; ship a new contract with a
    /// different value if needed).
    pub timelock_seconds: u64,
    /// Owner rotation queued. The new owner takes effect after
    /// `pending_owner_effective_at` has elapsed.
    pub pending_owner: Option<CanonicalAddr>,
    pub pending_owner_effective_at: Option<u64>,
}

/// What a `Propose` queues for the multisig to apply after the timelock
/// elapses. `Migrate` reissues a `WasmMsg::Migrate`; `Execute` reissues a
/// `WasmMsg::Execute`, letting the timelock invoke any execute message on
/// any contract it has authority over (e.g. rotating a farm's
/// `Config.owner` to its creator).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProposedAction {
    Migrate {
        contract: String,
        code_id: u64,
        msg: Binary,
    },
    Execute {
        contract: String,
        msg: Binary,
        /// Native coins to forward with the execute. The timelock must hold
        /// these in its balance at apply time. Empty for the common case.
        funds: Vec<Coin>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingAction {
    pub action: ProposedAction,
    /// Unix timestamp (seconds) at which the action may be applied.
    pub effective_at: u64,
}

pub const CONFIG: Item<Config> = Item::new("config");
pub const PENDING_ACTION: Item<PendingAction> = Item::new("pending_action");

/// Storage key used by v1.1.x for the migration-only pending slot. The
/// v1.2 migrate handler clears it so a stale value never blocks the new
/// `pending_action` slot from being read cleanly.
pub(crate) const LEGACY_PENDING_MIGRATION_KEY: &str = "pending_migration";

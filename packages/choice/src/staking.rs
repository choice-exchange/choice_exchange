use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::asset::AssetInfo;
use cosmwasm_std::{Decimal, Uint128};
use cw20::Cw20ReceiveMsg;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct InstantiateMsg {
    /// Address that holds owner privileges (`UpdateConfig`,
    /// `ProposeMigrateStaking`, `ProposeNewOwner`). For factory-spawned farms
    /// this is the Choice governance multisig — NOT the user who paid the
    /// launch fee, who is treated as a one-shot operator with no on-chain role.
    pub owner: String,
    pub reward_token: AssetInfo,
    pub staking_token: AssetInfo,
    pub distribution_schedule: Vec<(u64, u64, Uint128)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    Receive(Cw20ReceiveMsg),
    Bond {
        amount: Uint128,
    },
    Unbond {
        amount: Uint128,
    },
    /// Withdraw pending rewards
    Withdraw {},
    /// Deposit reward tokens into the distribution pool (native reward token).
    /// The caller sends `info.funds` matching the configured reward denom.
    /// Anyone can call this; the contract distributes only what has been funded.
    Fund {},
    /// H-4 follow-through: owner proposes a migration target. The actual
    /// `undistributed_rewards` forward only happens when the owner later calls
    /// `ApplyMigrateStaking` after `TIMELOCK_DELAY_SECONDS` have elapsed. The
    /// delay is the user exit window — stakers who distrust the incoming
    /// contract can `Unbond` + `Withdraw` before the move.
    ProposeMigrateStaking {
        new_staking_contract: String,
    },
    /// Owner-only. Finalizes a pending migration once the timelock has expired.
    ApplyMigrateStaking {},
    /// Owner-only. Clears any pending migration.
    CancelMigrateStakingProposal {},
    /// H-2: owner proposes a new `distribution_schedule`. Like
    /// `ProposeMigrateStaking`, the change cannot take effect until
    /// `TIMELOCK_DELAY_SECONDS` have elapsed so stakers have an exit window if
    /// they distrust the new schedule. Re-proposing overwrites the pending
    /// proposal and resets the timer.
    ProposeUpdateConfig {
        distribution_schedule: Vec<(u64, u64, Uint128)>,
    },
    /// Owner-only. Finalizes a pending schedule update once the timelock has expired.
    ApplyUpdateConfig {},
    /// Owner-only. Clears a pending schedule update proposal.
    CancelUpdateConfigProposal {},
    /// H-4 follow-through: owner proposes a new owner. The rotation cannot
    /// take effect until `TIMELOCK_DELAY_SECONDS` have elapsed, giving users
    /// a window to exit if they distrust the incoming operator. Proposing
    /// again resets the timer.
    ProposeNewOwner {
        new_owner: String,
    },
    /// Owner-only. Finalizes a pending owner rotation once the timelock has expired.
    ApplyOwnerRotation {},
    /// Owner-only. Clears any pending owner proposal.
    CancelOwnerProposal {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cw20HookMsg {
    Bond {},
    /// Deposit reward tokens (CW20) into the distribution pool. The CW20
    /// sending the tokens must be the configured reward token.
    Fund {},
}

/// migrate struct for distribution schedule
/// block-based schedule to a time-based schedule
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct MigrateMsg {}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Config {},
    State {
        block_time: Option<u64>,
    },
    StakerInfo {
        staker: String,
        block_time: Option<u64>,
    },
    /// Pending owner rotation, if any.
    PendingOwnerRotation {},
    /// Pending migrate_staking proposal, if any.
    PendingMigration {},
    /// Pending schedule update, if any.
    PendingConfigUpdate {},
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct ConfigResponse {
    pub owner: String,
    pub reward_token: String,
    pub staking_token: String,
    pub distribution_schedule: Vec<(u64, u64, Uint128)>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingOwnerRotationResponse {
    pub pending_owner: Option<String>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingMigrationResponse {
    pub new_staking_contract: Option<String>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingConfigUpdateResponse {
    pub distribution_schedule: Option<Vec<(u64, u64, Uint128)>>,
    pub effective_at: Option<u64>,
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct StateResponse {
    pub last_distributed: u64,
    pub total_bond_amount: Uint128,
    pub global_reward_index: Decimal,
    pub undistributed_rewards: Uint128,
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct StakerInfoResponse {
    pub staker: String,
    pub reward_index: Decimal,
    pub bond_amount: Uint128,
    pub pending_reward: Uint128,
}

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::Uint128;

use crate::asset::AssetInfo;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct InstantiateMsg {
    /// Owner of the factory. Receives admin powers (UpdateConfig, owner
    /// rotation). Should be a timelocked multisig in production.
    pub owner: String,
    /// Address that receives every `CreateFarm` INJ fee.
    pub fee_collector: String,
    /// Anti-spam launch fee, denominated in `inj` base units (1 INJ = 1e18).
    pub instantiate_fee_inj: Uint128,
    /// WASM code id of the `choice-farm` contract that this factory should
    /// spawn on `CreateFarm`.
    pub farm_code_id: u64,
    /// Address installed as the `Config.owner` AND wasm admin of every farm
    /// spawned by this factory. Should be the Choice governance multisig so
    /// users can trust that no individual launch operator can change a farm's
    /// schedule or drain undistributed rewards. Captured per-farm at create
    /// time; updating this only affects future farms.
    pub farm_owner: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Spawn a new farm. Caller must:
    ///   - Include exactly the configured `instantiate_fee_inj` of `inj` in
    ///     `info.funds` (combined with the reward when the reward denom is
    ///     also `inj`).
    ///   - For **native** reward tokens: include the full `total_schedule`
    ///     amount of the reward denom in `info.funds`.
    ///   - For **CW20** reward tokens: pre-approve the factory to spend the
    ///     full `total_schedule` amount via `cw20::IncreaseAllowance`. The
    ///     factory pulls via `TransferFrom` and forwards to the farm.
    CreateFarm {
        reward_token: AssetInfo,
        staking_token: AssetInfo,
        distribution_schedule: Vec<(u64, u64, Uint128)>,
    },
    /// Owner-only. Update mutable config fields. `fee_collector`,
    /// `instantiate_fee_inj`, and `farm_owner` only affect FUTURE
    /// `CreateFarm` calls, so they're applied immediately. `farm_code_id`
    /// is excluded — it goes through the timelocked
    /// `ProposeUpdateFarmCodeId` / `ApplyUpdateFarmCodeId` path so users
    /// have an exit window before the code that new farms are spawned with
    /// changes.
    UpdateConfig {
        fee_collector: Option<String>,
        instantiate_fee_inj: Option<Uint128>,
        farm_owner: Option<String>,
    },
    /// H-2: owner proposes a new `farm_code_id`. The actual swap is gated
    /// behind `ApplyUpdateFarmCodeId`, which requires `TIMELOCK_DELAY_SECONDS`
    /// to have elapsed.
    ProposeUpdateFarmCodeId { farm_code_id: u64 },
    /// Owner-only. Finalizes a pending farm_code_id swap once the timelock
    /// has expired.
    ApplyUpdateFarmCodeId {},
    /// Owner-only. Clears a pending farm_code_id proposal.
    CancelUpdateFarmCodeIdProposal {},
    /// Owner proposes a new owner. The rotation cannot take effect until
    /// `TIMELOCK_DELAY_SECONDS` have elapsed.
    ProposeNewOwner { new_owner: String },
    /// Owner finalizes a pending owner rotation once the timelock has expired.
    ApplyOwnerRotation {},
    /// Owner clears a pending owner proposal.
    CancelOwnerProposal {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct MigrateMsg {}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    /// Current factory config (owner, fee collector, fee amount, farm code id).
    Config {},
    /// Pending owner rotation (if any).
    PendingOwnerRotation {},
    /// Pending farm_code_id swap (if any).
    PendingFarmCodeIdUpdate {},
    /// Farm record for `id`. Errors if the id is unknown.
    Farm { id: u64 },
    /// Farm record by farm contract address. Errors if the address is not in
    /// the registry (i.e. the farm was not factory-created).
    FarmByAddr { addr: String },
    /// Paginated registry scan. `start_after` is exclusive; `limit` defaults to
    /// 30 and is capped at 100.
    Farms {
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    /// Total number of farms ever created (== next id).
    FarmCount {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct ConfigResponse {
    pub owner: String,
    pub fee_collector: String,
    pub instantiate_fee_inj: Uint128,
    pub farm_code_id: u64,
    pub farm_owner: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingOwnerRotationResponse {
    pub pending_owner: Option<String>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingFarmCodeIdUpdateResponse {
    pub farm_code_id: Option<u64>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct FarmRecord {
    pub id: u64,
    pub farm_addr: String,
    /// User that paid the launch fee and chose the farm's parameters. Has NO
    /// on-chain role on the farm contract; surfaced for off-chain attribution.
    pub operator: String,
    /// Address installed as the farm's `Config.owner` and wasm admin. For
    /// factory-spawned farms this is the Choice governance multisig at the
    /// time of creation.
    pub farm_owner: String,
    pub reward_token: AssetInfo,
    pub staking_token: AssetInfo,
    pub total_reward: Uint128,
    pub created_at: u64,
    pub created_at_height: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct FarmsResponse {
    pub farms: Vec<FarmRecord>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct FarmCountResponse {
    pub count: u64,
}

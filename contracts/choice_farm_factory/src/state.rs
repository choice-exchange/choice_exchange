use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::{CanonicalAddr, Uint128};
use cw_storage_plus::{Item, Map};

use choice::asset::AssetInfo;

/// Native denom of INJ in base units (1 INJ = 1e18 inj). The factory's launch
/// fee is always denominated in this.
pub const INJ_DENOM: &str = "inj";

/// Delay between proposing and applying an owner rotation. Mirrors the farm
/// contract's timelock so users have 48h to react before factory control
/// changes hands.
pub const TIMELOCK_DELAY_SECONDS: u64 = 48 * 60 * 60;

/// Reply id used for the `WasmMsg::Instantiate` that spawns the child farm.
/// The reply handler reads the instantiated address from the wasm event and
/// finalizes the `FarmRecord`.
pub const INSTANTIATE_FARM_REPLY_ID: u64 = 1;

/// Pagination defaults for `QueryMsg::Farms`.
pub const DEFAULT_LIMIT: u32 = 30;
pub const MAX_LIMIT: u32 = 100;

/// Maximum number of slots a `CreateFarm` schedule may contain. Mirrors
/// `choice_farm::MAX_SCHEDULE_SLOTS` so the factory rejects clearly before
/// burning gas on a sub-call. 20 is comfortably above any realistic
/// multi-phase emission plan.
pub const MAX_SCHEDULE_SLOTS: usize = 20;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct Config {
    pub owner: CanonicalAddr,
    pub fee_collector: CanonicalAddr,
    pub instantiate_fee_inj: Uint128,
    pub farm_code_id: u64,
    /// Address installed as the `Config.owner` and wasm admin of every
    /// factory-spawned farm. Captured into each `FarmRecord` at create time;
    /// changes here only affect farms created afterwards.
    pub farm_owner: CanonicalAddr,
    pub pending_owner: Option<CanonicalAddr>,
    pub pending_owner_effective_at: Option<u64>,
}

/// One row of the farm registry. Stored under both the auto-incrementing id
/// (`FARMS`) and the bech32 farm address (`FARM_BY_ADDR`) for cheap lookups
/// from indexers and front ends.
///
/// `operator` is the user that paid the launch fee and chose the schedule;
/// they have NO on-chain role on the farm contract itself. The farm's
/// `Config.owner` (admin powers) is `farm_owner`, captured here so the
/// registry stays informative even after the factory's global `farm_owner`
/// changes for future farms.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct FarmRecord {
    pub id: u64,
    pub farm_addr: CanonicalAddr,
    pub operator: CanonicalAddr,
    pub farm_owner: CanonicalAddr,
    pub reward_token: AssetInfo,
    pub staking_token: AssetInfo,
    /// Sum of the third element of every schedule slot. The on-chain truth for
    /// "how much was committed at launch."
    pub total_reward: Uint128,
    pub created_at: u64,
    pub created_at_height: u64,
}

/// Pending `farm_code_id` swap. Absent when no swap is queued.
/// H-2: code-id swap is two-step — `ProposeUpdateFarmCodeId` sets this item,
/// `ApplyUpdateFarmCodeId` clears it and installs the new code id once the
/// timelock has elapsed. Re-proposing overwrites and resets the timer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingFarmCodeIdUpdate {
    pub farm_code_id: u64,
    /// Unix timestamp (seconds) at which the swap may be applied.
    pub effective_at: u64,
}

/// Transient row written by `execute_create_farm` and consumed by the
/// `reply` handler. There is at most one outstanding `CreateFarm` per tx, so a
/// singleton `Item` is sufficient — `reply_on_success` guarantees we always
/// pair an execute with its reply within the same tx, and a failing submsg
/// reverts the whole tx (clearing the write).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub struct PendingFarm {
    pub id: u64,
    pub operator: CanonicalAddr,
    pub farm_owner: CanonicalAddr,
    pub reward_token: AssetInfo,
    pub staking_token: AssetInfo,
    pub total_reward: Uint128,
    /// How the reply handler delivers the reward funding to the new farm.
    /// Always populated (never `None` in real flows) so native and cw20 paths
    /// stay symmetric: the farm is instantiated with empty `funds` and the
    /// reply atomically credits `undistributed_rewards`. Kept `Option` only so
    /// future flows that pre-fund via instantiate funds can skip the reply
    /// step explicitly.
    pub fund_after: Option<FundAfter>,
}

/// How the reply handler funds the newly-instantiated farm. Both variants
/// route through the farm's `Fund {}` (native) or `Cw20HookMsg::Fund {}`
/// (cw20) entrypoint, so `state.undistributed_rewards` is credited atomically
/// with farm creation and the farm contract never holds untracked balance.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
pub enum FundAfter {
    /// Send `amount` of native `denom` along with an `ExecuteMsg::Fund {}` to
    /// the new farm. The factory must hold the funds at reply time.
    Native { denom: String, amount: Uint128 },
    /// Cw20::Send { contract: <farm>, amount, msg: Cw20HookMsg::Fund {} }.
    /// Factory must hold the cw20 tokens at reply time (pulled via
    /// `TransferFrom` in the execute step).
    Cw20 { cw20_contract: String, amount: Uint128 },
}

pub const CONFIG: Item<Config> = Item::new("config");
pub const NEXT_FARM_ID: Item<u64> = Item::new("next_farm_id");
pub const FARMS: Map<u64, FarmRecord> = Map::new("farms");
pub const FARM_BY_ADDR: Map<&[u8], u64> = Map::new("farm_by_addr");
pub const PENDING_FARM: Item<PendingFarm> = Item::new("pending_farm");
pub const PENDING_FARM_CODE_ID: Item<PendingFarmCodeIdUpdate> =
    Item::new("pending_farm_code_id");

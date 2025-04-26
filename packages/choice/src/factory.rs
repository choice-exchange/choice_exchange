use cosmwasm_std::Uint128;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::asset::{Asset, AssetInfo, PairInfo};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    pub pair_code_id: u64,
    pub burn_address: String,
    pub fee_wallet_address: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct UpdateConfigParams {
    pub owner: Option<String>,
    pub pair_code_id: Option<u64>,
    pub burn_address: Option<String>,
    pub fee_wallet_address: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// UpdateConfig update relevant code IDs
    UpdateConfig {
        params: UpdateConfigParams,
    },
    /// CreatePair instantiates pair contract
    CreatePair {
        assets: [Asset; 2],
    },
    AddNativeTokenDecimals {
        denom: String,
        decimals: u8,
    },
    MigratePair {
        contract: String,
        code_id: Option<u64>,
    },
    WithdrawNative {
        denom: String,
        amount: Uint128,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Config {},
    Pair {
        asset_infos: [AssetInfo; 2],
    },
    Pairs {
        start_after: Option<[AssetInfo; 2]>,
        limit: Option<u32>,
    },
    NativeTokenDecimals {
        denom: String,
    },
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct ConfigResponse {
    pub owner: String,
    pub pair_code_id: u64,

    pub burn_address: String,       // New field
    pub fee_wallet_address: String, // New field
}

/// We currently take no arguments for migrations
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct MigrateMsg {}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct PairsResponse {
    pub pairs: Vec<PairInfo>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct NativeTokenDecimalsResponse {
    pub decimals: u8,
}

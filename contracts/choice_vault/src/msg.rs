use choice::asset::AssetInfo;
use cosmwasm_std::Uint128;
use cw20::Cw20ReceiveMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize}; // Using the shared library

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    pub owner: String,
    pub pair_contract: String,
    pub farm_contract: String,
    pub lp_token: String,
    pub reward_token: AssetInfo,
    pub asset_infos: [AssetInfo; 2],
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Handles receiving CW20 tokens.
    Receive(Cw20ReceiveMsg),

    /// Withdraws a user's funds by redeeming shares.
    Withdraw { shares: Uint128 },

    /// Triggers the auto-compounding of rewards.
    Compound {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cw20HookMsg {
    /// The hook for depositing LP tokens into the vault.
    Deposit {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    /// Returns the contract's configuration.
    Config {},
    /// Returns the total number of shares issued.
    TotalShares {},
    /// Returns information for a specific user.
    UserInfo { user: String },
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct UserInfoResponse {
    pub shares: Uint128,
}

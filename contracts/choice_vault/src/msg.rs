use choice::asset::AssetInfo;
use cosmwasm_std::{Decimal, Uint128};
use cw20::Cw20ReceiveMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    pub owner: String,
    pub pair_contract: String,
    pub farm_contract: String,
    pub lp_token: AssetInfo,
    pub reward_token: AssetInfo,
    pub asset_infos: [AssetInfo; 2],
    pub fee_recipient: Option<String>,
    pub fee_percentage: Option<Decimal>,
    pub minimum_reward_to_compound: Uint128,
    pub compounder: String,
    pub slippage_tolerance: Decimal,
    pub reward_to_lp_token_route: Vec<SwapHop>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SwapHop {
    pub pair_contract: String,
    pub to_asset_info: AssetInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct HarvestReplyPayload {
    pub belief_prices: Vec<Decimal>,
    pub reward_amount_to_compound: Uint128,
    pub tvl_before_compound: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct CompoundRoutePayload {
    /// The index of the *next* hop to be executed.
    pub hop_index: u32,
    /// The full list of belief prices for the entire operation.
    pub belief_prices: Vec<Decimal>,
    /// For compounding info
    pub reward_amount_to_compound: Uint128,
    pub tvl_before_compound: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Handles receiving CW20 tokens.
    Receive(Cw20ReceiveMsg),

    /// Handles receiving native LP tokens. This is the new entry point for native deposits.
    DepositNativeLp {},

    /// Withdraws a user's funds by redeeming shares.
    Withdraw {
        shares: Uint128,
    },

    /// Triggers the auto-compounding of rewards.
    Compound {
        belief_prices: Vec<Decimal>,
    },

    /// Allows the owner to update the fee configuration.
    UpdateConfig {
        compounder: Option<String>,
        slippage_tolerance: Option<Decimal>,
        fee_recipient: Option<String>,
        fee_percentage: Option<Decimal>,
        minimum_reward_to_compound: Option<Uint128>,
    },
    ProposeNewOwner {
        new_owner: String,
    },
    AcceptOwnership,
    CancelOwnershipProposal,
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
    /// Returns information about the last compounding event for APR calculation.
    CompoundingInfo {},
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct UserInfoResponse {
    pub shares: Uint128,
}

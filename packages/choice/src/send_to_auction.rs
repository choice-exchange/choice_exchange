use crate::asset::Asset;
use cw20::Cw20ReceiveMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct InstantiateMsg {
    pub owner: String,
    pub adapter_contract: String,
    pub burn_auction_subaccount: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    Receive(Cw20ReceiveMsg),
    SendNative {
        asset: Asset,
    },
    UpdateConfig {
        adapter_contract: Option<String>,
        burn_auction_subaccount: Option<String>,
    },
    ProposeNewOwner {
        new_owner: String,
    },
    AcceptOwnership,
    CancelOwnershipProposal,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    GetConfig {},
}

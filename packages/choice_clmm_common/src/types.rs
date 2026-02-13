use cosmwasm_schema::cw_serde;
use cosmwasm_std::{to_json_binary, BankMsg, Coin, CosmosMsg, StdResult, Uint128, WasmMsg};
use cw20::Cw20ExecuteMsg;
use std::fmt;

/// Represents a token type: either a native bank denom or a CW20 contract.
#[cw_serde]
#[derive(Eq)]
pub enum AssetInfo {
    NativeToken { denom: String },
    Token { contract_addr: String },
}

impl AssetInfo {
    pub fn is_native(&self) -> bool {
        matches!(self, AssetInfo::NativeToken { .. })
    }

    /// Returns the canonical string key (denom or contract address).
    pub fn key(&self) -> &str {
        match self {
            AssetInfo::NativeToken { denom } => denom,
            AssetInfo::Token { contract_addr } => contract_addr,
        }
    }

    /// Build a transfer message to send `amount` of this asset to `recipient`.
    pub fn transfer_msg(&self, recipient: &str, amount: Uint128) -> StdResult<CosmosMsg> {
        match self {
            AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Bank(BankMsg::Send {
                to_address: recipient.to_string(),
                amount: vec![Coin {
                    denom: denom.clone(),
                    amount,
                }],
            })),
            AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                    recipient: recipient.to_string(),
                    amount,
                })?,
                funds: vec![],
            })),
        }
    }

    /// Build a TransferFrom message (CW20 only). Returns None for native tokens.
    pub fn transfer_from_msg(
        &self,
        owner: &str,
        recipient: &str,
        amount: Uint128,
    ) -> StdResult<Option<CosmosMsg>> {
        match self {
            AssetInfo::NativeToken { .. } => Ok(None),
            AssetInfo::Token { contract_addr } => Ok(Some(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                    owner: owner.to_string(),
                    recipient: recipient.to_string(),
                    amount,
                })?,
                funds: vec![],
            }))),
        }
    }

    /// Build an IncreaseAllowance message (CW20 only). Returns None for native tokens.
    pub fn increase_allowance_msg(
        &self,
        spender: &str,
        amount: Uint128,
    ) -> StdResult<Option<CosmosMsg>> {
        match self {
            AssetInfo::NativeToken { .. } => Ok(None),
            AssetInfo::Token { contract_addr } => Ok(Some(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                    spender: spender.to_string(),
                    amount,
                    expires: None,
                })?,
                funds: vec![],
            }))),
        }
    }
}

impl fmt::Display for AssetInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.key())
    }
}

/// Ordering: NativeToken < Token; within same variant, lexicographic by key.
impl PartialOrd for AssetInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AssetInfo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (AssetInfo::NativeToken { denom: a }, AssetInfo::NativeToken { denom: b }) => a.cmp(b),
            (AssetInfo::NativeToken { .. }, AssetInfo::Token { .. }) => std::cmp::Ordering::Less,
            (AssetInfo::Token { .. }, AssetInfo::NativeToken { .. }) => std::cmp::Ordering::Greater,
            (AssetInfo::Token { contract_addr: a }, AssetInfo::Token { contract_addr: b }) => {
                a.cmp(b)
            }
        }
    }
}

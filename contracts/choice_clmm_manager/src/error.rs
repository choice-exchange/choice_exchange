use cosmwasm_std::StdError;
use cw721::error::Cw721ContractError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error(transparent)]
    Cw721(#[from] Cw721ContractError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Invalid trait: {trait_name}")]
    InvalidTrait { trait_name: String },

    #[error("Deadline exceeded")]
    DeadlineExceeded {},

    #[error("Slippage: {reason}")]
    Slippage { reason: String },

    #[error("Position not cleared: {field} is non-zero (burn requires zero liquidity and zero tokens_owed)")]
    PositionNotCleared { field: String },

    #[error("Invalid reply payload: {reason}")]
    InvalidReply { reason: String },
}

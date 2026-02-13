use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Token0 must be lexicographically smaller than Token1")]
    InvalidTokenOrder {},

    #[error("Position Not Found")]
    PositionNotFound {},

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Insufficient output: expected at least {minimum}, got {actual}")]
    InsufficientOutput { minimum: String, actual: String },

    #[error("Deadline exceeded")]
    DeadlineExceeded {},

    #[error("Invalid funds: {reason}")]
    InvalidFunds { reason: String },

    #[error("Zero amount specified")]
    ZeroAmount {},

    #[error("Invalid config: {reason}")]
    InvalidConfig { reason: String },
}

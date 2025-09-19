use cosmwasm_std::StdError;
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Insufficient shares to withdraw")]
    InsufficientShares {},

    #[error("Invalid CW20 hook message")]
    InvalidCw20HookMsg {},
}

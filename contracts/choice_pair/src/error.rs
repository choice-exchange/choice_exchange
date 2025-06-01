use cosmwasm_std::{ConversionOverflowError, OverflowError, StdError};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("{0}")]
    OverflowError(#[from] OverflowError),

    #[error("{0}")]
    ConversionOverflowError(#[from] ConversionOverflowError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Invalid zero amount")]
    InvalidZeroAmount {},

    #[error("Max spread assertion")]
    MaxSpreadAssertion {},

    #[error("Asset mismatch")]
    AssetMismatch {},

    #[error("Min amount assertion ({min_asset} > {asset})")]
    MinAmountAssertion { min_asset: String, asset: String },

    #[error("Expired deadline")]
    ExpiredDeadline {},

    #[error("Max slippage assertion")]
    MaxSlippageAssertion {},

    #[error("Invalid LP token funds provided; expected a matching LP token with the exact withdrawal amount")]
    InvalidLiquidityFunds {},

    #[error("More initial liquidity needed ({min_lp_token} > {given_lp})")]
    MinimumLiquidityAmountError {
        min_lp_token: String,
        given_lp: String,
    },

    #[error("LP supply overflow")]
    LpSupplyOverflow {}
}

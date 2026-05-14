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

    #[error("Zap input must be a single native coin (got {count})")]
    InvalidInputFunds { count: usize },

    #[error("Zap input amount is zero")]
    ZeroInputAmount {},

    #[error("Input asset {got} does not match either side of the pair")]
    InputAssetMismatch { got: String },

    #[error("Receive hook payload must not carry native funds (got {count})")]
    ReceiveWithFunds { count: usize },

    #[error("Pair has no liquidity; zap requires an initialized pool")]
    EmptyPool {},

    #[error("min_lp_out not met: got {got}, expected at least {min}")]
    MinLpAssertion { got: String, min: String },

    #[error("Expired deadline")]
    ExpiredDeadline {},

    #[error("Optimal-split math overflowed")]
    SplitMathOverflow {},

    #[error("tip_bps {value} exceeds maximum {max}")]
    TipTooHigh { value: u16, max: u16 },

    #[error("Balance {balance} below min_zap_amount {min}")]
    BalanceBelowMin { balance: String, min: String },

    #[error("default_recipient must be set before ZapBalance can run")]
    DefaultRecipientUnset {},

    #[error("royalty route (input + pair) must be set before ZapBalance can run")]
    RoyaltyRouteUnset {},

    #[error("Caller is not in the keeper allowlist")]
    NotKeeper {},

    #[error("Migration state missing: no v1 config bytes at storage key `config`")]
    MigrationStateMissing {},

    #[error("Invalid migration: cannot apply variant `{requested}` from current version `{from}`")]
    InvalidMigration { from: String, requested: String },

    #[error("max_spread {value} exceeds maximum {max}")]
    MaxSpreadTooHigh { value: String, max: String },

    #[error("slippage_tolerance {value} exceeds maximum {max}")]
    SlippageToleranceTooHigh { value: String, max: String },
}

use cosmwasm_std::{OverflowError, StdError};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("{0}")]
    Overflow(#[from] OverflowError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Action `{action}` requires the {required} role; this instance is a {actual}")]
    WrongRole {
        action: String,
        required: String,
        actual: String,
    },

    #[error("tip_bps {value} exceeds factory's max_tip_bps {max}")]
    TipTooHigh { value: u16, max: u16 },

    #[error(
        "SinkInit.choice_factory `{got}` does not match factory's pinned choice_factory `{expected}`"
    )]
    SinkChoiceFactoryMismatch { got: String, expected: String },

    #[error(
        "Clmm sink rejected: factory pins clmm_{which} `{expected}` but sink_init supplied `{got}`"
    )]
    SinkClmmAddressMismatch {
        which: String,
        got: String,
        expected: String,
    },

    #[error("Clmm graduation is not configured on this factory (clmm_factory / clmm_manager unset)")]
    ClmmNotConfigured {},

    #[error("FactoryInit must set clmm_factory and clmm_manager together, or neither")]
    ClmmHalfConfigured {},

    #[error("fee tier {fee} is not enabled on the CLMM factory")]
    FeeTierNotSupported { fee: u32 },

    #[error("CLMM pool for this token pair + fee tier already exists at `{pool}` — refusing to seed into a pre-priced pool")]
    ClmmPoolAlreadyExists { pool: String },

    #[error("Clmm Settle takes no funds; attach nothing (CLMM pool creation is free)")]
    UnexpectedFundsForClmmSettle {},

    #[error("Locker has no admin; beneficiary is immutable")]
    LockerNoAdmin {},

    #[error("Locker owns no position NFTs to collect fees on")]
    LockerNoPositions {},

    #[error("token_denom and pair_denom must differ; got `{denom}` for both")]
    SameDenom { denom: String },

    #[error("deadline_seconds must be > 0")]
    ZeroDeadline {},

    #[error("Sink is already terminal (status={status})")]
    SinkTerminal { status: String },

    #[error(
        "Settle requires both balances non-zero: token_denom balance={token}, pair_denom balance={pair}"
    )]
    InsufficientBalanceForSettle { token: String, pair: String },

    #[error(
        "Attach the tokenfactory create-pair fee in `info.funds`: need {required} {denom}, supplied {supplied}"
    )]
    InsufficientCreateFee {
        denom: String,
        required: String,
        supplied: String,
    },

    #[error(
        "info.funds must equal the chain's create-pair fee exactly: {denom} required {required}, supplied {supplied}"
    )]
    CreateFeeOverpaid {
        denom: String,
        required: String,
        supplied: String,
    },

    #[error("info.funds carries unexpected denom `{denom}` (only the chain's create-pair fee may be attached)")]
    UnexpectedFundsDenom { denom: String },

    #[error("tip absorbed the entire pair-asset balance; deposit would be zero")]
    TipExhaustsBalance {},

    #[error("Refund deadline not yet reached: {remaining_seconds}s remaining")]
    RefundDeadlineNotReached { remaining_seconds: u64 },

    #[error("Refund refused: sink holds no token_denom or pair_denom balance")]
    NothingToRefund {},

    #[error(
        "Pair was not registered by choice_factory after CreatePair — expected a Pair entry but got none"
    )]
    PairNotFoundPostCreate {},

    #[error("ProvideLiquidity minted zero LP — likely both deposits were dust")]
    ZeroLpMinted {},

    #[error("Self-callback invoked by an external sender")]
    CallbackUnauthorized {},

    #[error(
        "Invalid migration: cannot apply variant `{requested}` from current version `{from}`"
    )]
    InvalidMigration { from: String, requested: String },
}

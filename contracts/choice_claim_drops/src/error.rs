use cosmwasm_std::{StdError, Uint128};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("unauthorized")]
    Unauthorized,

    #[error("campaign {id} not found")]
    CampaignNotFound { id: u64 },

    #[error("contract is paused")]
    Paused,

    #[error("campaign is paused")]
    CampaignPaused,

    #[error("campaign is frozen; its root can no longer change")]
    Frozen,

    #[error("cannot freeze a campaign before its first root is published")]
    NoRoot,

    #[error("campaign has expired")]
    Expired,

    #[error("campaign has not expired yet")]
    NotExpired,

    #[error("campaign has already been swept")]
    Swept,

    #[error("declared total may not decrease (current {current}, new {new})")]
    TotalDecreased { current: Uint128, new: Uint128 },

    #[error("merkle root must be exactly 32 bytes")]
    InvalidRoot,

    #[error("merkle proof does not match the current or previous root")]
    InvalidProof,

    #[error("merkle proof too long")]
    ProofTooLong,

    #[error("nothing to claim")]
    NothingToClaim,

    #[error("claim exceeds campaign funds: the published tree over-allocates its declared total")]
    ExceedsCampaignFunds,

    #[error("must attach exactly {expected} {denom}")]
    InvalidFunds { expected: Uint128, denom: String },

    #[error("this message accepts no funds")]
    UnexpectedFunds,

    #[error("denom must not be empty")]
    InvalidDenom,

    #[error("invalid expiry: {reason}")]
    InvalidExpiry { reason: String },

    #[error("fee_bps may not exceed {max}")]
    FeeTooHigh { max: u16 },

    #[error("meta too long (max {max} bytes)")]
    MetaTooLong { max: usize },

    #[error("leaves_uri too long (max {max} bytes)")]
    LeavesUriTooLong { max: usize },

    #[error("streaming campaigns can only be created by the contract owner")]
    StreamingRequiresOwner,

    #[error("a keeper can only be set on a streaming campaign")]
    KeeperRequiresStreaming,

    #[error("this action requires a streaming campaign")]
    NotStreaming,

    #[error("no pending ownership transfer to accept")]
    NoPendingOwnership,

    #[error("no pending creator transfer to accept")]
    NoPendingCreator,

    #[error("a frozen perpetual campaign cannot be given an expiry")]
    FrozenExpiryLocked,

    #[error("nothing to rescue for this denom")]
    NothingToRescue,

    #[error("rescue exceeds unencumbered balance (available {available})")]
    RescueExceedsExcess { available: Uint128 },
}

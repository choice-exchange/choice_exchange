use cosmwasm_std::{Decimal, StdError, Uint128};
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

    #[error("Fee percentage must be between 0 and 1")]
    InvalidFeePercentage {},

    #[error("slippage_tolerance {got} exceeds maximum allowed {max}")]
    SlippageToleranceAboveMax { got: Decimal, max: Decimal },

    #[error("Batch size exceeds the maximum limit")]
    BatchTooLarge {},

    #[error("Invalid belief_prices length: expected {expected}, got {got}")]
    InvalidBeliefPrices { expected: usize, got: usize },

    #[error("Belief price must be greater than zero")]
    ZeroBeliefPrice {},

    #[error("Minted LP {got} below minimum_lp_to_receive {minimum}")]
    InsufficientLpReceived { minimum: Uint128, got: Uint128 },

    #[error("minimum_lp_to_receive must be non-zero — callers must commit to an LP floor")]
    MinimumLpToReceiveZero {},

    #[error("vault is paused — entry paths are disabled; use WithdrawPending/WithdrawShares")]
    VaultPaused {},

    #[error("Pending farm rewards {pending} must be compounded before activating deposits")]
    PendingRewardsMustBeCompounded { pending: Uint128 },

    #[error("Compounder rotation timelock has not elapsed")]
    CompounderRotationNotReady {},

    #[error("No compounder rotation is pending")]
    NoPendingCompounderRotation {},

    #[error(
        "minimum_lp_to_receive {minimum} below heuristic floor {floor} — caller must \
         commit to at least ~10% of the fair-market expected LP. Raise the floor or \
         reduce minimum_reward_to_compound if expected LP has shrunk."
    )]
    MinimumLpBelowHeuristic { minimum: Uint128, floor: Uint128 },

    #[error("max_slippage_tolerance proposal already pending — cancel it before proposing again")]
    MaxSlippageRaiseAlreadyPending {},

    #[error("No max_slippage_tolerance raise is pending")]
    NoPendingMaxSlippageRaise {},

    #[error("max_slippage_tolerance raise timelock has not elapsed")]
    MaxSlippageRaiseNotReady {},

    #[error("max_slippage_tolerance {proposed} must be strictly greater than current {current} to propose a raise")]
    MaxSlippageMustBeHigher { proposed: Decimal, current: Decimal },

    #[error(
        "max_slippage_tolerance {proposed} must be at most the current cap {current} to tighten"
    )]
    MaxSlippageMustNotRaise { proposed: Decimal, current: Decimal },

    #[error("max_slippage_tolerance {proposed} exceeds absolute ceiling {ceiling}")]
    MaxSlippageAboveCeiling { proposed: Decimal, ceiling: Decimal },

    #[error("Caller has no pending deposit to activate")]
    NoPendingDeposit {},

    #[error(
        "Compound path must terminate on one of the pair's assets — \
         set a `reward_to_lp_token_route` whose last hop is a pair asset, \
         or use a reward_token that is already a pair asset"
    )]
    CompoundPathMustEndOnPairAsset {},
}

use cosmwasm_std::{StdError, Uint128};
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

    #[error("Batch size exceeds the maximum limit")]
    BatchTooLarge {},

    #[error("Invalid belief_prices length: expected {expected}, got {got}")]
    InvalidBeliefPrices { expected: usize, got: usize },

    #[error("Belief price must be greater than zero")]
    ZeroBeliefPrice {},

    #[error("Minted LP {got} below minimum_lp_to_receive {minimum}")]
    InsufficientLpReceived { minimum: Uint128, got: Uint128 },

    #[error("Pending farm rewards {pending} must be compounded before activating deposits")]
    PendingRewardsMustBeCompounded { pending: Uint128 },

    #[error("Compounder rotation timelock has not elapsed")]
    CompounderRotationNotReady {},

    #[error("No compounder rotation is pending")]
    NoPendingCompounderRotation {},

    #[error("Caller has no pending deposit to activate")]
    NoPendingDeposit {},

    #[error(
        "Compound path must terminate on one of the pair's assets — \
         set a `reward_to_lp_token_route` whose last hop is a pair asset, \
         or use a reward_token that is already a pair asset"
    )]
    CompoundPathMustEndOnPairAsset {},
}

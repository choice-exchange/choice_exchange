use choice::asset::AssetInfo;
use cosmwasm_std::{Decimal, Uint128};
use cw20::Cw20ReceiveMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    pub owner: String,
    pub pair_contract: String,
    pub farm_contract: String,
    pub lp_token: AssetInfo,
    pub reward_token: AssetInfo,
    pub asset_infos: [AssetInfo; 2],
    pub fee_recipient: Option<String>,
    pub fee_percentage: Option<Decimal>,
    pub minimum_reward_to_compound: Uint128,
    pub compounder: String,
    pub slippage_tolerance: Decimal,
    pub reward_to_lp_token_route: Vec<SwapHop>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SwapHop {
    pub pair_contract: String,
    pub to_asset_info: AssetInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct HarvestReplyPayload {
    pub reward_amount_to_compound: Uint128,
    pub tvl_before_compound: Uint128,
    /// Caller-supplied belief prices, one per swap, in execution order.
    /// Length is `route.len() + 1` when a route is configured, else `1`.
    pub belief_prices: Vec<Decimal>,
    /// Reverts the compound when newly-minted LP is below this floor.
    /// Non-zero by contract (validated in `execute_compound`).
    pub minimum_lp_to_receive: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct WithdrawSharesReplyPayload {
    /// Address receiving the exit proceeds.
    pub recipient: String,
    /// Shares being burnt — used to size the exiter's slice of the harvested reward_token.
    pub shares_burnt: Uint128,
    /// Total shares at the moment the exiter committed, before the burn was applied.
    /// Used as the denominator when sizing the reward distribution.
    pub total_shares_pre_burn: Uint128,
    /// LP tokens owed to the exiter from their share of bond_amount.
    pub lp_to_withdraw: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct CompoundRoutePayload {
    /// The index of the *next* hop to be executed.
    pub hop_index: u32,
    /// For compounding info
    pub reward_amount_to_compound: Uint128,
    pub tvl_before_compound: Uint128,
    pub belief_prices: Vec<Decimal>,
    pub minimum_lp_to_receive: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Handles receiving CW20 tokens.
    Receive(Cw20ReceiveMsg),

    /// Handles receiving native LP tokens. This is the new entry point for native deposits.
    Deposit {},

    /// Withdraws a user's pending LP tokens that have not yet been converted to shares.
    WithdrawPending {
        /// The amount of LP tokens to withdraw from the pending balance.
        /// If None, it withdraws the entire pending balance.
        amount: Option<Uint128>,
    },

    /// Withdraws a user's funds by redeeming active, value-accruing shares.
    WithdrawShares {
        /// The number of shares to burn in exchange for the underlying LP tokens.
        shares_to_burn: Uint128,
    },

    /// Triggers the auto-compounding of rewards. **Permissionless** — any caller
    /// may invoke, but MUST commit to a non-zero `minimum_lp_to_receive` so the
    /// vault's downside is bounded if they set lax belief prices or get
    /// sandwiched (C-3).
    ///
    /// `belief_prices` is the caller's expected `offer_pool / ask_pool` for each
    /// swap in the compound flow, in order. Length must equal the number of
    /// swaps the contract will perform (`route.len() + 1` when a route is
    /// configured, else `1`). Combined with the configured
    /// `slippage_tolerance`, these are what prevent a sandwicher from
    /// manipulating the AMM price during the compound tx.
    ///
    /// `minimum_lp_to_receive` reverts the tx when the liquidity provision
    /// mints less than this many LP tokens for the vault. Required and must be
    /// non-zero.
    Compound {
        belief_prices: Vec<Decimal>,
        minimum_lp_to_receive: Uint128,
    },

    /// Keeper-only function to activate pending deposits for a batch of users.
    ActivatePendingDeposits {
        users: Vec<String>,
    },

    /// Lets any user activate their own pending deposit without waiting on the keeper.
    /// Subject to the same dilution guard as `ActivatePendingDeposits` — pending farm
    /// rewards must be below the compound threshold before activation proceeds.
    ActivateMyDeposit {},

    /// Allows the owner to update the fee configuration.
    /// Compounder rotation is intentionally excluded — use
    /// `ProposeCompounder`/`ApplyCompounderRotation` which enforce a timelock.
    UpdateConfig {
        slippage_tolerance: Option<Decimal>,
        fee_recipient: Option<String>,
        fee_percentage: Option<Decimal>,
        minimum_reward_to_compound: Option<Uint128>,
    },
    /// Owner-only. Clears `fee_recipient` so future compounds skip the fee transfer
    /// entirely. `UpdateConfig` can only *set* the recipient; this is the only way
    /// to fully remove it (zeroing `fee_percentage` is a separate workaround).
    ClearFeeRecipient,
    /// Owner proposes a new compounder. The swap cannot take effect until
    /// `COMPOUNDER_ROTATION_DELAY_SECONDS` have elapsed. Proposing again resets the timer.
    ProposeCompounder {
        new_compounder: String,
    },
    /// Owner-only. Finalizes a pending compounder rotation once the timelock has expired.
    ApplyCompounderRotation,
    /// Owner-only. Clears any pending compounder rotation.
    CancelCompounderProposal,
    ProposeNewOwner {
        new_owner: String,
    },
    AcceptOwnership,
    CancelOwnershipProposal,
    /// H-4: owner-only. Freezes all entry paths (deposit, compound, activation). Exits
    /// remain open. Idempotent — re-pausing a paused vault is a no-op.
    Pause,
    /// H-4: owner-only. Lifts the freeze.
    Unpause,

    /// Owner-only. Instantly lowers `config.max_slippage_tolerance` to `new_max`.
    /// Must be `<= config.max_slippage_tolerance` — raises go through the
    /// timelocked `ProposeMaxSlippageRaise` flow. Does not touch any pending
    /// raise proposal (cancel explicitly if undesired).
    TightenMaxSlippage { new_max: Decimal },
    /// Owner-only. Proposes a raise of `config.max_slippage_tolerance` to `new_max`.
    /// Must be strictly greater than the current cap and at or below
    /// `MAX_SLIPPAGE_TOLERANCE_CEILING`. Applies after
    /// `MAX_SLIPPAGE_RAISE_DELAY_SECONDS` via `ApplyMaxSlippageRaise` — the
    /// 48h window gives depositors time to exit if they disagree with the new
    /// ceiling. Only one raise can be pending at a time.
    ProposeMaxSlippageRaise { new_max: Decimal },
    /// Owner-only. Finalizes the pending cap raise once the timelock elapses.
    ApplyMaxSlippageRaise,
    /// Owner-only. Clears the pending cap raise proposal.
    CancelMaxSlippageProposal,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct MigrateMsg {}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Cw20HookMsg {
    /// The hook for depositing LP tokens into the vault.
    Deposit {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    /// Returns the contract's configuration.
    Config {},
    /// Returns the total number of shares issued.
    TotalShares {},
    /// Returns information for a specific user.
    UserInfo { user: String },
    /// Returns information about the last compounding event for APR calculation.
    CompoundingInfo {},
    /// Pending deposits for the keeper bot to query
    PendingDeposits {
        start_after: Option<String>,
        limit: Option<u32>,
    },
    /// Returns the total amount of LP tokens in pending deposits.
    TotalPendingDeposits {},
    /// Returns the pending compounder rotation, if any.
    PendingCompounderRotation {},
    /// Returns the pending `max_slippage_tolerance` raise, if any.
    PendingMaxSlippageRaise {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct PendingCompounderRotationResponse {
    pub pending_compounder: Option<String>,
    pub effective_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct PendingMaxSlippageRaiseResponse {
    pub pending_max_slippage: Option<Decimal>,
    pub effective_at: Option<u64>,
}

// We define a custom struct for each query response
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct UserInfoResponse {
    pub shares: Uint128,
    pub pending_deposit: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct PendingDepositsResponse {
    pub users: Vec<String>,
    pub last_user: Option<String>,
}

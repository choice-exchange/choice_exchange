use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{Addr, HexBinary, Timestamp, Uint128};

use crate::state::Campaign;

#[cw_serde]
pub struct InstantiateMsg {
    /// Defaults to the instantiator.
    pub owner: Option<String>,
    /// Platform fee in bps charged on top of funding deltas. Capped at 1000 (10%).
    pub fee_bps: u16,
    /// Defaults to the owner.
    pub fee_collector: Option<String>,
}

/// A root published atomically with `CreateCampaign` — lets a one-shot drop be
/// created, funded, and (being non-streaming) auto-frozen in a single tx, so
/// the claim link is never shareable before the root is immutable. Attach
/// exactly `total + fee`.
#[cw_serde]
pub struct InitialRoot {
    pub root: HexBinary,
    pub total: Uint128,
    pub leaves_uri: String,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Open a new campaign.
    ///
    /// `streaming` campaigns accept repeated `UpdateRoot`s and a keeper, and may
    /// ONLY be created by the contract owner. A non-owner (or `streaming:false`)
    /// creates a one-shot: it is auto-frozen on its first published root, so its
    /// already-funded float can never be reassigned by a later republish.
    ///
    /// Pass `initial` to publish the first root in the same tx (attach
    /// `total + fee`); omit it (no funds) to publish later via `UpdateRoot`.
    CreateCampaign {
        denom: String,
        meta: String,
        keeper: Option<String>,
        expiry: Option<Timestamp>,
        streaming: bool,
        initial: Option<InitialRoot>,
    },
    /// Publish (or, for streaming campaigns, replace) the merkle root of
    /// lifetime cumulative allocations. `total` may only rise; attach exactly
    /// `total - previous_total` plus the platform fee on that delta. Creator or
    /// keeper. A one-shot campaign is auto-frozen after this call.
    UpdateRoot {
        id: u64,
        root: HexBinary,
        total: Uint128,
        leaves_uri: String,
    },
    /// One-way: permanently lock the root, and (if perpetual) the expiry policy.
    /// Auto-invoked on a one-shot's first publish; the owner may also call it on
    /// a streaming campaign to end it.
    Freeze {
        id: u64,
    },
    /// Claim `amount − already_claimed`, where `amount` is the sender's
    /// cumulative leaf value proven against the current or previous root.
    Claim {
        id: u64,
        amount: Uint128,
        proof: Vec<HexBinary>,
    },
    /// Claim across several campaigns in one tx (portfolio "claim all").
    /// `allow_partial` (default false): when true, individual claims that would
    /// fail (paused / expired / already-claimed) are skipped rather than
    /// reverting the whole tx; errors only if every claim is skipped.
    ClaimMany {
        claims: Vec<ClaimMsg>,
        allow_partial: Option<bool>,
    },
    /// After expiry the creator sweeps whatever was never claimed.
    Clawback {
        id: u64,
    },
    /// Recover tokens sent to the contract outside campaign accounting (wrong
    /// address, wrong denom). Owner-only; capped at `balance − liabilities` so
    /// it can never touch funds owed to claimants. Defaults: sweep all excess
    /// to the owner.
    Rescue {
        denom: String,
        amount: Option<Uint128>,
        recipient: Option<String>,
    },
    /// Two-step ownership transfer (propose → accept).
    TransferOwnership {
        new_owner: String,
    },
    AcceptOwnership {},
    /// Two-step per-campaign creator transfer (rotate a creator key).
    TransferCreator {
        id: u64,
        new_creator: String,
    },
    AcceptCreator {
        id: u64,
    },
    SetKeeper {
        id: u64,
        keeper: Option<String>,
    },
    /// Extend an expiry, or wind down a perpetual campaign with ≥7 days notice.
    /// A frozen perpetual campaign's expiry is locked.
    SetExpiry {
        id: u64,
        expiry: Timestamp,
    },
    SetCampaignPaused {
        id: u64,
        paused: bool,
    },
    UpdateMeta {
        id: u64,
        meta: Option<String>,
        leaves_uri: Option<String>,
    },
    /// Owner config. Ownership is transferred via the two-step flow, not here.
    UpdateConfig {
        fee_bps: Option<u16>,
        fee_collector: Option<String>,
        paused: Option<bool>,
    },
}

#[cw_serde]
pub struct ClaimMsg {
    pub id: u64,
    pub amount: Uint128,
    pub proof: Vec<HexBinary>,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(crate::state::Config)]
    Config {},
    #[returns(CampaignResponse)]
    Campaign { id: u64 },
    #[returns(Vec<CampaignResponse>)]
    Campaigns {
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(Vec<CampaignResponse>)]
    CampaignsByCreator {
        creator: String,
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    #[returns(ClaimedResponse)]
    Claimed { id: u64, address: String },
    /// Paginated list of a campaign's claimants + their cumulative claimed
    /// amounts. Powers the trippytools manage view without an off-chain indexer.
    #[returns(ClaimsResponse)]
    Claims {
        id: u64,
        start_after: Option<String>,
        limit: Option<u32>,
    },
    /// Dry-run a claim: proof validity plus the payable amount right now
    /// (zero when the campaign is paused, expired, swept, or exhausted).
    #[returns(ClaimableResponse)]
    Claimable {
        id: u64,
        address: String,
        amount: Uint128,
        proof: Vec<HexBinary>,
    },
    /// Exact funds to attach to move a campaign's declared total to `new_total`,
    /// so integrators never have to replicate the fee rounding.
    #[returns(FundingRequiredResponse)]
    FundingRequired { id: u64, new_total: Uint128 },
    /// Amount currently owed to claimants for a denom (Σ remaining across its
    /// campaigns). Reconcile against the contract's bank balance.
    #[returns(LiabilitiesResponse)]
    Liabilities { denom: String },
}

#[cw_serde]
pub struct CampaignResponse {
    pub id: u64,
    pub campaign: Campaign,
    pub remaining: Uint128,
}

#[cw_serde]
pub struct ClaimedResponse {
    pub claimed: Uint128,
}

#[cw_serde]
pub struct ClaimEntry {
    pub address: Addr,
    pub claimed: Uint128,
}

#[cw_serde]
pub struct ClaimsResponse {
    pub claims: Vec<ClaimEntry>,
}

#[cw_serde]
pub struct ClaimableResponse {
    pub valid: bool,
    pub payable: Uint128,
}

#[cw_serde]
pub struct FundingRequiredResponse {
    pub delta: Uint128,
    pub fee: Uint128,
    pub required: Uint128,
    pub denom: String,
}

#[cw_serde]
pub struct LiabilitiesResponse {
    pub owed: Uint128,
}

#[cw_serde]
pub struct MigrateMsg {}

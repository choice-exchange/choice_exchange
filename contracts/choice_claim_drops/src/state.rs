use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, HexBinary, Timestamp, Uint128};
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub struct Config {
    pub owner: Addr,
    /// Two-step ownership transfer: a proposed owner who must `AcceptOwnership`
    /// before the transfer takes effect, so a typo can never brick control.
    pub pending_owner: Option<Addr>,
    /// Platform fee in basis points, charged ON TOP of every funding delta and
    /// forwarded to `fee_collector`. The campaign always retains exactly its
    /// declared delta, so the fee never eats into claimants' allocations.
    pub fee_bps: u16,
    pub fee_collector: Addr,
    /// Emergency stop: blocks create / update_root / claim / clawback.
    pub paused: bool,
}

#[cw_serde]
pub struct Campaign {
    pub creator: Addr,
    /// Two-step creator transfer target (rotate a lost/compromised creator key
    /// without stranding the campaign's clawback + admin powers).
    pub pending_creator: Option<Addr>,
    /// Optional second address allowed to publish roots — the rewards keeper
    /// bot for streaming campaigns. Only meaningful when `streaming`.
    pub keeper: Option<Addr>,
    /// Streaming campaigns accept repeated root updates and a keeper; ONLY the
    /// contract owner may create them (the mutable root is a trusted-updater
    /// surface — see README threat model). Non-streaming ("one-shot") campaigns
    /// are auto-frozen on their first root publish, so a permissionless creator
    /// can never republish over already-funded float. This split is the core of
    /// the contract's safety model.
    pub streaming: bool,
    /// Native bank denom paid out by this campaign (tokenfactory / ibc / erc20:*).
    pub denom: String,
    /// Free-form JSON rendered by frontends (title, logo, description).
    pub meta: String,
    /// Where the full `(address, cumulative_amount)` leaves file for `root` is
    /// published. Clients rebuild the tree from it and derive their own proofs;
    /// it doubles as the public audit record of the campaign.
    pub leaves_uri: String,
    pub root: Option<HexBinary>,
    /// Declared sum of all leaf amounts under `root`. The contract cannot see
    /// the leaves, so honesty here is the publisher's responsibility; claims
    /// are hard-capped at `total - claimed_total` regardless, which confines a
    /// dishonest tree to its own campaign's balance.
    pub total: Uint128,
    /// The previous root stays claimable after an update so proofs fetched
    /// just before a keeper publish don't race to failure. Safe under
    /// cumulative semantics: an old leaf only ever claims less.
    pub prev_root: Option<HexBinary>,
    pub prev_total: Uint128,
    pub claimed_total: Uint128,
    /// Count of distinct addresses that have claimed at least once (manage-view
    /// stat; avoids paginating every claimant just to count).
    pub claimants: u64,
    /// One-way switch blocking all future root updates. Auto-set on a one-shot
    /// campaign's first publish; the owner may also set it on a streaming
    /// campaign to end it. Also locks the expiry policy (a frozen perpetual
    /// drop can never be given an expiry, so it can never be clawed back).
    pub frozen: bool,
    /// After expiry claims stop and the creator may claw back the remainder.
    /// `None` = perpetual.
    pub expiry: Option<Timestamp>,
    pub paused: bool,
    pub swept: bool,
}

impl Campaign {
    pub fn remaining(&self) -> Uint128 {
        if self.swept {
            return Uint128::zero();
        }
        self.total
            .checked_sub(self.claimed_total)
            .unwrap_or_default()
    }

    pub fn is_expired(&self, now: Timestamp) -> bool {
        matches!(self.expiry, Some(exp) if now >= exp)
    }
}

pub const CONFIG: Item<Config> = Item::new("config");
pub const CAMPAIGN_SEQ: Item<u64> = Item::new("campaign_seq");
pub const CAMPAIGNS: Map<u64, Campaign> = Map::new("campaigns");
/// (campaign_id, claimant) -> lifetime cumulative amount already paid out.
pub const CLAIMED: Map<(u64, &Addr), Uint128> = Map::new("claimed");
/// (creator, campaign_id) index backing the CampaignsByCreator query.
pub const CREATOR_CAMPAIGNS: Map<(&Addr, u64), ()> = Map::new("creator_campaigns");
/// denom -> total currently owed to claimants (Σ remaining across that denom's
/// campaigns). `Rescue` may only sweep the contract's bank balance above this,
/// so accidentally mis-sent tokens are recoverable without ever touching funds
/// promised to claimants.
pub const LIABILITIES: Map<&str, Uint128> = Map::new("liabilities");

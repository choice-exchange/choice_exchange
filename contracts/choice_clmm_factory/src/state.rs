use cosmwasm_schema::cw_serde;
use cosmwasm_std::Addr;
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub struct FeeTier {
    pub tick_spacing: u32,
    // Default fee rate in pips (e.g., 500 = 0.05%)
    pub default_fee: u32,
}

// Config for the factory itself
#[cw_serde]
pub struct Config {
    /// The factory owner is the SYSTEM ROOT OF TRUST: it gates fee-tier changes,
    /// pool-code repointing, and (because pools read their protocol-fee
    /// controller as the factory owner) every pool's protocol-fee controls. In
    /// production this MUST be the `choice_admin_timelock` (a structural
    /// timelock enforced at the owner-key / deployment level, not by this
    /// contract — see audit C-I2 / C-L3). Transfer is two-step via
    /// `ProposeOwner`/`AcceptOwner` (audit C-L2).
    pub owner: Addr,
    /// Pending owner set by `ProposeOwner`; promoted to `owner` only when this
    /// exact address calls `AcceptOwner`. `None` when no handover is in flight.
    pub pending_owner: Option<Addr>,
    /// The uploaded Code ID of the Choice CLMM Pool. This is the manager's
    /// implicit trust anchor: every `CreatePool` instantiates this code and the
    /// manager trusts the resulting pool's reply attributes. Repointing it is a
    /// two-step `ProposePoolCodeId`/`AcceptPoolCodeId` handover (audit C-L3) and
    /// SHOULD only happen under the `choice_admin_timelock` owner key.
    pub pool_code_id: u64,
    /// Pending pool code id set by `ProposePoolCodeId`; applied by
    /// `AcceptPoolCodeId`. `None` when no repoint is in flight.
    pub pending_pool_code_id: Option<u64>,
}

pub const CONFIG: Item<Config> = Item::new("config");

// Allowed fee tiers (e.g. 100->1, 500->10, 3000->60)
// Key: Fee (pips) -> Value: Tick Spacing
pub const FEE_TIERS: Map<u32, u32> = Map::new("fee_tiers");

// Registry of created pools
// Key: (Token0, Token1, FeeTier) -> Pool Address
pub const POOLS: Map<(&str, &str, u32), Addr> = Map::new("pools");

/// Anti-squat reservation for a single pool slot. While present and unexpired,
/// only `creator` may create the `(token0, token1, fee)` pool. Set by the
/// tokenfactory namespace owner of one side (or the factory owner) via
/// `AuthorizeCreation`; consumed by the matching `CreatePool` or cleared on
/// expiry. See `docs/graduation_antisquat_plan.md`.
#[cw_serde]
pub struct PoolCreationAuth {
    pub creator: Addr,
    /// Unix seconds; `u64::MAX` means no expiry.
    pub expires_at: u64,
}

// Key: (Token0, Token1, FeeTier) -> reservation
pub const POOL_CREATION_AUTH: Map<(&str, &str, u32), PoolCreationAuth> =
    Map::new("pool_creation_auth");

use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::Uint256;

use crate::types::AssetInfo;

#[cw_serde]
pub struct InstantiateMsg {
    pub pool_code_id: u64,
}

#[cw_serde]
pub enum ExecuteMsg {
    /// Creates a new pool using Instantiate2 (Deterministic address)
    CreatePool {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
        init_sqrt_price: Uint256,
    },
    /// Updates mutable factory config. NOTE: `owner` and `pool_code_id` are
    /// deliberately NOT settable here — they are the system's root of trust and
    /// each moves only through an explicit two-step propose/accept handshake
    /// (`ProposeOwner`/`AcceptOwner`, `ProposePoolCodeId`/`AcceptPoolCodeId`).
    /// This struct is kept for forward compatibility / future mutable fields.
    UpdateConfig {},
    /// Step 1 of the two-step owner handover (owner-only). Records
    /// `new_owner` as `pending_owner`; the current owner keeps all powers until
    /// the proposed owner calls `AcceptOwner`. A typo'd-but-valid address is
    /// therefore recoverable (just re-propose) instead of bricking the factory
    /// and every pool's protocol-fee controls. See audit C-L2.
    ProposeOwner { new_owner: String },
    /// Step 2 of the two-step owner handover. Callable ONLY by the address set
    /// as `pending_owner`; promotes it to `owner` and clears `pending_owner`.
    AcceptOwner {},
    /// Step 1 of the two-step `pool_code_id` repoint (owner-only). Records
    /// `code_id` as `pending_pool_code_id`. `pool_code_id` is the manager's
    /// trust anchor (every future `CreatePool` instantiates this code and the
    /// manager trusts its reply attributes), so changing it is gated behind an
    /// explicit accept. See audit C-L3.
    ProposePoolCodeId { code_id: u64 },
    /// Step 2 of the two-step `pool_code_id` repoint (owner-only). Applies the
    /// pending code id and emits old→new for observability.
    AcceptPoolCodeId {},
    /// Enable a new fee tier (e.g. 100 pips, spacing 1)
    EnableFeeAmount { fee: u32, tick_spacing: u32 },
    /// Reserve the canonical pool slot `(token0, token1, fee)` for `creator`.
    /// Until consumed (or expired), only `creator` may `CreatePool` for that
    /// slot; every other slot stays permissionless. Authorizer must own the
    /// tokenfactory namespace of one side (`factory/{sender}/…`) or be the
    /// factory owner. `ttl_seconds == 0` means no expiry. Anti-squat gate for
    /// the graduation on-ramp; see `docs/graduation_antisquat_plan.md`.
    AuthorizeCreation {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
        creator: String,
        ttl_seconds: u64,
    },
    /// Release a reservation early. Same authorizer rule as `AuthorizeCreation`.
    CancelCreationAuth {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
    /// Add `borrower` to the flash-loan allowlist (owner-only). Pools created by
    /// this factory live-query the allowlist when their `Flash {}` is called, so
    /// only allowlisted contracts (e.g. the dex aggregator) may flash-borrow.
    AuthorizeFlashBorrower { borrower: String },
    /// Remove `borrower` from the flash-loan allowlist (owner-only).
    RevokeFlashBorrower { borrower: String },
    /// Escape hatch (owner-only): when `open` is `true`, pools skip the
    /// flash-borrower gate and `Flash {}` is permissionless again.
    SetFlashUnrestricted { open: bool },
}

#[cw_serde]
pub struct PoolInfo {
    pub pool_address: String,
    pub token0: String,
    pub token1: String,
    pub fee: u32,
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(String)]
    GetPool {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
    #[returns(Vec<PoolInfo>)]
    GetAllPools {
        start_after: Option<(String, String, u32)>,
        limit: Option<u32>,
    },
    #[returns(ConfigResponse)]
    GetConfig {},
    #[returns(Vec<FeeTierEntry>)]
    GetFeeTiers {
        start_after: Option<u32>,
        limit: Option<u32>,
    },
    /// Returns the active creation reservation for a slot, if any.
    #[returns(Option<CreationAuthResponse>)]
    GetCreationAuth {
        token_a: AssetInfo,
        token_b: AssetInfo,
        fee: u32,
    },
    /// Whether `borrower` may call a pool's `Flash {}` (explicitly allowlisted, or
    /// flash is unrestricted). Pools query this during flash.
    #[returns(IsFlashBorrowerResponse)]
    IsFlashBorrower { borrower: String },
    /// Lists allowlisted flash borrowers (paginated) plus the unrestricted flag.
    #[returns(FlashBorrowersResponse)]
    FlashBorrowers {
        start_after: Option<String>,
        limit: Option<u32>,
    },
}

#[cw_serde]
pub struct ConfigResponse {
    pub owner: String,
    pub pool_code_id: u64,
}

#[cw_serde]
pub struct FeeTierEntry {
    pub fee: u32,
    pub tick_spacing: u32,
}

#[cw_serde]
pub struct CreationAuthResponse {
    /// The only address allowed to `CreatePool` for this slot until expiry.
    pub creator: String,
    /// Unix seconds; `u64::MAX` means no expiry.
    pub expires_at: u64,
}

#[cw_serde]
pub struct IsFlashBorrowerResponse {
    pub authorized: bool,
}

#[cw_serde]
pub struct FlashBorrowersResponse {
    pub borrowers: Vec<String>,
    pub unrestricted: bool,
}

#[cw_serde]
pub struct MigrateMsg {}

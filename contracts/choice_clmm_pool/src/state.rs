use choice_clmm_common::pool::{FeeConfig, PoolState, ProtocolFeeConfig, TickInfo};
use choice_clmm_common::types::AssetInfo;
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Storage, Uint128, Uint256};
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub struct PoolConfig {
    pub factory: Addr,
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    pub tick_spacing: u32,
    pub fee_config: FeeConfig,
    /// RESERVED hook seam (Uniswap-v4-style extensibility, no engine yet).
    ///
    /// When set, a future pool version will call this contract at the lifecycle
    /// points selected by `hook_permissions`. It is intentionally inert today:
    /// no entrypoint invokes it and no setter exists, so the field is always
    /// `None`. Carrying it now means a hook can later be added without a
    /// `PoolConfig` storage migration. See `HOOK_BEFORE_SWAP` etc. below and the
    /// documented call sites in `actions/swap.rs`.
    pub hook: Option<Addr>,
    /// Bitmask of which lifecycle points `hook` is invoked at. `0` = none.
    /// Reserved; see the `HOOK_*` flag constants.
    pub hook_permissions: u16,
}

// Reserved hook permission bits (for the future hook engine; unused today).
pub const HOOK_BEFORE_SWAP: u16 = 1 << 0;
pub const HOOK_AFTER_SWAP: u16 = 1 << 1;
pub const HOOK_BEFORE_MINT: u16 = 1 << 2;
pub const HOOK_AFTER_MINT: u16 = 1 << 3;
pub const HOOK_BEFORE_BURN: u16 = 1 << 4;
pub const HOOK_AFTER_BURN: u16 = 1 << 5;

#[cw_serde]
#[derive(Default)]
pub struct PositionInfo {
    pub liquidity: u128,
    pub fee_growth_inside_0_last: Uint256,
    pub fee_growth_inside_1_last: Uint256,
    pub tokens_owed_0: Uint128,
    pub tokens_owed_1: Uint128,
}

#[cw_serde]
pub struct OracleData {
    /// Exponential Moving Average of sqrt_price, Q64.96.
    pub price_ema_x96: Uint256,
    /// Wall-clock seconds of the last `update_oracle` call.
    pub last_block_time: u64,
    /// Most recent dynamic fee (ppm), rate-limited across blocks. This is
    /// the authoritative value `get_dynamic_fee` returns — the raw computed
    /// fee is clamped to ±`max_fee_change_ppm * seconds_elapsed` from the
    /// previous value, so a single manipulated-price swap cannot jerk the
    /// fee to `max_fee_ppm` for the next block's victim.
    pub last_fee_ppm: u32,
}

// Define Storage Keys
pub const POOL_CONFIG: Item<PoolConfig> = Item::new("pool_config");
pub const POOL_STATE: Item<PoolState> = Item::new("pool_state");

// Maps Tick Index (i32) -> Info
pub const TICKS: Map<i32, TickInfo> = Map::new("ticks");

// Maps (Owner, LowerTick, UpperTick) -> Info
pub const POSITIONS: Map<(&str, i32, i32), PositionInfo> = Map::new("positions");

pub const TICK_BITMAP: Map<i16, Uint256> = Map::new("tick_bitmap");

pub const FEE_GROWTH_GLOBAL_0: Item<Uint256> = Item::new("fg0");
pub const FEE_GROWTH_GLOBAL_1: Item<Uint256> = Item::new("fg1");

pub const ORACLE: Item<OracleData> = Item::new("oracle");

// Protocol-fee carve rates + routing (treasury / burn auction). Controlled by
// the factory owner. Defaults to off (rates 0) at instantiate.
pub const PROTOCOL_FEE_CONFIG: Item<ProtocolFeeConfig> = Item::new("protocol_fee_config");

// Accrued, not-yet-collected protocol fees. These are protocol-owned and never
// part of pool liquidity, so LPs can never withdraw them.
pub const PROTOCOL_FEES_0: Item<Uint128> = Item::new("protocol_fees_0");
pub const PROTOCOL_FEES_1: Item<Uint128> = Item::new("protocol_fees_1");

// Reentrancy guard. Set true while a flash loan is mid-callback; every
// fund-affecting entrypoint rejects entry while it is held. The flash handler
// sets it and the flash `reply` clears it — no other entrypoint mutates it,
// because no other entrypoint hands control to untrusted code across an open
// invariant (see docs/clmm_extensions_plan.md, Phase 2).
pub const REENTRANCY_LOCK: Item<bool> = Item::new("reentrancy_lock");

// Context carried from the flash execute into its reply, so repayment can be
// verified against the pre-loan balance snapshot.
#[cw_serde]
pub struct PendingFlash {
    pub amount0: Uint128,
    pub amount1: Uint128,
    pub fee0: Uint128,
    pub fee1: Uint128,
    /// Pool balances of token0/token1 captured before the loan was sent out.
    pub snapshot0: Uint128,
    pub snapshot1: Uint128,
}

pub const PENDING_FLASH: Item<PendingFlash> = Item::new("pending_flash");

/// Whether a flash loan is currently in progress (reentrancy lock held).
pub fn is_locked(storage: &dyn Storage) -> bool {
    REENTRANCY_LOCK.may_load(storage).ok().flatten().unwrap_or(false)
}

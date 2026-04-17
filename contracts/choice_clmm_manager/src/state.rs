use choice_clmm_common::manager::Position;
use choice_clmm_common::types::AssetInfo;
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Coin, Uint128, Uint256};
use cw_storage_plus::{Item, Map};

// Global config
#[cw_serde]
pub struct Config {
    pub factory: Addr,
}

pub const CONFIG: Item<Config> = Item::new("config");

// Auto-incrementing ID for positions.
pub const TOKEN_ID_COUNTER: Item<u64> = Item::new("token_id_counter");

/// Per-NFT position state, keyed by `token_id`.
///
/// This is the V3 periphery `positions[tokenId]` pattern: every NFT owns a
/// slice of the manager's single pool-level position at
/// `(manager, tick_lower, tick_upper)`. Per-NFT `liquidity` + `fee_growth_inside_last`
/// let the manager attribute fees pro-rata without fragmenting pool positions.
#[cw_serde]
pub struct PositionState {
    pub pool_address: Addr,
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: Uint128,
    pub fee_growth_inside_0_last_x128: Uint256,
    pub fee_growth_inside_1_last_x128: Uint256,
    pub tokens_owed_0: Uint128,
    pub tokens_owed_1: Uint128,
}

impl PositionState {
    /// Build the (immutable) metadata view for cw721 NFT extension.
    pub fn metadata(&self) -> Position {
        Position {
            token0: self.token0.clone(),
            token1: self.token1.clone(),
            fee: self.fee,
            tick_lower: self.tick_lower,
            tick_upper: self.tick_upper,
            pool_address: self.pool_address.to_string(),
        }
    }
}

pub const POSITIONS: Map<&str, PositionState> = Map::new("positions");

// --- Reply-chain pending-action context ---
//
// CosmWasm 2.x supports `SubMsg::with_payload(Binary)` but plumbing typed
// payloads through the submessage boundary is noisy. We stash the context in
// storage keyed by reply-id instead. Each reply handler reads, processes, and
// removes its entry. Any failure in the submsg reverts the whole tx, so stale
// entries are not a concern.

#[cw_serde]
pub struct PendingMint {
    pub token_id: String,
    pub owner: String,
    pub pool_address: Addr,
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: Uint128,
    // `amount*_min` are enforced against the *actual* consumed amounts the
    // pool reported in event attributes.
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub amount0_sent_to_pool: Uint128,
    pub amount1_sent_to_pool: Uint128,
    // Native excess already returned in the same response? For the
    // current-send-exact-actuals model this is always zero, so we track
    // nothing. Kept here as the field in case future designs reintroduce
    // overfunding.
    pub native_refunds: Vec<Coin>,
}

#[cw_serde]
pub struct PendingIncrease {
    pub token_id: String,
    pub liquidity_added: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
    pub amount0_sent_to_pool: Uint128,
    pub amount1_sent_to_pool: Uint128,
    pub native_refunds: Vec<Coin>,
}

#[cw_serde]
pub struct PendingDecrease {
    pub token_id: String,
    pub liquidity_removed: Uint128,
    pub amount0_min: Uint128,
    pub amount1_min: Uint128,
}

pub const PENDING_MINT: Item<PendingMint> = Item::new("pending_mint");
pub const PENDING_INCREASE: Item<PendingIncrease> = Item::new("pending_increase");
pub const PENDING_DECREASE: Item<PendingDecrease> = Item::new("pending_decrease");

// Reply IDs
pub const REPLY_MINT_POSITION: u64 = 1;
pub const REPLY_INCREASE_LIQUIDITY: u64 = 2;
pub const REPLY_DECREASE_LIQUIDITY: u64 = 3;

/// Returns the V3 Q128 constant (2^128) used for fee-growth scaling.
pub fn q128() -> Uint256 {
    Uint256::one() << 128u32
}

/// Checks that a newly-assigned `token_id` is canonical (strictly increasing)
/// to match the cw721-base id generation convention and prevent duplicates.
pub fn next_token_id(counter: u64) -> (String, u64) {
    (counter.to_string(), counter + 1)
}

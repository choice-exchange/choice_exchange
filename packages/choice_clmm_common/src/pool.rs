use cosmwasm_schema::cw_serde;

// Use U256 for high precision price math (Q64.96)
// We use Uint256 from cosmwasm_std
use cosmwasm_std::{Binary, Uint128, Uint256};
use cw20::Cw20ReceiveMsg;

use crate::types::AssetInfo;

// Slot0 contains the frequently accessed "hot" variables
// to save gas (only 1 read needed for price/tick)
#[cw_serde]
pub struct PoolState {
    pub sqrt_price: Uint256,
    pub tick: i32,
    pub liquidity: Uint128, // Currently active liquidity (L)
}

#[cw_serde]
pub struct FeeConfig {
    pub base_fee_ppm: u32, // e.g., 3000 = 0.3%
    pub max_fee_ppm: u32,  // e.g., 10000 = 1%
    pub volatility_multiplier: u32,
    pub ema_halflife_seconds: u64,
}

#[cw_serde]
pub struct InstantiateMsg {
    pub token0: AssetInfo,
    pub token1: AssetInfo,
    pub tick_spacing: u32,
    pub fee_config: FeeConfig,
    // Initial Price is mandatory in CLMM to determine the starting tick
    pub initial_sqrt_price: Uint256,
}

#[cw_serde]
#[derive(Default)]
pub struct TickInfo {
    pub active_positions_count: u128, // Total liquidity referencing this tick
    pub liquidity_delta: i128,        // Amount of L added/subtracted when crossing
    pub fee_growth_outside_0: Uint256, // Fee accumulator
    pub fee_growth_outside_1: Uint256,
    pub initialized: bool, // Is it in the bitmap?
}

#[cw_serde]
pub enum ExecuteMsg {
    Mint {
        recipient: String,
        lower_tick: i32,
        upper_tick: i32,
        amount: Uint128, // Liquidity (L), not token amount
        data: Option<Binary>,
    },
    Swap {
        recipient: String,
        zero_for_one: bool,
        amount_specified: Uint128,
        sqrt_price_limit_x96: Uint256,
    },
    /// User-friendly swap: send one pool token, receive the other.
    /// Direction is inferred from attached funds (native) or Receive hook (CW20).
    SwapExactInput {
        minimum_amount_out: Uint128,
        recipient: Option<String>,
        deadline: Option<u64>,
    },
    Burn {
        lower_tick: i32,
        upper_tick: i32,
        amount: Uint128, // Liquidity (L) to burn
    },
    Collect {
        recipient: String,
        lower_tick: i32,
        upper_tick: i32,
        amount0_requested: Uint128,
        amount1_requested: Uint128,
    },
    /// CW20 hook entry point for SwapExactInput via CW20 Send
    Receive(Cw20ReceiveMsg),
}

/// CW20 hook messages for the pool contract
#[cw_serde]
pub enum Cw20HookMsg {
    /// Swap via CW20 Send hook
    SwapExactInput {
        minimum_amount_out: Uint128,
        recipient: Option<String>,
        deadline: Option<u64>,
    },
}

#[cw_serde]
pub struct MigrateMsg {}

#[cw_serde]
pub enum QueryMsg {
    GetConfig {},
    GetSlot0 {},
    GetTickInfo {
        tick: i32,
    },
    /// Simulate a swap and return expected output without executing.
    Quote {
        token_in: AssetInfo,
        amount_in: Uint128,
    },
}

#[cw_serde]
pub struct QuoteResponse {
    pub amount_out: Uint128,
    pub amount_in_consumed: Uint128,
    pub fee_amount: Uint128,
}

use cosmwasm_schema::cw_serde;

// Use U256 for high precision price math (Q64.96)
// We use Uint256 from cosmwasm_std
use cosmwasm_std::{Addr, Binary, Uint128, Uint256};
use cw20::Cw20ReceiveMsg;

use crate::types::AssetInfo;

/// Protocol-fee configuration: the carve rate per direction plus where collected
/// protocol fees are routed when swept. Stored as a single item in the pool; all
/// fields are controlled by the factory owner.
///
/// The carve is v3-style: `fee_protocol_{0,1}` is a divisor `n` meaning the
/// protocol takes `fee_amount / n` of the fee charged on swaps in that token's
/// direction, with the remainder going to LPs. `0` disables the carve. Valid
/// nonzero values are `4..=10` (25%..10%), matching Uniswap V3.
#[cw_serde]
pub struct ProtocolFeeConfig {
    /// Divisor for token0-denominated fees (swaps where `zero_for_one`). 0 = off.
    pub fee_protocol_0: u8,
    /// Divisor for token1-denominated fees (swaps where `!zero_for_one`). 0 = off.
    pub fee_protocol_1: u8,
    /// Recipient of the non-burn remainder when protocol fees are collected.
    pub treasury: Addr,
    /// `choice_send_to_auction` contract; collected fees routed here go to the
    /// Injective burn auction. `None` disables the burn split (all to treasury).
    pub burn_auction: Option<Addr>,
    /// Fraction of collected protocol fees (basis points, 0..=10000) routed to
    /// the burn auction. The remainder goes to `treasury`.
    pub burn_share_bps: u16,
}

#[cw_serde]
pub struct ProtocolFeesResponse {
    pub protocol_fees_0: Uint128,
    pub protocol_fees_1: Uint128,
}

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
    /// Maximum fee change per second in ppm. The dynamic fee returned to the
    /// swap loop is clamped to `|new_fee - prev_fee| <= value * seconds_elapsed`
    /// — so a single-block manipulated price cannot jerk the fee to
    /// `max_fee_ppm` for the next block's victim. A value of 0 disables the
    /// cap (legacy behavior).
    ///
    /// Example: 100 ppm/sec = 600 ppm per 6-second block. To reach the 1%
    /// max_fee from 0.3% base takes (10000 - 3000) / 100 = 70 seconds of
    /// sustained volatility. Short enough to react to genuine volatility,
    /// long enough to make single-block griefing unprofitable.
    pub max_fee_change_per_second_ppm: u32,
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
    /// Mint liquidity into a position keyed by `info.sender`.
    ///
    /// The position is always credited to `info.sender`. This matches Uniswap V3
    /// where pool positions are keyed by `msg.sender`; callers that want an
    /// NFT-wrapped position must go through `choice_clmm_manager`.
    ///
    /// Arguments are bounded: `amount` must fit in `i128` (pool enforces this
    /// plus a `MAX_LIQUIDITY_PER_TICK` cap).
    Mint {
        lower_tick: i32,
        upper_tick: i32,
        amount: Uint128,
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
    /// User-friendly exact-output swap: receive exactly `amount_out` of the
    /// output token, paying at most `maximum_amount_in` of the input token.
    /// `zero_for_one = true` pays token0 and receives token1. Native input is
    /// attached as funds (surplus over the actual cost is refunded); CW20 input
    /// is pulled via allowance (exactly the cost). Reverts if the full output
    /// can't be delivered or the cost exceeds `maximum_amount_in`.
    SwapExactOutput {
        zero_for_one: bool,
        amount_out: Uint128,
        maximum_amount_in: Uint128,
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

    /// Set the protocol-fee carve divisors. Factory-owner only. Each value must
    /// be `0` (off) or in `4..=10`. Mirrors Uniswap V3's `setFeeProtocol`.
    SetFeeProtocol {
        fee_protocol_0: u8,
        fee_protocol_1: u8,
    },
    /// Update the protocol-fee routing (treasury / burn auction / burn share).
    /// Factory-owner only. `clear_burn_auction` removes the burn split; any
    /// `Some` field is updated, `None` left unchanged.
    UpdateProtocolFeeConfig {
        treasury: Option<String>,
        burn_auction: Option<String>,
        burn_share_bps: Option<u16>,
        clear_burn_auction: bool,
    },
    /// Sweep accrued protocol fees. Factory-owner only. Splits each token by
    /// `burn_share_bps` to the burn auction and the remainder to `treasury`.
    /// `MAX_UINT128` (or any value `>=` accrued) collects all of that token.
    CollectProtocol {
        amount0_requested: Uint128,
        amount1_requested: Uint128,
    },

    /// Flash loan `amount0` of token0 and/or `amount1` of token1 to `recipient`,
    /// which must implement [`FlashCallbackMsg`] and repay the borrowed amounts
    /// plus the flash fee (at the current dynamic fee rate) before the callback
    /// returns. Repayment is by direct transfer back to the pool (native Send /
    /// CW20 Transfer) and verified by balance delta in the reply. A reentrancy
    /// lock blocks all other pool mutators for the duration of the callback.
    Flash {
        recipient: String,
        amount0: Uint128,
        amount1: Uint128,
        data: Binary,
    },
}

/// Message the pool sends to a flash-loan borrower mid-loan. The borrower's
/// contract must handle this variant, do its work, and ensure the pool is repaid
/// `amount_borrowed + fee{0,1}` of each token before this call returns (repay by
/// transferring the tokens directly back to the pool — native bank Send or CW20
/// `Transfer`, NOT `Send`, which would re-enter the locked pool).
#[cw_serde]
pub enum FlashCallbackMsg {
    FlashCallback {
        fee0: Uint128,
        fee1: Uint128,
        data: Binary,
    },
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
    /// Simulate an exact-output swap: given a desired `amount_out` of
    /// `token_out`, return the input cost (in `amount_in_consumed`) and fee.
    /// `amount_out` in the response is the actually-deliverable output (equals
    /// the request when fully fillable, less if liquidity/price-limit bound).
    QuoteExactOutput {
        token_out: AssetInfo,
        amount_out: Uint128,
    },
    /// Get position info including unclaimed fees.
    GetPosition {
        owner: String,
        tick_lower: i32,
        tick_upper: i32,
    },
    /// List all positions in the pool with pagination.
    GetAllPositions {
        start_after: Option<(String, i32, i32)>,
        limit: Option<u32>,
    },
    /// Get the tick bitmap word at a given word position.
    GetTickBitmap {
        word_position: i16,
    },
    /// Get pool-level totals: active liquidity, fee growth globals, current state.
    GetTotalLiquidity {},
    /// Get the fee growth accumulated INSIDE a tick range, using the V3
    /// "outside model". Used by the NFT manager to attribute fees to
    /// individual NFTs that share one pool-level position.
    ///
    /// Errors if either `tick_lower` or `tick_upper` has not been initialized
    /// (i.e., no position has ever referenced that tick). Callers should
    /// only query this AFTER the pool's Mint has run for the range.
    GetFeeGrowthInside {
        tick_lower: i32,
        tick_upper: i32,
    },
    /// Accrued, not-yet-collected protocol fees for each token.
    GetProtocolFees {},
    /// Current protocol-fee carve rates and routing.
    GetProtocolFeeConfig {},
}

#[cw_serde]
pub struct PositionInfoResponse {
    pub liquidity: Uint128,
    pub tokens_owed_0: Uint128,
    pub tokens_owed_1: Uint128,
}

#[cw_serde]
pub struct AllPositionsEntry {
    pub owner: String,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: Uint128,
    pub tokens_owed_0: Uint128,
    pub tokens_owed_1: Uint128,
}

#[cw_serde]
pub struct TotalLiquidityResponse {
    pub sqrt_price: Uint256,
    pub tick: i32,
    pub liquidity: Uint128,
    pub fee_growth_global_0: Uint256,
    pub fee_growth_global_1: Uint256,
}

#[cw_serde]
pub struct QuoteResponse {
    pub amount_out: Uint128,
    pub amount_in_consumed: Uint128,
    pub fee_amount: Uint128,
}

#[cw_serde]
pub struct FeeGrowthInsideResponse {
    pub fee_growth_inside_0_x128: Uint256,
    pub fee_growth_inside_1_x128: Uint256,
}

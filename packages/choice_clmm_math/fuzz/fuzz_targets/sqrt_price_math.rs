#![no_main]
//! Fuzz `sqrt_price_math` — next-price and token-delta math. These functions
//! decide how far a swap moves the price and how many tokens change hands;
//! the rounding direction is *load-bearing for fund safety*.
//!
//! Invariants:
//!   MONOTONIC   adding token0 (zero_for_one) never raises price; token1 never lowers it.
//!   POSITIVE    the next sqrt price is never zero (would brick the pool).
//!   NO-OVER-IN  the token0/1 implied by the new price is <= the input supplied
//!               (pool consumes at most what the trader provided).
//!   ROUNDING    get_amount*_delta(round_up) >= get_amount*_delta(round_down)
//!               (the "you owe" leg always rounds toward the pool).
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::Uint256;
use choice_clmm_math::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
};
use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

#[derive(Arbitrary, Debug)]
struct In {
    price_a_hi: u128,
    price_a_lo: u128,
    price_b_hi: u128,
    price_b_lo: u128,
    liquidity: u128,
    amount: u128,
    zero_for_one: bool,
}

fn u256(hi: u128, lo: u128) -> Uint256 {
    (Uint256::from(hi) << 128) + Uint256::from(lo)
}

/// Clamp an arbitrary value into the valid sqrt-price band [MIN_SQRT_RATIO, max).
fn clamp_price(raw: Uint256) -> Uint256 {
    let min = Uint256::from(MIN_SQRT_RATIO);
    let max = max_sqrt_ratio() - Uint256::one();
    let span = max - min;
    min + (raw % (span + Uint256::one()))
}

fuzz_target!(|input: In| {
    let price_a = clamp_price(u256(input.price_a_hi, input.price_a_lo));
    let price_b = clamp_price(u256(input.price_b_hi, input.price_b_lo));
    let liquidity = input.liquidity | 1; // force > 0
    let amount = Uint256::from(input.amount);

    // ---- next sqrt price from input ----
    if let Ok(next) = get_next_sqrt_price_from_input(price_a, liquidity, amount, input.zero_for_one)
    {
        assert!(!next.is_zero(), "POSITIVE: next price never zero");
        if input.zero_for_one {
            assert!(next <= price_a, "MONOTONIC: token0 in never raises price");
            // NO-OVER-IN: token0 the pool would charge for this move <= supplied amount.
            if let Ok(implied) = get_amount0_delta(next, price_a, liquidity, true) {
                assert!(implied <= amount, "NO-OVER-IN (token0)");
            }
        } else {
            assert!(next >= price_a, "MONOTONIC: token1 in never lowers price");
            if let Ok(implied) = get_amount1_delta(price_a, next, liquidity, true) {
                assert!(implied <= amount, "NO-OVER-IN (token1)");
            }
        }
    }

    // ---- delta rounding direction ----
    let (lo, hi) = if price_a < price_b {
        (price_a, price_b)
    } else {
        (price_b, price_a)
    };
    if lo != hi {
        if let (Ok(up), Ok(down)) = (
            get_amount0_delta(lo, hi, liquidity, true),
            get_amount0_delta(lo, hi, liquidity, false),
        ) {
            assert!(up >= down, "ROUNDING amount0: up >= down");
        }
        if let (Ok(up), Ok(down)) = (
            get_amount1_delta(lo, hi, liquidity, true),
            get_amount1_delta(lo, hi, liquidity, false),
        ) {
            assert!(up >= down, "ROUNDING amount1: up >= down");
            // amount1 is a single mul_div, so the gap is at most one ulp.
            assert!(up - down <= Uint256::one(), "ROUNDING amount1: gap <= 1");
        }
    }
});

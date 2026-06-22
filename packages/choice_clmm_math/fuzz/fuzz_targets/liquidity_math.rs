#![no_main]
//! Fuzz `liquidity_math` — converts deposited token amounts into position
//! liquidity L. This file is the #1 coverage gap (0% native), and it sits on
//! the mint path: if L is credited too generously, an LP can burn out more than
//! they put in => pool insolvency.
//!
//! Core invariant — SOLVENCY (mint-then-burn can't profit):
//!   Credit L from (amount0, amount1), then value L back out at the SAME range
//!   using the burn rounding (round DOWN, favouring the pool). The recovered
//!   amounts must never exceed what was deposited:
//!       burn0 = get_amount0_delta(.., L, round_up=false) <= amount0
//!       burn1 = get_amount1_delta(.., L, round_up=false) <= amount1
//!   evaluated over the sub-range each token actually backs.
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::{Uint128, Uint256};
use choice_clmm_math::liquidity_math::{
    get_liquidity_for_amount0, get_liquidity_for_amount1, get_liquidity_for_amounts,
};
use choice_clmm_math::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

#[derive(Arbitrary, Debug)]
struct In {
    lower_hi: u128,
    lower_lo: u128,
    upper_hi: u128,
    upper_lo: u128,
    cur_hi: u128,
    cur_lo: u128,
    amount0: u128,
    amount1: u128,
}

fn u256(hi: u128, lo: u128) -> Uint256 {
    (Uint256::from(hi) << 128) + Uint256::from(lo)
}

fn clamp_price(raw: Uint256) -> Uint256 {
    let min = Uint256::from(MIN_SQRT_RATIO);
    let max = max_sqrt_ratio() - Uint256::one();
    let span = max - min;
    min + (raw % (span + Uint256::one()))
}

fuzz_target!(|input: In| {
    let a = clamp_price(u256(input.lower_hi, input.lower_lo));
    let b = clamp_price(u256(input.upper_hi, input.upper_lo));
    let (lower, upper) = if a < b { (a, b) } else { (b, a) };
    if lower == upper {
        return; // equal-bound is a documented error path, exercised elsewhere
    }
    let current = clamp_price(u256(input.cur_hi, input.cur_lo));
    let amount0 = Uint128::from(input.amount0);
    let amount1 = Uint128::from(input.amount1);

    // ---- direct single-sided helpers: burn-down must not exceed deposit ----
    if let Ok(l0) = get_liquidity_for_amount0(lower, upper, Uint256::from(amount0)) {
        if let Ok(back) = get_amount0_delta(lower, upper, l0.u128(), false) {
            assert!(back <= Uint256::from(amount0), "SOLVENCY amount0 single-sided");
        }
    }
    if let Ok(l1) = get_liquidity_for_amount1(lower, upper, Uint256::from(amount1)) {
        if let Ok(back) = get_amount1_delta(lower, upper, l1.u128(), false) {
            assert!(back <= Uint256::from(amount1), "SOLVENCY amount1 single-sided");
        }
    }

    // ---- combined entry point ----
    let l = match get_liquidity_for_amounts(current, lower, upper, amount0, amount1) {
        Ok(l) => l.u128(),
        Err(_) => return,
    };
    if l == 0 {
        return;
    }

    if current <= lower {
        // Range entirely above price: backed by token0 only.
        if let Ok(back0) = get_amount0_delta(lower, upper, l, false) {
            assert!(back0 <= Uint256::from(amount0), "SOLVENCY combined token0-only");
        }
    } else if current >= upper {
        // Range entirely below price: backed by token1 only.
        if let Ok(back1) = get_amount1_delta(lower, upper, l, false) {
            assert!(back1 <= Uint256::from(amount1), "SOLVENCY combined token1-only");
        }
    } else {
        // In range: token0 backs [current, upper], token1 backs [lower, current].
        if let Ok(back0) = get_amount0_delta(current, upper, l, false) {
            assert!(back0 <= Uint256::from(amount0), "SOLVENCY combined token0 leg");
        }
        if let Ok(back1) = get_amount1_delta(lower, current, l, false) {
            assert!(back1 <= Uint256::from(amount1), "SOLVENCY combined token1 leg");
        }
    }
});

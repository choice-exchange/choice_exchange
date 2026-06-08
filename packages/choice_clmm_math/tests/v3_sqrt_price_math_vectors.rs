//! Uniswap V3 v1.0.1 sqrt_price_math parity vectors.
//!
//! Selected cases from `v3-core/test/SqrtPriceMath.spec.ts` adapted to the
//! Rust API. The most important invariant these tests guard is the
//! *rounding direction* — off-by-one in the wrong direction leaks funds on
//! every swap.

use choice_clmm_math::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};
use cosmwasm_std::Uint256;

fn q96() -> Uint256 {
    Uint256::one() << 96
}

fn encode(n: u128) -> Uint256 {
    // Like V3's `encodePriceSqrt(n, 1)`: sqrt(n) * 2^96. We don't need
    // precise sqrt here — tests only require *some* price * Q96.
    q96() * Uint256::from(n)
}

// --- get_next_sqrt_price_from_input ---

#[test]
fn v3_next_sqrt_price_from_input_zero_amount_is_identity() {
    let p = encode(1);
    let l: u128 = 1u128 << 50;
    assert_eq!(
        get_next_sqrt_price_from_input(p, l, Uint256::zero(), true).unwrap(),
        p
    );
    assert_eq!(
        get_next_sqrt_price_from_input(p, l, Uint256::zero(), false).unwrap(),
        p
    );
}

#[test]
fn v3_next_sqrt_price_from_input_rejects_zero_price() {
    let l: u128 = 1u128 << 50;
    assert!(get_next_sqrt_price_from_input(Uint256::zero(), l, Uint256::one(), true).is_err());
    assert!(get_next_sqrt_price_from_input(Uint256::zero(), l, Uint256::one(), false).is_err());
}

#[test]
fn v3_next_sqrt_price_from_input_rejects_zero_liquidity() {
    let p = encode(1);
    assert!(get_next_sqrt_price_from_input(p, 0, Uint256::from(1u128), true).is_err());
    assert!(get_next_sqrt_price_from_input(p, 0, Uint256::from(1u128), false).is_err());
}

#[test]
fn v3_next_sqrt_price_zero_for_one_rounds_price_up() {
    // Adding token0 (zero_for_one) pushes price DOWN, and V3 rounds the new
    // sqrt_price UP (toward the original). This is the fund-safety
    // direction: recomputing amount0 at new_price yields <= amount_in.
    let p = q96();
    let l: u128 = 1_000_000_000_000u128;
    let amount_in = Uint256::from(500_000u128);
    let new_price = get_next_sqrt_price_from_input(p, l, amount_in, true).unwrap();

    // Direction: price decreases.
    assert!(new_price < p, "price must drop when token0 is added");

    // Invariant: implied-amount0 <= amount_in (pool never pays out more than
    // received). Rounding UP on new_price ensures this.
    let implied_in = get_amount0_delta(new_price, p, l, true).unwrap();
    assert!(
        implied_in <= amount_in,
        "rounding direction wrong: implied_in {} > amount_in {}",
        implied_in,
        amount_in
    );
}

#[test]
fn v3_next_sqrt_price_one_for_zero_rounds_price_down() {
    // Adding token1 (!zero_for_one) pushes price UP, and V3 rounds the new
    // sqrt_price DOWN (toward the original). Same fund-safety invariant.
    let p = q96();
    let l: u128 = 1_000_000_000_000u128;
    let amount_in = Uint256::from(500_000u128);
    let new_price = get_next_sqrt_price_from_input(p, l, amount_in, false).unwrap();

    assert!(new_price > p, "price must rise when token1 is added");

    let implied_in = get_amount1_delta(p, new_price, l, true).unwrap();
    assert!(
        implied_in <= amount_in,
        "rounding direction wrong: implied_in {} > amount_in {}",
        implied_in,
        amount_in
    );
}

// --- get_next_sqrt_price_from_output ---

#[test]
fn v3_next_sqrt_price_from_output_zero_amount_is_identity() {
    let p = encode(1);
    let l: u128 = 1u128 << 50;
    assert_eq!(
        get_next_sqrt_price_from_output(p, l, Uint256::zero(), true).unwrap(),
        p
    );
    assert_eq!(
        get_next_sqrt_price_from_output(p, l, Uint256::zero(), false).unwrap(),
        p
    );
}

#[test]
fn v3_next_sqrt_price_from_output_too_large_errors() {
    // Pulling out more token than liquidity supports must revert, not panic.
    let p = q96();
    let l: u128 = 1;
    let huge = Uint256::from(10u128).pow(30);
    assert!(get_next_sqrt_price_from_output(p, l, huge, true).is_err());
}

// --- get_amount0_delta / get_amount1_delta ---

#[test]
fn v3_amount0_delta_returns_zero_for_zero_liquidity() {
    let a = q96();
    let b = q96() * Uint256::from(2u128);
    assert_eq!(get_amount0_delta(a, b, 0, true).unwrap(), Uint256::zero());
    assert_eq!(get_amount0_delta(a, b, 0, false).unwrap(), Uint256::zero());
}

#[test]
fn v3_amount0_delta_returns_zero_for_equal_prices() {
    let a = q96();
    assert_eq!(
        get_amount0_delta(a, a, 1_000_000_000, true).unwrap(),
        Uint256::zero()
    );
}

#[test]
fn v3_amount0_delta_round_up_gte_round_down() {
    // Same inputs, up >= down, never more than 1 unit apart at typical scales.
    let a = q96();
    let b = q96() * Uint256::from(3u128) / Uint256::from(2u128);
    let l: u128 = 1_000_000u128;
    let up = get_amount0_delta(a, b, l, true).unwrap();
    let down = get_amount0_delta(a, b, l, false).unwrap();
    assert!(up >= down);
    assert!(up - down <= Uint256::one());
}

#[test]
fn v3_amount1_delta_round_up_gte_round_down() {
    let a = q96();
    let b = q96() * Uint256::from(3u128) / Uint256::from(2u128);
    let l: u128 = 1_000_000u128;
    let up = get_amount1_delta(a, b, l, true).unwrap();
    let down = get_amount1_delta(a, b, l, false).unwrap();
    assert!(up >= down);
    assert!(up - down <= Uint256::one());
}

#[test]
fn v3_amount0_delta_order_invariant() {
    // V3: get_amount0_delta(a, b, L, r) == get_amount0_delta(b, a, L, r).
    let a = q96();
    let b = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(18);
    assert_eq!(
        get_amount0_delta(a, b, l, true).unwrap(),
        get_amount0_delta(b, a, l, true).unwrap()
    );
    assert_eq!(
        get_amount0_delta(a, b, l, false).unwrap(),
        get_amount0_delta(b, a, l, false).unwrap()
    );
}

#[test]
fn v3_amount1_delta_order_invariant() {
    let a = q96();
    let b = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(18);
    assert_eq!(
        get_amount1_delta(a, b, l, true).unwrap(),
        get_amount1_delta(b, a, l, true).unwrap()
    );
    assert_eq!(
        get_amount1_delta(a, b, l, false).unwrap(),
        get_amount1_delta(b, a, l, false).unwrap()
    );
}

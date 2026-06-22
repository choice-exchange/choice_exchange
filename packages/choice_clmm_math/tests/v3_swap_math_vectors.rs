//! Uniswap V3 v1.0.1 swap_math parity vectors.
//!
//! Selected cases from `v3-core/test/SwapMath.spec.ts`. These cover the
//! fee-math and partial-step invariants that dominate swap safety.

use std::str::FromStr;

use choice_clmm_math::swap_math::{compute_swap_step, FEE_DENOMINATOR};
use cosmwasm_std::Uint256;

fn q96() -> Uint256 {
    Uint256::one() << 96
}

fn u(s: &str) -> Uint256 {
    Uint256::from_str(s).unwrap()
}

/// Asserts a full `compute_swap_step` result against independently-computed
/// reference values. The reference numbers were produced by a separate Python
/// implementation of Uniswap V3's `computeSwapStep` (exact integer arithmetic),
/// itself validated against the two published `getNextSqrtPriceFromInput`
/// vectors (87150978765690771352898345369 and 72025602285694852357767227579).
/// Pinning all four outputs — not just directional/structural properties —
/// catches a magnitude divergence from V3 that preserves rounding direction.
#[allow(clippy::too_many_arguments)]
fn assert_step(
    current: Uint256,
    target: Uint256,
    l: u128,
    remaining: Uint256,
    fee: u32,
    zero_for_one: bool,
    exp_next: Uint256,
    exp_in: Uint256,
    exp_out: Uint256,
    exp_fee: Uint256,
) {
    let r = compute_swap_step(current, target, l, remaining, fee, zero_for_one).unwrap();
    assert_eq!(r.sqrt_ratio_next_x96, exp_next, "sqrt_ratio_next mismatch");
    assert_eq!(r.amount_in, exp_in, "amount_in mismatch");
    assert_eq!(r.amount_out, exp_out, "amount_out mismatch");
    assert_eq!(r.fee_amount, exp_fee, "fee_amount mismatch");
}

#[test]
fn v3_parity_full_step_one_for_zero_fee_3000() {
    // current=1.0, target=1.01, L=2e18, huge input, fee=3000ppm, !zero_for_one.
    assert_step(
        q96(),
        q96() * Uint256::from(101u128) / Uint256::from(100u128),
        2u128 * 10u128.pow(18),
        Uint256::from(10u128).pow(30),
        3000,
        false,
        u("80020444139406980969479389839"),
        Uint256::from(20_000_000_000_000_000u128),
        Uint256::from(19_801_980_198_019_801u128),
        Uint256::from(60_180_541_624_875u128),
    );
}

#[test]
fn v3_parity_partial_step_one_for_zero_fee_3000() {
    // Partial step: amount_in and amount_out are pinned independently, so the
    // `amount_in + fee == remaining` relation is now backed by a real amount_in
    // check rather than being satisfied tautologically by `fee = remaining - in`.
    assert_step(
        q96(),
        q96() * Uint256::from(2u128),
        10u128.pow(18),
        Uint256::from(1_000u128),
        3000,
        false,
        u("79228162514264416584021977057"),
        Uint256::from(997u128),
        Uint256::from(996u128),
        Uint256::from(3u128),
    );
}

#[test]
fn v3_parity_full_step_zero_for_one_fee_500() {
    // current=1.0, target=0.99, L=2e18, huge input, fee=500ppm, zero_for_one.
    assert_step(
        q96(),
        q96() * Uint256::from(99u128) / Uint256::from(100u128),
        2u128 * 10u128.pow(18),
        Uint256::from(10u128).pow(30),
        500,
        true,
        u("78435880889121694217608510832"),
        Uint256::from(20_202_020_202_020_203u128),
        Uint256::from(20_000_000_000_000_000u128),
        Uint256::from(10_106_063_132_577u128),
    );
}

#[test]
fn v3_parity_partial_step_zero_for_one_fee_10000() {
    // current=1.0, target=0.25, L=1e18, remaining=5e15, fee=10000ppm, zero_for_one.
    assert_step(
        q96(),
        q96() / Uint256::from(2u128),
        10u128.pow(18),
        Uint256::from(5u128) * Uint256::from(10u128).pow(15),
        10000,
        true,
        u("78837914835826993973375740421"),
        Uint256::from(4_950_000_000_000_000u128),
        Uint256::from(4_925_618_189_959_699u128),
        Uint256::from(50_000_000_000_000u128),
    );
}

#[test]
fn v3_full_step_when_input_exceeds_cost_to_target() {
    // Target reached: amount_in equals the exact delta between prices, plus
    // a fee computed from `amount_in / (1 - fee_rate)`. Excess is NOT charged.
    let current = q96();
    let target = q96() * Uint256::from(101u128) / Uint256::from(100u128); // +1%
    let l: u128 = 2u128 * 10u128.pow(18);
    let remaining = Uint256::from(10u128).pow(30);
    let r = compute_swap_step(current, target, l, remaining, 600, false).unwrap();

    assert_eq!(r.sqrt_ratio_next_x96, target, "must reach target");
    assert!(r.amount_in > Uint256::zero());
    assert!(r.amount_out > Uint256::zero());
    assert!(r.fee_amount > Uint256::zero());
    // Total consumed ≤ remaining.
    assert!(r.amount_in + r.fee_amount <= remaining);
}

#[test]
fn v3_partial_step_consumes_all_remaining() {
    // Target NOT reached: amount_in + fee_amount MUST == amount_remaining
    // exactly. This invariant protects users from being under-charged or
    // over-charged when price doesn't reach the next tick.
    let current = q96();
    let target = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(18);
    let remaining = Uint256::from(1_000u128); // tiny
    let r = compute_swap_step(current, target, l, remaining, 3000, false).unwrap();

    assert_ne!(r.sqrt_ratio_next_x96, target, "must not reach target");
    // NOTE: `amount_in + fee == remaining` alone is tautological here — the
    // implementation defines `fee = remaining - amount_in`, so the sum always
    // equals `remaining` regardless of whether amount_in is computed correctly.
    // Pin amount_in (and amount_out) to the independent V3 reference so this
    // test actually constrains the price math, not just the fee bookkeeping.
    assert_eq!(r.amount_in, Uint256::from(997u128), "amount_in mismatch");
    assert_eq!(r.amount_out, Uint256::from(996u128), "amount_out mismatch");
    assert_eq!(
        r.amount_in + r.fee_amount,
        remaining,
        "amount_in + fee must consume remaining exactly"
    );
}

#[test]
fn v3_partial_step_zero_fee_pips_still_consumes_all() {
    // Zero fee edge case: fee_amount == 0, all remaining is amount_in.
    let current = q96();
    let target = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(18);
    let remaining = Uint256::from(1_000u128);
    let r = compute_swap_step(current, target, l, remaining, 0, false).unwrap();

    assert_eq!(r.fee_amount, Uint256::zero());
    assert_eq!(r.amount_in, remaining);
}

#[test]
fn v3_zero_liquidity_crosses_gap_for_free() {
    // Tick gap traversal: zero L + non-zero remaining produces zero in/out/fee
    // and advances price to the target, matching V3's `computeSwapStep`.
    let current = q96();
    let target = q96() / Uint256::from(2u128);
    let r = compute_swap_step(current, target, 0, Uint256::from(100u128), 3000, true).unwrap();
    assert_eq!(r.sqrt_ratio_next_x96, target);
    assert_eq!(r.amount_in, Uint256::zero());
    assert_eq!(r.amount_out, Uint256::zero());
    assert_eq!(r.fee_amount, Uint256::zero());
}

#[test]
fn v3_rejects_fee_at_or_above_denominator() {
    let current = q96();
    let target = q96() / Uint256::from(2u128);
    let l: u128 = 1_000_000;
    let remaining = Uint256::from(100u128);
    assert!(compute_swap_step(current, target, l, remaining, FEE_DENOMINATOR, true).is_err());
    assert!(compute_swap_step(current, target, l, remaining, FEE_DENOMINATOR + 1, true).is_err());
}

#[test]
fn v3_rejects_zero_sqrt_price() {
    let target = q96();
    let l: u128 = 1_000_000;
    let remaining = Uint256::from(100u128);
    assert!(compute_swap_step(Uint256::zero(), target, l, remaining, 3000, false).is_err());
    assert!(compute_swap_step(q96(), Uint256::zero(), l, remaining, 3000, false).is_err());
}

#[test]
fn v3_exact_input_amount_out_monotonic_in_remaining() {
    // Bigger input never produces smaller output (for the same price/L/fee).
    let current = q96();
    let target = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(15);
    let small = compute_swap_step(current, target, l, Uint256::from(100u128), 3000, false)
        .unwrap()
        .amount_out;
    let large = compute_swap_step(current, target, l, Uint256::from(10_000u128), 3000, false)
        .unwrap()
        .amount_out;
    assert!(large >= small);
}

#[test]
fn v3_fee_is_nonzero_whenever_amount_in_is_nonzero() {
    // Regression for the "clamp hides fee error" bug: a non-zero amount_in
    // must produce non-zero fee_amount for any fee > 0.
    let current = q96();
    let target = q96() * Uint256::from(2u128);
    let l: u128 = 10u128.pow(18);
    let r = compute_swap_step(current, target, l, Uint256::from(10_000u128), 3000, false).unwrap();
    if !r.amount_in.is_zero() {
        assert!(!r.fee_amount.is_zero(), "non-zero amount_in with zero fee");
    }
}

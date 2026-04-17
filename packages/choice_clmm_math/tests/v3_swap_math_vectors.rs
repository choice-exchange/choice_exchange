//! Uniswap V3 v1.0.1 swap_math parity vectors.
//!
//! Selected cases from `v3-core/test/SwapMath.spec.ts`. These cover the
//! fee-math and partial-step invariants that dominate swap safety.

use choice_clmm_math::swap_math::{compute_swap_step, FEE_DENOMINATOR};
use cosmwasm_std::Uint256;

fn q96() -> Uint256 {
    Uint256::one() << 96
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
    assert!(
        compute_swap_step(current, target, l, remaining, FEE_DENOMINATOR + 1, true).is_err()
    );
}

#[test]
fn v3_rejects_zero_sqrt_price() {
    let target = q96();
    let l: u128 = 1_000_000;
    let remaining = Uint256::from(100u128);
    assert!(
        compute_swap_step(Uint256::zero(), target, l, remaining, 3000, false).is_err()
    );
    assert!(
        compute_swap_step(q96(), Uint256::zero(), l, remaining, 3000, false).is_err()
    );
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

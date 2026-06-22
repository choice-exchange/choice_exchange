#![no_main]
//! Fuzz `swap_math::compute_swap_step` (+ exact-out) — the per-tick swap step
//! that the whole swap loop is built from. This is where over-charging,
//! price-limit overshoot, and fee mistakes would live.
//!
//! Target geometry is set on the correct side of `current` for the given
//! direction (as the real swap loop guarantees), so the price-limit invariant
//! is meaningful.
//!
//! Invariants — exact-in (compute_swap_step):
//!   OVER-CHARGE   amount_in + fee_amount <= amount_remaining   (never take more than offered)
//!   PRICE-LIMIT   next sqrt price never moves past `target`     (and never past `current`)
//! Invariants — exact-out (compute_swap_step_exact_out):
//!   DELIVER-CAP   amount_out <= amount_out_remaining            (never over-deliver)
//!   PRICE-LIMIT   next sqrt price stays within [current, target]
//!   PAY-SOMETHING amount_out > 0  =>  amount_in > 0
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::Uint256;
use choice_clmm_math::swap_math::{compute_swap_step, compute_swap_step_exact_out, FEE_DENOMINATOR};
use choice_clmm_math::tick_math::{max_sqrt_ratio, MIN_SQRT_RATIO};

#[derive(Arbitrary, Debug)]
struct In {
    cur_hi: u128,
    cur_lo: u128,
    tgt_hi: u128,
    tgt_lo: u128,
    liquidity: u128,
    amount: u128,
    fee: u32,
    zero_for_one: bool,
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

/// Map `raw` into the open interval (`min`, `current`) for a downward move,
/// or (`current`, `max`) for an upward move, so target is strictly on the
/// correct side of current for `zero_for_one`.
fn target_on_side(current: Uint256, raw: Uint256, zero_for_one: bool) -> Option<Uint256> {
    let min = Uint256::from(MIN_SQRT_RATIO);
    let max = max_sqrt_ratio() - Uint256::one();
    if zero_for_one {
        // price decreases: target in [min, current)
        if current <= min {
            return None;
        }
        let span = current - min; // > 0
        Some(min + (raw % span))
    } else {
        // price increases: target in (current, max]
        if current >= max {
            return None;
        }
        let span = max - current; // > 0
        Some(current + Uint256::one() + (raw % span))
    }
}

fuzz_target!(|input: In| {
    let current = clamp_price(u256(input.cur_hi, input.cur_lo));
    let fee = input.fee % FEE_DENOMINATOR; // < 1_000_000
    let amount = Uint256::from(input.amount);
    let raw_tgt = u256(input.tgt_hi, input.tgt_lo);

    let target = match target_on_side(current, raw_tgt, input.zero_for_one) {
        Some(t) => t,
        None => return,
    };

    let (lo, hi) = if input.zero_for_one {
        (target, current)
    } else {
        (current, target)
    };

    // ---- exact-in: liquidity may be 0 (gap traversal is legal) ----
    if let Ok(r) = compute_swap_step(current, target, input.liquidity, amount, fee, input.zero_for_one)
    {
        let cost = r.amount_in.checked_add(r.fee_amount).expect("cost fits");
        assert!(cost <= amount, "OVER-CHARGE: amount_in + fee <= remaining");
        assert!(r.sqrt_ratio_next_x96 >= lo, "PRICE-LIMIT lo (exact-in)");
        assert!(r.sqrt_ratio_next_x96 <= hi, "PRICE-LIMIT hi (exact-in)");
    }

    // ---- exact-out: requires liquidity > 0 ----
    let liquidity = input.liquidity | 1;
    if let Ok(r) =
        compute_swap_step_exact_out(current, target, liquidity, amount, fee, input.zero_for_one)
    {
        assert!(r.amount_out <= amount, "DELIVER-CAP: out <= requested");
        assert!(r.sqrt_ratio_next_x96 >= lo, "PRICE-LIMIT lo (exact-out)");
        assert!(r.sqrt_ratio_next_x96 <= hi, "PRICE-LIMIT hi (exact-out)");
        if !r.amount_out.is_zero() {
            assert!(!r.amount_in.is_zero(), "PAY-SOMETHING: out>0 => in>0");
        }
    }
});

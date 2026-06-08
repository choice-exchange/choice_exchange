#![no_main]
//! Fuzz `full_math::mul_div` / `mul_div_round_up` — the 512-bit-intermediate
//! floor/ceil division that every amount/fee/liquidity calculation funnels
//! through. A rounding-direction bug here silently mis-prices the whole AMM.
//!
//! Invariants (all must hold for any a, b and non-zero d):
//!   I1  floor = mul_div(a,b,d) satisfies   floor * d <= a*b          (never over-credits)
//!   I2  floor is maximal:                  (floor+1) * d > a*b
//!   I3  ceil  = mul_div_round_up(a,b,d):   ceil >= floor  and  ceil-floor <= 1
//!   I4  ceil  is the true ceiling:         ceil * d >= a*b
//!   I5  d == 0 must Err, never panic.
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::Uint256;
use choice_clmm_math::full_math::{mul_div, mul_div_round_up};

#[derive(Arbitrary, Debug)]
struct In {
    a: u128,
    b: u128,
    d: u128,
}

fuzz_target!(|input: In| {
    let a = Uint256::from(input.a);
    let b = Uint256::from(input.b);
    let d = Uint256::from(input.d);

    if input.d == 0 {
        // I5: divide-by-zero is an error, not a panic.
        assert!(mul_div(a, b, d).is_err());
        assert!(mul_div_round_up(a, b, d).is_err());
        return;
    }

    // a, b are u128 so a*b < 2^256 and fits Uint256 exactly — a trustworthy oracle.
    let ab = a.checked_mul(b).expect("u128*u128 fits Uint256");

    let floor = mul_div(a, b, d).expect("floor div ok for nonzero d");
    let ceil = mul_div_round_up(a, b, d).expect("ceil div ok for nonzero d");

    // I1: never round up the floor result (protocol must not over-credit).
    assert!(floor.checked_mul(d).unwrap() <= ab, "I1 floor*d <= a*b");

    // I2: floor is the *largest* q with q*d <= a*b.
    let floor_plus = floor.checked_add(Uint256::one()).unwrap();
    assert!(floor_plus.checked_mul(d).unwrap() > ab, "I2 (floor+1)*d > a*b");

    // I3: ceil sits one ulp above floor at most.
    assert!(ceil >= floor, "I3a ceil >= floor");
    assert!(ceil - floor <= Uint256::one(), "I3b ceil-floor <= 1");

    // I4: ceil*d covers a*b.
    assert!(ceil.checked_mul(d).unwrap() >= ab, "I4 ceil*d >= a*b");

    // Exactness: when d divides a*b evenly, floor == ceil.
    let remainder = ab - floor.checked_mul(d).unwrap();
    if remainder.is_zero() {
        assert_eq!(floor, ceil, "exact division => floor == ceil");
    } else {
        assert_eq!(ceil, floor_plus, "inexact division => ceil == floor+1");
    }
});

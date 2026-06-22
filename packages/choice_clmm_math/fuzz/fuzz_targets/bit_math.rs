#![no_main]
//! Fuzz `bit_math::most_significant_bit` / `least_significant_bit` — the binary
//! log used by `get_tick_at_sqrt_ratio` (MSB) and the tick-bitmap walk (LSB).
//! A wrong bit index here skips initialized ticks => liquidity silently not
//! crossed (the class of bug previously fixed at a bitmap word boundary).
//!
//! Invariants (for any non-zero n):
//!   MSB m:  (1 << m) <= n  AND  (m == 255 OR n < (1 << (m+1)))   — highest set bit
//!   LSB l:  bit l of n is set  AND  no lower bit is set           — lowest set bit
//!   n == 0 must Err for both, never panic.
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::Uint256;
use choice_clmm_math::bit_math::{least_significant_bit, most_significant_bit};

#[derive(Arbitrary, Debug)]
struct In {
    hi: u128,
    lo: u128,
}

fn u256(hi: u128, lo: u128) -> Uint256 {
    (Uint256::from(hi) << 128) + Uint256::from(lo)
}

fuzz_target!(|input: In| {
    let n = u256(input.hi, input.lo);
    let one = Uint256::one();

    if n.is_zero() {
        assert!(most_significant_bit(n).is_err());
        assert!(least_significant_bit(n).is_err());
        return;
    }

    // ---- MSB ----
    let m = most_significant_bit(n).expect("msb of nonzero");
    assert!((one << m as u32) <= n, "MSB: 1<<m <= n");
    if m < 255 {
        assert!(n < (one << (m as u32 + 1)), "MSB: n < 1<<(m+1)");
    }

    // ---- LSB ----
    // (cosmwasm Uint256 has no BitAnd, so express bit tests with shifts + Rem.)
    let two = Uint256::from(2u32);
    let l = least_significant_bit(n).expect("lsb of nonzero");
    // bit l is set  <=>  (n >> l) is odd
    assert!(!((n >> l as u32) % two).is_zero(), "LSB: bit l is set");
    // every bit below l is clear  <=>  n mod 2^l == 0
    if l > 0 {
        assert!((n % (one << l as u32)).is_zero(), "LSB: no lower bit set");
    }

    // sanity: lsb <= msb always
    assert!(l <= m, "lsb <= msb");
});

use crate::bit_math::most_significant_bit;
use crate::full_math::mul_div;
use cosmwasm_std::{StdError, StdResult, Uint256, Uint512};
use std::convert::TryFrom;
use std::str::FromStr;

/// The minimum tick passable to `get_sqrt_ratio_at_tick`.
/// Derived from `log_1.0001(2^-128)`.
pub const MIN_TICK: i32 = -887272;
/// The maximum tick passable to `get_sqrt_ratio_at_tick`.
/// Derived from `log_1.0001(2^128)`.
pub const MAX_TICK: i32 = 887272;

/// `get_sqrt_ratio_at_tick(MIN_TICK)` — inclusive lower bound on valid prices.
pub const MIN_SQRT_RATIO: u128 = 4295128739;

/// `get_sqrt_ratio_at_tick(MAX_TICK + 1)` — exclusive upper bound on valid prices.
pub fn max_sqrt_ratio() -> Uint256 {
    Uint256::from_str("1461446703485210103287273052203988822378723970342").unwrap()
}

/// Computes `sqrt(1.0001^tick) * 2^96` as a Q64.96 fixed-point Uint256.
///
/// This is the Uniswap V3 magic-constant algorithm. The prior Choice
/// implementation was missing the round-up on the final `>> 32`, which
/// produced off-by-one errors at tick boundaries. This implementation adds +1
/// whenever any of the 32 bits shifted out are non-zero, matching V3 v1.0.1.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> StdResult<Uint256> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(StdError::generic_err("Tick out of bounds"));
    }

    let abs_tick = tick.unsigned_abs();

    let mut ratio = Uint256::one() << 128;

    if (abs_tick & 0x1) != 0 {
        ratio = mul_shift_128(ratio, 0xfffcb933bd6fad37aa2d162d1a594001)?;
    }
    if (abs_tick & 0x2) != 0 {
        ratio = mul_shift_128(ratio, 0xfff97272373d413259a46990580e213a)?;
    }
    if (abs_tick & 0x4) != 0 {
        ratio = mul_shift_128(ratio, 0xfff2e50f5f656932ef12357cf3c7fdcc)?;
    }
    if (abs_tick & 0x8) != 0 {
        ratio = mul_shift_128(ratio, 0xffe5caca7e10e4e61c3624eaa0941cd0)?;
    }
    if (abs_tick & 0x10) != 0 {
        ratio = mul_shift_128(ratio, 0xffcb9843d60f6159c9db58835c926644)?;
    }
    if (abs_tick & 0x20) != 0 {
        ratio = mul_shift_128(ratio, 0xff973b41fa98c081472e6896dfb254c0)?;
    }
    if (abs_tick & 0x40) != 0 {
        ratio = mul_shift_128(ratio, 0xff2ea16466c96a3843ec78b326b52861)?;
    }
    if (abs_tick & 0x80) != 0 {
        ratio = mul_shift_128(ratio, 0xfe5dee046a99a2a811c461f1969c3053)?;
    }
    if (abs_tick & 0x100) != 0 {
        ratio = mul_shift_128(ratio, 0xfcbe86c7900a88aedcffc83b479aa3a4)?;
    }
    if (abs_tick & 0x200) != 0 {
        ratio = mul_shift_128(ratio, 0xf987a7253ac413176f2b074cf7815e54)?;
    }
    if (abs_tick & 0x400) != 0 {
        ratio = mul_shift_128(ratio, 0xf3392b0822b7566f5329a91d259bcb24)?;
    }
    if (abs_tick & 0x800) != 0 {
        ratio = mul_shift_128(ratio, 0xe7159475a2c29b7443b29c7fa6e889d9)?;
    }
    if (abs_tick & 0x1000) != 0 {
        ratio = mul_shift_128(ratio, 0xd097f3bdfd2022b8845ad8f792aa5825)?;
    }
    if (abs_tick & 0x2000) != 0 {
        ratio = mul_shift_128(ratio, 0xa9f746462d870fdf8a65dc1f90e061e5)?;
    }
    if (abs_tick & 0x4000) != 0 {
        ratio = mul_shift_128(ratio, 0x70d869a156d2a1b890bb3df62baf32f7)?;
    }
    if (abs_tick & 0x8000) != 0 {
        ratio = mul_shift_128(ratio, 0x31be135f97d08fd981231505542fcfa6)?;
    }
    if (abs_tick & 0x10000) != 0 {
        ratio = mul_shift_128(ratio, 0x9aa508b5b7a84e1c677de54f3e99bc9)?;
    }
    if (abs_tick & 0x20000) != 0 {
        ratio = mul_shift_128(ratio, 0x5d6af8dedb81196699c329225ee604)?;
    }
    if (abs_tick & 0x40000) != 0 {
        ratio = mul_shift_128(ratio, 0x2216e584f5fa1ea926041bedfe98)?;
    }
    if (abs_tick & 0x80000) != 0 {
        ratio = mul_shift_128(ratio, 0x48a170391f7dc42444e8fa2)?;
    }

    if tick > 0 {
        ratio = Uint256::MAX
            .checked_div(ratio)
            .map_err(|_| StdError::generic_err("Div by zero"))?;
    }

    // Q128.128 -> Q64.96 requires >> 32. Round UP if any low bit is non-zero.
    // Uint256 doesn't implement BitAnd, so we test "is multiple of 2^32" by
    // comparing to `(ratio >> 32) << 32`.
    let q = ratio >> 32u32;
    let trunc = q << 32u32;
    if trunc == ratio {
        Ok(q)
    } else {
        Ok(q + Uint256::one())
    }
}

/// Helper: `(a * b) >> 128` with full 512-bit precision.
fn mul_shift_128(a: Uint256, b: u128) -> StdResult<Uint256> {
    let shift = Uint256::one() << 128;
    mul_div(a, Uint256::from(b), shift)
}

/// Full-width product of two Uint256 values, returning Uint512.
fn full_mul_u512(a: Uint256, b: Uint256) -> StdResult<Uint512> {
    let a_512 = Uint512::from(a);
    let b_512 = Uint512::from(b);
    a_512
        .checked_mul(b_512)
        .map_err(|_| StdError::generic_err("full_mul_u512: overflow (unreachable for U256)"))
}

/// Extracts the low 256 bits of a Uint512 by masking off the top.
fn u512_low_256(v: Uint512) -> Uint256 {
    // Uint256::MAX fits in Uint512. The expression `v % (U256::MAX + 1)` isn't
    // directly representable, so we compute `v - ((v >> 256) << 256)`.
    let top = (v >> 256u32) << 256u32;
    let low = v - top;
    // Safe: low < 2^256.
    Uint256::try_from(low).expect("u512_low_256: result fits in Uint256 by construction")
}

/// Computes the greatest tick `T` such that `get_sqrt_ratio_at_tick(T) <= sqrt_price`.
///
/// Implements Uniswap V3 v1.0.1's O(1) log-2 algorithm. Uses sign-magnitude to
/// model the int256 arithmetic because cosmwasm_std has no native I256.
pub fn get_tick_at_sqrt_ratio(sqrt_price: Uint256) -> StdResult<i32> {
    if sqrt_price < Uint256::from(MIN_SQRT_RATIO) || sqrt_price >= max_sqrt_ratio() {
        return Err(StdError::generic_err("Price out of bounds"));
    }

    // Step 1: extend to Q128.128 for the log_2 computation.
    let ratio = sqrt_price << 32u32;

    // Step 2: find MSB. `most_significant_bit` on cosmwasm Uint256 via the
    // `bit_math` helper (routes through the `uint` crate internally).
    let msb_u8 = most_significant_bit(ratio)?;
    let msb: u32 = msb_u8 as u32;

    // Step 3: normalize so MSB sits at bit 127.
    let mut r = if msb >= 128 {
        ratio >> (msb - 127)
    } else {
        ratio << (127 - msb)
    };

    // Step 4: initialize log_2 = (msb - 128) << 64 in sign-magnitude.
    // |msb - 128| <= 128, shifted 64 bits = <= 2^71, fits comfortably in Uint256.
    let (mut log2_neg, mut log2_mag): (bool, Uint256) = if msb >= 128 {
        (false, Uint256::from(msb - 128) << 64u32)
    } else {
        (true, Uint256::from(128 - msb) << 64u32)
    };

    // Step 5: 14 iterations extracting one bit of log_2 per step.
    for i in 0..14 {
        // r = (r * r) >> 127   (r is ~2^127, so r^2 is ~2^254, shift back)
        let r_sq = full_mul_u512(r, r)?;
        let r_shifted = r_sq >> 127u32;
        r = Uint256::try_from(r_shifted)
            .map_err(|_| StdError::generic_err("r overflow after square"))?;

        // f = r >> 128 (either 0 or 1 by construction)
        let f_u256 = r >> 128u32;
        let f: u32 = if f_u256.is_zero() { 0 } else { 1 };

        // log_2 |= f << (63 - i)
        if f != 0 {
            let bit = Uint256::one() << (63 - i as u32);
            let (new_neg, new_mag) = signed_or_bit(log2_neg, log2_mag, bit);
            log2_neg = new_neg;
            log2_mag = new_mag;
        }

        // r >>= f
        if f != 0 {
            r >>= 1u32;
        }
    }

    // Step 6: log_sqrt10001 = log_2 * 255738958999603826347141 (signed).
    let k = Uint256::from(255738958999603826347141u128);
    let prod_512 = full_mul_u512(log2_mag, k)?;
    let log_sqrt_mag = u512_low_256(prod_512);
    let log_sqrt_neg = log2_neg;

    // Step 7: compute tickLow / tickHi.
    //   tick_low = (log_sqrt - 3402992956809132418596140100660247210) >>> 128
    //   tick_hi  = (log_sqrt + 291339464771989622907027621153398088495) >>> 128
    let sub_const = Uint256::from_str("3402992956809132418596140100660247210").unwrap();
    let add_const = Uint256::from_str("291339464771989622907027621153398088495").unwrap();

    let low_val = signed_sub_abs(log_sqrt_neg, log_sqrt_mag, false, sub_const);
    let hi_val = signed_add_abs(log_sqrt_neg, log_sqrt_mag, false, add_const);

    let tick_low = signed_arith_shift_right_128(low_val.0, low_val.1);
    let tick_hi = signed_arith_shift_right_128(hi_val.0, hi_val.1);

    let tick = if tick_low == tick_hi {
        tick_low
    } else {
        let tick_hi_i32 = i32::try_from(tick_hi)
            .map_err(|_| StdError::generic_err("tick_hi out of i32 range"))?;
        let sqrt_at_hi = get_sqrt_ratio_at_tick(tick_hi_i32)?;
        if sqrt_at_hi <= sqrt_price {
            tick_hi
        } else {
            tick_low
        }
    };

    i32::try_from(tick).map_err(|_| StdError::generic_err("tick out of i32 range"))
}

/// Signed sign-magnitude "OR single bit" operation: computes `signed_value | bit`
/// interpreting `signed_value` as two's complement. Since bit is always
/// positive and a single set bit, for positive operand the result is the
/// straightforward OR; for negative operand we must round magnitude down via
/// two's-complement semantics.
fn signed_or_bit(neg: bool, mag: Uint256, bit: Uint256) -> (bool, Uint256) {
    if !neg {
        // Uint256 has no BitOr; but bit is a single set bit, so OR is
        // equivalent to `+ bit` when that bit is unset. Test bitness by
        // checking `(mag / bit) is_even`.
        let shifted = mag / bit;
        // is_even iff `shifted - (shifted >> 1) << 1 == 0`
        let trunc = (shifted >> 1u32) << 1u32;
        let bit_already_set = shifted != trunc;
        if bit_already_set {
            (false, mag)
        } else {
            (false, mag + bit)
        }
    } else {
        // Two's complement of magnitude, then +bit if bit is unset, then
        // convert back. For sign-magnitude, we need: result = -(|mag| - bit) if
        // the OR operation effectively adds `bit` to the negative number (which
        // moves it toward zero by `bit` unless the bit was already set).
        // Equivalent logic: TwoC(x) has bit B set iff mag doesn't have B set
        // (wait — no, that's only true for certain bit positions).
        //
        // Simpler: convert to two's complement in a wide-enough (U512) space,
        // perform the OR, convert back.
        //
        // Using 128-bit window is enough for our magnitudes.
        let twos = twos_complement_256(mag);
        let twos_or = u256_or_bit(twos, bit);
        // Interpret result as signed (sign bit at 255).
        let sign_bit = twos_or >> 255u32;
        if sign_bit.is_zero() {
            (false, twos_or)
        } else {
            (true, twos_complement_256(twos_or))
        }
    }
}

/// `(!x) + 1` in Uint256 (two's complement negation).
fn twos_complement_256(x: Uint256) -> Uint256 {
    let not_x = Uint256::MAX - x;
    not_x + Uint256::one()
}

/// Computes `a | bit` where `bit` has exactly one bit set. Uint256 lacks
/// BitOr, so we test and add.
fn u256_or_bit(a: Uint256, bit: Uint256) -> Uint256 {
    let shifted = a / bit;
    let trunc = (shifted >> 1u32) << 1u32;
    let already_set = shifted != trunc;
    if already_set {
        a
    } else {
        a + bit
    }
}

/// Signed subtraction: `(a_neg ? -a_mag : a_mag) - (b_neg ? -b_mag : b_mag)`.
fn signed_sub_abs(a_neg: bool, a_mag: Uint256, b_neg: bool, b_mag: Uint256) -> (bool, Uint256) {
    match (a_neg, b_neg) {
        (false, false) => {
            if a_mag >= b_mag {
                (false, a_mag - b_mag)
            } else {
                (true, b_mag - a_mag)
            }
        }
        (false, true) => (false, a_mag + b_mag),
        (true, false) => (true, a_mag + b_mag),
        (true, true) => {
            if b_mag >= a_mag {
                (false, b_mag - a_mag)
            } else {
                (true, a_mag - b_mag)
            }
        }
    }
}

fn signed_add_abs(a_neg: bool, a_mag: Uint256, b_neg: bool, b_mag: Uint256) -> (bool, Uint256) {
    signed_sub_abs(a_neg, a_mag, !b_neg, b_mag)
}

/// Arithmetic right shift of a sign-magnitude value by 128, returning i64
/// (sufficient for tick values which are always in i24 range).
fn signed_arith_shift_right_128(neg: bool, mag: Uint256) -> i64 {
    let q = mag >> 128u32;
    let trunc = q << 128u32;
    if !neg {
        // Positive: plain floor(mag / 2^128).
        uint256_low_u64(q) as i64
    } else {
        // Negative: arithmetic shift rounds toward -inf, so
        //   -ceil(mag / 2^128)
        let ceil = if trunc == mag { q } else { q + Uint256::one() };
        let ceil_u64 = uint256_low_u64(ceil);
        -(ceil_u64 as i64)
    }
}

fn uint256_low_u64(x: Uint256) -> u64 {
    // Take least-significant 8 bytes.
    let bytes = x.to_be_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[24..32]);
    u64::from_be_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_ratio_at_tick_zero_is_q96() {
        let r = get_sqrt_ratio_at_tick(0).unwrap();
        assert_eq!(r, Uint256::one() << 96);
    }

    #[test]
    fn sqrt_ratio_at_min_and_max_tick() {
        // V3 convention: MIN_SQRT_RATIO == getSqrtRatioAtTick(MIN_TICK),
        // MAX_SQRT_RATIO == getSqrtRatioAtTick(MAX_TICK). The two differ only
        // in how `getTickAtSqrtRatio` treats them: MIN is inclusive, MAX is
        // exclusive. We preserve that asymmetry.
        let min_r = get_sqrt_ratio_at_tick(MIN_TICK).unwrap();
        assert_eq!(min_r, Uint256::from(MIN_SQRT_RATIO));
        let max_r = get_sqrt_ratio_at_tick(MAX_TICK).unwrap();
        assert_eq!(max_r, max_sqrt_ratio());
    }

    #[test]
    fn tick_roundtrip_common_values() {
        // MAX_TICK deliberately excluded: getSqrtRatioAtTick(MAX_TICK) ==
        // MAX_SQRT_RATIO, which getTickAtSqrtRatio rejects (V3 invariant).
        let cases = [
            -887272, -500000, -100000, -10000, -1000, -500, -100, -1, 0, 1, 100, 500, 1000, 10000,
            100000, 500000, 887271,
        ];
        for &t in &cases {
            let p = get_sqrt_ratio_at_tick(t).unwrap();
            let recovered = get_tick_at_sqrt_ratio(p).unwrap();
            assert_eq!(recovered, t, "roundtrip failed for tick {}", t);
        }
    }

    #[test]
    fn tick_at_min_and_max_sqrt_ratio() {
        assert_eq!(
            get_tick_at_sqrt_ratio(Uint256::from(MIN_SQRT_RATIO)).unwrap(),
            MIN_TICK
        );
        assert!(get_tick_at_sqrt_ratio(max_sqrt_ratio()).is_err());
    }

    #[test]
    fn tick_at_sqrt_ratio_below_min_rejected() {
        assert!(get_tick_at_sqrt_ratio(Uint256::from(MIN_SQRT_RATIO - 1)).is_err());
    }

    #[test]
    fn tick_math_half_and_double_price() {
        let q96 = Uint256::one() << 96;
        let price_two = q96 << 1u32;
        let tick_two = get_tick_at_sqrt_ratio(price_two).unwrap();
        assert_eq!(tick_two, 13863);

        let price_half = q96 >> 1u32;
        let tick_half = get_tick_at_sqrt_ratio(price_half).unwrap();
        assert_eq!(tick_half, -13864);
    }

    // ---------------------------------------------------------------------
    // Fuzzing: validate `get_tick_at_sqrt_ratio` against its defining contract
    // directly, not just against tick-derived prices.
    //
    // The contract (Uniswap V3): `get_tick_at_sqrt_ratio(p)` returns the
    // GREATEST tick `T` such that `get_sqrt_ratio_at_tick(T) <= p`. Equivalently,
    // for the returned `T`:
    //     sqrtRatio(T)     <= p
    //     sqrtRatio(T + 1) >  p          (when T < MAX_TICK)
    //
    // We check this on a large set of pseudo-random prices spanning the entire
    // valid Q64.96 range `[MIN_SQRT_RATIO, MAX_SQRT_RATIO)`. This is stronger
    // than a tick→price→tick roundtrip: it exercises prices that fall strictly
    // BETWEEN adjacent ticks, where the v3 log-base algorithm's `tick_low` /
    // `tick_hi` disambiguation (and the sign-magnitude arithmetic emulating
    // int256) is most likely to be off by one.
    // ---------------------------------------------------------------------

    /// Deterministic splitmix64 PRNG — reproducible, no external dependency.
    fn next_u64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn random_u256(state: &mut u64) -> Uint256 {
        let mut bytes = [0u8; 32];
        for chunk in bytes.chunks_mut(8) {
            chunk.copy_from_slice(&next_u64(state).to_be_bytes());
        }
        Uint256::from_be_bytes(bytes)
    }

    /// Uniform-ish random price in `[MIN_SQRT_RATIO, MAX_SQRT_RATIO)`.
    fn random_price_in_range(state: &mut u64) -> Uint256 {
        let lo = Uint256::from(MIN_SQRT_RATIO);
        let hi = max_sqrt_ratio();
        let span = hi - lo; // > 0
        lo + (random_u256(state) % span)
    }

    /// Assert the defining contract holds for a single price.
    fn assert_tick_contract(p: Uint256) {
        let t = get_tick_at_sqrt_ratio(p)
            .unwrap_or_else(|e| panic!("get_tick_at_sqrt_ratio({}) errored: {}", p, e));

        assert!(
            (MIN_TICK..=MAX_TICK).contains(&t),
            "tick {} out of bounds for price {}",
            t,
            p
        );

        let at_t = get_sqrt_ratio_at_tick(t).unwrap();
        assert!(
            at_t <= p,
            "sqrtRatio(T)={} must be <= price={} (T={})",
            at_t,
            p,
            t
        );

        if t < MAX_TICK {
            let at_t_plus_1 = get_sqrt_ratio_at_tick(t + 1).unwrap();
            assert!(
                at_t_plus_1 > p,
                "sqrtRatio(T+1)={} must be > price={} (T={})",
                at_t_plus_1,
                p,
                t
            );
        }
    }

    #[test]
    fn fuzz_tick_at_sqrt_ratio_contract_random_prices() {
        let mut state: u64 = 0x00C0_FFEE_1234_5678;
        // 200k random prices across the full range. Deterministic seed so any
        // failure is reproducible.
        for _ in 0..200_000 {
            let p = random_price_in_range(&mut state);
            assert_tick_contract(p);
        }
    }

    #[test]
    fn fuzz_tick_at_sqrt_ratio_contract_near_boundaries() {
        // Stress the inclusive/exclusive endpoints and a dense band of prices
        // hugging the exact tick boundaries, where off-by-one is most likely.
        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;

        // Exact endpoints.
        assert_tick_contract(Uint256::from(MIN_SQRT_RATIO));
        assert_tick_contract(max_sqrt_ratio() - Uint256::one());

        // For a spread of ticks, probe `sqrtRatio(T)`, `±1`, and a random jitter
        // up toward the next tick. Covers the just-above / just-below cases.
        let mut tick = MIN_TICK;
        while tick < MAX_TICK {
            let base = get_sqrt_ratio_at_tick(tick).unwrap();

            // Exactly on the tick: result must be >= tick (it's a valid floor).
            assert_tick_contract(base);
            // One unit above the tick boundary (still < next tick boundary).
            assert_tick_contract(base + Uint256::one());
            // Just below the boundary maps to the previous tick — only valid
            // while we stay at/above MIN_SQRT_RATIO.
            if base > Uint256::from(MIN_SQRT_RATIO) {
                assert_tick_contract(base - Uint256::one());
            }
            // Random point inside [sqrtRatio(tick), sqrtRatio(tick+1)).
            let next = get_sqrt_ratio_at_tick(tick + 1).unwrap();
            let gap = next - base;
            if gap > Uint256::one() {
                assert_tick_contract(base + (random_u256(&mut state) % gap));
            }

            // Non-uniform stride so we don't only sample multiples of a power of
            // two; still covers the whole range in a reasonable test runtime.
            tick += 9973; // prime stride
        }
    }

    /// Exhaustive tick→price→tick roundtrip. Heavy (~1.77M ticks); run with
    /// `--release` for speed. `MAX_TICK` is excluded because
    /// `sqrtRatio(MAX_TICK) == MAX_SQRT_RATIO`, which `get_tick_at_sqrt_ratio`
    /// rejects by design (exclusive upper bound).
    #[test]
    #[ignore = "exhaustive 1.77M-tick sweep; run explicitly with --release"]
    fn fuzz_tick_roundtrip_exhaustive() {
        for t in MIN_TICK..MAX_TICK {
            let p = get_sqrt_ratio_at_tick(t).unwrap();
            let recovered = get_tick_at_sqrt_ratio(p).unwrap();
            assert_eq!(recovered, t, "roundtrip failed at tick {}", t);
        }
    }
}

//! Uniswap V3 v1.0.1 tick_math parity vectors.
//!
//! Values imported verbatim from `v3-core/test/TickMath.spec.ts`. If any of
//! these assertions fail, the library has diverged from V3 and cross-contract
//! integrations that assume V3 parity will mis-price positions.

use std::str::FromStr;

use choice_clmm_math::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};
use cosmwasm_std::Uint256;

/// Helper matching V3's `encodePriceSqrt(reserve1, reserve0)`:
/// returns `sqrt(reserve1 / reserve0) * 2^96` as Uint256. For test purposes
/// we compute the exact price ratios V3 uses (e.g., `encodePriceSqrt(1, 1)`
/// is `2^96`, `encodePriceSqrt(2, 1)` is `sqrt(2) * 2^96`, etc.).
fn q96() -> Uint256 {
    Uint256::one() << 96
}

#[test]
fn v3_get_sqrt_ratio_at_tick_min() {
    // V3: `getSqrtRatioAtTick(MIN_TICK)` == MIN_SQRT_RATIO
    assert_eq!(
        get_sqrt_ratio_at_tick(MIN_TICK).unwrap(),
        Uint256::from(MIN_SQRT_RATIO)
    );
    assert_eq!(MIN_SQRT_RATIO, 4295128739u128);
}

#[test]
fn v3_get_sqrt_ratio_at_tick_min_plus_one() {
    // V3: `getSqrtRatioAtTick(MIN_TICK + 1)` == 4295343490
    assert_eq!(
        get_sqrt_ratio_at_tick(MIN_TICK + 1).unwrap(),
        Uint256::from(4295343490u128)
    );
}

#[test]
fn v3_get_sqrt_ratio_at_tick_max() {
    // V3: `getSqrtRatioAtTick(MAX_TICK)` == MAX_SQRT_RATIO
    //     == 1461446703485210103287273052203988822378723970342
    assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK).unwrap(), max_sqrt_ratio());
}

#[test]
fn v3_get_sqrt_ratio_at_tick_max_minus_one() {
    // V3: `getSqrtRatioAtTick(MAX_TICK - 1)`
    //     == 1461373636630004318706518188784493106690254656249
    let expected = Uint256::from_str("1461373636630004318706518188784493106690254656249").unwrap();
    assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK - 1).unwrap(), expected);
}

#[test]
fn v3_monotonic_in_tick() {
    // Strictly increasing on ticks.
    let sample = [
        MIN_TICK,
        MIN_TICK + 1,
        -500000,
        -100000,
        -1000,
        -50,
        -1,
        0,
        1,
        50,
        1000,
        100000,
        500000,
        MAX_TICK - 1,
        MAX_TICK,
    ];
    let mut prev: Option<Uint256> = None;
    for t in sample {
        let r = get_sqrt_ratio_at_tick(t).unwrap();
        if let Some(p) = prev {
            assert!(r > p, "non-monotonic at tick {}: {} !< {}", t, p, r);
        }
        prev = Some(r);
    }
}

#[test]
fn v3_reciprocal_relationship_holds_approximately() {
    // sqrt(-t) * sqrt(t) ≈ 2^192. Exact equality never holds because the
    // magic-constant algorithm truncates at every multiply step and rounds up
    // once at the end. V3 tolerates the same drift — this test guards against
    // regressions of more than a handful of units in the low bits.
    let q192 = Uint256::one() << 192u32;
    let sample = [1, 50, 1000, 10_000, 100_000, 500_000, MAX_TICK - 1];
    for t in sample {
        let pos = get_sqrt_ratio_at_tick(t).unwrap();
        let neg = get_sqrt_ratio_at_tick(-t).unwrap();
        let product = pos * neg;
        let diff = if product > q192 {
            product - q192
        } else {
            q192 - product
        };
        // Drift scales with |pos|, so we compare relative magnitude: the
        // product should be within 2 * pos units of 2^192 (equivalent to the
        // sqrt_ratio being off by at most 2 ulps).
        let tolerance = pos * Uint256::from(2u128);
        assert!(
            diff <= tolerance,
            "reciprocal drift too large at tick {}: product={} q192={} diff={} tol={}", t, product, q192, diff, tolerance,
        );
    }
}

// --- get_tick_at_sqrt_ratio vectors ---

#[test]
fn v3_get_tick_at_min_sqrt_ratio() {
    assert_eq!(
        get_tick_at_sqrt_ratio(Uint256::from(MIN_SQRT_RATIO)).unwrap(),
        MIN_TICK
    );
}

#[test]
fn v3_get_tick_at_min_plus_one_boundary() {
    // Anywhere in [MIN_SQRT_RATIO, getSqrtRatioAtTick(MIN_TICK+1)) maps to MIN_TICK.
    assert_eq!(
        get_tick_at_sqrt_ratio(Uint256::from(4295128740u128)).unwrap(),
        MIN_TICK
    );
    // `4295343490` is exactly `getSqrtRatioAtTick(MIN_TICK + 1)`, so tick
    // boundary shifts to MIN_TICK + 1.
    assert_eq!(
        get_tick_at_sqrt_ratio(Uint256::from(4295343490u128)).unwrap(),
        MIN_TICK + 1
    );
}

#[test]
fn v3_get_tick_at_encode_price_one_one() {
    // V3: getTickAtSqrtRatio(encodePriceSqrt(1, 1)) == 0
    assert_eq!(get_tick_at_sqrt_ratio(q96()).unwrap(), 0);
}

#[test]
fn v3_rejects_below_min_sqrt_ratio() {
    assert!(get_tick_at_sqrt_ratio(Uint256::from(MIN_SQRT_RATIO - 1)).is_err());
    assert!(get_tick_at_sqrt_ratio(Uint256::zero()).is_err());
}

#[test]
fn v3_rejects_at_or_above_max_sqrt_ratio() {
    // MAX_SQRT_RATIO is exclusive in V3 — this is the pair's MAX_TICK + 1 sqrt.
    assert!(get_tick_at_sqrt_ratio(max_sqrt_ratio()).is_err());
}

#[test]
fn v3_tick_sqrt_tick_roundtrip_dense() {
    // Every 2500 ticks across the full range except MAX_TICK itself
    // (`getSqrtRatioAtTick(MAX_TICK) == MAX_SQRT_RATIO` is rejected by
    // `getTickAtSqrtRatio` per V3).
    let mut t = MIN_TICK;
    while t < MAX_TICK {
        let sqrt = get_sqrt_ratio_at_tick(t).unwrap();
        let recovered = get_tick_at_sqrt_ratio(sqrt).unwrap();
        assert_eq!(recovered, t, "roundtrip failed at tick {}", t);
        t += 2500;
    }
}

#[test]
fn v3_tick_roundtrip_on_sqrt_samples() {
    // Inverse roundtrip: for arbitrary sqrt_prices in range,
    // `getSqrtRatioAtTick(getTickAtSqrtRatio(p)) <= p` and
    // `getSqrtRatioAtTick(getTickAtSqrtRatio(p) + 1) > p`.
    let min_sqrt = Uint256::from(MIN_SQRT_RATIO);
    let max_sqrt = max_sqrt_ratio();

    // Evenly spaced exponential samples.
    let samples: Vec<Uint256> = (1..20u32)
        .map(|i| min_sqrt + (max_sqrt - min_sqrt) * Uint256::from(i) / Uint256::from(20u32))
        .collect();

    for p in samples {
        let t = get_tick_at_sqrt_ratio(p).unwrap();
        let low = get_sqrt_ratio_at_tick(t).unwrap();
        assert!(low <= p, "tick {}: sqrt({}) = {} > p = {}", t, t, low, p);
        if t < MAX_TICK {
            let high = get_sqrt_ratio_at_tick(t + 1).unwrap();
            assert!(
                high > p,
                "tick {} + 1: sqrt({}) = {} !> p = {}",
                t,
                t + 1,
                high,
                p,
            );
        }
    }
}

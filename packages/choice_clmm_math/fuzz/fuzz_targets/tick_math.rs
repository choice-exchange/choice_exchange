#![no_main]
//! Fuzz `tick_math` — the Q64.96 tick<->sqrt-price bijection that anchors every
//! price in the pool. Two entry points, both fuzzed:
//!
//!   A. tick -> sqrt:  get_sqrt_ratio_at_tick
//!        - strictly monotonic in tick
//!        - round-trips: get_tick_at_sqrt_ratio(ratio(t)) == t
//!   B. sqrt -> tick:  get_tick_at_sqrt_ratio
//!        - the returned tick T brackets the price:
//!              ratio(T) <= price < ratio(T+1)
//!        - out-of-range prices Err, never panic.
use libfuzzer_sys::fuzz_target;
use arbitrary::Arbitrary;
use cosmwasm_std::Uint256;
use choice_clmm_math::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, max_sqrt_ratio, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};

#[derive(Arbitrary, Debug)]
struct In {
    tick: i32,
    price_hi: u128,
    price_lo: u128,
}

fn u256(hi: u128, lo: u128) -> Uint256 {
    (Uint256::from(hi) << 128) + Uint256::from(lo)
}

fuzz_target!(|input: In| {
    // ---- A. tick -> sqrt path ----
    // Clamp into the valid tick domain so we exercise the success path.
    let span = (MAX_TICK - MIN_TICK) as i64 + 1;
    let tick = (MIN_TICK as i64 + (input.tick as i64).rem_euclid(span)) as i32;

    let ratio = get_sqrt_ratio_at_tick(tick).expect("ratio for in-range tick");

    // Strict monotonicity vs the next tick.
    if tick < MAX_TICK {
        let ratio_next = get_sqrt_ratio_at_tick(tick + 1).expect("ratio for tick+1");
        assert!(ratio_next > ratio, "monotonic: ratio(t+1) > ratio(t)");
    }

    // Round-trip, respecting V3's HALF-OPEN domain: ratio(MAX_TICK) ==
    // max_sqrt_ratio(), which get_tick_at_sqrt_ratio treats as EXCLUSIVE
    // (MIN inclusive, MAX exclusive). So the identity holds for tick < MAX_TICK;
    // at MAX_TICK the inverse must instead reject the price.
    if tick < MAX_TICK {
        let recovered = get_tick_at_sqrt_ratio(ratio).expect("tick from valid ratio");
        assert_eq!(recovered, tick, "roundtrip tick -> sqrt -> tick");
    } else {
        assert!(
            get_tick_at_sqrt_ratio(ratio).is_err(),
            "MAX_TICK ratio == max_sqrt_ratio() is out of the inverse's domain"
        );
    }

    // ---- B. sqrt -> tick path on an arbitrary price ----
    let price = u256(input.price_hi, input.price_lo);
    let min = Uint256::from(MIN_SQRT_RATIO);
    let max = max_sqrt_ratio();

    if price < min || price >= max {
        // Domain is [MIN_SQRT_RATIO, max_sqrt_ratio): outside must Err.
        assert!(get_tick_at_sqrt_ratio(price).is_err(), "out-of-range price errs");
    } else {
        let t = get_tick_at_sqrt_ratio(price).expect("tick for in-range price");
        assert!((MIN_TICK..=MAX_TICK).contains(&t), "tick within bounds");
        // Bracket invariant: ratio(t) <= price < ratio(t+1).
        let rt = get_sqrt_ratio_at_tick(t).expect("ratio(t)");
        assert!(rt <= price, "bracket low: ratio(t) <= price");
        if t < MAX_TICK {
            let rt1 = get_sqrt_ratio_at_tick(t + 1).expect("ratio(t+1)");
            assert!(price < rt1, "bracket high: price < ratio(t+1)");
        }
    }
});

use crate::full_math::mul_div;
use cosmwasm_std::{StdError, StdResult, Uint256};
use std::str::FromStr;

/// The minimum tick that may be passed to #get_sqrt_ratio_at_tick computed from log base 1.0001 of 2^-128
pub const MIN_TICK: i32 = -887272;
/// The maximum tick that may be passed to #get_sqrt_ratio_at_tick computed from log base 1.0001 of 2^128
pub const MAX_TICK: i32 = 887272;

/// The minimum value that can be returned from #get_sqrt_ratio_at_tick. Equivalent to get_sqrt_ratio_at_tick(MIN_TICK)
pub const MIN_SQRT_RATIO: u128 = 4295128739;

pub fn max_sqrt_ratio() -> Uint256 {
    // 1461446703485210103287273052203988822378723970342
    Uint256::from_str("1461446703485210103287273052203988822378723970342").unwrap()
}

/// Calculates sqrt(1.0001^tick) * 2^96
pub fn get_sqrt_ratio_at_tick(tick: i32) -> StdResult<Uint256> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(StdError::generic_err("Tick out of bounds"));
    }

    let abs_tick = tick.unsigned_abs();

    // Start with 2^128 (0x1000...00)
    let mut ratio = Uint256::one() << 128;

    if (abs_tick & 0x1) != 0 {
        ratio = mul_shift_128(ratio, 0xfffcb933bd6fad37aa2d162d1a594001);
    }
    if (abs_tick & 0x2) != 0 {
        ratio = mul_shift_128(ratio, 0xfff97272373d413259a46990580e213a);
    }
    if (abs_tick & 0x4) != 0 {
        ratio = mul_shift_128(ratio, 0xfff2e50f5f656932ef12357cf3c7fdcc);
    }
    if (abs_tick & 0x8) != 0 {
        ratio = mul_shift_128(ratio, 0xffe5caca7e10e4e61c3624eaa0941cd0);
    }
    if (abs_tick & 0x10) != 0 {
        ratio = mul_shift_128(ratio, 0xffcb9843d60f6159c9db58835c926644);
    }
    if (abs_tick & 0x20) != 0 {
        ratio = mul_shift_128(ratio, 0xff973b41fa98c081472e6896dfb254c0);
    }
    if (abs_tick & 0x40) != 0 {
        ratio = mul_shift_128(ratio, 0xff2ea16466c96a3843ec78b326b52861);
    }
    if (abs_tick & 0x80) != 0 {
        ratio = mul_shift_128(ratio, 0xfe5dee046a99a2a811c461f1969c3053);
    }
    if (abs_tick & 0x100) != 0 {
        ratio = mul_shift_128(ratio, 0xfcbe86c7900a88aedcffc83b479aa3a4);
    }
    if (abs_tick & 0x200) != 0 {
        ratio = mul_shift_128(ratio, 0xf987a7253ac413176f2b074cf7815e54);
    }
    if (abs_tick & 0x400) != 0 {
        ratio = mul_shift_128(ratio, 0xf3392b0822b7566f5329a91d259bcb24);
    }
    if (abs_tick & 0x800) != 0 {
        ratio = mul_shift_128(ratio, 0xe7159475a2c29b7443b29c7fa6e889d9);
    }
    if (abs_tick & 0x1000) != 0 {
        ratio = mul_shift_128(ratio, 0xd097f3bdfd2022b8845ad8f792aa5825);
    }
    if (abs_tick & 0x2000) != 0 {
        ratio = mul_shift_128(ratio, 0xa9f746462d870fdf8a65dc1f90e061e5);
    }
    if (abs_tick & 0x4000) != 0 {
        ratio = mul_shift_128(ratio, 0x70d869a156d2a1b890bb3df62baf32f7);
    }
    if (abs_tick & 0x8000) != 0 {
        ratio = mul_shift_128(ratio, 0x31be135f97d08fd981231505542fcfa6);
    }
    if (abs_tick & 0x10000) != 0 {
        ratio = mul_shift_128(ratio, 0x9aa508b5b7a84e1c677de54f3e99bc9);
    }
    if (abs_tick & 0x20000) != 0 {
        ratio = mul_shift_128(ratio, 0x5d6af8dedb81196699c329225ee604);
    }
    if (abs_tick & 0x40000) != 0 {
        ratio = mul_shift_128(ratio, 0x2216e584f5fa1ea926041bedfe98);
    }
    if (abs_tick & 0x80000) != 0 {
        ratio = mul_shift_128(ratio, 0x48a170391f7dc42444e8fa2);
    }

    if tick > 0 {
        ratio = Uint256::MAX
            .checked_div(ratio)
            .map_err(|_| StdError::generic_err("Div by zero"))?;
    }

    // shift right by 32 (128 - 96)
    ratio >>= 32;

    Ok(ratio)
}

/// Helper: Performs (a * b) >> 128
fn mul_shift_128(a: Uint256, b: u128) -> Uint256 {
    let shift = Uint256::one() << 128;
    mul_div(a, Uint256::from(b), shift)
}

pub fn get_tick_at_sqrt_ratio(sqrt_price: Uint256) -> StdResult<i32> {
    // 1. Validation
    if sqrt_price < Uint256::from(MIN_SQRT_RATIO) || sqrt_price >= max_sqrt_ratio() {
        return Err(StdError::generic_err("Price out of bounds"));
    }

    // 2. Binary Search
    // We want to find the largest tick T such that Price(T) <= sqrt_price

    let mut low = MIN_TICK;
    let mut high = MAX_TICK;

    while low < high {
        // Calculate mid using ceiling division to handle the "High" side correctly
        // mid = low + (high - low + 1) / 2
        let mid = low + (high - low + 1) / 2;

        // This call is safe because mid is always within MIN/MAX bounds
        let mid_price = get_sqrt_ratio_at_tick(mid)?;

        if mid_price <= sqrt_price {
            // mid is a valid floor candidate, so we move the lower bound up to mid.
            // We don't exclude mid because it might be the exact answer.
            low = mid;
        } else {
            // mid is strictly greater than the target.
            // So the answer must be strictly less than mid.
            high = mid - 1;
        }
    }

    Ok(low)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tick_math_roundtrip() {
        // Test 1.0
        let price_one = Uint256::one() << 96u32;
        let tick = get_tick_at_sqrt_ratio(price_one).unwrap();
        assert_eq!(tick, 0);

        // Test SqrtPrice = 2.0 (Price = 4.0)
        // log_1.0001(4) approx 13863
        let price_two = price_one << 1u32;
        let tick_two = get_tick_at_sqrt_ratio(price_two).unwrap();
        assert_eq!(tick_two, 13863);

        // Test SqrtPrice = 0.5 (Price = 0.25)
        // log_1.0001(0.25) approx -13863
        let price_half = price_one >> 1u32;
        let tick_half = get_tick_at_sqrt_ratio(price_half).unwrap();
        assert_eq!(tick_half, -13864);

        // Roundtrip random
        // Tick 500
        let test_tick = 500;
        let p = get_sqrt_ratio_at_tick(test_tick).unwrap();
        let t = get_tick_at_sqrt_ratio(p).unwrap();
        assert_eq!(t, test_tick);

        // Tick -500
        let test_tick_neg = -500;
        let p_neg = get_sqrt_ratio_at_tick(test_tick_neg).unwrap();
        let t_neg = get_tick_at_sqrt_ratio(p_neg).unwrap();
        assert_eq!(t_neg, test_tick_neg);
    }
}

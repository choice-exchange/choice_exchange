use std::convert::TryFrom;

use cosmwasm_std::{StdError, StdResult, Uint256, Uint512};

/// Calculates floor((a * b) / denominator) with 512-bit intermediate precision.
///
/// Returns an error on:
/// - denominator == 0
/// - result > U256::MAX
///
/// Never panics. This replaces unchecked `.expect()` calls that could be
/// triggered by attacker-controlled inputs and brick the contract.
pub fn mul_div(a: Uint256, b: Uint256, denominator: Uint256) -> StdResult<Uint256> {
    if denominator.is_zero() {
        return Err(StdError::generic_err("mul_div: division by zero"));
    }

    let a_512 = Uint512::from(a);
    let b_512 = Uint512::from(b);
    let denom_512 = Uint512::from(denominator);

    // a * b fits in U512 for any a,b in U256 (product <= (2^256-1)^2 < 2^512).
    let product = a_512.checked_mul(b_512).map_err(|_| {
        StdError::generic_err("mul_div: multiplication overflow (unreachable for U256 inputs)")
    })?;

    let result = product
        .checked_div(denom_512)
        .map_err(|_| StdError::generic_err("mul_div: division by zero"))?;

    Uint256::try_from(result).map_err(|_| StdError::generic_err("mul_div: result exceeds U256"))
}

/// Calculates ceil((a * b) / denominator) with 512-bit intermediate precision.
///
/// Returns an error on:
/// - denominator == 0
/// - result > U256::MAX
///
/// Never panics.
pub fn mul_div_round_up(a: Uint256, b: Uint256, denominator: Uint256) -> StdResult<Uint256> {
    if denominator.is_zero() {
        return Err(StdError::generic_err("mul_div_round_up: division by zero"));
    }

    let a_512 = Uint512::from(a);
    let b_512 = Uint512::from(b);
    let denom_512 = Uint512::from(denominator);

    let product = a_512.checked_mul(b_512).map_err(|_| {
        StdError::generic_err(
            "mul_div_round_up: multiplication overflow (unreachable for U256 inputs)",
        )
    })?;

    if product.is_zero() {
        return Ok(Uint256::zero());
    }

    // ceil(p / d) = (p + d - 1) / d; safe since product <= 2^512 - 1 - denom.
    let numerator = product + denom_512 - Uint512::from(1u64);
    let result = numerator.checked_div(denom_512).map_err(|_| {
        StdError::generic_err("mul_div_round_up: division by zero (unreachable)")
    })?;

    Uint256::try_from(result)
        .map_err(|_| StdError::generic_err("mul_div_round_up: result exceeds U256"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_basic() {
        let a = Uint256::from(10u128);
        let b = Uint256::from(20u128);
        let d = Uint256::from(3u128);
        // floor(200 / 3) = 66
        assert_eq!(mul_div(a, b, d).unwrap(), Uint256::from(66u128));
    }

    #[test]
    fn mul_div_round_up_basic() {
        let a = Uint256::from(10u128);
        let b = Uint256::from(20u128);
        let d = Uint256::from(3u128);
        // ceil(200 / 3) = 67
        assert_eq!(mul_div_round_up(a, b, d).unwrap(), Uint256::from(67u128));
    }

    #[test]
    fn mul_div_exact() {
        let a = Uint256::from(10u128);
        let b = Uint256::from(20u128);
        let d = Uint256::from(4u128);
        // 200 / 4 = 50 exactly
        assert_eq!(mul_div(a, b, d).unwrap(), Uint256::from(50u128));
        assert_eq!(mul_div_round_up(a, b, d).unwrap(), Uint256::from(50u128));
    }

    #[test]
    fn mul_div_div_by_zero() {
        let a = Uint256::from(10u128);
        let b = Uint256::from(20u128);
        let d = Uint256::zero();
        assert!(mul_div(a, b, d).is_err());
        assert!(mul_div_round_up(a, b, d).is_err());
    }

    #[test]
    fn mul_div_result_overflow() {
        // a = b = U256::MAX, d = 1 -> result U256::MAX^2 > U256::MAX
        let max = Uint256::MAX;
        let one = Uint256::one();
        assert!(mul_div(max, max, one).is_err());
        assert!(mul_div_round_up(max, max, one).is_err());
    }

    #[test]
    fn mul_div_max_result_fits() {
        // a = U256::MAX, b = 1, d = 1 -> U256::MAX (fits)
        let max = Uint256::MAX;
        let one = Uint256::one();
        assert_eq!(mul_div(max, one, one).unwrap(), max);
        assert_eq!(mul_div_round_up(max, one, one).unwrap(), max);
    }

    #[test]
    fn mul_div_zero_numerator() {
        let zero = Uint256::zero();
        let d = Uint256::from(7u128);
        assert_eq!(mul_div(zero, Uint256::from(100u128), d).unwrap(), zero);
        assert_eq!(
            mul_div_round_up(zero, Uint256::from(100u128), d).unwrap(),
            zero
        );
    }
}

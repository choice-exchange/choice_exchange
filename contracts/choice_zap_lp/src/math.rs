use cosmwasm_std::{Isqrt, Uint128, Uint256};

use crate::error::ContractError;

/// Commission charged by choice_pair on swaps, expressed as a /1000 rate.
/// Mirrors the pair's `COMMISSION_RATE = 3` (= 0.3%) defined in
/// `contracts/choice_pair/src/contract.rs`. Both `optimal_swap_in` and the
/// `TWO_MINUS_F_*` / `FOUR_ONE_MINUS_F_*` coefficients below are hardcoded to
/// f = 0.003. If the pair's commission ever changes, this contract must be
/// re-derived and re-deployed — the closed form here will not auto-adjust.
/// The downstream `slippage_tolerance` on `ProvideLiquidity` catches gross
/// mismatches via `MaxSlippageAssertion`, so a stale constant fails gracefully
/// at the LP step rather than silently mis-allocating funds.
const COMMISSION_RATE_PERMILLE: u128 = 3;

/// Coefficients for the optimal-split quadratic with the pair's 0.3% fee.
///   (2-f)^2 = (1997/1000)^2 = 3988009/10^6
///   4*(1-f) = 4*997/1000   = 3988/1000     = 3988000/10^6
/// We carry the inside-sqrt term in 10^-6-scaled units, then divide the
/// final sqrt by 1000. **Pinned to f = 0.003** — see [`COMMISSION_RATE_PERMILLE`].
const TWO_MINUS_F_SQ_SCALED: u128 = 3_988_009;
const FOUR_ONE_MINUS_F_SCALED: u128 = 3_988_000;
/// 1997 = (2-f) * 1000
const TWO_MINUS_F_TIMES_1000: u128 = 1_997;
/// 1994 = 2*(1-f) * 1000
const TWO_ONE_MINUS_F_TIMES_1000: u128 = 1_994;

// Compile-time sanity: the four scaled coefficients below must remain
// consistent with COMMISSION_RATE_PERMILLE. If a future maintainer changes the
// commission constant, these asserts force them to re-derive the table.
const _: () = {
    assert!(COMMISSION_RATE_PERMILLE == 3);
    assert!(TWO_MINUS_F_TIMES_1000 == 2_000 - COMMISSION_RATE_PERMILLE);
    assert!(
        TWO_ONE_MINUS_F_TIMES_1000 == 2 * (1_000 - COMMISSION_RATE_PERMILLE)
    );
    assert!(
        TWO_MINUS_F_SQ_SCALED == TWO_MINUS_F_TIMES_1000 * TWO_MINUS_F_TIMES_1000
    );
    assert!(
        FOUR_ONE_MINUS_F_SCALED == 4 * (1_000 - COMMISSION_RATE_PERMILLE) * 1_000
    );
};

/// Integer square root of a Uint256, floored.
///
/// Delegates to `Uint256::isqrt` (bit-shift Newton's method that works across
/// the full Uint256 range). Earlier versions wrapped `Decimal256::sqrt`, which
/// internally scales by 10^18 and overflows once `radicand > ~1.16e59` — easy
/// to hit for pairs holding 18-decimal tokens with sizable reserves.
pub fn isqrt(n: Uint256) -> Result<Uint256, ContractError> {
    Ok(n.isqrt())
}

/// Optimal-split: given reserve `r_a` of the input side and total input
/// `x_in` (both in raw token units), return the amount of input to swap into
/// the other side so that, after the swap, the remaining input and the
/// received output are in pool ratio.
///
/// Solves `(1-f) * s^2 + (2-f) * r_a * s - r_a * x_in = 0` with f = 0.3%.
/// The closed form is
///   s = [sqrt(r_a * ((2-f)^2 * r_a + 4(1-f) * x_in)) - (2-f) * r_a] / (2(1-f))
///
/// Decimal scaling between the two pair sides cancels out in raw units, so
/// this works regardless of `asset_decimals` differences.
pub fn optimal_swap_in(r_a: Uint128, x_in: Uint128) -> Result<Uint128, ContractError> {
    if x_in.is_zero() {
        return Ok(Uint128::zero());
    }
    if r_a.is_zero() {
        return Err(ContractError::EmptyPool {});
    }

    let r_a256: Uint256 = r_a.into();
    let x256: Uint256 = x_in.into();

    let term_pool = r_a256
        .checked_mul(Uint256::from(TWO_MINUS_F_SQ_SCALED))
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let term_in = x256
        .checked_mul(Uint256::from(FOUR_ONE_MINUS_F_SCALED))
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let inside = term_pool
        .checked_add(term_in)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let radicand = r_a256
        .checked_mul(inside)
        .map_err(|_| ContractError::SplitMathOverflow {})?;

    let sqrt_scaled = isqrt(radicand)?;

    // sqrt_scaled is sqrt(radicand) where radicand is the true value scaled by
    // 10^6. So sqrt(true) = sqrt_scaled / 1000.
    //
    // s = (sqrt(true) - (2-f) * r_a) / (2(1-f))
    //   = (sqrt_scaled/1000 - 1997*r_a/1000) / (1994/1000)
    //   = (sqrt_scaled - 1997*r_a) / 1994
    let subtrahend = r_a256
        .checked_mul(Uint256::from(TWO_MINUS_F_TIMES_1000))
        .map_err(|_| ContractError::SplitMathOverflow {})?;

    if sqrt_scaled <= subtrahend {
        // Degenerate: input is too small to move anything past commission rounding.
        return Ok(Uint128::zero());
    }

    let numerator = sqrt_scaled - subtrahend;
    let s = numerator / Uint256::from(TWO_ONE_MINUS_F_TIMES_1000);

    // Cap at x_in to defend against pathological inputs (shouldn't happen given
    // the quadratic, but the contract treats this as an invariant).
    let s_u128: Uint128 = Uint128::try_from(s).map_err(ContractError::ConversionOverflowError)?;
    Ok(std::cmp::min(s_u128, x_in))
}

/// Approximate the pair's `compute_swap_raw` in raw units for unit tests of
/// the optimal-split derivation. **Test-only** — the on-chain `SimulateZap`
/// query delegates to the pair's `Simulation` entry point so the returned
/// `expected_return` matches the live swap wei-for-wei. This helper carries
/// slightly different rounding from the pair (integer-divisibility vs
/// `Decimal256` round-trip), which is acceptable for the ratio-check in
/// `optimal_split_balances_post_swap_ratio` but not for production use.
#[cfg(test)]
pub fn simulate_swap_return(
    offer_pool: Uint128,
    ask_pool: Uint128,
    offer_amount: Uint128,
) -> Result<Uint128, ContractError> {
    if offer_amount.is_zero() {
        return Ok(Uint128::zero());
    }
    let op: Uint256 = offer_pool.into();
    let ap: Uint256 = ask_pool.into();
    let oa: Uint256 = offer_amount.into();

    let denom = op
        .checked_add(oa)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    if denom.is_zero() {
        return Ok(Uint128::zero());
    }
    let numerator = ap
        .checked_mul(oa)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let gross = numerator / denom;

    // commission_amount = ceil(gross * 3 / 1000) — match the pair's ceiling fix-up.
    let commission_floor = gross
        .checked_mul(Uint256::from(COMMISSION_RATE_PERMILLE))
        .map_err(|_| ContractError::SplitMathOverflow {})?
        / Uint256::from(1000u128);
    let mut commission = commission_floor;
    if commission.checked_mul(Uint256::from(1000u128)).unwrap_or(Uint256::zero())
        != gross.checked_mul(Uint256::from(COMMISSION_RATE_PERMILLE)).unwrap_or(Uint256::zero())
    {
        commission += Uint256::from(1u128);
    }

    let net = gross
        .checked_sub(commission)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    Uint128::try_from(net).map_err(ContractError::ConversionOverflowError)
}

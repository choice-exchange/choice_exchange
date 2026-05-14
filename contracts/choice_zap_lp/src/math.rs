use cosmwasm_std::{Isqrt, Uint128, Uint256};

use crate::error::ContractError;

/// Commission charged by choice_pair on swaps, expressed as a /1000 rate.
/// Mirrors the pair's `COMMISSION_RATE = 3` (= 0.3%) defined in
/// `contracts/choice_pair/src/contract.rs`. The closed form below is hardcoded
/// to f = 0.003. If the pair's commission ever changes, this contract must be
/// re-derived and re-deployed — the closed form will not auto-adjust. The
/// downstream `slippage_tolerance` on `ProvideLiquidity` catches gross
/// mismatches via `MaxSlippageAssertion`, so a stale constant fails gracefully
/// at the LP step rather than silently mis-allocating funds.
///
/// Only consumed by the `const _` sanity block below (and the `#[cfg(test)]`
/// `simulate_swap_return`); `#[allow(dead_code)]` because rustc's dead-code
/// lint doesn't see const-expression uses.
#[allow(dead_code)]
const COMMISSION_RATE_PERMILLE: u128 = 3;

/// Fraction of the commission that STAYS in the pool (= LP-accrued fee). The
/// pair's swap splits `commission_amount` 6-ways: 1/6 burn, 1/6 fee_wallet,
/// the rest stays — see `choice_pair/src/contract.rs` (`fee_wallet_amount`,
/// `burn_amount`, `lp_amount`). Net `1 - 1/6 - 1/6 = 4/6 = 2/3` stays.
///
/// See `COMMISSION_RATE_PERMILLE` for the dead-code-allow rationale.
#[allow(dead_code)]
const POOL_KEEP_NUM: u128 = 4;
#[allow(dead_code)]
const POOL_KEEP_DEN: u128 = 6;

// ---- Coefficients of the corrected optimal-split quadratic ----
//
// Derivation: the user holds `(x - s)` of A and `(1-f) * gross` of B after a
// swap of size `s`, where `gross = R_b * s / (R_a + s)`. The pair's reserves
// after the swap are
//     R_a' = R_a + s
//     R_b' = R_b - gross * (1 - f * (1 - POOL_KEEP)) = R_b - gross * (1 - f*k')
// where k' = 1 - POOL_KEEP/1 = 1/3 is the leaving-pool share of the
// commission. With k = POOL_KEEP_NUM/POOL_KEEP_DEN = 2/3:
//     fee that LEAVES pool = f * (1 - k) = f/3
//     so R_b' = R_b - gross * (1 - f + f - f*k)  -- careful, redo:
// User receives `(1-f)*gross`. Burn + fee_wallet remove `(1-k)*f*gross`.
// Total leaving pool's B side = (1-f)*gross + (1-k)*f*gross = (1 - f*k)*gross.
//
// Setting A_user / B_user = R_a' / R_b' and substituting yields
//     (1 - f + f*k) * s² + ((2 - f) * R_a - f*k * x) * s - x * R_a = 0.
//
// With f = 0.003, k = 2/3, f*k = 0.002:
//     0.999 s² + (1.997 R_a - 0.002 x) s - x R_a = 0
// Multiply by 1000 to keep integer arithmetic:
//     999 s² + (1997 R_a - 2 x) s - 1000 x R_a = 0     (eq. *)
//
// Discriminant of (*): D = b² - 4ac
//   = (1997 R_a - 2x)² + 4 * 999 * 1000 * x * R_a
//   = 3_988_009 R_a² - 7988 R_a x + 4 x² + 3_996_000 R_a x
//   = 3_988_009 R_a² + 3_988_012 R_a x + 4 x²
// All non-negative, so D is always a clean Uint256 add.
//
// Root: s = (-b + sqrt(D)) / 2a = (sqrt(D) - 1997 R_a + 2x) / 1998.

/// `(2 - f) * 1000` — coefficient on R_a in `b`.
const TWO_MINUS_F_TIMES_1000: u128 = 1_997;
/// `f * k * 1000` — coefficient on x in `b`. With f=0.003, k=2/3 → 2.
const F_K_TIMES_1000: u128 = 2;
/// `2 * (1 - f + f*k) * 1000` — denominator `2a`. With f=0.003, k=2/3 → 1998.
const TWO_A_TIMES_1000: u128 = 1_998;

/// Discriminant coefficients (pre-multiplied to keep the integer math
/// straight). All three pieces of `D = D_R²·R² + D_RX·R·x + D_X²·x²` are
/// non-negative for the corrected quadratic.
const D_R_SQ: u128 = 3_988_009; // (2-f)² * 10⁶
const D_RX: u128 = 3_988_012; // (4*999*1000) - 2*1997*2 = 3,996,000 - 7988
const D_X_SQ: u128 = 4; // (f*k * 1000)² = 2²

// Compile-time sanity: the scaled coefficients must stay consistent with
// COMMISSION_RATE_PERMILLE and the pool-keep fraction. If a future maintainer
// changes either, these asserts force them to re-derive the whole table.
const _: () = {
    assert!(COMMISSION_RATE_PERMILLE == 3);
    assert!(POOL_KEEP_NUM == 4 && POOL_KEEP_DEN == 6);
    assert!(TWO_MINUS_F_TIMES_1000 == 2_000 - COMMISSION_RATE_PERMILLE);
    // f*k * 1000 = 1000 * (3/1000) * (4/6) = 2
    assert!(F_K_TIMES_1000 * POOL_KEEP_DEN == COMMISSION_RATE_PERMILLE * POOL_KEEP_NUM);
    // 2a = 2 * (1000 - 3 + 2) = 1998
    assert!(
        TWO_A_TIMES_1000
            == 2 * (1_000 - COMMISSION_RATE_PERMILLE
                + (COMMISSION_RATE_PERMILLE * POOL_KEEP_NUM) / POOL_KEEP_DEN)
    );
    // D_R² = (2-f)² * 10⁶ = 1997²
    assert!(D_R_SQ == TWO_MINUS_F_TIMES_1000 * TWO_MINUS_F_TIMES_1000);
    // D_RX = 4 * (1-f+fk) * 1000 * 1000 - 2 * (2-f) * (f*k) * 1_000
    //      = 4 * 999 * 1000 - 2 * 1997 * 2 = 3_996_000 - 7988 = 3_988_012
    assert!(
        D_RX == 4 * 999 * 1_000 - 2 * TWO_MINUS_F_TIMES_1000 * F_K_TIMES_1000
    );
    assert!(D_X_SQ == F_K_TIMES_1000 * F_K_TIMES_1000);
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
/// Solves the corrected quadratic
///     999 * s² + (1997 * r_a − 2 * x_in) * s − 1000 * r_a * x_in = 0
/// (see the coefficient block above for the full derivation), which accounts
/// for the 2/3 of the swap commission that the pair retains as LP-accrued
/// fee. The previous derivation assumed commission fully exited the pool
/// (Uniswap-V2-style), which left a small systematic dust on the input side.
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

    let r256: Uint256 = r_a.into();
    let x256: Uint256 = x_in.into();

    // D = D_R²·R² + D_RX·R·x + D_X²·x²
    let r_sq = r256
        .checked_mul(r256)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let rx = r256
        .checked_mul(x256)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let x_sq = x256
        .checked_mul(x256)
        .map_err(|_| ContractError::SplitMathOverflow {})?;

    let term_r = r_sq
        .checked_mul(Uint256::from(D_R_SQ))
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let term_rx = rx
        .checked_mul(Uint256::from(D_RX))
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let term_x = x_sq
        .checked_mul(Uint256::from(D_X_SQ))
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    let discriminant = term_r
        .checked_add(term_rx)
        .map_err(|_| ContractError::SplitMathOverflow {})?
        .checked_add(term_x)
        .map_err(|_| ContractError::SplitMathOverflow {})?;

    let sqrt_d = isqrt(discriminant)?;

    // s = (sqrt(D) − b) / 2a where b = 1997*R − 2*x. Express b as the
    // signed difference of two non-negative Uint256s.
    let b_pos = r256
        .checked_mul(Uint256::from(TWO_MINUS_F_TIMES_1000))
        .map_err(|_| ContractError::SplitMathOverflow {})?; // 1997 R
    let b_neg = x256
        .checked_mul(Uint256::from(F_K_TIMES_1000))
        .map_err(|_| ContractError::SplitMathOverflow {})?; // 2 x

    // numerator = sqrt(D) − (b_pos − b_neg) = sqrt(D) + b_neg − b_pos.
    // Always ≥ 0 because sqrt(D)² = D ≥ (b_pos − b_neg)² for any non-negative
    // R, x — the cross-term `4·999·1000·R·x` only adds to D. We still guard
    // against the degenerate `x = 0` case (already handled above).
    let lhs = sqrt_d
        .checked_add(b_neg)
        .map_err(|_| ContractError::SplitMathOverflow {})?;
    if lhs <= b_pos {
        return Ok(Uint128::zero());
    }
    let numerator = lhs - b_pos;
    let s = numerator / Uint256::from(TWO_A_TIMES_1000);

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

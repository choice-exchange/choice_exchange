//! Shared "attach exactly the tokenfactory create fee" enforcement.
//!
//! Both `choice_mts_issuer` (the create-denom fee on `RegisterLaunch`) and
//! `choice_pool_seeder` (the create-pair fee on the XYK `Settle`) require the
//! caller to attach the live chain fee EXACTLY — same denom set, same per-denom
//! amounts, no over-pay. The two contracts previously carried byte-identical
//! copies of this check; centralising it here keeps the exact-match semantics
//! from drifting between them. Each consumer wraps [`CreateFeeError`]
//! transparently in its own `ContractError` via `#[from]`.

use cosmwasm_std::Coin;
use std::fmt;

/// Error returned by [`require_exact_create_fee_funds`].
#[derive(Debug, PartialEq, Eq)]
pub enum CreateFeeError {
    /// Attached funds were short of the chain's create fee for `denom`.
    Insufficient {
        denom: String,
        required: String,
        supplied: String,
    },
    /// Attached funds exceeded the chain's create fee for `denom`. Over-pay is
    /// rejected (not refunded) so the contract never accumulates caller dust.
    Overpaid {
        denom: String,
        required: String,
        supplied: String,
    },
    /// A denom outside the chain's create-fee set was attached.
    UnexpectedDenom { denom: String },
}

impl fmt::Display for CreateFeeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateFeeError::Insufficient {
                denom,
                required,
                supplied,
            } => write!(
                f,
                "Attach the tokenfactory create fee in `info.funds`: need {required} {denom}, supplied {supplied}"
            ),
            CreateFeeError::Overpaid {
                denom,
                required,
                supplied,
            } => write!(
                f,
                "info.funds must equal the chain's create fee exactly: {denom} required {required}, supplied {supplied}"
            ),
            CreateFeeError::UnexpectedDenom { denom } => write!(
                f,
                "info.funds carries unexpected denom `{denom}` (only the chain's create fee may be attached)"
            ),
        }
    }
}

impl std::error::Error for CreateFeeError {}

/// Require `funds` to be EXACTLY the chain's tokenfactory create fee — same
/// denom set, same per-denom amounts. Over-pay and extra denoms are rejected
/// (rather than refunded) so the contract never accumulates caller-belonging
/// dust and the post-fee balance is trivially `balance - fee` for every denom:
/// no refund-vs-deposit ordering hazard. The keeper reads the live chain fee in
/// preflight and on a governance fee change retries with the new value.
pub fn require_exact_create_fee_funds(
    funds: &[Coin],
    create_fee: &[Coin],
) -> Result<(), CreateFeeError> {
    for fee in create_fee {
        let supplied = funds
            .iter()
            .find(|c| c.denom == fee.denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        if supplied < fee.amount {
            return Err(CreateFeeError::Insufficient {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
        if supplied > fee.amount {
            return Err(CreateFeeError::Overpaid {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
    }
    for c in funds.iter() {
        if !create_fee.iter().any(|f| f.denom == c.denom) {
            return Err(CreateFeeError::UnexpectedDenom {
                denom: c.denom.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::coin;

    #[test]
    fn exact_match_ok() {
        let fee = vec![coin(100, "inj")];
        assert!(require_exact_create_fee_funds(&fee, &fee).is_ok());
    }

    #[test]
    fn under_pay_rejected() {
        let res = require_exact_create_fee_funds(&[coin(99, "inj")], &[coin(100, "inj")]);
        assert!(matches!(res, Err(CreateFeeError::Insufficient { .. })));
    }

    #[test]
    fn missing_denom_is_under_pay() {
        let res = require_exact_create_fee_funds(&[], &[coin(100, "inj")]);
        assert!(matches!(res, Err(CreateFeeError::Insufficient { .. })));
    }

    #[test]
    fn over_pay_rejected() {
        let res = require_exact_create_fee_funds(&[coin(101, "inj")], &[coin(100, "inj")]);
        assert!(matches!(res, Err(CreateFeeError::Overpaid { .. })));
    }

    #[test]
    fn extra_denom_rejected() {
        let res = require_exact_create_fee_funds(
            &[coin(100, "inj"), coin(1, "foo")],
            &[coin(100, "inj")],
        );
        assert!(matches!(
            res,
            Err(CreateFeeError::UnexpectedDenom { ref denom }) if denom == "foo"
        ));
    }
}

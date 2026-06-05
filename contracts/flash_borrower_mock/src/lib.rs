//! Minimal flash-loan borrower used by `choice_clmm_pool` integration tests.
//!
//! It implements the pool's `FlashCallbackMsg::FlashCallback { fee0, fee1, data }`
//! shape. On callback it decodes a [`RepayPlan`] (passed through the pool as the
//! flash `data`) and repays the pool via a native bank `Send` of
//! `principal + fee` per token. A non-zero `underpay{0,1}` deliberately repays
//! short so the test can assert the pool's repayment check reverts.
//!
//! Repayment uses a direct bank `Send` (NOT a CW20 `Send` / no pool re-entry),
//! matching the passive-repayment model the pool documents.

use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    entry_point, from_json, BankMsg, Binary, Coin, Deps, DepsMut, Env, MessageInfo, Reply, Response,
    StdResult, SubMsg, Uint128,
};

/// Reply id for the optional submessage-wrapped repayment.
const REPLY_REPAY: u64 = 1;

#[cw_serde]
pub struct InstantiateMsg {}

#[cw_serde]
pub enum ExecuteMsg {
    /// Mirrors `choice_clmm_common::pool::FlashCallbackMsg::FlashCallback`.
    FlashCallback {
        fee0: Uint128,
        fee1: Uint128,
        data: Binary,
    },
}

/// Encoded by the test and threaded through the pool as the flash `data`.
/// Tells the borrower how much it borrowed (the callback only carries the fee)
/// and how to repay it.
#[cw_serde]
pub struct RepayPlan {
    pub denom0: String,
    pub denom1: String,
    /// Principal borrowed of each token.
    pub amount0: Uint128,
    pub amount1: Uint128,
    /// Amount to short the repayment by (0 = honest full repayment).
    pub underpay0: Uint128,
    pub underpay1: Uint128,
    /// When true, repay via a `SubMsg::reply_on_success` (the borrower's own
    /// reply runs) instead of a plain message. Exercises that a submessage bank
    /// send still settles BEFORE the pool's `reply_flash` queries the balance.
    /// `serde(default)` keeps older encodings (without this field) decodable.
    #[serde(default)]
    pub repay_via_submsg: bool,
}

#[entry_point]
pub fn instantiate(
    _deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    _msg: InstantiateMsg,
) -> StdResult<Response> {
    Ok(Response::new().add_attribute("action", "instantiate"))
}

#[entry_point]
pub fn execute(
    _deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::FlashCallback { fee0, fee1, data } => {
            let plan: RepayPlan = from_json(&data)?;
            // The pool is the caller of the flash callback; repay it directly.
            let pool = info.sender.into_string();

            let repay0 = (plan.amount0 + fee0).saturating_sub(plan.underpay0);
            let repay1 = (plan.amount1 + fee1).saturating_sub(plan.underpay1);

            let mut coins: Vec<Coin> = vec![];
            if !repay0.is_zero() {
                coins.push(Coin {
                    denom: plan.denom0,
                    amount: repay0,
                });
            }
            if !repay1.is_zero() {
                coins.push(Coin {
                    denom: plan.denom1,
                    amount: repay1,
                });
            }

            let mut resp = Response::new()
                .add_attribute("action", "flash_callback")
                .add_attribute("repay0", repay0)
                .add_attribute("repay1", repay1)
                .add_attribute("via_submsg", plan.repay_via_submsg.to_string());
            if !coins.is_empty() {
                let bank = BankMsg::Send {
                    to_address: pool,
                    amount: coins,
                };
                if plan.repay_via_submsg {
                    resp = resp.add_submessage(SubMsg::reply_on_success(bank, REPLY_REPAY));
                } else {
                    resp = resp.add_message(bank);
                }
            }
            Ok(resp)
        }
    }
}

#[entry_point]
pub fn reply(_deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
    // Only the repayment submessage routes here. Nothing to do — its bank send
    // has already settled by the time this reply runs (and, in turn, before the
    // pool's own `reply_flash` queries the balance).
    if msg.id != REPLY_REPAY {
        return Err(cosmwasm_std::StdError::generic_err("unknown reply id"));
    }
    Ok(Response::new().add_attribute("action", "repay_submsg_reply"))
}

#[entry_point]
pub fn query(_deps: Deps, _env: Env, _msg: Empty) -> StdResult<Binary> {
    Ok(Binary::default())
}

#[cw_serde]
pub struct Empty {}

//! Test-only adversarial CW20 used by the CLMM "blast-radius" integration test.
//!
//! It speaks enough of the CW20 wire protocol for `choice_clmm_pool` /
//! `choice_clmm_manager` to treat it as a normal token (`Transfer`,
//! `TransferFrom`, `IncreaseAllowance`, `Balance`/`TokenInfo` queries), but its
//! behaviour is attacker-controlled via [`Mode`] and an optional reentry hook:
//!
//!   * `Honest`            — behaves like a vanilla CW20.
//!   * `FeeOnTransfer{bps}`— every transfer delivers `amount - amount*bps/1e4`
//!     to the recipient and burns the rest. On the INBOUND leg (the pool pulling
//!     a deposit via `TransferFrom`) this makes the pool physically receive less
//!     than it credits — the classic over-credit vector.
//!   * `RevertOnTransfer`  — outbound `Transfer` always errors (a blacklist /
//!     pausable-token griefing the pool's payout leg). `TransferFrom` still
//!     works so deposits can be set up first.
//!
//! The optional reentry hook appends a `WasmMsg::Execute(target, msg)` AFTER the
//! balance move on `Transfer` and/or `TransferFrom`, letting the test re-enter
//! the pool mid-operation to probe the checks-effects-interactions ordering.
//!
//! This contract is NEVER deployed; it exists only to produce
//! `malicious_cw20.wasm` for tests.

use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{
    entry_point, to_json_binary, Addr, Binary, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Response, StdError, StdResult, Uint128, WasmMsg,
};
use cw20::{BalanceResponse, TokenInfoResponse};
use cw_storage_plus::{Item, Map};

#[cw_serde]
pub enum Mode {
    Honest,
    /// Burn `bps/10_000` of every transferred amount; deliver the rest.
    FeeOnTransfer {
        bps: u16,
    },
    /// Outbound `Transfer` always reverts (deposits via `TransferFrom` still work).
    RevertOnTransfer,
}

/// Re-enter `contract` with `msg` after a balance move, to probe CEI ordering.
#[cw_serde]
pub struct ReentryPlan {
    pub contract: String,
    pub msg: Binary,
    pub on_transfer: bool,
    pub on_transfer_from: bool,
}

#[cw_serde]
pub struct InstantiateMsg {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub initial_balances: Vec<(String, Uint128)>,
    pub mode: Mode,
}

#[cw_serde]
pub enum ExecuteMsg {
    // --- standard CW20 subset the pool/manager use ---
    Transfer {
        recipient: String,
        amount: Uint128,
    },
    TransferFrom {
        owner: String,
        recipient: String,
        amount: Uint128,
    },
    IncreaseAllowance {
        spender: String,
        amount: Uint128,
        expires: Option<cw_utils::Expiration>,
    },
    // --- adversarial controls (test-only) ---
    SetMode {
        mode: Mode,
    },
    SetReentry {
        plan: Option<ReentryPlan>,
    },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    #[returns(BalanceResponse)]
    Balance { address: String },
    #[returns(TokenInfoResponse)]
    TokenInfo {},
}

const MODE: Item<Mode> = Item::new("mode");
const REENTRY: Item<ReentryPlan> = Item::new("reentry");
const BALANCES: Map<&Addr, Uint128> = Map::new("balances");
const ALLOWANCES: Map<(&Addr, &Addr), Uint128> = Map::new("allowances");
const TOTAL: Item<Uint128> = Item::new("total");
const META: Item<(String, String, u8)> = Item::new("meta");

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    MODE.save(deps.storage, &msg.mode)?;
    META.save(deps.storage, &(msg.name, msg.symbol, msg.decimals))?;
    let mut total = Uint128::zero();
    for (addr, amount) in msg.initial_balances {
        let a = deps.api.addr_validate(&addr)?;
        BALANCES.save(deps.storage, &a, &amount)?;
        total = total.checked_add(amount)?;
    }
    TOTAL.save(deps.storage, &total)?;
    Ok(Response::new())
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Transfer { recipient, amount } => {
            let mode = MODE.load(deps.storage)?;
            if matches!(mode, Mode::RevertOnTransfer) {
                return Err(StdError::generic_err("malicious: transfer blocked"));
            }
            let recipient = deps.api.addr_validate(&recipient)?;
            move_funds(deps.storage, &info.sender, &recipient, amount, &mode)?;
            Ok(reentry(deps, "transfer", |p| p.on_transfer)?.add_attribute("action", "transfer"))
        }
        ExecuteMsg::TransferFrom {
            owner,
            recipient,
            amount,
        } => {
            let mode = MODE.load(deps.storage)?;
            let owner = deps.api.addr_validate(&owner)?;
            let recipient = deps.api.addr_validate(&recipient)?;
            // Spend allowance (owner -> info.sender).
            let key = (&owner, &info.sender);
            let allowed = ALLOWANCES.may_load(deps.storage, key)?.unwrap_or_default();
            let new_allowance = allowed
                .checked_sub(amount)
                .map_err(|_| StdError::generic_err("malicious: insufficient allowance"))?;
            ALLOWANCES.save(deps.storage, key, &new_allowance)?;
            move_funds(deps.storage, &owner, &recipient, amount, &mode)?;
            Ok(reentry(deps, "transfer_from", |p| p.on_transfer_from)?
                .add_attribute("action", "transfer_from"))
        }
        ExecuteMsg::IncreaseAllowance {
            spender,
            amount,
            expires: _,
        } => {
            let spender = deps.api.addr_validate(&spender)?;
            let key = (&info.sender, &spender);
            let cur = ALLOWANCES.may_load(deps.storage, key)?.unwrap_or_default();
            ALLOWANCES.save(deps.storage, key, &cur.checked_add(amount)?)?;
            Ok(Response::new().add_attribute("action", "increase_allowance"))
        }
        ExecuteMsg::SetMode { mode } => {
            MODE.save(deps.storage, &mode)?;
            Ok(Response::new().add_attribute("action", "set_mode"))
        }
        ExecuteMsg::SetReentry { plan } => {
            match plan {
                Some(p) => REENTRY.save(deps.storage, &p)?,
                None => REENTRY.remove(deps.storage),
            }
            Ok(Response::new().add_attribute("action", "set_reentry"))
        }
    }
}

/// Move `amount` out of `from`. With `FeeOnTransfer`, the recipient is credited
/// `amount - fee` and `fee` is burned (total supply shrinks); `from` is always
/// debited the full `amount`. This is what makes the pool over-credit on the
/// inbound (`TransferFrom`) leg.
fn move_funds(
    storage: &mut dyn cosmwasm_std::Storage,
    from: &Addr,
    to: &Addr,
    amount: Uint128,
    mode: &Mode,
) -> StdResult<()> {
    let from_bal = BALANCES.may_load(storage, from)?.unwrap_or_default();
    let from_new = from_bal
        .checked_sub(amount)
        .map_err(|_| StdError::generic_err("malicious: insufficient balance"))?;
    BALANCES.save(storage, from, &from_new)?;

    let fee = match mode {
        Mode::FeeOnTransfer { bps } => amount.multiply_ratio(*bps as u128, 10_000u128),
        _ => Uint128::zero(),
    };
    let net = amount.checked_sub(fee)?;

    let to_bal = BALANCES.may_load(storage, to)?.unwrap_or_default();
    BALANCES.save(storage, to, &to_bal.checked_add(net)?)?;

    if !fee.is_zero() {
        let total = TOTAL.load(storage)?;
        TOTAL.save(storage, &total.checked_sub(fee)?)?;
    }
    Ok(())
}

/// Build a response, appending the reentry message when the plan opts into this
/// trigger. The reentry executes (in the same tx) AFTER this contract's balance
/// move has been committed — exactly the window a re-entrant token would use.
fn reentry(
    deps: DepsMut,
    _trigger: &str,
    want: impl Fn(&ReentryPlan) -> bool,
) -> StdResult<Response> {
    let mut resp = Response::new();
    if let Some(plan) = REENTRY.may_load(deps.storage)? {
        if want(&plan) {
            resp = resp.add_message(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: plan.contract,
                msg: plan.msg,
                funds: vec![],
            }));
        }
    }
    Ok(resp)
}

#[entry_point]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Balance { address } => {
            let a = deps.api.addr_validate(&address)?;
            let balance = BALANCES.may_load(deps.storage, &a)?.unwrap_or_default();
            to_json_binary(&BalanceResponse { balance })
        }
        QueryMsg::TokenInfo {} => {
            let (name, symbol, decimals) = META.load(deps.storage)?;
            to_json_binary(&TokenInfoResponse {
                name,
                symbol,
                decimals,
                total_supply: TOTAL.load(deps.storage)?,
            })
        }
    }
}

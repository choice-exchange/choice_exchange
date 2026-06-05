use std::convert::TryFrom;

use cosmwasm_std::{
    to_json_binary, Addr, CosmosMsg, DepsMut, Env, MessageInfo, QuerierWrapper, Reply, Response,
    StdError, SubMsg, Uint128, Uint256, WasmMsg,
};
use cw20::{BalanceResponse, Cw20QueryMsg};

use crate::core::oracle::get_dynamic_fee;
use crate::error::ContractError;
use crate::state::{
    PendingFlash, FEE_GROWTH_GLOBAL_0, FEE_GROWTH_GLOBAL_1, PENDING_FLASH, POOL_CONFIG, POOL_STATE,
    PROTOCOL_FEES_0, PROTOCOL_FEES_1, PROTOCOL_FEE_CONFIG, REENTRANCY_LOCK,
};

use choice_clmm_common::pool::FlashCallbackMsg;
use choice_clmm_common::types::AssetInfo;
use choice_clmm_math::full_math::{mul_div, mul_div_round_up};
use choice_clmm_math::swap_math::FEE_DENOMINATOR;

/// Reply id for the flash-loan borrower callback.
pub const REPLY_FLASH: u64 = 100;

/// Flash fee for `amount` at `fee_pips`, rounded UP (in the pool's favor).
fn flash_fee(amount: Uint128, fee_pips: u32) -> Result<Uint128, ContractError> {
    if amount.is_zero() {
        return Ok(Uint128::zero());
    }
    let fee = mul_div_round_up(
        Uint256::from(amount),
        Uint256::from(fee_pips),
        Uint256::from(FEE_DENOMINATOR),
    )?;
    Uint128::try_from(fee).map_err(|_| {
        ContractError::Std(StdError::generic_err("flash: fee exceeds u128"))
    })
}

/// Query the pool's own balance of `asset`.
fn query_pool_balance(
    querier: &QuerierWrapper,
    pool: &Addr,
    asset: &AssetInfo,
) -> Result<Uint128, ContractError> {
    let amount = match asset {
        AssetInfo::NativeToken { denom } => querier.query_balance(pool, denom)?.amount,
        AssetInfo::Token { contract_addr } => {
            let resp: BalanceResponse = querier.query_wasm_smart(
                contract_addr,
                &Cw20QueryMsg::Balance {
                    address: pool.to_string(),
                },
            )?;
            resp.balance
        }
    };
    Ok(amount)
}

pub fn execute_flash(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    recipient: String,
    amount0: Uint128,
    amount1: Uint128,
    data: cosmwasm_std::Binary,
) -> Result<Response, ContractError> {
    if amount0.is_zero() && amount1.is_zero() {
        return Err(ContractError::ZeroAmount {});
    }

    let recipient = deps.api.addr_validate(&recipient)?;
    let config = POOL_CONFIG.load(deps.storage)?;
    let slot0 = POOL_STATE.load(deps.storage)?;

    // Flash does not move the price, so the read-only current fee is correct.
    let fee_pips = get_dynamic_fee(deps.storage, &env, slot0.sqrt_price)?;
    let fee0 = flash_fee(amount0, fee_pips)?;
    let fee1 = flash_fee(amount1, fee_pips)?;

    // Snapshot balances BEFORE lending so the reply can verify repayment by
    // delta. The loan + fee must bring each balance back to `snapshot + fee`.
    let pool = &env.contract.address;
    let snapshot0 = query_pool_balance(&deps.querier, pool, &config.token0)?;
    let snapshot1 = query_pool_balance(&deps.querier, pool, &config.token1)?;

    // Can't lend more than we hold.
    if amount0 > snapshot0 || amount1 > snapshot1 {
        return Err(ContractError::InvalidFunds {
            reason: "flash amount exceeds pool balance".to_string(),
        });
    }

    // Acquire the reentrancy lock; cleared in `reply_flash`. (The central guard
    // in `execute` already asserted it was not held.)
    REENTRANCY_LOCK.save(deps.storage, &true)?;
    PENDING_FLASH.save(
        deps.storage,
        &PendingFlash {
            amount0,
            amount1,
            fee0,
            fee1,
            snapshot0,
            snapshot1,
        },
    )?;

    // Lend the requested tokens, then call the borrower. Loan transfers are
    // ordered before the callback submessage so the funds arrive first.
    let mut messages: Vec<CosmosMsg> = vec![];
    if !amount0.is_zero() {
        messages.push(config.token0.transfer_msg(recipient.as_str(), amount0)?);
    }
    if !amount1.is_zero() {
        messages.push(config.token1.transfer_msg(recipient.as_str(), amount1)?);
    }

    let callback = SubMsg::reply_on_success(
        WasmMsg::Execute {
            contract_addr: recipient.to_string(),
            msg: to_json_binary(&FlashCallbackMsg::FlashCallback { fee0, fee1, data })?,
            funds: vec![],
        },
        REPLY_FLASH,
    );

    Ok(Response::new()
        .add_messages(messages)
        .add_submessage(callback)
        .add_attribute("action", "flash")
        .add_attribute("recipient", recipient)
        .add_attribute("amount0", amount0)
        .add_attribute("amount1", amount1)
        .add_attribute("fee0", fee0)
        .add_attribute("fee1", fee1))
}

pub fn reply_flash(deps: DepsMut, env: Env, _msg: Reply) -> Result<Response, ContractError> {
    let pending = PENDING_FLASH.load(deps.storage)?;
    PENDING_FLASH.remove(deps.storage);
    let config = POOL_CONFIG.load(deps.storage)?;
    let pool = &env.contract.address;

    // Verify repayment by balance delta for each borrowed token.
    let required0 = pending.snapshot0 + pending.fee0;
    let required1 = pending.snapshot1 + pending.fee1;

    if !pending.amount0.is_zero() || !pending.fee0.is_zero() {
        let bal0 = query_pool_balance(&deps.querier, pool, &config.token0)?;
        if bal0 < required0 {
            return Err(ContractError::FlashNotRepaid {
                token: 0,
                required: required0.to_string(),
                actual: bal0.to_string(),
            });
        }
    }
    if !pending.amount1.is_zero() || !pending.fee1.is_zero() {
        let bal1 = query_pool_balance(&deps.querier, pool, &config.token1)?;
        if bal1 < required1 {
            return Err(ContractError::FlashNotRepaid {
                token: 1,
                required: required1.to_string(),
                actual: bal1.to_string(),
            });
        }
    }

    // Accrue the flash fees through the same LP/protocol carve as swaps.
    let (fp0, fp1) = PROTOCOL_FEE_CONFIG
        .may_load(deps.storage)?
        .map(|c| (c.fee_protocol_0, c.fee_protocol_1))
        .unwrap_or((0, 0));
    let liquidity = POOL_STATE.load(deps.storage)?.liquidity.u128();

    accrue_flash_fee(deps.storage, true, pending.fee0, fp0, liquidity)?;
    accrue_flash_fee(deps.storage, false, pending.fee1, fp1, liquidity)?;

    // Release the reentrancy lock.
    REENTRANCY_LOCK.save(deps.storage, &false)?;

    Ok(Response::new()
        .add_attribute("action", "flash_repaid")
        .add_attribute("fee0", pending.fee0)
        .add_attribute("fee1", pending.fee1))
}

/// Split `fee` (of token0 if `is_token0`, else token1) between LPs and the
/// protocol and accrue each side. LP share goes to `fee_growth_global` when
/// liquidity > 0; if liquidity is 0 there is nothing to distribute to, so the
/// LP share is routed to the protocol accumulator (never lost, never a
/// divide-by-zero).
fn accrue_flash_fee(
    storage: &mut dyn cosmwasm_std::Storage,
    is_token0: bool,
    fee: Uint128,
    fee_protocol: u8,
    liquidity: u128,
) -> Result<(), ContractError> {
    if fee.is_zero() {
        return Ok(());
    }

    let mut protocol_part = if fee_protocol != 0 {
        fee / Uint128::from(fee_protocol)
    } else {
        Uint128::zero()
    };
    let lp_part = fee - protocol_part;

    if liquidity > 0 && !lp_part.is_zero() {
        let q128 = Uint256::one() << 128u32;
        let delta = mul_div(Uint256::from(lp_part), q128, Uint256::from(liquidity))?;
        let item = if is_token0 {
            &FEE_GROWTH_GLOBAL_0
        } else {
            &FEE_GROWTH_GLOBAL_1
        };
        let cur = item.may_load(storage)?.unwrap_or_default();
        item.save(storage, &cur.wrapping_add(delta))?;
    } else {
        // No active liquidity: the LP share has no recipient, route to protocol.
        protocol_part += lp_part;
    }

    if !protocol_part.is_zero() {
        let item = if is_token0 {
            &PROTOCOL_FEES_0
        } else {
            &PROTOCOL_FEES_1
        };
        let cur = item.may_load(storage)?.unwrap_or_default();
        let next = cur
            .checked_add(protocol_part)
            .map_err(|e| StdError::generic_err(e.to_string()))?;
        item.save(storage, &next)?;
    }

    Ok(())
}

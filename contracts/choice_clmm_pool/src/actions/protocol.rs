use cosmwasm_schema::cw_serde;
use cosmwasm_std::{
    to_json_binary, Coin, CosmosMsg, DepsMut, MessageInfo, Response, StdError, Uint128, WasmMsg,
};
use cw20::Cw20ExecuteMsg;

use crate::error::ContractError;
use crate::state::{
    PoolConfig, POOL_CONFIG, PROTOCOL_FEES_0, PROTOCOL_FEES_1, PROTOCOL_FEE_CONFIG,
};

use choice_clmm_common::factory::{ConfigResponse, QueryMsg as FactoryQueryMsg};
use choice_clmm_common::pool::ProtocolFeeConfig;
use choice_clmm_common::types::AssetInfo;

/// Minimal mirror of `choice_send_to_auction`'s `SendNative` entry. The pool's
/// `AssetInfo` serializes identically (externally-tagged `native_token`/`token`
/// with the same field names), so this deserializes correctly on the auction
/// contract without taking a dependency on the legacy `choice` package.
#[cw_serde]
enum BurnAuctionExecuteMsg {
    SendNative { asset: BurnAuctionAsset },
}

#[cw_serde]
struct BurnAuctionAsset {
    info: AssetInfo,
    amount: Uint128,
}

/// Assert the caller is the factory's `owner`. The pool stores the factory
/// address at instantiate; the owner is queried live so an ownership transfer on
/// the factory immediately takes effect here (matches V3's `factory.owner` gate
/// on `setFeeProtocol` / `collectProtocol`).
fn assert_factory_owner(
    deps: &DepsMut,
    config: &PoolConfig,
    info: &MessageInfo,
) -> Result<(), ContractError> {
    let factory_config: ConfigResponse = deps
        .querier
        .query_wasm_smart(config.factory.clone(), &FactoryQueryMsg::GetConfig {})?;
    if info.sender.as_str() != factory_config.owner {
        return Err(ContractError::Unauthorized {});
    }
    Ok(())
}

/// Validate a protocol-fee divisor: `0` (off) or `4..=10` (25%..10%), per V3.
fn validate_fee_protocol(n: u8) -> Result<(), ContractError> {
    if n == 0 || (4..=10).contains(&n) {
        Ok(())
    } else {
        Err(ContractError::InvalidConfig {
            reason: format!("fee_protocol must be 0 or in 4..=10, got {n}"),
        })
    }
}

pub fn execute_set_fee_protocol(
    deps: DepsMut,
    info: MessageInfo,
    fee_protocol_0: u8,
    fee_protocol_1: u8,
) -> Result<Response, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    assert_factory_owner(&deps, &config, &info)?;

    validate_fee_protocol(fee_protocol_0)?;
    validate_fee_protocol(fee_protocol_1)?;

    let mut pfc = PROTOCOL_FEE_CONFIG.load(deps.storage)?;
    pfc.fee_protocol_0 = fee_protocol_0;
    pfc.fee_protocol_1 = fee_protocol_1;
    PROTOCOL_FEE_CONFIG.save(deps.storage, &pfc)?;

    Ok(Response::new()
        .add_attribute("action", "set_fee_protocol")
        .add_attribute("fee_protocol_0", fee_protocol_0.to_string())
        .add_attribute("fee_protocol_1", fee_protocol_1.to_string()))
}

pub fn execute_update_protocol_fee_config(
    deps: DepsMut,
    info: MessageInfo,
    treasury: Option<String>,
    burn_auction: Option<String>,
    burn_share_bps: Option<u16>,
    clear_burn_auction: bool,
) -> Result<Response, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    assert_factory_owner(&deps, &config, &info)?;

    let mut pfc = PROTOCOL_FEE_CONFIG.load(deps.storage)?;

    if let Some(t) = treasury {
        pfc.treasury = deps.api.addr_validate(&t)?;
    }
    if clear_burn_auction {
        pfc.burn_auction = None;
    } else if let Some(a) = burn_auction {
        pfc.burn_auction = Some(deps.api.addr_validate(&a)?);
    }
    if let Some(bps) = burn_share_bps {
        if bps > 10_000 {
            return Err(ContractError::InvalidConfig {
                reason: "burn_share_bps must be <= 10000".to_string(),
            });
        }
        pfc.burn_share_bps = bps;
    }

    PROTOCOL_FEE_CONFIG.save(deps.storage, &pfc)?;

    Ok(Response::new()
        .add_attribute("action", "update_protocol_fee_config")
        .add_attribute("treasury", pfc.treasury.to_string())
        .add_attribute(
            "burn_auction",
            pfc.burn_auction
                .as_ref()
                .map(|a| a.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute("burn_share_bps", pfc.burn_share_bps.to_string()))
}

pub fn execute_collect_protocol(
    deps: DepsMut,
    info: MessageInfo,
    amount0_requested: Uint128,
    amount1_requested: Uint128,
) -> Result<Response, ContractError> {
    let config = POOL_CONFIG.load(deps.storage)?;
    assert_factory_owner(&deps, &config, &info)?;

    let pfc = PROTOCOL_FEE_CONFIG.load(deps.storage)?;

    let accrued_0 = PROTOCOL_FEES_0.may_load(deps.storage)?.unwrap_or_default();
    let accrued_1 = PROTOCOL_FEES_1.may_load(deps.storage)?.unwrap_or_default();

    let collect_0 = amount0_requested.min(accrued_0);
    let collect_1 = amount1_requested.min(accrued_1);

    // Decrement accrued balances by exactly what we route out.
    PROTOCOL_FEES_0.save(deps.storage, &(accrued_0 - collect_0))?;
    PROTOCOL_FEES_1.save(deps.storage, &(accrued_1 - collect_1))?;

    let mut messages: Vec<CosmosMsg> = vec![];
    split_to_messages(&config.token0, collect_0, &pfc, &mut messages)?;
    split_to_messages(&config.token1, collect_1, &pfc, &mut messages)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "collect_protocol")
        .add_attribute("collected_0", collect_0)
        .add_attribute("collected_1", collect_1)
        .add_attribute("burn_share_bps", pfc.burn_share_bps.to_string()))
}

/// Split `amount` of `token` between the burn auction (`burn_share_bps`) and the
/// treasury (remainder), appending the resulting transfer messages.
fn split_to_messages(
    token: &AssetInfo,
    amount: Uint128,
    pfc: &ProtocolFeeConfig,
    messages: &mut Vec<CosmosMsg>,
) -> Result<(), ContractError> {
    if amount.is_zero() {
        return Ok(());
    }

    // Burn share, floored. Only routed if a burn auction is configured.
    let burn_amt = match (&pfc.burn_auction, pfc.burn_share_bps) {
        (Some(_), bps) if bps > 0 => {
            amount
                .checked_mul(Uint128::from(bps))
                .map_err(|e| StdError::generic_err(e.to_string()))?
                / Uint128::from(10_000u16)
        }
        _ => Uint128::zero(),
    };
    let treasury_amt = amount
        .checked_sub(burn_amt)
        .map_err(|e| StdError::generic_err(format!("collect_protocol split underflow: {e}")))?;

    if !burn_amt.is_zero() {
        // Safe: burn_amt > 0 implies burn_auction is Some.
        let auction = pfc.burn_auction.as_ref().unwrap();
        messages.push(burn_auction_msg(token, burn_amt, auction.as_str())?);
    }
    if !treasury_amt.is_zero() {
        messages.push(token.transfer_msg(pfc.treasury.as_str(), treasury_amt)?);
    }

    Ok(())
}

/// Build the message that forwards `amount` of `token` to the burn-auction
/// contract. Native tokens use `SendNative` (funds attached); CW20 tokens use
/// the `Send` hook so the auction's `Receive` handles them.
fn burn_auction_msg(
    token: &AssetInfo,
    amount: Uint128,
    auction: &str,
) -> Result<CosmosMsg, ContractError> {
    let msg = match token {
        AssetInfo::NativeToken { denom } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: auction.to_string(),
            msg: to_json_binary(&BurnAuctionExecuteMsg::SendNative {
                asset: BurnAuctionAsset {
                    info: token.clone(),
                    amount,
                },
            })?,
            funds: vec![Coin {
                denom: denom.clone(),
                amount,
            }],
        }),
        AssetInfo::Token { contract_addr } => CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: contract_addr.clone(),
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: auction.to_string(),
                amount,
                msg: cosmwasm_std::Binary::default(),
            })?,
            funds: vec![],
        }),
    };
    Ok(msg)
}

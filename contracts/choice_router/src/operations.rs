use cosmwasm_std::{
    to_json_binary, Addr, Coin, CosmosMsg, Decimal, Deps, DepsMut, Env, MessageInfo, Response,
    StdError, StdResult, WasmMsg,
};

use crate::state::{Config, CONFIG};

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use choice::querier::{query_balance, query_pair_info, query_token_balance};
use choice::router::SwapOperation;
use cw20::Cw20ExecuteMsg;
use injective_cosmwasm::query::InjectiveQueryWrapper;

/// Execute swap operation
/// swap all offer asset to ask asset
pub fn execute_swap_operation(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    operation: SwapOperation,
    to: Option<String>,
    deadline: Option<u64>,
) -> StdResult<Response> {
    if env.contract.address != info.sender {
        return Err(StdError::generic_err("unauthorized"));
    }

    let messages: CosmosMsg = match operation {
        SwapOperation::Choice {
            offer_asset_info,
            ask_asset_info,
        } => {
            let config: Config = CONFIG.load(deps.as_ref().storage)?;
            let choice_factory = deps.api.addr_humanize(&config.choice_factory)?;
            let pair_info: PairInfo = query_pair_info(
                &deps.querier,
                choice_factory,
                &[offer_asset_info.clone(), ask_asset_info],
            )?;

            let amount = match offer_asset_info.clone() {
                AssetInfo::NativeToken { denom } => {
                    query_balance(&deps.querier, env.contract.address, denom)?
                }
                AssetInfo::Token { contract_addr } => query_token_balance(
                    &deps.querier,
                    deps.api.addr_validate(contract_addr.as_str())?,
                    env.contract.address,
                )?,
            };
            let offer_asset: Asset = Asset {
                info: offer_asset_info,
                amount,
            };

            asset_into_swap_msg(
                deps.as_ref(),
                Addr::unchecked(pair_info.contract_addr),
                offer_asset,
                None,
                to,
                deadline,
            )?
        }
    };

    Ok(Response::new().add_message(messages))
}

pub fn asset_into_swap_msg(
    _deps: Deps<InjectiveQueryWrapper>,
    pair_contract: Addr,
    offer_asset: Asset,
    max_spread: Option<Decimal>,
    to: Option<String>,
    deadline: Option<u64>,
) -> StdResult<CosmosMsg> {
    match offer_asset.info.clone() {
        AssetInfo::NativeToken { denom } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: pair_contract.to_string(),
            funds: vec![Coin {
                denom,
                amount: offer_asset.amount,
            }],
            msg: to_json_binary(&PairExecuteMsg::Swap {
                offer_asset,
                belief_price: None,
                max_spread,
                to,
                deadline,
            })?,
        })),
        AssetInfo::Token { contract_addr } => Ok(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            funds: vec![],
            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                contract: pair_contract.to_string(),
                amount: offer_asset.amount,
                msg: to_json_binary(&PairExecuteMsg::Swap {
                    offer_asset,
                    belief_price: None,
                    max_spread,
                    to,
                    deadline,
                })?,
            })?,
        })),
    }
}

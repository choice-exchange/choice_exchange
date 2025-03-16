use cosmwasm_std::{to_json_binary, Addr, QuerierWrapper, QueryRequest, StdResult, WasmQuery};
use cw20::{Cw20QueryMsg, MinterResponse};

// query the minter of a cw20 token
pub fn query_cw20_minter(querier: &QuerierWrapper, reward_token: Addr) -> StdResult<String> {
    let res: MinterResponse = querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: reward_token.to_string(),
        msg: to_json_binary(&Cw20QueryMsg::Minter {})?,
    }))?;

    Ok(res.minter)
}

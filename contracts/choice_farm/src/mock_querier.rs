use std::collections::HashMap;
use std::marker::PhantomData;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use cosmwasm_std::testing::{MockApi, MockQuerier, MockStorage, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    from_json, to_json_binary, Coin, ContractResult, Empty, OwnedDeps, Querier, QuerierResult,
    QueryRequest, SystemError, SystemResult, Uint128, WasmQuery,
};
use cw20::{BalanceResponse as Cw20BalanceResponse, Cw20QueryMsg, MinterResponse};

/// mock_dependencies is a drop-in replacement for cosmwasm_std::testing::mock_dependencies
/// this uses our CustomQuerier.
pub fn mock_dependencies(
    contract_balance: &[Coin],
) -> OwnedDeps<MockStorage, MockApi, WasmMockQuerier> {
    let custom_querier: WasmMockQuerier =
        WasmMockQuerier::new(MockQuerier::new(&[(MOCK_CONTRACT_ADDR, contract_balance)]));

    OwnedDeps {
        api: MockApi::default(),
        storage: MockStorage::default(),
        querier: custom_querier,
        custom_query_type: PhantomData,
    }
}

pub struct WasmMockQuerier {
    base: MockQuerier<Empty>,
    minter_querier: MinterQuerier,
    /// CW20 balance fixtures keyed by (cw20_contract, account) so tests that
    /// exercise the migrate-balance-query path can configure how much each
    /// CW20 reports the farm itself holds.
    cw20_balances: HashMap<(String, String), Uint128>,
}

#[derive(Clone, Default)]
pub struct MinterQuerier {
    minter_addr: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Minter {},
}

impl Querier for WasmMockQuerier {
    fn raw_query(&self, bin_request: &[u8]) -> QuerierResult {
        // MockQuerier doesn't support Custom, so we ignore it completely here
        let request: QueryRequest<Empty> = match from_json(bin_request) {
            Ok(v) => v,
            Err(e) => {
                return SystemResult::Err(SystemError::InvalidRequest {
                    error: format!("Parsing query request: {}", e),
                    request: bin_request.into(),
                })
            }
        };
        self.handle_query(&request)
    }
}

impl WasmMockQuerier {
    pub fn handle_query(&self, request: &QueryRequest<Empty>) -> QuerierResult {
        match &request {
            QueryRequest::Wasm(WasmQuery::Smart { contract_addr, msg }) => {
                // CW20 balance first — the farm's apply_migrate_staking path
                // queries the CW20 reward token for its own balance.
                if let Ok(Cw20QueryMsg::Balance { address }) = from_json(msg) {
                    let bal = self
                        .cw20_balances
                        .get(&(contract_addr.clone(), address))
                        .cloned()
                        .unwrap_or_default();
                    return SystemResult::Ok(ContractResult::from(to_json_binary(
                        &Cw20BalanceResponse { balance: bal },
                    )));
                }
                match from_json(msg) {
                    Ok(QueryMsg::Minter {}) => {
                        SystemResult::Ok(ContractResult::from(to_json_binary(&MinterResponse {
                            minter: self.minter_querier.minter_addr.clone(),
                            cap: None,
                        })))
                    }
                    _ => panic!("query not mocked"),
                }
            }
            _ => self.base.handle_query(request),
        }
    }
}

impl WasmMockQuerier {
    pub fn new(base: MockQuerier<Empty>) -> Self {
        WasmMockQuerier {
            base,
            minter_querier: MinterQuerier::default(),
            cw20_balances: HashMap::new(),
        }
    }

    /// Fixture: the CW20 at `cw20_contract` will report `amount` as the
    /// balance held by `account` when queried via `Cw20QueryMsg::Balance`.
    pub fn set_cw20_balance(
        &mut self,
        cw20_contract: impl Into<String>,
        account: impl Into<String>,
        amount: Uint128,
    ) {
        self.cw20_balances
            .insert((cw20_contract.into(), account.into()), amount);
    }

    /// Read what the fixture currently has stored for `(cw20_contract, account)`.
    /// Returns `Uint128::zero()` if unset.
    pub fn get_cw20_balance(&self, cw20_contract: &str, account: &str) -> Uint128 {
        self.cw20_balances
            .get(&(cw20_contract.to_string(), account.to_string()))
            .cloned()
            .unwrap_or_default()
    }
}

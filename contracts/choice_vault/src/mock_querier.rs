use cosmwasm_std::testing::{MockApi, MockQuerier, MockStorage};
use cosmwasm_std::{
    from_json, to_json_binary, Coin, ContractResult, OwnedDeps, Querier, QuerierResult,
    QueryRequest, SystemError, SystemResult, Uint128, WasmQuery,
};

use choice::staking::{QueryMsg as FarmQueryMsg, StakerInfoResponse};
use cw20::{BalanceResponse as Cw20BalanceResponse, Cw20QueryMsg};
use std::collections::HashMap;

/// A mock querier that can be customized with responses for specific contracts.
pub struct WasmMockQuerier {
    base: MockQuerier,
    staker_info_responses: HashMap<String, StakerInfoResponse>,
    token_balances: HashMap<String, HashMap<String, Uint128>>,
}

impl Querier for WasmMockQuerier {
    fn raw_query(&self, bin_request: &[u8]) -> QuerierResult {
        let request: QueryRequest<cosmwasm_std::Empty> = match from_json(bin_request) {
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
    pub fn handle_query(&self, request: &QueryRequest<cosmwasm_std::Empty>) -> QuerierResult {
        match &request {
            QueryRequest::Wasm(WasmQuery::Smart { contract_addr, msg }) => {
                // --- START CHANGES ---
                // Handle StakerInfo query for the farm contract
                if let Ok(FarmQueryMsg::StakerInfo { .. }) = from_json(msg) {
                    if let Some(response) = self.staker_info_responses.get(contract_addr) {
                        return SystemResult::Ok(ContractResult::Ok(
                            to_json_binary(response).unwrap(),
                        ));
                    }
                }
                // Handle CW20 Balance query
                if let Ok(Cw20QueryMsg::Balance { address }) = from_json(msg) {
                    let balances = self.token_balances.get(contract_addr);
                    let balance = balances
                        .and_then(|m| m.get(&address).copied())
                        .unwrap_or_default();
                    return SystemResult::Ok(ContractResult::Ok(
                        to_json_binary(&Cw20BalanceResponse { balance }).unwrap(),
                    ));
                }
                // --- END CHANGES ---
                self.base.handle_query(request)
            }
            _ => self.base.handle_query(request),
        }
    }

    // Method to set a mock response for a StakerInfo query
    pub fn with_staker_info(&mut self, farm_addr: String, response: StakerInfoResponse) {
        self.staker_info_responses.insert(farm_addr, response);
    }

    // --- ADD THIS NEW METHOD ---
    // Method to set a mock response for a CW20 Balance query
    pub fn with_token_balance(&mut self, token_addr: &str, account_addr: &str, balance: Uint128) {
        self.token_balances
            .entry(token_addr.to_string())
            .or_default()
            .insert(account_addr.to_string(), balance);
    }

    // Boilerplate for balance handling from MockQuerier
    pub fn with_balance(&mut self, balances: &[(String, &[Coin])]) {
        for (addr, balance) in balances {
            self.base
                .bank
                .update_balance(addr.clone(), balance.to_vec());
        }
    }
}

// Function to create dependencies with our custom querier
pub fn mock_dependencies() -> OwnedDeps<MockStorage, MockApi, WasmMockQuerier> {
    OwnedDeps {
        storage: MockStorage::default(),
        api: MockApi::default(),
        querier: WasmMockQuerier {
            base: MockQuerier::default(),
            staker_info_responses: HashMap::new(),
            token_balances: HashMap::new(),
        },
        custom_query_type: std::marker::PhantomData,
    }
}

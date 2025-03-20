use cosmwasm_std::testing::{MockApi, MockQuerier, MockStorage, MOCK_CONTRACT_ADDR};
use cosmwasm_std::{
    from_json, to_json_binary, Binary, Coin, ContractResult, Empty, OwnedDeps, Querier,
    QuerierResult, QueryRequest, SystemError, SystemResult, Uint128, WasmQuery,
};
use injective_cosmwasm::tokenfactory::response::TokenFactoryCreateDenomFeeResponse;
use injective_cosmwasm::{
    HandlesDenomSupplyQuery, HandlesFeeQuery, InjectiveQuery, InjectiveRoute,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::panic;

use crate::asset::{AssetInfo, PairInfo};
use crate::factory::{NativeTokenDecimalsResponse, QueryMsg as FactoryQueryMsg};
use crate::pair::QueryMsg as PairQueryMsg;
use crate::pair::{ReverseSimulationResponse, SimulationResponse};
use cw20::{BalanceResponse as Cw20BalanceResponse, Cw20QueryMsg, TokenInfoResponse};
use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::WasmMockQuerier as InjWasmMockQuerier;

use std::iter::FromIterator;

/// mock_dependencies is a drop-in replacement for cosmwasm_std::testing::mock_dependencies
/// this uses our CustomQuerier.
pub fn mock_dependencies(
    contract_balance: &[Coin],
) -> OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper> {
    let custom_querier: WasmMockQuerier =
        WasmMockQuerier::new(MockQuerier::new(&[(MOCK_CONTRACT_ADDR, contract_balance)]));

    OwnedDeps {
        storage: MockStorage::default(),
        api: MockApi::default(),
        querier: custom_querier,
        custom_query_type: PhantomData,
    }
}

pub struct WasmMockQuerier {
    base: MockQuerier,
    token_querier: TokenQuerier,
    choice_factory_querier: ChoiceFactoryQuerier,
    token_factory_denom_total_supply_handler: Option<Box<dyn HandlesDenomSupplyQuery>>,
    token_factory_denom_creation_fee_handler: Option<Box<dyn HandlesFeeQuery>>,
    inj: InjWasmMockQuerier,
}

#[derive(Clone, Default)]
pub struct TokenQuerier {
    // this lets us iterate over all pairs that match the first string
    balances: HashMap<String, HashMap<String, Uint128>>,
}

impl TokenQuerier {
    pub fn new(balances: &[(&String, &[(&String, &Uint128)])]) -> Self {
        TokenQuerier {
            balances: balances_to_map(balances),
        }
    }
}

pub(crate) fn balances_to_map(
    balances: &[(&String, &[(&String, &Uint128)])],
) -> HashMap<String, HashMap<String, Uint128>> {
    let mut balances_map: HashMap<String, HashMap<String, Uint128>> = HashMap::new();
    for (contract_addr, balances) in balances.iter() {
        let mut contract_balances_map: HashMap<String, Uint128> = HashMap::new();
        for (addr, balance) in balances.iter() {
            contract_balances_map.insert(addr.to_string(), **balance);
        }

        balances_map.insert(contract_addr.to_string(), contract_balances_map);
    }
    balances_map
}

#[derive(Clone, Default)]
pub struct ChoiceFactoryQuerier {
    pairs: HashMap<String, PairInfo>,
    native_token_decimals: HashMap<String, u8>,
}

impl ChoiceFactoryQuerier {
    pub fn new(pairs: &[(&String, &PairInfo)], native_token_decimals: &[(String, u8)]) -> Self {
        ChoiceFactoryQuerier {
            pairs: pairs_to_map(pairs),
            native_token_decimals: native_token_decimals_to_map(native_token_decimals),
        }
    }
}

pub(crate) fn pairs_to_map(pairs: &[(&String, &PairInfo)]) -> HashMap<String, PairInfo> {
    let mut pairs_map: HashMap<String, PairInfo> = HashMap::new();
    for (key, pair) in pairs.iter() {
        let mut sort_key: Vec<char> = key.chars().collect();
        sort_key.sort_by(|a, b| b.cmp(a));
        pairs_map.insert(String::from_iter(sort_key.iter()), (**pair).clone());
    }
    pairs_map
}

pub(crate) fn native_token_decimals_to_map(
    native_token_decimals: &[(String, u8)],
) -> HashMap<String, u8> {
    let mut native_token_decimals_map: HashMap<String, u8> = HashMap::new();

    for (denom, decimals) in native_token_decimals.iter() {
        native_token_decimals_map.insert(denom.to_string(), *decimals);
    }
    native_token_decimals_map
}

impl Querier for WasmMockQuerier {
    fn raw_query(&self, bin_request: &[u8]) -> QuerierResult {
        // MockQuerier doesn't support Custom, so we ignore it completely here
        let request: QueryRequest<InjectiveQueryWrapper> = match from_json(bin_request) {
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
    pub fn handle_query(&self, request: &QueryRequest<InjectiveQueryWrapper>) -> QuerierResult {
        let deps = mock_dependencies(&[]);
        match &request {
            QueryRequest::Wasm(WasmQuery::Smart { contract_addr, msg }) => match from_json(msg) {
                Ok(FactoryQueryMsg::Pair { asset_infos }) => {
                    let key = [asset_infos[0].to_string(), asset_infos[1].to_string()].join("");
                    let mut sort_key: Vec<char> = key.chars().collect();
                    sort_key.sort_by(|a, b| b.cmp(a));
                    match self
                        .choice_factory_querier
                        .pairs
                        .get(&String::from_iter(sort_key.iter()))
                    {
                        Some(v) => SystemResult::Ok(ContractResult::Ok(to_json_binary(v).unwrap())),
                        None => SystemResult::Err(SystemError::InvalidRequest {
                            error: "No pair info exists".to_string(),
                            request: msg.as_slice().into(),
                        }),
                    }
                }
                Ok(FactoryQueryMsg::NativeTokenDecimals { denom }) => {
                    match self
                        .choice_factory_querier
                        .native_token_decimals
                        .get(&denom)
                    {
                        Some(decimals) => SystemResult::Ok(ContractResult::Ok(
                            to_json_binary(&NativeTokenDecimalsResponse {
                                decimals: *decimals,
                            })
                            .unwrap(),
                        )),
                        None => SystemResult::Err(SystemError::InvalidRequest {
                            error: "No decimal info exist".to_string(),
                            request: msg.as_slice().into(),
                        }),
                    }
                }
                _ => match from_json(msg) {
                    Ok(PairQueryMsg::Pair {}) => {
                        let pair_addr = deps.api.addr_make("pair0000").to_string();
                        let liquidity_token = deps.api.addr_make("liquidity0000").to_string();
                        let burn_address = deps.api.addr_make("burnaddr0000").to_string();
                        let fee_wallet_address = deps.api.addr_make("feeaddr0000").to_string();

                        SystemResult::Ok(ContractResult::from(to_json_binary(&PairInfo {
                            asset_infos: [
                                AssetInfo::NativeToken {
                                    denom: "inj".to_string(),
                                },
                                AssetInfo::NativeToken {
                                    denom: "inj".to_string(),
                                },
                            ],
                            asset_decimals: [6u8, 6u8],
                            contract_addr: pair_addr,
                            liquidity_token,
                            burn_address,
                            fee_wallet_address,
                        })))
                    }
                    Ok(PairQueryMsg::Simulation { offer_asset }) => SystemResult::Ok(
                        ContractResult::from(to_json_binary(&SimulationResponse {
                            return_amount: offer_asset.amount,
                            commission_amount: Uint128::zero(),
                            spread_amount: Uint128::zero(),
                        })),
                    ),
                    Ok(PairQueryMsg::ReverseSimulation { ask_asset }) => SystemResult::Ok(
                        ContractResult::from(to_json_binary(&ReverseSimulationResponse {
                            offer_amount: ask_asset.amount,
                            commission_amount: Uint128::zero(),
                            spread_amount: Uint128::zero(),
                        })),
                    ),
                    _ => match from_json(msg).unwrap() {
                        Cw20QueryMsg::TokenInfo {} => {
                            let balances: &HashMap<String, Uint128> =
                                match self.token_querier.balances.get(contract_addr) {
                                    Some(balances) => balances,
                                    None => {
                                        return SystemResult::Err(SystemError::InvalidRequest {
                                            error: format!(
                                                "No balance info exists for the contract {}",
                                                contract_addr
                                            ),
                                            request: msg.as_slice().into(),
                                        })
                                    }
                                };

                            let mut total_supply = Uint128::zero();

                            for balance in balances {
                                total_supply += *balance.1;
                            }

                            SystemResult::Ok(ContractResult::Ok(
                                to_json_binary(&TokenInfoResponse {
                                    name: "mAAPL".to_string(),
                                    symbol: "mAAPL".to_string(),
                                    decimals: 8,
                                    total_supply,
                                })
                                .unwrap(),
                            ))
                        }
                        Cw20QueryMsg::Balance { address } => {
                            let balances: &HashMap<String, Uint128> =
                                match self.token_querier.balances.get(contract_addr) {
                                    Some(balances) => balances,
                                    None => {
                                        return SystemResult::Err(SystemError::InvalidRequest {
                                            error: format!(
                                                "No balance info exists for the contract {}",
                                                contract_addr
                                            ),
                                            request: msg.as_slice().into(),
                                        })
                                    }
                                };

                            let balance = match balances.get(&address) {
                                Some(v) => *v,
                                None => {
                                    return SystemResult::Ok(ContractResult::Ok(
                                        to_json_binary(&Cw20BalanceResponse {
                                            balance: Uint128::zero(),
                                        })
                                        .unwrap(),
                                    ));
                                }
                            };

                            SystemResult::Ok(ContractResult::Ok(
                                to_json_binary(&Cw20BalanceResponse { balance }).unwrap(),
                            ))
                        }

                        _ => panic!("DO NOT ENTER HERE"),
                    },
                },
            },
            QueryRequest::Custom(custom) => {
                match custom {
                    // Match on our token factory total supply query variant
                    InjectiveQueryWrapper {
                        route: InjectiveRoute::Tokenfactory,
                        query_data: InjectiveQuery::TokenFactoryDenomTotalSupply { denom },
                    } => {
                        if let Some(handler) = &self.token_factory_denom_total_supply_handler {
                            // Call our handler to get the total supply
                            handler.handle(denom.clone())
                        } else {
                            SystemResult::Err(SystemError::UnsupportedRequest {
                                kind: "TokenFactoryDenomTotalSupply".to_string(),
                            })
                        }
                    }
                    InjectiveQueryWrapper {
                        route: InjectiveRoute::Tokenfactory,
                        query_data: InjectiveQuery::TokenFactoryDenomCreationFee {},
                    } => {
                        if let Some(handler) = &self.token_factory_denom_creation_fee_handler {
                            handler.handle()
                        } else {
                            SystemResult::Err(SystemError::UnsupportedRequest {
                                kind: "TokenFactoryDenomCreationFee".to_string(),
                            })
                        }
                    }
                    // Fallback: if it's a Custom query that we don't handle specially, try the base querier.
                    _ => self.inj.handle_query(request),
                }
            }
            _ => {
                // Convert the custom query into the default type by round-tripping through binary.
                let bin = match to_json_binary(request) {
                    Ok(b) => b,
                    Err(e) => {
                        return SystemResult::Err(SystemError::InvalidRequest {
                            error: format!("Failed to serialize custom query: {}", e),
                            request: Binary::default(),
                        })
                    }
                };
                let req_empty: QueryRequest<Empty> = match from_json(&bin) {
                    Ok(r) => r,
                    Err(e) => {
                        return SystemResult::Err(SystemError::InvalidRequest {
                            error: format!("Failed to deserialize query into Empty: {}", e),
                            request: bin.clone(),
                        })
                    }
                };
                self.base.handle_query(&req_empty)
            }
        }
    }
}

// Define a response type that matches what the token factory returns.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct TokenFactoryDenomSupplyResponse {
    pub total_supply: Uint128,
}

// Our mock handler stores a mapping of denom -> total_supply.
#[derive(Clone, Default)]
pub struct MockDenomSupplyHandler {
    pub supplies: HashMap<String, Uint128>,
}

impl HandlesDenomSupplyQuery for MockDenomSupplyHandler {
    fn handle(&self, denom: String) -> SystemResult<ContractResult<Binary>> {
        match self.supplies.get(&denom) {
            Some(total_supply) => {
                // Here, you may have a response type from Injective token factory,
                // for example:
                #[derive(Serialize, Deserialize, Clone, Debug)]
                pub struct TokenFactoryDenomSupplyResponse {
                    pub total_supply: Uint128,
                }
                let response = TokenFactoryDenomSupplyResponse {
                    total_supply: *total_supply,
                };
                let bin = match to_json_binary(&response) {
                    Ok(bin) => bin,
                    Err(e) => {
                        return SystemResult::Err(SystemError::InvalidRequest {
                            error: format!("Serialization error: {}", e),
                            request: Binary::default(),
                        })
                    }
                };
                SystemResult::Ok(ContractResult::Ok(bin))
            }
            None => SystemResult::Err(SystemError::InvalidRequest {
                error: format!("No supply info for denom: {}", denom),
                request: Binary::default(),
            }),
        }
    }
}

// Create a mock fee handler that stores fees in a map (denom -> fee).
#[derive(Clone, Default)]
pub struct MockFeeHandler {
    pub fees: HashMap<String, Uint128>,
}

impl HandlesFeeQuery for MockFeeHandler {
    fn handle(&self) -> SystemResult<ContractResult<Binary>> {
        let (denom, amount) = self
            .fees
            .iter()
            .next()
            .ok_or(SystemError::InvalidRequest {
                error: "No fee provided".to_string(),
                request: Binary::default(),
            })
            .unwrap();

        let response = TokenFactoryCreateDenomFeeResponse {
            fee: vec![Coin {
                denom: denom.clone(),
                amount: *amount,
            }],
        };

        let bin = to_json_binary(&response)
            .map_err(|e| SystemError::InvalidRequest {
                error: format!("Serialization error: {}", e),
                request: Binary::default(),
            })
            .unwrap();

        SystemResult::Ok(ContractResult::Ok(bin))
    }
}

impl WasmMockQuerier {
    pub fn new(base: MockQuerier) -> Self {
        WasmMockQuerier {
            base,
            token_querier: TokenQuerier::default(),
            choice_factory_querier: ChoiceFactoryQuerier::default(),
            token_factory_denom_total_supply_handler: None,
            token_factory_denom_creation_fee_handler: None,
            inj: InjWasmMockQuerier::default(),
        }
    }

    // configure the mint whitelist mock querier
    pub fn with_token_balances(&mut self, balances: &[(&String, &[(&String, &Uint128)])]) {
        self.token_querier = TokenQuerier::new(balances);
    }

    // configure the choice pair
    pub fn with_choice_factory(
        &mut self,
        pairs: &[(&String, &PairInfo)],
        native_token_decimals: &[(String, u8)],
    ) {
        self.choice_factory_querier = ChoiceFactoryQuerier::new(pairs, native_token_decimals);
    }

    pub fn with_balance(&mut self, balances: &[(&String, Vec<Coin>)]) {
        for (addr, balance) in balances {
            self.base
                .bank
                .update_balance(addr.to_string(), balance.clone());
        }
    }

    pub fn with_token_factory_denom_supply(&mut self, supplies: &[(&str, Uint128)]) {
        let mut supply_map = HashMap::new();
        for (denom, supply) in supplies.iter() {
            supply_map.insert(denom.to_string(), *supply);
        }
        self.token_factory_denom_total_supply_handler = Some(Box::new(MockDenomSupplyHandler {
            supplies: supply_map,
        }));
    }

    pub fn with_token_factory_denom_create_fee(&mut self, fees: &[(&str, Uint128)]) {
        let mut fee_map = HashMap::new();
        for (denom, fee) in fees.iter() {
            fee_map.insert(denom.to_string(), *fee);
        }
        self.token_factory_denom_creation_fee_handler =
            Some(Box::new(MockFeeHandler { fees: fee_map }));
    }
}

#[cfg(test)]
mod mock_exception {
    use cosmwasm_std::Binary;

    use super::*;

    #[test]
    fn raw_query_err() {
        let deps = mock_dependencies(&[]);
        assert_eq!(
            deps.querier.raw_query(&[]),
            SystemResult::Err(SystemError::InvalidRequest {
                error: "Parsing query request: Error parsing into type cosmwasm_std::query::QueryRequest<injective_cosmwasm::query::InjectiveQueryWrapper>: EOF while parsing a JSON value.".to_string(),
                request: Binary::new(vec![])
            })
        );
    }

    #[test]
    fn none_factory_pair_will_err() {
        let deps = mock_dependencies(&[]);

        let msg = to_json_binary(&FactoryQueryMsg::Pair {
            asset_infos: [
                AssetInfo::NativeToken {
                    denom: "inj".to_string(),
                },
                AssetInfo::NativeToken {
                    denom: "ulunc".to_string(),
                },
            ],
        })
        .unwrap();
        assert_eq!(
            deps.querier
                .handle_query(&QueryRequest::Wasm(WasmQuery::Smart {
                    contract_addr: "contract0000".to_string(),
                    msg: msg.clone()
                })),
            SystemResult::Err(SystemError::InvalidRequest {
                error: "No pair info exists".to_string(),
                request: msg
            })
        )
    }

    #[test]
    fn none_tokens_info_will_err() {
        let deps = mock_dependencies(&[]);

        let msg = to_json_binary(&Cw20QueryMsg::TokenInfo {}).unwrap();

        assert_eq!(
            deps.querier
                .handle_query(&QueryRequest::Wasm(WasmQuery::Smart {
                    contract_addr: "token0000".to_string(),
                    msg: msg.clone()
                })),
            SystemResult::Err(SystemError::InvalidRequest {
                error: "No balance info exists for the contract token0000".to_string(),
                request: msg
            })
        )
    }

    #[test]
    fn none_tokens_balance_will_err() {
        let deps = mock_dependencies(&[]);

        let msg = to_json_binary(&Cw20QueryMsg::Balance {
            address: "address0000".to_string(),
        })
        .unwrap();

        assert_eq!(
            deps.querier
                .handle_query(&QueryRequest::Wasm(WasmQuery::Smart {
                    contract_addr: "token0000".to_string(),
                    msg: msg.clone()
                })),
            SystemResult::Err(SystemError::InvalidRequest {
                error: "No balance info exists for the contract token0000".to_string(),
                request: msg
            })
        )
    }

    #[test]
    #[should_panic]
    fn none_tokens_minter_will_panic() {
        let deps = mock_dependencies(&[]);

        let msg = to_json_binary(&Cw20QueryMsg::Minter {}).unwrap();

        deps.querier
            .handle_query(&QueryRequest::Wasm(WasmQuery::Smart {
                contract_addr: "token0000".to_string(),
                msg,
            }));
    }
}

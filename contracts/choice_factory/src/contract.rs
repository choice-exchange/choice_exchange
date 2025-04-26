use choice::querier::{
    query_balance, query_pair_info_from_pair, query_token_factory_denom_create_fee,
};
#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coin, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Reply, ReplyOn, Response, StdError, StdResult, SubMsg, SubMsgResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::Cw20ExecuteMsg;

use crate::response::MsgInstantiateContractResponse;
use crate::state::{
    add_allow_native_token, pair_key, read_pairs, Config, TmpPairInfo, ALLOW_NATIVE_TOKENS, CONFIG,
    PAIRS, TMP_PAIR_INFO,
};

use choice::asset::{Asset, AssetInfo, AssetInfoRaw, PairInfo, PairInfoRaw};
use choice::factory::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, MigrateMsg, NativeTokenDecimalsResponse,
    PairsResponse, QueryMsg, UpdateConfigParams,
};
use choice::pair::{
    ExecuteMsg as PairExecuteMsg, InstantiateMsg as PairInstantiateMsg,
    MigrateMsg as PairMigrateMsg,
};
use choice::util::migrate_version;
use injective_cosmwasm::query::InjectiveQueryWrapper;
use protobuf::Message;

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:choice-factory";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const CREATE_PAIR_REPLY_ID: u64 = 1;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let config = Config {
        owner: deps.api.addr_canonicalize(info.sender.as_str())?,
        pair_code_id: msg.pair_code_id,

        burn_address: deps.api.addr_canonicalize(&msg.burn_address)?, // Store burn address
        fee_wallet_address: deps.api.addr_canonicalize(&msg.fee_wallet_address)?, // Store fee wallet address
        proposed_owner: None,
    };

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> StdResult<Response> {
    match msg {
        ExecuteMsg::UpdateConfig { params } => execute_update_config(deps, env, info, params),
        ExecuteMsg::CreatePair { assets } => execute_create_pair(deps, env, info, assets),
        ExecuteMsg::AddNativeTokenDecimals { denom, decimals } => {
            execute_add_native_token_decimals(deps, env, info, denom, decimals)
        }
        ExecuteMsg::MigratePair { contract, code_id } => {
            execute_migrate_pair(deps, env, info, contract, code_id)
        }
        ExecuteMsg::WithdrawNative { denom, amount } => {
            execute_withdraw_native(deps, env, info, denom, amount)
        }
        ExecuteMsg::ProposeNewOwner { new_owner } => {
            execute_propose_new_owner(deps, info, new_owner)
        }
        ExecuteMsg::AcceptOwnership => execute_accept_ownership(deps, info),
        ExecuteMsg::CancelOwnershipProposal => execute_cancel_ownership_proposal(deps, info),
    }
}

// Only owner can execute it
pub fn execute_update_config(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    params: UpdateConfigParams,
) -> StdResult<Response> {
    let mut config: Config = CONFIG.load(deps.storage)?;

    // permission check
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    if let Some(pair_code_id) = params.pair_code_id {
        config.pair_code_id = pair_code_id;
    }

    if let Some(burn_address) = params.burn_address {
        config.burn_address = deps.api.addr_canonicalize(&burn_address)?;
    }

    if let Some(fee_wallet_address) = params.fee_wallet_address {
        config.fee_wallet_address = deps.api.addr_canonicalize(&fee_wallet_address)?;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "update_config"))
}

// Anyone can execute it to create swap pair
pub fn execute_create_pair(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    assets: [Asset; 2],
) -> StdResult<Response> {
    let config: Config = CONFIG.load(deps.storage)?;

    if assets[0].info == assets[1].info {
        return Err(StdError::generic_err("same asset"));
    }

    let asset_1_decimal = match assets[0]
        .info
        .query_decimals(env.contract.address.clone(), &deps.querier)
    {
        Ok(decimal) => decimal,
        Err(_) => return Err(StdError::generic_err("asset1 is invalid")),
    };

    let asset_2_decimal = match assets[1]
        .info
        .query_decimals(env.contract.address.clone(), &deps.querier)
    {
        Ok(decimal) => decimal,
        Err(_) => return Err(StdError::generic_err("asset2 is invalid")),
    };

    let raw_assets = [assets[0].to_raw(deps.api)?, assets[1].to_raw(deps.api)?];

    let asset_infos = [assets[0].info.clone(), assets[1].info.clone()];
    let raw_infos = [
        asset_infos[0].to_raw(deps.api)?,
        asset_infos[1].to_raw(deps.api)?,
    ];

    let asset_decimals = [asset_1_decimal, asset_2_decimal];

    let pair_key = pair_key(&raw_infos);
    if let Ok(Some(_)) = PAIRS.may_load(deps.storage, &pair_key) {
        return Err(StdError::generic_err("Pair already exists"));
    }

    TMP_PAIR_INFO.save(
        deps.storage,
        &TmpPairInfo {
            pair_key,
            assets: raw_assets,
            asset_decimals,
            sender: info.sender,
        },
    )?;

    let creation_fee: Vec<Coin> = query_token_factory_denom_create_fee(&deps.querier).unwrap();

    // Check that the sender provided at least the required funds for each coin in the creation fee.
    for fee in creation_fee.iter() {
        let coin_opt = info.funds.iter().find(|c| c.denom == fee.denom);
        if coin_opt.is_none() || coin_opt.unwrap().amount < fee.amount {
            return Err(StdError::generic_err(format!(
                "Insufficient funds: require at least {} {}",
                fee.amount, fee.denom
            )));
        }
    }

    Ok(Response::new()
        .add_attributes(vec![
            ("action", "create_pair"),
            ("pair", &format!("{}-{}", assets[0].info, assets[1].info)),
        ])
        .add_submessage(SubMsg {
            id: CREATE_PAIR_REPLY_ID,
            payload: Binary::default(),
            gas_limit: None,
            msg: CosmosMsg::Wasm(WasmMsg::Instantiate {
                code_id: config.pair_code_id,
                funds: creation_fee,
                admin: Some(env.contract.address.to_string()),
                label: "pair".to_string(),
                msg: to_json_binary(&PairInstantiateMsg {
                    asset_infos,
                    asset_decimals,
                    burn_address: deps.api.addr_humanize(&config.burn_address)?.to_string(), // Pass burn address
                    fee_wallet_address: deps
                        .api
                        .addr_humanize(&config.fee_wallet_address)?
                        .to_string(), // Pass fee wallet address
                })?,
            }),
            reply_on: ReplyOn::Success,
        }))
}

pub fn execute_add_native_token_decimals(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    denom: String,
    decimals: u8,
) -> StdResult<Response> {
    let config: Config = CONFIG.load(deps.storage)?;

    // permission check
    if denom.starts_with("factory/") {
        // Expect format: "factory/owneraddr/subdenom"
        let parts: Vec<&str> = denom.split('/').collect();
        if parts.len() < 3 {
            return Err(StdError::generic_err("invalid denom format"));
        }
        // parts[1] is the owner address part.
        let owner_in_denom = parts[1];
        let sender_canonical = deps.api.addr_canonicalize(info.sender.as_str())?;
        let owner_in_denom_canonical = deps.api.addr_canonicalize(owner_in_denom)?;
        if sender_canonical != owner_in_denom_canonical && sender_canonical != config.owner {
            return Err(StdError::generic_err(
                "unauthorized: sender does not match owner in denom and is not contract owner",
            ));
        }
    } else {
        // For non-factory denoms, require that the sender is the contract owner.
        if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
            return Err(StdError::generic_err("unauthorized"));
        }
    }

    let balance = query_balance(&deps.querier, env.contract.address, denom.to_string())?;
    if balance.is_zero() {
        return Err(StdError::generic_err(
            "a balance greater than zero is required by the factory for verification",
        ));
    }

    add_allow_native_token(deps.storage, denom.to_string(), decimals)?;

    Ok(Response::new().add_attributes(vec![
        ("action", "add_allow_native_token"),
        ("denom", &denom),
        ("decimals", &decimals.to_string()),
    ]))
}

pub fn execute_withdraw_native(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    denom: String,
    amount: Uint128,
) -> Result<Response, StdError> {
    // Only owner can withdraw
    let config = CONFIG.load(deps.storage)?;
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    // Send the specified amount to the owner
    let bank_msg = BankMsg::Send {
        to_address: info.sender.to_string(),
        amount: vec![Coin { denom, amount }],
    };

    Ok(Response::new()
        .add_message(bank_msg)
        .add_attribute("action", "withdraw_native")
        .add_attribute("owner", info.sender))
}

pub fn execute_propose_new_owner(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_owner: String,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;

    // Only current owner can propose
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    let validated = deps.api.addr_validate(&new_owner)?;
    config.proposed_owner = Some(validated);
    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "propose_new_owner")
        .add_attribute("proposed_owner", new_owner))
}

pub fn execute_accept_ownership(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;

    match config.proposed_owner {
        Some(proposed) if proposed == info.sender => {
            config.owner = deps.api.addr_canonicalize(info.sender.as_str())?;
            config.proposed_owner = None; // clear proposed owner
            CONFIG.save(deps.storage, &config)?;

            Ok(Response::new()
                .add_attribute("action", "accept_ownership")
                .add_attribute("new_owner", info.sender.to_string()))
        }
        _ => Err(StdError::generic_err("No ownership proposal for you")),
    }
}

pub fn execute_cancel_ownership_proposal(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;

    // Only current owner can cancel
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    // Clear the proposed owner
    config.proposed_owner = None;

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new()
        .add_attribute("action", "cancel_ownership_proposal")
        .add_attribute("owner", info.sender))
}

pub fn execute_migrate_pair(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    contract: String,
    code_id: Option<u64>,
) -> StdResult<Response> {
    let config: Config = CONFIG.load(deps.storage)?;

    // permission check
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("unauthorized"));
    }

    let code_id = code_id.unwrap_or(config.pair_code_id);

    Ok(
        Response::new().add_message(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: contract,
            new_code_id: code_id,
            msg: to_json_binary(&PairMigrateMsg {})?,
        })),
    )
}

/// This just stores the result for future query
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut<InjectiveQueryWrapper>, env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id != CREATE_PAIR_REPLY_ID {
        return Err(StdError::generic_err("invalid reply msg"));
    }

    let tmp_pair_info = TMP_PAIR_INFO.load(deps.storage)?;

    let sub_msg_response = match msg.result {
        SubMsgResult::Ok(resp) => resp,
        SubMsgResult::Err(err) => {
            return Err(StdError::generic_err(format!("Submessage error: {}", err)))
        }
    };

    // Use msg_responses if available, otherwise fall back to data
    let data_bytes: Binary = if !sub_msg_response.msg_responses.is_empty() {
        sub_msg_response.msg_responses[0].value.clone()
    } else {
        return Err(StdError::generic_err(
            "no data or msg_responses found in submessage response",
        ));
    };

    let res: MsgInstantiateContractResponse = Message::parse_from_bytes(data_bytes.as_slice())
        .map_err(|_| {
            StdError::parse_err("MsgInstantiateContractResponse", "failed to parse data")
        })?;

    let pair_contract = &res.address;
    let pair_info = query_pair_info_from_pair(&deps.querier, Addr::unchecked(pair_contract))?;

    let raw_infos = [
        tmp_pair_info.assets[0].info.clone(),
        tmp_pair_info.assets[1].info.clone(),
    ];

    let factory_config: Config = CONFIG.load(deps.storage)?;
    let burn_address = factory_config.burn_address.clone();
    let fee_wallet_address = factory_config.fee_wallet_address.clone();

    PAIRS.save(
        deps.storage,
        &tmp_pair_info.pair_key,
        &PairInfoRaw {
            liquidity_token: pair_info.liquidity_token.clone(),
            contract_addr: deps.api.addr_canonicalize(pair_contract)?,
            asset_infos: raw_infos,
            asset_decimals: tmp_pair_info.asset_decimals,
            burn_address,       // Add burn address
            fee_wallet_address, // Add fee wallet address
        },
    )?;

    let mut messages: Vec<CosmosMsg> = vec![];
    if !tmp_pair_info.assets[0].amount.is_zero() || !tmp_pair_info.assets[1].amount.is_zero() {
        let assets = [
            tmp_pair_info.assets[0].to_normal(deps.api)?,
            tmp_pair_info.assets[1].to_normal(deps.api)?,
        ];
        let mut funds: Vec<Coin> = vec![];
        for asset in tmp_pair_info.assets.iter() {
            if let AssetInfoRaw::NativeToken { denom, .. } = &asset.info {
                funds.push(coin(asset.amount.u128(), denom.to_string()));
            } else if let AssetInfoRaw::Token { contract_addr } = &asset.info {
                let contract_addr = deps.api.addr_humanize(contract_addr)?.to_string();
                messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: contract_addr.to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::IncreaseAllowance {
                        spender: pair_contract.to_string(),
                        amount: asset.amount,
                        expires: None,
                    })?,
                    funds: vec![],
                }));
                messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr,
                    msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                        owner: tmp_pair_info.sender.to_string(),
                        recipient: env.contract.address.to_string(),
                        amount: asset.amount,
                    })?,
                    funds: vec![],
                }));
            }
        }

        funds.sort_by(|a, b| a.denom.cmp(&b.denom));
        messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: pair_contract.to_string(),
            msg: to_json_binary(&PairExecuteMsg::ProvideLiquidity {
                assets,
                receiver: Some(tmp_pair_info.sender.to_string()),
                deadline: None,
                slippage_tolerance: None,
            })?,
            funds,
        }));
    }

    Ok(Response::new()
        .add_attributes(vec![
            ("pair_contract_addr", pair_contract),
            ("liquidity_token_addr", &pair_info.liquidity_token),
        ])
        .add_messages(messages))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::Pair { asset_infos } => to_json_binary(&query_pair(deps, asset_infos)?),
        QueryMsg::Pairs { start_after, limit } => {
            to_json_binary(&query_pairs(deps, start_after, limit)?)
        }
        QueryMsg::NativeTokenDecimals { denom } => {
            to_json_binary(&query_native_token_decimal(deps, denom)?)
        }
    }
}

pub fn query_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<ConfigResponse> {
    let state: Config = CONFIG.load(deps.storage)?;
    let resp = ConfigResponse {
        owner: deps.api.addr_humanize(&state.owner)?.to_string(),
        pair_code_id: state.pair_code_id,

        burn_address: deps.api.addr_humanize(&state.burn_address)?.to_string(), // Return burn address
        fee_wallet_address: deps
            .api
            .addr_humanize(&state.fee_wallet_address)?
            .to_string(), // Return fee wallet address
    };

    Ok(resp)
}

pub fn query_pair(
    deps: Deps<InjectiveQueryWrapper>,
    asset_infos: [AssetInfo; 2],
) -> StdResult<PairInfo> {
    let pair_key = pair_key(&[
        asset_infos[0].to_raw(deps.api)?,
        asset_infos[1].to_raw(deps.api)?,
    ]);
    let pair_info: PairInfoRaw = PAIRS.load(deps.storage, &pair_key)?;
    pair_info.to_normal(deps.api)
}

pub fn query_pairs(
    deps: Deps<InjectiveQueryWrapper>,
    start_after: Option<[AssetInfo; 2]>,
    limit: Option<u32>,
) -> StdResult<PairsResponse> {
    let start_after = if let Some(start_after) = start_after {
        Some([
            start_after[0].to_raw(deps.api)?,
            start_after[1].to_raw(deps.api)?,
        ])
    } else {
        None
    };

    let pairs: Vec<PairInfo> = read_pairs(deps.storage, deps.api, start_after, limit)?;
    let resp = PairsResponse { pairs };

    Ok(resp)
}

pub fn query_native_token_decimal(
    deps: Deps<InjectiveQueryWrapper>,
    denom: String,
) -> StdResult<NativeTokenDecimalsResponse> {
    let decimals = ALLOW_NATIVE_TOKENS.load(deps.storage, denom.as_bytes())?;

    Ok(NativeTokenDecimalsResponse { decimals })
}

const TARGET_CONTRACT_VERSION: &str = "1.1.1";
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _msg: MigrateMsg,
) -> StdResult<Response> {
    migrate_version(
        deps,
        TARGET_CONTRACT_VERSION,
        CONTRACT_NAME,
        CONTRACT_VERSION,
    )?;

    Ok(Response::default())
}

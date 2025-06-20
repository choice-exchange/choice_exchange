use crate::error::ContractError;
use crate::state::PAIR_INFO;

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, from_json, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Decimal, Decimal256,
    Deps, DepsMut, Env, MessageInfo, Response, StdResult, Uint128, Uint256, WasmMsg,
};

use choice::asset::{Asset, AssetInfo, PairInfo, PairInfoRaw};
use choice::pair::{
    Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, PoolResponse, QueryMsg,
    ReverseSimulationResponse, SimulationResponse,
};
use choice::querier::query_token_factory_denom_total_supply;
use choice::util::migrate_version;
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};
use std::cmp::Ordering;
use std::convert::TryInto;
use std::ops::Mul;
use std::str::FromStr;

use choice::send_to_auction::ExecuteMsg as BurnAuctionExecuteMsg;

use injective_cosmwasm::msg::{
    create_burn_tokens_msg, create_mint_tokens_msg, create_new_denom_msg,
    create_set_token_metadata_msg,
};
use injective_cosmwasm::InjectiveMsgWrapper;

use injective_cosmwasm::query::InjectiveQueryWrapper;

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:choice-pair";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Commission rate == 0.3%
const COMMISSION_RATE: u64 = 3;

const MINIMUM_LIQUIDITY_AMOUNT: u128 = 1_000;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response<InjectiveMsgWrapper>> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let subdenom = "lp".to_string();
    let lp_denom = format!("factory/{}/{}", env.contract.address, subdenom);

    let pair_info = &PairInfoRaw {
        contract_addr: deps.api.addr_canonicalize(env.contract.address.as_str())?,
        liquidity_token: lp_denom.clone(),
        asset_infos: [
            msg.asset_infos[0].to_raw(deps.api)?,
            msg.asset_infos[1].to_raw(deps.api)?,
        ],
        asset_decimals: msg.asset_decimals,
        burn_address: deps.api.addr_canonicalize(&msg.burn_address)?,
        fee_wallet_address: deps.api.addr_canonicalize(&msg.fee_wallet_address)?,
    };

    PAIR_INFO.save(deps.storage, pair_info)?;

    let create_msg = create_new_denom_msg(env.contract.address.to_string(), subdenom.clone());

    let metadata_msg = create_set_token_metadata_msg(
        lp_denom.clone(),
        "choice liquidity token".to_string(),
        "uLP".to_string(),
        6,
    );

    Ok(Response::new()
        .add_messages(vec![create_msg, metadata_msg])
        .add_attribute("lp_denom", lp_denom))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::ProvideLiquidity {
            assets,
            receiver,
            deadline,
            slippage_tolerance,
        } => provide_liquidity(
            deps,
            env,
            info,
            assets,
            receiver,
            deadline,
            slippage_tolerance,
        ),

        ExecuteMsg::WithdrawLiquidity {
            amount,
            min_assets,
            deadline,
        } => {
            let sender_addr = info.sender.clone();
            withdraw_liquidity(deps, env, info, sender_addr, amount, min_assets, deadline)
        }

        ExecuteMsg::Swap {
            offer_asset,
            belief_price,
            max_spread,
            to,
            deadline,
        } => {
            if !offer_asset.is_native_token() {
                return Err(ContractError::Unauthorized {});
            }

            let to_addr = if let Some(to_addr) = to {
                Some(deps.api.addr_validate(&to_addr)?)
            } else {
                None
            };

            swap(
                deps,
                env,
                info.clone(),
                info.sender,
                offer_asset,
                belief_price,
                max_spread,
                to_addr,
                deadline,
            )
        }
    }
}

pub fn receive_cw20(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let contract_addr = info.sender.clone();

    match from_json(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Swap {
            belief_price,
            max_spread,
            to,
            deadline,
        }) => {
            // only asset contract can execute this message
            let mut authorized: bool = false;
            let config: PairInfoRaw = PAIR_INFO.load(deps.storage)?;
            let pools: [Asset; 2] =
                config.query_pools(&deps.querier, deps.api, env.contract.address.clone())?;

            for pool in pools.iter() {
                if let AssetInfo::Token { contract_addr, .. } = &pool.info {
                    if contract_addr == &info.sender.to_string() {
                        authorized = true;
                        break;
                    }
                }
            }

            if !authorized {
                return Err(ContractError::Unauthorized {});
            }

            let to_addr = if let Some(to_addr) = to {
                Some(deps.api.addr_validate(to_addr.as_str())?)
            } else {
                None
            };

            swap(
                deps,
                env,
                info,
                Addr::unchecked(cw20_msg.sender),
                Asset {
                    info: AssetInfo::Token {
                        contract_addr: contract_addr.to_string(),
                    },
                    amount: cw20_msg.amount,
                },
                belief_price,
                max_spread,
                to_addr,
                deadline,
            )
        }
        Err(err) => Err(ContractError::Std(err)),
    }
}

/// CONTRACT - should approve contract to use the amount of token
pub fn provide_liquidity(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    assets: [Asset; 2],
    receiver: Option<String>,
    deadline: Option<u64>,
    slippage_tolerance: Option<Decimal>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;

    for asset in assets.iter() {
        asset.assert_sent_native_token_balance(&info)?;
    }

    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;

    let mut pools: [Asset; 2] =
        pair_info.query_pools(&deps.querier, deps.api, env.contract.address.clone())?;

    let deposits: [Uint128; 2] = [
        assets
            .iter()
            .find(|a| a.info.equal(&pools[0].info))
            .map(|a| a.amount)
            .expect("Wrong asset info is given"),
        assets
            .iter()
            .find(|a| a.info.equal(&pools[1].info))
            .map(|a| a.amount)
            .expect("Wrong asset info is given"),
    ];

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    for (i, pool) in pools.iter_mut().enumerate() {
        if pool.is_native_token() {
            // If the asset is native token, balance is already increased
            // To calculated properly we should subtract user deposit from the pool
            pool.amount = pool.amount.checked_sub(deposits[i])?;
        }
    }

    let total_share: Uint128 =
        query_token_factory_denom_total_supply(&deps.querier, pair_info.liquidity_token.clone())
            .unwrap();

    let share: Uint128 = if total_share.is_zero() {
        // Initial share = collateral amount
        let deposit0: Uint256 = deposits[0].into();
        let deposit1: Uint256 = deposits[1].into();

        // Compute the square root of the product.
        let computed = Decimal256::from_ratio(deposit0.mul(deposit1), 1u8).sqrt();
        // Assume Decimal256 uses 18 decimals. Its internal representation of 1 is 1e18.
        // To get the integer value 1, we divide by 10^18.
        let scaling_factor = Uint256::from(1_000_000_000_000_000_000u128);
        let share: Uint128 = (computed.atomics() / scaling_factor)
            .try_into()
            .map_err(ContractError::ConversionOverflowError)?;

        // Mint the minimum liquidity tokens to lock forever (to protect the pair)
        messages.push(create_mint_tokens_msg(
            env.contract.address.clone(),
            Coin {
                denom: pair_info.liquidity_token.clone(),
                amount: MINIMUM_LIQUIDITY_AMOUNT.into(),
            },
            env.contract.address.to_string(),
        ));

        // Deduct the minimum liquidity amount and return the result.
        share
            .checked_sub(MINIMUM_LIQUIDITY_AMOUNT.into())
            .map_err(|_| ContractError::MinimumLiquidityAmountError {
                min_lp_token: MINIMUM_LIQUIDITY_AMOUNT.to_string(),
                given_lp: share.to_string(),
            })?
    } else {
        std::cmp::min(
            deposits[0].multiply_ratio(total_share, pools[0].amount),
            deposits[1].multiply_ratio(total_share, pools[1].amount),
        )
    };

    // prevent providing free token
    if share.is_zero() {
        return Err(ContractError::InvalidZeroAmount {});
    }

    // the total lp token cannot exceed the max value of a Uint128
    if total_share
        .checked_add(share)
        .is_err()                       
    {
        return Err(ContractError::LpSupplyOverflow{});  
    }

    // refund of remaining native token & desired of token
    let mut refund_assets: Vec<Asset> = vec![];
    for (i, pool) in pools.iter().enumerate() {
        let desired_amount = match total_share.is_zero() {
            true => deposits[i],
            false => {
                let mut desired_amount = pool.amount.multiply_ratio(share, total_share);
                if desired_amount.multiply_ratio(total_share, share) != pool.amount {
                    desired_amount += Uint128::from(1u8);
                }

                desired_amount
            }
        };

        let mut remain_amount = deposits[i] - desired_amount;

        // Override remain_amount to 0 if CW20
        if let AssetInfo::Token { .. } = &pool.info {
            remain_amount = Uint128::zero();
        }

        if let Some(slippage_tolerance) = slippage_tolerance {
            if remain_amount > deposits[i].mul_floor(slippage_tolerance) {
                return Err(ContractError::MaxSlippageAssertion {});
            }
        }

        refund_assets.push(Asset {
            info: pool.info.clone(),
            amount: remain_amount,
        });

        if let AssetInfo::NativeToken { denom, .. } = &pool.info {
            if !remain_amount.is_zero() {
                messages.push(CosmosMsg::Bank(BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: coins(remain_amount.u128(), denom),
                }))
            }
        } else if let AssetInfo::Token { contract_addr, .. } = &pool.info {
            messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::TransferFrom {
                    owner: info.sender.to_string(),
                    recipient: env.contract.address.to_string(),
                    amount: desired_amount,
                })?,
                funds: vec![],
            }));
        }
    }

    // mint LP token to sender
    let receiver = receiver.unwrap_or_else(|| info.sender.to_string());
    messages.push(create_mint_tokens_msg(
        env.contract.address.clone(), // use contract as the minter/sender
        Coin {
            denom: pair_info.liquidity_token.clone(), // the LP denom stored as string
            amount: share,
        },
        receiver.to_string(), // mint to the receiver
    ));

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        ("action", "provide_liquidity"),
        ("sender", info.sender.as_str()),
        ("receiver", receiver.as_str()),
        ("assets", &format!("{}, {}", assets[0], assets[1])),
        ("share", &share.to_string()),
        (
            "refund_assets",
            &format!("{}, {}", refund_assets[0], refund_assets[1]),
        ),
    ]))
}

pub fn withdraw_liquidity(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    _info: MessageInfo,
    sender: Addr,
    amount: Uint128,
    min_assets: Option<[Asset; 2]>,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;

    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;

    // Ensure that the transaction includes at least one coin matching the LP token denomination
    // and with an amount exactly equal to the withdrawal amount.
    let valid = _info
        .funds
        .iter()
        .any(|coin| coin.denom == pair_info.liquidity_token && coin.amount == amount);
    if !valid {
        return Err(ContractError::InvalidLiquidityFunds {});
    }

    let contract_addr = env.contract.address.clone();

    let pools: [Asset; 2] =
        pair_info.query_pools(&deps.querier, deps.api, contract_addr.clone())?;

    let total_share: Uint128 =
        query_token_factory_denom_total_supply(&deps.querier, pair_info.liquidity_token.clone())
            .unwrap();

    let share_ratio: Decimal = Decimal::from_ratio(amount, total_share);
    let refund_assets: Vec<Asset> = pools
        .iter()
        .map(|a| Asset {
            info: a.info.clone(),
            amount: a.amount.mul_floor(share_ratio),
        })
        .collect();

    assert_minimum_assets(refund_assets.to_vec(), min_assets)?;

    // update pool info
    Ok(Response::new()
        .add_messages(vec![
            refund_assets[0].clone().into_msg(sender.clone())?,
            refund_assets[1].clone().into_msg(sender.clone())?,
            // burn liquidity token
            create_burn_tokens_msg(
                contract_addr.clone(), // sender: contract address as the minter/burner
                Coin {
                    denom: pair_info.liquidity_token.clone(), // our LP denom string
                    amount,                                   // amount to burn
                },
            ),
        ])
        .add_attributes(vec![
            ("action", "withdraw_liquidity"),
            ("sender", sender.as_str()),
            ("withdrawn_share", &amount.to_string()),
            (
                "refund_assets",
                &format!("{}, {}", refund_assets[0], refund_assets[1]),
            ),
        ]))
}

// CONTRACT - a user must do token approval
#[allow(clippy::too_many_arguments)]
pub fn swap(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    sender: Addr,
    offer_asset: Asset,
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    to: Option<Addr>,
    deadline: Option<u64>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    assert_deadline(env.block.time.seconds(), deadline)?;

    offer_asset.assert_sent_native_token_balance(&info)?;

    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;

    let pools: [Asset; 2] = pair_info.query_pools(&deps.querier, deps.api, env.contract.address)?;

    let offer_pool: Asset;
    let ask_pool: Asset;

    let offer_decimal: u8;
    let ask_decimal: u8;
    // If the asset balance is already increased
    // To calculated properly we should subtract user deposit from the pool
    if offer_asset.info.equal(&pools[0].info) {
        offer_pool = Asset {
            amount: pools[0].amount.checked_sub(offer_asset.amount)?,
            info: pools[0].info.clone(),
        };
        ask_pool = pools[1].clone();

        offer_decimal = pair_info.asset_decimals[0];
        ask_decimal = pair_info.asset_decimals[1];
    } else if offer_asset.info.equal(&pools[1].info) {
        offer_pool = Asset {
            amount: pools[1].amount.checked_sub(offer_asset.amount)?,
            info: pools[1].info.clone(),
        };
        ask_pool = pools[0].clone();

        offer_decimal = pair_info.asset_decimals[1];
        ask_decimal = pair_info.asset_decimals[0];
    } else {
        return Err(ContractError::AssetMismatch {});
    }

    let offer_amount = offer_asset.amount;
    let (return_amount, spread_amount, commission_amount) =
        compute_swap(offer_pool.amount, ask_pool.amount, offer_amount, offer_decimal, ask_decimal)?;

    let return_asset = Asset {
        info: ask_pool.info.clone(),
        amount: return_amount,
    };

    // check max spread limit if exist
    assert_max_spread(
        belief_price,
        max_spread,
        offer_asset.clone(),
        return_asset.clone(),
        spread_amount,
        offer_decimal,
        ask_decimal,
    )?;

    let receiver = to.unwrap_or_else(|| sender.clone());

    let total_fee = commission_amount; // Total fee, assumed to be 0.3% of the transaction
    let fee_wallet_amount = total_fee.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let burn_amount = total_fee.multiply_ratio(1u128, 6u128); // 0.05% (1/6 of the total fee)
    let lp_amount = total_fee
        .checked_sub(fee_wallet_amount)?
        .checked_sub(burn_amount)?;

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = vec![];
    if !return_amount.is_zero() {
        messages.push(return_asset.into_msg(receiver.clone())?);
    }

    // Handle the burn amount
    if !burn_amount.is_zero() {
        let burn_asset = Asset {
            info: ask_pool.info.clone(),
            amount: burn_amount,
        };

        let burn_handler_address = deps.api.addr_humanize(&pair_info.burn_address)?;

        if let AssetInfo::NativeToken { denom } = &burn_asset.info {
            // Call send_native for native tokens
            messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: burn_handler_address.to_string(),
                msg: to_json_binary(&BurnAuctionExecuteMsg::SendNative {
                    asset: burn_asset.clone(),
                })?,
                funds: vec![Coin {
                    denom: denom.clone(),
                    amount: burn_amount,
                }],
            }));
        } else if let AssetInfo::Token { contract_addr } = &burn_asset.info {
            // Send CW20 tokens directly to the burn address
            messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.clone(),
                msg: to_json_binary(&Cw20ExecuteMsg::Send {
                    contract: burn_handler_address.to_string(),
                    amount: burn_amount,
                    msg: Binary::default(),
                })?,
                funds: vec![],
            }));
        }
    }

    // Handle the fee wallet amount
    if !fee_wallet_amount.is_zero() {
        let fee_wallet_asset = Asset {
            info: ask_pool.info.clone(),
            amount: fee_wallet_amount,
        };
        messages.push(
            fee_wallet_asset.into_msg(deps.api.addr_humanize(&pair_info.fee_wallet_address)?)?,
        );
    }

    // new pool amounts
    let offer_pool_post = offer_pool.amount.checked_add(offer_amount)?;
    let ask_pool_post = ask_pool.amount
        .checked_sub(return_amount)?
        .checked_sub(fee_wallet_amount)?
        .checked_sub(burn_amount)?;

    // 1. send collateral token from the contract to a user
    // 2. send inactive commission to collector
    Ok(Response::new().add_messages(messages).add_attributes(vec![
        ("action", "swap"),
        ("sender", sender.as_str()),
        ("receiver", receiver.as_str()),
        ("offer_asset", &offer_asset.info.to_string()),
        ("ask_asset", &ask_pool.info.to_string()),
        ("offer_amount", &offer_amount.to_string()),
        ("return_amount", &return_amount.to_string()),
        ("spread_amount", &spread_amount.to_string()),
        ("commission_amount", &commission_amount.to_string()),
        ("burn_amount", &burn_amount.to_string()),
        ("fee_wallet_amount", &fee_wallet_amount.to_string()),
        ("pool_amount", &lp_amount.to_string()),
        ("offer_pool_balance", &offer_pool_post.to_string()),
        ("ask_pool_balance", &ask_pool_post.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(
    deps: Deps<InjectiveQueryWrapper>,
    _env: Env,
    msg: QueryMsg,
) -> Result<Binary, ContractError> {
    match msg {
        QueryMsg::Pair {} => Ok(to_json_binary(&query_pair_info(deps)?)?),
        QueryMsg::Pool {} => Ok(to_json_binary(&query_pool(deps)?)?),
        QueryMsg::Simulation { offer_asset } => {
            Ok(to_json_binary(&query_simulation(deps, offer_asset)?)?)
        }
        QueryMsg::ReverseSimulation { ask_asset } => {
            Ok(to_json_binary(&query_reverse_simulation(deps, ask_asset)?)?)
        }
    }
}

pub fn query_pair_info(deps: Deps<InjectiveQueryWrapper>) -> Result<PairInfo, ContractError> {
    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;
    let pair_info = pair_info.to_normal(deps.api)?;

    Ok(pair_info)
}

pub fn query_pool(deps: Deps<InjectiveQueryWrapper>) -> Result<PoolResponse, ContractError> {
    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;
    let contract_addr = deps.api.addr_humanize(&pair_info.contract_addr)?;
    let assets: [Asset; 2] = pair_info.query_pools(&deps.querier, deps.api, contract_addr)?;

    let total_share: Uint128 =
        query_token_factory_denom_total_supply(&deps.querier, pair_info.liquidity_token.clone())
            .unwrap();

    let resp = PoolResponse {
        assets,
        total_share,
    };

    Ok(resp)
}

pub fn query_simulation(
    deps: Deps<InjectiveQueryWrapper>,
    offer_asset: Asset,
) -> Result<SimulationResponse, ContractError> {
    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;

    let contract_addr = deps.api.addr_humanize(&pair_info.contract_addr)?;
    let pools: [Asset; 2] = pair_info.query_pools(&deps.querier, deps.api, contract_addr)?;

    let offer_pool: Asset;
    let ask_pool: Asset;
    let offer_decimal: u8;
    let ask_decimal: u8;
    if offer_asset.info.equal(&pools[0].info) {
        offer_pool = pools[0].clone();
        ask_pool = pools[1].clone();
        offer_decimal = pair_info.asset_decimals[0];
        ask_decimal = pair_info.asset_decimals[1];
    } else if offer_asset.info.equal(&pools[1].info) {
        offer_pool = pools[1].clone();
        ask_pool = pools[0].clone();
        offer_decimal = pair_info.asset_decimals[1];
        ask_decimal = pair_info.asset_decimals[0];
    } else {
        return Err(ContractError::AssetMismatch {});
    }

    let (return_amount, spread_amount, commission_amount) =
        compute_swap(offer_pool.amount, ask_pool.amount, offer_asset.amount, offer_decimal, ask_decimal)?;

    Ok(SimulationResponse {
        return_amount,
        spread_amount,
        commission_amount,
    })
}

pub fn query_reverse_simulation(
    deps: Deps<InjectiveQueryWrapper>,
    ask_asset: Asset,
) -> Result<ReverseSimulationResponse, ContractError> {
    let pair_info: PairInfoRaw = PAIR_INFO.load(deps.storage)?;

    let contract_addr = deps.api.addr_humanize(&pair_info.contract_addr)?;
    let pools: [Asset; 2] = pair_info.query_pools(&deps.querier, deps.api, contract_addr)?;

    let offer_pool: Asset;
    let ask_pool: Asset;
    if ask_asset.info.equal(&pools[0].info) {
        ask_pool = pools[0].clone();
        offer_pool = pools[1].clone();
    } else if ask_asset.info.equal(&pools[1].info) {
        ask_pool = pools[1].clone();
        offer_pool = pools[0].clone();
    } else {
        return Err(ContractError::AssetMismatch {});
    }

    let (offer_amount, spread_amount, commission_amount) =
        compute_offer_amount(offer_pool.amount, ask_pool.amount, ask_asset.amount)?;

    Ok(ReverseSimulationResponse {
        offer_amount,
        spread_amount,
        commission_amount,
    })
}


pub fn compute_swap(
    offer_pool: Uint128,
    ask_pool: Uint128,
    offer_amount: Uint128,
    offer_dec: u8,
    ask_dec: u8,
) -> StdResult<(Uint128, Uint128, Uint128)> {
    let target_dec = offer_dec.max(ask_dec);
    let pow10 = |d: u8| Uint256::from(10u128.pow(d as u32));
    let up = |x: Uint128, from: u8| Uint256::from(x) * pow10(target_dec - from);

    // 1. upscale helper
    let offer_pool_u   = up(offer_pool,   offer_dec);
    let ask_pool_u     = up(ask_pool,     ask_dec);
    let offer_amount_u = up(offer_amount, offer_dec);

    // 2. raw math
    let (ret_u128, spread_u128, fee_u128) = compute_swap_raw(
        offer_pool_u.try_into()?,   
        ask_pool_u.try_into()?,     
        offer_amount_u.try_into()?, 
    )?;

    // 3. down-scale helper
    let down = |x: Uint128| -> Uint128 {
        if target_dec > ask_dec {
            x / Uint128::from(10u128.pow((target_dec - ask_dec) as u32))
        } else {
            x
        }
    };

    Ok((down(ret_u128), down(spread_u128), down(fee_u128)))
}


fn compute_swap_raw(
    offer_pool: Uint128,
    ask_pool: Uint128,
    offer_amount: Uint128,
) -> StdResult<(Uint128, Uint128, Uint128)> {
    let offer_pool: Uint256 = offer_pool.into();
    let ask_pool: Uint256 = ask_pool.into();
    let offer_amount: Uint256 = offer_amount.into();

    let commission_rate = Decimal256::permille(COMMISSION_RATE);

    // offer => ask
    // ask_amount = (ask_pool - cp / (offer_pool + offer_amount)) * (1 - commission_rate)
    let return_amount: Uint256 = (ask_pool * offer_amount) / (offer_pool + offer_amount);

    // calculate spread & commission
    let spread_amount: Uint256 =
        (offer_amount.mul_floor(Decimal256::from_ratio(ask_pool, offer_pool))) - return_amount;
    let mut commission_amount: Uint256 = return_amount.mul_floor(commission_rate);
    if return_amount != (commission_amount.mul_floor(Decimal256::one() / commission_rate)) {
        commission_amount += Uint256::from(1u128);
    }
    // commission will be absorbed to pool
    let return_amount: Uint256 = return_amount - commission_amount;
    Ok((
        return_amount.try_into()?,
        spread_amount.try_into()?,
        commission_amount.try_into()?,
    ))
}


fn compute_offer_amount(
    offer_pool: Uint128,
    ask_pool: Uint128,
    ask_amount: Uint128,
) -> StdResult<(Uint128, Uint128, Uint128)> {
    let offer_pool: Uint256 = offer_pool.into();
    let ask_pool: Uint256 = ask_pool.into();
    let ask_amount: Uint256 = ask_amount.into();

    let commission_rate = Decimal256::permille(COMMISSION_RATE);

    // ask => offer
    // offer_amount = cp / (ask_pool - ask_amount / (1 - commission_rate)) - offer_pool
    let cp: Uint256 = offer_pool * ask_pool;

    let one_minus_commission = Decimal256::one() - commission_rate;
    let inv_one_minus_commission = Decimal256::one() / one_minus_commission;
    let mut before_commission_deduction: Uint256 = ask_amount.mul_floor(inv_one_minus_commission);
    if before_commission_deduction.mul_floor(one_minus_commission) != ask_amount {
        before_commission_deduction += Uint256::from(1u8);
    }

    let after_ask_pool = ask_pool - before_commission_deduction;
    let mut after_offer_pool = Uint256::from(1u8).multiply_ratio(cp, after_ask_pool);

    if after_offer_pool * (ask_pool - before_commission_deduction) != cp {
        after_offer_pool += Uint256::from(1u8);
    }

    let offer_amount: Uint256 = after_offer_pool - offer_pool;
    let before_spread_deduction: Uint256 =
        offer_amount.mul_floor(Decimal256::from_ratio(ask_pool, offer_pool));

    let spread_amount = if before_spread_deduction > before_commission_deduction {
        before_spread_deduction - before_commission_deduction
    } else {
        Uint256::zero()
    };

    let commission_amount = before_commission_deduction - ask_amount;

    Ok((
        offer_amount.try_into()?,
        spread_amount.try_into()?,
        commission_amount.try_into()?,
    ))
}

/// If `belief_price` and `max_spread` both are given,
/// we compute new spread else we just use choice
/// spread to check `max_spread`
pub fn assert_max_spread(
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    offer_asset: Asset,
    return_asset: Asset,
    spread_amount: Uint128,
    offer_decimal: u8,
    return_decimal: u8,
) -> Result<(), ContractError> {
    let (offer_amount, return_amount, spread_amount): (Uint256, Uint256, Uint256) =
        match offer_decimal.cmp(&return_decimal) {
            Ordering::Greater => {
                let diff_decimal = 10u64.pow((offer_decimal - return_decimal).into());

                (
                    offer_asset.amount.into(),
                    return_asset
                        .amount
                        .checked_mul(Uint128::from(diff_decimal))?
                        .into(),
                    spread_amount
                        .checked_mul(Uint128::from(diff_decimal))?
                        .into(),
                )
            }
            Ordering::Less => {
                let diff_decimal = 10u64.pow((return_decimal - offer_decimal).into());

                (
                    offer_asset
                        .amount
                        .checked_mul(Uint128::from(diff_decimal))?
                        .into(),
                    return_asset.amount.into(),
                    spread_amount.into(),
                )
            }
            Ordering::Equal => (
                offer_asset.amount.into(),
                return_asset.amount.into(),
                spread_amount.into(),
            ),
        };

    if let (Some(max_spread), Some(belief_price)) = (max_spread, belief_price) {
        let belief_price: Decimal256 = Decimal256::from_str(&belief_price.to_string())?;
        let max_spread: Decimal256 = Decimal256::from_str(&max_spread.to_string())?;

        let expected_return = offer_amount.mul_floor(Decimal256::one() / belief_price);
        let spread_amount = if expected_return > return_amount {
            expected_return - return_amount
        } else {
            Uint256::zero()
        };

        if return_amount < expected_return
            && Decimal256::from_ratio(spread_amount, expected_return) > max_spread
        {
            return Err(ContractError::MaxSpreadAssertion {});
        }
    } else if let Some(max_spread) = max_spread {
        let max_spread: Decimal256 = Decimal256::from_str(&max_spread.to_string())?;
        if Decimal256::from_ratio(spread_amount, return_amount + spread_amount) > max_spread {
            return Err(ContractError::MaxSpreadAssertion {});
        }
    }

    Ok(())
}

pub fn assert_minimum_assets(
    assets: Vec<Asset>,
    min_assets: Option<[Asset; 2]>,
) -> Result<(), ContractError> {
    if let Some(min_assets) = min_assets {
        min_assets.iter().try_for_each(|min_asset| {
            match assets.iter().find(|asset| asset.info == min_asset.info) {
                Some(asset) => {
                    if asset.amount.cmp(&min_asset.amount).is_lt() {
                        return Err(ContractError::MinAmountAssertion {
                            min_asset: min_asset.to_string(),
                            asset: asset.to_string(),
                        });
                    }
                }
                None => {
                    return Err(ContractError::MinAmountAssertion {
                        min_asset: min_asset.to_string(),
                        asset: Asset {
                            info: min_asset.info.clone(),
                            amount: Uint128::zero(),
                        }
                        .to_string(),
                    })
                }
            };

            Ok(())
        })?;
    }

    Ok(())
}

pub fn assert_deadline(blocktime: u64, deadline: Option<u64>) -> Result<(), ContractError> {
    if let Some(deadline) = deadline {
        if blocktime >= deadline {
            return Err(ContractError::ExpiredDeadline {});
        }
    }

    Ok(())
}

const TARGET_CONTRACT_VERSION: &str = "1.1.2";
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _msg: MigrateMsg,
) -> Result<Response, ContractError> {
    migrate_version(
        deps,
        TARGET_CONTRACT_VERSION,
        CONTRACT_NAME,
        CONTRACT_VERSION,
    )?;

    Ok(Response::default())
}

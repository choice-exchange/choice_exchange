use cosmwasm_std::testing::{
    message_info, mock_dependencies, mock_env, MockApi, MockQuerier, MockStorage,
};
use cosmwasm_std::{
    coins, from_json, to_json_binary, Binary, Coin, ContractInfoResponse, ContractResult,
    CosmosMsg, Event, OwnedDeps, Reply, SubMsgResponse, SubMsgResult, SystemError, SystemResult,
    Uint128, WasmMsg, WasmQuery,
};

use choice::asset::AssetInfo;
use choice::farm_factory::{
    ConfigResponse, ExecuteMsg, FarmCountResponse, FarmRecord as FarmRecordResp, FarmsResponse,
    InstantiateMsg, PendingOwnerRotationResponse, QueryMsg,
};
use choice::staking::{
    Cw20HookMsg as FarmCw20HookMsg, ExecuteMsg as FarmExecuteMsg,
    InstantiateMsg as FarmInstantiateMsg,
};
use cw20::Cw20ExecuteMsg;

use crate::contract::{execute, instantiate, query, reply};
use crate::state::INSTANTIATE_FARM_REPLY_ID;

type TestDeps = OwnedDeps<MockStorage, MockApi, MockQuerier>;

const FEE: u128 = 1_000_000_000_000_000_000; // 1 INJ

/// Schedule slots must end after `env.block.time` (the factory rejects past-
/// only schedules, M-1). Use this helper so each slot is anchored to the
/// mock env's wall-clock and we don't have to hand-roll absolute timestamps.
fn future_t(offset: u64) -> u64 {
    mock_env().block.time.seconds() + offset
}

fn default_init_msg(api: &MockApi) -> InstantiateMsg {
    InstantiateMsg {
        owner: api.addr_make("owner").to_string(),
        fee_collector: api.addr_make("treasury").to_string(),
        instantiate_fee_inj: Uint128::from(FEE),
        farm_code_id: 42,
        farm_owner: api.addr_make("multisig").to_string(),
    }
}

/// Pre-initialized factory. Also registers a wasm-info handler on the mock
/// querier so the C-1 admin assertion inside `execute_create_farm` sees the
/// factory's own `ContractInfo` with `admin == multisig`. Tests that
/// deliberately test the mismatch path call `set_factory_admin` to override.
fn setup() -> TestDeps {
    let mut deps = mock_dependencies();
    let msg = default_init_msg(&deps.api);
    let deployer = deps.api.addr_make("deployer");
    let info = message_info(&deployer, &[]);
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
    let multisig = deps.api.addr_make("multisig").to_string();
    install_factory_admin_handler(&mut deps, Some(multisig));
    deps
}

/// Install a `WasmQuery::ContractInfo` handler that reports the factory's
/// own admin as `admin`. Pass `None` to simulate no admin (cleared via
/// `MsgUpdateAdmin`).
fn install_factory_admin_handler(deps: &mut TestDeps, admin: Option<String>) {
    let factory_addr = mock_env().contract.address.to_string();
    deps.querier.update_wasm(move |query| match query {
        WasmQuery::ContractInfo { contract_addr } if contract_addr == &factory_addr => {
            let resp = ContractInfoResponse::new(
                1,
                cosmwasm_std::Addr::unchecked("creator"),
                admin.clone().map(cosmwasm_std::Addr::unchecked),
                false,
                None,
            );
            SystemResult::Ok(ContractResult::Ok(to_json_binary(&resp).unwrap()))
        }
        _ => SystemResult::Err(SystemError::NoSuchContract {
            addr: match query {
                WasmQuery::ContractInfo { contract_addr } => contract_addr.clone(),
                WasmQuery::Smart { contract_addr, .. } => contract_addr.clone(),
                WasmQuery::Raw { contract_addr, .. } => contract_addr.clone(),
                _ => "unknown".to_string(),
            },
        }),
    });
}

/// Build a synthetic reply for `INSTANTIATE_FARM_REPLY_ID` mimicking the chain's
/// `instantiate` event for a successful `WasmMsg::Instantiate`.
#[allow(deprecated)]
fn synthetic_instantiate_reply(farm_addr: &str) -> Reply {
    Reply {
        id: INSTANTIATE_FARM_REPLY_ID,
        payload: Binary::default(),
        gas_used: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![
                Event::new("instantiate").add_attribute("_contract_address", farm_addr.to_string()),
            ],
            data: None,
            msg_responses: vec![],
        }),
    }
}

#[test]
fn proper_initialization() {
    let deps = setup();

    let resp: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(
        resp,
        ConfigResponse {
            owner: deps.api.addr_make("owner").to_string(),
            fee_collector: deps.api.addr_make("treasury").to_string(),
            instantiate_fee_inj: Uint128::from(FEE),
            farm_code_id: 42,
            farm_owner: deps.api.addr_make("multisig").to_string(),
        }
    );

    let count: FarmCountResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FarmCount {}).unwrap()).unwrap();
    assert_eq!(count.count, 0);
}

#[test]
fn instantiate_rejects_zero_fee() {
    let mut deps = mock_dependencies();
    let mut msg = default_init_msg(&deps.api);
    msg.instantiate_fee_inj = Uint128::zero();
    let deployer = deps.api.addr_make("deployer");
    let info = message_info(&deployer, &[]);
    let err = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(err
        .to_string()
        .contains("instantiate_fee_inj must be non-zero"));
}

#[test]
fn instantiate_rejects_zero_code_id() {
    let mut deps = mock_dependencies();
    let mut msg = default_init_msg(&deps.api);
    msg.farm_code_id = 0;
    let deployer = deps.api.addr_make("deployer");
    let info = message_info(&deployer, &[]);
    let err = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(err.to_string().contains("farm_code_id must be non-zero"));
}

#[test]
fn instantiate_rejects_attached_funds() {
    let mut deps = mock_dependencies();
    let msg = default_init_msg(&deps.api);
    let deployer = deps.api.addr_make("deployer");
    let info = message_info(&deployer, &coins(1, "inj"));
    let err = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(err
        .to_string()
        .contains("factory instantiate accepts no funds"));
}

/// CW20 reward: caller sends just the inj fee; factory dispatches TransferFrom
/// + Instantiate; reply emits Cw20::Send to fund the new farm.
#[test]
fn create_farm_cw20_reward_happy_path() {
    let mut deps = setup();

    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let treasury = deps.api.addr_make("treasury");
    let farm_addr = deps.api.addr_make("farm0000");
    let total_reward = Uint128::from(11_000_000u128);

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![
            (future_t(100), future_t(150), Uint128::from(1_000_000u128)),
            (future_t(150), future_t(200), Uint128::from(10_000_000u128)),
        ],
    };

    let info = message_info(&user, &coins(FEE, "inj"));
    let res = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap();

    assert_eq!(res.messages.len(), 3);

    // 1: Bank send fee → treasury
    match &res.messages[0].msg {
        CosmosMsg::Bank(cosmwasm_std::BankMsg::Send {
            to_address,
            amount,
        }) => {
            assert_eq!(to_address, &treasury.to_string());
            assert_eq!(amount, &coins(FEE, "inj"));
        }
        other => panic!("expected BankMsg::Send, got {:?}", other),
    }

    // 2: Cw20::TransferFrom user → factory
    match &res.messages[1].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(contract_addr, &reward_cw20.to_string());
            assert!(funds.is_empty());
            let parsed: Cw20ExecuteMsg = from_json(msg).unwrap();
            match parsed {
                Cw20ExecuteMsg::TransferFrom {
                    owner,
                    recipient: _,
                    amount,
                } => {
                    assert_eq!(owner, user.to_string());
                    assert_eq!(amount, total_reward);
                }
                _ => panic!("expected Cw20::TransferFrom"),
            }
        }
        other => panic!("expected Wasm::Execute, got {:?}", other),
    }

    // 3: SubMsg Instantiate farm. wasm admin = configured `farm_owner`
    // (the timelock). `Config.owner` (carried in the msg) = the user who
    // called CreateFarm. Funds are empty (reward arrives via Fund {} in
    // the reply).
    let multisig = deps.api.addr_make("multisig").to_string();
    match &res.messages[2].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin,
            code_id,
            msg,
            funds,
            ..
        }) => {
            assert_eq!(*code_id, 42);
            assert_eq!(admin, &Some(multisig.clone()));
            assert!(funds.is_empty());
            let parsed: FarmInstantiateMsg = from_json(msg).unwrap();
            assert_eq!(parsed.distribution_schedule.len(), 2);
            assert_eq!(parsed.owner, user.to_string());
        }
        other => panic!("expected Wasm::Instantiate, got {:?}", other),
    }

    // Reply: hydrates registry and emits Cw20::Send to fund the new farm.
    let reply_resp = reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm_addr.as_str()),
    )
    .unwrap();

    assert_eq!(reply_resp.messages.len(), 1);
    match &reply_resp.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            ..
        }) => {
            assert_eq!(contract_addr, &reward_cw20.to_string());
            let parsed: Cw20ExecuteMsg = from_json(msg).unwrap();
            match parsed {
                Cw20ExecuteMsg::Send {
                    contract,
                    amount,
                    msg: hook,
                } => {
                    assert_eq!(contract, farm_addr.to_string());
                    assert_eq!(amount, total_reward);
                    let parsed_hook: FarmCw20HookMsg = from_json(&hook).unwrap();
                    assert!(matches!(parsed_hook, FarmCw20HookMsg::Fund {}));
                }
                _ => panic!("expected Cw20::Send"),
            }
        }
        other => panic!("expected Wasm::Execute, got {:?}", other),
    }

    // Registry hydrated. operator is the user who paid the fee;
    // farm_owner is the multisig installed as Config.owner on the farm.
    let record: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 0 }).unwrap()).unwrap();
    assert_eq!(record.id, 0);
    assert_eq!(record.farm_addr, farm_addr.to_string());
    assert_eq!(record.operator, user.to_string());
    assert_eq!(record.farm_owner, deps.api.addr_make("multisig").to_string());
    assert_eq!(record.total_reward, total_reward);

    let by_addr: FarmRecordResp = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::FarmByAddr {
                addr: farm_addr.to_string(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_addr.id, 0);

    let count: FarmCountResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FarmCount {}).unwrap()).unwrap();
    assert_eq!(count.count, 1);
}

/// Native reward (non-inj): caller sends fee + reward; reward is held by the
/// factory through instantiate (which receives empty funds) and forwarded to
/// the new farm via `Fund {}` in the reply.
#[test]
fn create_farm_native_reward_happy_path() {
    let mut deps = setup();

    let user = deps.api.addr_make("alice");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let farm_addr = deps.api.addr_make("farm0000");
    let total_reward = Uint128::from(500u128);

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::NativeToken {
            denom: "uatom".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), total_reward)],
    };

    let info = message_info(
        &user,
        &[
            Coin::new(FEE, "inj"),
            Coin::new(total_reward.u128(), "uatom"),
        ],
    );
    let res = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap();

    // 1: Bank fee, 2: SubMsg Instantiate with empty funds (Fund{} is in reply).
    assert_eq!(res.messages.len(), 2);

    match &res.messages[1].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate { funds, .. }) => {
            assert!(funds.is_empty(), "expected empty instantiate funds, got {:?}", funds);
        }
        other => panic!("expected Wasm::Instantiate, got {:?}", other),
    }

    let reply_resp = reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm_addr.as_str()),
    )
    .unwrap();

    // Reply forwards `Fund {}` with the reward funds attached.
    assert_eq!(reply_resp.messages.len(), 1);
    match &reply_resp.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(contract_addr, &farm_addr.to_string());
            assert_eq!(funds, &vec![Coin::new(total_reward.u128(), "uatom")]);
            let parsed: FarmExecuteMsg = from_json(msg).unwrap();
            assert!(matches!(parsed, FarmExecuteMsg::Fund {}));
        }
        other => panic!("expected Wasm::Execute Fund, got {:?}", other),
    }

    let record: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 0 }).unwrap()).unwrap();
    assert_eq!(record.total_reward, total_reward);
}

/// Reward denom == fee denom ("inj"): caller sends one combined coin; reward
/// is forwarded via `Fund {}` in the reply (not as instantiate funds).
#[test]
fn create_farm_inj_reward_combines_funds() {
    let mut deps = setup();

    let user = deps.api.addr_make("alice");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let farm_addr = deps.api.addr_make("farm0000");
    let total_reward = Uint128::from(7u128);

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::NativeToken {
            denom: "inj".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), total_reward)],
    };

    let info = message_info(&user, &coins(FEE + total_reward.u128(), "inj"));
    let res = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap();

    assert_eq!(res.messages.len(), 2);
    match &res.messages[1].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate { funds, .. }) => {
            assert!(funds.is_empty(), "expected empty instantiate funds, got {:?}", funds);
        }
        other => panic!("expected Wasm::Instantiate, got {:?}", other),
    }

    let reply_resp = reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm_addr.as_str()),
    )
    .unwrap();

    assert_eq!(reply_resp.messages.len(), 1);
    match &reply_resp.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(contract_addr, &farm_addr.to_string());
            assert_eq!(funds, &vec![Coin::new(total_reward.u128(), "inj")]);
            let parsed: FarmExecuteMsg = from_json(msg).unwrap();
            assert!(matches!(parsed, FarmExecuteMsg::Fund {}));
        }
        other => panic!("expected Wasm::Execute Fund, got {:?}", other),
    }
}

#[test]
fn create_farm_rejects_wrong_fee_amount() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(100u128))],
    };

    let info = message_info(&user, &coins(FEE / 2, "inj"));
    let err = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap_err();
    assert!(err.to_string().contains("expected"));
}

#[test]
fn create_farm_rejects_native_reward_shortfall() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let total_reward = Uint128::from(500u128);

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::NativeToken {
            denom: "uatom".to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), total_reward)],
    };

    let info = message_info(
        &user,
        &[Coin::new(FEE, "inj"), Coin::new(250u128, "uatom")],
    );
    let err = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap_err();
    assert!(err.to_string().contains("expected 500 uatom"));
}

#[test]
fn update_config_owner_only() {
    let mut deps = setup();
    let outsider = deps.api.addr_make("outsider");
    let owner = deps.api.addr_make("owner");
    let new_treasury = deps.api.addr_make("new_treasury");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&outsider, &[]),
        ExecuteMsg::UpdateConfig {
            fee_collector: Some(outsider.to_string()),
            instantiate_fee_inj: None,
            farm_owner: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            fee_collector: Some(new_treasury.to_string()),
            instantiate_fee_inj: Some(Uint128::from(2 * FEE)),
            farm_owner: None,
        },
    )
    .unwrap();

    let resp: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(resp.fee_collector, new_treasury.to_string());
    assert_eq!(resp.instantiate_fee_inj, Uint128::from(2 * FEE));
    // farm_code_id stays as instantiated; rotated through the timelocked path.
    assert_eq!(resp.farm_code_id, 42);
}

/// H-2: farm_code_id swap is timelocked. Outsider can't propose / apply; owner
/// can propose, must wait the timelock, then apply.
#[test]
fn farm_code_id_timelock() {
    let mut deps = setup();
    let owner = deps.api.addr_make("owner");
    let outsider = deps.api.addr_make("outsider");
    let mut env = mock_env();

    // Outsider can't propose.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::ProposeUpdateFarmCodeId { farm_code_id: 99 },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    // Zero rejected.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::ProposeUpdateFarmCodeId { farm_code_id: 0 },
    )
    .unwrap_err();
    assert!(err.to_string().contains("farm_code_id must be non-zero"));

    // Owner proposes 99.
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::ProposeUpdateFarmCodeId { farm_code_id: 99 },
    )
    .unwrap();

    let pending: choice::farm_factory::PendingFarmCodeIdUpdateResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::PendingFarmCodeIdUpdate {},
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(pending.farm_code_id, Some(99));

    // Premature apply rejected.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::ApplyUpdateFarmCodeId {},
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("farm_code_id update timelock has not elapsed"));

    // Wait, apply.
    env.block.time = env.block.time.plus_seconds(
        crate::state::TIMELOCK_DELAY_SECONDS + 1,
    );
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::ApplyUpdateFarmCodeId {},
    )
    .unwrap();

    let resp: ConfigResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(resp.farm_code_id, 99);

    // Pending cleared.
    let pending: choice::farm_factory::PendingFarmCodeIdUpdateResponse = from_json(
        query(deps.as_ref(), env, QueryMsg::PendingFarmCodeIdUpdate {}).unwrap(),
    )
    .unwrap();
    assert_eq!(pending.farm_code_id, None);
}

/// C-1 regression: `execute_create_farm` refuses to spawn when the factory's
/// own wasm admin does not match `config.farm_owner`. Catches the "operator
/// rotated factory admin without updating farm_owner" footgun.
#[test]
fn create_farm_rejects_factory_admin_mismatch() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward = deps.api.addr_make("rewardcw20");
    let staking = deps.api.addr_make("stakingcw20");

    // Swap the factory's wasm admin out from under the configured farm_owner.
    let evil = deps.api.addr_make("evil").to_string();
    install_factory_admin_handler(&mut deps, Some(evil));

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        ExecuteMsg::CreateFarm {
            reward_token: AssetInfo::Token {
                contract_addr: reward.to_string(),
            },
            staking_token: AssetInfo::Token {
                contract_addr: staking.to_string(),
            },
            distribution_schedule: vec![(
                future_t(100),
                future_t(200),
                Uint128::from(1_000_000u128),
            )],
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("factory admin mismatch"),
        "got: {}",
        err
    );
}

/// C-1 regression: `execute_create_farm` refuses to spawn when the factory has
/// no wasm admin set.
#[test]
fn create_farm_rejects_no_factory_admin() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward = deps.api.addr_make("rewardcw20");
    let staking = deps.api.addr_make("stakingcw20");

    install_factory_admin_handler(&mut deps, None);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        ExecuteMsg::CreateFarm {
            reward_token: AssetInfo::Token {
                contract_addr: reward.to_string(),
            },
            staking_token: AssetInfo::Token {
                contract_addr: staking.to_string(),
            },
            distribution_schedule: vec![(
                future_t(100),
                future_t(200),
                Uint128::from(1_000_000u128),
            )],
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("no wasm admin set"),
        "got: {}",
        err
    );
}

/// Cancel removes a pending farm_code_id proposal.
#[test]
fn farm_code_id_cancel() {
    let mut deps = setup();
    let owner = deps.api.addr_make("owner");

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ProposeUpdateFarmCodeId { farm_code_id: 99 },
    )
    .unwrap();

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::CancelUpdateFarmCodeIdProposal {},
    )
    .unwrap();

    let pending: choice::farm_factory::PendingFarmCodeIdUpdateResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::PendingFarmCodeIdUpdate {},
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(pending.farm_code_id, None);

    // Re-cancel errors.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::CancelUpdateFarmCodeIdProposal {},
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("no pending farm_code_id update"));
}

#[test]
fn owner_rotation_timelock() {
    let mut deps = setup();
    let owner = deps.api.addr_make("owner");
    let new_owner = deps.api.addr_make("new_owner");

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner.to_string(),
        },
    )
    .unwrap();

    let pending: PendingOwnerRotationResponse = from_json(
        query(deps.as_ref(), mock_env(), QueryMsg::PendingOwnerRotation {}).unwrap(),
    )
    .unwrap();
    assert_eq!(pending.pending_owner, Some(new_owner.to_string()));

    let mut early_env = mock_env();
    early_env.block.time = early_env.block.time.plus_seconds(1);
    let err = execute(
        deps.as_mut(),
        early_env,
        message_info(&owner, &[]),
        ExecuteMsg::ApplyOwnerRotation {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("timelock has not elapsed"));

    let mut late_env = mock_env();
    late_env.block.time = late_env.block.time.plus_seconds(48 * 60 * 60 + 1);
    execute(
        deps.as_mut(),
        late_env,
        message_info(&owner, &[]),
        ExecuteMsg::ApplyOwnerRotation {},
    )
    .unwrap();

    let resp: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(resp.owner, new_owner.to_string());
}

#[test]
fn query_farms_pagination() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    for i in 0..3u64 {
        let create_msg = ExecuteMsg::CreateFarm {
            reward_token: AssetInfo::Token {
                contract_addr: reward_cw20.to_string(),
            },
            staking_token: AssetInfo::Token {
                contract_addr: staking_cw20.to_string(),
            },
            distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(1_000u128))],
        };
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&user, &coins(FEE, "inj")),
            create_msg,
        )
        .unwrap();

        let made = deps.api.addr_make(&format!("farm{}", i));
        reply(
            deps.as_mut(),
            mock_env(),
            synthetic_instantiate_reply(made.as_str()),
        )
        .unwrap();
    }

    let all: FarmsResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::Farms {
                start_after: None,
                limit: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(all.farms.len(), 3);
    assert_eq!(all.farms[0].id, 0);
    assert_eq!(all.farms[2].id, 2);

    let page: FarmsResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::Farms {
                start_after: Some(0),
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(page.farms.len(), 1);
    assert_eq!(page.farms[0].id, 1);
}

/// Phase C: the spawned farm's wasm `admin` = configured `farm_owner`
/// (multisig/timelock), `Config.owner` = the user who paid the launch fee.
/// `FarmRecord` captures both roles distinctly so off-chain consumers can
/// tell them apart.
#[test]
fn create_farm_installs_multisig_as_wasm_admin_and_user_as_owner() {
    let mut deps = setup();

    let user = deps.api.addr_make("alice");
    let multisig = deps.api.addr_make("multisig");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let farm_addr = deps.api.addr_make("farm0000");

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(1_000u128))],
    };

    let info = message_info(&user, &coins(FEE, "inj"));
    let res = execute(deps.as_mut(), mock_env(), info, create_msg).unwrap();

    // Instantiate submsg (index 2): wasm admin == multisig (timelock).
    // Embedded `Config.owner` == user (the creator).
    match &res.messages[2].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate { admin, msg, .. }) => {
            assert_eq!(admin, &Some(multisig.to_string()));
            let parsed: FarmInstantiateMsg = from_json(msg).unwrap();
            assert_eq!(parsed.owner, user.to_string());
        }
        other => panic!("expected Wasm::Instantiate, got {:?}", other),
    }

    reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm_addr.as_str()),
    )
    .unwrap();

    // FarmRecord captures both roles distinctly.
    let record: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 0 }).unwrap()).unwrap();
    assert_eq!(record.operator, user.to_string());
    assert_eq!(record.farm_owner, multisig.to_string());
    assert_ne!(record.operator, record.farm_owner);
}

/// Phase C invariant: wasm admin (`farm_owner`) and farm `Config.owner`
/// (creator) are independently controlled. Two different users creating
/// farms get distinct `Config.owner`s, but the wasm admin on every farm is
/// the same protocol-side `farm_owner` configured on the factory.
#[test]
fn create_farm_separates_admin_from_config_owner() {
    let mut deps = setup();
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let multisig = deps.api.addr_make("multisig").to_string();
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");
    let farm0 = deps.api.addr_make("farm0");
    let farm1 = deps.api.addr_make("farm1");

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(1_000u128))],
    };

    for (caller, farm_addr) in [(&alice, &farm0), (&bob, &farm1)] {
        let res = execute(
            deps.as_mut(),
            mock_env(),
            message_info(caller, &coins(FEE, "inj")),
            create_msg.clone(),
        )
        .unwrap();
        match &res.messages[2].msg {
            CosmosMsg::Wasm(WasmMsg::Instantiate { admin, msg, .. }) => {
                assert_eq!(admin, &Some(multisig.clone()));
                let parsed: FarmInstantiateMsg = from_json(msg).unwrap();
                assert_eq!(parsed.owner, caller.to_string());
                assert_ne!(parsed.owner, multisig);
            }
            other => panic!("expected Wasm::Instantiate, got {:?}", other),
        }
        reply(
            deps.as_mut(),
            mock_env(),
            synthetic_instantiate_reply(farm_addr.as_str()),
        )
        .unwrap();
    }

    let a: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 0 }).unwrap()).unwrap();
    let b: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 1 }).unwrap()).unwrap();
    assert_eq!(a.operator, alice.to_string());
    assert_eq!(b.operator, bob.to_string());
    assert_eq!(a.farm_owner, multisig);
    assert_eq!(b.farm_owner, multisig);
}

/// H-1 follow-up: changing the factory's `farm_owner` only affects farms
/// created AFTER the update. Existing farms keep their original wasm admin
/// (the `farm_owner` config at creation time). `Config.owner` is the
/// creator and was never tied to factory state to begin with.
#[test]
fn update_farm_owner_does_not_retro_apply() {
    let mut deps = setup();
    let owner = deps.api.addr_make("owner");
    let new_multisig = deps.api.addr_make("new_multisig");
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    // First farm created with the original multisig.
    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(1_000u128))],
    };
    let farm0 = deps.api.addr_make("farm0");
    let farm1 = deps.api.addr_make("farm1");

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        create_msg.clone(),
    )
    .unwrap();
    reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm0.as_str()),
    )
    .unwrap();

    // Owner rotates `farm_owner` config to a new multisig.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            fee_collector: None,
            instantiate_fee_inj: None,
            farm_owner: Some(new_multisig.to_string()),
        },
    )
    .unwrap();

    // The C-1 admin-assertion in execute_create_farm requires the factory's
    // wasm admin to match the new farm_owner. Reflect the operational step
    // (MsgUpdateAdmin on the factory) in the mock querier.
    install_factory_admin_handler(&mut deps, Some(new_multisig.to_string()));

    // Second farm gets the NEW multisig.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        create_msg,
    )
    .unwrap();
    reply(
        deps.as_mut(),
        mock_env(),
        synthetic_instantiate_reply(farm1.as_str()),
    )
    .unwrap();

    let first: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 0 }).unwrap()).unwrap();
    let second: FarmRecordResp =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Farm { id: 1 }).unwrap()).unwrap();
    assert_eq!(first.farm_owner, deps.api.addr_make("multisig").to_string());
    assert_eq!(second.farm_owner, new_multisig.to_string());
}

/// H-2: a second `CreateFarm` in the same tx (e.g., re-entered from a
/// malicious CW20 TransferFrom handler) must be rejected up-front so we
/// never silently overwrite the in-flight `PENDING_FARM`.
#[test]
fn create_farm_rejects_reentrant_call() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    let create_msg = ExecuteMsg::CreateFarm {
        reward_token: AssetInfo::Token {
            contract_addr: reward_cw20.to_string(),
        },
        staking_token: AssetInfo::Token {
            contract_addr: staking_cw20.to_string(),
        },
        distribution_schedule: vec![(future_t(100), future_t(200), Uint128::from(1_000u128))],
    };

    // First call writes PENDING_FARM and returns its submessages — the reply
    // hasn't fired yet, so PENDING_FARM is still set.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        create_msg.clone(),
    )
    .unwrap();

    // Second call inside the same in-flight window must fail.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        create_msg,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("reentrant"),
        "expected reentrant guard, got {}",
        err
    );
}

/// M-1: schedules whose latest `end` is at or before the current block time
/// strand the reward forever (no slot ever overlaps). Reject at the factory.
#[test]
fn create_farm_rejects_past_only_schedule() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    // start > end is the existing strict check; here `end <= now` is the new one.
    let now = mock_env().block.time.seconds();
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        ExecuteMsg::CreateFarm {
            reward_token: AssetInfo::Token {
                contract_addr: reward_cw20.to_string(),
            },
            staking_token: AssetInfo::Token {
                contract_addr: staking_cw20.to_string(),
            },
            distribution_schedule: vec![(now - 200, now - 100, Uint128::from(1_000u128))],
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("end must be in the future"),
        "expected past-end rejection, got {}",
        err
    );
}

/// M-2: schedules above `MAX_SCHEDULE_SLOTS` are rejected. Guards against
/// accidentally O(slots) gas on every bond/unbond/withdraw.
#[test]
fn create_farm_rejects_too_many_slots() {
    let mut deps = setup();
    let user = deps.api.addr_make("alice");
    let reward_cw20 = deps.api.addr_make("rewardcw20");
    let staking_cw20 = deps.api.addr_make("stakingcw20");

    // 21 contiguous future slots — one over the cap of 20.
    let schedule: Vec<(u64, u64, Uint128)> = (0..21u64)
        .map(|i| {
            (
                future_t(100 + i * 100),
                future_t(200 + i * 100),
                Uint128::from(1u128),
            )
        })
        .collect();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&user, &coins(FEE, "inj")),
        ExecuteMsg::CreateFarm {
            reward_token: AssetInfo::Token {
                contract_addr: reward_cw20.to_string(),
            },
            staking_token: AssetInfo::Token {
                contract_addr: staking_cw20.to_string(),
            },
            distribution_schedule: schedule,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("at most 20 slots"),
        "expected slot-cap rejection, got {}",
        err
    );
}

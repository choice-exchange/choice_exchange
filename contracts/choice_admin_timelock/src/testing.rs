use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
use cosmwasm_std::{
    coins, from_json, to_json_binary, Binary, Coin, CosmosMsg, SubMsg, Uint128, WasmMsg,
};

use crate::contract::{
    execute, instantiate, query, ConfigResponse, ExecuteMsg, InstantiateMsg, PendingActionResponse,
    PendingOwnerRotationResponse, QueryMsg,
};
use crate::state::{ProposedAction, MIN_TIMELOCK_SECONDS};

const TIMELOCK: u64 = 48 * 60 * 60;

type Deps = cosmwasm_std::OwnedDeps<
    cosmwasm_std::testing::MockStorage,
    cosmwasm_std::testing::MockApi,
    cosmwasm_std::testing::MockQuerier,
>;

fn setup() -> (Deps, cosmwasm_std::Addr) {
    let mut deps = mock_dependencies();
    let owner = deps.api.addr_make("owner");
    let deployer = deps.api.addr_make("deployer");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&deployer, &[]),
        InstantiateMsg {
            owner: owner.to_string(),
            timelock_seconds: TIMELOCK,
        },
    )
    .unwrap();
    (deps, owner)
}

fn migrate_action(contract: &str, code_id: u64, msg: Binary) -> ProposedAction {
    ProposedAction::Migrate {
        contract: contract.to_string(),
        code_id,
        msg,
    }
}

fn execute_action(contract: &str, msg: Binary, funds: Vec<Coin>) -> ProposedAction {
    ProposedAction::Execute {
        contract: contract.to_string(),
        msg,
        funds,
    }
}

#[test]
fn instantiate_rejects_short_timelock() {
    let mut deps = mock_dependencies();
    let deployer = deps.api.addr_make("deployer");
    let owner = deps.api.addr_make("owner");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&deployer, &[]),
        InstantiateMsg {
            owner: owner.to_string(),
            timelock_seconds: MIN_TIMELOCK_SECONDS - 1,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("timelock_seconds must be at least"));
}

#[test]
fn instantiate_records_config() {
    let (deps, owner) = setup();
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.owner, owner.to_string());
    assert_eq!(cfg.timelock_seconds, TIMELOCK);
}

#[test]
fn propose_migrate_apply_full_path() {
    let (mut deps, owner) = setup();
    let target = deps.api.addr_make("farm0000");
    let mut env = mock_env();

    let outsider = deps.api.addr_make("outsider");
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Propose {
            action: migrate_action(target.as_str(), 99, Binary::default()),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: migrate_action(target.as_str(), 0, Binary::default()),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("code_id must be non-zero"));

    let migrate_msg = to_json_binary(&Uint128::from(7u128)).unwrap();
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: migrate_action(target.as_str(), 99, migrate_msg.clone()),
        },
    )
    .unwrap();

    let pending: PendingActionResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::PendingAction {}).unwrap()).unwrap();
    assert_eq!(
        pending.action,
        Some(migrate_action(target.as_str(), 99, migrate_msg.clone()))
    );
    assert_eq!(pending.effective_at, Some(env.block.time.seconds() + TIMELOCK));

    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("action timelock has not elapsed"));

    env.block.time = env.block.time.plus_seconds(TIMELOCK + 1);
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: target.to_string(),
            new_code_id: 99,
            msg: migrate_msg,
        }))]
    );

    let pending: PendingActionResponse =
        from_json(query(deps.as_ref(), env, QueryMsg::PendingAction {}).unwrap()).unwrap();
    assert_eq!(pending.action, None);
}

#[test]
fn propose_execute_apply_full_path() {
    let (mut deps, owner) = setup();
    let farm = deps.api.addr_make("farm0000");
    let creator = deps.api.addr_make("creator");
    let mut env = mock_env();

    // ProposeNewOwner-on-farm payload — opaque from the timelock's POV.
    // Opaque payload from the timelock's POV — any Binary is forwarded as-is.
    let rotate_msg = to_json_binary(&Uint128::from(creator.as_str().len() as u128)).unwrap();

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: execute_action(farm.as_str(), rotate_msg.clone(), vec![]),
        },
    )
    .unwrap();

    let pending: PendingActionResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::PendingAction {}).unwrap()).unwrap();
    assert_eq!(
        pending.action,
        Some(execute_action(farm.as_str(), rotate_msg.clone(), vec![]))
    );

    env.block.time = env.block.time.plus_seconds(TIMELOCK + 1);
    let outsider = deps.api.addr_make("outsider");
    let res = execute(
        deps.as_mut(),
        env,
        message_info(&outsider, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm.to_string(),
            msg: rotate_msg,
            funds: vec![],
        }))]
    );
}

#[test]
fn propose_execute_carries_funds() {
    let (mut deps, owner) = setup();
    let farm = deps.api.addr_make("farm0000");
    let mut env = mock_env();
    let fund_msg = to_json_binary(&Uint128::from(1u128)).unwrap();
    let funds = coins(1_000_000_000_000_000_000u128, "inj");

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: execute_action(farm.as_str(), fund_msg.clone(), funds.clone()),
        },
    )
    .unwrap();

    env.block.time = env.block.time.plus_seconds(TIMELOCK + 1);
    let res = execute(
        deps.as_mut(),
        env,
        message_info(&owner, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap();

    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: farm.to_string(),
            msg: fund_msg,
            funds,
        }))]
    );
}

#[test]
fn propose_execute_rejects_zero_amount_funds() {
    let (mut deps, owner) = setup();
    let farm = deps.api.addr_make("farm0000");
    let env = mock_env();

    let err = execute(
        deps.as_mut(),
        env,
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: execute_action(
                farm.as_str(),
                Binary::default(),
                vec![Coin {
                    denom: "inj".to_string(),
                    amount: Uint128::zero(),
                }],
            ),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("zero-amount coin in funds"));
}

#[test]
fn cancel_clears_pending() {
    let (mut deps, owner) = setup();
    let target = deps.api.addr_make("farm0000");
    let env = mock_env();

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: migrate_action(target.as_str(), 99, Binary::default()),
        },
    )
    .unwrap();

    let outsider = deps.api.addr_make("outsider");
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Cancel {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Cancel {},
    )
    .unwrap();

    let pending: PendingActionResponse =
        from_json(query(deps.as_ref(), env, QueryMsg::PendingAction {}).unwrap()).unwrap();
    assert_eq!(pending.action, None);
}

#[test]
fn re_propose_resets_timer_and_overwrites_action() {
    let (mut deps, owner) = setup();
    let target = deps.api.addr_make("farm0000");
    let mut env = mock_env();

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: migrate_action(target.as_str(), 1, Binary::default()),
        },
    )
    .unwrap();

    env.block.time = env.block.time.plus_seconds(TIMELOCK - 100);

    // Switch from Migrate to Execute; timer + action both reset.
    let exec_msg = to_json_binary(&Uint128::from(1u128)).unwrap();
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            action: execute_action(target.as_str(), exec_msg.clone(), vec![]),
        },
    )
    .unwrap();

    let pending: PendingActionResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::PendingAction {}).unwrap()).unwrap();
    assert_eq!(
        pending.action,
        Some(execute_action(target.as_str(), exec_msg, vec![]))
    );
    assert_eq!(pending.effective_at, Some(env.block.time.seconds() + TIMELOCK));
}

#[test]
fn owner_rotation_timelocked() {
    let (mut deps, owner) = setup();
    let mut env = mock_env();
    let new_owner = deps.api.addr_make("new_owner");

    let outsider = deps.api.addr_make("outsider");
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner.to_string(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::ProposeNewOwner {
            new_owner: new_owner.to_string(),
        },
    )
    .unwrap();

    let pending: PendingOwnerRotationResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::PendingOwnerRotation {},
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(pending.pending_owner, Some(new_owner.to_string()));

    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::ApplyOwnerRotation {},
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("owner rotation timelock has not elapsed"));

    env.block.time = env.block.time.plus_seconds(TIMELOCK + 1);
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::ApplyOwnerRotation {},
    )
    .unwrap();

    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), env, QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.owner, new_owner.to_string());
}

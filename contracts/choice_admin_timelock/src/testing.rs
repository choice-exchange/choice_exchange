use cosmwasm_std::testing::{message_info, mock_dependencies, mock_env};
use cosmwasm_std::{
    from_json, to_json_binary, Binary, CosmosMsg, SubMsg, Uint128, WasmMsg,
};

use crate::contract::{
    execute, instantiate, query, ConfigResponse, ExecuteMsg, InstantiateMsg,
    PendingMigrationResponse, PendingOwnerRotationResponse, QueryMsg,
};
use crate::state::MIN_TIMELOCK_SECONDS;

const TIMELOCK: u64 = 48 * 60 * 60;

fn setup() -> (cosmwasm_std::OwnedDeps<
    cosmwasm_std::testing::MockStorage,
    cosmwasm_std::testing::MockApi,
    cosmwasm_std::testing::MockQuerier,
>, cosmwasm_std::Addr) {
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
fn propose_apply_full_path() {
    let (mut deps, owner) = setup();
    let target = deps.api.addr_make("farm0000");
    let mut env = mock_env();

    // Outsider can't propose.
    let outsider = deps.api.addr_make("outsider");
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Propose {
            contract: target.to_string(),
            code_id: 99,
            msg: Binary::default(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("unauthorized"));

    // code_id zero rejected.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            contract: target.to_string(),
            code_id: 0,
            msg: Binary::default(),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("code_id must be non-zero"));

    // Owner proposes.
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            contract: target.to_string(),
            code_id: 99,
            msg: to_json_binary(&Uint128::from(7u128)).unwrap(),
        },
    )
    .unwrap();

    let pending: PendingMigrationResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::PendingMigration {}).unwrap())
            .unwrap();
    assert_eq!(pending.contract, Some(target.to_string()));
    assert_eq!(pending.code_id, Some(99));
    assert_eq!(pending.effective_at, Some(env.block.time.seconds() + TIMELOCK));

    // Premature apply rejected.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("migration timelock has not elapsed"));

    // After timelock elapses, *anyone* can apply.
    env.block.time = env.block.time.plus_seconds(TIMELOCK + 1);
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&outsider, &[]),
        ExecuteMsg::Apply {},
    )
    .unwrap();

    // It dispatches WasmMsg::Migrate as itself.
    assert_eq!(
        res.messages,
        vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Migrate {
            contract_addr: target.to_string(),
            new_code_id: 99,
            msg: to_json_binary(&Uint128::from(7u128)).unwrap(),
        }))]
    );

    // Pending cleared.
    let pending: PendingMigrationResponse =
        from_json(query(deps.as_ref(), env, QueryMsg::PendingMigration {}).unwrap()).unwrap();
    assert_eq!(pending.contract, None);
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
            contract: target.to_string(),
            code_id: 99,
            msg: Binary::default(),
        },
    )
    .unwrap();

    // Outsider cancel rejected.
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

    let pending: PendingMigrationResponse =
        from_json(query(deps.as_ref(), env, QueryMsg::PendingMigration {}).unwrap()).unwrap();
    assert_eq!(pending.contract, None);
}

#[test]
fn re_propose_resets_timer() {
    let (mut deps, owner) = setup();
    let target = deps.api.addr_make("farm0000");
    let mut env = mock_env();

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            contract: target.to_string(),
            code_id: 1,
            msg: Binary::default(),
        },
    )
    .unwrap();

    env.block.time = env.block.time.plus_seconds(TIMELOCK - 100);

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Propose {
            contract: target.to_string(),
            code_id: 2,
            msg: Binary::default(),
        },
    )
    .unwrap();

    let pending: PendingMigrationResponse =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::PendingMigration {}).unwrap())
            .unwrap();
    assert_eq!(pending.code_id, Some(2));
    assert_eq!(pending.effective_at, Some(env.block.time.seconds() + TIMELOCK));
}

#[test]
fn owner_rotation_timelocked() {
    let (mut deps, owner) = setup();
    let mut env = mock_env();
    let new_owner = deps.api.addr_make("new_owner");

    // Outsider can't propose.
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

    // Premature apply rejected.
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

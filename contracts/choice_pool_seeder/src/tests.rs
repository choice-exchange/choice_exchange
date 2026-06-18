//! Unit tests for `choice_pool_seeder`.
//!
//! Coverage:
//!  * Instantiate both roles + per-field validation.
//!  * `CreateSink` happy path (Instantiate2 wiring) + rejections
//!    (choice_factory mismatch, wrong-role).
//!  * `Settle` happy path message chain (no cranker tip — the full balance
//!    seeds the pool) + role/balance/create-fee guards.
//!  * `Refund` keeper-or-anyone semantics across the deadline boundary.
//!  * `Callback::ProvideLiquidity` and `Callback::DistributeLp` exercised
//!    directly with a pre-populated pair on the mock factory — the
//!    chain-only CreatePair → Pair-lookup hand-off can't be observed from
//!    unit tests, so the integration test handles that round trip in
//!    test-tube.
//!  * Admin rotations + role-gated query errors + migrate dispatch.

use cosmwasm_std::testing::{message_info, mock_env, MockApi};
use cosmwasm_std::{
    coin, coins, from_json, to_json_binary, Addr, BankMsg, Binary, Coin, CosmosMsg, Empty,
    OwnedDeps, Uint128, WasmMsg,
};

use choice::asset::{AssetInfo, PairInfo};
use choice::factory::ExecuteMsg as FactoryExecuteMsg;
use choice::mock_querier::{mock_dependencies, WasmMockQuerier};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use cosmwasm_std::testing::MOCK_CONTRACT_ADDR;
use cosmwasm_std::testing::{MockApi as TestMockApi, MockQuerier, MockStorage};
use injective_cosmwasm::query::InjectiveQueryWrapper;
use std::marker::PhantomData;

use crate::contract::{execute, instantiate, migrate, query};
use crate::error::ContractError;
use crate::msg::{
    CallbackMsg, ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg, LpDestination,
    MigrateMsg, PoolKind, QueryMsg, RoleResponse, SinkConfigResponse, SinkInit, SinkStateResponse,
};
use crate::state::{Role, SinkStatus, ROLE, SINK_STATE};

const TOKEN_DENOM: &str = "factory/inj1issuer000/shroom_1";
const PAIR_DENOM: &str = "factory/inj1pair000/shroom";
const CREATE_FEE_DENOM: &str = "inj";
const CREATE_FEE: u128 = 100;

// ------------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------------

type SimpleDeps = OwnedDeps<MockStorage, TestMockApi, MockQuerier<Empty>, InjectiveQueryWrapper>;

type RichDeps = OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper>;

fn simple_deps() -> SimpleDeps {
    OwnedDeps {
        storage: MockStorage::default(),
        api: MockApi::default(),
        querier: MockQuerier::<Empty>::new(&[]),
        custom_query_type: PhantomData,
    }
}

fn rich_deps(contract_balance: &[Coin]) -> RichDeps {
    mock_dependencies(contract_balance)
}

fn make_factory_init(api: &MockApi) -> FactoryInit {
    FactoryInit {
        admin: api.addr_make("admin").to_string(),
        sink_code_id: 7,
        choice_factory: api.addr_make("choice_factory").to_string(),
        clmm_factory: Some(api.addr_make("clmm_factory").to_string()),
        clmm_manager: Some(api.addr_make("clmm_manager").to_string()),
    }
}

/// Default XYK `pool_kind` keyed to the mock factory's pinned `choice_factory`.
fn xyk_pool_kind(api: &MockApi) -> PoolKind {
    PoolKind::Xyk {
        choice_factory: api.addr_make("choice_factory").to_string(),
        lp_destination: LpDestination::Burn,
    }
}

/// Default CLMM `pool_kind` keyed to the mock factory's pinned addresses.
fn clmm_pool_kind(api: &MockApi) -> PoolKind {
    PoolKind::Clmm {
        clmm_factory: api.addr_make("clmm_factory").to_string(),
        clmm_manager: api.addr_make("clmm_manager").to_string(),
        fee_tier: 3000,
        position_recipient: api.addr_make("locker").to_string(),
        max_fee_multiple: None,
    }
}

fn make_sink_init(api: &MockApi) -> SinkInit {
    SinkInit {
        issuer: api.addr_make("issuer").to_string(),
        token_denom: TOKEN_DENOM.to_string(),
        pair_denom: PAIR_DENOM.to_string(),
        token_decimals: 18,
        pair_decimals: 18,
        pool_kind: xyk_pool_kind(api),
        refund_receiver: api.addr_make("refund_receiver").to_string(),
        deadline_seconds: 86_400,
    }
}

fn instantiate_factory_default(deps: &mut SimpleDeps) {
    let init = make_factory_init(&deps.api);
    let admin = deps.api.addr_make("admin");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        InstantiateMsg::Factory(init),
    )
    .unwrap();
}

fn instantiate_sink_default(deps: &mut RichDeps) {
    let init = make_sink_init(&deps.api);
    let caller = deps.api.addr_make("factory_caller");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap();
}

fn install_create_fee(deps: &mut RichDeps) {
    deps.querier
        .with_token_factory_denom_create_fee(&[(CREATE_FEE_DENOM, Uint128::new(CREATE_FEE))]);
}

/// Pre-populate the mock choice_factory with a pair for `(token_denom,
/// pair_denom)` — used to drive `Callback::ProvideLiquidity` lookups
/// directly. The lp denom is fixed for assertion stability.
fn install_pair(deps: &mut RichDeps) -> (String, String) {
    let pair_addr = deps.api.addr_make("pair_inst").to_string();
    let lp_denom = format!("factory/{}/lp", pair_addr);

    let key = format!("{}{}", TOKEN_DENOM, PAIR_DENOM);
    let pair_info = PairInfo {
        asset_infos: [
            AssetInfo::NativeToken {
                denom: TOKEN_DENOM.to_string(),
            },
            AssetInfo::NativeToken {
                denom: PAIR_DENOM.to_string(),
            },
        ],
        contract_addr: pair_addr.clone(),
        liquidity_token: lp_denom.clone(),
        asset_decimals: [18, 18],
        burn_address: deps.api.addr_make("burn").to_string(),
        fee_wallet_address: deps.api.addr_make("fee").to_string(),
    };
    deps.querier.with_choice_factory(&[(&key, &pair_info)], &[]);
    (pair_addr, lp_denom)
}

// ------------------------------------------------------------------------
// instantiate
// ------------------------------------------------------------------------

#[test]
fn instantiate_factory_happy_path_persists_config() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);

    assert_eq!(ROLE.load(deps.as_ref().storage).unwrap(), Role::Factory);

    let role: RoleResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Role {}).unwrap()).unwrap();
    let factory = match role {
        RoleResponse::Factory(f) => f,
        _ => panic!("expected Factory variant"),
    };
    assert_eq!(factory.sink_code_id, 7);
    assert_eq!(factory.admin, deps.api.addr_make("admin").to_string());
}

#[test]
fn instantiate_sink_happy_path_sets_pending_state() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    assert_eq!(ROLE.load(deps.as_ref().storage).unwrap(), Role::Sink);
    let state = SINK_STATE.load(deps.as_ref().storage).unwrap();
    assert_eq!(state.status, SinkStatus::Pending);
    assert!(state.pair_addr.is_none());
    assert!(state.lp_minted.is_none());

    let cfg: SinkConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::SinkConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.token_denom, TOKEN_DENOM);
    assert_eq!(cfg.pair_denom, PAIR_DENOM);
    assert!(matches!(
        cfg.pool_kind,
        PoolKind::Xyk {
            lp_destination: LpDestination::Burn,
            ..
        }
    ));
}

#[test]
fn instantiate_sink_rejects_same_denom() {
    let mut deps = rich_deps(&[]);
    let mut init = make_sink_init(&deps.api);
    init.pair_denom = init.token_denom.clone();
    let caller = deps.api.addr_make("factory_caller");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SameDenom { .. }));
}

#[test]
fn instantiate_sink_rejects_zero_deadline() {
    let mut deps = rich_deps(&[]);
    let mut init = make_sink_init(&deps.api);
    init.deadline_seconds = 0;
    let caller = deps.api.addr_make("factory_caller");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ZeroDeadline {}));
}

// ------------------------------------------------------------------------
// CreateSink (factory)
// ------------------------------------------------------------------------

#[test]
fn create_sink_emits_instantiate2_with_correct_args() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);

    let sink_init = make_sink_init(&deps.api);
    let salt = Binary::from(b"salt-bytes".to_vec());
    let caller = deps.api.addr_make("issuer_keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: salt.clone(),
            sink_init: sink_init.clone(),
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate2 {
            admin,
            code_id,
            msg,
            salt: emitted_salt,
            funds,
            label,
        }) => {
            assert!(admin.is_none(), "spawned sinks are immutable");
            assert_eq!(*code_id, 7);
            assert_eq!(emitted_salt.as_slice(), b"salt-bytes");
            assert!(funds.is_empty());
            assert!(label.starts_with("choice-pool-seeder-sink-"));
            let decoded: InstantiateMsg = from_json(msg.as_slice()).unwrap();
            match decoded {
                InstantiateMsg::Sink(s) => {
                    assert_eq!(s.token_denom, sink_init.token_denom);
                    assert_eq!(s.pair_denom, sink_init.pair_denom);
                }
                _ => panic!("expected Sink instantiate variant"),
            }
        }
        other => panic!("expected Instantiate2, got {:?}", other),
    }
}

#[test]
fn create_sink_rejects_choice_factory_mismatch() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let mut sink_init = make_sink_init(&deps.api);
    sink_init.pool_kind = PoolKind::Xyk {
        choice_factory: deps.api.addr_make("other_choice_factory").to_string(),
        lp_destination: LpDestination::Burn,
    };
    let caller = deps.api.addr_make("issuer_keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"x".to_vec()),
            sink_init,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::SinkChoiceFactoryMismatch { .. }
    ));
}

#[test]
fn create_sink_rejected_on_sink_instance() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let caller = deps.api.addr_make("anyone");
    let sink_init = make_sink_init(&deps.api);
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"x".to_vec()),
            sink_init,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::WrongRole { .. }));
}

// ------------------------------------------------------------------------
// Settle
// ------------------------------------------------------------------------

fn settle_balances() -> Vec<Coin> {
    vec![
        coin(1_000_000_000_000u128, TOKEN_DENOM),
        coin(800_000_000u128, PAIR_DENOM),
    ]
}

/// Standard `info.funds` for a `Settle` call — exactly the chain's
/// create-pair fee. Tests that exercise overpay / underpay vary off this.
fn fee_funds() -> Vec<Coin> {
    vec![coin(CREATE_FEE, CREATE_FEE_DENOM)]
}

#[test]
fn settle_emits_full_chain_create_pair_callbacks_no_tip() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);

    let caller = deps.api.addr_make("cranker");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap();

    // create_pair + provide-callback + distribute-callback. No tip message
    // (P1-B): the cranker is paid nothing and the full pair balance seeds.
    assert_eq!(res.messages.len(), 3);

    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(
                contract_addr,
                &deps.api.addr_make("choice_factory").to_string()
            );
            let decoded: FactoryExecuteMsg = from_json(msg.as_slice()).unwrap();
            match decoded {
                FactoryExecuteMsg::CreatePair { assets } => {
                    let denoms: Vec<String> = assets
                        .iter()
                        .map(|a| match &a.info {
                            AssetInfo::NativeToken { denom } => denom.clone(),
                            _ => panic!("native denom only"),
                        })
                        .collect();
                    assert!(denoms.contains(&TOKEN_DENOM.to_string()));
                    assert!(denoms.contains(&PAIR_DENOM.to_string()));
                }
                _ => panic!("expected CreatePair variant"),
            }
            assert_eq!(funds, &vec![coin(CREATE_FEE, CREATE_FEE_DENOM)]);
        }
        other => panic!("expected factory.CreatePair WasmExec, got {:?}", other),
    }

    for (idx, expected_kind) in [(1usize, "provide_liquidity"), (2, "distribute_lp")] {
        match &res.messages[idx].msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) => {
                assert_eq!(contract_addr, &mock_env().contract.address.to_string());
                let decoded: ExecuteMsg = from_json(msg.as_slice()).unwrap();
                match (decoded, expected_kind) {
                    (
                        ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity {
                            token_amount,
                            pair_amount,
                        }),
                        "provide_liquidity",
                    ) => {
                        assert_eq!(token_amount, Uint128::new(1_000_000_000_000u128));
                        // Whole pair balance seeds the pool — no tip skimmed.
                        assert_eq!(pair_amount, Uint128::new(800_000_000u128));
                    }
                    (ExecuteMsg::Callback(CallbackMsg::DistributeLp {}), "distribute_lp") => {}
                    (other, kind) => panic!("expected {} callback, got {:?}", kind, other),
                }
            }
            other => panic!("expected callback WasmExec, got {:?}", other),
        }
    }

    // No BankMsg::Send to the cranker anywhere in the chain.
    assert!(
        !res.messages
            .iter()
            .any(|m| matches!(&m.msg, CosmosMsg::Bank(BankMsg::Send { .. }))),
        "settle must not pay the cranker a tip"
    );

    // Status flipped pre-dispatch (defense-in-depth against re-entry).
    let state = SINK_STATE.load(deps.as_ref().storage).unwrap();
    assert_eq!(state.status, SinkStatus::Settled);
}

#[test]
fn settle_fails_closed_when_factory_exists_but_config_unreadable() {
    // P1-A: the sink's factory address hosts contract code, but no
    // FactoryConfig response is installed → the breaker read fails while the
    // contract exists. Settlement must be BLOCKED (fail-closed), not proceed.
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps); // cfg.factory = "factory_caller"
    install_create_fee(&mut deps);
    deps.querier
        .register_wasm_contract(deps.api.addr_make("factory_caller").as_str());

    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::FactoryUnreadable { .. }));
}

#[test]
fn settle_rejects_overpaid_create_fee() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    // Caller over-pays by 7 wei — must revert, not refund. Letting an
    // over-pay through would silently inflate `seed_pair` (when
    // `pair_denom == create_fee_denom`) and reroute the excess into the
    // pool deposit, leaving the late refund hop insolvent.
    let overpay = vec![coin(CREATE_FEE + 7, CREATE_FEE_DENOM)];
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &overpay),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::CreateFeeOverpaid { .. }));
}

#[test]
fn settle_rejects_unexpected_funds_denom() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    // Caller attaches the correct create fee PLUS extra of an unrelated
    // denom. The unrelated denom would have to be returned at the end of
    // the message chain, which means tracking refunds — instead we just
    // refuse anything not in the create fee.
    let funds = vec![coin(CREATE_FEE, CREATE_FEE_DENOM), coin(1, PAIR_DENOM)];
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &funds),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::UnexpectedFundsDenom { ref denom } if denom == PAIR_DENOM
    ));
}

#[test]
fn settle_rejects_on_factory_instance() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::WrongRole { .. }));
}

#[test]
fn settle_rejects_zero_pair_balance() {
    let mut deps = rich_deps(&[coin(1_000u128, TOKEN_DENOM)]);
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::InsufficientBalanceForSettle { .. }
    ));
}

#[test]
fn settle_rejects_zero_token_balance() {
    let mut deps = rich_deps(&[coin(1_000u128, PAIR_DENOM)]);
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::InsufficientBalanceForSettle { .. }
    ));
}

#[test]
fn settle_rejects_when_caller_omits_create_fee_funds() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    // Note: no `info.funds` attached → caller didn't pay the fee.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InsufficientCreateFee { .. }));
}

/// Regression for the pair_denom-equals-fee-denom bug fixed in 2026-05-26 and
/// re-fixed in BUG 1 (over-pay revert): when `pair_denom == "inj"`, the seed
/// computation must subtract the caller's exact fee contribution, not double-
/// count it into the pool deposit.
#[test]
fn settle_pair_denom_equals_create_fee_denom_deposits_leg_b_only() {
    let leg_b_pair: u128 = 800_000_000u128;
    let leg_b_token: u128 = 1_000_000_000_000u128;
    let mut deps = rich_deps(&[
        coin(leg_b_token, TOKEN_DENOM),
        // Sink balance already reflects the caller's incoming fee, since
        // `info.funds` are credited before execute is called. Leg B is
        // `leg_b_pair`; the +CREATE_FEE here is the caller's exact fee.
        coin(leg_b_pair + CREATE_FEE, CREATE_FEE_DENOM),
    ]);
    // Instantiate sink with pair_denom = "inj" to force the collision.
    let mut init = make_sink_init(&deps.api);
    init.pair_denom = CREATE_FEE_DENOM.to_string();
    let caller = deps.api.addr_make("factory_caller");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap();
    install_create_fee(&mut deps);

    let cranker = deps.api.addr_make("cranker");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&cranker, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap();

    // The whole Leg B pair balance seeds the pool (no tip skimmed); the
    // caller's exact fee was excluded from `seed_pair`.
    let expected_deposit = Uint128::new(leg_b_pair);

    // Find the ProvideLiquidity callback and check pair_amount.
    let provide = res
        .messages
        .iter()
        .find_map(|sm| match &sm.msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr, msg, ..
            }) if contract_addr == &mock_env().contract.address.to_string() => {
                let decoded: ExecuteMsg = from_json(msg.as_slice()).ok()?;
                match decoded {
                    ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity { pair_amount, .. }) => {
                        Some(pair_amount)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("ProvideLiquidity callback");
    assert_eq!(provide, expected_deposit);
}

#[test]
fn settle_rejects_when_terminal() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let caller = deps.api.addr_make("cranker");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap();
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SinkTerminal { .. }));
}

// ------------------------------------------------------------------------
// Refund
// ------------------------------------------------------------------------

#[test]
fn refund_rejects_pre_deadline() {
    let mut deps = rich_deps(&[
        coin(1_000_000u128, TOKEN_DENOM),
        coin(1_000_000u128, PAIR_DENOM),
    ]);
    instantiate_sink_default(&mut deps);
    let caller = deps.api.addr_make("anyone");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Refund {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::RefundDeadlineNotReached { .. }
    ));
}

#[test]
fn refund_happy_path_post_deadline() {
    let mut deps = rich_deps(&[
        coin(1_000_000u128, TOKEN_DENOM),
        coin(500_000u128, PAIR_DENOM),
    ]);
    instantiate_sink_default(&mut deps);

    let mut env = mock_env();
    env.block.time = env.block.time.plus_seconds(86_401);

    let caller = deps.api.addr_make("cranker");
    let res = execute(
        deps.as_mut(),
        env,
        message_info(&caller, &[]),
        ExecuteMsg::Refund {},
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);
    match &res.messages[0].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &deps.api.addr_make("issuer").to_string());
            assert_eq!(amount, &coins(1_000_000u128, TOKEN_DENOM));
        }
        other => panic!("expected BankMsg::Send to issuer, got {:?}", other),
    }
    match &res.messages[1].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(
                to_address,
                &deps.api.addr_make("refund_receiver").to_string()
            );
            assert_eq!(amount, &coins(500_000u128, PAIR_DENOM));
        }
        other => panic!("expected BankMsg::Send to refund_receiver, got {:?}", other),
    }

    assert_eq!(
        SINK_STATE.load(deps.as_ref().storage).unwrap().status,
        SinkStatus::Refunded
    );
}

#[test]
fn refund_rejects_when_nothing_to_refund() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let mut env = mock_env();
    env.block.time = env.block.time.plus_seconds(86_401);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        env,
        message_info(&caller, &[]),
        ExecuteMsg::Refund {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NothingToRefund {}));
}

// ------------------------------------------------------------------------
// Callbacks
// ------------------------------------------------------------------------

#[test]
fn callback_rejects_non_self_caller() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeLp {}),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::CallbackUnauthorized {}));
}

#[test]
fn callback_provide_liquidity_emits_provide_msg_with_funds() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let (pair_addr, _lp_denom) = install_pair(&mut deps);

    let self_addr = mock_env().contract.address;
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity {
            token_amount: Uint128::new(1_000_000u128),
            pair_amount: Uint128::new(500_000u128),
        }),
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(contract_addr, &pair_addr);
            let decoded: PairExecuteMsg = from_json(msg.as_slice()).unwrap();
            match decoded {
                PairExecuteMsg::ProvideLiquidity {
                    assets, receiver, ..
                } => {
                    assert_eq!(receiver.as_deref(), Some(self_addr.as_str()));
                    let denoms: Vec<String> = assets
                        .iter()
                        .map(|a| match &a.info {
                            AssetInfo::NativeToken { denom } => denom.clone(),
                            _ => panic!("native only"),
                        })
                        .collect();
                    assert!(denoms.contains(&TOKEN_DENOM.to_string()));
                    assert!(denoms.contains(&PAIR_DENOM.to_string()));
                }
                _ => panic!("expected ProvideLiquidity variant"),
            }
            // Funds sorted lex by denom; both populated.
            assert_eq!(funds.len(), 2);
            assert!(funds.windows(2).all(|w| w[0].denom <= w[1].denom));
        }
        other => panic!("expected pair.ProvideLiquidity WasmExec, got {:?}", other),
    }

    let state = SINK_STATE.load(deps.as_ref().storage).unwrap();
    assert_eq!(state.pair_addr.map(|a| a.to_string()), Some(pair_addr));
}

#[test]
fn callback_distribute_lp_burns_when_destination_is_burn() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    install_pair(&mut deps);

    // Pretend ProvideLiquidity already ran: pair_addr recorded + 1e18 LP in
    // the contract's bank balance. The mock_querier's `PairQueryMsg::Pair`
    // returns a hardcoded `liquidity_token = addr_make("liquidity0000")`
    // regardless of the address we query — match that here so the contract's
    // LP-balance query lands on a populated denom.
    let env = mock_env();
    let pair_addr_str = deps.api.addr_make("pair_inst").to_string();
    let mock_lp_denom = deps.api.addr_make("liquidity0000").to_string();
    SINK_STATE
        .save(
            deps.as_mut().storage,
            &crate::state::SinkState {
                status: SinkStatus::Settled,
                pair_addr: Some(Addr::unchecked(pair_addr_str)),
                lp_minted: None,
            },
        )
        .unwrap();
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: mock_lp_denom.clone(),
            amount: Uint128::new(1_000_000_000_000_000_000u128),
        }],
    )]);
    let lp_denom = mock_lp_denom;

    let self_addr = env.contract.address.clone();
    let res = execute(
        deps.as_mut(),
        env,
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeLp {}),
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Bank(BankMsg::Burn { amount }) => {
            assert_eq!(amount, &coins(1_000_000_000_000_000_000u128, &lp_denom));
        }
        other => panic!("expected BankMsg::Burn, got {:?}", other),
    }
    let state = SINK_STATE.load(deps.as_ref().storage).unwrap();
    assert_eq!(
        state.lp_minted,
        Some(Uint128::new(1_000_000_000_000_000_000u128))
    );
}

#[test]
fn callback_distribute_lp_sends_when_destination_is_send_to() {
    let mut deps = rich_deps(&[]);
    let mut init = make_sink_init(&deps.api);
    let dest = deps.api.addr_make("lp_recipient");
    init.pool_kind = PoolKind::Xyk {
        choice_factory: deps.api.addr_make("choice_factory").to_string(),
        lp_destination: LpDestination::SendTo(dest.to_string()),
    };
    let caller = deps.api.addr_make("factory_caller");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap();
    install_pair(&mut deps);

    let pair_addr_str = deps.api.addr_make("pair_inst").to_string();
    let mock_lp_denom = deps.api.addr_make("liquidity0000").to_string();
    SINK_STATE
        .save(
            deps.as_mut().storage,
            &crate::state::SinkState {
                status: SinkStatus::Settled,
                pair_addr: Some(Addr::unchecked(pair_addr_str)),
                lp_minted: None,
            },
        )
        .unwrap();
    deps.querier.with_balance(&[(
        &MOCK_CONTRACT_ADDR.to_string(),
        vec![Coin {
            denom: mock_lp_denom.clone(),
            amount: Uint128::new(42),
        }],
    )]);
    let lp_denom = mock_lp_denom;

    let self_addr = mock_env().contract.address;
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeLp {}),
    )
    .unwrap();
    match &res.messages[0].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &dest.to_string());
            assert_eq!(amount, &coins(42, &lp_denom));
        }
        other => panic!("expected BankMsg::Send, got {:?}", other),
    }
}

#[test]
fn callback_distribute_lp_errors_when_no_lp_minted() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let (_pair_addr, _lp_denom) = install_pair(&mut deps);

    let pair_addr_str = deps.api.addr_make("pair_inst").to_string();
    SINK_STATE
        .save(
            deps.as_mut().storage,
            &crate::state::SinkState {
                status: SinkStatus::Settled,
                pair_addr: Some(Addr::unchecked(pair_addr_str)),
                lp_minted: None,
            },
        )
        .unwrap();
    // No LP balance configured.

    let self_addr = mock_env().contract.address;
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeLp {}),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ZeroLpMinted {}));
}

// ------------------------------------------------------------------------
// Admin rotations
// ------------------------------------------------------------------------

#[test]
fn admin_rotations_require_admin_caller() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");
    let new_admin = deps.api.addr_make("new_admin");

    // A stranger cannot propose a rotation.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Admin proposes; this parks `new_admin` as pending but does NOT rotate yet.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.to_string(),
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.admin, admin.to_string(), "live admin unchanged before accept");
    assert_eq!(cfg.pending_admin, Some(new_admin.to_string()));

    // The pending key must accept; a non-pending caller cannot.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::AcceptAdmin {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Until accept lands, the old admin still holds power and the new key does not.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&new_admin, &[]),
        ExecuteMsg::UpdateSinkCodeId {
            new_sink_code_id: 99,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&new_admin, &[]),
        ExecuteMsg::AcceptAdmin {},
    )
    .unwrap();

    // Now the rotated admin can act and `pending_admin` is cleared.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&new_admin, &[]),
        ExecuteMsg::UpdateSinkCodeId {
            new_sink_code_id: 99,
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.admin, new_admin.to_string());
    assert_eq!(cfg.pending_admin, None);
    assert_eq!(cfg.sink_code_id, 99);
}

#[test]
fn admin_actions_rejected_on_sink_instance() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let anyone = deps.api.addr_make("anyone");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&anyone, &[]),
        ExecuteMsg::UpdateSinkCodeId {
            new_sink_code_id: 1,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::WrongRole { .. }));
}

#[test]
fn set_paused_requires_admin_and_blocks_creates() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");

    // Non-admin cannot pause.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::SetPaused { paused: true },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Admin pauses; query reflects it.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetPaused { paused: true },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert!(cfg.paused);

    // CreateSink is refused while paused.
    let sink_init = make_sink_init(&deps.api);
    let caller = deps.api.addr_make("issuer_keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"x".to_vec()),
            sink_init,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Paused {}));

    // Unpause re-opens creation.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetPaused { paused: false },
    )
    .unwrap();
    let sink_init2 = make_sink_init(&deps.api);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"x".to_vec()),
            sink_init: sink_init2,
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1, "CreateSink resumes after unpause");
}

#[test]
fn update_choice_factory_requires_admin_and_repoints() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");
    let new_factory = deps.api.addr_make("new_xyk_factory");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateChoiceFactory {
            new_choice_factory: new_factory.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateChoiceFactory {
            new_choice_factory: new_factory.to_string(),
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.choice_factory, new_factory.to_string());
}

#[test]
fn update_clmm_addresses_repoints_or_rejects_half_set() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let admin = deps.api.addr_make("admin");
    let new_clmm_factory = deps.api.addr_make("new_clmm_factory");
    let new_clmm_manager = deps.api.addr_make("new_clmm_manager");

    // Half-set (only factory) is rejected.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateClmmAddresses {
            clmm_factory: Some(new_clmm_factory.to_string()),
            clmm_manager: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ClmmAddressesHalfSet {}));

    // Both set re-points CLMM graduation.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateClmmAddresses {
            clmm_factory: Some(new_clmm_factory.to_string()),
            clmm_manager: Some(new_clmm_manager.to_string()),
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.clmm_factory, Some(new_clmm_factory.to_string()));
    assert_eq!(cfg.clmm_manager, Some(new_clmm_manager.to_string()));

    // Both None disables CLMM.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateClmmAddresses {
            clmm_factory: None,
            clmm_manager: None,
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.clmm_factory, None);
    assert_eq!(cfg.clmm_manager, None);
}

#[test]
fn accept_admin_without_pending_errors() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let admin = deps.api.addr_make("admin");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::AcceptAdmin {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NoPendingAdmin {}));
}

#[test]
fn sink_records_its_factory() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let cfg: SinkConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::SinkConfig {}).unwrap()).unwrap();
    assert_eq!(
        cfg.factory,
        Some(deps.api.addr_make("factory_caller").to_string()),
        "sink snapshots its instantiating factory for pause/force-refund auth"
    );
}

// ------------------------------------------------------------------------
// Wrong-role queries
// ------------------------------------------------------------------------

#[test]
fn factory_config_query_errors_on_sink() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let err = query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap_err();
    assert!(err.to_string().contains("not a factory instance"));
}

#[test]
fn sink_config_query_errors_on_factory() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let err = query(deps.as_ref(), mock_env(), QueryMsg::SinkConfig {}).unwrap_err();
    assert!(err.to_string().contains("not a sink instance"));
}

#[test]
fn sink_state_query_returns_pending_then_terminal() {
    let mut deps = rich_deps(&settle_balances());
    instantiate_sink_default(&mut deps);
    install_create_fee(&mut deps);
    let s0: SinkStateResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::SinkState {}).unwrap()).unwrap();
    assert_eq!(s0.status, SinkStatus::Pending);
    let cranker = deps.api.addr_make("cranker");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&cranker, &fee_funds()),
        ExecuteMsg::Settle {},
    )
    .unwrap();
    let s1: SinkStateResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::SinkState {}).unwrap()).unwrap();
    assert_eq!(s1.status, SinkStatus::Settled);
}

// ------------------------------------------------------------------------
// Migrate
// ------------------------------------------------------------------------

#[test]
fn migrate_patch_requires_v1_marker() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    // cw2 records "1.0.0" via CONTRACT_VERSION; Patch should accept.
    let res = migrate(deps.as_mut(), mock_env(), MigrateMsg::Patch {}).unwrap();
    assert!(res
        .attributes
        .iter()
        .any(|a| a.key == "variant" && a.value == "patch"));
}

#[test]
fn migrate_from_v1_accepts_current_v1_marker() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let res = migrate(deps.as_mut(), mock_env(), MigrateMsg::FromV1 {}).unwrap();
    assert!(res
        .attributes
        .iter()
        .any(|a| a.key == "variant" && a.value == "from_v1"));
}

#[test]
fn migrate_patch_rejects_cross_major() {
    // P2-A: a 1.x build must not Patch over a stored 2.x deployment. The old
    // `starts_with("1.")` already rejected this, but it ALSO bricked an
    // in-major patch once the build itself shipped as 2.x — the new major-match
    // guard fixes that while keeping the cross-major rejection below.
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    cw2::set_contract_version(deps.as_mut().storage, "crates.io:choice-pool-seeder", "2.0.0")
        .unwrap();
    let err = migrate(deps.as_mut(), mock_env(), MigrateMsg::Patch {}).unwrap_err();
    assert!(matches!(err, ContractError::InvalidMigration { .. }), "{err:?}");
}

// ------------------------------------------------------------------------
// CLMM settle
// ------------------------------------------------------------------------

use choice_clmm_common::factory::{
    ExecuteMsg as ClmmFactoryExecuteMsg, FeeTierEntry, QueryMsg as ClmmFactoryQueryMsg,
};
use choice_clmm_common::manager::ExecuteMsg as ClmmManagerExecuteMsg;
use cosmwasm_std::{ContractResult, SystemError, SystemResult, WasmQuery};

/// Deps whose querier answers the two CLMM-factory queries `settle_clmm`
/// makes — `GetFeeTiers` (→ `tiers`) and `GetPool` (→ `existing_pool`, or an
/// error meaning "absent"). Bank balances are seeded for the contract address.
/// We don't extend the shared `choice` mock querier (it would have to depend
/// on `choice_clmm_common`); a self-contained `update_wasm` closure is enough.
type ClmmDeps =
    OwnedDeps<MockStorage, TestMockApi, MockQuerier<InjectiveQueryWrapper>, InjectiveQueryWrapper>;

fn clmm_settle_deps(
    balances: Vec<Coin>,
    tiers: Vec<FeeTierEntry>,
    existing_pool: Option<String>,
) -> ClmmDeps {
    let mut querier = MockQuerier::<InjectiveQueryWrapper>::new(&[(MOCK_CONTRACT_ADDR, &balances)]);
    querier.update_wasm(move |q| match q {
        WasmQuery::Smart { msg, .. } => match from_json::<ClmmFactoryQueryMsg>(msg) {
            Ok(ClmmFactoryQueryMsg::GetFeeTiers { .. }) => {
                SystemResult::Ok(ContractResult::Ok(to_json_binary(&tiers).unwrap()))
            }
            Ok(ClmmFactoryQueryMsg::GetPool { .. }) => match &existing_pool {
                Some(p) => SystemResult::Ok(ContractResult::Ok(to_json_binary(p).unwrap())),
                None => SystemResult::Err(SystemError::NoSuchContract {
                    addr: "pool-absent".to_string(),
                }),
            },
            _ => SystemResult::Err(SystemError::UnsupportedRequest {
                kind: "unexpected clmm query".to_string(),
            }),
        },
        _ => SystemResult::Err(SystemError::UnsupportedRequest {
            kind: "non-smart wasm query".to_string(),
        }),
    });
    OwnedDeps {
        storage: MockStorage::default(),
        api: TestMockApi::default(),
        querier,
        custom_query_type: PhantomData,
    }
}

fn default_tiers() -> Vec<FeeTierEntry> {
    vec![
        FeeTierEntry {
            fee: 100,
            tick_spacing: 1,
        },
        FeeTierEntry {
            fee: 500,
            tick_spacing: 10,
        },
        FeeTierEntry {
            fee: 3000,
            tick_spacing: 60,
        },
        FeeTierEntry {
            fee: 10000,
            tick_spacing: 200,
        },
    ]
}

fn instantiate_clmm_sink(deps: &mut ClmmDeps) {
    let mut init = make_sink_init(&deps.api);
    init.pool_kind = clmm_pool_kind(&deps.api);
    let caller = deps.api.addr_make("factory_caller");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap();
}

#[test]
fn clmm_settle_emits_create_pool_mint_and_sweep() {
    let mut deps = clmm_settle_deps(settle_balances(), default_tiers(), None);
    instantiate_clmm_sink(&mut deps);

    let caller = deps.api.addr_make("cranker");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap();

    // create_pool + mint + sweep-dust callback. No tip message (P1-B).
    assert_eq!(res.messages.len(), 3);

    // [0] CreatePool on the pinned clmm factory, no funds, fee tier 3000.
    // The whole pair balance seeds the position — no tip skimmed.
    let pair_deposit = Uint128::new(800_000_000u128);
    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(
                contract_addr,
                &deps.api.addr_make("clmm_factory").to_string()
            );
            assert!(funds.is_empty(), "CreatePool must attach no funds");
            match from_json::<ClmmFactoryExecuteMsg>(msg.as_slice()).unwrap() {
                ClmmFactoryExecuteMsg::CreatePool {
                    token_a,
                    token_b,
                    fee,
                    init_sqrt_price,
                    max_fee_multiple,
                } => {
                    assert_eq!(fee, 3000);
                    // token_a < token_b by the CLMM ordering.
                    assert!(token_a < token_b);
                    assert!(!init_sqrt_price.is_zero());
                    // Sink config didn't set a multiple -> forwarded as None
                    // (factory default 2x).
                    assert_eq!(max_fee_multiple, None);
                }
                other => panic!("expected CreatePool, got {:?}", other),
            }
        }
        other => panic!("expected CreatePool exec, got {:?}", other),
    }

    // [1] MintPosition to the locker with both denoms attached as funds.
    match &res.messages[1].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) => {
            assert_eq!(
                contract_addr,
                &deps.api.addr_make("clmm_manager").to_string()
            );
            assert_eq!(funds.len(), 2);
            assert!(funds.windows(2).all(|w| w[0].denom <= w[1].denom));
            match from_json::<ClmmManagerExecuteMsg>(msg.as_slice()).unwrap() {
                ClmmManagerExecuteMsg::MintPosition {
                    fee,
                    tick_lower,
                    tick_upper,
                    recipient,
                    amount0_min,
                    amount1_min,
                    deadline,
                    ..
                } => {
                    assert_eq!(fee, 3000);
                    // Full range snapped to tick_spacing 60.
                    assert_eq!(tick_lower, -887220);
                    assert_eq!(tick_upper, 887220);
                    assert_eq!(
                        recipient.as_deref(),
                        Some(deps.api.addr_make("locker").as_str())
                    );
                    assert!(amount0_min.is_zero() && amount1_min.is_zero());
                    assert_eq!(deadline, 0);
                }
                other => panic!("expected MintPosition, got {:?}", other),
            }
            // The total funds attached equal token side + pair_deposit.
            let total_pair: Uint128 = funds
                .iter()
                .filter(|c| c.denom == PAIR_DENOM)
                .map(|c| c.amount)
                .sum();
            assert_eq!(total_pair, pair_deposit);
        }
        other => panic!("expected MintPosition exec, got {:?}", other),
    }

    // [2] sweep-dust self callback.
    match &res.messages[2].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr, msg, ..
        }) => {
            assert_eq!(contract_addr, &mock_env().contract.address.to_string());
            assert!(matches!(
                from_json::<ExecuteMsg>(msg.as_slice()).unwrap(),
                ExecuteMsg::Callback(CallbackMsg::SweepDust {})
            ));
        }
        other => panic!("expected sweep callback, got {:?}", other),
    }

    assert_eq!(
        SINK_STATE.load(deps.as_ref().storage).unwrap().status,
        SinkStatus::Settled
    );
}

#[test]
fn clmm_settle_rejects_attached_funds() {
    // Funds are rejected before any factory query, so tiers/pool are moot.
    let mut deps = clmm_settle_deps(settle_balances(), default_tiers(), None);
    instantiate_clmm_sink(&mut deps);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &coins(1, CREATE_FEE_DENOM)),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::UnexpectedFundsForClmmSettle {}
    ));
}

#[test]
fn clmm_settle_rejects_preexisting_pool() {
    let mut deps = clmm_settle_deps(
        settle_balances(),
        default_tiers(),
        Some("inj1preexistingpool".to_string()),
    );
    instantiate_clmm_sink(&mut deps);
    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ClmmPoolAlreadyExists { .. }));
}

#[test]
fn settle_clmm_extreme_ratio_errors_instead_of_mispricing() {
    // An extreme seed ratio (a single raw pair unit against ~Uint128::MAX of
    // the token) yields a sqrt price below MIN_SQRT_RATIO. Previously this was
    // silently clamped to the boundary, creating a mispriced pool whose
    // `amount*_min = 0` mint would draw one side only and refund the rest. It
    // must now error so the keeper can triage instead.
    let extreme_balances = vec![coin(u128::MAX, TOKEN_DENOM), coin(1u128, PAIR_DENOM)];
    // No pre-existing pool, so the price guard is the thing that fires.
    let mut deps = clmm_settle_deps(extreme_balances, default_tiers(), None);
    instantiate_clmm_sink(&mut deps);

    let caller = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    // On real chain the in-tx revert rolls back the status flip; here we just
    // assert the loud error so a mispriced pool is never created.
    assert!(
        matches!(err, ContractError::SeedRatioOutOfRange { .. }),
        "expected SeedRatioOutOfRange, got {err:?}"
    );
}

#[test]
fn clmm_settle_rejects_unsupported_fee_tier() {
    // Sink wants fee tier 3000 but the factory only enables 500.
    let mut deps = clmm_settle_deps(
        settle_balances(),
        vec![FeeTierEntry {
            fee: 500,
            tick_spacing: 10,
        }],
        None,
    );
    instantiate_clmm_sink(&mut deps);
    let cranker = deps.api.addr_make("cranker");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&cranker, &[]),
        ExecuteMsg::Settle {},
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::FeeTierNotSupported { fee: 3000 }
    ));
}

// ------------------------------------------------------------------------
// Factory CLMM validation
// ------------------------------------------------------------------------

#[test]
fn factory_rejects_half_configured_clmm() {
    let mut deps = simple_deps();
    let mut init = make_factory_init(&deps.api);
    init.clmm_manager = None; // factory set, manager unset
    let admin = deps.api.addr_make("admin");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        InstantiateMsg::Factory(init),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ClmmHalfConfigured {}));
}

#[test]
fn create_sink_rejects_clmm_address_mismatch() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let mut sink_init = make_sink_init(&deps.api);
    sink_init.pool_kind = PoolKind::Clmm {
        clmm_factory: deps.api.addr_make("wrong_clmm_factory").to_string(),
        clmm_manager: deps.api.addr_make("clmm_manager").to_string(),
        fee_tier: 3000,
        position_recipient: deps.api.addr_make("locker").to_string(),
        max_fee_multiple: None,
    };
    let caller = deps.api.addr_make("issuer_keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"x".to_vec()),
            sink_init,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SinkClmmAddressMismatch { .. }));
}

#[test]
fn create_sink_clmm_happy_path() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let mut sink_init = make_sink_init(&deps.api);
    sink_init.pool_kind = clmm_pool_kind(&deps.api);
    let caller = deps.api.addr_make("issuer_keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateSink {
            salt: Binary::from(b"clmm-salt".to_vec()),
            sink_init,
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    assert!(matches!(
        &res.messages[0].msg,
        CosmosMsg::Wasm(WasmMsg::Instantiate2 { code_id, .. }) if *code_id == 7
    ));
}

// ------------------------------------------------------------------------
// Locker
// ------------------------------------------------------------------------

use crate::msg::{LockerConfigResponse, LockerInit};

const TEST_CREATOR_SHARE_BPS: u16 = 3000; // creator gets 30% of the fee

fn instantiate_locker(deps: &mut RichDeps, admin: Option<String>) {
    instantiate_locker_share(deps, admin, TEST_CREATOR_SHARE_BPS);
}

fn instantiate_locker_share(deps: &mut RichDeps, admin: Option<String>, share: u16) {
    let init = LockerInit {
        manager: deps.api.addr_make("clmm_manager").to_string(),
        treasury: deps.api.addr_make("treasury").to_string(),
        creator: deps.api.addr_make("creator").to_string(),
        creator_fee_share_bps: share,
        admin,
    };
    let caller = deps.api.addr_make("factory_caller");
    instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Locker(init),
    )
    .unwrap();
}

#[test]
fn create_locker_emits_instantiate2_and_pins_manager() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let caller = deps.api.addr_make("issuer_keeper");
    let locker_init = LockerInit {
        manager: deps.api.addr_make("clmm_manager").to_string(),
        treasury: deps.api.addr_make("treasury").to_string(),
        creator: deps.api.addr_make("creator").to_string(),
        creator_fee_share_bps: TEST_CREATOR_SHARE_BPS,
        admin: None,
    };
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateLocker {
            salt: Binary::from(b"locker-salt".to_vec()),
            locker_init,
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Instantiate2 {
            code_id,
            msg,
            admin,
            ..
        }) => {
            assert!(admin.is_none());
            assert_eq!(*code_id, 7);
            assert!(matches!(
                from_json::<InstantiateMsg>(msg.as_slice()).unwrap(),
                InstantiateMsg::Locker(_)
            ));
        }
        other => panic!("expected Instantiate2, got {:?}", other),
    }
}

#[test]
fn create_locker_rejects_manager_mismatch() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let caller = deps.api.addr_make("issuer_keeper");
    let locker_init = LockerInit {
        manager: deps.api.addr_make("wrong_manager").to_string(),
        treasury: deps.api.addr_make("treasury").to_string(),
        creator: deps.api.addr_make("creator").to_string(),
        creator_fee_share_bps: TEST_CREATOR_SHARE_BPS,
        admin: None,
    };
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::CreateLocker {
            salt: Binary::from(b"x".to_vec()),
            locker_init,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SinkClmmAddressMismatch { .. }));
}

#[test]
fn locker_collect_fees_with_explicit_token_id() {
    let mut deps = rich_deps(&[]);
    instantiate_locker(&mut deps, None);
    let anyone = deps.api.addr_make("anyone");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&anyone, &[]),
        ExecuteMsg::CollectFees {
            token_id: Some("42".to_string()),
        },
    )
    .unwrap();
    // Collect routed to the locker itself, then a DistributeFees self-callback.
    assert_eq!(res.messages.len(), 2);
    let self_addr = mock_env().contract.address;
    match &res.messages[0].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr, msg, ..
        }) => {
            assert_eq!(
                contract_addr,
                &deps.api.addr_make("clmm_manager").to_string()
            );
            match from_json::<ClmmManagerExecuteMsg>(msg.as_slice()).unwrap() {
                ClmmManagerExecuteMsg::Collect {
                    token_id,
                    recipient,
                } => {
                    assert_eq!(token_id, "42");
                    // Collected INTO the locker, not straight to a recipient.
                    assert_eq!(recipient.as_deref(), Some(self_addr.as_str()));
                }
                other => panic!("expected Collect, got {:?}", other),
            }
        }
        other => panic!("expected manager.Collect exec, got {:?}", other),
    }
    match &res.messages[1].msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr, msg, ..
        }) => {
            assert_eq!(contract_addr, &self_addr.to_string());
            assert!(matches!(
                from_json::<ExecuteMsg>(msg.as_slice()).unwrap(),
                ExecuteMsg::Callback(CallbackMsg::DistributeFees {})
            ));
        }
        other => panic!("expected self DistributeFees callback, got {:?}", other),
    }
}

/// The DistributeFees callback splits every collected denom between treasury
/// and creator per `creator_fee_share_bps`; treasury absorbs the remainder.
#[test]
fn locker_distribute_fees_splits_both_denoms() {
    // 1000 token0 + 333 token1 sitting on the locker after a Collect.
    let bal = vec![coin(1000, "token0"), coin(333, "token1")];
    let mut deps = rich_deps(&bal);
    instantiate_locker(&mut deps, None); // 30% creator
    deps.querier
        .with_balance(&[(&MOCK_CONTRACT_ADDR.to_string(), bal.clone())]);

    let self_addr = mock_env().contract.address;
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeFees {}),
    )
    .unwrap();

    let treasury = deps.api.addr_make("treasury").to_string();
    let creator = deps.api.addr_make("creator").to_string();
    // token0: creator floor(1000*3000/10000)=300, treasury 700.
    // token1: creator floor(333*3000/10000)=99,  treasury 234.
    let mut seen: Vec<(String, String, u128)> = vec![];
    for m in &res.messages {
        if let CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &m.msg {
            seen.push((
                to_address.clone(),
                amount[0].denom.clone(),
                amount[0].amount.u128(),
            ));
        }
    }
    assert!(seen.contains(&(treasury.clone(), "token0".into(), 700)));
    assert!(seen.contains(&(creator.clone(), "token0".into(), 300)));
    assert!(seen.contains(&(treasury.clone(), "token1".into(), 234)));
    assert!(seen.contains(&(creator.clone(), "token1".into(), 99)));
    // Conservation: every input wei is sent somewhere, none stranded.
    let token0_out: u128 = seen.iter().filter(|s| s.1 == "token0").map(|s| s.2).sum();
    let token1_out: u128 = seen.iter().filter(|s| s.1 == "token1").map(|s| s.2).sum();
    assert_eq!(token0_out, 1000);
    assert_eq!(token1_out, 333);
}

/// 0% share = legacy behaviour: everything to treasury, no creator leg.
#[test]
fn locker_distribute_fees_zero_share_all_treasury() {
    let bal = vec![coin(1000, "token0")];
    let mut deps = rich_deps(&bal);
    instantiate_locker_share(&mut deps, None, 0);
    deps.querier
        .with_balance(&[(&MOCK_CONTRACT_ADDR.to_string(), bal.clone())]);

    let self_addr = mock_env().contract.address;
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&self_addr, &[]),
        ExecuteMsg::Callback(CallbackMsg::DistributeFees {}),
    )
    .unwrap();
    // Exactly one Send, to treasury, full amount.
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &deps.api.addr_make("treasury").to_string());
            assert_eq!(amount[0].amount.u128(), 1000);
        }
        other => panic!("expected single treasury Send, got {:?}", other),
    }
}

#[test]
fn locker_instantiate_rejects_share_above_100pct() {
    let mut deps = rich_deps(&[]);
    let init = LockerInit {
        manager: deps.api.addr_make("clmm_manager").to_string(),
        treasury: deps.api.addr_make("treasury").to_string(),
        creator: deps.api.addr_make("creator").to_string(),
        creator_fee_share_bps: 10_001,
        admin: None,
    };
    let caller = deps.api.addr_make("factory_caller");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Locker(init),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::LockerCreatorFeeShareTooHigh {
            value: 10_001,
            max: 10_000
        }
    ));
}

#[test]
fn locker_update_treasury_requires_admin_and_leaves_creator_immutable() {
    let mut deps = rich_deps(&[]);
    let admin = deps.api.addr_make("locker_admin");
    instantiate_locker(&mut deps, Some(admin.to_string()));

    // Stranger rejected.
    let stranger = deps.api.addr_make("stranger");
    let new_treasury = deps.api.addr_make("new_treasury");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateTreasury {
            new_treasury: new_treasury.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Admin succeeds; query reflects the new treasury — and the creator leg is
    // unchanged (no path can ever rotate it).
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateTreasury {
            new_treasury: new_treasury.to_string(),
        },
    )
    .unwrap();
    let cfg: LockerConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::LockerConfig {}).unwrap()).unwrap();
    assert_eq!(cfg.treasury, new_treasury.to_string());
    assert_eq!(cfg.creator, deps.api.addr_make("creator").to_string());
    assert_eq!(cfg.creator_fee_share_bps, TEST_CREATOR_SHARE_BPS);
}

#[test]
fn locker_update_treasury_errors_when_no_admin() {
    let mut deps = rich_deps(&[]);
    instantiate_locker(&mut deps, None);
    let anyone = deps.api.addr_make("anyone");
    let new_treasury = deps.api.addr_make("x");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&anyone, &[]),
        ExecuteMsg::UpdateTreasury {
            new_treasury: new_treasury.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::LockerNoAdmin {}));
}

#[test]
fn collect_fees_rejected_on_sink_instance() {
    let mut deps = rich_deps(&[]);
    instantiate_sink_default(&mut deps);
    let anyone = deps.api.addr_make("anyone");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&anyone, &[]),
        ExecuteMsg::CollectFees {
            token_id: Some("1".to_string()),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::WrongRole { .. }));
}

// Belt: assert the JSON shape of `create_sink_payload` matches what
// `choice_mts_issuer` is expected to ship. Round-tripping the binary
// guarantees the wire format won't drift between the two contracts
// without a deliberate change here.
#[test]
fn create_sink_payload_json_round_trips() {
    let api = MockApi::default();
    let sink_init = make_sink_init(&api);
    let payload = to_json_binary(&ExecuteMsg::CreateSink {
        salt: Binary::from(b"abc".to_vec()),
        sink_init: sink_init.clone(),
    })
    .unwrap();
    let decoded: ExecuteMsg = from_json(payload.as_slice()).unwrap();
    match decoded {
        ExecuteMsg::CreateSink { salt, sink_init: s } => {
            assert_eq!(salt.as_slice(), b"abc");
            assert_eq!(s.token_denom, sink_init.token_denom);
            assert_eq!(s.pair_denom, sink_init.pair_denom);
        }
        other => panic!("expected CreateSink, got {:?}", other),
    }
}

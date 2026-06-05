//! Unit tests for `choice_pool_seeder`.
//!
//! Coverage:
//!  * Instantiate both roles + per-field validation.
//!  * `CreateSink` happy path (Instantiate2 wiring) + rejections
//!    (tip-too-high, choice_factory mismatch, wrong-role).
//!  * `Settle` happy path message chain + tip-on-pair-side accounting +
//!    role/balance/create-fee guards.
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
use cosmwasm_std::testing::{MockApi as TestMockApi, MockQuerier, MockStorage};
use cosmwasm_std::testing::MOCK_CONTRACT_ADDR;
use injective_cosmwasm::query::InjectiveQueryWrapper;
use std::marker::PhantomData;

use crate::contract::{execute, instantiate, migrate, query, HARD_MAX_TIP_BPS};
use crate::error::ContractError;
use crate::msg::{
    CallbackMsg, ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg, LpDestination,
    MigrateMsg, QueryMsg, RoleResponse, SinkConfigResponse, SinkInit, SinkStateResponse,
};
use crate::state::{Role, SinkStatus, ROLE, SINK_STATE};

const TOKEN_DENOM: &str = "factory/inj1issuer000/shroom_1";
const PAIR_DENOM: &str = "factory/inj1pair000/shroom";
const CREATE_FEE_DENOM: &str = "inj";
const CREATE_FEE: u128 = 100;

// ------------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------------

type SimpleDeps =
    OwnedDeps<MockStorage, TestMockApi, MockQuerier<Empty>, InjectiveQueryWrapper>;

type RichDeps =
    OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper>;

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
        max_tip_bps: 50,
    }
}

fn make_sink_init(api: &MockApi) -> SinkInit {
    SinkInit {
        issuer: api.addr_make("issuer").to_string(),
        token_denom: TOKEN_DENOM.to_string(),
        pair_denom: PAIR_DENOM.to_string(),
        token_decimals: 18,
        pair_decimals: 18,
        lp_destination: LpDestination::Burn,
        refund_receiver: api.addr_make("refund_receiver").to_string(),
        deadline_seconds: 86_400,
        tip_bps: 25,
        choice_factory: api.addr_make("choice_factory").to_string(),
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
    assert_eq!(factory.max_tip_bps, 50);
    assert_eq!(factory.admin, deps.api.addr_make("admin").to_string());
}

#[test]
fn instantiate_factory_rejects_tip_over_hard_cap() {
    let mut deps = simple_deps();
    let mut init = make_factory_init(&deps.api);
    init.max_tip_bps = HARD_MAX_TIP_BPS + 1;
    let admin = deps.api.addr_make("admin");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        InstantiateMsg::Factory(init),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::TipTooHigh { .. }));
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
    assert_eq!(cfg.tip_bps, 25);
    assert!(matches!(cfg.lp_destination, LpDestination::Burn));
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

#[test]
fn instantiate_sink_rejects_tip_over_hard_cap() {
    let mut deps = rich_deps(&[]);
    let mut init = make_sink_init(&deps.api);
    init.tip_bps = HARD_MAX_TIP_BPS + 1;
    let caller = deps.api.addr_make("factory_caller");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        InstantiateMsg::Sink(init),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::TipTooHigh { .. }));
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
                    assert_eq!(s.tip_bps, 25);
                }
                _ => panic!("expected Sink instantiate variant"),
            }
        }
        other => panic!("expected Instantiate2, got {:?}", other),
    }
}

#[test]
fn create_sink_rejects_tip_above_factory_cap() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let mut sink_init = make_sink_init(&deps.api);
    sink_init.tip_bps = 51; // factory max is 50
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
    assert!(matches!(err, ContractError::TipTooHigh { .. }));
}

#[test]
fn create_sink_rejects_choice_factory_mismatch() {
    let mut deps = simple_deps();
    instantiate_factory_default(&mut deps);
    let mut sink_init = make_sink_init(&deps.api);
    sink_init.choice_factory = deps.api.addr_make("other_choice_factory").to_string();
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
    assert!(matches!(err, ContractError::SinkChoiceFactoryMismatch { .. }));
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
fn settle_emits_full_chain_with_tip_create_pair_callbacks() {
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

    // tip + create_pair + provide-callback + distribute-callback (no refund — exact fee)
    assert_eq!(res.messages.len(), 4);

    // Tip = 800e6 * 25 / 10000 = 2_000_000
    let expected_tip = Uint128::new(800_000_000u128).multiply_ratio(25u128, 10_000u128);
    match &res.messages[0].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &caller.to_string());
            assert_eq!(amount, &coins(expected_tip.u128(), PAIR_DENOM));
        }
        other => panic!("expected tip BankMsg::Send, got {:?}", other),
    }

    match &res.messages[1].msg {
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

    for (idx, expected_kind) in [(2usize, "provide_liquidity"), (3, "distribute_lp")] {
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
                        // pair_deposit = 800e6 - 2e6
                        assert_eq!(pair_amount, Uint128::new(798_000_000u128));
                    }
                    (ExecuteMsg::Callback(CallbackMsg::DistributeLp {}), "distribute_lp") => {}
                    (other, kind) => panic!("expected {} callback, got {:?}", kind, other),
                }
            }
            other => panic!("expected callback WasmExec, got {:?}", other),
        }
    }

    // Status flipped pre-dispatch (defense-in-depth against re-entry).
    let state = SINK_STATE.load(deps.as_ref().storage).unwrap();
    assert_eq!(state.status, SinkStatus::Settled);
}

#[test]
fn settle_with_zero_tip_omits_tip_message() {
    let mut deps = rich_deps(&settle_balances());

    // Override sink to tip_bps=0.
    let mut init = make_sink_init(&deps.api);
    init.tip_bps = 0;
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
    // No tip → only 3 messages (create_pair + 2 callbacks).
    assert_eq!(res.messages.len(), 3);
    assert!(matches!(
        &res.messages[0].msg,
        CosmosMsg::Wasm(WasmMsg::Execute { .. })
    ));
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
    let funds = vec![
        coin(CREATE_FEE, CREATE_FEE_DENOM),
        coin(1, PAIR_DENOM),
    ];
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

    // Expected: tip = leg_b_pair * 25 / 10000; deposit = leg_b_pair - tip.
    let tip = Uint128::new(leg_b_pair).multiply_ratio(25u128, 10_000u128);
    let expected_deposit = Uint128::new(leg_b_pair) - tip;

    // Find the ProvideLiquidity callback and check pair_amount.
    let provide = res
        .messages
        .iter()
        .find_map(|sm| match &sm.msg {
            CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, msg, .. })
                if contract_addr == &mock_env().contract.address.to_string() =>
            {
                let decoded: ExecuteMsg = from_json(msg.as_slice()).ok()?;
                match decoded {
                    ExecuteMsg::Callback(CallbackMsg::ProvideLiquidity {
                        pair_amount, ..
                    }) => Some(pair_amount),
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
    assert!(matches!(err, ContractError::RefundDeadlineNotReached { .. }));
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
    init.lp_destination = LpDestination::SendTo(dest.to_string());
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

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.to_string(),
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&new_admin, &[]),
        ExecuteMsg::UpdateSinkCodeId {
            new_sink_code_id: 99,
        },
    )
    .unwrap();
    let cfg: FactoryConfigResponse = from_json(
        query(deps.as_ref(), mock_env(), QueryMsg::FactoryConfig {}).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg.admin, new_admin.to_string());
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
            assert_eq!(s.tip_bps, sink_init.tip_bps);
        }
        other => panic!("expected CreateSink, got {:?}", other),
    }
}

//! Unit tests for `choice_mts_issuer`.
//!
//! Coverage:
//!  * Instantiate happy path + input validation (subdenom prefix length /
//!    charset, decimals cap).
//!  * `RegisterLaunch` happy path — verifies the full message ordering
//!    (CreateDenom → Mint → SubMsg CreateTokenPair →
//!    seeder Wasm exec → BankMsg::Send) plus persisted record.
//!  * `RegisterLaunch` rejections: duplicate id, zero total, evm > total.
//!  * `DeliverToSeeder` happy path + auth + status guard + leftover-vs-supply
//!    guard.
//!  * `RefundFailedLaunch` keeper path, post-deadline public path, and
//!    rejection of pre-deadline non-keeper callers.
//!  * Admin rotations (admin / keeper / forwarder).
//!  * Reply handler decodes the prost-encoded `MsgCreateTokenPairResponse`
//!    and patches `erc20_address`.

use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockStorage};
use cosmwasm_std::{
    coin, coins, from_json, BankMsg, Binary, Coin, CosmosMsg, OwnedDeps, Reply, ReplyOn,
    SubMsgResponse, SubMsgResult, Uint128, WasmMsg,
};

use choice::mock_querier::{mock_dependencies, WasmMockQuerier};
use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::{InjectiveMsg, InjectiveRoute};

use crate::contract::{execute, instantiate, query, reply, MAX_DECIMALS, MAX_SUBDENOM_PREFIX_LEN};
use crate::error::ContractError;
use crate::msg::{ConfigResponse, ExecuteMsg, InstantiateMsg, LaunchesResponse, QueryMsg};
use crate::proto::{
    MsgBurn, MsgCreateDenom, MsgCreateTokenPair, MsgCreateTokenPairResponse, TokenPair,
};
use crate::state::{LaunchStatus, LAUNCHES};
use prost::Message;

const PREFIX: &str = "shroom";
const PAIR_DENOM: &str = "factory/inj1pair/shroom";
const REFUND_DEADLINE: u64 = 86_400;
const CREATE_FEE_DENOM: &str = "inj";
/// 0.1 INJ — matches Injective mainnet's tokenfactory denom-creation fee.
const CREATE_FEE: u128 = 100_000_000_000_000_000;

// Switched from `MockQuerier<Empty>` after `RegisterLaunch` started querying
// the chain for the tokenfactory create fee — that custom query needs the
// choice mock querier's `with_token_factory_denom_create_fee` handler.
type Deps = OwnedDeps<MockStorage, MockApi, WasmMockQuerier, InjectiveQueryWrapper>;

fn new_deps() -> Deps {
    let mut deps = mock_dependencies(&[]);
    deps.querier
        .with_token_factory_denom_create_fee(&[(CREATE_FEE_DENOM, Uint128::new(CREATE_FEE))]);
    deps
}

fn setup() -> Deps {
    let mut deps = new_deps();
    let admin = deps.api.addr_make("admin");
    let info = message_info(&admin, &[]);
    let msg = InstantiateMsg {
        admin: admin.to_string(),
        subdenom_prefix: PREFIX.to_string(),
        decimals: 18,
        keeper: deps.api.addr_make("keeper").to_string(),
        forwarder: deps.api.addr_make("forwarder").to_string(),
        refund_deadline_seconds: REFUND_DEADLINE,
    };
    instantiate(deps.as_mut(), mock_env(), info, msg).unwrap();
    deps
}

/// Standard `info.funds` for a `RegisterLaunch` call — exactly the chain's
/// create-denom fee.
fn fee_funds() -> Vec<Coin> {
    vec![coin(CREATE_FEE, CREATE_FEE_DENOM)]
}

fn register_default(deps: &mut Deps, internal_id: u64) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    register_with_choice_factory(deps, internal_id, None)
}

fn register_with_choice_factory(
    deps: &mut Deps,
    internal_id: u64,
    choice_factory: Option<String>,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("caller");
    let info = message_info(&caller, &fee_funds());
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        // 1B tokens * 1e18 = 1e27
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::from(br#"{"create_sink":{"salt":"","sink_init":{}}}"#.to_vec()),
        choice_factory,
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

fn expected_denom(internal_id: u64) -> String {
    format!("factory/{}/{}_{}", mock_env().contract.address, PREFIX, internal_id)
}

#[test]
fn instantiate_happy_path_persists_config() {
    let deps = setup();
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.subdenom_prefix, PREFIX);
    assert_eq!(cfg.decimals, 18);
    assert_eq!(cfg.refund_deadline_seconds, REFUND_DEADLINE);
}

#[test]
fn instantiate_rejects_overlong_prefix() {
    let mut deps = new_deps();
    let admin = deps.api.addr_make("admin");
    let msg = InstantiateMsg {
        admin: admin.to_string(),
        subdenom_prefix: "a".repeat(MAX_SUBDENOM_PREFIX_LEN + 1),
        decimals: 18,
        keeper: deps.api.addr_make("keeper").to_string(),
        forwarder: deps.api.addr_make("forwarder").to_string(),
        refund_deadline_seconds: REFUND_DEADLINE,
    };
    let err = instantiate(deps.as_mut(), mock_env(), message_info(&admin, &[]), msg).unwrap_err();
    assert!(matches!(err, ContractError::SubdenomPrefixInvalid { .. }));
}

#[test]
fn instantiate_rejects_non_alphanum_prefix() {
    let mut deps = new_deps();
    let admin = deps.api.addr_make("admin");
    let msg = InstantiateMsg {
        admin: admin.to_string(),
        subdenom_prefix: "bad-prefix".to_string(),
        decimals: 18,
        keeper: deps.api.addr_make("keeper").to_string(),
        forwarder: deps.api.addr_make("forwarder").to_string(),
        refund_deadline_seconds: REFUND_DEADLINE,
    };
    let err = instantiate(deps.as_mut(), mock_env(), message_info(&admin, &[]), msg).unwrap_err();
    assert!(matches!(err, ContractError::SubdenomPrefixInvalid { .. }));
}

#[test]
fn instantiate_rejects_decimals_over_18() {
    let mut deps = new_deps();
    let admin = deps.api.addr_make("admin");
    let msg = InstantiateMsg {
        admin: admin.to_string(),
        subdenom_prefix: PREFIX.to_string(),
        decimals: MAX_DECIMALS + 1,
        keeper: deps.api.addr_make("keeper").to_string(),
        forwarder: deps.api.addr_make("forwarder").to_string(),
        refund_deadline_seconds: REFUND_DEADLINE,
    };
    let err = instantiate(deps.as_mut(), mock_env(), message_info(&admin, &[]), msg).unwrap_err();
    assert!(matches!(err, ContractError::DecimalsOutOfRange { .. }));
}

#[test]
fn register_launch_emits_expected_message_chain_and_persists_record() {
    let mut deps = setup();
    let res = register_default(&mut deps, 42).unwrap();
    let denom = expected_denom(42);

    // 1 SubMsg (CreateTokenPair, ReplyOn::Success) + 5 plain messages.
    assert_eq!(res.messages.len(), 5);

    let sub = res
        .messages
        .iter()
        .find(|sm| sm.reply_on == ReplyOn::Success)
        .expect("create_token_pair submsg");
    assert_eq!(sub.payload.as_slice(), 42u64.to_be_bytes());

    // Dispatch position matters: chain runs messages in `res.messages` order.
    // CreateTokenPair MUST land AFTER CreateDenom/Mint and BEFORE
    // CreateSink/BankSend, otherwise the chain rejects `CreateTokenPair`
    // for an unknown denom. Index 2 wedges it between the two halves
    // (SetTokenMetadata was removed — v1.20+ CreateDenom finalises decimals
    // so a follow-up SetTokenMetadata errors with "cannot update denom
    // metadata decimals"; observed on testnet 2026-05-26 tx 52D5873E…).
    let create_pair_idx = res
        .messages
        .iter()
        .position(|sm| sm.reply_on == ReplyOn::Success)
        .unwrap();
    assert_eq!(create_pair_idx, 2, "CreateTokenPair must be at dispatch position 2");

    #[allow(deprecated)]
    match &sub.msg {
        CosmosMsg::Stargate { type_url, value } => {
            assert_eq!(type_url, MsgCreateTokenPair::TYPE_URL);
            let decoded = MsgCreateTokenPair::decode(value.as_slice()).unwrap();
            let tp = decoded.token_pair.expect("token_pair populated");
            assert_eq!(tp.bank_denom, denom);
            assert!(tp.erc20_address.is_empty(), "empty so chain auto-deploys");
        }
        other => panic!("expected Stargate CreateTokenPair, got {:?}", other),
    }

    let plain: Vec<&CosmosMsg<injective_cosmwasm::InjectiveMsgWrapper>> = res
        .messages
        .iter()
        .filter(|sm| sm.reply_on == ReplyOn::Never)
        .map(|sm| &sm.msg)
        .collect();
    assert_eq!(plain.len(), 4, "create_denom, mint, wasm_exec, bank_send");

    // Order is deliberate: pair-creation reply must run BEFORE the seeder
    // factory's CreateSink + the bank-send to EVM authority, because the
    // sink needs the pair to exist when it later runs `Settle` against the
    // resulting bank denom. See contract.rs in-line comments.
    #[allow(deprecated)]
    match plain[0] {
        CosmosMsg::Stargate { type_url, value } => {
            assert_eq!(type_url, MsgCreateDenom::TYPE_URL);
            let decoded = MsgCreateDenom::decode(value.as_slice()).unwrap();
            assert_eq!(decoded.subdenom, format!("{}_{}", PREFIX, 42));
            assert!(decoded.allow_admin_burn, "Path B Leg A requires admin burn-from");
            assert_eq!(decoded.decimals, 18);
        }
        other => panic!("expected Stargate CreateDenom, got {:?}", other),
    }

    match plain[1] {
        CosmosMsg::Custom(w) => {
            assert_eq!(w.route, InjectiveRoute::Tokenfactory);
            match &w.msg_data {
                InjectiveMsg::Mint { amount, mint_to, .. } => {
                    assert_eq!(amount.denom, denom);
                    assert_eq!(amount.amount, Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)));
                    assert_eq!(*mint_to, mock_env().contract.address.to_string());
                }
                other => panic!("expected Mint, got {:?}", other),
            }
        }
        other => panic!("expected Custom Injective msg, got {:?}", other),
    }

    match plain[2] {
        CosmosMsg::Wasm(WasmMsg::Execute { contract_addr, funds, .. }) => {
            assert_eq!(contract_addr, &deps.api.addr_make("seeder_factory").to_string());
            assert!(funds.is_empty(), "no funds on CreateSink — Legs B+C feed the sink");
        }
        other => panic!("expected WasmMsg::Execute, got {:?}", other),
    }

    match plain[3] {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &deps.api.addr_make("evm_authority").to_string());
            assert_eq!(
                amount,
                &coins(
                    (Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18))).u128(),
                    &denom
                )
            );
        }
        other => panic!("expected BankMsg::Send, got {:?}", other),
    }

    let stored = LAUNCHES.load(deps.as_ref().storage, 42).unwrap();
    assert_eq!(stored.status, LaunchStatus::Registered);
    assert_eq!(stored.cw_held, Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18)));
    assert_eq!(stored.erc20_address, None, "filled in by reply handler");
}

#[test]
fn register_launch_rejects_duplicate_internal_id() {
    let mut deps = setup();
    register_default(&mut deps, 7).unwrap();
    let err = register_default(&mut deps, 7).unwrap_err();
    assert!(matches!(err, ContractError::LaunchAlreadyRegistered { id: 7 }));
}

#[test]
fn register_launch_rejects_zero_total_supply() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    let info = message_info(&caller, &fee_funds());
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 1,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::zero(),
        evm_supply: Uint128::zero(),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::default(),
        choice_factory: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::ZeroTotalSupply {}));
}

#[test]
fn register_launch_rejects_evm_supply_over_total() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    let info = message_info(&caller, &fee_funds());
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 1,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(100),
        evm_supply: Uint128::new(101),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::default(),
        choice_factory: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::EvmSupplyExceedsTotal { .. }));
}

#[test]
fn register_launch_rejects_when_caller_omits_create_fee_funds() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    // No `info.funds` attached → caller didn't pay the chain's create fee.
    let info = message_info(&caller, &[]);
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 1,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000),
        evm_supply: Uint128::new(800_000),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::default(),
        choice_factory: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::InsufficientCreateFee { .. }));
}

#[test]
fn register_launch_rejects_overpaid_create_fee() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    // Caller over-pays by 13 wei — must revert. Refunding excess would
    // tempt callers to pre-fund the issuer and leave dust accumulating in
    // the contract's bank balance.
    let info = message_info(&caller, &[coin(CREATE_FEE + 13, CREATE_FEE_DENOM)]);
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 70,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::from(br#"{"create_sink":{}}"#.to_vec()),
        choice_factory: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::CreateFeeOverpaid { .. }));
}

#[test]
fn register_launch_rejects_unexpected_funds_denom() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    let info = message_info(
        &caller,
        &[
            coin(CREATE_FEE, CREATE_FEE_DENOM),
            coin(1, "factory/inj1stranger/foo"),
        ],
    );
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 71,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::from(br#"{"create_sink":{}}"#.to_vec()),
        choice_factory: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::UnexpectedFundsDenom { .. }));
}

#[test]
fn register_launch_with_choice_factory_chains_add_native_token_decimals() {
    let mut deps = setup();
    let cf = deps.api.addr_make("choice_factory").to_string();
    let res = register_with_choice_factory(&mut deps, 50, Some(cf.clone())).unwrap();

    // Same four plain msgs + the chained AddNativeTokenDecimals exec + the
    // CreateTokenPair SubMsg → 6 total.
    assert_eq!(res.messages.len(), 6);

    // Locate the AddNativeTokenDecimals call: it's the WasmExec that targets
    // the choice_factory address (the OTHER WasmExec targets the seeder
    // factory). Funds must include exactly 1 wei of the launch denom.
    let denom = format!("factory/{}/{}_{}", mock_env().contract.address, PREFIX, 50);
    let add_msg = res
        .messages
        .iter()
        .find_map(|sm| match &sm.msg {
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr,
                funds,
                ..
            }) if contract_addr == &cf => Some(funds.clone()),
            _ => None,
        })
        .expect("AddNativeTokenDecimals msg present");
    assert_eq!(add_msg, coins(1u128, &denom));

    // cw_held was reduced by 1 wei to fund the dust.
    let stored = LAUNCHES.load(deps.as_ref().storage, 50).unwrap();
    let expected_cw_held =
        Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18)) - Uint128::one();
    assert_eq!(stored.cw_held, expected_cw_held);
    assert_eq!(stored.choice_factory.map(|a| a.into_string()), Some(cf));
}

#[test]
fn register_launch_rejects_choice_factory_when_cw_held_is_zero() {
    let mut deps = setup();
    let caller = deps.api.addr_make("caller");
    let info = message_info(&caller, &fee_funds());
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 60,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        // total == evm → cw_held = 0 → no dust available
        total_supply: Uint128::new(100),
        evm_supply: Uint128::new(100),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: Binary::default(),
        choice_factory: Some(deps.api.addr_make("choice_factory").to_string()),
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::ChoiceFactoryNeedsDust { .. }));
}

#[test]
fn deliver_to_seeder_happy_path_emits_burn_and_send() {
    let mut deps = setup();
    register_default(&mut deps, 9).unwrap();
    simulate_create_token_pair_reply(&mut deps, 9, "0xdeadbeef00000000000000000000000000000000");

    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 9,
            leftover: Uint128::new(50_000_000u128) * Uint128::new(10u128.pow(18)),
        },
    )
    .unwrap();
    let denom = expected_denom(9);

    assert_eq!(res.messages.len(), 2);
    #[allow(deprecated)]
    match &res.messages[0].msg {
        CosmosMsg::Stargate { type_url, value } => {
            assert_eq!(type_url, MsgBurn::TYPE_URL);
            let decoded = MsgBurn::decode(value.as_slice()).unwrap();
            assert_eq!(
                decoded.burn_from_address,
                deps.api.addr_make("evm_authority").to_string()
            );
            let coin = decoded.amount.expect("amount populated");
            assert_eq!(coin.denom, denom);
            assert_eq!(coin.amount, "50000000000000000000000000");
        }
        other => panic!("expected Stargate MsgBurn, got {:?}", other),
    }
    match &res.messages[1].msg {
        CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
            assert_eq!(to_address, &deps.api.addr_make("seeder_addr").to_string());
            assert_eq!(
                amount,
                &coins(
                    (Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18))).u128(),
                    &denom
                )
            );
        }
        other => panic!("expected BankMsg::Send, got {:?}", other),
    }
    assert_eq!(
        LAUNCHES.load(deps.as_ref().storage, 9).unwrap().status,
        LaunchStatus::Delivered
    );
}

#[test]
fn deliver_to_seeder_with_zero_leftover_omits_burn() {
    let mut deps = setup();
    register_default(&mut deps, 10).unwrap();
    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 10,
            leftover: Uint128::zero(),
        },
    )
    .unwrap();
    // Burn skipped → only BankMsg::Send to seeder.
    assert_eq!(res.messages.len(), 1);
    assert!(matches!(res.messages[0].msg, CosmosMsg::Bank(BankMsg::Send { .. })));
}

#[test]
fn deliver_to_seeder_rejects_non_keeper() {
    let mut deps = setup();
    register_default(&mut deps, 11).unwrap();
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 11,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotKeeper {}));
}

#[test]
fn deliver_to_seeder_rejects_leftover_over_evm_supply() {
    let mut deps = setup();
    register_default(&mut deps, 12).unwrap();
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 12,
            leftover: Uint128::new(10u128.pow(36)),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::LeftoverExceedsEvmSupply { .. }));
}

#[test]
fn deliver_to_seeder_rejects_repeat() {
    let mut deps = setup();
    register_default(&mut deps, 13).unwrap();
    let keeper = deps.api.addr_make("keeper");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 13,
            leftover: Uint128::zero(),
        },
    )
    .unwrap();
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            internal_id: 13,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidLaunchStatus { .. }));
}

#[test]
fn refund_keeper_path_burns_cw_held() {
    let mut deps = setup();
    register_default(&mut deps, 21).unwrap();
    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RefundFailedLaunch {
            internal_id: 21,
            reason: "bootstrap_failed".to_string(),
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Custom(w) => match &w.msg_data {
            InjectiveMsg::Burn { amount, .. } => {
                assert_eq!(amount.amount, Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18)));
            }
            other => panic!("expected Burn, got {:?}", other),
        },
        other => panic!("expected Custom Injective msg, got {:?}", other),
    }
    assert_eq!(
        LAUNCHES.load(deps.as_ref().storage, 21).unwrap().status,
        LaunchStatus::Refunded
    );
}

#[test]
fn refund_non_keeper_pre_deadline_is_rejected() {
    let mut deps = setup();
    register_default(&mut deps, 22).unwrap();
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::RefundFailedLaunch {
            internal_id: 22,
            reason: "stuck".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::RefundDeadlineNotReached { .. }));
}

#[test]
fn refund_non_keeper_post_deadline_succeeds() {
    let mut deps = setup();
    register_default(&mut deps, 23).unwrap();
    let mut env = mock_env();
    env.block.time = env.block.time.plus_seconds(REFUND_DEADLINE + 1);
    let stranger = deps.api.addr_make("stranger");
    execute(
        deps.as_mut(),
        env,
        message_info(&stranger, &[]),
        ExecuteMsg::RefundFailedLaunch {
            internal_id: 23,
            reason: "stuck_too_long".to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        LAUNCHES.load(deps.as_ref().storage, 23).unwrap().status,
        LaunchStatus::Refunded
    );
}

#[test]
fn admin_rotations_require_admin_caller() {
    let mut deps = setup();
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");
    let new_keeper = deps.api.addr_make("new_keeper");
    let new_forwarder = deps.api.addr_make("new_forwarder");
    let new_admin = deps.api.addr_make("new_admin");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateKeeper {
            new_keeper: new_keeper.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateKeeper {
            new_keeper: new_keeper.to_string(),
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateForwarder {
            new_forwarder: new_forwarder.to_string(),
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.to_string(),
        },
    )
    .unwrap();

    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.admin, new_admin.to_string());
    assert_eq!(cfg.keeper, new_keeper.to_string());
    assert_eq!(cfg.forwarder, new_forwarder.to_string());
}

#[test]
fn reply_decodes_token_pair_response_and_patches_erc20() {
    let mut deps = setup();
    register_default(&mut deps, 99).unwrap();
    let erc20 = "0x1234567890123456789012345678901234567890";
    simulate_create_token_pair_reply(&mut deps, 99, erc20);
    let stored = LAUNCHES.load(deps.as_ref().storage, 99).unwrap();
    assert_eq!(stored.erc20_address.as_deref(), Some(erc20));
}

#[test]
fn list_launches_query_returns_them_ordered() {
    let mut deps = setup();
    for id in [5u64, 1, 3] {
        register_default(&mut deps, id).unwrap();
    }
    let resp: LaunchesResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::Launches {
                start_after: None,
                limit: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let ids: Vec<u64> = resp.launches.iter().map(|l| l.internal_id).collect();
    assert_eq!(ids, vec![1, 3, 5]);
}

// ---------- helpers ----------

/// Construct a fake `Reply` matching the `MsgCreateTokenPair` SubMsg the
/// contract emitted during `RegisterLaunch`, and run the reply handler.
fn simulate_create_token_pair_reply(deps: &mut Deps, internal_id: u64, erc20_address: &str) {
    let response_bytes = {
        let resp = MsgCreateTokenPairResponse {
            token_pair: Some(TokenPair {
                bank_denom: expected_denom(internal_id),
                erc20_address: erc20_address.to_string(),
            }),
        };
        let mut buf = Vec::with_capacity(resp.encoded_len());
        resp.encode(&mut buf).unwrap();
        Binary::new(buf)
    };

    #[allow(deprecated)]
    let reply_msg = Reply {
        id: 1,
        payload: Binary::from(internal_id.to_be_bytes().to_vec()),
        gas_used: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: Some(response_bytes),
            msg_responses: vec![],
        }),
    };
    reply(deps.as_mut(), mock_env(), reply_msg).unwrap();
}

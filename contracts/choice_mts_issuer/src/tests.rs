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
    coin, coins, from_json, instantiate2_address, to_json_binary, Api, BankMsg, Binary,
    CanonicalAddr, Coin, CosmosMsg, OwnedDeps, Reply, ReplyOn, SubMsgResponse, SubMsgResult,
    Uint128, WasmMsg,
};

use choice::mock_querier::{mock_dependencies, WasmMockQuerier};
use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::{InjectiveMsg, InjectiveRoute};

use crate::contract::{
    execute, instantiate, migrate, query, reply, MAX_DECIMALS, MAX_SUBDENOM_PREFIX_LEN,
    MIN_REFUND_DEADLINE_SECONDS,
};
use crate::error::ContractError;
use crate::msg::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, LaunchResponse, LaunchesResponse, MigrateMsg,
    QueryMsg,
};
use crate::proto::{
    MsgBurn, MsgChangeAdmin, MsgCreateDenom, MsgCreateTokenPair, MsgCreateTokenPairResponse,
    TokenPair,
};
use crate::state::{LaunchStatus, LAUNCHES};
use choice_pool_seeder::msg::{
    ExecuteMsg as SeederExecuteMsg, FactoryConfigResponse, LpDestination, PoolKind,
    SinkConfigResponse, SinkInit,
};
use prost::Message;

const PREFIX: &str = "shroom";
const PAIR_DENOM: &str = "factory/inj1pair/shroom";
const REFUND_DEADLINE: u64 = 86_400;
const CREATE_FEE_DENOM: &str = "inj";
/// A valid Layer A `salt_suffix`: ASCII-alphanumeric and >= `MIN_SALT_SUFFIX_LEN`
/// (8) chars. Every success-path `RegisterLaunch` must now carry one (the suffix
/// is mandatory), and the launch denom always embeds it.
const TEST_SALT: &str = "tstsalt00";
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
    instantiate(deps.as_mut(), mock_env(), info.clone(), msg).unwrap();
    // The bulk of these tests predate the P1 on-chain `seeder_addr` derivation
    // (default ON) and use placeholder `addr_make("seeder_addr")` sinks that
    // don't match a real Instantiate2 derivation — turn the check OFF here so
    // they keep exercising their orthogonal concerns. Dedicated tests below
    // (`register_launch_*verify_seeder*`) cover the derivation with it ON, and
    // the `chain_capability_harness` integration tests run it ON by default.
    execute(
        deps.as_mut(),
        mock_env(),
        info,
        ExecuteMsg::SetVerifySeederDerivation { enabled: false },
    )
    .unwrap();
    deps
}

/// Standard `info.funds` for a `RegisterLaunch` call — exactly the chain's
/// create-denom fee.
fn fee_funds() -> Vec<Coin> {
    vec![coin(CREATE_FEE, CREATE_FEE_DENOM)]
}

fn register_default(
    deps: &mut Deps,
    internal_id: u64,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    register_with_choice_factory(deps, internal_id, None)
}

fn register_with_choice_factory(
    deps: &mut Deps,
    internal_id: u64,
    choice_factory: Option<String>,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    // salt_suffix is mandatory now; the sink payload's token_denom must embed it.
    let payload = xyk_sink_payload(deps, internal_id);
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        // 1B tokens * 1e18 = 1e27
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: payload,
        choice_factory,
        salt_suffix: Some(TEST_SALT.to_string()),
        clmm_pool_auth: None,
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

fn expected_denom(internal_id: u64) -> String {
    format!(
        "factory/{}/{}_{}",
        mock_env().contract.address,
        PREFIX,
        internal_id
    )
}

fn denom_with_salt(internal_id: u64, salt: &str) -> String {
    format!(
        "factory/{}/{}_{}_{}",
        mock_env().contract.address,
        PREFIX,
        internal_id,
        salt
    )
}

/// The launch denom for a default `register_default`/`register_with_choice_factory`
/// launch — these always pass `TEST_SALT`, so the denom embeds it.
fn default_denom(internal_id: u64) -> String {
    denom_with_salt(internal_id, TEST_SALT)
}

/// Stub the seeder sink's `SinkConfig` so `DeliverToSeeder` (P2-B) sees a sink
/// genuinely configured for launch `internal_id` (matching token/pair denom).
/// Also marks the address as a hosting contract for the old ghost-check.
fn install_matching_sink(deps: &mut Deps, seeder: &str, internal_id: u64) {
    let cfg = SinkConfigResponse {
        factory: Some(deps.api.addr_make("seeder_factory").to_string()),
        issuer: mock_env().contract.address.to_string(),
        token_denom: default_denom(internal_id),
        pair_denom: PAIR_DENOM.to_string(),
        token_decimals: 18,
        pair_decimals: 18,
        pool_kind: PoolKind::Xyk {
            choice_factory: deps.api.addr_make("choice_factory").to_string(),
            lp_destination: LpDestination::Burn,
        },
        refund_receiver: deps.api.addr_make("refund_receiver").to_string(),
        deadline_seconds: REFUND_DEADLINE,
        instantiated_at: 0,
        expected_token: None,
        expected_pair: None,
    };
    deps.querier
        .with_smart_query_response(seeder, to_json_binary(&cfg).unwrap());
}

/// Build a coherent XYK `CreateSink` payload whose `token_denom`/`pair_denom`
/// match what `RegisterLaunch` will derive for `internal_id`. The issuer now
/// decodes and cross-checks this payload (anti-squat enforcement). The
/// `refund_receiver` is pinned to the default `evm_authority` (audit H-1).
fn xyk_sink_payload(deps: &Deps, internal_id: u64) -> Binary {
    sink_payload(
        deps,
        default_denom(internal_id),
        PoolKind::Xyk {
            choice_factory: deps.api.addr_make("choice_factory").to_string(),
            lp_destination: LpDestination::Burn,
        },
        &deps.api.addr_make("evm_authority").to_string(),
    )
}

/// Build a coherent CLMM `CreateSink` payload.
fn clmm_sink_payload(
    deps: &Deps,
    token_denom: String,
    clmm_factory: String,
    fee_tier: u32,
) -> Binary {
    sink_payload(
        deps,
        token_denom,
        PoolKind::Clmm {
            clmm_factory,
            clmm_manager: deps.api.addr_make("clmm_manager").to_string(),
            fee_tier,
            position_recipient: deps.api.addr_make("locker").to_string(),
            max_fee_multiple: None,
        },
        &deps.api.addr_make("evm_authority").to_string(),
    )
}

/// `refund_receiver` is pinned to the launch's `evm_authority` (audit H-1), so
/// the helper takes it explicitly — most tests pass the default authority; the
/// multi-authority test passes each registering authority in turn.
fn sink_payload(
    _deps: &Deps,
    token_denom: String,
    pool_kind: PoolKind,
    refund_receiver: &str,
) -> Binary {
    to_json_binary(&SeederExecuteMsg::CreateSink {
        salt: Binary::default(),
        sink_init: SinkInit {
            issuer: mock_env().contract.address.to_string(),
            token_denom,
            pair_denom: PAIR_DENOM.to_string(),
            token_decimals: 18,
            pair_decimals: 18,
            pool_kind,
            refund_receiver: refund_receiver.to_string(),
            deadline_seconds: REFUND_DEADLINE,
            expected_token: None,
            expected_pair: None,
        },
    })
    .unwrap()
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
    // register_default carries the mandatory salt, so the denom embeds it.
    let denom = default_denom(42);

    // 1 SubMsg (CreateTokenPair, ReplyOn::Success) + 5 plain messages.
    assert_eq!(res.messages.len(), 5);

    let sub = res
        .messages
        .iter()
        .find(|sm| sm.reply_on == ReplyOn::Success)
        .expect("create_token_pair submsg");
    // Payload is a JSON `ReplyPayload {evm_authority, internal_id}` (C-H1).
    let expected_payload = format!(
        r#"{{"evm_authority":"{}","internal_id":42}}"#,
        deps.api.addr_make("evm_authority")
    );
    assert_eq!(sub.payload.as_slice(), expected_payload.as_bytes());

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
    assert_eq!(
        create_pair_idx, 2,
        "CreateTokenPair must be at dispatch position 2"
    );

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
            assert_eq!(decoded.subdenom, format!("{}_{}_{}", PREFIX, 42, TEST_SALT));
            assert!(
                decoded.allow_admin_burn,
                "Path B Leg A requires admin burn-from"
            );
            assert_eq!(decoded.decimals, 18);
        }
        other => panic!("expected Stargate CreateDenom, got {:?}", other),
    }

    match plain[1] {
        CosmosMsg::Custom(w) => {
            assert_eq!(w.route, InjectiveRoute::Tokenfactory);
            match &w.msg_data {
                InjectiveMsg::Mint {
                    amount, mint_to, ..
                } => {
                    assert_eq!(amount.denom, denom);
                    assert_eq!(
                        amount.amount,
                        Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18))
                    );
                    assert_eq!(*mint_to, mock_env().contract.address.to_string());
                }
                other => panic!("expected Mint, got {:?}", other),
            }
        }
        other => panic!("expected Custom Injective msg, got {:?}", other),
    }

    match plain[2] {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            funds,
            ..
        }) => {
            assert_eq!(
                contract_addr,
                &deps.api.addr_make("seeder_factory").to_string()
            );
            assert!(
                funds.is_empty(),
                "no funds on CreateSink — Legs B+C feed the sink"
            );
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

    let stored = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("evm_authority"), 42),
        )
        .unwrap();
    assert_eq!(stored.status, LaunchStatus::Registered);
    assert_eq!(
        stored.cw_held,
        Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18))
    );
    assert_eq!(stored.erc20_address, None, "filled in by reply handler");
}

#[test]
fn register_launch_rejects_duplicate_internal_id() {
    let mut deps = setup();
    register_default(&mut deps, 7).unwrap();
    let err = register_default(&mut deps, 7).unwrap_err();
    assert!(matches!(
        err,
        ContractError::LaunchAlreadyRegistered { id: 7 }
    ));
}

#[test]
fn register_launch_rejects_zero_total_supply() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::ZeroTotalSupply {}));
}

#[test]
fn register_launch_rejects_evm_supply_over_total() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::EvmSupplyExceedsTotal { .. }));
}

#[test]
fn register_launch_rejects_when_caller_omits_create_fee_funds() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::InsufficientCreateFee { .. }));
}

#[test]
fn register_launch_rejects_overpaid_create_fee() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::CreateFeeOverpaid { .. }));
}

#[test]
fn register_launch_rejects_unexpected_funds_denom() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
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
    // factory). Funds must include exactly 1 wei of the launch denom (which now
    // embeds the mandatory salt).
    let denom = default_denom(50);
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
    let stored = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("evm_authority"), 50),
        )
        .unwrap();
    let expected_cw_held =
        Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18)) - Uint128::one();
    assert_eq!(stored.cw_held, expected_cw_held);
    assert_eq!(stored.choice_factory.map(|a| a.into_string()), Some(cf));
}

#[test]
fn register_launch_rejects_choice_factory_when_cw_held_is_zero() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::ChoiceFactoryNeedsDust { .. }));
}

#[test]
fn deliver_to_seeder_happy_path_emits_burn_and_send() {
    let mut deps = setup();
    register_default(&mut deps, 9).unwrap();
    simulate_create_token_pair_reply(&mut deps, 9, "0xdeadbeef00000000000000000000000000000000");

    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    install_matching_sink(&mut deps, &seeder, 9);
    // DeliverToSeeder now burns the authority's ACTUAL on-chain balance of the
    // launch denom (capped at evm_supply), not the relayed `leftover`. Seed the
    // authority with the unsold supply so the burn leg is emitted.
    let leftover_amt = Uint128::new(50_000_000u128) * Uint128::new(10u128.pow(18));
    deps.querier.with_balance(&[(
        &evm_authority,
        coins(leftover_amt.u128(), &default_denom(9)),
    )]);
    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
            internal_id: 9,
            leftover: leftover_amt,
        },
    )
    .unwrap();
    let denom = default_denom(9);

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
        LAUNCHES
            .load(
                deps.as_ref().storage,
                (&deps.api.addr_make("evm_authority"), 9)
            )
            .unwrap()
            .status,
        LaunchStatus::Delivered
    );
}

#[test]
fn deliver_to_seeder_with_zero_leftover_omits_burn() {
    let mut deps = setup();
    register_default(&mut deps, 10).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    install_matching_sink(&mut deps, &seeder, 10);
    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
            internal_id: 10,
            leftover: Uint128::zero(),
        },
    )
    .unwrap();
    // Burn skipped → only BankMsg::Send to seeder.
    assert_eq!(res.messages.len(), 1);
    assert!(matches!(
        res.messages[0].msg,
        CosmosMsg::Bank(BankMsg::Send { .. })
    ));
}

#[test]
fn deliver_to_seeder_rejects_non_keeper() {
    let mut deps = setup();
    register_default(&mut deps, 11).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
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
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
            internal_id: 12,
            leftover: Uint128::new(10u128.pow(36)),
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::LeftoverExceedsEvmSupply { .. }
    ));
}

#[test]
fn deliver_to_seeder_rejects_repeat() {
    let mut deps = setup();
    register_default(&mut deps, 13).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    install_matching_sink(&mut deps, &seeder, 13);
    let keeper = deps.api.addr_make("keeper");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
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
            evm_authority: evm_authority.clone(),
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
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let keeper = deps.api.addr_make("keeper");
    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority: evm_authority.clone(),
            internal_id: 21,
            reason: "bootstrap_failed".to_string(),
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    match &res.messages[0].msg {
        CosmosMsg::Custom(w) => match &w.msg_data {
            InjectiveMsg::Burn { amount, .. } => {
                assert_eq!(
                    amount.amount,
                    Uint128::new(200_000_000u128) * Uint128::new(10u128.pow(18))
                );
            }
            other => panic!("expected Burn, got {:?}", other),
        },
        other => panic!("expected Custom Injective msg, got {:?}", other),
    }
    assert_eq!(
        LAUNCHES
            .load(
                deps.as_ref().storage,
                (&deps.api.addr_make("evm_authority"), 21)
            )
            .unwrap()
            .status,
        LaunchStatus::Refunded
    );
}

#[test]
fn refund_non_keeper_pre_deadline_is_rejected() {
    let mut deps = setup();
    register_default(&mut deps, 22).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority: evm_authority.clone(),
            internal_id: 22,
            reason: "stuck".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::RefundDeadlineNotReached { .. }
    ));
}

#[test]
fn refund_post_deadline_admin_succeeds_stranger_rejected() {
    let mut deps = setup();
    register_default(&mut deps, 23).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let mut env = mock_env();
    env.block.time = env.block.time.plus_seconds(REFUND_DEADLINE + 1);

    // A random caller is rejected even after the deadline — the post-deadline
    // path is NOT permissionless (anti-griefing); only keeper or admin.
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&stranger, &[]),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority: evm_authority.clone(),
            internal_id: 23,
            reason: "stuck_too_long".to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // The admin (consumer dApp timelock) can clean it up post-deadline.
    let admin = deps.api.addr_make("admin");
    execute(
        deps.as_mut(),
        env,
        message_info(&admin, &[]),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority: evm_authority.clone(),
            internal_id: 23,
            reason: "stuck_too_long".to_string(),
        },
    )
    .unwrap();
    assert_eq!(
        LAUNCHES
            .load(
                deps.as_ref().storage,
                (&deps.api.addr_make("evm_authority"), 23)
            )
            .unwrap()
            .status,
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
    // Two-step admin rotation: UpdateAdmin only parks `new_admin` as pending.
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
    assert_eq!(
        cfg.admin,
        admin.to_string(),
        "live admin unchanged pre-accept"
    );
    assert_eq!(cfg.pending_admin, Some(new_admin.to_string()));

    // A non-pending caller cannot accept.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::AcceptAdmin {},
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // The pending key accepts and becomes admin.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&new_admin, &[]),
        ExecuteMsg::AcceptAdmin {},
    )
    .unwrap();

    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.admin, new_admin.to_string());
    assert_eq!(cfg.pending_admin, None);
    assert_eq!(cfg.keeper, new_keeper.to_string());
    assert_eq!(cfg.forwarder, new_forwarder.to_string());
}

#[test]
fn register_launch_rejected_when_paused() {
    let mut deps = setup();
    let admin = deps.api.addr_make("admin");

    // A stranger cannot pause.
    let stranger = deps.api.addr_make("stranger");
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
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert!(cfg.paused);

    // RegisterLaunch is refused while paused (even for the keeper).
    let err = register_default(&mut deps, 1).unwrap_err();
    assert!(matches!(err, ContractError::Paused {}));

    // Unpause restores registration.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetPaused { paused: false },
    )
    .unwrap();
    register_default(&mut deps, 1).unwrap();
}

#[test]
fn update_refund_deadline_requires_admin_and_applies() {
    let mut deps = setup();
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateRefundDeadline {
            new_refund_deadline_seconds: 1,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateRefundDeadline {
            new_refund_deadline_seconds: 12_345,
        },
    )
    .unwrap();
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.refund_deadline_seconds, 12_345);
}

#[test]
fn update_decimals_requires_admin_and_bounds() {
    let mut deps = setup();
    let admin = deps.api.addr_make("admin");
    let stranger = deps.api.addr_make("stranger");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateDecimals { new_decimals: 6 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Above the 18-cap is rejected.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateDecimals { new_decimals: 19 },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::DecimalsOutOfRange { got: 19 }));

    // A valid retune applies to the default.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateDecimals { new_decimals: 6 },
    )
    .unwrap();
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.decimals, 6);
}

#[test]
fn accept_admin_without_pending_errors() {
    let mut deps = setup();
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
fn reply_decodes_token_pair_response_and_patches_erc20() {
    let mut deps = setup();
    register_default(&mut deps, 99).unwrap();
    let erc20 = "0x1234567890123456789012345678901234567890";
    simulate_create_token_pair_reply(&mut deps, 99, erc20);
    let stored = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("evm_authority"), 99),
        )
        .unwrap();
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
                evm_authority: deps.api.addr_make("evm_authority").to_string(),
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

#[test]
fn launch_by_denom_resolves_record_and_errors_on_unknown() {
    let mut deps = setup();
    register_default(&mut deps, 42).unwrap();
    let denom = default_denom(42);

    // The reverse index resolves the same record the (authority, id) path would.
    let by_denom: LaunchResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::LaunchByDenom {
                denom: denom.clone(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_denom.internal_id, 42);
    assert_eq!(by_denom.denom, denom);
    assert_eq!(
        by_denom.evm_authority,
        deps.api.addr_make("evm_authority").to_string()
    );

    let direct: LaunchResponse = from_json(
        query(
            deps.as_ref(),
            mock_env(),
            QueryMsg::Launch {
                evm_authority: deps.api.addr_make("evm_authority").to_string(),
                internal_id: 42,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_denom, direct, "denom lookup matches the direct record");

    // An unknown denom errors `not_found` rather than returning a stale record.
    let err = query(
        deps.as_ref(),
        mock_env(),
        QueryMsg::LaunchByDenom {
            denom: "factory/inj1bogus/never_registered".to_string(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, cosmwasm_std::StdError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

// --------------------------------------------------------------------------
// C-H1: keeper-gate + per-authority id namespace
// --------------------------------------------------------------------------

#[test]
fn register_launch_rejects_non_keeper() {
    // The squat/DoS fix: only the keeper can register a launch now.
    let mut deps = setup();
    let stranger = deps.api.addr_make("stranger");
    let info = message_info(&stranger, &fee_funds());
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
        salt_suffix: None,
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(matches!(err, ContractError::NotKeeper {}));
}

#[test]
fn two_authorities_share_internal_id_without_record_collision() {
    // Launches are keyed by (evm_authority, internal_id): the same id under two
    // distinct authorities yields two independent records (no overwrite/hijack).
    let mut deps = setup();
    let keeper = deps.api.addr_make("keeper");
    let auth_a = deps.api.addr_make("authority_a").to_string();
    let auth_b = deps.api.addr_make("authority_b").to_string();
    let seeder_factory = deps.api.addr_make("seeder_factory").to_string();
    let seeder_addr = deps.api.addr_make("seeder_addr").to_string();

    // Both authorities share internal_id 0, so the launch denom (which doesn't
    // embed the authority) is identical. The payloads differ only in
    // `refund_receiver`, which H-1 pins to each registering authority.
    for auth in [&auth_a, &auth_b] {
        let payload = sink_payload(
            &deps,
            default_denom(0),
            PoolKind::Xyk {
                choice_factory: deps.api.addr_make("choice_factory").to_string(),
                lp_destination: LpDestination::Burn,
            },
            auth,
        );
        let msg = ExecuteMsg::RegisterLaunch {
            internal_id: 0,
            evm_authority: auth.clone(),
            total_supply: Uint128::new(1_000_000),
            evm_supply: Uint128::new(800_000),
            pair_denom: PAIR_DENOM.to_string(),
            seeder_factory: seeder_factory.clone(),
            seeder_addr: seeder_addr.clone(),
            create_sink_payload: payload.clone(),
            choice_factory: None,
            salt_suffix: Some(TEST_SALT.to_string()),
            clmm_pool_auth: None,
        };
        execute(
            deps.as_mut(),
            mock_env(),
            message_info(&keeper, &fee_funds()),
            msg,
        )
        .unwrap();
    }

    let rec_a = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("authority_a"), 0),
        )
        .unwrap();
    let rec_b = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("authority_b"), 0),
        )
        .unwrap();
    assert_eq!(rec_a.evm_authority.to_string(), auth_a);
    assert_eq!(rec_b.evm_authority.to_string(), auth_b);
}

// --------------------------------------------------------------------------
// C-M2: RenounceDenomAdmin
// --------------------------------------------------------------------------

#[test]
fn renounce_denom_admin_after_delivered_emits_change_admin() {
    let mut deps = setup();
    register_default(&mut deps, 5).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    install_matching_sink(&mut deps, &seeder, 5);
    let keeper = deps.api.addr_make("keeper");

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority: evm_authority.clone(),
            internal_id: 5,
            leftover: Uint128::zero(),
        },
    )
    .unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RenounceDenomAdmin {
            evm_authority: evm_authority.clone(),
            internal_id: 5,
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    #[allow(deprecated)]
    match &res.messages[0].msg {
        CosmosMsg::Stargate { type_url, value } => {
            assert_eq!(type_url, MsgChangeAdmin::TYPE_URL);
            let decoded = MsgChangeAdmin::decode(value.as_slice()).unwrap();
            assert_eq!(decoded.denom, default_denom(5));
            assert_eq!(
                decoded.new_admin,
                "inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqe2hm49"
            );
        }
        other => panic!("expected Stargate MsgChangeAdmin, got {:?}", other),
    }

    let rec = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("evm_authority"), 5),
        )
        .unwrap();
    assert!(rec.admin_renounced);

    // Second call is rejected.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RenounceDenomAdmin {
            evm_authority,
            internal_id: 5,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ContractError::DenomAdminAlreadyRenounced { .. }
    ));
}

#[test]
fn renounce_denom_admin_rejected_before_delivered() {
    let mut deps = setup();
    register_default(&mut deps, 6).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RenounceDenomAdmin {
            evm_authority,
            internal_id: 6,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidLaunchStatus { .. }));
}

#[test]
fn renounce_denom_admin_rejects_unauthorized() {
    let mut deps = setup();
    register_default(&mut deps, 8).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::RenounceDenomAdmin {
            evm_authority,
            internal_id: 8,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

// --------------------------------------------------------------------------
// C-M3: DeliverToSeeder refuses a ghost seeder address
// --------------------------------------------------------------------------

#[test]
fn deliver_to_seeder_rejects_ghost_seeder_addr() {
    let mut deps = setup();
    register_default(&mut deps, 30).unwrap();
    // seeder_addr was NOT registered as a contract → looks like an EOA/ghost.
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority,
            internal_id: 30,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::SeederAddrNotAContract { .. }));
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

    // The contract sets the reply payload to a JSON `ReplyPayload`
    // `{evm_authority, internal_id}` (finding C-H1). Mirror that here.
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let payload = Binary::from(
        format!(
            r#"{{"evm_authority":"{}","internal_id":{}}}"#,
            evm_authority, internal_id
        )
        .into_bytes(),
    );
    #[allow(deprecated)]
    let reply_msg = Reply {
        id: 1,
        payload,
        gas_used: 0,
        result: SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: Some(response_bytes),
            msg_responses: vec![],
        }),
    };
    reply(deps.as_mut(), mock_env(), reply_msg).unwrap();
}

// --------------------------------------------------------------------------
// Anti-squat: Layer A entropy (salt_suffix) + Layer B gate (clmm_pool_auth)
// --------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn register_full(
    deps: &mut Deps,
    internal_id: u64,
    salt_suffix: Option<String>,
    clmm_pool_auth: Option<crate::msg::ClmmPoolAuth>,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    // Build a payload coherent with the args: token_denom tracks the salt and
    // pool_kind tracks whether a CLMM reservation is supplied. (Tests that
    // expect a pre-decode rejection — bad/empty/overlong salt — still reject
    // before the payload is inspected.)
    let token_denom = match &salt_suffix {
        Some(s) => denom_with_salt(internal_id, s),
        None => expected_denom(internal_id),
    };
    let pool_kind = match &clmm_pool_auth {
        Some(a) => PoolKind::Clmm {
            clmm_factory: a.clmm_factory.clone(),
            clmm_manager: deps.api.addr_make("clmm_manager").to_string(),
            fee_tier: a.fee,
            position_recipient: deps.api.addr_make("locker").to_string(),
            max_fee_multiple: None,
        },
        None => PoolKind::Xyk {
            choice_factory: deps.api.addr_make("choice_factory").to_string(),
            lp_destination: LpDestination::Burn,
        },
    };
    let payload = sink_payload(
        deps,
        token_denom,
        pool_kind,
        &deps.api.addr_make("evm_authority").to_string(),
    );
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: payload,
        choice_factory: None,
        salt_suffix,
        clmm_pool_auth,
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

#[test]
fn register_launch_salt_suffix_changes_denom() {
    let mut deps = setup();
    // The salt is mandatory and must be >= MIN_SALT_SUFFIX_LEN (8) alnum chars.
    let salt = "a1b2c3d4";
    let res = register_full(&mut deps, 42, Some(salt.to_string()), None).unwrap();

    // Still the legacy 5-message chain (entropy only changes the denom string).
    assert_eq!(res.messages.len(), 5);

    let expected = format!(
        "factory/{}/{}_{}_{}",
        mock_env().contract.address,
        PREFIX,
        42,
        salt
    );
    let stored = LAUNCHES
        .load(
            deps.as_ref().storage,
            (&deps.api.addr_make("evm_authority"), 42),
        )
        .unwrap();
    assert_eq!(stored.denom, expected);

    // And the CreateDenom subdenom carries the suffix.
    let plain: Vec<&CosmosMsg<injective_cosmwasm::InjectiveMsgWrapper>> = res
        .messages
        .iter()
        .filter(|sm| sm.reply_on == ReplyOn::Never)
        .map(|sm| &sm.msg)
        .collect();
    #[allow(deprecated)]
    match plain[0] {
        CosmosMsg::Stargate { value, .. } => {
            let decoded = MsgCreateDenom::decode(value.as_slice()).unwrap();
            assert_eq!(decoded.subdenom, format!("{}_{}_{}", PREFIX, 42, salt));
        }
        other => panic!("expected Stargate CreateDenom, got {:?}", other),
    }
}

#[test]
fn register_launch_rejects_non_alnum_salt_suffix() {
    let mut deps = setup();
    let err = register_full(&mut deps, 1, Some("bad-salt".to_string()), None).unwrap_err();
    assert!(
        matches!(err, ContractError::SaltSuffixInvalid { .. }),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_empty_salt_suffix() {
    let mut deps = setup();
    let err = register_full(&mut deps, 1, Some(String::new()), None).unwrap_err();
    assert!(
        matches!(err, ContractError::SaltSuffixInvalid { .. }),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_overlong_subdenom() {
    let mut deps = setup();
    // prefix(6) + "_" + id-digits + "_" + 40 chars overflows the 44-char cap.
    let err = register_full(&mut deps, 1, Some("a".repeat(40)), None).unwrap_err();
    assert!(
        matches!(err, ContractError::SubdenomTooLong { .. }),
        "{err:?}"
    );
}

#[test]
fn register_launch_clmm_pool_auth_emits_authorize_creation() {
    // LOW-1 (this session): the CLMM locker `position_recipient` pin now runs
    // UNCONDITIONALLY (no longer gated on `verify_seeder_derivation`), so this
    // CLMM register must go through the verify-ON harness — stub the factory
    // queries + supply the correctly-derived sink/locker — rather than the
    // `setup()` (verify-OFF) placeholder path it used before.
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    let clmm_factory = deps.api.addr_make("clmm_factory");
    let seeder_factory = deps.api.addr_make("seeder_factory").to_string();

    let salt = canonical_sink_salt(&deps.api, 42, TEST_SALT);
    let seeder_addr = install_seeder_factory(&mut deps, &salt);
    let locker = derive_locker_addr(&deps.api, &seeder_factory, 42, TEST_SALT);

    let res = register_clmm_verify(&mut deps, 42, seeder_addr.clone(), TEST_SALT, locker).unwrap();

    // The gate adds one extra WasmExec to the legacy 5-message chain.
    assert_eq!(res.messages.len(), 6);

    let denom = denom_with_salt(42, TEST_SALT);

    // Find the AuthorizeCreation message (the WasmExec aimed at the CLMM factory).
    let found = res.messages.iter().find_map(|sm| match &sm.msg {
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr,
            msg,
            funds,
        }) if contract_addr == &clmm_factory.to_string() => {
            assert!(funds.is_empty(), "AuthorizeCreation carries no funds");
            let parsed: choice_clmm_common::factory::ExecuteMsg = from_json(msg).unwrap();
            Some(parsed)
        }
        _ => None,
    });

    match found.expect("AuthorizeCreation message present") {
        choice_clmm_common::factory::ExecuteMsg::AuthorizeCreation {
            token_a,
            token_b,
            fee,
            creator,
            ttl_seconds,
        } => {
            assert_eq!(
                token_a,
                choice_clmm_common::types::AssetInfo::NativeToken { denom }
            );
            assert_eq!(
                token_b,
                choice_clmm_common::types::AssetInfo::NativeToken {
                    denom: PAIR_DENOM.to_string()
                }
            );
            assert_eq!(fee, 3000);
            assert_eq!(creator, seeder_addr, "creator must be the sink");
            assert_eq!(ttl_seconds, 0, "no-expiry for graduations");
        }
        other => panic!("expected AuthorizeCreation, got {:?}", other),
    }
}

#[test]
fn register_launch_without_clmm_pool_auth_keeps_legacy_chain() {
    let mut deps = setup();
    let res = register_full(&mut deps, 42, Some(TEST_SALT.to_string()), None).unwrap();
    assert_eq!(
        res.messages.len(),
        5,
        "no auth message when clmm_pool_auth is None"
    );
}

#[test]
fn register_launch_rejects_nonzero_clmm_auth_ttl() {
    // S-3: a lapsing pool reservation re-opens the CLMM squat window, so the
    // issuer requires `ttl_seconds == 0` (no-expiry reservation).
    let mut deps = setup();
    let auth = crate::msg::ClmmPoolAuth {
        clmm_factory: deps.api.addr_make("clmm_factory").to_string(),
        fee: 3000,
        ttl_seconds: 3_600,
    };
    let err = register_full(&mut deps, 42, Some(TEST_SALT.to_string()), Some(auth)).unwrap_err();
    assert!(
        matches!(err, ContractError::ClmmAuthTtlMustBeZero { got: 3_600 }),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_missing_salt_suffix() {
    // salt_suffix is mandatory now (Layer A anti-squat entropy): None is refused.
    let mut deps = setup();
    let err = register_full(&mut deps, 1, None, None).unwrap_err();
    assert!(
        matches!(err, ContractError::SaltSuffixRequired {}),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_too_short_salt_suffix() {
    // A salt below MIN_SALT_SUFFIX_LEN (8) chars is rejected even though it's
    // ASCII-alphanumeric.
    let mut deps = setup();
    let err = register_full(&mut deps, 1, Some("a1b2c3".to_string()), None).unwrap_err();
    assert!(
        matches!(err, ContractError::SaltSuffixInvalid { .. }),
        "{err:?}"
    );
}

// --------------------------------------------------------------------------
// Anti-squat enforcement: the issuer decodes create_sink_payload and refuses
// any launch whose reservation / denom is incoherent with what the sink seeds.
// --------------------------------------------------------------------------

/// Register with an explicit payload + auth, bypassing the coherent helpers so
/// we can exercise the rejection paths.
fn register_raw(
    deps: &mut Deps,
    internal_id: u64,
    create_sink_payload: Binary,
    clmm_pool_auth: Option<crate::msg::ClmmPoolAuth>,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload,
        choice_factory: None,
        // salt_suffix is mandatory; callers that want to reach the payload-decode
        // / auth checks must build their payload's token_denom with TEST_SALT.
        salt_suffix: Some(TEST_SALT.to_string()),
        clmm_pool_auth,
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

#[test]
fn register_launch_rejects_clmm_payload_without_auth() {
    let mut deps = setup();
    let clmm_factory = deps.api.addr_make("clmm_factory").to_string();
    let payload = clmm_sink_payload(&deps, denom_with_salt(1, TEST_SALT), clmm_factory, 3000);
    // CLMM sink but no reservation → squattable slot, must be refused.
    let err = register_raw(&mut deps, 1, payload, None).unwrap_err();
    assert!(matches!(err, ContractError::ClmmAuthRequired {}), "{err:?}");
}

#[test]
fn register_launch_rejects_clmm_auth_fee_mismatch() {
    let mut deps = setup();
    let clmm_factory = deps.api.addr_make("clmm_factory").to_string();
    // Sink seeds the 3000 tier but the reservation guards the 500 tier.
    let payload = clmm_sink_payload(&deps, denom_with_salt(1, TEST_SALT), clmm_factory.clone(), 3000);
    let auth = crate::msg::ClmmPoolAuth {
        clmm_factory,
        fee: 500,
        ttl_seconds: 0,
    };
    let err = register_raw(&mut deps, 1, payload, Some(auth)).unwrap_err();
    assert!(
        matches!(
            err,
            ContractError::ClmmAuthFeeMismatch {
                auth_fee: 500,
                sink_fee: 3000
            }
        ),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_clmm_auth_factory_mismatch() {
    let mut deps = setup();
    let sink_factory = deps.api.addr_make("clmm_factory_a").to_string();
    let payload = clmm_sink_payload(&deps, denom_with_salt(1, TEST_SALT), sink_factory, 3000);
    let auth = crate::msg::ClmmPoolAuth {
        clmm_factory: deps.api.addr_make("clmm_factory_b").to_string(),
        fee: 3000,
        ttl_seconds: 0,
    };
    let err = register_raw(&mut deps, 1, payload, Some(auth)).unwrap_err();
    assert!(
        matches!(err, ContractError::ClmmAuthFactoryMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_xyk_payload_with_stray_auth() {
    let mut deps = setup();
    let payload = xyk_sink_payload(&deps, 1);
    let auth = crate::msg::ClmmPoolAuth {
        clmm_factory: deps.api.addr_make("clmm_factory").to_string(),
        fee: 3000,
        ttl_seconds: 0,
    };
    let err = register_raw(&mut deps, 1, payload, Some(auth)).unwrap_err();
    assert!(
        matches!(err, ContractError::ClmmAuthUnexpected {}),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_sink_token_denom_substitution() {
    let mut deps = setup();
    // Sink claims to seed a DIFFERENT denom than the one this launch creates.
    let payload = xyk_sink_payload(&deps, 999);
    let err = register_raw(&mut deps, 1, payload, None).unwrap_err();
    assert!(
        matches!(
            err,
            ContractError::SinkPayloadMismatch { ref field, .. } if field == "token_denom"
        ),
        "{err:?}"
    );
}

#[test]
fn register_launch_rejects_undecodable_sink_payload() {
    let mut deps = setup();
    let payload = Binary::from(br#"{"not_create_sink":{}}"#.to_vec());
    let err = register_raw(&mut deps, 1, payload, None).unwrap_err();
    assert!(
        matches!(err, ContractError::SinkPayloadDecode(_)),
        "{err:?}"
    );
}

// ------------------------------------------------------------------------
// P2-B / P2-C / P2-A regression coverage
// ------------------------------------------------------------------------

#[test]
fn deliver_to_seeder_rejects_sink_configured_for_a_different_launch() {
    // P2-B: the seeder_addr hosts a real sink, but it is configured for a
    // DIFFERENT launch's denom. DeliverToSeeder must refuse to bank-send
    // cw_held there rather than stranding it at a wrong-but-codeful contract.
    let mut deps = setup();
    register_default(&mut deps, 21).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    // Sink claims token_denom of launch 999, not launch 21.
    install_matching_sink(&mut deps, &seeder, 999);
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority,
            internal_id: 21,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, ContractError::SeederSinkConfigMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn deliver_to_seeder_rejects_sink_with_wrong_issuer() {
    // S-6: even a denom/pair-matching sink must name THIS issuer as its `issuer`,
    // so `cw_held` can only ship to a sink that was created to be fed by us.
    let mut deps = setup();
    register_default(&mut deps, 21).unwrap();
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();

    // A sink whose denom/pair match launch 21 but whose `issuer` is some other
    // contract — a denom-matching look-alike under attacker control.
    let cfg = SinkConfigResponse {
        factory: Some(deps.api.addr_make("seeder_factory").to_string()),
        issuer: deps.api.addr_make("other_issuer").to_string(),
        token_denom: default_denom(21),
        pair_denom: PAIR_DENOM.to_string(),
        token_decimals: 18,
        pair_decimals: 18,
        pool_kind: PoolKind::Xyk {
            choice_factory: deps.api.addr_make("choice_factory").to_string(),
            lp_destination: LpDestination::Burn,
        },
        refund_receiver: deps.api.addr_make("refund_receiver").to_string(),
        deadline_seconds: REFUND_DEADLINE,
        instantiated_at: 0,
        expected_token: None,
        expected_pair: None,
    };
    deps.querier
        .with_smart_query_response(&seeder, to_json_binary(&cfg).unwrap());

    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority,
            internal_id: 21,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, ContractError::SeederSinkConfigMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn update_refund_deadline_rejects_below_floor() {
    // P2-C: admin cannot collapse the refund grace window below the floor.
    let mut deps = setup();
    let admin = deps.api.addr_make("admin");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateRefundDeadline {
            new_refund_deadline_seconds: MIN_REFUND_DEADLINE_SECONDS - 1,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::RefundDeadlineTooShort { .. }));

    // At the floor it is accepted.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::UpdateRefundDeadline {
            new_refund_deadline_seconds: MIN_REFUND_DEADLINE_SECONDS,
        },
    )
    .unwrap();
}

#[test]
fn migrate_patch_rejects_cross_major_but_accepts_same_major() {
    // P2-A: Patch accepts an in-major bump (current build is 1.x) ...
    let mut deps = setup();
    migrate(deps.as_mut(), mock_env(), MigrateMsg::Patch {}).unwrap();

    // ... and rejects a cross-major patch (a 1.x build must not patch over a
    // stored 2.x deployment), instead of the old hard-coded `starts_with("1.")`
    // that bricked every patch once a 2.x build shipped.
    cw2::set_contract_version(
        deps.as_mut().storage,
        "crates.io:choice-mts-issuer",
        "2.0.0",
    )
    .unwrap();
    let err = migrate(deps.as_mut(), mock_env(), MigrateMsg::Patch {}).unwrap_err();
    assert!(
        matches!(err, ContractError::InvalidMigration { .. }),
        "{err:?}"
    );
}

// =====================================================================
// P1: on-chain `seeder_addr` Instantiate2-derivation verification.
//
// `setup()` flips the check OFF for the legacy tests; these instantiate a
// fresh issuer (default ON), stub the seeder factory's `FactoryConfig` +
// `CodeInfo` so the contract can read `sink_code_id`/checksum, and recompute
// the expected sink address the same way the contract does. They prove the
// CODE PATH (query → derive → truncate-20 → compare) is internally correct;
// the `chain_capability_harness` integration tests prove the truncation +
// key construction match Injective's real `Instantiate2`.
// =====================================================================

/// The seeder `sink_code_id` + its wasm checksum the verify tests pin.
const SINK_CODE_ID: u64 = 7;
const SINK_CHECKSUM: [u8; 32] = [0xab; 32];

/// Instantiate an issuer WITHOUT flipping the derivation check off (i.e. with
/// the secure default ON). Mirrors `setup()` minus the `SetVerifySeederDerivation`.
fn instantiate_issuer(deps: &mut Deps) {
    let admin = deps.api.addr_make("admin");
    let msg = InstantiateMsg {
        admin: admin.to_string(),
        subdenom_prefix: PREFIX.to_string(),
        decimals: 18,
        keeper: deps.api.addr_make("keeper").to_string(),
        forwarder: deps.api.addr_make("forwarder").to_string(),
        refund_deadline_seconds: REFUND_DEADLINE,
    };
    instantiate(deps.as_mut(), mock_env(), message_info(&admin, &[]), msg).unwrap();
}

/// The canonical entropic sink `Instantiate2` salt the contract now requires the
/// forwarded `CreateSink` to carry (verify ON):
/// `canonical(issuer) || be_u64(internal_id) || salt_suffix`. Mirrors the
/// contract's `sink_salt_bytes`. `salt_suffix` must be the same mandatory Layer A
/// suffix folded into the launch denom.
fn canonical_sink_salt(api: &MockApi, internal_id: u64, salt_suffix: &str) -> Binary {
    let issuer_canon = api
        .addr_canonicalize(mock_env().contract.address.as_str())
        .unwrap();
    let mut salt = Vec::new();
    salt.extend_from_slice(issuer_canon.as_slice());
    salt.extend_from_slice(&internal_id.to_be_bytes());
    salt.extend_from_slice(salt_suffix.as_bytes());
    Binary::new(salt)
}

/// Recompute the sink `Instantiate2` address exactly as the contract does:
/// the full 32-byte wasmd hash truncated to Injective's 20-byte width, then
/// humanized through the same `MockApi`.
fn derive_sink_addr(api: &MockApi, seeder_factory: &str, salt: &Binary) -> String {
    let creator = api.addr_canonicalize(seeder_factory).unwrap();
    let full = instantiate2_address(&SINK_CHECKSUM, &creator, salt.as_slice()).unwrap();
    let canon: CanonicalAddr = full.as_slice()[..20].into();
    api.addr_humanize(&canon).unwrap().to_string()
}

/// Stub the seeder factory's `FactoryConfig` (→ `sink_code_id`) + the matching
/// `CodeInfo` checksum so `RegisterLaunch`'s derivation can run, and return the
/// sink address it must derive for `salt`.
fn install_seeder_factory(deps: &mut Deps, salt: &Binary) -> String {
    let seeder_factory = deps.api.addr_make("seeder_factory");
    let cfg = FactoryConfigResponse {
        admin: deps.api.addr_make("seeder_admin").to_string(),
        pending_admin: None,
        sink_code_id: SINK_CODE_ID,
        choice_factory: deps.api.addr_make("choice_factory").to_string(),
        clmm_factory: None,
        clmm_manager: None,
        paused: false,
    };
    deps.querier
        .with_smart_query_response(seeder_factory.as_str(), to_json_binary(&cfg).unwrap());
    deps.querier.with_code_info(SINK_CODE_ID, SINK_CHECKSUM);
    derive_sink_addr(&deps.api, seeder_factory.as_str(), salt)
}

/// Build a coherent XYK `CreateSink` payload carrying the canonical `salt` for
/// launch `id` whose `token_denom` embeds the mandatory `salt_suffix`.
fn salted_xyk_payload(deps: &Deps, internal_id: u64, salt_suffix: &str) -> Binary {
    to_json_binary(&SeederExecuteMsg::CreateSink {
        salt: canonical_sink_salt(&deps.api, internal_id, salt_suffix),
        sink_init: SinkInit {
            issuer: mock_env().contract.address.to_string(),
            token_denom: denom_with_salt(internal_id, salt_suffix),
            pair_denom: PAIR_DENOM.to_string(),
            token_decimals: 18,
            pair_decimals: 18,
            pool_kind: PoolKind::Xyk {
                choice_factory: deps.api.addr_make("choice_factory").to_string(),
                lp_destination: LpDestination::Burn,
            },
            // H-1: refund_receiver must equal the launch's evm_authority.
            refund_receiver: deps.api.addr_make("evm_authority").to_string(),
            deadline_seconds: REFUND_DEADLINE,
            expected_token: None,
            expected_pair: None,
        },
    })
    .unwrap()
}

/// `RegisterLaunch` with an explicit `seeder_addr` + mandatory `salt_suffix`
/// (verification ON). The forwarded sink salt is derived canonically from the
/// suffix so it satisfies the Layer A sink-salt-convention check.
fn register_with_seeder(
    deps: &mut Deps,
    internal_id: u64,
    seeder_addr: String,
    salt_suffix: &str,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    let payload = salted_xyk_payload(deps, internal_id, salt_suffix);
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr,
        create_sink_payload: payload,
        choice_factory: None,
        salt_suffix: Some(salt_suffix.to_string()),
        clmm_pool_auth: None,
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

#[test]
fn instantiate_defaults_verify_seeder_derivation_on() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert!(
        cfg.verify_seeder_derivation,
        "seeder-addr derivation must be ON by default (secure-by-default)"
    );
}

#[test]
fn register_launch_verify_accepts_derived_seeder_addr() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    // The forwarded sink salt must be the canonical entropic salt derived from
    // the mandatory salt_suffix.
    let salt = canonical_sink_salt(&deps.api, 77, TEST_SALT);
    let sink = install_seeder_factory(&mut deps, &salt);

    // The keeper supplies the correctly-derived sink address → accepted.
    register_with_seeder(&mut deps, 77, sink.clone(), TEST_SALT).unwrap();

    let rec = LAUNCHES
        .load(&deps.storage, (&deps.api.addr_make("evm_authority"), 77))
        .unwrap();
    assert_eq!(rec.seeder_addr.as_str(), sink);
    assert_eq!(rec.status, LaunchStatus::Registered);
}

#[test]
fn register_launch_verify_rejects_lookalike_seeder_addr() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    let salt = canonical_sink_salt(&deps.api, 77, TEST_SALT);
    let _correct = install_seeder_factory(&mut deps, &salt);

    // A fully-compromised keeper points at a denom-matching look-alike sink it
    // controls (any address other than the derived one). With derivation ON the
    // issuer recomputes the real sink and rejects the substitution.
    let lookalike = deps.api.addr_make("attacker_sink").to_string();
    let err = register_with_seeder(&mut deps, 77, lookalike, TEST_SALT).unwrap_err();
    assert!(
        matches!(err, ContractError::SeederAddrDerivationMismatch { .. }),
        "expected SeederAddrDerivationMismatch, got {err:?}"
    );
}

#[test]
fn register_launch_verify_errors_when_factory_config_unavailable() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    // No FactoryConfig/CodeInfo stub → the factory query fails and registration
    // reverts (rather than silently trusting the keeper address). The forwarded
    // sink salt is still canonical, so the salt-convention check passes and the
    // revert comes from the (unstubbed) factory query, as intended.
    let sink = deps.api.addr_make("seeder_addr").to_string();
    let err = register_with_seeder(&mut deps, 77, sink, TEST_SALT).unwrap_err();
    assert!(
        matches!(err, ContractError::SeederFactoryConfigQuery { .. }),
        "expected SeederFactoryConfigQuery, got {err:?}"
    );
}

// =====================================================================
// Audit H-1 / M-1 / M-3 (this session).
// =====================================================================

/// H-1: the sink's `refund_receiver` must equal the launch's `evm_authority`.
/// Always enforced (independent of `verify_seeder_derivation`).
#[test]
fn register_launch_rejects_refund_receiver_mismatch() {
    let mut deps = setup();
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    // Payload whose refund_receiver is an attacker address, not the authority.
    // token_denom must carry the mandatory salt so the launch reaches the
    // refund_receiver check rather than tripping the token_denom mismatch first.
    let payload = sink_payload(
        &deps,
        denom_with_salt(5, TEST_SALT),
        PoolKind::Xyk {
            choice_factory: deps.api.addr_make("choice_factory").to_string(),
            lp_destination: LpDestination::Burn,
        },
        &deps.api.addr_make("attacker").to_string(),
    );
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id: 5,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr: deps.api.addr_make("seeder_addr").to_string(),
        create_sink_payload: payload,
        choice_factory: None,
        salt_suffix: Some(TEST_SALT.to_string()),
        clmm_pool_auth: None,
    };
    let err = execute(deps.as_mut(), mock_env(), info, msg).unwrap_err();
    assert!(
        matches!(err, ContractError::RefundReceiverMismatch { .. }),
        "expected RefundReceiverMismatch, got {err:?}"
    );
}

/// Recompute the LOCKER `Instantiate2` address the contract pins
/// `position_recipient` to: salt = `b"locker" || canonical(issuer) || be(id) ||
/// salt_suffix` (the locker salt now shares the mandatory Layer A suffix).
fn derive_locker_addr(api: &MockApi, seeder_factory: &str, internal_id: u64, salt_suffix: &str) -> String {
    let issuer_canon = api
        .addr_canonicalize(mock_env().contract.address.as_str())
        .unwrap();
    let mut salt = Vec::new();
    salt.extend_from_slice(b"locker");
    salt.extend_from_slice(issuer_canon.as_slice());
    salt.extend_from_slice(&internal_id.to_be_bytes());
    salt.extend_from_slice(salt_suffix.as_bytes());
    let creator = api.addr_canonicalize(seeder_factory).unwrap();
    let full = instantiate2_address(&SINK_CHECKSUM, &creator, &salt).unwrap();
    let canon: CanonicalAddr = full.as_slice()[..20].into();
    api.addr_humanize(&canon).unwrap().to_string()
}

/// Build a verify-on CLMM `CreateSink` payload with a chosen `position_recipient`.
/// The forwarded sink salt is canonical and the token_denom embeds the suffix.
fn salted_clmm_payload(
    deps: &Deps,
    internal_id: u64,
    salt_suffix: &str,
    position_recipient: String,
) -> Binary {
    to_json_binary(&SeederExecuteMsg::CreateSink {
        salt: canonical_sink_salt(&deps.api, internal_id, salt_suffix),
        sink_init: SinkInit {
            issuer: mock_env().contract.address.to_string(),
            token_denom: denom_with_salt(internal_id, salt_suffix),
            pair_denom: PAIR_DENOM.to_string(),
            token_decimals: 18,
            pair_decimals: 18,
            pool_kind: PoolKind::Clmm {
                clmm_factory: deps.api.addr_make("clmm_factory").to_string(),
                clmm_manager: deps.api.addr_make("clmm_manager").to_string(),
                fee_tier: 3000,
                position_recipient,
                max_fee_multiple: None,
            },
            refund_receiver: deps.api.addr_make("evm_authority").to_string(),
            deadline_seconds: REFUND_DEADLINE,
            expected_token: None,
            expected_pair: None,
        },
    })
    .unwrap()
}

fn register_clmm_verify(
    deps: &mut Deps,
    internal_id: u64,
    seeder_addr: String,
    salt_suffix: &str,
    position_recipient: String,
) -> Result<cosmwasm_std::Response<injective_cosmwasm::InjectiveMsgWrapper>, ContractError> {
    let caller = deps.api.addr_make("keeper");
    let info = message_info(&caller, &fee_funds());
    let payload = salted_clmm_payload(deps, internal_id, salt_suffix, position_recipient);
    let msg = ExecuteMsg::RegisterLaunch {
        internal_id,
        evm_authority: deps.api.addr_make("evm_authority").to_string(),
        total_supply: Uint128::new(1_000_000_000u128) * Uint128::new(10u128.pow(18)),
        evm_supply: Uint128::new(800_000_000u128) * Uint128::new(10u128.pow(18)),
        pair_denom: PAIR_DENOM.to_string(),
        seeder_factory: deps.api.addr_make("seeder_factory").to_string(),
        seeder_addr,
        create_sink_payload: payload,
        choice_factory: None,
        salt_suffix: Some(salt_suffix.to_string()),
        clmm_pool_auth: Some(crate::msg::ClmmPoolAuth {
            clmm_factory: deps.api.addr_make("clmm_factory").to_string(),
            fee: 3000,
            ttl_seconds: 0,
        }),
    };
    execute(deps.as_mut(), mock_env(), info, msg)
}

/// H-1: a CLMM launch's `position_recipient` is pinned to the derived locker.
/// The correct locker is accepted; any other address (a compromised keeper's
/// attacker-owned recipient) is rejected — closing the locked-LP theft vector.
#[test]
fn register_launch_verify_pins_clmm_position_recipient() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);
    let seeder_factory = deps.api.addr_make("seeder_factory").to_string();

    // The canonical sink salt embeds the internal_id, so each launch derives its
    // own sink. Stub the factory and derive the per-launch sink + locker.
    let salt_88 = canonical_sink_salt(&deps.api, 88, TEST_SALT);
    let sink_88 = install_seeder_factory(&mut deps, &salt_88);
    let locker_88 = derive_locker_addr(&deps.api, &seeder_factory, 88, TEST_SALT);

    // Correct derived locker → accepted.
    register_clmm_verify(&mut deps, 88, sink_88, TEST_SALT, locker_88).unwrap();
    let rec = LAUNCHES
        .load(&deps.storage, (&deps.api.addr_make("evm_authority"), 88))
        .unwrap();
    assert_eq!(rec.status, LaunchStatus::Registered);

    // Attacker-owned recipient at the (valid, correctly-derived) sink for id 89 →
    // the locker pin rejects it (sink salt + addr still canonical so we reach the
    // locker check, not an earlier guard).
    let salt_89 = canonical_sink_salt(&deps.api, 89, TEST_SALT);
    let sink_89 = install_seeder_factory(&mut deps, &salt_89);
    let attacker = deps.api.addr_make("attacker").to_string();
    let err = register_clmm_verify(&mut deps, 89, sink_89, TEST_SALT, attacker).unwrap_err();
    assert!(
        matches!(err, ContractError::LockerAddrDerivationMismatch { .. }),
        "expected LockerAddrDerivationMismatch, got {err:?}"
    );
}

/// M-1: while paused, a keeper-initiated `RefundFailedLaunch` is halted (so a
/// compromised keeper can't mass-refund in-flight launches during an incident).
#[test]
fn refund_failed_launch_keeper_blocked_when_paused() {
    let mut deps = setup();
    register_default(&mut deps, 30).unwrap();
    let admin = deps.api.addr_make("admin");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetPaused { paused: true },
    )
    .unwrap();

    let keeper = deps.api.addr_make("keeper");
    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority,
            internal_id: 30,
            reason: "test".to_string(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, ContractError::KeeperRefundPaused {}),
        "expected KeeperRefundPaused, got {err:?}"
    );
}

/// M-3: a launch registered while derivation was OFF (`seeder_verified=false`)
/// cannot `DeliverToSeeder` once verification is flipped back ON.
#[test]
fn deliver_to_seeder_rejects_unverified_record_when_verify_on() {
    let mut deps = setup(); // verify is OFF here → record.seeder_verified=false
    register_default(&mut deps, 31).unwrap();
    simulate_create_token_pair_reply(&mut deps, 31, "0xdeadbeef00000000000000000000000000000000");

    // Admin flips derivation back ON.
    let admin = deps.api.addr_make("admin");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetVerifySeederDerivation { enabled: true },
    )
    .unwrap();

    let evm_authority = deps.api.addr_make("evm_authority").to_string();
    let seeder = deps.api.addr_make("seeder_addr").to_string();
    install_matching_sink(&mut deps, &seeder, 31);
    let keeper = deps.api.addr_make("keeper");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::DeliverToSeeder {
            evm_authority,
            internal_id: 31,
            leftover: Uint128::zero(),
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, ContractError::SeederDerivationNotVerified {}),
        "expected SeederDerivationNotVerified, got {err:?}"
    );
}

#[test]
fn set_verify_seeder_derivation_admin_only_and_toggles() {
    let mut deps = new_deps();
    instantiate_issuer(&mut deps);

    // Non-admin rejected.
    let stranger = deps.api.addr_make("stranger");
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::SetVerifySeederDerivation { enabled: false },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}), "{err:?}");

    // Admin disables → config reflects it.
    let admin = deps.api.addr_make("admin");
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&admin, &[]),
        ExecuteMsg::SetVerifySeederDerivation { enabled: false },
    )
    .unwrap();
    let cfg: ConfigResponse =
        from_json(query(deps.as_ref(), mock_env(), QueryMsg::Config {}).unwrap()).unwrap();
    assert!(!cfg.verify_seeder_derivation);

    // With the check OFF, a look-alike `seeder_addr` is accepted (v1 trust model)
    // — confirms the toggle actually gates the derivation path. The salt_suffix is
    // still mandatory and the payload's token_denom must embed it.
    let lookalike = deps.api.addr_make("any_sink").to_string();
    register_with_seeder(&mut deps, 88, lookalike, TEST_SALT).unwrap();
}

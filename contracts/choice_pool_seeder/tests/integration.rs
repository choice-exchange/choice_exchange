#![cfg(test)]
//! Integration tests for `choice_pool_seeder` against `injective_test_tube`.
//!
//! Requires the optimised wasm artifacts in `artifacts/` — run `make build-all`
//! (or `./build_release.sh`) before
//! `cargo test --test integration -p choice-pool-seeder`. The full-lifecycle
//! tests deploy the real `choice_factory` + `choice_pair` (XYK) and
//! `choice_clmm_factory` + `choice_clmm_pool` + `choice_clmm_manager` (CLMM)
//! stacks alongside the seeder, so all of those artifacts must be present.
//!
//! ## Coverage matrix
//!
//! Wiring / auth:
//!  * `InstantiateMsg::Factory` store + instantiate + `Role` / `FactoryConfig`
//!    query round-trip.
//!  * Admin rotation (`UpdateAdmin` / `UpdateSinkCodeId`) auth: stranger
//!    rejected, admin succeeds, both fields persist.
//!  * Wrong-role gate on `Settle` / `Refund` against a factory instance
//!    (verifies the dispatch table).
//!
//! Full on-chain lifecycle (previously `#[ignore]`d — now wired up):
//!  * XYK: `CreateSink` → fund sink → permissionless `Settle` → `choice_factory`
//!    creates the pair, liquidity is provided, the minted LP is burned, and the
//!    seed balances are fully drained.
//!  * CLMM: `CreateLocker` + `CreateSink` → fund sink → permissionless `Settle`
//!    → `choice_clmm_factory` creates the pool at the seed ratio, a full-range
//!    position NFT is minted to the locker, dust is swept,
//!    and `locker.CollectFees` routes swap fees to the beneficiary.
//!
//! Genericity (a non-SHROOM consumer):
//!  * `second_consumer_sendto_lp_and_committed_seed_xyk` — drives a full XYK
//!    graduation for a consumer whose economic model differs from SHROOM's:
//!    LP routed to a treasury (`LpDestination::SendTo`) instead of burned, with
//!    committed `expected_token` / `expected_pair` seed amounts. Proves the
//!    seeder is dApp-agnostic — Burn + uncommitted seed are config, not code.

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::{MsgSend, QueryBalanceRequest},
    injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin,
    Account, Bank, InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_pool_seeder::msg::{
    ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg, LockerInit, LpDestination,
    PoolKind, QueryMsg, RoleResponse, SinkInit, SinkStateResponse,
};
use choice_pool_seeder::state::SinkStatus;

// Legacy XYK stack
use choice::asset::{AssetInfo as ChoiceAssetInfo, PairInfo};
use choice::factory::{
    ExecuteMsg as ChoiceFactoryExecuteMsg, InstantiateMsg as ChoiceFactoryInstantiateMsg,
    QueryMsg as ChoiceFactoryQueryMsg,
};
use choice::pair::{PoolResponse, QueryMsg as ChoicePairQueryMsg};

// CLMM stack
use choice_clmm_common::factory::{
    InstantiateMsg as ClmmFactoryInstantiateMsg, QueryMsg as ClmmFactoryQueryMsg,
};
use choice_clmm_common::manager::{
    InstantiateMsg as ClmmManagerInstantiateMsg, PositionWithFeesResponse,
    QueryMsg as ClmmManagerQueryMsg,
};
use choice_clmm_common::pool::ExecuteMsg as ClmmPoolExecuteMsg;
use choice_clmm_common::types::AssetInfo as ClmmAssetInfo;

// ------------------------------------------------------------------------
// Artifacts + small helpers
// ------------------------------------------------------------------------

fn get_wasm(filename: &str) -> Vec<u8> {
    let path = format!("../../artifacts/{}", filename);
    std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "missing {}. Run `make build-all` from choice_exchange/ first.",
            path
        )
    })
}

fn artifact() -> Vec<u8> {
    get_wasm("choice_pool_seeder.wasm")
}

fn store(wasm: &Wasm<InjectiveTestApp>, admin: &SigningAccount, filename: &str) -> u64 {
    wasm.store_code(&get_wasm(filename), None, admin)
        .unwrap()
        .data
        .code_id
}

/// Bank-send a single native denom from a signing account to any address.
fn bank_send(app: &InjectiveTestApp, from: &SigningAccount, to: &str, denom: &str, amount: u128) {
    Bank::new(app)
        .send(
            MsgSend {
                from_address: from.address(),
                to_address: to.to_string(),
                amount: vec![ProtoCoin {
                    denom: denom.to_string(),
                    amount: amount.to_string(),
                }],
            },
            from,
        )
        .unwrap();
}

fn bank_balance(app: &InjectiveTestApp, address: &str, denom: &str) -> u128 {
    Bank::new(app)
        .query_balance(&QueryBalanceRequest {
            address: address.to_string(),
            denom: denom.to_string(),
        })
        .unwrap()
        .balance
        .map(|c| c.amount.parse::<u128>().unwrap())
        .unwrap_or(0)
}

/// Pull the `_contract_address` emitted by an `instantiate` event — used to
/// recover the deterministic Instantiate2 address of a sink / locker that the
/// factory just spawned.
fn instantiated_addr(
    res: &injective_test_tube::ExecuteResponse<
        injective_test_tube::injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse,
    >,
) -> String {
    res.events
        .iter()
        .find(|e| e.ty == "instantiate")
        .and_then(|e| e.attributes.iter().find(|a| a.key == "_contract_address"))
        .map(|a| a.value.clone())
        .expect("instantiate event with _contract_address attribute")
}

// Minimal mirrors of the cw721 response shapes the manager returns, so the
// test doesn't take a direct cw721 dependency (serde ignores extra fields).
#[derive(serde::Deserialize)]
struct TokensResp {
    tokens: Vec<String>,
}
#[derive(serde::Deserialize)]
struct OwnerOfResp {
    owner: String,
}

fn native(denom: &str) -> ChoiceAssetInfo {
    ChoiceAssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

fn clmm_native(denom: &str) -> ClmmAssetInfo {
    ClmmAssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

// ------------------------------------------------------------------------
// Wiring / auth tests (factory-only, no DEX stack)
// ------------------------------------------------------------------------

struct Env {
    app: InjectiveTestApp,
    admin: SigningAccount,
    stranger: SigningAccount,
    factory: String,
}

fn setup() -> Env {
    let app = InjectiveTestApp::new();
    let funded = &[Coin::new(1_000_000_000_000_000_000_000u128, "inj")];
    let admin = app.init_account(funded).unwrap();
    let stranger = app.init_account(funded).unwrap();
    let choice_factory = app.init_account(funded).unwrap();

    let wasm = Wasm::new(&app);
    let code_id = wasm
        .store_code(&artifact(), None, &admin)
        .unwrap()
        .data
        .code_id;

    let factory_addr = wasm
        .instantiate(
            code_id,
            &InstantiateMsg::Factory(FactoryInit {
                admin: admin.address(),
                sink_code_id: code_id, // single-binary: factory + sink share the same code-id
                choice_factory: choice_factory.address(),
                clmm_factory: None,
                clmm_manager: None,
            }),
            Some(&admin.address()),
            Some("choice_pool_seeder_factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    Env {
        app,
        admin,
        stranger,
        factory: factory_addr,
    }
}

#[test]
fn instantiate_factory_and_query_config() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let role: RoleResponse = wasm.query(&env.factory, &QueryMsg::Role {}).unwrap();
    match role {
        RoleResponse::Factory(cfg) => {
            assert_eq!(cfg.admin, env.admin.address());
        }
        _ => panic!("expected Factory role"),
    }
    let cfg: FactoryConfigResponse = wasm
        .query(&env.factory, &QueryMsg::FactoryConfig {})
        .unwrap();
    assert_eq!(cfg.admin, env.admin.address());
}

#[test]
fn admin_rotation_requires_admin() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let new_admin = env
        .app
        .init_account(&[Coin::new(1_000_000_000_000_000_000u128, "inj")])
        .unwrap();

    let err = wasm
        .execute(
            &env.factory,
            &ExecuteMsg::UpdateAdmin {
                new_admin: new_admin.address(),
            },
            &[],
            &env.stranger,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Unauthorized"),
        "stranger should be rejected, got: {}",
        err
    );

    // Step 1: admin proposes — parks `new_admin` as pending, no rotation yet.
    wasm.execute(
        &env.factory,
        &ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.address(),
        },
        &[],
        &env.admin,
    )
    .unwrap();
    let cfg: FactoryConfigResponse = wasm
        .query(&env.factory, &QueryMsg::FactoryConfig {})
        .unwrap();
    assert_eq!(
        cfg.admin,
        env.admin.address(),
        "live admin unchanged pre-accept"
    );
    assert_eq!(cfg.pending_admin, Some(new_admin.address()));

    // The pending key cannot yet act as admin.
    let err = wasm
        .execute(
            &env.factory,
            &ExecuteMsg::UpdateSinkCodeId {
                new_sink_code_id: 42,
            },
            &[],
            &new_admin,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Unauthorized"),
        "pending admin cannot act before accept, got: {}",
        err
    );

    // A non-pending caller cannot accept.
    let err = wasm
        .execute(
            &env.factory,
            &ExecuteMsg::AcceptAdmin {},
            &[],
            &env.stranger,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Unauthorized"),
        "stranger cannot accept, got: {}",
        err
    );

    // Step 2: the pending key accepts and is now admin.
    wasm.execute(&env.factory, &ExecuteMsg::AcceptAdmin {}, &[], &new_admin)
        .unwrap();
    wasm.execute(
        &env.factory,
        &ExecuteMsg::UpdateSinkCodeId {
            new_sink_code_id: 42,
        },
        &[],
        &new_admin,
    )
    .unwrap();
    let cfg: FactoryConfigResponse = wasm
        .query(&env.factory, &QueryMsg::FactoryConfig {})
        .unwrap();
    assert_eq!(cfg.admin, new_admin.address());
    assert_eq!(cfg.pending_admin, None);
    assert_eq!(cfg.sink_code_id, 42);
}

#[test]
fn settle_and_refund_rejected_on_factory_instance() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let err = wasm
        .execute(&env.factory, &ExecuteMsg::Settle {}, &[], &env.admin)
        .unwrap_err();
    assert!(
        format!("{}", err).contains("WrongRole") || format!("{}", err).contains("factory"),
        "expected wrong-role error, got: {}",
        err
    );

    let err = wasm
        .execute(&env.factory, &ExecuteMsg::Refund {}, &[], &env.admin)
        .unwrap_err();
    assert!(
        format!("{}", err).contains("WrongRole") || format!("{}", err).contains("factory"),
        "expected wrong-role error, got: {}",
        err
    );
}

// ------------------------------------------------------------------------
// Full XYK lifecycle: CreateSink -> Settle -> pair created + LP burned
// ------------------------------------------------------------------------

const TOKEN_DENOM: &str = "launch";
const PAIR_DENOM: &str = "atom";
const INJ: &str = "inj";
/// Tokenfactory denom-create fee on test-tube (10 INJ), debited by
/// `choice_factory.CreatePair` when it mints the LP denom.
const CREATE_PAIR_FEE: u128 = 10_000_000_000_000_000_000;
const SEED: u128 = 1_000_000_000;

#[test]
fn create_sink_then_settle_full_lifecycle() {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    // Accounts. `admin` owns the choice_factory + seeder factory and pre-funds
    // everything; `settler` cranks the permissionless Settle (and pays the
    // create-pair fee). Both hold all three denoms + INJ for gas.
    let big = &[
        Coin::new(1_000_000_000_000_000_000_000u128, INJ),
        Coin::new(1_000_000_000_000u128, TOKEN_DENOM),
        Coin::new(1_000_000_000_000u128, PAIR_DENOM),
    ];
    let decimals = &[18u32, 6, 6];
    let admin = app.init_account_decimals(big, decimals).unwrap();
    let settler = app.init_account_decimals(big, decimals).unwrap();
    let issuer = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();

    // Store codes.
    let pair_code_id = store(&wasm, &admin, "choice_pair.wasm");
    let factory_code_id = store(&wasm, &admin, "choice_factory.wasm");
    let auction_code_id = store(&wasm, &admin, "choice_send_to_auction.wasm");
    let seeder_code_id = store(&wasm, &admin, "choice_pool_seeder.wasm");

    // Native-only auction acts as the factory's burn_address (never invoked in
    // this test — no swaps — but keeps the pair instantiation schema honest).
    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: admin.address(),
                burn_auction_subaccount:
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
            },
            Some(&admin.address()),
            Some("Auction"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // choice_factory.
    let choice_factory = wasm
        .instantiate(
            factory_code_id,
            &ChoiceFactoryInstantiateMsg {
                pair_code_id,
                burn_address: auction_addr,
                fee_wallet_address: admin.address(),
            },
            Some(&admin.address()),
            Some("Choice Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Register decimals for both pool denoms (owner-only).
    for denom in [TOKEN_DENOM, PAIR_DENOM] {
        wasm.execute(
            &choice_factory,
            &ChoiceFactoryExecuteMsg::AddNativeTokenDecimals {
                denom: denom.to_string(),
                decimals: 6,
            },
            &[Coin::new(1u128, denom)],
            &admin,
        )
        .unwrap();
    }

    // Seeder factory pinned at the choice_factory (XYK only).
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &InstantiateMsg::Factory(FactoryInit {
                admin: admin.address(),
                sink_code_id: seeder_code_id,
                choice_factory: choice_factory.clone(),
                clmm_factory: None,
                clmm_manager: None,
            }),
            Some(&admin.address()),
            Some("Seeder Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // CreateSink (permissionless). No tip — the full balance seeds the pool.
    let res = wasm
        .execute(
            &seeder_factory,
            &ExecuteMsg::CreateSink {
                salt: b"xyk-sink-1".into(),
                sink_init: SinkInit {
                    issuer: issuer.address(),
                    token_denom: TOKEN_DENOM.to_string(),
                    pair_denom: PAIR_DENOM.to_string(),
                    token_decimals: 6,
                    pair_decimals: 6,
                    pool_kind: PoolKind::Xyk {
                        choice_factory: choice_factory.clone(),
                        lp_destination: LpDestination::Burn,
                    },
                    refund_receiver: refund_receiver.address(),
                    deadline_seconds: 3600,
                    expected_token: None,
                    expected_pair: None,
                },
            },
            &[],
            &admin,
        )
        .unwrap();
    let sink = instantiated_addr(&res);

    // Fund the sink with both seed legs (Leg B token + Leg C pair).
    bank_send(&app, &admin, &sink, TOKEN_DENOM, SEED);
    bank_send(&app, &admin, &sink, PAIR_DENOM, SEED);

    // Permissionless Settle — caller attaches exactly the create-pair fee.
    wasm.execute(
        &sink,
        &ExecuteMsg::Settle {},
        &[Coin::new(CREATE_PAIR_FEE, INJ)],
        &settler,
    )
    .unwrap();

    // The pair now exists in the factory.
    let pair_info: PairInfo = wasm
        .query(
            &choice_factory,
            &ChoiceFactoryQueryMsg::Pair {
                asset_infos: [native(TOKEN_DENOM), native(PAIR_DENOM)],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();

    // Reserves equal the full seed (no tip skimmed).
    let pool: PoolResponse = wasm
        .query(&pair_addr, &ChoicePairQueryMsg::Pool {})
        .unwrap();
    for asset in pool.assets.iter() {
        assert_eq!(
            asset.amount,
            Uint128::new(SEED),
            "reserve for {:?} should equal the seed",
            asset.info
        );
    }

    // Sink state: Settled, pair recorded, LP minted > 0.
    let state: SinkStateResponse = wasm.query(&sink, &QueryMsg::SinkState {}).unwrap();
    assert_eq!(state.status, SinkStatus::Settled);
    assert_eq!(state.pair_addr.as_deref(), Some(pair_addr.as_str()));
    assert!(
        state.lp_minted.unwrap_or_default() > Uint128::zero(),
        "expected positive LP minted"
    );

    // LP was burned (Burn destination): the sink holds none.
    assert_eq!(
        bank_balance(&app, &sink, &lp_denom),
        0,
        "sink should hold no LP after burn"
    );
    // Seed balances fully consumed into the pool.
    assert_eq!(bank_balance(&app, &sink, TOKEN_DENOM), 0, "token drained");
    assert_eq!(bank_balance(&app, &sink, PAIR_DENOM), 0, "pair drained");

    // Settle is single-shot.
    let err = wasm
        .execute(
            &sink,
            &ExecuteMsg::Settle {},
            &[Coin::new(CREATE_PAIR_FEE, INJ)],
            &settler,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Terminal") || format!("{}", err).contains("Settled"),
        "second Settle should be rejected as terminal, got: {}",
        err
    );
}

// ------------------------------------------------------------------------
// Pre-mainnet operability: factory circuit breaker + admin force-refund.
// These exercise the cross-contract paths a sink takes against its parent
// factory (pause read on Settle, admin auth on ForceRefund) which the unit
// tests can't reach with the mock querier.
// ------------------------------------------------------------------------

struct SeederEnv {
    app: InjectiveTestApp,
    seeder_factory: String,
    choice_factory: String,
    sink: String,
    admin: SigningAccount,
    stranger: SigningAccount,
    issuer: SigningAccount,
    refund_receiver: SigningAccount,
}

/// Minimal bring-up: a seeder factory + one `Pending` XYK sink created under
/// it. The pinned `choice_factory` is a plain account (these tests never reach
/// `CreatePair`), but the sink's recorded `factory` is the real seeder factory
/// contract, which answers `FactoryConfig` for the pause / force-refund checks.
fn setup_seeder_with_sink() -> SeederEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);
    let big = &[
        Coin::new(1_000_000_000_000_000_000_000u128, INJ),
        Coin::new(1_000_000_000_000u128, TOKEN_DENOM),
        Coin::new(1_000_000_000_000u128, PAIR_DENOM),
    ];
    let decimals = &[18u32, 6, 6];
    let admin = app.init_account_decimals(big, decimals).unwrap();
    let stranger = app
        .init_account(&[Coin::new(1_000_000_000_000_000_000u128, INJ)])
        .unwrap();
    let issuer = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let choice_factory_acct = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let choice_factory = choice_factory_acct.address();

    let seeder_code_id = store(&wasm, &admin, "choice_pool_seeder.wasm");
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &InstantiateMsg::Factory(FactoryInit {
                admin: admin.address(),
                sink_code_id: seeder_code_id,
                choice_factory: choice_factory.clone(),
                clmm_factory: None,
                clmm_manager: None,
            }),
            Some(&admin.address()),
            Some("Seeder Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let res = wasm
        .execute(
            &seeder_factory,
            &ExecuteMsg::CreateSink {
                salt: b"sink-ops".into(),
                sink_init: SinkInit {
                    issuer: issuer.address(),
                    token_denom: TOKEN_DENOM.to_string(),
                    pair_denom: PAIR_DENOM.to_string(),
                    token_decimals: 6,
                    pair_decimals: 6,
                    pool_kind: PoolKind::Xyk {
                        choice_factory: choice_factory.clone(),
                        lp_destination: LpDestination::Burn,
                    },
                    refund_receiver: refund_receiver.address(),
                    deadline_seconds: 3600,
                    expected_token: None,
                    expected_pair: None,
                },
            },
            &[],
            &admin,
        )
        .unwrap();
    let sink = instantiated_addr(&res);

    SeederEnv {
        app,
        seeder_factory,
        choice_factory,
        sink,
        admin,
        stranger,
        issuer,
        refund_receiver,
    }
}

#[test]
fn force_refund_by_admin_bypasses_deadline() {
    let env = setup_seeder_with_sink();
    let wasm = Wasm::new(&env.app);

    // Fund both seed legs.
    bank_send(&env.app, &env.admin, &env.sink, TOKEN_DENOM, SEED);
    bank_send(&env.app, &env.admin, &env.sink, PAIR_DENOM, SEED);

    // Deadline is 3600s out with no block-time advance — a permissionless
    // Refund is rejected.
    let err = wasm
        .execute(&env.sink, &ExecuteMsg::Refund {}, &[], &env.stranger)
        .unwrap_err();
    assert!(
        format!("{}", err).contains("deadline"),
        "pre-deadline Refund should be rejected, got: {}",
        err
    );

    // A non-admin cannot force-refund (auth is the parent factory's admin).
    let err = wasm
        .execute(&env.sink, &ExecuteMsg::ForceRefund {}, &[], &env.stranger)
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Unauthorized"),
        "stranger force-refund should be rejected, got: {}",
        err
    );

    // The factory admin force-refunds immediately, bypassing the deadline.
    wasm.execute(&env.sink, &ExecuteMsg::ForceRefund {}, &[], &env.admin)
        .unwrap();

    // Token leg → issuer, pair leg → refund_receiver; sink drained + Refunded.
    assert_eq!(
        bank_balance(&env.app, &env.issuer.address(), TOKEN_DENOM),
        SEED
    );
    assert_eq!(
        bank_balance(&env.app, &env.refund_receiver.address(), PAIR_DENOM),
        SEED
    );
    assert_eq!(bank_balance(&env.app, &env.sink, TOKEN_DENOM), 0);
    assert_eq!(bank_balance(&env.app, &env.sink, PAIR_DENOM), 0);
    let state: SinkStateResponse = wasm.query(&env.sink, &QueryMsg::SinkState {}).unwrap();
    assert_eq!(state.status, SinkStatus::Refunded);

    // Force-refund is single-shot.
    let err = wasm
        .execute(&env.sink, &ExecuteMsg::ForceRefund {}, &[], &env.admin)
        .unwrap_err();
    assert!(
        format!("{}", err).contains("Terminal") || format!("{}", err).contains("Refunded"),
        "second force-refund should be terminal, got: {}",
        err
    );
}

#[test]
fn paused_factory_blocks_create_and_settle() {
    let env = setup_seeder_with_sink();
    let wasm = Wasm::new(&env.app);

    // Pause the factory.
    wasm.execute(
        &env.seeder_factory,
        &ExecuteMsg::SetPaused { paused: true },
        &[],
        &env.admin,
    )
    .unwrap();

    // New CreateSink is refused at the factory.
    let err = wasm
        .execute(
            &env.seeder_factory,
            &ExecuteMsg::CreateSink {
                salt: b"sink-ops-2".into(),
                sink_init: SinkInit {
                    issuer: env.issuer.address(),
                    token_denom: TOKEN_DENOM.to_string(),
                    pair_denom: PAIR_DENOM.to_string(),
                    token_decimals: 6,
                    pair_decimals: 6,
                    pool_kind: PoolKind::Xyk {
                        choice_factory: env.choice_factory.clone(),
                        lp_destination: LpDestination::Burn,
                    },
                    refund_receiver: env.refund_receiver.address(),
                    deadline_seconds: 3600,
                    expected_token: None,
                    expected_pair: None,
                },
            },
            &[],
            &env.admin,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("paused"),
        "CreateSink should be refused while paused, got: {}",
        err
    );

    // The already-created sink freezes settlement — it reads the parent
    // factory's live pause flag cross-contract. (The pause check precedes the
    // balance/fee checks, so funding is unnecessary to prove the gate.)
    let err = wasm
        .execute(
            &env.sink,
            &ExecuteMsg::Settle {},
            &[Coin::new(CREATE_PAIR_FEE, INJ)],
            &env.admin,
        )
        .unwrap_err();
    assert!(
        format!("{}", err).contains("paused"),
        "Settle should be frozen by the factory pause, got: {}",
        err
    );

    // Lifting the pause clears the breaker (Settle no longer reports Paused —
    // it now fails downstream for the unrelated missing-pair reason).
    wasm.execute(
        &env.seeder_factory,
        &ExecuteMsg::SetPaused { paused: false },
        &[],
        &env.admin,
    )
    .unwrap();
    let cfg: FactoryConfigResponse = wasm
        .query(&env.seeder_factory, &QueryMsg::FactoryConfig {})
        .unwrap();
    assert!(!cfg.paused, "factory unpaused");
}

// ------------------------------------------------------------------------
// Full CLMM lifecycle: CreateLocker + CreateSink -> Settle -> pool + NFT,
// then swap volume -> locker.CollectFees -> beneficiary receives fees.
// ------------------------------------------------------------------------

const CLMM_FEE_TIER: u32 = 500;

#[test]
fn create_clmm_sink_then_settle_full_lifecycle() {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let big = &[
        Coin::new(1_000_000_000_000_000_000_000u128, INJ),
        Coin::new(1_000_000_000_000u128, TOKEN_DENOM),
        Coin::new(1_000_000_000_000u128, PAIR_DENOM),
    ];
    let decimals = &[18u32, 6, 6];
    let admin = app.init_account_decimals(big, decimals).unwrap();
    let settler = app.init_account_decimals(big, decimals).unwrap();
    let issuer = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    // Treasury + creator each start with no pair-denom so the fee-split
    // assertions are clean 0 -> positive transitions. The locker splits every
    // collected fee `creator_fee_share_bps` to the creator, the rest to the
    // treasury.
    let treasury = app
        .init_account(&[Coin::new(1_000_000_000_000_000_000u128, INJ)])
        .unwrap();
    let creator = app
        .init_account(&[Coin::new(1_000_000_000_000_000_000u128, INJ)])
        .unwrap();
    const CLMM_CREATOR_SHARE_BPS: u128 = 3000; // creator gets 30% of fees

    // token0 is the lexicographically-smaller denom; "atom" < "launch", so the
    // pair denom is token0 and a zero_for_one swap pays fees in PAIR_DENOM.
    assert!(PAIR_DENOM < TOKEN_DENOM);
    let token0 = PAIR_DENOM;

    // Store codes.
    let clmm_factory_code = store(&wasm, &admin, "choice_clmm_factory.wasm");
    let clmm_pool_code = store(&wasm, &admin, "choice_clmm_pool.wasm");
    let clmm_manager_code = store(&wasm, &admin, "choice_clmm_manager.wasm");
    let seeder_code_id = store(&wasm, &admin, "choice_pool_seeder.wasm");

    // CLMM factory + manager.
    let clmm_factory = wasm
        .instantiate(
            clmm_factory_code,
            &ClmmFactoryInstantiateMsg {
                pool_code_id: clmm_pool_code,
            },
            Some(&admin.address()),
            Some("CLMM Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let clmm_manager = wasm
        .instantiate(
            clmm_manager_code,
            &ClmmManagerInstantiateMsg {
                name: "Choice Positions".to_string(),
                symbol: "CH-POS".to_string(),
                factory_addr: clmm_factory.clone(),
            },
            Some(&admin.address()),
            Some("CLMM Manager"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Seeder factory wired for CLMM. `choice_factory` is required by the schema
    // even on a CLMM-only factory; a plain address satisfies addr_validate and
    // is never touched on the CLMM path.
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &InstantiateMsg::Factory(FactoryInit {
                admin: admin.address(),
                sink_code_id: seeder_code_id,
                choice_factory: admin.address(),
                clmm_factory: Some(clmm_factory.clone()),
                clmm_manager: Some(clmm_manager.clone()),
            }),
            Some(&admin.address()),
            Some("Seeder Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // CreateLocker first — its deterministic address is the sink's
    // position_recipient.
    let res = wasm
        .execute(
            &seeder_factory,
            &ExecuteMsg::CreateLocker {
                salt: b"clmm-locker-1".into(),
                locker_init: LockerInit {
                    manager: clmm_manager.clone(),
                    treasury: treasury.address(),
                    creator: creator.address(),
                    creator_fee_share_bps: CLMM_CREATOR_SHARE_BPS as u16,
                    admin: None,
                },
            },
            &[],
            &admin,
        )
        .unwrap();
    let locker = instantiated_addr(&res);

    // CreateSink targeting the CLMM pool, minting the position NFT to the locker.
    let res = wasm
        .execute(
            &seeder_factory,
            &ExecuteMsg::CreateSink {
                salt: b"clmm-sink-1".into(),
                sink_init: SinkInit {
                    issuer: issuer.address(),
                    token_denom: TOKEN_DENOM.to_string(),
                    pair_denom: PAIR_DENOM.to_string(),
                    token_decimals: 6,
                    pair_decimals: 6,
                    pool_kind: PoolKind::Clmm {
                        clmm_factory: clmm_factory.clone(),
                        clmm_manager: clmm_manager.clone(),
                        fee_tier: CLMM_FEE_TIER,
                        position_recipient: locker.clone(),
                        max_fee_multiple: None,
                    },
                    refund_receiver: refund_receiver.address(),
                    deadline_seconds: 3600,
                    // Factory CLMM sinks must commit their seed amounts (audit
                    // H-1/M-1); the sink is funded with SEED of each leg at 1:1.
                    expected_token: Some(Uint128::new(SEED)),
                    expected_pair: Some(Uint128::new(SEED)),
                },
            },
            &[],
            &admin,
        )
        .unwrap();
    let sink = instantiated_addr(&res);

    // Fund the sink with both legs at a 1:1 ratio.
    bank_send(&app, &admin, &sink, TOKEN_DENOM, SEED);
    bank_send(&app, &admin, &sink, PAIR_DENOM, SEED);

    let settler_pair_before = bank_balance(&app, &settler.address(), PAIR_DENOM);

    // Permissionless Settle (CLMM rejects attached funds — pool creation is free).
    wasm.execute(&sink, &ExecuteMsg::Settle {}, &[], &settler)
        .unwrap();

    // Pool now exists at the factory for (token0, token1, fee).
    let pool_addr: String = wasm
        .query(
            &clmm_factory,
            &ClmmFactoryQueryMsg::GetPool {
                token_a: clmm_native(PAIR_DENOM),
                token_b: clmm_native(TOKEN_DENOM),
                fee: CLMM_FEE_TIER,
            },
        )
        .unwrap();
    assert!(!pool_addr.is_empty(), "pool should have been created");

    // P1-B: the cranker receives no tip — its pair balance is unchanged.
    let settler_pair_after = bank_balance(&app, &settler.address(), PAIR_DENOM);
    assert_eq!(
        settler_pair_after, settler_pair_before,
        "cranker must not receive any tip"
    );

    // The locker owns exactly one position NFT.
    let tokens: TokensResp = wasm
        .query(
            &clmm_manager,
            &ClmmManagerQueryMsg::Tokens {
                owner: locker.clone(),
                start_after: None,
                limit: Some(10),
            },
        )
        .unwrap();
    assert_eq!(
        tokens.tokens.len(),
        1,
        "locker should hold one position NFT"
    );
    let token_id = tokens.tokens[0].clone();

    let owner: OwnerOfResp = wasm
        .query(
            &clmm_manager,
            &ClmmManagerQueryMsg::OwnerOf {
                token_id: token_id.clone(),
                include_expired: None,
            },
        )
        .unwrap();
    assert_eq!(owner.owner, locker, "locker must own the position NFT");

    // The position carries real liquidity.
    let pos: PositionWithFeesResponse = wasm
        .query(
            &clmm_manager,
            &ClmmManagerQueryMsg::PositionWithFees {
                token_id: token_id.clone(),
            },
        )
        .unwrap();
    assert!(
        pos.liquidity > Uint128::zero(),
        "seeded position should have liquidity"
    );

    // Seed balances drained from the sink (dust swept to refund_receiver).
    assert_eq!(bank_balance(&app, &sink, TOKEN_DENOM), 0, "token drained");
    assert_eq!(bank_balance(&app, &sink, PAIR_DENOM), 0, "pair drained");

    // --- Generate swap volume so the position accrues fees ---
    // zero_for_one pays token0 (== PAIR_DENOM), so fees accrue on the token0
    // side, which is what CollectFees will route to the beneficiary.
    let swap_amount = 100_000_000u128;
    wasm.execute(
        &pool_addr,
        &ClmmPoolExecuteMsg::Swap {
            recipient: admin.address(),
            zero_for_one: true,
            amount_specified: Uint128::new(swap_amount),
            // MIN_SQRT_RATIO + 1 — effectively no price limit downward.
            sqrt_price_limit_x96: Uint256::from(4295128739u128 + 1),
        },
        &[Coin::new(swap_amount, token0)],
        &admin,
    )
    .unwrap();

    // Collect fees (permissionless on the locker) — collected into the locker,
    // then split between treasury and creator per creator_fee_share_bps.
    let treasury_before = bank_balance(&app, &treasury.address(), token0);
    let creator_before = bank_balance(&app, &creator.address(), token0);
    wasm.execute(
        &locker,
        &ExecuteMsg::CollectFees { token_id: None },
        &[],
        &settler,
    )
    .unwrap();
    let treasury_got = bank_balance(&app, &treasury.address(), token0) - treasury_before;
    let creator_got = bank_balance(&app, &creator.address(), token0) - creator_before;

    assert!(treasury_got > 0, "treasury should receive its fee leg");
    assert!(creator_got > 0, "creator should receive its fee leg");

    // The split matches creator_fee_share_bps: creator == floor(total * share),
    // treasury == remainder. Reconstruct the total and check both legs.
    let total = treasury_got + creator_got;
    let expected_creator = total * CLMM_CREATOR_SHARE_BPS / 10_000;
    assert_eq!(creator_got, expected_creator, "creator leg = share of fee");
    assert_eq!(
        treasury_got,
        total - expected_creator,
        "treasury = remainder"
    );

    // Fees never strand in the locker.
    assert_eq!(
        bank_balance(&app, &locker, token0),
        0,
        "locker must not hold collected fees"
    );
}

// ------------------------------------------------------------------------
// Genericity: a SECOND consumer with a non-SHROOM economic model.
//
// SHROOM's v1 uses `LpDestination::Burn` (permanently locked floor). This test
// stands up the seeder for a deliberately *different* consumer — one that wants
// the graduated LP routed to its own treasury (`SendTo`) rather than burned —
// and drives a full XYK graduation with **committed** seed amounts
// (`expected_token` / `expected_pair`). It proves the seeder is dApp-agnostic:
// the only SHROOM-specific choices (Burn LP, uncommitted seed) are config, not
// code. Covers the two production-relevant paths the SHROOM-shaped lifecycle
// tests don't: `SendTo` LP routing and committed-amount seeding on XYK.
#[test]
fn second_consumer_sendto_lp_and_committed_seed_xyk() {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let big = &[
        Coin::new(1_000_000_000_000_000_000_000u128, INJ),
        Coin::new(1_000_000_000_000u128, TOKEN_DENOM),
        Coin::new(1_000_000_000_000u128, PAIR_DENOM),
    ];
    let decimals = &[18u32, 6, 6];
    let admin = app.init_account_decimals(big, decimals).unwrap();
    let settler = app.init_account_decimals(big, decimals).unwrap();
    let issuer = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();
    // The distinguishing actor: this consumer wants its graduated LP here,
    // not burned. (A different launchpad economic model — treasury-owned LP.)
    let treasury = app.init_account(&[Coin::new(1u128, INJ)]).unwrap();

    // Real XYK DEX stack.
    let pair_code_id = store(&wasm, &admin, "choice_pair.wasm");
    let factory_code_id = store(&wasm, &admin, "choice_factory.wasm");
    let auction_code_id = store(&wasm, &admin, "choice_send_to_auction.wasm");
    let seeder_code_id = store(&wasm, &admin, "choice_pool_seeder.wasm");

    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: admin.address(),
                burn_auction_subaccount:
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
            },
            Some(&admin.address()),
            Some("Auction"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let choice_factory = wasm
        .instantiate(
            factory_code_id,
            &ChoiceFactoryInstantiateMsg {
                pair_code_id,
                burn_address: auction_addr,
                fee_wallet_address: admin.address(),
            },
            Some(&admin.address()),
            Some("Choice Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    for denom in [TOKEN_DENOM, PAIR_DENOM] {
        wasm.execute(
            &choice_factory,
            &ChoiceFactoryExecuteMsg::AddNativeTokenDecimals {
                denom: denom.to_string(),
                decimals: 6,
            },
            &[Coin::new(1u128, denom)],
            &admin,
        )
        .unwrap();
    }

    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &InstantiateMsg::Factory(FactoryInit {
                admin: admin.address(),
                sink_code_id: seeder_code_id,
                choice_factory: choice_factory.clone(),
                clmm_factory: None,
                clmm_manager: None,
            }),
            Some(&admin.address()),
            Some("Seeder Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // CreateSink with the second consumer's model: LP → treasury, and the seed
    // amounts are COMMITTED (pins the opening ratio; sweeps any surplus).
    let res = wasm
        .execute(
            &seeder_factory,
            &ExecuteMsg::CreateSink {
                salt: b"second-consumer-xyk".into(),
                sink_init: SinkInit {
                    issuer: issuer.address(),
                    token_denom: TOKEN_DENOM.to_string(),
                    pair_denom: PAIR_DENOM.to_string(),
                    token_decimals: 6,
                    pair_decimals: 6,
                    pool_kind: PoolKind::Xyk {
                        choice_factory: choice_factory.clone(),
                        lp_destination: LpDestination::SendTo(treasury.address()),
                    },
                    refund_receiver: refund_receiver.address(),
                    deadline_seconds: 3600,
                    expected_token: Some(Uint128::new(SEED)),
                    expected_pair: Some(Uint128::new(SEED)),
                },
            },
            &[],
            &admin,
        )
        .unwrap();
    let sink = instantiated_addr(&res);

    // Fund both legs at exactly the committed amounts.
    bank_send(&app, &admin, &sink, TOKEN_DENOM, SEED);
    bank_send(&app, &admin, &sink, PAIR_DENOM, SEED);

    // Permissionless Settle (XYK: attach exactly the create-pair fee).
    wasm.execute(
        &sink,
        &ExecuteMsg::Settle {},
        &[Coin::new(CREATE_PAIR_FEE, INJ)],
        &settler,
    )
    .unwrap();

    let pair_info: PairInfo = wasm
        .query(
            &choice_factory,
            &ChoiceFactoryQueryMsg::Pair {
                asset_infos: [native(TOKEN_DENOM), native(PAIR_DENOM)],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();

    // Pool reserves == the committed seed, exactly.
    let pool: PoolResponse = wasm
        .query(&pair_addr, &ChoicePairQueryMsg::Pool {})
        .unwrap();
    for asset in pool.assets.iter() {
        assert_eq!(
            asset.amount,
            Uint128::new(SEED),
            "reserve for {:?} should equal the committed seed",
            asset.info
        );
    }

    let state: SinkStateResponse = wasm.query(&sink, &QueryMsg::SinkState {}).unwrap();
    assert_eq!(state.status, SinkStatus::Settled);
    let lp_minted = state.lp_minted.unwrap_or_default();
    assert!(lp_minted > Uint128::zero(), "expected positive LP minted");

    // The genericity assertion: LP landed in the consumer's TREASURY (SendTo),
    // not burned and not stranded in the sink.
    assert_eq!(
        bank_balance(&app, &treasury.address(), &lp_denom),
        lp_minted.u128(),
        "all minted LP should be routed to the consumer treasury"
    );
    assert_eq!(
        bank_balance(&app, &sink, &lp_denom),
        0,
        "sink should retain no LP after SendTo"
    );
    // Seed balances fully consumed (committed == funded, so no surplus dust).
    assert_eq!(bank_balance(&app, &sink, TOKEN_DENOM), 0, "token drained");
    assert_eq!(bank_balance(&app, &sink, PAIR_DENOM), 0, "pair drained");
}

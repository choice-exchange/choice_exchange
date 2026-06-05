#![cfg(test)]
//! Integration tests for `choice_pool_seeder` against `injective_test_tube`.
//!
//! Requires the optimised wasm artifact at `artifacts/choice_pool_seeder.wasm`
//! — run `make build-pool-seeder` (or `./build_release.sh`) before
//! `cargo test --test integration -p choice-pool-seeder`.
//!
//! ## Coverage matrix
//!
//! Currently covered:
//!  * `InstantiateMsg::Factory` store + instantiate + `Role` / `FactoryConfig`
//!    query round-trip.
//!  * Admin rotation (`UpdateAdmin` / `UpdateSinkCodeId`) auth: stranger
//!    rejected, admin succeeds, both fields persist.
//!  * Wrong-role gate on `Settle` / `Refund` against a factory instance
//!    (verifies the dispatch table).
//!
//! Deferred (marked `#[ignore]` until test-tube ships an Injective image with
//! all the moving pieces wired up):
//!  * Full `CreateSink` → Instantiate2 sink at deterministic address →
//!    `Settle` lifecycle. Requires the test-tube chain to have a working
//!    `choice_factory` deployed (so `CreatePair` succeeds) plus tokenfactory
//!    metadata for both denoms — both are achievable but heavy to wire up
//!    inside one test file. The unit tests in `src/tests.rs` cover the
//!    message-emission shape end-to-end with mocked storage; the full
//!    on-chain round trip belongs in a launchpad-side integration suite
//!    where the issuer + seeder + a real choice_factory all coexist.
//!  * `Refund` after the deadline elapses. Test-tube's clock advance is
//!    available; covered by unit tests today.

use cosmwasm_std::Coin;
use injective_test_tube::{Account, InjectiveTestApp, Module, SigningAccount, Wasm};

use choice_pool_seeder::msg::{
    ExecuteMsg, FactoryConfigResponse, FactoryInit, InstantiateMsg, QueryMsg, RoleResponse,
};

fn artifact() -> Vec<u8> {
    let path = "../../artifacts/choice_pool_seeder.wasm";
    std::fs::read(path).unwrap_or_else(|_| {
        panic!(
            "missing {}. Run `make build-pool-seeder` from choice_exchange/ first.",
            path
        )
    })
}

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
                max_tip_bps: 100,
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
            assert_eq!(cfg.max_tip_bps, 100);
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

    wasm.execute(
        &env.factory,
        &ExecuteMsg::UpdateAdmin {
            new_admin: new_admin.address(),
        },
        &[],
        &env.admin,
    )
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

#[test]
#[ignore = "needs a wired choice_factory deployment + tokenfactory metadata to drive CreatePair + ProvideLiquidity. Recheck after 2026-07-15 alongside the choice_mts_issuer integration test (or sooner if a launchpad-side cross-contract testbed harness lands). Unit tests cover the message-chain shape until then."]
fn create_sink_then_settle_full_lifecycle() {
    let env = setup();
    let _ = env;
    // Intended scope when re-enabled:
    //  1. Deploy choice_factory + a wasm pair code-id into the test-tube chain.
    //  2. Pre-register both denoms (token + pair) via AddNativeTokenDecimals.
    //  3. Issue a tokenfactory `token_denom` and fund the factory's
    //     instantiator with both denoms.
    //  4. `CreateSink` with a fixed salt; assert sink lives at the predicted
    //     instantiate2 address.
    //  5. Bank-send token + pair + INJ (create-pair fee) to the sink.
    //  6. Permissionless `Settle`; assert tip lands, pair gets created,
    //     liquidity provided, LP burned (default LpDestination).
    unimplemented!("re-enable when a wired choice_factory deployment fits inside one test file");
}

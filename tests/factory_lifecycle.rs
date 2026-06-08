#![cfg(test)]
//! Item 5 (residual-risk): factory lifecycle on the REAL VM (test-tube).
//!
//! The factory's `src/test.rs` mock suite is thorough, but a `mock_dependencies`
//! backend cannot exercise the one property that only wasmd provides: that the
//! address the pool is actually instantiated at (via `Instantiate2` with the
//! salt = sha256(key0‖key1‖fee)) is the SAME address the factory stores and
//! serves from `GetPool`. (Injective truncates instantiate2 addresses to 20
//! bytes — "instantiate2 = 20B-trunc" — so re-deriving with the stock cosmwasm
//! helper would mismatch; we instead assert the on-chain instantiated address
//! equals the stored one.) This suite also re-proves, end-to-end on real
//! reverts: duplicate-creation rejection, owner-gating of the admin entrypoints,
//! and the anti-squat `POOL_CREATION_AUTH` reservation gate.

use cosmwasm_std::Uint256;
use injective_test_tube::{
    injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse, Account, ExecuteResponse,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_clmm_common::factory::{
    ConfigResponse, CreationAuthResponse, ExecuteMsg as FactoryExecuteMsg,
    InstantiateMsg as FactoryInstantiateMsg, QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::pool::QueryMsg as PoolQueryMsg;
use choice_clmm_common::types::AssetInfo;
use cosmwasm_std::Coin;

const ATOM: &str = "atom";
const USDT: &str = "usdt";
const PRICE_ONE: u128 = 79_228_162_514_264_337_593_543_950_336; // 2^96

type ExecResp = ExecuteResponse<MsgExecuteContractResponse>;

fn native(d: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: d.to_string(),
    }
}
fn wasm_bytes(f: &str) -> Vec<u8> {
    std::fs::read(format!("../../artifacts/{f}"))
        .unwrap_or_else(|_| panic!("missing artifact {}", f))
}

/// Pull the address the pool was actually instantiated at from the create tx's
/// `instantiate` event (wasmd's `_contract_address`).
fn instantiated_addr(res: &ExecResp) -> String {
    res.events
        .iter()
        .find(|e| e.ty == "instantiate")
        .and_then(|e| e.attributes.iter().find(|a| a.key == "_contract_address"))
        .map(|a| a.value.clone())
        .expect("create tx must emit an instantiate event with _contract_address")
}

struct Env {
    app: InjectiveTestApp,
    admin: SigningAccount,
    user_x: SigningAccount,
    user_y: SigningAccount,
    factory: String,
    pool_code: u64,
}

fn setup() -> Env {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);
    let admin = app
        .init_account_decimals(
            &[
                Coin::new(1_000_000_000_000_000_000_000_000_000_000u128, "inj"),
                Coin::new(1_000_000_000_000_000_000u128, USDT),
                Coin::new(1_000_000_000_000_000_000u128, ATOM),
            ],
            &[18, 6, 6],
        )
        .unwrap();
    let mk = || {
        app.init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000_000_000u128, USDT),
            Coin::new(1_000_000_000_000_000_000u128, ATOM),
        ])
        .unwrap()
    };
    let user_x = mk();
    let user_y = mk();

    let factory_code = wasm
        .store_code(&wasm_bytes("choice_clmm_factory.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let pool_code = wasm
        .store_code(&wasm_bytes("choice_clmm_pool.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let factory = wasm
        .instantiate(
            factory_code,
            &FactoryInstantiateMsg {
                pool_code_id: pool_code,
            },
            Some(&admin.address()),
            Some("F"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    Env {
        app,
        admin,
        user_x,
        user_y,
        factory,
        pool_code,
    }
}

fn create_pool(env: &Env, who: &SigningAccount, fee: u32) -> Result<ExecResp, String> {
    Wasm::new(&env.app)
        .execute(
            &env.factory,
            &FactoryExecuteMsg::CreatePool {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee,
                init_sqrt_price: Uint256::from(PRICE_ONE),
            },
            &[],
            who,
        )
        .map_err(|e| e.to_string())
}

fn get_pool(env: &Env, fee: u32) -> String {
    Wasm::new(&env.app)
        .query(
            &env.factory,
            &FactoryQueryMsg::GetPool {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee,
            },
        )
        .unwrap()
}

/// Minimal view of pool config to assert canonical token order + tick_spacing.
#[derive(serde::Deserialize)]
struct CfgPeek {
    token0: AssetInfo,
    token1: AssetInfo,
    tick_spacing: u32,
}

// ===========================================================================
// 1. The instantiated pool address == the address stored & served by the
//    factory, and the pool is configured with the canonical token order and the
//    fee tier's tick_spacing.
// ===========================================================================
#[test]
fn create_pool_address_matches_stored_and_config_is_canonical() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let res = create_pool(&env, &env.admin, 500).unwrap();
    let on_chain = instantiated_addr(&res);
    let stored = get_pool(&env, 500);
    assert_eq!(
        on_chain, stored,
        "factory stored {} but instantiated {}",
        stored, on_chain
    );

    // The stored address is a live pool whose config matches the factory's intent.
    let cfg: CfgPeek = wasm.query(&stored, &PoolQueryMsg::GetConfig {}).unwrap();
    assert_eq!(cfg.tick_spacing, 10, "fee 500 must map to tick_spacing 10");
    // Canonical ordering: token0 < token1 (AssetInfo ordering). "atom" < "usdt".
    assert!(
        cfg.token0 < cfg.token1,
        "tokens not stored in canonical order"
    );
    assert_eq!(cfg.token0, native(ATOM));
    assert_eq!(cfg.token1, native(USDT));

    // A different fee tier yields a DIFFERENT deterministic address.
    let res2 = create_pool(&env, &env.admin, 3000).unwrap();
    let addr2 = instantiated_addr(&res2);
    assert_ne!(
        addr2, on_chain,
        "distinct fee tiers must instantiate distinct pools"
    );
    assert_eq!(addr2, get_pool(&env, 3000));
    let cfg2: CfgPeek = wasm.query(&addr2, &PoolQueryMsg::GetConfig {}).unwrap();
    assert_eq!(
        cfg2.tick_spacing, 60,
        "fee 3000 must map to tick_spacing 60"
    );
}

// ===========================================================================
// 2. Duplicate creation of the same (pair, fee) reverts.
// ===========================================================================
#[test]
fn duplicate_create_reverts() {
    let env = setup();
    create_pool(&env, &env.admin, 500).unwrap();
    let err = create_pool(&env, &env.admin, 500).unwrap_err();
    assert!(
        err.contains("Pool already exists"),
        "wrong duplicate error: {}",
        err
    );
}

// ===========================================================================
// 3. Admin entrypoints are owner-gated on the real VM (non-owner reverts; owner
//    succeeds and the new fee tier becomes usable).
// ===========================================================================
#[test]
fn admin_entrypoints_are_owner_gated() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Non-owner is rejected on every admin entrypoint.
    let e1 = wasm
        .execute(
            &env.factory,
            &FactoryExecuteMsg::EnableFeeAmount {
                fee: 2500,
                tick_spacing: 50,
            },
            &[],
            &env.user_x,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e1.contains("Unauthorized"),
        "EnableFeeAmount not owner-gated: {}",
        e1
    );
    let e2 = wasm
        .execute(
            &env.factory,
            &FactoryExecuteMsg::UpdateConfig {},
            &[],
            &env.user_x,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e2.contains("Unauthorized"),
        "UpdateConfig not owner-gated: {}",
        e2
    );
    let e3 = wasm
        .execute(
            &env.factory,
            &FactoryExecuteMsg::ProposePoolCodeId {
                code_id: env.pool_code,
            },
            &[],
            &env.user_x,
        )
        .unwrap_err()
        .to_string();
    assert!(
        e3.contains("Unauthorized"),
        "ProposePoolCodeId not owner-gated: {}",
        e3
    );

    // Owner enables a new tier; it becomes immediately usable.
    wasm.execute(
        &env.factory,
        &FactoryExecuteMsg::EnableFeeAmount {
            fee: 2500,
            tick_spacing: 50,
        },
        &[],
        &env.admin,
    )
    .unwrap();
    // Re-enabling the SAME fee must be rejected (no silent overwrite).
    let dup = wasm
        .execute(
            &env.factory,
            &FactoryExecuteMsg::EnableFeeAmount {
                fee: 2500,
                tick_spacing: 99,
            },
            &[],
            &env.admin,
        )
        .unwrap_err()
        .to_string();
    assert!(
        dup.contains("already enabled"),
        "fee-tier overwrite not rejected: {}",
        dup
    );

    create_pool(&env, &env.admin, 2500).unwrap();
    let cfg: CfgPeek = wasm
        .query(&get_pool(&env, 2500), &PoolQueryMsg::GetConfig {})
        .unwrap();
    assert_eq!(
        cfg.tick_spacing, 50,
        "new fee tier did not apply its tick_spacing"
    );

    // Owner-gating is verified above; the two-step owner handoff itself is
    // covered exhaustively by the factory mock suite.
    let _: ConfigResponse = wasm
        .query(&env.factory, &FactoryQueryMsg::GetConfig {})
        .unwrap();
}

// ===========================================================================
// 4. Anti-squat gate: a reserved slot blocks every creator but the authorized
//    one, the authorized create consumes the reservation, and the slot then
//    behaves like any created pool (duplicate rejected).
// ===========================================================================
#[test]
fn antisquat_gate_blocks_then_allows_then_consumes() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // Factory owner reserves the (ATOM, USDT, 500) slot for user_x (finite ttl).
    wasm.execute(
        &env.factory,
        &FactoryExecuteMsg::AuthorizeCreation {
            token_a: native(ATOM),
            token_b: native(USDT),
            fee: 500,
            creator: env.user_x.address(),
            ttl_seconds: 100_000,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // The reservation is observable and names user_x.
    let auth: Option<CreationAuthResponse> = wasm
        .query(
            &env.factory,
            &FactoryQueryMsg::GetCreationAuth {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee: 500,
            },
        )
        .unwrap();
    assert_eq!(
        auth.expect("reservation must exist").creator,
        env.user_x.address()
    );

    // user_y (unauthorized) is blocked.
    let blocked = create_pool(&env, &env.user_y, 500).unwrap_err();
    assert!(
        blocked.contains("reserved for another creator"),
        "gate did not block unauthorized creator: {}",
        blocked
    );

    // user_x (authorized) succeeds and consumes the reservation.
    create_pool(&env, &env.user_x, 500).unwrap();
    let auth_after: Option<CreationAuthResponse> = wasm
        .query(
            &env.factory,
            &FactoryQueryMsg::GetCreationAuth {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee: 500,
            },
        )
        .unwrap();
    assert!(
        auth_after.is_none(),
        "reservation not consumed by the authorized create"
    );

    // The slot now hosts a pool: any further create (even user_x) is a duplicate.
    let dup = create_pool(&env, &env.user_x, 500).unwrap_err();
    assert!(
        dup.contains("Pool already exists"),
        "post-create duplicate not rejected: {}",
        dup
    );
}

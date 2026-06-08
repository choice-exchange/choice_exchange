//! CLMM graduation variant of the `choice_mts_issuer` lifecycle against the
//! real bundled chain, driving the actual compiled wasm of `choice_mts_issuer`,
//! `choice_pool_seeder`, and `choice_clmm_factory`.
//!
//! Difference vs the XYK test: the launch is a CLMM graduation, so
//!   * the seeder Factory is configured with `clmm_factory` + `clmm_manager`,
//!   * the sink's `pool_kind` is `Clmm { fee_tier, position_recipient, … }`,
//!   * `RegisterLaunch` carries a `clmm_pool_auth`, which makes the issuer emit
//!     `AuthorizeCreation` at a REAL `choice_clmm_factory` — reserving the
//!     `(launch_denom, pair_denom, fee)` pool slot for the sink (anti-squat).
//!
//! `Settle` is not exercised (it needs a real CLMM manager + pool + funded
//! pair-asset leg), so the CLMM manager and the position recipient are dummy
//! valid addresses; the factory is the only extra real contract required.

use chain_capability_harness::{
    artifact, bank_balance, canon20, custom_gas, exec_tolerant, instantiate2_addr, issuer_salt, FUND,
};
use cosmwasm_std::{Binary, Coin};
use injective_std::types::injective::tokenfactory::v1beta1::QueryParamsRequest;
use injective_test_tube::{Account, Bank, InjectiveTestApp, Module, TokenFactory, Wasm};
use serde_json::{json, Value};

const INTERNAL_ID: u64 = 1;
const FEE_TIER: u32 = 3000; // 0.30% — pre-enabled by the factory at instantiate

#[test]
fn issuer_clmm_graduation_register_then_deliver() {
    let app = InjectiveTestApp::new();
    let admin = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    // Dummy CLMM manager + position recipient — string-validated only (Settle
    // isn't run), so any valid bech32 works.
    let clmm_manager = app.init_account(&[Coin::new(1u128, "inj")]).unwrap().address();
    let position_recipient = app.init_account(&[Coin::new(1u128, "inj")]).unwrap().address();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let tf = TokenFactory::new(&app);

    // --- store contracts --------------------------------------------------
    let seeder_store = wasm
        .store_code(&artifact("choice_pool_seeder"), None, &admin)
        .unwrap();
    let seeder_code_id = seeder_store.data.code_id;
    let seeder_checksum = seeder_store.data.checksum.clone();
    let issuer_code_id = wasm
        .store_code(&artifact("choice_mts_issuer"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let clmm_factory_code_id = wasm
        .store_code(&artifact("choice_clmm_factory"), None, &admin)
        .unwrap()
        .data
        .code_id;
    // pool_code_id is stored by the factory but only used at CreatePool (not
    // exercised); a real CLMM pool code-id keeps it honest.
    let pool_code_id = wasm
        .store_code(&artifact("choice_clmm_pool"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // --- deploy a REAL CLMM factory (owner = admin) -----------------------
    let clmm_factory = wasm
        .instantiate(
            clmm_factory_code_id,
            &json!({ "pool_code_id": pool_code_id }),
            Some(&admin.address()),
            Some("clmm-factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- seeder factory configured for CLMM -------------------------------
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &json!({ "factory": {
                "admin": admin.address(),
                "sink_code_id": seeder_code_id,
                "choice_factory": admin.address(), // unused on the CLMM path
                "clmm_factory": clmm_factory,
                "clmm_manager": clmm_manager,
                "max_tip_bps": 1000u16,
            }}),
            Some(&admin.address()),
            Some("seeder-factory-clmm"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- issuer -----------------------------------------------------------
    let issuer = wasm
        .instantiate(
            issuer_code_id,
            &json!({
                "admin": admin.address(),
                "subdenom_prefix": "shroom",
                "decimals": 18u32,
                "keeper": keeper.address(),
                "forwarder": forwarder.address(),
                "refund_deadline_seconds": 86_400u64,
            }),
            Some(&admin.address()),
            Some("mts-issuer-clmm"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- keeper-side derivation -------------------------------------------
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let seeder_addr = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    let total: u128 = 1_000_000_000_000_000_000_000_000_000; // 1e27
    let evm: u128 = 800_000_000_000_000_000_000_000_000; //     8e26
    let cw_held = total - evm; //                                2e26
    let leftover: u128 = 50_000_000_000_000_000_000_000_000; //  5e25

    // --- CreateSink payload: Clmm pool_kind -------------------------------
    let sink_init = json!({
        "issuer": issuer,
        "token_denom": denom,
        "pair_denom": "inj",
        "token_decimals": 18u8,
        "pair_decimals": 18u8,
        "pool_kind": { "clmm": {
            "clmm_factory": clmm_factory,
            "clmm_manager": clmm_manager,
            "fee_tier": FEE_TIER,
            "position_recipient": position_recipient,
        }},
        "refund_receiver": refund_receiver.address(),
        "deadline_seconds": 86_400u64,
        "tip_bps": 0u16,
    });
    let create_sink_msg = json!({ "create_sink": {
        "salt": Binary::new(salt.clone()).to_base64(),
        "sink_init": sink_init,
    }});
    let create_sink_payload =
        Binary::new(serde_json::to_vec(&create_sink_msg).unwrap()).to_base64();

    let fee_coins: Vec<Coin> = tf
        .query_params(&QueryParamsRequest {})
        .unwrap()
        .params
        .map(|p| p.denom_creation_fee)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Coin::new(c.amount.parse::<u128>().unwrap(), c.denom))
        .collect();

    // ===================== 1. RegisterLaunch (CLMM) =======================
    let keeper = custom_gas(keeper);
    let register = json!({ "register_launch": {
        "internal_id": INTERNAL_ID,
        "evm_authority": evm_authority.address(),
        "total_supply": total.to_string(),
        "evm_supply": evm.to_string(),
        "pair_denom": "inj",
        "seeder_factory": seeder_factory,
        "seeder_addr": seeder_addr,
        "create_sink_payload": create_sink_payload,
        "choice_factory": null,
        "salt_suffix": null,
        "clmm_pool_auth": { "clmm_factory": clmm_factory, "fee": FEE_TIER, "ttl_seconds": 0u64 },
    }});
    exec_tolerant(|| {
        let _ = wasm.execute(&issuer, &register, &fee_coins, &keeper);
    });

    // --- assert RegisterLaunch landed -------------------------------------
    let rec: Value = wasm
        .query(
            &issuer,
            &json!({ "launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID }}),
        )
        .expect("Launch query failed — CLMM RegisterLaunch did not commit");
    assert_eq!(rec["status"], "registered", "record: {rec}");
    assert_eq!(rec["denom"], denom);
    assert_eq!(rec["cw_held"], cw_held.to_string());
    let erc20_addr = rec["erc20_address"].as_str().expect("erc20 not captured");
    assert!(erc20_addr.starts_with("0x") && erc20_addr.len() == 42);
    println!("CLMM RegisterLaunch OK: denom={denom}\n  erc20={erc20_addr}\n  sink={seeder_addr}");

    assert_eq!(bank_balance(&bank, &evm_authority.address(), &denom), evm.to_string());
    assert_eq!(bank_balance(&bank, &issuer, &denom), cw_held.to_string());

    // the sink exists at the computed address AND is a CLMM-configured sink
    let role: Value = wasm
        .query(&seeder_addr, &json!({ "role": {} }))
        .expect("sink not found at computed instantiate2 address");
    let sink_cfg = role
        .get("sink")
        .and_then(|s| s.get("config"))
        .expect("expected a Sink role");
    assert!(
        sink_cfg.to_string().contains("clmm"),
        "sink not configured for CLMM: {sink_cfg}"
    );

    // THE anti-squat assertion: the issuer reserved the CLMM pool slot for the
    // sink at the real factory.
    let auth: Value = wasm
        .query(
            &clmm_factory,
            &json!({ "get_creation_auth": {
                "token_a": { "native_token": { "denom": denom } },
                "token_b": { "native_token": { "denom": "inj" } },
                "fee": FEE_TIER,
            }}),
        )
        .expect("GetCreationAuth query failed");
    assert!(!auth.is_null(), "no creation auth reserved on the CLMM factory");
    assert_eq!(
        auth["creator"], seeder_addr,
        "pool slot reserved for the wrong creator: {auth}"
    );
    println!("CLMM pool slot reserved for sink {seeder_addr} (anti-squat AuthorizeCreation OK)");

    // ===================== 2. DeliverToSeeder =============================
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &json!({ "deliver_to_seeder": {
                "evm_authority": evm_authority.address(),
                "internal_id": INTERNAL_ID,
                "leftover": leftover.to_string(),
            }}),
            &[],
            &keeper,
        );
    });

    let rec: Value = wasm
        .query(
            &issuer,
            &json!({ "launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID }}),
        )
        .unwrap();
    assert_eq!(rec["status"], "delivered", "DeliverToSeeder did not finalize: {rec}");
    assert_eq!(bank_balance(&bank, &seeder_addr, &denom), cw_held.to_string());
    assert_eq!(bank_balance(&bank, &issuer, &denom), "0");
    // DeliverToSeeder admin-burns the authority's ACTUAL launch-denom balance
    // (capped at evm_supply), not the relayed `leftover`. No curve trading here,
    // so the authority held the full evm_supply — all unsold, all burned → 0.
    assert_eq!(
        bank_balance(&bank, &evm_authority.address(), &denom),
        "0"
    );
    println!("CLMM DeliverToSeeder OK: delivered cw_held={cw_held} to sink, authority fully burned");
}

//! Full `choice_mts_issuer` lifecycle against the real bundled chain
//! (`injective-test-tube 1.19.0` → `injective-core v1.19.0`), driving the
//! *actual compiled wasm* for both `choice_mts_issuer` and
//! `choice_pool_seeder` — no compile-time dependency on either contract crate
//! (messages are JSON), so the cw-std 2.2.2 workspace is untouched.
//!
//! Flow (XYK graduation path, `choice_factory = None` to skip the optional
//! AddNativeTokenDecimals hop — no real XYK factory needed):
//!   1. store + instantiate `choice_pool_seeder` as a Factory.
//!   2. store + instantiate `choice_mts_issuer`.
//!   3. keeper `RegisterLaunch` — one atomic tx that creates the denom, mints
//!      total_supply, pairs it (auto-deploys the ERC20), forwards `CreateSink`
//!      to the seeder factory (Instantiate2 → real sink), and ships evm_supply
//!      to the EVM authority. Assert via queries.
//!   4. keeper `DeliverToSeeder` — admin-burn `leftover` from the EVM
//!      authority + bank-send `cw_held` to the sink. Assert via queries.
//!
//! The `seeder_addr` the issuer stores must equal the Instantiate2 address the
//! forwarded `CreateSink` produces, or `DeliverToSeeder`'s C-M3 contract-code
//! check fails — so we replicate the keeper's Injective-specific instantiate2
//! derivation (20-byte truncation) here and the test's success is itself proof
//! the derivation is correct.

use chain_capability_harness::{
    artifact, bank_balance, canon20, custom_gas, exec_tolerant, instantiate2_addr, issuer_salt, FUND,
};
use cosmwasm_std::{Binary, Coin};
use injective_std::types::injective::erc20::v1beta1::QueryTokenPairByDenomRequest;
use injective_std::types::injective::tokenfactory::v1beta1::QueryParamsRequest;
use injective_test_tube::{Account, Bank, Erc20, InjectiveTestApp, Module, TokenFactory, Wasm};
use serde_json::{json, Value};

const INTERNAL_ID: u64 = 1;

#[test]
fn issuer_full_lifecycle_register_then_deliver() {
    let app = InjectiveTestApp::new();
    let admin = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let erc20 = Erc20::new(&app);
    let tf = TokenFactory::new(&app);

    // --- store both contracts ---------------------------------------------
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

    // --- instantiate the seeder factory -----------------------------------
    // `choice_factory` here is a stored/compared string only (CreateSink never
    // queries it for an XYK sink), so any valid bech32 works — we never deploy
    // a real XYK factory.
    let dummy_choice_factory = admin.address();
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &json!({ "factory": {
                "admin": admin.address(),
                "sink_code_id": seeder_code_id,
                "choice_factory": dummy_choice_factory,
                "clmm_factory": null,
                "clmm_manager": null,
                "max_tip_bps": 1000u16,
            }}),
            Some(&admin.address()),
            Some("seeder-factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- instantiate the issuer -------------------------------------------
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
            Some("mts-issuer"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- derive the launch denom + sink address (keeper-side math) ---------
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let seeder_addr = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    // --- supplies ---------------------------------------------------------
    let total: u128 = 1_000_000_000_000_000_000_000_000_000; // 1e27
    let evm: u128 = 800_000_000_000_000_000_000_000_000; //     8e26
    let cw_held = total - evm; //                                2e26
    let leftover: u128 = 50_000_000_000_000_000_000_000_000; //  5e25

    // --- build the opaque CreateSink payload (XYK / Burn LP) --------------
    let sink_init = json!({
        "issuer": issuer,
        "token_denom": denom,
        "pair_denom": "inj",
        "token_decimals": 18u8,
        "pair_decimals": 18u8,
        "pool_kind": { "xyk": {
            "choice_factory": dummy_choice_factory,
            "lp_destination": "burn",
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

    // --- denom-creation fee the keeper must attach EXACTLY -----------------
    let fee_coins: Vec<Coin> = tf
        .query_params(&QueryParamsRequest {})
        .unwrap()
        .params
        .map(|p| p.denom_creation_fee)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Coin::new(c.amount.parse::<u128>().unwrap(), c.denom))
        .collect();
    println!("tokenfactory denom_creation_fee = {fee_coins:?}");

    // ===================== 1. RegisterLaunch ==============================
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
        "clmm_pool_auth": null,
    }});
    exec_tolerant(|| {
        // Atomic; emits EVM deploy events → tolerate the decode panic.
        let _ = wasm.execute(&issuer, &register, &fee_coins, &keeper);
    });

    // --- assert RegisterLaunch landed -------------------------------------
    let rec: Value = wasm
        .query(
            &issuer,
            &json!({ "launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID }}),
        )
        .expect("Launch query failed — RegisterLaunch did not commit");
    assert_eq!(rec["status"], "registered", "record: {rec}");
    assert_eq!(rec["denom"], denom);
    assert_eq!(rec["cw_held"], cw_held.to_string());
    assert_eq!(rec["seeder_addr"], seeder_addr);
    let erc20_addr = rec["erc20_address"].as_str().expect("erc20_address not captured in reply");
    assert!(
        erc20_addr.starts_with("0x") && erc20_addr.len() == 42,
        "bad erc20 addr {erc20_addr}"
    );
    println!("RegisterLaunch OK: denom={denom}\n  erc20={erc20_addr}\n  sink={seeder_addr}");

    // bank balances: evm_authority got evm_supply, issuer retains cw_held
    assert_eq!(bank_balance(&bank, &evm_authority.address(), &denom), evm.to_string());
    assert_eq!(bank_balance(&bank, &issuer, &denom), cw_held.to_string());

    // erc20 module persisted the pair
    let pair = erc20
        .query_token_pair_by_denom(&QueryTokenPairByDenomRequest { bank_denom: denom.clone() })
        .unwrap()
        .token_pair
        .expect("no token pair");
    assert_eq!(pair.erc20_address, erc20_addr);

    // the sink contract really exists at our computed instantiate2 address
    // (proves the derivation) — query its Role.
    let role: Value = wasm
        .query(&seeder_addr, &json!({ "role": {} }))
        .expect("sink not found at computed instantiate2 address");
    assert!(role.get("sink").is_some(), "expected a Sink role, got {role}");
    println!("sink Role query OK at computed address (instantiate2 derivation correct)");

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

    // Leg B: cw_held delivered to the sink; issuer drained.
    assert_eq!(bank_balance(&bank, &seeder_addr, &denom), cw_held.to_string());
    assert_eq!(bank_balance(&bank, &issuer, &denom), "0");

    // Leg A: the issuer admin-burns the EVM authority's ACTUAL launch-denom
    // balance (capped at evm_supply), NOT the keeper-relayed `leftover` — a
    // hardening so a mis-reported leftover can't strand unsold supply. This
    // harness does no EVM-side curve trading, so the authority still holds the
    // full evm_supply; all of it is unsold and burned, draining it to 0. (Proves
    // allow_admin_burn + admin burn-from works on injective-core v1.19.0.)
    assert_eq!(
        bank_balance(&bank, &evm_authority.address(), &denom),
        "0",
        "admin burn-from should drain the authority's unsold balance"
    );

    println!(
        "DeliverToSeeder OK: status=delivered, sink got cw_held={cw_held}, \
         evm_authority fully burned (now 0)"
    );
}

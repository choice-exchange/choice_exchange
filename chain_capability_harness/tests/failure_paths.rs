//! Failure-terminal coverage for the graduation pipeline against the real
//! bundled chain: the CW-side cleanup paths a launch hits when it never
//! graduates.
//!
//!   * `issuer_refund_failed_launch` — a launch stuck in `Registered` (e.g. the
//!     EVM curve never filled / bootstrap aborted). Exercises the keeper/deadline
//!     gate and `RefundFailedLaunch` burning `cw_held` AND admin-burning the
//!     authority's unsold `evm_supply` (status → Refunded) so no dangling
//!     launch-denom supply lingers on either side.
//!   * `sink_refund_returns_legs` — a sink that was funded but never settled
//!     (e.g. `Settle` reverted). Exercises the deadline gate and `Refund`
//!     routing the token side back to the issuer and the pair side to
//!     `refund_receiver`.
//!
//! The launch denom is ERC20-paired, so burns/transfers of it mirror to EVM and
//! emit non-UTF-8 events → successful mutating calls are wrapped in
//! `exec_tolerant` + verified by query; reverting calls return cleanly (the
//! revert precedes any event emission) so the gate assertions read the error.

use chain_capability_harness::{
    artifact, bank_balance, bank_send, canon20, custom_gas, exec_tolerant, instantiate2_addr,
    issuer_salt, FUND,
};
use cosmwasm_std::{Binary, Coin};
use injective_std::types::injective::tokenfactory::v1beta1::QueryParamsRequest;
use injective_test_tube::{Account, Bank, InjectiveTestApp, Module, TokenFactory, Wasm};
use serde_json::{json, Value};

const INTERNAL_ID: u64 = 1;
const PAIR: &str = "upair";

fn fee_coins(tf: &TokenFactory<InjectiveTestApp>) -> Vec<Coin> {
    tf.query_params(&QueryParamsRequest {})
        .unwrap()
        .params
        .map(|p| p.denom_creation_fee)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Coin::new(c.amount.parse::<u128>().unwrap(), c.denom))
        .collect()
}

/// Common setup: a CLMM-agnostic XYK-style seeder factory + issuer. Returns
/// (issuer, seeder_factory, seeder_checksum, dummy_choice_factory).
fn deploy_stack(
    wasm: &Wasm<InjectiveTestApp>,
    admin: &injective_test_tube::SigningAccount,
    keeper_addr: &str,
    forwarder_addr: &str,
) -> (String, String, Vec<u8>) {
    let seeder_store = wasm.store_code(&artifact("choice_pool_seeder"), None, admin).unwrap();
    let seeder_code_id = seeder_store.data.code_id;
    let seeder_checksum = seeder_store.data.checksum.clone();
    let issuer_code_id = wasm
        .store_code(&artifact("choice_mts_issuer"), None, admin)
        .unwrap()
        .data
        .code_id;

    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &json!({ "factory": {
                "admin": admin.address(),
                "sink_code_id": seeder_code_id,
                "choice_factory": admin.address(),
                "clmm_factory": null,
                "clmm_manager": null,
                "max_tip_bps": 1000u16,
            }}),
            Some(&admin.address()),
            Some("seeder-factory"),
            &[],
            admin,
        )
        .unwrap()
        .data
        .address;
    let issuer = wasm
        .instantiate(
            issuer_code_id,
            &json!({
                "admin": admin.address(),
                "subdenom_prefix": "shroom",
                "decimals": 18u32,
                "keeper": keeper_addr,
                "forwarder": forwarder_addr,
                "refund_deadline_seconds": 86_400u64,
            }),
            Some(&admin.address()),
            Some("mts-issuer"),
            &[],
            admin,
        )
        .unwrap()
        .data
        .address;
    (issuer, seeder_factory, seeder_checksum)
}

/// Build the XYK CreateSink payload + RegisterLaunch JSON for a launch whose
/// sink targets `dummy_choice_factory` (never settled in these tests).
fn register_msg(
    issuer: &str,
    seeder_factory: &str,
    sink: &str,
    evm_authority: &str,
    refund_receiver: &str,
    dummy_choice_factory: &str,
    denom: &str,
    salt: &[u8],
    total: u128,
    evm: u128,
    sink_deadline: u64,
) -> Value {
    let sink_init = json!({
        "issuer": issuer,
        "token_denom": denom,
        "pair_denom": PAIR,
        "token_decimals": 18u8,
        "pair_decimals": 18u8,
        "pool_kind": { "xyk": { "choice_factory": dummy_choice_factory, "lp_destination": "burn" }},
        "refund_receiver": refund_receiver,
        "deadline_seconds": sink_deadline,
        "tip_bps": 0u16,
    });
    let create_sink_payload = Binary::new(
        serde_json::to_vec(&json!({ "create_sink": {
            "salt": Binary::new(salt.to_vec()).to_base64(),
            "sink_init": sink_init,
        }}))
        .unwrap(),
    )
    .to_base64();
    json!({ "register_launch": {
        "internal_id": INTERNAL_ID,
        "evm_authority": evm_authority,
        "total_supply": total.to_string(),
        "evm_supply": evm.to_string(),
        "pair_denom": PAIR,
        "seeder_factory": seeder_factory,
        "seeder_addr": sink,
        "create_sink_payload": create_sink_payload,
        "choice_factory": null,
        "salt_suffix": null,
        "clmm_pool_auth": null,
    }})
}

#[test]
fn issuer_refund_failed_launch() {
    let app = InjectiveTestApp::new();
    let admin = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let stranger = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let tf = TokenFactory::new(&app);

    let (issuer, seeder_factory, seeder_checksum) =
        deploy_stack(&wasm, &admin, &keeper.address(), &forwarder.address());
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let sink = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    let total: u128 = 5_000_000_000_000_000_000;
    let evm: u128 = 4_000_000_000_000_000_000;
    let cw_held = total - evm; // 1e18 held by the issuer

    let keeper = custom_gas(keeper);
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &register_msg(
                &issuer, &seeder_factory, &sink, &evm_authority.address(),
                &refund_receiver.address(), &admin.address(), &denom, &salt, total, evm, 86_400,
            ),
            &fee_coins(&tf),
            &keeper,
        );
    });
    assert_eq!(bank_balance(&bank, &issuer, &denom), cw_held.to_string(), "issuer holds cw_held");

    // Gate: a non-keeper before the refund deadline is rejected.
    let err = format!(
        "{:?}",
        wasm.execute(
            &issuer,
            &json!({ "refund_failed_launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID, "reason": "too early" }}),
            &[],
            &stranger,
        )
        .unwrap_err()
    );
    assert!(err.contains("Refund deadline not yet reached"), "expected deadline gate, got: {err}");

    // Keeper may refund anytime → burns cw_held, status Refunded.
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &json!({ "refund_failed_launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID, "reason": "curve never filled" }}),
            &[],
            &keeper,
        );
    });

    let rec: Value = wasm
        .query(&issuer, &json!({ "launch": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID }}))
        .unwrap();
    assert_eq!(rec["status"], "refunded", "record: {rec}");
    assert_eq!(bank_balance(&bank, &issuer, &denom), "0", "cw_held burned");
    // The refund now ALSO admin-burns the EVM authority's unsold launch-denom
    // balance (capped at evm_supply), so no dangling supply lingers on either
    // side. This harness does no curve trading, so the authority held the full
    // evm_supply; all of it is burned, draining it to 0.
    assert_eq!(bank_balance(&bank, &evm_authority.address(), &denom), "0", "evm_supply admin-burned");
    println!("issuer RefundFailedLaunch OK: cw_held {cw_held} burned, evm_supply admin-burned, status=refunded");

    // Terminal: no further transition off Refunded.
    let err = format!(
        "{:?}",
        wasm.execute(
            &issuer,
            &json!({ "deliver_to_seeder": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID, "leftover": "0" }}),
            &[],
            &keeper,
        )
        .unwrap_err()
    );
    assert!(err.contains("status is") || err.contains("Registered"), "expected terminal-state guard, got: {err}");
}

#[test]
fn sink_refund_returns_legs() {
    let app = InjectiveTestApp::new();
    let admin = app
        .init_account_decimals(&[Coin::new(FUND, "inj"), Coin::new(FUND, PAIR)], &[18, 18])
        .unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let stranger = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let tf = TokenFactory::new(&app);

    let (issuer, seeder_factory, seeder_checksum) =
        deploy_stack(&wasm, &admin, &keeper.address(), &forwarder.address());
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let sink = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    let total: u128 = 5_000_000_000_000_000_000;
    let evm: u128 = 4_000_000_000_000_000_000;
    let cw_held = total - evm; // 1e18
    let pair_seed: u128 = 1_000_000_000_000_000_000;
    let sink_deadline: u64 = 3_600;

    // Register, then deliver so the sink holds the CW-side token (Leg B).
    let keeper = custom_gas(keeper);
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &register_msg(
                &issuer, &seeder_factory, &sink, &evm_authority.address(),
                &refund_receiver.address(), &admin.address(), &denom, &salt, total, evm, sink_deadline,
            ),
            &fee_coins(&tf),
            &keeper,
        );
    });
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &json!({ "deliver_to_seeder": { "evm_authority": evm_authority.address(), "internal_id": INTERNAL_ID, "leftover": "0" }}),
            &[],
            &keeper,
        );
    });
    assert_eq!(bank_balance(&bank, &sink, &denom), cw_held.to_string(), "sink holds token after delivery");

    // Fund the sink's pair side (simulating EVM Leg C), but never Settle.
    bank_send(&app, &admin, &sink, PAIR, pair_seed);

    // Gate: Refund before the sink deadline is rejected (permissionless, but
    // time-gated — no caller exemption).
    let err = format!(
        "{:?}",
        wasm.execute(&sink, &json!({ "refund": {} }), &[], &stranger).unwrap_err()
    );
    assert!(err.contains("Refund deadline not yet reached"), "expected deadline gate, got: {err}");

    // Advance past the deadline → permissionless Refund succeeds.
    app.increase_time(sink_deadline + 1);
    let stranger = custom_gas(stranger);
    exec_tolerant(|| {
        // Token side is ERC20-paired → EVM mirror events.
        let _ = wasm.execute(&sink, &json!({ "refund": {} }), &[], &stranger);
    });

    let state: Value = wasm.query(&sink, &json!({ "sink_state": {} })).unwrap();
    assert_eq!(state["status"], "refunded", "sink state: {state}");
    // Token side returned to the issuer; pair side to refund_receiver.
    assert_eq!(bank_balance(&bank, &issuer, &denom), cw_held.to_string(), "token returned to issuer");
    assert_eq!(bank_balance(&bank, &refund_receiver.address(), PAIR), pair_seed.to_string(), "pair returned to refund_receiver");
    assert_eq!(bank_balance(&bank, &sink, &denom), "0", "sink token drained");
    assert_eq!(bank_balance(&bank, &sink, PAIR), "0", "sink pair drained");
    println!("sink Refund OK: token {cw_held} → issuer, pair {pair_seed} → refund_receiver, sink drained");
}

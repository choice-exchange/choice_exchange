//! End-to-end XYK graduation incl. **Settle** against the real bundled chain,
//! driving the actual compiled wasm of `choice_mts_issuer`, `choice_pool_seeder`,
//! `choice_factory`, `choice_pair`, `choice_send_to_auction`.
//!
//!   1. deploy the XYK factory (+ a native auction as its burn address) and
//!      register the pair denom's decimals (owner-only); the launch denom is
//!      registered by the issuer during RegisterLaunch (`choice_factory: Some`).
//!   2. seeder factory (XYK-wired) + issuer.
//!   3. issuer `RegisterLaunch` (XYK) → denom + ERC20 + Instantiate2 sink.
//!   4. issuer `DeliverToSeeder` → sink gets the CW-side token (Leg B).
//!   5. fund the sink's pair side (simulating EVM Leg C).
//!   6. permissionless `Settle` (caller attaches the create-pair fee) →
//!      `choice_factory.CreatePair` + `ProvideLiquidity`, LP burned.
//!
//! The launch denom is ERC20-paired → mutating calls touching it are wrapped in
//! `exec_tolerant` and verified via queries; the pair side is a plain native
//! denom (`upair`) so reserve assertions aren't perturbed by inj gas/fees.

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
/// Tokenfactory denom-create fee on the genesis (10 INJ) — `CreatePair` mints
/// the LP denom, so `Settle` requires the caller attach it exactly.
const CREATE_PAIR_FEE: u128 = 10_000_000_000_000_000_000;

#[test]
fn xyk_full_graduation_with_settle() {
    let app = InjectiveTestApp::new();
    let admin = app
        .init_account_decimals(&[Coin::new(FUND, "inj"), Coin::new(FUND, PAIR)], &[18, 18])
        .unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let settler = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let tf = TokenFactory::new(&app);

    // --- store everything -------------------------------------------------
    let pair_code_id = wasm.store_code(&artifact("choice_pair"), None, &admin).unwrap().data.code_id;
    let factory_code_id = wasm.store_code(&artifact("choice_factory"), None, &admin).unwrap().data.code_id;
    let auction_code_id = wasm.store_code(&artifact("choice_send_to_auction"), None, &admin).unwrap().data.code_id;
    let seeder_store = wasm.store_code(&artifact("choice_pool_seeder"), None, &admin).unwrap();
    let seeder_code_id = seeder_store.data.code_id;
    let seeder_checksum = seeder_store.data.checksum.clone();
    let issuer_code_id = wasm.store_code(&artifact("choice_mts_issuer"), None, &admin).unwrap().data.code_id;

    // --- XYK factory (+ auction burn address) -----------------------------
    let auction = wasm
        .instantiate(
            auction_code_id,
            &json!({
                "owner": admin.address(),
                "adapter_contract": admin.address(),
                "burn_auction_subaccount":
                    "0x1111111111111111111111111111111111111111111111111111111111111111",
            }),
            Some(&admin.address()),
            Some("auction"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let choice_factory = wasm
        .instantiate(
            factory_code_id,
            &json!({
                "pair_code_id": pair_code_id,
                "burn_address": auction,
                "fee_wallet_address": admin.address(),
            }),
            Some(&admin.address()),
            Some("choice-factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Register the pair denom's decimals (owner-only; attach 1 unit). The launch
    // denom is registered by the issuer during RegisterLaunch.
    wasm.execute(
        &choice_factory,
        &json!({ "add_native_token_decimals": { "denom": PAIR, "decimals": 18u8 }}),
        &[Coin::new(1u128, PAIR)],
        &admin,
    )
    .unwrap();

    // --- seeder factory (XYK-wired) + issuer ------------------------------
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &json!({ "factory": {
                "admin": admin.address(),
                "sink_code_id": seeder_code_id,
                "choice_factory": choice_factory,
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

    // --- derivation + supplies --------------------------------------------
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let sink = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    let total: u128 = 5_000_000_000_000_000_000; // 5e18
    let evm: u128 = 4_000_000_000_000_000_000; //   4e18
    // choice_factory: Some reserves 1 wei of cw_held as the AddNativeTokenDecimals dust.
    let cw_held = total - evm - 1; //                1e18 - 1 (token seed)
    let leftover: u128 = 1_000_000_000_000_000_000;
    let pair_seed: u128 = 1_000_000_000_000_000_000; // pair seed (Leg C)

    let sink_init = json!({
        "issuer": issuer,
        "token_denom": denom,
        "pair_denom": PAIR,
        "token_decimals": 18u8,
        "pair_decimals": 18u8,
        "pool_kind": { "xyk": { "choice_factory": choice_factory, "lp_destination": "burn" }},
        "refund_receiver": refund_receiver.address(),
        "deadline_seconds": 86_400u64,
        "tip_bps": 0u16,
    });
    let create_sink_payload = Binary::new(
        serde_json::to_vec(&json!({ "create_sink": {
            "salt": Binary::new(salt.clone()).to_base64(),
            "sink_init": sink_init,
        }}))
        .unwrap(),
    )
    .to_base64();

    let fee_coins: Vec<Coin> = tf
        .query_params(&QueryParamsRequest {})
        .unwrap()
        .params
        .map(|p| p.denom_creation_fee)
        .unwrap_or_default()
        .into_iter()
        .map(|c| Coin::new(c.amount.parse::<u128>().unwrap(), c.denom))
        .collect();

    // --- RegisterLaunch (choice_factory: Some) + DeliverToSeeder ----------
    let keeper = custom_gas(keeper);
    exec_tolerant(|| {
        let _ = wasm.execute(
            &issuer,
            &json!({ "register_launch": {
                "internal_id": INTERNAL_ID,
                "evm_authority": evm_authority.address(),
                "total_supply": total.to_string(),
                "evm_supply": evm.to_string(),
                "pair_denom": PAIR,
                "seeder_factory": seeder_factory,
                "seeder_addr": sink,
                "create_sink_payload": create_sink_payload,
                "choice_factory": choice_factory,
                "salt_suffix": null,
                "clmm_pool_auth": null,
            }}),
            &fee_coins,
            &keeper,
        );
    });
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
    assert_eq!(bank_balance(&bank, &sink, &denom), cw_held.to_string());

    // --- fund pair side, then Settle --------------------------------------
    bank_send(&app, &admin, &sink, PAIR, pair_seed);
    let settler = custom_gas(settler);
    exec_tolerant(|| {
        let _ = wasm.execute(
            &sink,
            &json!({ "settle": {} }),
            &[Coin::new(CREATE_PAIR_FEE, "inj")],
            &settler,
        );
    });

    // --- assertions -------------------------------------------------------
    let pair_info: Value = wasm
        .query(
            &choice_factory,
            &json!({ "pair": { "asset_infos": [
                { "native_token": { "denom": denom } },
                { "native_token": { "denom": PAIR } },
            ]}}),
        )
        .expect("Pair query failed — pair not created");
    let pair_addr = pair_info["contract_addr"].as_str().expect("no contract_addr").to_string();
    let lp_denom = pair_info["liquidity_token"].as_str().expect("no liquidity_token").to_string();
    println!("XYK pair created at {pair_addr}, lp={lp_denom}");

    // Reserves equal the seed (tip 0, fresh pair → both legs deposited fully).
    let pool: Value = wasm.query(&pair_addr, &json!({ "pool": {} })).unwrap();
    for asset in pool["assets"].as_array().expect("assets") {
        let d = asset["info"]["native_token"]["denom"].as_str().unwrap();
        let amt = asset["amount"].as_str().unwrap();
        let expected = if d == denom { cw_held } else { pair_seed };
        assert_eq!(amt, expected.to_string(), "reserve for {d}");
    }

    // Sink state: Settled, pair recorded, LP minted > 0.
    let state: Value = wasm.query(&sink, &json!({ "sink_state": {} })).unwrap();
    assert_eq!(state["status"], "settled");
    assert_eq!(state["pair_addr"], pair_addr);
    let lp_minted: u128 = state["lp_minted"].as_str().expect("lp_minted").parse().unwrap();
    assert!(lp_minted > 0, "expected positive LP minted");

    // LP was burned (Burn destination); seed fully consumed.
    assert_eq!(bank_balance(&bank, &sink, &lp_denom), "0", "sink holds no LP after burn");
    assert_eq!(bank_balance(&bank, &sink, &denom), "0", "token drained");
    assert_eq!(bank_balance(&bank, &sink, PAIR), "0", "pair drained");

    println!("XYK Settle OK: reserves seeded, LP {lp_minted} minted then burned, sink drained");
}

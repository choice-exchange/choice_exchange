//! End-to-end CLMM **graduation incl. Settle** against the real bundled chain,
//! driving the actual compiled wasm of the whole stack: `choice_mts_issuer`,
//! `choice_pool_seeder` (factory + sink + locker roles), `choice_clmm_factory`,
//! `choice_clmm_manager`, `choice_clmm_pool`.
//!
//! This is the full launch → graduation → locked-liquidity-earns-fees path:
//!   1. deploy CLMM factory + manager, seeder factory (CLMM-wired), issuer.
//!   2. `CreateLocker` (its address is the sink's `position_recipient`).
//!   3. issuer `RegisterLaunch` (CLMM) → denom + ERC20 + AuthorizeCreation +
//!      Instantiate2 sink.
//!   4. issuer `DeliverToSeeder` → sink gets the CW-side token (Leg B).
//!   5. fund the sink's pair side (simulating EVM Leg C).
//!   6. permissionless `Settle` → CLMM pool created at the seed ratio, full-range
//!      position NFT minted to the locker, sink drained.
//!   7. swap to accrue fees → `Locker::CollectFees` → fees land with the
//!      beneficiary, never stranding in the locker.
//!
//! The launch denom is ERC20-paired, so bank transfers of it mirror to EVM and
//! emit non-UTF-8 events → every mutating call that touches it is wrapped in
//! `exec_tolerant` and verified via queries. Pair side is a plain native denom
//! (`upair`) so tip/fee assertions aren't perturbed by inj gas.

use chain_capability_harness::{
    artifact, bank_balance, bank_send, canon20, custom_gas, exec_tolerant, instantiate2_addr,
    instantiated_addr, issuer_salt, FUND,
};
use cosmwasm_std::{Binary, Coin};
use injective_std::types::injective::tokenfactory::v1beta1::QueryParamsRequest;
use injective_test_tube::{Account, Bank, InjectiveTestApp, Module, TokenFactory, Wasm};
use serde_json::{json, Value};

const INTERNAL_ID: u64 = 1;
const FEE_TIER: u32 = 3000;
const TIP_BPS: u16 = 100; // 1%
const PAIR: &str = "upair";

// MIN_SQRT_RATIO + 1 — effectively no downward price limit for a zero_for_one swap.
const MIN_SQRT_PLUS_1: &str = "4295128740";

#[test]
fn clmm_full_graduation_with_settle_and_fees() {
    let app = InjectiveTestApp::new();
    // Funder holds inj (gas) + the pair denom (Leg C source).
    let admin = app
        .init_account_decimals(
            &[Coin::new(FUND, "inj"), Coin::new(FUND, PAIR)],
            &[18, 18],
        )
        .unwrap();
    let keeper = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let forwarder = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let evm_authority = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let refund_receiver = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let beneficiary = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    let settler = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();
    // A "curve buyer": acquires launch denom from the authority before
    // graduation, then trades against the graduated pool to generate fees.
    let trader = app.init_account(&[Coin::new(FUND, "inj")]).unwrap();

    let wasm = Wasm::new(&app);
    let bank = Bank::new(&app);
    let tf = TokenFactory::new(&app);

    // --- store everything -------------------------------------------------
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
    let clmm_pool_code_id = wasm
        .store_code(&artifact("choice_clmm_pool"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let clmm_manager_code_id = wasm
        .store_code(&artifact("choice_clmm_manager"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // --- real CLMM factory + manager --------------------------------------
    let clmm_factory = wasm
        .instantiate(
            clmm_factory_code_id,
            &json!({ "pool_code_id": clmm_pool_code_id }),
            Some(&admin.address()),
            Some("clmm-factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let clmm_manager = wasm
        .instantiate(
            clmm_manager_code_id,
            &json!({ "name": "Choice Positions", "symbol": "CH-POS", "factory_addr": clmm_factory }),
            Some(&admin.address()),
            Some("clmm-manager"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- seeder factory (CLMM-wired) + issuer -----------------------------
    let seeder_factory = wasm
        .instantiate(
            seeder_code_id,
            &json!({ "factory": {
                "admin": admin.address(),
                "sink_code_id": seeder_code_id,
                "choice_factory": admin.address(),
                "clmm_factory": clmm_factory,
                "clmm_manager": clmm_manager,
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

    // --- locker (position recipient) --------------------------------------
    let locker = instantiated_addr(
        &wasm
            .execute(
                &seeder_factory,
                &json!({ "create_locker": {
                    "salt": Binary::new(b"locker-1".to_vec()).to_base64(),
                    "locker_init": {
                        "manager": clmm_manager,
                        "beneficiary": beneficiary.address(),
                        "admin": null,
                    },
                }}),
                &[],
                &admin,
            )
            .unwrap(),
    );

    // --- keeper-side derivation -------------------------------------------
    let denom = format!("factory/{issuer}/shroom_{INTERNAL_ID}");
    let salt = issuer_salt(&issuer, INTERNAL_ID);
    let sink = instantiate2_addr(&seeder_checksum, &canon20(&seeder_factory), &salt);

    let total: u128 = 5_000_000_000_000_000_000; // 5e18
    let evm: u128 = 4_000_000_000_000_000_000; //   4e18
    let cw_held = total - evm; //                    1e18 (token seed)
    let leftover: u128 = 1_000_000_000_000_000_000; // 1e18 burned at deliver
    let pair_seed: u128 = 1_000_000_000_000_000_000; // 1e18 pair seed (Leg C)

    let sink_init = json!({
        "issuer": issuer,
        "token_denom": denom,
        "pair_denom": PAIR,
        "token_decimals": 18u8,
        "pair_decimals": 18u8,
        "pool_kind": { "clmm": {
            "clmm_factory": clmm_factory,
            "clmm_manager": clmm_manager,
            "fee_tier": FEE_TIER,
            "position_recipient": locker,
        }},
        "refund_receiver": refund_receiver.address(),
        "deadline_seconds": 86_400u64,
        "tip_bps": TIP_BPS,
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

    // --- RegisterLaunch + DeliverToSeeder ---------------------------------
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
                "choice_factory": null,
                "salt_suffix": null,
                "clmm_pool_auth": { "clmm_factory": clmm_factory, "fee": FEE_TIER, "ttl_seconds": 0u64 },
            }}),
            &fee_coins,
            &keeper,
        );
    });
    // Simulate the bonding curve selling `sold` of the launch supply to a buyer
    // BEFORE graduation — in production sold tokens leave the curve contract
    // (evm_authority) into buyer wallets, and only the UNSOLD remainder is
    // burned at delivery. DeliverToSeeder now burns the authority's actual
    // remaining balance (the unsold portion), so the buyer must be funded first
    // or there'd be no launch-denom holder left to trade and accrue fees.
    // (Launch-denom transfer mirrors to EVM → non-UTF8 events → exec_tolerant;
    // verified by the balance query below.)
    let sold: u128 = 1_000_000_000_000_000_000; // 1e18 sold to the trader
    let evm_authority = custom_gas(evm_authority);
    exec_tolerant(|| bank_send(&app, &evm_authority, &trader.address(), &denom, sold));
    assert_eq!(
        bank_balance(&bank, &trader.address(), &denom),
        sold.to_string(),
        "buyer should hold the launch denom it bought from the curve"
    );

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
    // Sink holds the CW-side token after delivery.
    assert_eq!(bank_balance(&bank, &sink, &denom), cw_held.to_string());
    // The authority's unsold remainder is fully burned (it kept `evm - sold`,
    // all of which DeliverToSeeder now admin-burns).
    assert_eq!(
        bank_balance(&bank, &evm_authority.address(), &denom),
        "0",
        "DeliverToSeeder should burn the authority's entire unsold balance"
    );

    // --- fund the sink's pair side (simulating EVM Leg C) -----------------
    bank_send(&app, &admin, &sink, PAIR, pair_seed);

    // ===================== Settle =========================================
    let settler = custom_gas(settler);
    let settler_pair_before: u128 = bank_balance(&bank, &settler.address(), PAIR).parse().unwrap();
    exec_tolerant(|| {
        // CLMM settle takes no funds; moving the paired token → EVM events.
        let _ = wasm.execute(&sink, &json!({ "settle": {} }), &[], &settler);
    });

    // Pool created at the factory for (token, pair, fee).
    let pool_addr: String = wasm
        .query(
            &clmm_factory,
            &json!({ "get_pool": {
                "token_a": { "native_token": { "denom": denom } },
                "token_b": { "native_token": { "denom": PAIR } },
                "fee": FEE_TIER,
            }}),
        )
        .expect("GetPool failed — pool not created");
    assert!(!pool_addr.is_empty(), "pool not created");
    println!("CLMM pool created at {pool_addr}");

    // Tip (1% of the pair side) landed with the settler (pair denom != inj, so
    // gas doesn't perturb it).
    let settler_pair_after: u128 = bank_balance(&bank, &settler.address(), PAIR).parse().unwrap();
    assert_eq!(
        settler_pair_after - settler_pair_before,
        pair_seed * TIP_BPS as u128 / 10_000,
        "settler tip"
    );

    // Locker owns exactly one position NFT, with real liquidity.
    let tokens: Value = wasm
        .query(
            &clmm_manager,
            &json!({ "tokens": { "owner": locker, "start_after": null, "limit": 10u32 }}),
        )
        .unwrap();
    let ids = tokens["tokens"].as_array().expect("tokens array");
    assert_eq!(ids.len(), 1, "locker should hold exactly one position NFT");
    let token_id = ids[0].as_str().unwrap().to_string();

    let owner: Value = wasm
        .query(
            &clmm_manager,
            &json!({ "owner_of": { "token_id": token_id, "include_expired": null }}),
        )
        .unwrap();
    assert_eq!(owner["owner"], locker, "locker must own the position NFT");

    let pos: Value = wasm
        .query(&clmm_manager, &json!({ "position_with_fees": { "token_id": token_id }}))
        .unwrap();
    let liquidity: u128 = pos["liquidity"].as_str().unwrap().parse().unwrap();
    assert!(liquidity > 0, "seeded position must have liquidity");
    println!("position NFT {token_id} owned by locker, liquidity={liquidity}");

    // Sink fully drained (dust swept to refund_receiver).
    assert_eq!(bank_balance(&bank, &sink, &denom), "0", "token drained");
    assert_eq!(bank_balance(&bank, &sink, PAIR), "0", "pair drained");

    // ===================== swap → CollectFees =============================
    // token0 = min(denom, PAIR). "factory/…" < "upair", so token0 == launch
    // denom; a zero_for_one swap pays token0, accruing fees on that side, which
    // CollectFees routes to the beneficiary. evm_authority holds launch denom.
    assert!(denom.as_str() < PAIR, "assumed token0 == launch denom");
    let swap_amount: u128 = 100_000_000_000_000_000; // 1e17 launch denom
    let trader = custom_gas(trader);
    exec_tolerant(|| {
        let _ = wasm.execute(
            &pool_addr,
            &json!({ "swap": {
                "recipient": trader.address(),
                "zero_for_one": true,
                "amount_specified": swap_amount.to_string(),
                "sqrt_price_limit_x96": MIN_SQRT_PLUS_1,
            }}),
            &[Coin::new(swap_amount, &denom)],
            &trader,
        );
    });

    let bene_before: u128 = bank_balance(&bank, &beneficiary.address(), &denom).parse().unwrap();
    exec_tolerant(|| {
        let _ = wasm.execute(
            &locker,
            &json!({ "collect_fees": { "token_id": null }}),
            &[],
            &settler,
        );
    });
    let bene_after: u128 = bank_balance(&bank, &beneficiary.address(), &denom).parse().unwrap();
    assert!(
        bene_after > bene_before,
        "beneficiary should receive collected swap fees (before={bene_before}, after={bene_after})"
    );
    assert_eq!(
        bank_balance(&bank, &locker, &denom),
        "0",
        "fees must not strand in the locker"
    );
    println!(
        "CollectFees OK: beneficiary received {} of the launch denom; locker holds 0",
        bene_after - bene_before
    );
}

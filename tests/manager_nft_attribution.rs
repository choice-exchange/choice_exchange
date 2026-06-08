//! Per-NFT fee-attribution exactness (test-tube, regime 13).
//!
//! The pool keys ONE position per `(manager, lower, upper)`; many NFTs collapse
//! onto it. `manager_solvency_fuzz` already proves the aggregate is solvent
//! (Σ NFT owed ≤ pool balance) and that no NFT over-collects. This test proves
//! the SPLIT is correct: two NFTs sharing the identical range, both minted before
//! any swap, must be credited fees in EXACT proportion to their liquidity — one
//! NFT's fees can never leak to another. Equal liquidity ⇒ byte-equal fees.
//!
//! Uses the prebuilt `artifacts/*.wasm` (manager.wasm must be the clean
//! docker-optimized build; a fresh `cargo` build emits bulk-memory ops the
//! test-tube VM rejects — see `./build_release.sh`).

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse, Account, ExecuteResponse,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::manager::{
    ExecuteMsg as ManagerExecuteMsg, InstantiateMsg as ManagerInstantiateMsg,
    PositionWithFeesResponse, QueryMsg as ManagerQueryMsg,
};
use choice_clmm_common::pool::ExecuteMsg as PoolExecuteMsg;
use choice_clmm_common::types::AssetInfo;

const ATOM: &str = "atom";
const USDT: &str = "usdt";
const FEE: u32 = 500; // tick_spacing 10
const DEADLINE: u64 = 9_999_999_999;
const PRICE_ONE: u128 = 79_228_162_514_264_337_593_543_950_336;
const MIN_SQRT_LIMIT: u128 = 4_295_128_739;
const MAX_SQRT_LIMIT: u128 = 158_456_325_028_528_675_187_087_900_672;

type ExecResp = ExecuteResponse<MsgExecuteContractResponse>;

fn native(d: &str) -> AssetInfo {
    AssetInfo::NativeToken { denom: d.to_string() }
}
fn wasm_bytes(f: &str) -> Vec<u8> {
    std::fs::read(format!("../../artifacts/{f}")).unwrap_or_else(|_| panic!("missing artifact {f}"))
}
fn attr(res: &ExecResp, key: &str) -> Option<String> {
    res.events.iter().flat_map(|e| e.attributes.iter()).find(|a| a.key == key).map(|a| a.value.clone())
}

struct Env {
    app: InjectiveTestApp,
    a: SigningAccount,
    b: SigningAccount,
    trader: SigningAccount,
    manager: String,
    pool: String,
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
    let (a, b, trader) = (mk(), mk(), mk());

    let factory_code = wasm.store_code(&wasm_bytes("choice_clmm_factory.wasm"), None, &admin).unwrap().data.code_id;
    let pool_code = wasm.store_code(&wasm_bytes("choice_clmm_pool.wasm"), None, &admin).unwrap().data.code_id;
    let manager_code = wasm.store_code(&wasm_bytes("choice_clmm_manager.wasm"), None, &admin).unwrap().data.code_id;

    let factory = wasm
        .instantiate(factory_code, &FactoryInstantiateMsg { pool_code_id: pool_code }, Some(&admin.address()), Some("F"), &[], &admin)
        .unwrap().data.address;
    let manager = wasm
        .instantiate(manager_code, &ManagerInstantiateMsg { name: "P".into(), symbol: "P".into(), factory_addr: factory.clone() }, Some(&admin.address()), Some("M"), &[], &admin)
        .unwrap().data.address;
    wasm.execute(&factory, &FactoryExecuteMsg::CreatePool {
        token_a: native(ATOM), token_b: native(USDT), fee: FEE, init_sqrt_price: Uint256::from(PRICE_ONE),
    }, &[], &admin).unwrap();
    let pool: String = wasm.query(&factory, &FactoryQueryMsg::GetPool { token_a: native(ATOM), token_b: native(USDT), fee: FEE }).unwrap();

    Env { app, a, b, trader, manager, pool }
}

fn mint(env: &Env, who: &SigningAccount, lower: i32, upper: i32, amt: u128) -> String {
    let wasm = Wasm::new(&env.app);
    let r = wasm.execute(
        &env.manager,
        &ManagerExecuteMsg::MintPosition {
            token0: native(ATOM), token1: native(USDT), fee: FEE,
            tick_lower: lower, tick_upper: upper,
            amount0_desired: Uint128::new(amt), amount1_desired: Uint128::new(amt),
            amount0_min: Uint128::zero(), amount1_min: Uint128::zero(),
            recipient: None, deadline: DEADLINE,
        },
        &[Coin::new(amt, ATOM), Coin::new(amt, USDT)],
        who,
    ).unwrap();
    attr(&r, "token_id").expect("mint emits token_id")
}

fn nft(env: &Env, id: &str) -> PositionWithFeesResponse {
    Wasm::new(&env.app).query(&env.manager, &ManagerQueryMsg::PositionWithFees { token_id: id.to_string() }).unwrap()
}

fn swap(env: &Env, zero_for_one: bool, amt: u128) {
    let wasm = Wasm::new(&env.app);
    let (denom, limit) = if zero_for_one { (ATOM, MIN_SQRT_LIMIT) } else { (USDT, MAX_SQRT_LIMIT) };
    // Low-level Swap with a wide price limit so it fully consumes the input.
    wasm.execute(
        &env.pool,
        &PoolExecuteMsg::Swap {
            recipient: env.trader.address(),
            zero_for_one,
            amount_specified: Uint128::new(amt),
            sqrt_price_limit_x96: Uint256::from(limit),
        },
        &[Coin::new(amt, denom)],
        &env.trader,
    ).unwrap();
}

/// Two NFTs in the same range, liquidity ratio `~ratio` (B has `ratio`x A's
/// deposit). After symmetric swaps, fees must split in proportion to liquidity.
fn attribution_for_ratio(deposit_a: u128, deposit_b: u128) {
    let env = setup();
    // Symmetric range around price 1.0 ⇒ both tokens needed; liquidity ∝ deposit.
    let (lower, upper) = (-1000i32, 1000i32);
    let id_a = mint(&env, &env.a, lower, upper, deposit_a);
    let id_b = mint(&env, &env.b, lower, upper, deposit_b);

    let la = nft(&env, &id_a).liquidity.u128();
    let lb = nft(&env, &id_b).liquidity.u128();
    assert!(la > 0 && lb > 0, "zero liquidity minted (la={}, lb={})", la, lb);

    // Swaps in BOTH directions, kept inside [-1000,1000] so both NFTs stay in
    // range the whole time (price moves only a few ticks per swap at this depth).
    for _ in 0..6 {
        swap(&env, true, 20_000_000);
        swap(&env, false, 20_000_000);
    }

    let pa = nft(&env, &id_a);
    let pb = nft(&env, &id_b);
    for (oa, ob, tok) in [
        (pa.tokens_owed_0.u128(), pb.tokens_owed_0.u128(), "token0"),
        (pa.tokens_owed_1.u128(), pb.tokens_owed_1.u128(), "token1"),
    ] {
        assert!(oa > 0 && ob > 0, "no {} fees credited (la={}, lb={})", tok, la, lb);
        if la == lb {
            // Equal liquidity, same range, same checkpoints ⇒ identical fees.
            assert_eq!(oa, ob, "{}: equal-L NFTs credited unequal fees {} vs {}", tok, oa, ob);
        } else {
            // owed_a/la == owed_b/lb  ⇒  owed_a*lb == owed_b*la (± independent flooring).
            let lhs = oa.checked_mul(lb).unwrap();
            let rhs = ob.checked_mul(la).unwrap();
            let diff = lhs.abs_diff(rhs);
            // Each side floors once per swap step; bound slack generously by the
            // larger liquidity times a small constant.
            let slack = la.max(lb).checked_mul(4).unwrap();
            assert!(diff <= slack, "{}: attribution not proportional: {}/{} vs {}/{} (diff {} > slack {})", tok, oa, la, ob, lb, diff, slack);
        }
    }

    // End-to-end: each NFT can actually collect its credited fees, and the
    // manager strands nothing.
    let wasm = Wasm::new(&env.app);
    for (who, id) in [(&env.a, &id_a), (&env.b, &id_b)] {
        wasm.execute(&env.manager, &ManagerExecuteMsg::Collect { token_id: id.clone(), recipient: None }, &[], who).unwrap();
    }
    let bank = injective_test_tube::Bank::new(&env.app);
    for denom in [ATOM, USDT] {
        let bal = bank.query_balance(&injective_test_tube::injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest {
            address: env.manager.clone(), denom: denom.to_string(),
        }).unwrap().balance.map(|c| c.amount.parse::<u128>().unwrap()).unwrap_or(0);
        assert_eq!(bal, 0, "manager stranded {} {}", bal, denom);
    }
}

#[test]
fn nft_fee_attribution_equal_liquidity_is_identical() {
    attribution_for_ratio(100_000_000, 100_000_000);
}

#[test]
fn nft_fee_attribution_proportional_unequal_liquidity() {
    attribution_for_ratio(100_000_000, 300_000_000);
}

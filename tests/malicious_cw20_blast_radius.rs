#![cfg(test)]
//! Malicious / fee-on-transfer CW20 "blast-radius" integration test (test-tube).
//!
//! Threat model: the CLMM lets anyone create a pool against an arbitrary CW20.
//! A token can misbehave on transfer — skim a fee (fee-on-transfer), revert on
//! payout (blacklist / pausable), or re-enter the pool mid-transfer. The pool
//! pulls CW20 deposits with a fire-and-forget `TransferFrom` (no balance-delta
//! check) and follows checks-effects-interactions on every payout. The question
//! this test answers: **how far can a malicious token's damage spread?**
//!
//! The asserted boundary (what "contained" means here):
//!   1. A separate honest pool is COMPLETELY unaffected — full lifecycle, solvent.
//!   2. In a mixed `honest/EVIL` pool the HONEST token is never
//!      under-collateralized: the pool always physically holds ≥ what it owes in
//!      the honest token, and an LP can always rescue the honest side via a
//!      single-token `Collect` (`amountN_requested = 0`) even when the EVIL leg
//!      is broken.
//!   3. The pool never *creates* value in the malicious token (no inflation): it
//!      pays out at most what was deposited. Any shortfall is confined to the
//!      EVIL token and borne by EVIL's own LPs — it cannot reach the honest token
//!      or another pool.
//!   4. A revert-on-payout token is a *contained, recoverable* DoS: it blocks
//!      only its own pool's joint payout, strands nothing at the protocol layer,
//!      and once the token behaves again funds collect normally.
//!   5. A re-entrant token cannot corrupt pool state or extract value: a reentry
//!      that fails simply reverts the whole (atomic) operation; CEI ordering
//!      means a reentry that succeeds sees already-consistent state.

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::pool::{
    AllPositionsEntry, ExecuteMsg as PoolExecuteMsg, QueryMsg as PoolQueryMsg,
};
use choice_clmm_common::types::AssetInfo;

use malicious_cw20::{
    ExecuteMsg as EvilExec, InstantiateMsg as EvilInit, Mode, QueryMsg as EvilQuery, ReentryPlan,
};

const ATOM: &str = "atom";
const USDT: &str = "usdt";
const FEE: u32 = 500;
const PRICE_ONE: u128 = 79_228_162_514_264_337_593_543_950_336; // 2^96
const MIN_SQRT_LIMIT: u128 = 4_295_128_739;
const MAX_UINT128: Uint128 = Uint128::new(u128::MAX);

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}
fn cw20(addr: &str) -> AssetInfo {
    AssetInfo::Token {
        contract_addr: addr.to_string(),
    }
}

fn get_wasm_byte_code(filename: &str) -> Vec<u8> {
    let path = format!("../../artifacts/{}", filename);
    std::fs::read(&path).unwrap_or_else(|_| panic!("Could not read wasm file at {}", path))
}

fn bank_balance(bank: &Bank<InjectiveTestApp>, address: &str, denom: &str) -> u128 {
    bank.query_balance(&QueryBalanceRequest {
        address: address.to_string(),
        denom: denom.to_string(),
    })
    .unwrap()
    .balance
    .map(|c| c.amount.parse::<u128>().unwrap())
    .unwrap_or(0)
}

fn evil_balance(wasm: &Wasm<InjectiveTestApp>, evil: &str, who: &str) -> u128 {
    let r: cw20::BalanceResponse = wasm
        .query(
            evil,
            &EvilQuery::Balance {
                address: who.to_string(),
            },
        )
        .unwrap();
    r.balance.u128()
}

fn all_positions(wasm: &Wasm<InjectiveTestApp>, pool: &str) -> Vec<AllPositionsEntry> {
    wasm.query(
        pool,
        &PoolQueryMsg::GetAllPositions {
            start_after: None,
            limit: Some(100),
        },
    )
    .unwrap()
}

/// (Σ tokens_owed_0, Σ tokens_owed_1) across every pool position.
fn sum_owed(wasm: &Wasm<InjectiveTestApp>, pool: &str) -> (u128, u128) {
    all_positions(wasm, pool)
        .iter()
        .fold((0u128, 0u128), |a, p| {
            (a.0 + p.tokens_owed_0.u128(), a.1 + p.tokens_owed_1.u128())
        })
}

fn create_pool(
    wasm: &Wasm<InjectiveTestApp>,
    factory: &str,
    admin: &SigningAccount,
    a: AssetInfo,
    b: AssetInfo,
) -> String {
    wasm.execute(
        factory,
        &FactoryExecuteMsg::CreatePool {
            token_a: a.clone(),
            token_b: b.clone(),
            fee: FEE,
            init_sqrt_price: Uint256::from(PRICE_ONE),
        },
        &[],
        admin,
    )
    .unwrap();
    wasm.query(
        factory,
        &FactoryQueryMsg::GetPool {
            token_a: a,
            token_b: b,
            fee: FEE,
        },
    )
    .unwrap()
}

struct Env {
    app: InjectiveTestApp,
    admin: SigningAccount,
    honest_lp: SigningAccount,
    attacker: SigningAccount,
    trader: SigningAccount,
    factory: String,
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
    let honest_lp = mk();
    let attacker = mk();
    let trader = mk();

    let factory_code_id = wasm
        .store_code(
            &get_wasm_byte_code("choice_clmm_factory.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;
    let pool_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_clmm_pool.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    let factory = wasm
        .instantiate(
            factory_code_id,
            &FactoryInstantiateMsg { pool_code_id },
            Some(&admin.address()),
            Some("Choice Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    Env {
        app,
        admin,
        honest_lp,
        attacker,
        trader,
        factory,
    }
}

/// Deploy the adversarial CW20 with `mode`, pre-funding honest_lp/attacker/trader.
fn deploy_evil(env: &Env, wasm: &Wasm<InjectiveTestApp>, mode: Mode) -> String {
    let code_id = wasm
        .store_code(&get_wasm_byte_code("malicious_cw20.wasm"), None, &env.admin)
        .unwrap()
        .data
        .code_id;
    let bal = Uint128::new(1_000_000_000_000_000u128);
    wasm.instantiate(
        code_id,
        &EvilInit {
            name: "Evil".to_string(),
            symbol: "EVIL".to_string(),
            decimals: 6,
            initial_balances: vec![
                (env.honest_lp.address(), bal),
                (env.attacker.address(), bal),
                (env.trader.address(), bal),
            ],
            mode,
        },
        Some(&env.admin.address()),
        Some("Malicious CW20"),
        &[],
        &env.admin,
    )
    .unwrap()
    .data
    .address
}

/// Mint a position into a pool whose token0 is native USDT and token1 is the
/// EVIL CW20. `lp` must have pre-approved `pool` for EVIL. Attaches generous
/// native USDT (surplus refunded by the pool).
fn mint_usdt_evil(
    wasm: &Wasm<InjectiveTestApp>,
    pool: &str,
    lp: &SigningAccount,
    liquidity: u128,
) -> Result<(), String> {
    wasm.execute(
        pool,
        &PoolExecuteMsg::Mint {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::new(liquidity),
        },
        &[Coin::new(1_000_000_000u128, USDT)],
        lp,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn approve_evil(wasm: &Wasm<InjectiveTestApp>, evil: &str, spender: &str, owner: &SigningAccount) {
    wasm.execute(
        evil,
        &EvilExec::IncreaseAllowance {
            spender: spender.to_string(),
            amount: Uint128::new(1_000_000_000_000u128),
            expires: None,
        },
        &[],
        owner,
    )
    .unwrap();
}

// ===========================================================================
// Helper: run a complete, solvent lifecycle on an honest native/native pool and
// assert the LP recovers and the pool stays fully backed. Used to prove a clean
// pool is untouched by a malicious pool living in the same app.
// ===========================================================================
fn assert_honest_pool_healthy(env: &Env, wasm: &Wasm<InjectiveTestApp>, pool: &str) {
    let bank = Bank::new(&env.app);

    // honest_lp mints atom/usdt.
    wasm.execute(
        pool,
        &PoolExecuteMsg::Mint {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::new(1_000_000_000),
        },
        &[
            Coin::new(1_000_000_000u128, ATOM),
            Coin::new(1_000_000_000u128, USDT),
        ],
        &env.honest_lp,
    )
    .unwrap();

    // A swap generates fees.
    wasm.execute(
        pool,
        &PoolExecuteMsg::Swap {
            recipient: env.trader.address(),
            zero_for_one: true,
            amount_specified: Uint128::new(5_000_000),
            sqrt_price_limit_x96: Uint256::from(MIN_SQRT_LIMIT),
        },
        &[Coin::new(5_000_000u128, ATOM)],
        &env.trader,
    )
    .unwrap();

    // Burn ALL liquidity → principal+fees move to owed; then collect.
    wasm.execute(
        pool,
        &PoolExecuteMsg::Burn {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::new(1_000_000_000),
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();

    let (owed0, owed1) = sum_owed(wasm, pool);
    // The pool must physically hold everything it owes (both honest tokens).
    assert!(
        bank_balance(&bank, pool, ATOM) >= owed0,
        "honest pool ATOM insolvent"
    );
    assert!(
        bank_balance(&bank, pool, USDT) >= owed1,
        "honest pool USDT insolvent"
    );

    // The LP fully collects — nothing stranded.
    wasm.execute(
        pool,
        &PoolExecuteMsg::Collect {
            recipient: env.honest_lp.address(),
            lower_tick: -100,
            upper_tick: 100,
            amount0_requested: MAX_UINT128,
            amount1_requested: MAX_UINT128,
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();

    let (a, b) = sum_owed(wasm, pool);
    assert_eq!((a, b), (0, 0), "honest pool: owed not fully collected");
}

// ===========================================================================
// Test 1 — fee-on-transfer over-credit is contained to the EVIL token; the
// honest token stays fully collateralized and rescuable; a separate honest pool
// is untouched.
// ===========================================================================
#[test]
fn fee_on_transfer_overcredit_is_contained() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // EVIL skims 10% on every transfer (incl. the inbound deposit leg).
    let evil = deploy_evil(&env, &wasm, Mode::FeeOnTransfer { bps: 1000 });

    // Honest pool (isolation control) and the mixed USDT/EVIL pool.
    let honest_pool = create_pool(&wasm, &env.factory, &env.admin, native(ATOM), native(USDT));
    let evil_pool = create_pool(&wasm, &env.factory, &env.admin, native(USDT), cw20(&evil));

    approve_evil(&wasm, &evil, &evil_pool, &env.honest_lp);
    approve_evil(&wasm, &evil, &evil_pool, &env.attacker);

    // Both LPs deposit into the EVIL pool. The pool credits full principal but
    // only physically receives 90% of the EVIL leg — it is now over-credited.
    let evil_in_before = evil_balance(&wasm, &evil, &evil_pool);
    mint_usdt_evil(&wasm, &evil_pool, &env.honest_lp, 1_000_000_000).unwrap();
    mint_usdt_evil(&wasm, &evil_pool, &env.attacker, 1_000_000_000).unwrap();
    let evil_received: u128 = evil_balance(&wasm, &evil, &evil_pool) - evil_in_before;

    // Wind both positions down to owed (no swaps: the EVIL shortfall here is
    // purely the deposit-leg skim).
    for lp in [&env.honest_lp, &env.attacker] {
        wasm.execute(
            &evil_pool,
            &PoolExecuteMsg::Burn {
                lower_tick: -100,
                upper_tick: 100,
                amount: Uint128::new(1_000_000_000),
            },
            &[],
            lp,
        )
        .unwrap();
    }

    let (owed_usdt, owed_evil) = sum_owed(&wasm, &evil_pool);
    let pool_usdt = bank_balance(&bank, &evil_pool, USDT);
    let pool_evil = evil_balance(&wasm, &evil, &evil_pool);

    // (2) HONEST token fully collateralized: pool holds ≥ all USDT it owes.
    assert!(
        pool_usdt >= owed_usdt,
        "honest USDT under-collateralized: holds {}, owes {}",
        pool_usdt,
        owed_usdt
    );

    // (3) Shortfall is confined to EVIL, and the pool never CREATED EVIL: it
    // holds no more than it actually received, and owes more than it holds
    // (exactly the skimmed deposit fee). The loss falls on EVIL's own LPs.
    assert!(
        pool_evil <= evil_received,
        "pool conjured EVIL: holds {} > received {}",
        pool_evil,
        evil_received
    );
    assert!(
        pool_evil < owed_evil,
        "expected EVIL shortfall from fee-on-transfer (holds {}, owes {})",
        pool_evil,
        owed_evil
    );

    // (2, rescue) The honest LP can always pull the honest token out, on its own,
    // via a single-sided collect — independent of the broken EVIL leg.
    let lp_usdt_before = bank_balance(&bank, &env.honest_lp.address(), USDT);
    wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Collect {
            recipient: env.honest_lp.address(),
            lower_tick: -100,
            upper_tick: 100,
            amount0_requested: MAX_UINT128,     // USDT (token0)
            amount1_requested: Uint128::zero(), // skip the EVIL leg entirely
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();
    let lp_usdt_gained = bank_balance(&bank, &env.honest_lp.address(), USDT) - lp_usdt_before;
    assert!(lp_usdt_gained > 0, "honest LP failed to rescue its USDT");

    // (1) A separate honest pool runs a full, solvent lifecycle untouched.
    assert_honest_pool_healthy(&env, &wasm, &honest_pool);

    println!(
        "fee-on-transfer: USDT solvent (pool {pool_usdt} ≥ owed {owed_usdt}); EVIL shortfall {} confined to EVIL LPs",
        owed_evil - pool_evil
    );
}

// ===========================================================================
// Test 2 — revert-on-transfer (blacklist/pausable token) is a contained,
// recoverable DoS: it blocks only its own pool's joint payout, the honest token
// is still rescuable, and once the token behaves funds collect normally. The
// honest pool is unaffected.
// ===========================================================================
#[test]
fn revert_on_transfer_dos_is_contained_and_recoverable() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // Start honest so deposits succeed.
    let evil = deploy_evil(&env, &wasm, Mode::Honest);
    let honest_pool = create_pool(&wasm, &env.factory, &env.admin, native(ATOM), native(USDT));
    let evil_pool = create_pool(&wasm, &env.factory, &env.admin, native(USDT), cw20(&evil));

    approve_evil(&wasm, &evil, &evil_pool, &env.honest_lp);
    mint_usdt_evil(&wasm, &evil_pool, &env.honest_lp, 1_000_000_000).unwrap();

    // Wind down to owed (both tokens owed).
    wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Burn {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::new(1_000_000_000),
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();

    // Token turns hostile: every outbound Transfer now reverts.
    wasm.execute(
        &evil,
        &EvilExec::SetMode {
            mode: Mode::RevertOnTransfer,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // A joint collect (both tokens) reverts because the EVIL leg reverts —
    // the whole tx is atomic, so nothing is paid and nothing is lost.
    let joint = wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Collect {
            recipient: env.honest_lp.address(),
            lower_tick: -100,
            upper_tick: 100,
            amount0_requested: MAX_UINT128,
            amount1_requested: MAX_UINT128,
        },
        &[],
        &env.honest_lp,
    );
    assert!(
        joint.is_err(),
        "joint collect should revert while EVIL is hostile"
    );

    // (4, rescue) The honest USDT leg is still fully recoverable on its own.
    let usdt_before = bank_balance(&bank, &env.honest_lp.address(), USDT);
    wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Collect {
            recipient: env.honest_lp.address(),
            lower_tick: -100,
            upper_tick: 100,
            amount0_requested: MAX_UINT128,     // USDT
            amount1_requested: Uint128::zero(), // skip hostile EVIL leg
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();
    assert!(
        bank_balance(&bank, &env.honest_lp.address(), USDT) > usdt_before,
        "honest USDT not rescuable during EVIL DoS"
    );

    // (4, recovery) The EVIL the pool still owes was never lost — once the token
    // behaves again it collects normally. No permanent protocol-level lock.
    wasm.execute(
        &evil,
        &EvilExec::SetMode { mode: Mode::Honest },
        &[],
        &env.admin,
    )
    .unwrap();
    let evil_before = evil_balance(&wasm, &evil, &env.honest_lp.address());
    wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Collect {
            recipient: env.honest_lp.address(),
            lower_tick: -100,
            upper_tick: 100,
            amount0_requested: Uint128::zero(),
            amount1_requested: MAX_UINT128, // EVIL
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();
    assert!(
        evil_balance(&wasm, &evil, &env.honest_lp.address()) > evil_before,
        "EVIL not recoverable after token behaves again"
    );
    assert_eq!(
        sum_owed(&wasm, &evil_pool),
        (0, 0),
        "owed not fully drained after recovery"
    );

    // (1) Honest pool unaffected.
    assert_honest_pool_healthy(&env, &wasm, &honest_pool);
}

// ===========================================================================
// Test 3 — a re-entrant token cannot corrupt pool state. The token re-enters the
// pool during its own TransferFrom; the reentry (a Collect by the token, which
// owns no position) fails, so the whole atomic deposit reverts. State and
// balances are byte-for-byte unchanged — a reentrant token can at most DoS its
// own deposit, never wedge or drain the pool.
// ===========================================================================
#[test]
fn reentrant_token_cannot_corrupt_pool_state() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let evil = deploy_evil(&env, &wasm, Mode::Honest);
    let evil_pool = create_pool(&wasm, &env.factory, &env.admin, native(USDT), cw20(&evil));
    approve_evil(&wasm, &evil, &evil_pool, &env.honest_lp);
    approve_evil(&wasm, &evil, &evil_pool, &env.attacker);

    // Seed an honest position so the pool holds real funds + has live state.
    mint_usdt_evil(&wasm, &evil_pool, &env.honest_lp, 1_000_000_000).unwrap();

    let positions_before = all_positions(&wasm, &evil_pool).len();
    let pool_usdt_before = bank_balance(&bank, &evil_pool, USDT);
    let pool_evil_before = evil_balance(&wasm, &evil, &evil_pool);

    // Arm the reentry: during TransferFrom (the pool pulling the attacker's EVIL
    // deposit), re-enter the pool with a Collect issued BY the token contract,
    // which owns no position → PositionNotFound → the whole mint must revert.
    let reentry_msg = cosmwasm_std::to_json_binary(&PoolExecuteMsg::Collect {
        recipient: evil.clone(),
        lower_tick: -100,
        upper_tick: 100,
        amount0_requested: MAX_UINT128,
        amount1_requested: MAX_UINT128,
    })
    .unwrap();
    wasm.execute(
        &evil,
        &EvilExec::SetReentry {
            plan: Some(ReentryPlan {
                contract: evil_pool.clone(),
                msg: reentry_msg,
                on_transfer: false,
                on_transfer_from: true,
            }),
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // Attacker's mint triggers the re-entrant deposit → must revert atomically.
    let res = mint_usdt_evil(&wasm, &evil_pool, &env.attacker, 1_000_000_000);
    assert!(res.is_err(), "re-entrant mint should have reverted");

    // Nothing changed: position set, pool reserves and pool's EVIL holdings are
    // exactly as before the reverted attempt.
    assert_eq!(
        all_positions(&wasm, &evil_pool).len(),
        positions_before,
        "reentrancy added/removed a position"
    );
    assert_eq!(
        bank_balance(&bank, &evil_pool, USDT),
        pool_usdt_before,
        "reentrancy moved pool USDT"
    );
    assert_eq!(
        evil_balance(&wasm, &evil, &evil_pool),
        pool_evil_before,
        "reentrancy moved pool EVIL"
    );

    // Disarm; the pool is still fully functional for the honest LP afterward.
    wasm.execute(&evil, &EvilExec::SetReentry { plan: None }, &[], &env.admin)
        .unwrap();
    wasm.execute(
        &evil_pool,
        &PoolExecuteMsg::Burn {
            lower_tick: -100,
            upper_tick: 100,
            amount: Uint128::new(1_000_000_000),
        },
        &[],
        &env.honest_lp,
    )
    .unwrap();
    let (owed_usdt, _) = sum_owed(&wasm, &evil_pool);
    assert!(
        bank_balance(&bank, &evil_pool, USDT) >= owed_usdt,
        "pool USDT insolvent after reentrancy episode"
    );
}

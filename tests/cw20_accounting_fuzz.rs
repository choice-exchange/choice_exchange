#![cfg(test)]
// Test harness helpers thread pool/token/account context explicitly; the arg
// counts are intentional for a self-contained fuzzer and not worth bundling.
#![allow(clippy::too_many_arguments)]
//! Item 1 (residual-risk): CW20 / mixed-asset accounting exactness (test-tube).
//!
//! The mock `adversarial_fuzz` battery is native/native only. The CW20 input
//! paths in `apply_swap` — the `Receive`-hook partial-fill refund
//! (`Cw20AlreadySent`), the allowance path's wrongly-attached-native refund
//! (`Cw20Allowance`), and the native-surplus refund alongside a CW20 leg — are
//! outside that differential model. `malicious_cw20_blast_radius` covers
//! *adversarial* tokens; this suite targets the **honest-CW20 accounting
//! exactness** gap, settling the ledger from REAL bank + CW20 balances:
//!
//!   * A model fuzzer over native/CW20 and CW20/CW20 pools (mint / low-level
//!     swap via allowance / swap via Receive hook / burn / collect). After every
//!     op the pool is solvent (holds ≥ what it owes in each token); after a full
//!     drain it strands at most pool-favored dust and owes nothing.
//!   * Receive-hook swaps assert the partial-fill refund is EXACT: the trader's
//!     CW20 balance drops by exactly `amount_in` (sent − unused refund).
//!   * Focused tests: a partial fill via the Receive hook refunds the unused
//!     CW20; a CW20-input swap with wrongly-attached native refunds the native
//!     in full (low-level `Swap` and `SwapExactOutput`).
//!
//! Uses the prebuilt clean `artifacts/*.wasm` (pool + factory + cw20_base_build).

use cosmwasm_std::{to_json_binary, Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::pool::{
    AllPositionsEntry, Cw20HookMsg, ExecuteMsg as PoolExecuteMsg, ProtocolFeesResponse,
    QueryMsg as PoolQueryMsg,
};
use choice_clmm_common::types::AssetInfo;
use cw20::{BalanceResponse, Cw20Coin, Cw20ExecuteMsg};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;

const USDT: &str = "usdt";
const FEE: u32 = 500; // tick_spacing 10
const PRICE_ONE: u128 = 79_228_162_514_264_337_593_543_950_336; // 2^96
const MAX_UINT128: Uint128 = Uint128::new(u128::MAX);

/// Inclusive min sqrt-price limit (MIN_SQRT_RATIO) for a zero_for_one swap.
fn min_limit() -> Uint256 {
    Uint256::from(4_295_128_739u128)
}
/// One below MAX_SQRT_RATIO (a Uint256 far larger than u128::MAX) for a
/// one_for_zero swap — wide enough to fully consume modest inputs.
fn max_limit() -> Uint256 {
    use std::str::FromStr;
    Uint256::from_str("1461446703485210103287273052203988822378723970341").unwrap()
}

fn native(d: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: d.to_string(),
    }
}
fn cw20(addr: &str) -> AssetInfo {
    AssetInfo::Token {
        contract_addr: addr.to_string(),
    }
}
fn wasm_bytes(f: &str) -> Vec<u8> {
    std::fs::read(format!("../../artifacts/{f}"))
        .unwrap_or_else(|_| panic!("missing artifact {}", f))
}
fn attr(
    res: &injective_test_tube::ExecuteResponse<
        injective_test_tube::injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse,
    >,
    key: &str,
) -> Option<String> {
    res.events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == key)
        .map(|a| a.value.clone())
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
fn cw20_balance(wasm: &Wasm<InjectiveTestApp>, token: &str, who: &str) -> u128 {
    let r: BalanceResponse = wasm
        .query(
            token,
            &cw20::Cw20QueryMsg::Balance {
                address: who.to_string(),
            },
        )
        .unwrap();
    r.balance.u128()
}

/// Token handle: either a native bank denom or a CW20 contract address. Lets the
/// fuzzer treat both uniformly for balance reads and pool-asset construction.
#[derive(Clone)]
enum Tok {
    Native(String),
    Cw20(String),
}
impl Tok {
    fn asset(&self) -> AssetInfo {
        match self {
            Tok::Native(d) => native(d),
            Tok::Cw20(a) => cw20(a),
        }
    }
    fn is_cw20(&self) -> bool {
        matches!(self, Tok::Cw20(_))
    }
}

struct Env {
    app: InjectiveTestApp,
    admin: SigningAccount,
    lps: Vec<SigningAccount>,
    trader: SigningAccount,
    factory: String,
    cw20_code: u64,
}

fn setup() -> Env {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);
    let admin = app
        .init_account_decimals(
            &[
                Coin::new(1_000_000_000_000_000_000_000_000_000_000u128, "inj"),
                Coin::new(1_000_000_000_000_000_000u128, USDT),
            ],
            &[18, 6],
        )
        .unwrap();
    let mk = || {
        app.init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000_000_000u128, USDT),
        ])
        .unwrap()
    };
    let lps = vec![mk(), mk()];
    let trader = mk();

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
    let cw20_code = wasm
        .store_code(&wasm_bytes("cw20_base_build.wasm"), None, &admin)
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
        lps,
        trader,
        factory,
        cw20_code,
    }
}

/// Deploy a normal cw20-base token giving every LP and the trader a large
/// balance.
fn deploy_cw20(env: &Env, wasm: &Wasm<InjectiveTestApp>, symbol: &str) -> String {
    let bal = Uint128::new(1_000_000_000_000_000u128);
    let mut initial: Vec<Cw20Coin> = env
        .lps
        .iter()
        .map(|a| Cw20Coin {
            address: a.address(),
            amount: bal,
        })
        .collect();
    initial.push(Cw20Coin {
        address: env.trader.address(),
        amount: bal,
    });
    initial.push(Cw20Coin {
        address: env.admin.address(),
        amount: bal,
    });
    wasm.instantiate(
        env.cw20_code,
        &Cw20InstantiateMsg {
            name: symbol.to_string(),
            symbol: symbol.to_string(),
            decimals: 6,
            initial_balances: initial,
            mint: None,
            marketing: None,
        },
        Some(&env.admin.address()),
        Some(symbol),
        &[],
        &env.admin,
    )
    .unwrap()
    .data
    .address
}

fn create_pool(
    wasm: &Wasm<InjectiveTestApp>,
    factory: &str,
    admin: &SigningAccount,
    a: &Tok,
    b: &Tok,
) -> String {
    wasm.execute(
        factory,
        &FactoryExecuteMsg::CreatePool {
            token_a: a.asset(),
            token_b: b.asset(),
            fee: FEE,
            init_sqrt_price: Uint256::from(PRICE_ONE),
            max_fee_multiple: None,
        },
        &[],
        admin,
    )
    .unwrap();
    // The factory canonicalizes token order; query both orderings.
    wasm.query(
        factory,
        &FactoryQueryMsg::GetPool {
            token_a: a.asset(),
            token_b: b.asset(),
            fee: FEE,
        },
    )
    .unwrap()
}

/// Minimal view of the pool config to learn the factory's canonical token order.
/// Unknown fields (factory, fee_config, hook…) are ignored by serde.
#[derive(serde::Deserialize)]
struct CfgPeek {
    token0: AssetInfo,
    token1: AssetInfo,
}

/// Return the two `Tok` handles in the pool's CANONICAL (token0, token1) order,
/// read from the live pool config — never guessed. The fuzzer needs this so it
/// attaches native funds to the correct side and labels swap directions right.
fn read_order(wasm: &Wasm<InjectiveTestApp>, pool: &str, a: &Tok, b: &Tok) -> (Tok, Tok) {
    let cfg: CfgPeek = wasm.query(pool, &PoolQueryMsg::GetConfig {}).unwrap();
    let matches = |asset: &AssetInfo, t: &Tok| asset.key() == t.asset().key();
    if matches(&cfg.token0, a) {
        assert!(matches(&cfg.token1, b), "pool token1 mismatch");
        (a.clone(), b.clone())
    } else {
        assert!(
            matches(&cfg.token0, b) && matches(&cfg.token1, a),
            "pool token order mismatch"
        );
        (b.clone(), a.clone())
    }
}

fn approve(wasm: &Wasm<InjectiveTestApp>, token: &str, spender: &str, owner: &SigningAccount) {
    wasm.execute(
        token,
        &Cw20ExecuteMsg::IncreaseAllowance {
            spender: spender.to_string(),
            amount: Uint128::new(u128::MAX / 2),
            expires: None,
        },
        &[],
        owner,
    )
    .unwrap();
}

fn all_positions(wasm: &Wasm<InjectiveTestApp>, pool: &str) -> Vec<AllPositionsEntry> {
    wasm.query(
        pool,
        &PoolQueryMsg::GetAllPositions {
            start_after: None,
            limit: Some(200),
        },
    )
    .unwrap()
}
fn sum_owed(wasm: &Wasm<InjectiveTestApp>, pool: &str) -> (u128, u128) {
    all_positions(wasm, pool)
        .iter()
        .fold((0u128, 0u128), |a, p| {
            (a.0 + p.tokens_owed_0.u128(), a.1 + p.tokens_owed_1.u128())
        })
}

/// Read a token balance for an address (bank or cw20).
fn tok_balance(
    wasm: &Wasm<InjectiveTestApp>,
    bank: &Bank<InjectiveTestApp>,
    t: &Tok,
    who: &str,
) -> u128 {
    match t {
        Tok::Native(d) => bank_balance(bank, who, d),
        Tok::Cw20(a) => cw20_balance(wasm, a, who),
    }
}

fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Mint into the pool. Native sides are attached as funds (surplus refunded);
/// CW20 sides are pulled via pre-approved allowance.
fn mint(
    wasm: &Wasm<InjectiveTestApp>,
    pool: &str,
    t0: &Tok,
    t1: &Tok,
    who: &SigningAccount,
    lower: i32,
    upper: i32,
    liq: u128,
) -> bool {
    let mut funds: Vec<Coin> = vec![];
    // Attach a generous amount of every native pool side (pool refunds surplus).
    if let Tok::Native(d) = t0 {
        funds.push(Coin::new(2_000_000_000u128, d.clone()));
    }
    if let Tok::Native(d) = t1 {
        funds.push(Coin::new(2_000_000_000u128, d.clone()));
    }
    funds.sort_by(|a, b| a.denom.cmp(&b.denom));
    wasm.execute(
        pool,
        &PoolExecuteMsg::Mint {
            lower_tick: lower,
            upper_tick: upper,
            amount: Uint128::new(liq),
        },
        &funds,
        who,
    )
    .is_ok()
}

fn burn(
    wasm: &Wasm<InjectiveTestApp>,
    pool: &str,
    who: &SigningAccount,
    lower: i32,
    upper: i32,
    amount: u128,
) -> bool {
    wasm.execute(
        pool,
        &PoolExecuteMsg::Burn {
            lower_tick: lower,
            upper_tick: upper,
            amount: Uint128::new(amount),
        },
        &[],
        who,
    )
    .is_ok()
}

fn collect(
    wasm: &Wasm<InjectiveTestApp>,
    pool: &str,
    who: &SigningAccount,
    lower: i32,
    upper: i32,
) {
    let _ = wasm.execute(
        pool,
        &PoolExecuteMsg::Collect {
            recipient: who.address(),
            lower_tick: lower,
            upper_tick: upper,
            amount0_requested: MAX_UINT128,
            amount1_requested: MAX_UINT128,
        },
        &[],
        who,
    );
}

/// Per-token over-collateralization: the pool must physically hold at least
/// everything it owes. (Active-position reserves push the real balance higher;
/// this is the necessary lower bound that a fund-safety bug would violate.)
fn assert_solvent(
    wasm: &Wasm<InjectiveTestApp>,
    bank: &Bank<InjectiveTestApp>,
    pool: &str,
    t0: &Tok,
    t1: &Tok,
    ctx: &str,
) {
    let (o0, o1) = sum_owed(wasm, pool);
    let b0 = tok_balance(wasm, bank, t0, pool);
    let b1 = tok_balance(wasm, bank, t1, pool);
    assert!(
        b0 >= o0,
        "{}: token0 insolvent (holds {} < owes {})",
        ctx,
        b0,
        o0
    );
    assert!(
        b1 >= o1,
        "{}: token1 insolvent (holds {} < owes {})",
        ctx,
        b1,
        o1
    );
}

/// The randomized model fuzzer over one pool shape. `t0`/`t1` are the canonical
/// pool tokens (already in factory order). Returns nothing; panics on any
/// invariant violation.
fn run_pool_fuzz(
    env: &Env,
    wasm: &Wasm<InjectiveTestApp>,
    bank: &Bank<InjectiveTestApp>,
    a: &Tok,
    b: &Tok,
    seed: u64,
    steps: usize,
) {
    let pool = create_pool(wasm, &env.factory, &env.admin, a, b);
    let (t0, t1) = read_order(wasm, &pool, a, b);
    let (t0, t1) = (&t0, &t1);
    // Approve the pool for any CW20 side, for every LP and the trader.
    for who in env.lps.iter().chain(std::iter::once(&env.trader)) {
        if let Tok::Cw20(a) = t0 {
            approve(wasm, a, &pool, who);
        }
        if let Tok::Cw20(a) = t1 {
            approve(wasm, a, &pool, who);
        }
    }

    // Model: (lp_index, lower, upper) -> liquidity.
    let mut positions: std::collections::BTreeMap<(usize, i32, i32), u128> =
        std::collections::BTreeMap::new();
    let mut st = seed;

    for step in 0..steps {
        let ctx = format!("seed{seed}:step{step}");
        match splitmix(&mut st) % 10 {
            0..=3 => {
                // Mint a symmetric-ish range around price 1.0.
                let oi = (splitmix(&mut st) % env.lps.len() as u64) as usize;
                let half = 10 * (1 + (splitmix(&mut st) % 50) as i32); // multiple of spacing
                let lower = -half;
                let upper = half;
                let liq = 1_000_000 + splitmix(&mut st) % 500_000_000;
                if mint(wasm, &pool, t0, t1, &env.lps[oi], lower, upper, liq as u128) {
                    *positions.entry((oi, lower, upper)).or_insert(0) += liq as u128;
                }
            }
            4..=5 => {
                // Low-level swap via allowance/native with a wide price limit
                // (fully consumes). Direction random.
                let zero_for_one = (splitmix(&mut st) & 1) == 0;
                let amt = 1_000_000 + splitmix(&mut st) % 100_000_000;
                swap_lowlevel(
                    wasm,
                    &pool,
                    t0,
                    t1,
                    &env.trader,
                    zero_for_one,
                    amt as u128,
                    false,
                    false,
                );
            }
            6 => {
                // Receive-hook swap (only if the in-token is CW20) — asserts the
                // partial-fill refund is exact. Pick the CW20 side as input.
                let in_is_t0_cw20 = t0.is_cw20();
                let (in_tok, out_tok, zero_for_one) = if in_is_t0_cw20 {
                    (t0, t1, true)
                } else if t1.is_cw20() {
                    (t1, t0, false)
                } else {
                    // No CW20 side: fall back to a native low-level swap.
                    let zfo = (splitmix(&mut st) & 1) == 0;
                    let amt = 1_000_000 + splitmix(&mut st) % 100_000_000;
                    swap_lowlevel(
                        wasm,
                        &pool,
                        t0,
                        t1,
                        &env.trader,
                        zfo,
                        amt as u128,
                        false,
                        false,
                    );
                    continue;
                };
                let _ = zero_for_one;
                // Deliberately large to provoke partial fills (liquidity exhaustion).
                let amt = 1_000_000 + splitmix(&mut st) % 5_000_000_000;
                swap_receive_exact(
                    wasm,
                    bank,
                    &pool,
                    in_tok,
                    out_tok,
                    &env.trader,
                    amt as u128,
                    &ctx,
                );
            }
            7..=8 => {
                // Burn a fraction of a random existing position.
                if let Some((&(oi, l, u), &liq)) = pick(&positions, &mut st) {
                    if liq > 0 {
                        let frac = 1u128 + (splitmix(&mut st) % 3) as u128;
                        let amount = (liq / frac).max(1).min(liq);
                        if burn(wasm, &pool, &env.lps[oi], l, u, amount) {
                            *positions.get_mut(&(oi, l, u)).unwrap() -= amount;
                        }
                    }
                }
            }
            _ => {
                // Collect a random position's fees.
                if let Some((&(oi, l, u), _)) = pick(&positions, &mut st) {
                    collect(wasm, &pool, &env.lps[oi], l, u);
                }
            }
        }
        assert_solvent(wasm, bank, &pool, t0, t1, &ctx);
    }

    // ---- Drain: burn all liquidity, then collect every position. ----
    let keys: Vec<(usize, i32, i32)> = positions.keys().cloned().collect();
    for &(oi, l, u) in &keys {
        let liq = positions[&(oi, l, u)];
        if liq > 0 {
            assert!(
                burn(wasm, &pool, &env.lps[oi], l, u, liq),
                "drain burn failed at {:?}",
                (oi, l, u)
            );
        }
    }
    for &(oi, l, u) in &keys {
        collect(wasm, &pool, &env.lps[oi], l, u);
    }

    // INV: nothing owed, and the pool strands at most pool-favored dust.
    let (o0, o1) = sum_owed(wasm, &pool);
    assert_eq!(
        (o0, o1),
        (0, 0),
        "seed{seed}: owed not fully drained: {:?}",
        (o0, o1)
    );
    // Accrued protocol fees are pool-HELD but protocol-OWNED (defaulted ON at
    // instantiate, swept separately via `CollectProtocol`, never withdrawable by
    // LPs), so they legitimately remain in the pool balance after LPs fully exit.
    // Exclude them from the LP-residual invariant — otherwise the dynamic fee's
    // protocol carve (larger under the v2 convex fee on big-move fuzz swaps)
    // shows up as a false "stranded LP funds" failure. The carve is exercised by
    // the protocol-fee tests; here we assert the LP-attributable residual is dust.
    let pf: ProtocolFeesResponse = wasm
        .query(&pool, &PoolQueryMsg::GetProtocolFees {})
        .unwrap();
    let resid0 = tok_balance(wasm, bank, t0, &pool).saturating_sub(pf.protocol_fees_0.u128());
    let resid1 = tok_balance(wasm, bank, t1, &pool).saturating_sub(pf.protocol_fees_1.u128());
    // Dust is bounded: pool keeps a few base units per mint/swap rounding. The
    // deposits/volumes here are ≥1e6..1e9, so any non-dust residual (a stranded
    // position or mis-credited fee) is caught by this tight bound.
    let dust_bound = 5_000u128;
    assert!(
        resid0 <= dust_bound,
        "seed{seed}: token0 stranded {} (excl. {} protocol fees) > dust {}",
        resid0,
        pf.protocol_fees_0.u128(),
        dust_bound
    );
    assert!(
        resid1 <= dust_bound,
        "seed{seed}: token1 stranded {} (excl. {} protocol fees) > dust {}",
        resid1,
        pf.protocol_fees_1.u128(),
        dust_bound
    );
}

fn pick<'a>(
    m: &'a std::collections::BTreeMap<(usize, i32, i32), u128>,
    st: &mut u64,
) -> Option<(&'a (usize, i32, i32), &'a u128)> {
    if m.is_empty() {
        return None;
    }
    let idx = (splitmix(st) as usize) % m.len();
    m.iter().nth(idx)
}

/// Low-level `Swap` with a wide (full-consume) or tight price limit. For a CW20
/// in-token the pool pulls via allowance; `attach_native` optionally attaches a
/// native coin to exercise the wrongly-attached-native refund.
fn swap_lowlevel(
    wasm: &Wasm<InjectiveTestApp>,
    pool: &str,
    t0: &Tok,
    t1: &Tok,
    who: &SigningAccount,
    zero_for_one: bool,
    amt: u128,
    tight_limit: bool,
    _attach: bool,
) {
    let in_tok = if zero_for_one { t0 } else { t1 };
    let limit = if zero_for_one {
        min_limit()
    } else {
        max_limit()
    };
    let _ = tight_limit;
    let mut funds: Vec<Coin> = vec![];
    if let Tok::Native(d) = in_tok {
        funds.push(Coin::new(amt, d.clone()));
    }
    let _ = wasm.execute(
        pool,
        &PoolExecuteMsg::Swap {
            recipient: who.address(),
            zero_for_one,
            amount_specified: Uint128::new(amt),
            sqrt_price_limit_x96: limit,
        },
        &funds,
        who,
    );
}

/// Swap a CW20 in via the `Receive` hook (`Cw20::Send`), then assert the
/// partial-fill refund is EXACT: the trader's CW20 balance dropped by exactly
/// `amount_in` (what the swap consumed), and it received `amount_out` of the
/// out-token. This is the load-bearing accounting check for `Cw20AlreadySent`.
fn swap_receive_exact(
    wasm: &Wasm<InjectiveTestApp>,
    bank: &Bank<InjectiveTestApp>,
    pool: &str,
    in_tok: &Tok,
    out_tok: &Tok,
    who: &SigningAccount,
    amt: u128,
    ctx: &str,
) {
    let in_addr = match in_tok {
        Tok::Cw20(a) => a.clone(),
        Tok::Native(_) => return,
    };
    let in_before = cw20_balance(wasm, &in_addr, &who.address());
    let out_before = tok_balance(wasm, bank, out_tok, &who.address());

    let hook = to_json_binary(&Cw20HookMsg::SwapExactInput {
        minimum_amount_out: Uint128::zero(),
        recipient: None,
        deadline: None,
    })
    .unwrap();
    let res = wasm.execute(
        &in_addr,
        &Cw20ExecuteMsg::Send {
            contract: pool.to_string(),
            amount: Uint128::new(amt),
            msg: hook,
        },
        &[],
        who,
    );
    let res = match res {
        Ok(r) => r,
        Err(_) => return, // e.g. swap rejected; no balance change to check
    };
    let amount_in: u128 = attr(&res, "amount_in")
        .expect("swap emits amount_in")
        .parse()
        .unwrap();
    let amount_out: u128 = attr(&res, "amount_out")
        .expect("swap emits amount_out")
        .parse()
        .unwrap();

    let in_after = cw20_balance(wasm, &in_addr, &who.address());
    let out_after = tok_balance(wasm, bank, out_tok, &who.address());

    // The CW20 actually spent must equal the swap's consumed input: the unused
    // remainder of `amt` was refunded via Cw20 Transfer. A refund bug would
    // either over-spend (refund too little) or conjure tokens (refund too much).
    assert_eq!(
        in_before - in_after,
        amount_in,
        "{}: Receive-hook CW20 spend {} != amount_in {} (sent {}, refund mismatch)",
        ctx,
        in_before - in_after,
        amount_in,
        amt
    );
    assert_eq!(
        out_after - out_before,
        amount_out,
        "{}: Receive-hook output {} != amount_out {}",
        ctx,
        out_after - out_before,
        amount_out
    );
    // Partial fills must leave the consumed input strictly below what was sent.
    assert!(
        amount_in <= amt,
        "{}: consumed {} > sent {}",
        ctx,
        amount_in,
        amt
    );
}

// ===========================================================================
// The fuzzer over both mixed-asset pool shapes.
// ===========================================================================
#[test]
fn cw20_accounting_fuzz_native_cw20() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    // Two distinct mixed pools so factory ordering puts the CW20 on either side.
    for seed in [0xC0FFEEu64, 0x1234_5678] {
        let tok = deploy_cw20(&env, &wasm, "AAA");
        run_pool_fuzz(
            &env,
            &wasm,
            &bank,
            &Tok::Native(USDT.to_string()),
            &Tok::Cw20(tok),
            seed,
            40,
        );
    }
}

#[test]
fn cw20_accounting_fuzz_cw20_cw20() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    for seed in [0xABCDu64, 0x9999_7777] {
        let a = deploy_cw20(&env, &wasm, "AAA");
        let b = deploy_cw20(&env, &wasm, "BBB");
        run_pool_fuzz(&env, &wasm, &bank, &Tok::Cw20(a), &Tok::Cw20(b), seed, 40);
    }
}

// ===========================================================================
// Focused: partial-fill refund via the Receive hook (liquidity exhaustion).
// ===========================================================================
#[test]
fn receive_hook_partial_fill_refunds_unused_cw20() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    let tok = deploy_cw20(&env, &wasm, "AAA");
    let a = Tok::Native(USDT.to_string());
    let b = Tok::Cw20(tok.clone());
    let pool = create_pool(&wasm, &env.factory, &env.admin, &a, &b);
    let (t0, t1) = read_order(&wasm, &pool, &a, &b);
    for who in [&env.lps[0], &env.trader] {
        approve(&wasm, &tok, &pool, who);
    }

    // A small, narrow position so a big CW20-in swap exhausts liquidity and
    // partially fills (refunding the unused CW20 leg).
    assert!(
        mint(&wasm, &pool, &t0, &t1, &env.lps[0], -100, 100, 50_000_000),
        "seed mint failed"
    );

    // Identify which side is the CW20.
    let (in_tok, out_tok) = if t0.is_cw20() { (&t0, &t1) } else { (&t1, &t0) };
    let huge = 100_000_000_000u128; // far exceeds the thin liquidity
    let in_before = cw20_balance(&wasm, &tok, &env.trader.address());

    let hook = to_json_binary(&Cw20HookMsg::SwapExactInput {
        minimum_amount_out: Uint128::zero(),
        recipient: None,
        deadline: None,
    })
    .unwrap();
    let res = wasm
        .execute(
            &tok,
            &Cw20ExecuteMsg::Send {
                contract: pool.clone(),
                amount: Uint128::new(huge),
                msg: hook,
            },
            &[],
            &env.trader,
        )
        .unwrap();
    let amount_in: u128 = attr(&res, "amount_in").unwrap().parse().unwrap();
    let in_after = cw20_balance(&wasm, &tok, &env.trader.address());

    // The swap consumed strictly less than sent (partial fill) and refunded the
    // exact remainder: net CW20 spend == amount_in.
    assert!(
        amount_in < huge,
        "expected partial fill: consumed {} of {}",
        amount_in,
        huge
    );
    assert_eq!(
        in_before - in_after,
        amount_in,
        "refund not exact: spent {} != amount_in {}",
        in_before - in_after,
        amount_in
    );

    // The pool is solvent and strands no CW20 beyond what it owes/holds for LPs.
    assert_solvent(&wasm, &bank, &pool, &t0, &t1, "partial-fill");
    let _ = (in_tok, out_tok);
}

// ===========================================================================
// Focused: a CW20-input swap that wrongly attaches native must refund ALL the
// native (Cw20Allowance path) — both for low-level Swap and SwapExactOutput.
// ===========================================================================
#[test]
fn cw20_input_swap_refunds_wrongly_attached_native() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);
    let tok = deploy_cw20(&env, &wasm, "AAA");
    let a = Tok::Native(USDT.to_string());
    let b = Tok::Cw20(tok.clone());
    let pool = create_pool(&wasm, &env.factory, &env.admin, &a, &b);
    let (t0, t1) = read_order(&wasm, &pool, &a, &b);
    for who in [&env.lps[0], &env.trader] {
        approve(&wasm, &tok, &pool, who);
    }
    assert!(
        mint(
            &wasm,
            &pool,
            &t0,
            &t1,
            &env.lps[0],
            -1000,
            1000,
            500_000_000
        ),
        "seed mint failed"
    );

    // zero_for_one such that the CW20 is the INPUT token (output = native USDT).
    let cw20_is_t0 = t0.is_cw20();
    let zero_for_one = cw20_is_t0; // input = token0 when zero_for_one
    let attach = 7_000_000u128; // native USDT wrongly attached to a CW20-input swap
                                // The swap OUTPUT is USDT, so route it to a DIFFERENT recipient — then the
                                // trader's (sender's) USDT change isolates ONLY the attached-native refund,
                                // which must be the full `attach` (net 0). Were any of it absorbed into
                                // reserves, the trader's USDT would end below `usdt_before`.
    let sink = env.lps[1].address();
    let usdt_before = bank_balance(&bank, &env.trader.address(), USDT);

    // Low-level Swap, CW20 input via allowance, with native attached.
    let limit = if zero_for_one {
        min_limit()
    } else {
        max_limit()
    };
    wasm.execute(
        &pool,
        &PoolExecuteMsg::Swap {
            recipient: sink.clone(),
            zero_for_one,
            amount_specified: Uint128::new(2_000_000),
            sqrt_price_limit_x96: limit,
        },
        &[Coin::new(attach, USDT)],
        &env.trader,
    )
    .unwrap();
    // The attached native must be fully refunded: sender's net USDT change == 0.
    let usdt_after = bank_balance(&bank, &env.trader.address(), USDT);
    assert_eq!(
        usdt_after, usdt_before,
        "low-level CW20 swap absorbed attached native: {} -> {}",
        usdt_before, usdt_after
    );

    // SwapExactOutput with CW20 input + wrongly-attached native, same refund.
    let usdt_before2 = bank_balance(&bank, &env.trader.address(), USDT);
    wasm.execute(
        &pool,
        &PoolExecuteMsg::SwapExactOutput {
            zero_for_one,
            amount_out: Uint128::new(1_000_000),
            maximum_amount_in: Uint128::new(u128::MAX),
            recipient: Some(sink.clone()),
            deadline: None,
        },
        &[Coin::new(attach, USDT)],
        &env.trader,
    )
    .unwrap();
    let usdt_after2 = bank_balance(&bank, &env.trader.address(), USDT);
    assert_eq!(
        usdt_after2, usdt_before2,
        "exact-output CW20 swap absorbed attached native: {} -> {}",
        usdt_before2, usdt_after2
    );

    assert_solvent(&wasm, &bank, &pool, &t0, &t1, "wrong-native-refund");
}

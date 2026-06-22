#![cfg(test)]
//! Manager-level stateful solvency / fee-accounting fuzzer (test-tube).
//!
//! Threat model: the `choice_clmm_manager` implements the Uniswap-V3 NFT
//! periphery model where *many* NFTs that share one `(pool, tickLower, tickUpper)`
//! collapse onto a *single* pool-level position keyed by the manager contract.
//! Per-NFT `liquidity` + `fee_growth_inside_last` are supposed to attribute the
//! shared position's fees and principal pro-rata. The load-bearing invariant —
//! reasoned about but never integration-tested — is:
//!
//!     Σ over NFTs of (what an NFT is owed)  ≤  what the pool will actually pay
//!
//! and, equivalently in physical terms, the pool always *holds* at least
//! everything every NFT could collect. If the manager ever over-credits an NFT
//! (rounding in the LP's favor, double-counted fees, wrong liquidity slice) then
//! either some NFT cannot fully collect (the pool clamps and short-pays) or the
//! pool is drained below what it owes the *other* NFTs in the same range.
//!
//! Strategy: drive random interleavings of MintPosition / IncreaseLiquidity /
//! DecreaseLiquidity / Collect / Burn across several owners and many NFTs,
//! **deliberately forcing multiple NFTs into the same tick range**, with swaps
//! interleaved to accrue fees and cross ticks, against the REAL pool + factory +
//! manager wasm on `injective_test_tube` (real bank, real cross-contract
//! reply chains). After every operation we assert, using only on-chain queries:
//!
//!   (a) no NFT can collect more than it is owed, and a legitimate collect is
//!       paid IN FULL (no shortfall) — the pool never short-pays a collect;
//!   (b) `pool_bank_balance(tokenN) ≥ Σ live NFT.tokens_owed_N + protocol_feesN`
//!       — the pool physically holds everything it owes to every NFT (the
//!       strongest, always-true realization of `Σ NFT.owed ≤ pool.position.owed`);
//!   (c) every NFT can be fully drained: decrease-all → collect-all (paid in
//!       full) → burn the NFT succeeds;
//!   (d) no operation strands funds: the manager contract never retains a token
//!       balance, and the burn guard rejects clearing a non-empty position.
//!
//! The fuzzer is validated as non-vacuous by a mutation test (break the
//! manager's `accrue_fees_to_nft` to over-credit) — see the module footer.

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest,
    injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse, Account, Bank,
    ExecuteResponse, InjectiveTestApp, Module, RunnerError, RunnerExecuteResult, SigningAccount,
    Wasm,
};

/// Concrete execute response/result aliases (the wasm execute path returns a
/// `MsgExecuteContractResponse`).
type ExecResp = ExecuteResponse<MsgExecuteContractResponse>;
type ExecResult = RunnerExecuteResult<MsgExecuteContractResponse>;

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::manager::{
    ExecuteMsg as ManagerExecuteMsg, InstantiateMsg as ManagerInstantiateMsg,
    PositionWithFeesResponse, QueryMsg as ManagerQueryMsg,
};
use choice_clmm_common::pool::{
    ExecuteMsg as PoolExecuteMsg, ProtocolFeesResponse, QueryMsg as PoolQueryMsg,
};
use choice_clmm_common::types::AssetInfo;

// token0 < token1: native ordering is lexicographic, "atom" < "usdt".
const ATOM: &str = "atom";
const USDT: &str = "usdt";
const FEE: u32 = 500; // 0.05% tier, tick_spacing = 10 (factory default).
const DEADLINE: u64 = 9_999_999_999;

// Q64.96 sqrt price for 1.0 = 2^96.
const PRICE_ONE: u128 = 79_228_162_514_264_337_593_543_950_336;
// zero_for_one (price down) limit: Uniswap MIN_SQRT_RATIO.
const MIN_SQRT_LIMIT: u128 = 4_295_128_739;
// one_for_zero (price up) limit: sqrt price for ~4.0 (tick ~13863) — comfortably
// above every range used here (|tick| <= 700), so upward swaps can fully cross
// our ranges without ever touching MAX_SQRT_RATIO.
const MAX_SQRT_LIMIT: u128 = 158_456_325_028_528_675_187_087_900_672;

// Tick ranges the fuzzer mints into. All are multiples of TICK_SPACING. The
// first range is heavily weighted in selection so MANY NFTs land in the same
// `(pool, lower, upper)` and collapse onto one pool-level position — the exact
// shared-position accounting this fuzzer exists to stress. The rest overlap it
// partially so swaps cross tick boundaries where some NFTs activate/deactivate.
const RANGES: &[(i32, i32)] = &[
    (-100, 100),
    (-100, 100),
    (-100, 100),
    (-50, 50),
    (-200, 200),
    (0, 300),
    (-300, 0),
    (100, 400),
];

const NUM_OWNERS: usize = 4;

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
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

/// Deterministic splitmix64 PRNG — reproducible failures from a seed.
fn next(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Uniform integer in `[lo, hi]`.
fn rand_in(s: &mut u64, lo: u128, hi: u128) -> u128 {
    debug_assert!(hi >= lo);
    lo + (next(s) as u128) % (hi - lo + 1)
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Env {
    app: InjectiveTestApp,
    owners: Vec<SigningAccount>,
    trader: SigningAccount,
    manager_addr: String,
    pool_addr: String,
}

/// One tracked NFT in the fuzzer's off-chain registry. `lower`/`upper` are
/// retained for failure diagnostics even though the invariants are checked
/// pool-wide rather than per-range.
#[derive(Clone)]
#[allow(dead_code)]
struct Nft {
    token_id: String,
    owner: usize,
    lower: i32,
    upper: i32,
    burned: bool,
}

fn setup() -> Env {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    // The admin registers denom decimals (Injective denom metadata) and creates
    // the pool. Owners/trader just receive genesis balances.
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

    let mut owners = Vec::with_capacity(NUM_OWNERS);
    for _ in 0..NUM_OWNERS {
        owners.push(
            app.init_account(&[
                Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
                Coin::new(1_000_000_000_000_000_000u128, USDT),
                Coin::new(1_000_000_000_000_000_000u128, ATOM),
            ])
            .unwrap(),
        );
    }
    // Trader: a big ATOM + USDT war chest for swapping both directions.
    let trader = app
        .init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000_000_000u128, USDT),
            Coin::new(1_000_000_000_000_000_000u128, ATOM),
        ])
        .unwrap();

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
    let manager_code_id = wasm
        .store_code(
            &get_wasm_byte_code("choice_clmm_manager.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;

    let factory_addr = wasm
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

    let manager_addr = wasm
        .instantiate(
            manager_code_id,
            &ManagerInstantiateMsg {
                name: "Choice Positions".to_string(),
                symbol: "CH-POS".to_string(),
                factory_addr: factory_addr.clone(),
            },
            Some(&admin.address()),
            Some("Choice Manager"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Create the single ATOM/USDT pool at price 1.0 (tick 0).
    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePool {
            token_a: native(ATOM),
            token_b: native(USDT),
            fee: FEE,
            init_sqrt_price: Uint256::from(PRICE_ONE),
            max_fee_multiple: None,
        },
        &[],
        &admin,
    )
    .unwrap();

    let pool_addr: String = wasm
        .query(
            &factory_addr,
            &FactoryQueryMsg::GetPool {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee: FEE,
            },
        )
        .unwrap();

    Env {
        app,
        owners,
        trader,
        manager_addr,
        pool_addr,
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Manager-live owed for an NFT: recorded `tokens_owed` + the unrealized pending
/// fee delta (the manager's `PositionWithFees` computes both). This is exactly
/// what the NFT could collect right now.
fn nft_live(
    wasm: &Wasm<InjectiveTestApp>,
    manager: &str,
    token_id: &str,
) -> PositionWithFeesResponse {
    wasm.query(
        manager,
        &ManagerQueryMsg::PositionWithFees {
            token_id: token_id.to_string(),
        },
    )
    .unwrap()
}

fn protocol_fees(wasm: &Wasm<InjectiveTestApp>, pool: &str) -> (u128, u128) {
    let f: ProtocolFeesResponse = wasm.query(pool, &PoolQueryMsg::GetProtocolFees {}).unwrap();
    (f.protocol_fees_0.u128(), f.protocol_fees_1.u128())
}

/// Find the first occurrence of attribute `key` across all events in a response.
fn attr(res: &ExecResp, key: &str) -> Option<String> {
    res.events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == key)
        .map(|a| a.value.clone())
}

// ---------------------------------------------------------------------------
// The load-bearing invariant: physical pool solvency + no manager residue.
// ---------------------------------------------------------------------------

/// After ANY operation the pool must physically hold at least everything it owes
/// every live NFT (per token), plus any accrued protocol fees. The remaining
/// pool balance is active-liquidity principal. A manager over-credit eventually
/// pushes `Σ owed` above the pool balance and trips here.
///
/// Also asserts the manager contract holds zero of either pool token — it
/// forwards everything to the pool and refunds surplus in the same tx, so any
/// residue is stranded value.
fn assert_solvent(env: &Env, nfts: &[Nft], ctx: &str) {
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let mut owed0: u128 = 0;
    let mut owed1: u128 = 0;
    for n in nfts.iter().filter(|n| !n.burned) {
        let p = nft_live(&wasm, &env.manager_addr, &n.token_id);
        owed0 = owed0
            .checked_add(p.tokens_owed_0.u128())
            .expect("owed0 sum overflow");
        owed1 = owed1
            .checked_add(p.tokens_owed_1.u128())
            .expect("owed1 sum overflow");
    }

    let (pf0, pf1) = protocol_fees(&wasm, &env.pool_addr);
    let need0 = owed0 + pf0;
    let need1 = owed1 + pf1;

    let pool0 = bank_balance(&bank, &env.pool_addr, ATOM);
    let pool1 = bank_balance(&bank, &env.pool_addr, USDT);

    assert!(
        pool0 >= need0,
        "INSOLVENT token0 [{}]: pool holds {} ATOM but owes {} to NFTs + {} protocol = {}",
        ctx,
        pool0,
        owed0,
        pf0,
        need0,
    );
    assert!(
        pool1 >= need1,
        "INSOLVENT token1 [{}]: pool holds {} USDT but owes {} to NFTs + {} protocol = {}",
        ctx,
        pool1,
        owed1,
        pf1,
        need1,
    );

    // The manager is a pure conduit: it must never sit on pool-token value.
    let mgr0 = bank_balance(&bank, &env.manager_addr, ATOM);
    let mgr1 = bank_balance(&bank, &env.manager_addr, USDT);
    assert_eq!(mgr0, 0, "manager stranded {} ATOM [{}]", mgr0, ctx);
    assert_eq!(mgr1, 0, "manager stranded {} USDT [{}]", mgr1, ctx);
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Attempt to mint a new NFT for `owner` in `(lower, upper)` with random desired
/// amounts. We attach generous funds of BOTH tokens; the manager pulls the exact
/// actuals and refunds the rest, so single-sided ranges fund cleanly. Returns
/// the new NFT on success; mints that compute zero liquidity (desired too small
/// for the range vs current price) simply revert atomically and return None.
fn try_mint(env: &Env, s: &mut u64, owner_idx: usize, lower: i32, upper: i32) -> Option<Nft> {
    let wasm = Wasm::new(&env.app);
    let owner = &env.owners[owner_idx];

    let d0 = rand_in(s, 1_000_000, 500_000_000);
    let d1 = rand_in(s, 1_000_000, 500_000_000);

    let funds = vec![Coin::new(d0, ATOM), Coin::new(d1, USDT)];
    let res = wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::MintPosition {
            token0: native(ATOM),
            token1: native(USDT),
            fee: FEE,
            tick_lower: lower,
            tick_upper: upper,
            amount0_desired: Uint128::new(d0),
            amount1_desired: Uint128::new(d1),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            recipient: None,
            deadline: DEADLINE,
        },
        &funds,
        owner,
    );

    match res {
        Ok(r) => {
            let token_id = attr(&r, "token_id").expect("mint emits token_id");
            Some(Nft {
                token_id,
                owner: owner_idx,
                lower,
                upper,
                burned: false,
            })
        }
        // Zero-liquidity / slippage reverts are expected and harmless: the tx is
        // atomic, so the owner's attached funds are fully returned.
        Err(_) => None,
    }
}

/// Increase liquidity on an existing NFT (random desired amounts, both tokens
/// attached generously, surplus refunded).
fn try_increase(env: &Env, s: &mut u64, n: &Nft) -> ExecResult {
    let wasm = Wasm::new(&env.app);
    let d0 = rand_in(s, 1_000_000, 300_000_000);
    let d1 = rand_in(s, 1_000_000, 300_000_000);
    wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::IncreaseLiquidity {
            token_id: n.token_id.clone(),
            amount0_desired: Uint128::new(d0),
            amount1_desired: Uint128::new(d1),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: DEADLINE,
        },
        &[Coin::new(d0, ATOM), Coin::new(d1, USDT)],
        &env.owners[n.owner],
    )
}

/// Decrease a random fraction of an NFT's current liquidity.
fn try_decrease(env: &Env, s: &mut u64, n: &Nft) -> ExecResult {
    let wasm = Wasm::new(&env.app);
    let l = nft_live(&wasm, &env.manager_addr, &n.token_id)
        .liquidity
        .u128();
    if l == 0 {
        return Err(RunnerError::ExecuteError {
            msg: "nft has zero liquidity".to_string(),
        });
    }
    let remove = rand_in(s, 1, l);
    wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::DecreaseLiquidity {
            token_id: n.token_id.clone(),
            liquidity: Uint128::new(remove),
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: DEADLINE,
        },
        &[],
        &env.owners[n.owner],
    )
}

/// Collect an NFT's owed fees+principal to its owner, then assert invariant (a):
/// the NFT is never paid MORE than it was owed, and a legitimate collect is paid
/// IN FULL (the pool must not short-pay). A shortfall would mean the manager had
/// credited the NFT more than the pool's shared position could cover.
fn do_collect_checked(env: &Env, n: &Nft, ctx: &str) {
    let wasm = Wasm::new(&env.app);

    // Owed snapshot BEFORE the collect (live) bounds the legitimate payout.
    let before = nft_live(&wasm, &env.manager_addr, &n.token_id);
    let owed0 = before.tokens_owed_0.u128();
    let owed1 = before.tokens_owed_1.u128();

    let res = wasm
        .execute(
            &env.manager_addr,
            &ManagerExecuteMsg::Collect {
                token_id: n.token_id.clone(),
                recipient: None,
            },
            &[],
            &env.owners[n.owner],
        )
        .unwrap_or_else(|e| panic!("collect must not revert [{ctx}] token {}: {e}", n.token_id));

    // If anything was owed, the pool emits `collect_complete` with the actual
    // paid amounts and the shortfall. Zero-owed collects short-circuit with no
    // event — nothing to check.
    if let Some(paid0) = attr(&res, "amount0_paid") {
        let paid0: u128 = paid0.parse().unwrap();
        let paid1: u128 = attr(&res, "amount1_paid").unwrap().parse().unwrap();
        let short0: u128 = attr(&res, "amount0_shortfall").unwrap().parse().unwrap();
        let short1: u128 = attr(&res, "amount1_shortfall").unwrap().parse().unwrap();

        // (a) never pay out more than the NFT was owed.
        assert!(
            paid0 <= owed0 && paid1 <= owed1,
            "OVER-COLLECT [{ctx}] token {}: paid ({paid0},{paid1}) > owed ({owed0},{owed1})",
            n.token_id,
        );
        // (a) a legitimate collect is fully honoured — the pool's shared
        // position covered everything the manager attributed to this NFT.
        assert_eq!(
            short0, 0,
            "SHORTFALL token0 [{ctx}] token {}: owed {owed0}, paid {paid0}",
            n.token_id,
        );
        assert_eq!(
            short1, 0,
            "SHORTFALL token1 [{ctx}] token {}: owed {owed1}, paid {paid1}",
            n.token_id,
        );
    }
}

/// Push the price around with a random swap to accrue fees and cross ticks.
/// Errors (price already pinned at the limit, or no active liquidity to trade
/// against) are swallowed — they leave state untouched.
fn try_swap(env: &Env, s: &mut u64) {
    let wasm = Wasm::new(&env.app);
    let zero_for_one = next(s) & 1 == 0;
    let amount = rand_in(s, 1_000_000, 200_000_000);
    let (limit, denom) = if zero_for_one {
        (MIN_SQRT_LIMIT, ATOM) // sell token0
    } else {
        (MAX_SQRT_LIMIT, USDT) // sell token1
    };
    let _: ExecResult = wasm.execute(
        &env.pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: env.trader.address(),
            zero_for_one,
            amount_specified: Uint128::new(amount),
            sqrt_price_limit_x96: Uint256::from(limit),
        },
        &[Coin::new(amount, denom)],
        &env.trader,
    );
}

/// Attempt to burn an NFT and assert the guard (d): the manager must accept the
/// burn IFF the position is fully cleared (zero liquidity AND zero owed both
/// sides); otherwise it must reject with the position-not-cleared guard and
/// strand nothing. Marks the NFT burned in the registry on success.
fn try_burn_checked(env: &Env, n: &mut Nft, ctx: &str) {
    let wasm = Wasm::new(&env.app);
    let p = nft_live(&wasm, &env.manager_addr, &n.token_id);
    let cleared = p.liquidity.is_zero() && p.tokens_owed_0.is_zero() && p.tokens_owed_1.is_zero();

    let res: ExecResult = wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::Burn {
            token_id: n.token_id.clone(),
        },
        &[],
        &env.owners[n.owner],
    );

    if cleared {
        res.unwrap_or_else(|e| {
            panic!(
                "burn of cleared NFT must succeed [{ctx}] {}: {e}",
                n.token_id
            )
        });
        n.burned = true;
    } else {
        assert!(
            res.is_err(),
            "burn of NON-cleared NFT must be rejected [{ctx}] token {} (L={}, owed=({},{}))",
            n.token_id,
            p.liquidity,
            p.tokens_owed_0,
            p.tokens_owed_1,
        );
    }
}

/// Drain a single NFT to zero: decrease all liquidity → collect everything
/// (asserting full payment) → burn the NFT (asserting success). Proves
/// invariant (c) for this NFT.
fn drain_nft(env: &Env, n: &mut Nft, ctx: &str) {
    let wasm = Wasm::new(&env.app);

    let l = nft_live(&wasm, &env.manager_addr, &n.token_id)
        .liquidity
        .u128();
    if l > 0 {
        let r: ExecResult = wasm.execute(
            &env.manager_addr,
            &ManagerExecuteMsg::DecreaseLiquidity {
                token_id: n.token_id.clone(),
                liquidity: Uint128::new(l),
                amount0_min: Uint128::zero(),
                amount1_min: Uint128::zero(),
                deadline: DEADLINE,
            },
            &[],
            &env.owners[n.owner],
        );
        r.unwrap_or_else(|e| panic!("drain decrease must succeed [{ctx}] {}: {e}", n.token_id));
    }

    // Collect everything the NFT is now owed; full payment is asserted inside.
    do_collect_checked(env, n, ctx);

    // The NFT must now be fully cleared and burnable.
    let after = nft_live(&wasm, &env.manager_addr, &n.token_id);
    assert!(
        after.liquidity.is_zero(),
        "drain left liquidity on {} [{ctx}]: {}",
        n.token_id,
        after.liquidity
    );
    assert!(
        after.tokens_owed_0.is_zero() && after.tokens_owed_1.is_zero(),
        "drain left owed on {} [{ctx}]: ({},{})",
        n.token_id,
        after.tokens_owed_0,
        after.tokens_owed_1
    );

    let r: ExecResult = wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::Burn {
            token_id: n.token_id.clone(),
        },
        &[],
        &env.owners[n.owner],
    );
    r.unwrap_or_else(|e| panic!("burn after drain must succeed [{ctx}] {}: {e}", n.token_id));
    n.burned = true;
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Run one randomized op sequence from `seed`, asserting solvency after every
/// op, then fully drain every surviving NFT.
fn run_seed(seed: u64, ops: usize) {
    let env = setup();
    let mut s = seed;
    let mut nfts: Vec<Nft> = Vec::new();

    for i in 0..ops {
        let ctx = format!("seed={seed} op={i}");

        // Bias toward mint early (so there's something to act on) and weight a
        // spread of mutators afterward. Always have a mint fallback when no NFT
        // exists yet.
        let live: Vec<usize> = (0..nfts.len()).filter(|&j| !nfts[j].burned).collect();
        let choice = if live.is_empty() {
            0
        } else {
            next(&mut s) % 100
        };

        match choice {
            // Mint a fresh NFT (heavily into RANGES[0..3] == (-100,100)).
            0..=29 => {
                let owner = (next(&mut s) as usize) % NUM_OWNERS;
                let (lo, hi) = RANGES[(next(&mut s) as usize) % RANGES.len()];
                if let Some(n) = try_mint(&env, &mut s, owner, lo, hi) {
                    nfts.push(n);
                }
            }
            // Increase liquidity on a random live NFT.
            30..=44 => {
                let n = nfts[live[(next(&mut s) as usize) % live.len()]].clone();
                let _ = try_increase(&env, &mut s, &n);
            }
            // Decrease a random fraction.
            45..=59 => {
                let n = nfts[live[(next(&mut s) as usize) % live.len()]].clone();
                let _ = try_decrease(&env, &mut s, &n);
            }
            // Collect (checked: no over-collect, no shortfall).
            60..=77 => {
                let n = nfts[live[(next(&mut s) as usize) % live.len()]].clone();
                do_collect_checked(&env, &n, &ctx);
            }
            // Swap to accrue fees / cross ticks.
            78..=92 => {
                try_swap(&env, &mut s);
            }
            // Opportunistic burn attempt (checked: guard accepts iff cleared).
            _ => {
                let idx = live[(next(&mut s) as usize) % live.len()];
                let mut n = nfts[idx].clone();
                try_burn_checked(&env, &mut n, &ctx);
                nfts[idx] = n;
            }
        }

        assert_solvent(&env, &nfts, &ctx);
    }

    // (c) Full drain: every surviving NFT must decrease→collect-in-full→burn.
    let ctx = format!("seed={seed} drain");
    let n = nfts.len();
    for j in 0..n {
        if !nfts[j].burned {
            let mut nft = nfts[j].clone();
            drain_nft(&env, &mut nft, &ctx);
            nfts[j] = nft;
            // Solvency must hold at every step of the wind-down too.
            assert_solvent(&env, &nfts, &ctx);
        }
    }

    // Everything drained: no NFT is owed anything, and the manager is empty.
    assert_solvent(&env, &nfts, &format!("seed={seed} post-drain"));
    for nft in &nfts {
        assert!(nft.burned, "NFT {} survived the full drain", nft.token_id);
    }

    // Report the pool's leftover rounding dust (kept in the pool's favor — a
    // solvency-positive residue, never a deficit).
    let bank = Bank::new(&env.app);
    println!(
        "seed={seed}: drained {} NFTs; pool dust = {} ATOM / {} USDT",
        nfts.len(),
        bank_balance(&bank, &env.pool_addr, ATOM),
        bank_balance(&bank, &env.pool_addr, USDT),
    );
}

#[test]
fn fuzz_manager_solvency_seed_1() {
    run_seed(0x1234_5678_9abc_def0, 70);
}

#[test]
fn fuzz_manager_solvency_seed_2() {
    run_seed(0xfeed_face_dead_beef, 70);
}

#[test]
fn fuzz_manager_solvency_seed_3() {
    run_seed(0x0bad_c0de_cafe_1337, 80);
}

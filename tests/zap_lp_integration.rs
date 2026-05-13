#![cfg(test)]
//! Integration tests for `choice_zap_lp` against a real `choice_pair`.
//!
//! These tests address the audit follow-up that flagged the absence of
//! end-to-end coverage. They require compiled WASM in
//! `choice_exchange/artifacts/` — run `make build-zap-lp` (which also builds
//! choice_pair / choice_factory / choice_send_to_auction via Docker if not
//! present) before `cargo test --test zap_lp_integration`.
//!
//! Coverage:
//!   * `Zap` happy path against a freshly-seeded pair — LP minted, deltas
//!     swept to recipient.
//!   * Snapshot isolation: a user `Zap` does **not** drain pre-existing
//!     contract balances of either pair denom or of the LP token.
//!   * `ZapBalance` round-trip including the keeper `tip_bps` BankMsg.
//!   * Near-empty-pool zap where the swap delta lands at `1` wei — the
//!     M-01 fix skips ProvideLiquidity and sweeps the deltas back.

use cosmwasm_std::{Coin, Decimal, Uint128};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg};
use choice::pair::ExecuteMsg as PairExecuteMsg;

use choice_zap_lp::msg::{
    ExecuteMsg as ZapExecuteMsg, InstantiateMsg as ZapInstantiateMsg, QueryMsg as ZapQueryMsg,
    SimulateZapResponse,
};

const DENOM_INJ: &str = "inj";
const DENOM_ATOM: &str = "atom";
const DENOM_USDT: &str = "usdt";

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

fn get_wasm_byte_code(filename: &str) -> Vec<u8> {
    let path = format!("../../artifacts/{}", filename);
    std::fs::read(&path).unwrap_or_else(|_| panic!("Could not read wasm file at {}", path))
}

#[allow(dead_code)]
struct ZapEnv {
    app: InjectiveTestApp,
    admin: SigningAccount,
    user: SigningAccount,
    keeper: SigningAccount,
    treasury: SigningAccount,
    pair_addr: String,
    lp_denom: String,
    zap_addr: String,
}

/// Bootstrap atom/usdt pair seeded with `seed_amt` per side, then deploy the
/// zap with `treasury` as the default recipient and a 25-bp tip.
fn setup(seed_amt: u128) -> ZapEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let initial = &[
        Coin::new(1_000_000_000_000_000_000_000_000u128, DENOM_INJ),
        Coin::new(100_000_000_000_000u128, DENOM_ATOM),
        Coin::new(100_000_000_000_000u128, DENOM_USDT),
    ];
    let decimals = &[18u32, 6, 6];

    let admin = app.init_account_decimals(initial, decimals).unwrap();
    let user = app.init_account_decimals(initial, decimals).unwrap();
    let keeper = app.init_account_decimals(initial, decimals).unwrap();
    let treasury = app.init_account_decimals(initial, decimals).unwrap();

    // --- Store codes -------------------------------------------------------
    let pair_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_pair.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let factory_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_factory.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let auction_code_id = wasm
        .store_code(
            &get_wasm_byte_code("choice_send_to_auction.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;
    let zap_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_zap_lp.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // --- Burn auction forwarder (required by pair on swap fees) ------------
    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: admin.address(),
                burn_auction_subaccount:
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
            },
            Some(&admin.address()),
            Some("Auction"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- Factory + pair ----------------------------------------------------
    let factory_addr = wasm
        .instantiate(
            factory_code_id,
            &FactoryInstantiateMsg {
                pair_code_id,
                burn_address: auction_addr.clone(),
                fee_wallet_address: admin.address(),
            },
            Some(&admin.address()),
            Some("Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    for (denom, dec) in [(DENOM_ATOM, 6u8), (DENOM_USDT, 6u8)] {
        wasm.execute(
            &factory_addr,
            &FactoryExecuteMsg::AddNativeTokenDecimals {
                denom: denom.to_string(),
                decimals: dec,
            },
            &[Coin::new(1u128, denom)],
            &admin,
        )
        .unwrap();
    }

    let create_pair_fee = vec![Coin::new(10_000_000_000_000_000_000u128, DENOM_INJ)];
    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePair {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: Uint128::zero(),
                },
                Asset {
                    info: native(DENOM_USDT),
                    amount: Uint128::zero(),
                },
            ],
        },
        &create_pair_fee,
        &admin,
    )
    .unwrap();

    let pair_info: PairInfo = wasm
        .query(
            &factory_addr,
            &choice::factory::QueryMsg::Pair {
                asset_infos: [native(DENOM_ATOM), native(DENOM_USDT)],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();

    // --- Seed liquidity (admin) -------------------------------------------
    let seed = Uint128::new(seed_amt);
    wasm.execute(
        &pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: seed,
                },
                Asset {
                    info: native(DENOM_USDT),
                    amount: seed,
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[
            Coin::new(seed.u128(), DENOM_ATOM),
            Coin::new(seed.u128(), DENOM_USDT),
        ],
        &admin,
    )
    .unwrap();

    // --- Zap contract ------------------------------------------------------
    let zap_addr = wasm
        .instantiate(
            zap_code_id,
            &ZapInstantiateMsg {
                owner: Some(admin.address()),
                default_recipient: Some(treasury.address()),
                tip_bps: Some(25),
                min_zap_amount: Some(Uint128::new(1_000_000)),
            },
            Some(&admin.address()),
            Some("Zap"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Owner registers the keeper + the input-denom route (atom-side keeper path).
    wasm.execute(
        &zap_addr,
        &ZapExecuteMsg::AddKeeper {
            address: keeper.address(),
        },
        &[],
        &admin,
    )
    .unwrap();
    wasm.execute(
        &zap_addr,
        &ZapExecuteMsg::RegisterRoute {
            input_denom: DENOM_ATOM.to_string(),
            pair: pair_addr.clone(),
        },
        &[],
        &admin,
    )
    .unwrap();

    ZapEnv {
        app,
        admin,
        user,
        keeper,
        treasury,
        pair_addr,
        lp_denom,
        zap_addr,
    }
}

fn bal(env: &ZapEnv, addr: &str, denom: &str) -> u128 {
    Bank::new(&env.app)
        .query_balance(&QueryBalanceRequest {
            address: addr.to_string(),
            denom: denom.to_string(),
        })
        .unwrap()
        .balance
        .map(|b| b.amount.parse::<u128>().unwrap())
        .unwrap_or(0)
}

/// Permissionless `Zap` against a deep pool: the user sends one denom, gets
/// LP + sub-1% dust back. Sanity-checks the optimal-split + sweep path.
#[test]
fn zap_happy_path_user_receives_lp() {
    let env = setup(10_000_000_000_000u128); // 1e13 each side, 6 decimals
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let input_amt = 1_000_000_000u128; // 1000 ATOM at 6dp — ~0.01% of pool.

    wasm.execute(
        &env.zap_addr,
        &ZapExecuteMsg::Zap {
            pair: env.pair_addr.clone(),
            recipient: None,
            max_spread: Some(Decimal::permille(5)),
            slippage_tolerance: Some(Decimal::percent(1)),
            min_lp_out: Some(Uint128::new(1)),
            deadline: None,
        },
        &[Coin::new(input_amt, DENOM_ATOM)],
        &env.user,
    )
    .unwrap();

    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    assert!(
        user_lp_after > user_lp_before,
        "user should have received LP: before={} after={}",
        user_lp_before,
        user_lp_after
    );

    // Zap contract retains no dust of the two pair sides (within the 1-wei
    // haircut floor) and no LP.
    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(bal(&env, &env.zap_addr, DENOM_USDT) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

/// Snapshot isolation — the headline security invariant for permissionless
/// `Zap`. We seed the contract with attacker-controlled balances of both pair
/// sides AND of LP, then a user zaps in. The user must receive **only** the
/// LP/dust their own input produced; the seeded amounts must stay put.
#[test]
fn zap_snapshot_isolation_does_not_drain_preexisting_balance() {
    let env = setup(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // Seed the zap contract with a non-trivial pre-existing balance of each
    // pair denom (simulating queued royalties) AND of the LP token (simulating
    // LP left over from a previous flow). These must remain untouched after a
    // permissionless user Zap.
    let pre_seed_a = 7_777_777u128;
    let pre_seed_b = 3_333_333u128;
    bank.send(
        injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend {
            from_address: env.admin.address(),
            to_address: env.zap_addr.clone(),
            amount: vec![
                injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin {
                    denom: DENOM_ATOM.to_string(),
                    amount: pre_seed_a.to_string(),
                },
                injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin {
                    denom: DENOM_USDT.to_string(),
                    amount: pre_seed_b.to_string(),
                },
            ],
        },
        &env.admin,
    )
    .unwrap();

    // Mint some LP to admin first (provide_liquidity), then bank-send a slice
    // to the zap so it has a pre-existing LP balance.
    let admin_lp_before = bal(&env, &env.admin.address(), &env.lp_denom);
    wasm.execute(
        &env.pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: Uint128::new(1_000_000),
                },
                Asset {
                    info: native(DENOM_USDT),
                    amount: Uint128::new(1_000_000),
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[
            Coin::new(1_000_000u128, DENOM_ATOM),
            Coin::new(1_000_000u128, DENOM_USDT),
        ],
        &env.admin,
    )
    .unwrap();
    let admin_lp_after = bal(&env, &env.admin.address(), &env.lp_denom);
    let pre_seed_lp = admin_lp_after - admin_lp_before;
    assert!(pre_seed_lp > 0);
    bank.send(
        injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend {
            from_address: env.admin.address(),
            to_address: env.zap_addr.clone(),
            amount: vec![
                injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin {
                    denom: env.lp_denom.clone(),
                    amount: pre_seed_lp.to_string(),
                },
            ],
        },
        &env.admin,
    )
    .unwrap();

    assert_eq!(bal(&env, &env.zap_addr, DENOM_ATOM), pre_seed_a);
    assert_eq!(bal(&env, &env.zap_addr, DENOM_USDT), pre_seed_b);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), pre_seed_lp);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);

    wasm.execute(
        &env.zap_addr,
        &ZapExecuteMsg::Zap {
            pair: env.pair_addr.clone(),
            recipient: None,
            max_spread: Some(Decimal::permille(5)),
            slippage_tolerance: Some(Decimal::percent(1)),
            min_lp_out: Some(Uint128::new(1)),
            deadline: None,
        },
        &[Coin::new(1_000_000_000u128, DENOM_ATOM)],
        &env.user,
    )
    .unwrap();

    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    assert!(user_lp_after > user_lp_before, "user did not receive LP");

    // The seeded balances are untouched (modulo a 1-wei haircut on each pair
    // denom and the LP token — the user's deltas leave dust of at most 1 wei
    // on each pair side, which the contract retains).
    let post_a = bal(&env, &env.zap_addr, DENOM_ATOM);
    let post_b = bal(&env, &env.zap_addr, DENOM_USDT);
    let post_lp = bal(&env, &env.zap_addr, &env.lp_denom);
    assert!(
        post_a >= pre_seed_a && post_a - pre_seed_a <= 1,
        "atom balance drifted: pre={} post={}",
        pre_seed_a,
        post_a
    );
    assert!(
        post_b >= pre_seed_b && post_b - pre_seed_b <= 1,
        "usdt balance drifted: pre={} post={}",
        pre_seed_b,
        post_b
    );
    assert_eq!(
        post_lp, pre_seed_lp,
        "pre-existing LP must not have moved (pre={}, post={})",
        pre_seed_lp, post_lp
    );
}

/// `ZapBalance` round-trip: keeper triggers; tip routes to keeper, LP + dust
/// route to `default_recipient` (treasury).
#[test]
fn zap_balance_pays_tip_and_lps_to_default_recipient() {
    let env = setup(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // Simulate a royalty payout into the zap contract.
    let royalty = 1_000_000_000u128; // 1000 ATOM at 6dp
    bank.send(
        injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend {
            from_address: env.admin.address(),
            to_address: env.zap_addr.clone(),
            amount: vec![
                injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin {
                    denom: DENOM_ATOM.to_string(),
                    amount: royalty.to_string(),
                },
            ],
        },
        &env.admin,
    )
    .unwrap();

    let keeper_atom_before = bal(&env, &env.keeper.address(), DENOM_ATOM);
    let treasury_lp_before = bal(&env, &env.treasury.address(), &env.lp_denom);

    wasm.execute(
        &env.zap_addr,
        &ZapExecuteMsg::ZapBalance {
            input_denom: DENOM_ATOM.to_string(),
            max_spread: Some(Decimal::permille(5)),
            slippage_tolerance: Some(Decimal::percent(1)),
            min_lp_out: Some(Uint128::new(1)),
            deadline: None,
        },
        &[],
        &env.keeper,
    )
    .unwrap();

    let keeper_atom_after = bal(&env, &env.keeper.address(), DENOM_ATOM);
    let treasury_lp_after = bal(&env, &env.treasury.address(), &env.lp_denom);

    // 25 bps of 1e9 = 2_500_000. Keeper also paid gas in inj (irrelevant to
    // the atom-balance check).
    let expected_tip = royalty * 25 / 10_000;
    assert_eq!(
        keeper_atom_after - keeper_atom_before,
        expected_tip,
        "keeper tip should equal tip_bps of input balance"
    );

    assert!(
        treasury_lp_after > treasury_lp_before,
        "treasury should have received LP: before={} after={}",
        treasury_lp_before,
        treasury_lp_after
    );

    // Drain semantics: the zap contract should not retain anything after a
    // ZapBalance (beyond the haircut residual the contract can't avoid).
    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(bal(&env, &env.zap_addr, DENOM_USDT) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

/// M-01: when the optimal-swap output rounds to ≤ 1 wei on one side, the
/// ProvideLiquidity step is skipped instead of being rejected by the pair's
/// `InvalidZeroAmount` (which would happen after the 1-wei haircut zeroed out
/// the deposit). The user's input is fully returned via the sweep.
///
/// We use the standard deep pool (1e13 / 1e13) with a 4-wei ATOM input.
/// `optimal_swap_in` returns ~2; the swap output (gross 2, commission 1) is
/// 1 wei USDT — which after the haircut would be deposit_b = 0 and trigger
/// the pair's zero-share guard if the M-01 skip didn't fire first.
#[test]
fn zap_skips_provide_when_swap_delta_too_small() {
    let env = setup(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let user_atom_before = bal(&env, &env.user.address(), DENOM_ATOM);
    let user_usdt_before = bal(&env, &env.user.address(), DENOM_USDT);

    let tiny = 4u128;
    let resp = wasm
        .execute(
            &env.zap_addr,
            &ZapExecuteMsg::Zap {
                pair: env.pair_addr.clone(),
                recipient: None,
                // A 4-wei trade on a deep pool yields a 100% spread (the entire
                // return is consumed by commission rounding). Set the cap to
                // exactly 1.0 — the pair's check is strict `>`, so this passes.
                max_spread: Some(Decimal::one()),
                slippage_tolerance: Some(Decimal::percent(99)),
                min_lp_out: None,
                deadline: None,
            },
            &[Coin::new(tiny, DENOM_ATOM)],
            &env.user,
        )
        .expect("zap should not panic on tiny input — M-01 skip should fire");

    // Look for the `zap_provide_skip` action attribute — the M-01 path.
    let skipped = resp.events.iter().any(|e| {
        e.ty == "wasm"
            && e.attributes
                .iter()
                .any(|a| a.key == "action" && a.value == "zap_provide_skip")
    });
    assert!(
        skipped,
        "expected the provide step to be skipped under M-01"
    );

    // No LP minted, user got their input back modulo a few wei of net change
    // (some ATOM was sold for ~1 wei USDT).
    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    let user_atom_after = bal(&env, &env.user.address(), DENOM_ATOM);
    let user_usdt_after = bal(&env, &env.user.address(), DENOM_USDT);
    assert_eq!(user_lp_after, user_lp_before, "no LP should have been minted");
    // ATOM net loss bounded by `tiny`; USDT net gain bounded by a few wei.
    let net_atom_loss = user_atom_before as i128 - user_atom_after as i128;
    let net_usdt_gain = user_usdt_after as i128 - user_usdt_before as i128;
    assert!(
        (0..=tiny as i128).contains(&net_atom_loss),
        "net ATOM loss out of bounds: got {}",
        net_atom_loss
    );
    assert!(
        (0..=2).contains(&net_usdt_gain),
        "net USDT gain out of bounds: got {}",
        net_usdt_gain
    );
}

/// Sanity-check `SimulateZap` against the actual `Zap` result. Confirms L-01
/// fix — the query delegates to the pair's `Simulation` so the returned
/// `expected_return` matches what the live swap produces.
#[test]
fn simulate_zap_matches_pair_simulation_query() {
    let env = setup(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let input_amt = Uint128::new(1_000_000_000);
    let sim: SimulateZapResponse = wasm
        .query(
            &env.zap_addr,
            &ZapQueryMsg::SimulateZap {
                pair: env.pair_addr.clone(),
                input_denom: DENOM_ATOM.to_string(),
                input_amount: input_amt,
            },
        )
        .unwrap();

    // Cross-check expected_return against the pair's own Simulation.
    let pair_sim: choice::pair::SimulationResponse = wasm
        .query(
            &env.pair_addr,
            &choice::pair::QueryMsg::Simulation {
                offer_asset: Asset {
                    info: native(DENOM_ATOM),
                    amount: sim.swap_amount,
                },
            },
        )
        .unwrap();

    assert_eq!(
        sim.expected_return, pair_sim.return_amount,
        "zap.SimulateZap.expected_return must equal pair.Simulation.return_amount"
    );
    assert_eq!(sim.swap_amount + sim.deposit_input_side, input_amt);
}

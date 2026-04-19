#![cfg(test)]
//! Vault integration tests (L-13 follow-up to the v0.1 audit).
//!
//! These tests exercise the two-phase deposit, permissionless compound (C-3),
//! pause/unpause (H-4), and reply-chain end-to-end using `injective_test_tube`.
//! They require compiled WASM in `choice_exchange/artifacts/` — run
//! `./build_release.sh` at the repo root before `cargo test --test
//! vault_integration`.

use cosmwasm_std::{Coin, Decimal, Uint128};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank, FeeSetting,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use choice::staking::{
    ExecuteMsg as FarmExecuteMsg, InstantiateMsg as FarmInstantiateMsg,
    QueryMsg as FarmQueryMsg, StakerInfoResponse,
};
use choice_vault::msg::{
    ExecuteMsg as VaultExecuteMsg, InstantiateMsg as VaultInstantiateMsg, QueryMsg as VaultQueryMsg,
    UserInfoResponse,
};

use cw20::{BalanceResponse, Cw20Coin, Cw20ExecuteMsg, Cw20QueryMsg};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;

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

#[allow(dead_code)] // admin/factory_addr used by scenarios yet to be added
struct VaultEnv {
    app: InjectiveTestApp,
    admin: SigningAccount,
    user: SigningAccount,
    compounder: SigningAccount,
    factory_addr: String,
    pair_addr: String,
    lp_denom: String,
    farm_addr: String,
    vault_addr: String,
}

/// One-shot setup for the native/native happy-path scenario.
///
/// Layout:
///   pair:      atom / usdt
///   farm:      stakes lp_denom, rewards in atom (same as asset_infos[0])
///   vault:     empty reward_to_lp_token_route (no swap, single 50/50 split)
///
/// Every account starts with plenty of `inj` for token-factory denom creation
/// fees and gas.
fn setup_native_native() -> VaultEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let initial = &[
        Coin::new(1_000_000_000_000_000_000_000_000u128, DENOM_INJ),
        Coin::new(100_000_000_000_000u128, DENOM_ATOM),
        Coin::new(100_000_000_000_000u128, DENOM_USDT),
    ];
    let decimals = &[18u32, 6, 6];

    // Auto gas estimation under-shoots WithdrawShares' reply chain (farm
    // harvest → unbond → pair withdraw_liquidity → bank sends); pin a
    // generous manual limit so the test is not gas-bound.
    let custom_fee = FeeSetting::Custom {
        amount: Coin::new(1_000_000_000_000_000_000u128, DENOM_INJ),
        gas_limit: 50_000_000,
    };
    let admin = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let user = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let compounder = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee);

    // --- Store codes --------------------------------------------------------
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
    let farm_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_farm.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let vault_code_id = wasm
        .store_code(&get_wasm_byte_code("choice_vault.wasm"), None, &admin)
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

    // --- Burn auction forwarder --------------------------------------------
    // The pair routes 0.25% of every swap fee to a burn-address contract via
    // `BurnAuctionExecuteMsg::SendNative`. That must be a real contract — an
    // EOA gets rejected as "no such contract" when the pair's reply fires.
    // `adapter_contract` is only touched on CW20 swaps; admin is a valid
    // stand-in here.
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
            Some("Choice Send To Auction"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- Factory ------------------------------------------------------------
    let factory_addr = wasm
        .instantiate(
            factory_code_id,
            &FactoryInstantiateMsg {
                pair_code_id,
                burn_address: auction_addr.clone(),
                fee_wallet_address: admin.address(),
            },
            Some(&admin.address()),
            Some("Choice Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Register native decimals for atom and usdt — `CreatePair` requires them.
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

    // --- Pair via factory ---------------------------------------------------
    // `CreatePair` forwards the token-factory denom-create fee from caller funds.
    // Over-provision 10 INJ to cover any chain config.
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

    // --- Seed pair liquidity (admin) ---------------------------------------
    // Reserves need to be large relative to the rewards-to-be-compounded so
    // the 50/50 split's swap leg stays under slippage_tolerance. With a
    // ~1.67e9 reward → ~8.3e8 half-leg, a 1e13 pool gives <0.01% spread.
    let seed_amt = Uint128::new(10_000_000_000_000); // 1e13 of each (6 decimals)
    wasm.execute(
        &pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: seed_amt,
                },
                Asset {
                    info: native(DENOM_USDT),
                    amount: seed_amt,
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[
            Coin::new(seed_amt.u128(), DENOM_ATOM),
            Coin::new(seed_amt.u128(), DENOM_USDT),
        ],
        &admin,
    )
    .unwrap();

    // --- Farm ---------------------------------------------------------------
    // Distribution: 1e12 atom over an hour, starting 60s from now. The delayed
    // start gives the test time to run deposit → activate without the dilution
    // guard tripping on mid-tx accrual (the vault bonds LP during Deposit, so
    // any active schedule would start crediting rewards before Activate).
    let now = app.get_block_time_seconds() as u64;
    let schedule_start = now + 60;
    let farm_addr = wasm
        .instantiate(
            farm_code_id,
            &FarmInstantiateMsg {
                reward_token: native(DENOM_ATOM),
                staking_token: native(&lp_denom),
                distribution_schedule: vec![(
                    schedule_start,
                    schedule_start + 3_600,
                    Uint128::new(10_000_000_000),
                )],
            },
            Some(&admin.address()),
            Some("Choice Farm"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // Fund the farm with the reward tokens it will pay out.
    wasm.execute(
        &farm_addr,
        &FarmExecuteMsg::Fund {},
        &[Coin::new(10_000_000_000u128, DENOM_ATOM)],
        &admin,
    )
    .unwrap();

    // --- Vault --------------------------------------------------------------
    let vault_addr = wasm
        .instantiate(
            vault_code_id,
            &VaultInstantiateMsg {
                owner: admin.address(),
                pair_contract: pair_addr.clone(),
                farm_contract: farm_addr.clone(),
                lp_token: native(&lp_denom),
                reward_token: native(DENOM_ATOM),
                asset_infos: [native(DENOM_ATOM), native(DENOM_USDT)],
                fee_recipient: None,
                fee_percentage: None,
                // Must clear 1 second of schedule accrual (~277M atom at this
                // rate) without tripping either guard: low enough that
                // `increase_time(600)` below pushes pending_reward above it
                // (so Compound fires), high enough that any mid-sequence
                // accrual between Deposit and Activate stays safely below it.
                minimum_reward_to_compound: Uint128::new(1_000_000_000),
                compounder: compounder.address(),
                slippage_tolerance: Decimal::percent(1),
                reward_to_lp_token_route: vec![],
            },
            Some(&admin.address()),
            Some("Choice Vault"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    VaultEnv {
        app,
        admin,
        user,
        compounder,
        factory_addr,
        pair_addr,
        lp_denom,
        farm_addr,
        vault_addr,
    }
}

/// Provide liquidity as `who`, returning the minted LP amount (delta in their
/// lp_denom balance).
fn provide_liquidity(
    env: &VaultEnv,
    who: &SigningAccount,
    atom_amt: u128,
    usdt_amt: u128,
) -> Uint128 {
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let before = bank
        .query_balance(&QueryBalanceRequest {
            address: who.address(),
            denom: env.lp_denom.clone(),
        })
        .unwrap()
        .balance
        .map(|b| b.amount.parse::<u128>().unwrap())
        .unwrap_or(0);

    wasm.execute(
        &env.pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: Uint128::new(atom_amt),
                },
                Asset {
                    info: native(DENOM_USDT),
                    amount: Uint128::new(usdt_amt),
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[
            Coin::new(atom_amt, DENOM_ATOM),
            Coin::new(usdt_amt, DENOM_USDT),
        ],
        who,
    )
    .unwrap();

    let after = bank
        .query_balance(&QueryBalanceRequest {
            address: who.address(),
            denom: env.lp_denom.clone(),
        })
        .unwrap()
        .balance
        .map(|b| b.amount.parse::<u128>().unwrap())
        .unwrap_or(0);

    Uint128::new(after - before)
}

fn bank_balance(env: &VaultEnv, addr: &str, denom: &str) -> u128 {
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

fn user_info(env: &VaultEnv, who: &str) -> UserInfoResponse {
    Wasm::new(&env.app)
        .query(
            &env.vault_addr,
            &VaultQueryMsg::UserInfo {
                user: who.to_string(),
            },
        )
        .unwrap()
}

fn total_shares(env: &VaultEnv) -> Uint128 {
    Wasm::new(&env.app)
        .query(&env.vault_addr, &VaultQueryMsg::TotalShares {})
        .unwrap()
}

fn farm_bond(env: &VaultEnv, staker: &str) -> Uint128 {
    let resp: StakerInfoResponse = Wasm::new(&env.app)
        .query(
            &env.farm_addr,
            &FarmQueryMsg::StakerInfo {
                staker: staker.to_string(),
                block_time: None,
            },
        )
        .unwrap();
    resp.bond_amount
}

/// User deposits `lp_amount` native LP tokens into the vault.
fn deposit_lp(env: &VaultEnv, who: &SigningAccount, lp_amount: Uint128) {
    Wasm::new(&env.app)
        .execute(
            &env.vault_addr,
            &VaultExecuteMsg::Deposit {},
            &[Coin::new(lp_amount.u128(), &env.lp_denom)],
            who,
        )
        .unwrap();
}

#[test]
fn native_native_deposit_activate_compound_withdraw() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    // User mints LP, deposits into vault.
    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    assert!(!lp_amount.is_zero(), "user minted no LP");

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Deposit {},
        &[Coin::new(lp_amount.u128(), &env.lp_denom)],
        &env.user,
    )
    .unwrap();

    // Pending deposit should now reflect the full amount; shares still zero.
    let info: UserInfoResponse = wasm
        .query(
            &env.vault_addr,
            &VaultQueryMsg::UserInfo {
                user: env.user.address(),
            },
        )
        .unwrap();
    assert_eq!(info.pending_deposit, lp_amount);
    assert!(info.shares.is_zero());

    // Compounder activates the pending deposit → shares are minted.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    let info: UserInfoResponse = wasm
        .query(
            &env.vault_addr,
            &VaultQueryMsg::UserInfo {
                user: env.user.address(),
            },
        )
        .unwrap();
    assert!(info.pending_deposit.is_zero());
    assert_eq!(info.shares, lp_amount, "first depositor mints 1:1");

    // Let rewards accrue for 10 minutes.
    env.app.increase_time(600);

    // Sanity: farm should show the vault has pending rewards. `block_time`
    // must be supplied — the query is a pure read and only projects
    // accrued-but-uncredited rewards when given the current time.
    let staker: StakerInfoResponse = wasm
        .query(
            &env.farm_addr,
            &FarmQueryMsg::StakerInfo {
                staker: env.vault_addr.clone(),
                block_time: Some(env.app.get_block_time_seconds() as u64),
            },
        )
        .unwrap();
    assert!(
        !staker.pending_reward.is_zero(),
        "farm did not accrue rewards"
    );

    // Permissionless compound (anyone can call). `minimum_lp_to_receive` must
    // be non-zero — set to 1 here; in prod the keeper computes a bound.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()], // single-swap: atom→usdt at ~1:1
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user, // any signer works post-C-3
    )
    .unwrap();

    // Withdraw all shares.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: info.shares,
        },
        &[],
        &env.user,
    )
    .unwrap();

    // Exit returns LP (vault does not unwrap for the user) + proportional
    // slice of any unharvested farm rewards. The user's LP count must exceed
    // what they originally deposited — the surplus is the compound gain.
    let user_lp_after = bank_balance(&env, &env.user.address(), &env.lp_denom);
    assert!(
        user_lp_after > lp_amount.u128(),
        "expected compound gain: LP after {} > deposited {}",
        user_lp_after,
        lp_amount.u128()
    );

    let vault_shares: Uint128 = wasm
        .query(&env.vault_addr, &VaultQueryMsg::TotalShares {})
        .unwrap();
    assert!(vault_shares.is_zero(), "vault should be fully unwound");
}

/// H-4: paused vault rejects entry paths but still honors exits. Unpause
/// restores the happy path.
#[test]
fn pause_blocks_entry_but_allows_exit() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    // Get the user into active shares.
    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    deposit_lp(&env, &env.user, lp_amount);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    // Owner pauses.
    wasm.execute(&env.vault_addr, &VaultExecuteMsg::Pause, &[], &env.admin)
        .unwrap();

    // Every entry path is gated. Error strings need not be stable — what matters
    // is "not a success". Each call is in isolation; none must affect state.
    let extra_lp = provide_liquidity(&env, &env.user, 1_000_000, 1_000_000);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Deposit {},
        &[Coin::new(extra_lp.u128(), &env.lp_denom)],
        &env.user,
    )
    .unwrap_err();

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap_err();

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivateMyDeposit {},
        &[],
        &env.user,
    )
    .unwrap_err();

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap_err();

    // Exit path is open even when paused — the whole point of H-4.
    let pre_exit_shares = user_info(&env, &env.user.address()).shares;
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: pre_exit_shares,
        },
        &[],
        &env.user,
    )
    .unwrap();
    assert!(total_shares(&env).is_zero(), "user exit should clear shares");

    // Unpause restores entry paths.
    wasm.execute(&env.vault_addr, &VaultExecuteMsg::Unpause, &[], &env.admin)
        .unwrap();

    let fresh_lp = provide_liquidity(&env, &env.user, 1_000_000_000, 1_000_000_000);
    deposit_lp(&env, &env.user, fresh_lp);
    assert_eq!(
        user_info(&env, &env.user.address()).pending_deposit,
        fresh_lp,
        "unpaused deposit must land in pending"
    );
}

/// Compound that fails mid-route (belief_price wildly off from pool) must
/// revert cleanly — no partial state, farm bond unchanged, user can still
/// exit afterwards.
#[test]
fn compound_reverts_on_tight_belief_price() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    deposit_lp(&env, &env.user, lp_amount);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    env.app.increase_time(600);

    let shares_before = total_shares(&env);
    let bond_before = farm_bond(&env, &env.vault_addr);

    // `assert_max_spread` computes expected_return = offer_amount / belief.
    // Real pair is ~1:1, so setting belief=0.01 implies "I expect 100x
    // return" — actual return lands 99% short → MaxSpreadAssertion fires.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::from_ratio(1u128, 100u128)],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap_err();

    // Full revert — shares + farm bond untouched.
    assert_eq!(
        shares_before,
        total_shares(&env),
        "compound revert must not mutate shares"
    );
    assert_eq!(
        bond_before,
        farm_bond(&env, &env.vault_addr),
        "compound revert must not mutate farm bond"
    );

    // Recovery: a sane compound still works, and the exit path is intact.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap();

    let shares_after_compound = user_info(&env, &env.user.address()).shares;
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: shares_after_compound,
        },
        &[],
        &env.user,
    )
    .unwrap();
}

/// B-6: minimum_lp_to_receive heuristic trip — sanity-check the on-chain
/// `estimate_expected_lp` path against a real pair. With reserves of 10^11
/// each side and ~540s of 10^10/hour accrual, pending_reward ≈ 1.5e9 →
/// expected_lp ≈ 7e8 → floor (at k = 10%) ≈ 7e7. A caller passing
/// `minimum_lp_to_receive = 1` must be rejected by `MinimumLpBelowHeuristic`
/// *before* harvest fires; passing a floor just above the heuristic bound
/// (here: 1e8) proceeds normally. Complements the unit-test coverage which
/// mocks the pool response — this guards against simulation-math drift
/// from the pair contract's real `Pool {}` / `Simulation` responses.
#[test]
fn compound_heuristic_rejects_min_lp_of_one() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    deposit_lp(&env, &env.user, lp_amount);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    env.app.increase_time(600);

    let shares_before = total_shares(&env);
    let bond_before = farm_bond(&env, &env.vault_addr);

    // Token dust → must trip the heuristic floor. The error string is not
    // part of the public API; match the MinimumLpBelowHeuristic marker via
    // substring to keep the test stable against future wording tweaks.
    let err = wasm
        .execute(
            &env.vault_addr,
            &VaultExecuteMsg::Compound {
                belief_prices: vec![Decimal::one()],
                minimum_lp_to_receive: Uint128::new(1),
            },
            &[],
            &env.user,
        )
        .unwrap_err();
    let err_str = format!("{}", err);
    assert!(
        err_str.contains("below heuristic floor"),
        "expected MinimumLpBelowHeuristic, got: {}",
        err_str
    );

    // Pre-harvest rejection: no state changes.
    assert_eq!(shares_before, total_shares(&env));
    assert_eq!(bond_before, farm_bond(&env, &env.vault_addr));

    // A realistic floor clears the heuristic and the compound succeeds.
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap();
    // Farm bond must strictly grow — the compound staked new LP.
    assert!(
        farm_bond(&env, &env.vault_addr) > bond_before,
        "compound should grow the farm bond"
    );
}

/// Native + CW20 pair variant of the happy-path scenario. Exercises the
/// CW20-sensitive code paths in the vault: `IncreaseAllowance` + CW20 leg in
/// `ProvideLiquidity` (compound step 3), plus the vault receiving CW20 as
/// the swap return.
///
/// Layout:
///   pair:   atom / choicoin (CW20)
///   reward: atom (same as asset_infos[0] → empty route)
///   vault LP: native factory/{pair}/lp (LP is always native for choice_pair)
///
/// User never has to touch the CW20 — admin seeds pair liquidity, then sends
/// LP to the user directly. The vault handles CW20 on the compound leg.
#[test]
fn native_cw20_deposit_activate_compound_withdraw() {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let initial = &[
        Coin::new(1_000_000_000_000_000_000_000_000u128, DENOM_INJ),
        Coin::new(100_000_000_000_000u128, DENOM_ATOM),
    ];
    let decimals = &[18u32, 6];
    let custom_fee = FeeSetting::Custom {
        amount: Coin::new(1_000_000_000_000_000_000u128, DENOM_INJ),
        gas_limit: 50_000_000,
    };
    let admin = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let user = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let compounder = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee);

    // --- Codes --------------------------------------------------------------
    let pair_code = wasm
        .store_code(&get_wasm_byte_code("choice_pair.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let factory_code = wasm
        .store_code(&get_wasm_byte_code("choice_factory.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let farm_code = wasm
        .store_code(&get_wasm_byte_code("choice_farm.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let vault_code = wasm
        .store_code(&get_wasm_byte_code("choice_vault.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let auction_code = wasm
        .store_code(
            &get_wasm_byte_code("choice_send_to_auction.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;
    let cw20_code = wasm
        .store_code(&get_wasm_byte_code("cw20_base_build.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let cw20_adapter_code = wasm
        .store_code(&get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // The pair routes its CW20 swap-fee burn through the auction contract,
    // which in turn calls `cw20.Send(cw20_adapter, amount)` to convert the
    // CW20 into a factory denom before depositing to the exchange burn
    // subaccount. The adapter MUST be a real contract — an EOA fails with
    // "no such contract" because CW20 Send invokes the recipient's hook.
    #[derive(serde::Serialize)]
    struct EmptyInstantiate {}
    let cw20_adapter_addr = wasm
        .instantiate(
            cw20_adapter_code,
            &EmptyInstantiate {},
            Some(&admin.address()),
            Some("CW20 Adapter"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    // Adapter needs INJ on hand to pay the token-factory denom-create fee
    // when it first sees a new CW20 (one denom per token, one fee).
    use injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend as MsgSend0;
    use injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin0;
    Bank::new(&app)
        .send(
            MsgSend0 {
                from_address: admin.address(),
                to_address: cw20_adapter_addr.clone(),
                amount: vec![ProtoCoin0 {
                    denom: DENOM_INJ.to_string(),
                    amount: "100000000000000000000".to_string(), // 100 INJ
                }],
            },
            &admin,
        )
        .unwrap();

    // --- CW20 token (choicoin) ---------------------------------------------
    // Huge initial balance to admin; user gets their LP via bank send later.
    let cw20_addr = wasm
        .instantiate(
            cw20_code,
            &Cw20InstantiateMsg {
                name: "Choicoin".to_string(),
                symbol: "CHOI".to_string(),
                decimals: 6,
                initial_balances: vec![Cw20Coin {
                    address: admin.address(),
                    amount: Uint128::new(100_000_000_000_000),
                }],
                mint: None,
                marketing: None,
            },
            Some(&admin.address()),
            Some("Choicoin"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    let cw20_info = AssetInfo::Token {
        contract_addr: cw20_addr.clone(),
    };

    // --- Burn auction + factory --------------------------------------------
    let auction_addr = wasm
        .instantiate(
            auction_code,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: cw20_adapter_addr.clone(),
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
    let factory_addr = wasm
        .instantiate(
            factory_code,
            &FactoryInstantiateMsg {
                pair_code_id: pair_code,
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

    // atom decimals registration (factory queries CW20 decimals directly).
    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::AddNativeTokenDecimals {
            denom: DENOM_ATOM.to_string(),
            decimals: 6,
        },
        &[Coin::new(1u128, DENOM_ATOM)],
        &admin,
    )
    .unwrap();

    // --- Create the native/CW20 pair ---------------------------------------
    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePair {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: Uint128::zero(),
                },
                Asset {
                    info: cw20_info.clone(),
                    amount: Uint128::zero(),
                },
            ],
        },
        &[Coin::new(10_000_000_000_000_000_000u128, DENOM_INJ)],
        &admin,
    )
    .unwrap();
    let pair_info: PairInfo = wasm
        .query(
            &factory_addr,
            &choice::factory::QueryMsg::Pair {
                asset_infos: [native(DENOM_ATOM), cw20_info.clone()],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();

    // --- Seed liquidity: admin approves pair for CW20 pull, provides both --
    let seed_amt = Uint128::new(10_000_000_000_000); // 1e13 each side
    wasm.execute(
        &cw20_addr,
        &Cw20ExecuteMsg::IncreaseAllowance {
            spender: pair_addr.clone(),
            amount: seed_amt,
            expires: None,
        },
        &[],
        &admin,
    )
    .unwrap();
    wasm.execute(
        &pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: seed_amt,
                },
                Asset {
                    info: cw20_info.clone(),
                    amount: seed_amt,
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[Coin::new(seed_amt.u128(), DENOM_ATOM)],
        &admin,
    )
    .unwrap();

    // --- Farm + vault -------------------------------------------------------
    let now = app.get_block_time_seconds() as u64;
    let schedule_start = now + 60;
    let farm_addr = wasm
        .instantiate(
            farm_code,
            &FarmInstantiateMsg {
                reward_token: native(DENOM_ATOM),
                staking_token: native(&lp_denom),
                distribution_schedule: vec![(
                    schedule_start,
                    schedule_start + 3_600,
                    Uint128::new(10_000_000_000),
                )],
            },
            Some(&admin.address()),
            Some("Farm"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    wasm.execute(
        &farm_addr,
        &FarmExecuteMsg::Fund {},
        &[Coin::new(10_000_000_000u128, DENOM_ATOM)],
        &admin,
    )
    .unwrap();
    let vault_addr = wasm
        .instantiate(
            vault_code,
            &VaultInstantiateMsg {
                owner: admin.address(),
                pair_contract: pair_addr.clone(),
                farm_contract: farm_addr.clone(),
                lp_token: native(&lp_denom),
                reward_token: native(DENOM_ATOM),
                asset_infos: [native(DENOM_ATOM), cw20_info.clone()],
                fee_recipient: None,
                fee_percentage: None,
                minimum_reward_to_compound: Uint128::new(1_000_000_000),
                compounder: compounder.address(),
                slippage_tolerance: Decimal::percent(1),
                reward_to_lp_token_route: vec![],
            },
            Some(&admin.address()),
            Some("Vault"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- Admin sends seed LP to user so they can deposit --------------------
    // Bypasses the user's need to hold the CW20 — pair LP is always native,
    // so a bank send is sufficient.
    let user_lp = 100_000_000_000u128;
    use injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend;
    use injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin;
    Bank::new(&app)
        .send(
            MsgSend {
                from_address: admin.address(),
                to_address: user.address(),
                amount: vec![ProtoCoin {
                    denom: lp_denom.clone(),
                    amount: user_lp.to_string(),
                }],
            },
            &admin,
        )
        .unwrap();

    // Build a transient VaultEnv for reuse of helpers.
    let env = VaultEnv {
        app,
        admin,
        user,
        compounder,
        factory_addr,
        pair_addr,
        lp_denom,
        farm_addr,
        vault_addr,
    };
    let wasm = Wasm::new(&env.app);

    deposit_lp(&env, &env.user, Uint128::new(user_lp));
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    env.app.increase_time(600);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap();

    // Vault's CW20 (choicoin) balance should be ~zero after compound —
    // ProvideLiquidity consumes (nearly) all of it.
    let cw20_vault_balance: BalanceResponse = wasm
        .query(
            &cw20_addr,
            &Cw20QueryMsg::Balance {
                address: env.vault_addr.clone(),
            },
        )
        .unwrap();
    // Some dust from provide_liquidity's min-ratio rule may remain.
    assert!(
        cw20_vault_balance.balance < Uint128::new(10_000),
        "vault should not hoard CW20 after compound, got {}",
        cw20_vault_balance.balance
    );

    // Exit and verify compound gain on LP.
    let shares = user_info(&env, &env.user.address()).shares;
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: shares,
        },
        &[],
        &env.user,
    )
    .unwrap();
    let user_lp_after = bank_balance(&env, &env.user.address(), &env.lp_denom);
    assert!(
        user_lp_after > user_lp,
        "compound gain missing: post-exit LP {} vs deposited {}",
        user_lp_after,
        user_lp
    );
    assert!(total_shares(&env).is_zero(), "vault should be fully unwound");
}

/// CW20/CW20 pair with a CW20 reward token. Exercises the swap path where
/// the OFFER is a CW20 (vault uses `cw20.Send(pair, amount, Cw20HookMsg::Swap)`
/// instead of the native `PairExecuteMsg::Swap` with funds) and the
/// ProvideLiquidity leg with two CW20 allowances. Also the CW20 reward
/// harvest path (`cw20.Transfer` from farm to vault, not a bank send).
///
/// Layout:
///   pair:   choicoinA / choicoinB
///   reward: choicoinA (same as asset_infos[0] → empty route)
#[test]
fn cw20_cw20_deposit_activate_compound_withdraw() {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let initial = &[Coin::new(
        1_000_000_000_000_000_000_000_000u128,
        DENOM_INJ,
    )];
    let decimals = &[18u32];
    let custom_fee = FeeSetting::Custom {
        amount: Coin::new(1_000_000_000_000_000_000u128, DENOM_INJ),
        gas_limit: 50_000_000,
    };
    let admin = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let user = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee.clone());
    let compounder = app
        .init_account_decimals(initial, decimals)
        .unwrap()
        .with_fee_setting(custom_fee);

    // --- Codes --------------------------------------------------------------
    let pair_code = wasm
        .store_code(&get_wasm_byte_code("choice_pair.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let factory_code = wasm
        .store_code(&get_wasm_byte_code("choice_factory.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let farm_code = wasm
        .store_code(&get_wasm_byte_code("choice_farm.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let vault_code = wasm
        .store_code(&get_wasm_byte_code("choice_vault.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let auction_code = wasm
        .store_code(
            &get_wasm_byte_code("choice_send_to_auction.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;
    let cw20_code = wasm
        .store_code(&get_wasm_byte_code("cw20_base_build.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let cw20_adapter_code = wasm
        .store_code(&get_wasm_byte_code("cw20_adapter.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;

    // --- CW20 A and B -------------------------------------------------------
    let mk_cw20 = |name: &str, symbol: &str| -> String {
        wasm.instantiate(
            cw20_code,
            &Cw20InstantiateMsg {
                name: name.to_string(),
                symbol: symbol.to_string(),
                decimals: 6,
                initial_balances: vec![Cw20Coin {
                    address: admin.address(),
                    amount: Uint128::new(100_000_000_000_000),
                }],
                mint: None,
                marketing: None,
            },
            Some(&admin.address()),
            Some(name),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address
    };
    let cw20_a = mk_cw20("Choicoin A", "CHOIA");
    let cw20_b = mk_cw20("Choicoin B", "CHOIB");
    let info_a = AssetInfo::Token {
        contract_addr: cw20_a.clone(),
    };
    let info_b = AssetInfo::Token {
        contract_addr: cw20_b.clone(),
    };

    // --- Adapter + auction + factory ---------------------------------------
    #[derive(serde::Serialize)]
    struct EmptyInstantiate {}
    let cw20_adapter_addr = wasm
        .instantiate(
            cw20_adapter_code,
            &EmptyInstantiate {},
            Some(&admin.address()),
            Some("CW20 Adapter"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    use injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend as MsgSend1;
    use injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin1;
    Bank::new(&app)
        .send(
            MsgSend1 {
                from_address: admin.address(),
                to_address: cw20_adapter_addr.clone(),
                amount: vec![ProtoCoin1 {
                    denom: DENOM_INJ.to_string(),
                    amount: "200000000000000000000".to_string(), // 200 INJ (two denoms)
                }],
            },
            &admin,
        )
        .unwrap();

    let auction_addr = wasm
        .instantiate(
            auction_code,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: cw20_adapter_addr.clone(),
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
    let factory_addr = wasm
        .instantiate(
            factory_code,
            &FactoryInstantiateMsg {
                pair_code_id: pair_code,
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

    // --- Create A/B pair ---------------------------------------------------
    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePair {
            assets: [
                Asset {
                    info: info_a.clone(),
                    amount: Uint128::zero(),
                },
                Asset {
                    info: info_b.clone(),
                    amount: Uint128::zero(),
                },
            ],
        },
        &[Coin::new(10_000_000_000_000_000_000u128, DENOM_INJ)],
        &admin,
    )
    .unwrap();
    let pair_info: PairInfo = wasm
        .query(
            &factory_addr,
            &choice::factory::QueryMsg::Pair {
                asset_infos: [info_a.clone(), info_b.clone()],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();

    // --- Seed pair (admin allows pair to pull both CW20s) ------------------
    let seed_amt = Uint128::new(10_000_000_000_000); // 1e13 each
    for cw20_addr in [&cw20_a, &cw20_b] {
        wasm.execute(
            cw20_addr,
            &Cw20ExecuteMsg::IncreaseAllowance {
                spender: pair_addr.clone(),
                amount: seed_amt,
                expires: None,
            },
            &[],
            &admin,
        )
        .unwrap();
    }
    wasm.execute(
        &pair_addr,
        &PairExecuteMsg::ProvideLiquidity {
            assets: [
                Asset {
                    info: info_a.clone(),
                    amount: seed_amt,
                },
                Asset {
                    info: info_b.clone(),
                    amount: seed_amt,
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[],
        &admin,
    )
    .unwrap();

    // --- Farm (CW20 reward) + vault ----------------------------------------
    // Farm reward is CW20 A. Funding uses `cw20.Send(farm, amount, Cw20HookMsg::Fund)`.
    let now = app.get_block_time_seconds() as u64;
    let schedule_start = now + 120; // extra slack for heavier CW20 setup
    let farm_addr = wasm
        .instantiate(
            farm_code,
            &FarmInstantiateMsg {
                reward_token: info_a.clone(),
                staking_token: native(&lp_denom),
                distribution_schedule: vec![(
                    schedule_start,
                    schedule_start + 3_600,
                    Uint128::new(10_000_000_000),
                )],
            },
            Some(&admin.address()),
            Some("Farm"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;
    wasm.execute(
        &cw20_a,
        &Cw20ExecuteMsg::Send {
            contract: farm_addr.clone(),
            amount: Uint128::new(10_000_000_000),
            msg: cosmwasm_std::to_json_binary(&choice::staking::Cw20HookMsg::Fund {}).unwrap(),
        },
        &[],
        &admin,
    )
    .unwrap();
    let vault_addr = wasm
        .instantiate(
            vault_code,
            &VaultInstantiateMsg {
                owner: admin.address(),
                pair_contract: pair_addr.clone(),
                farm_contract: farm_addr.clone(),
                lp_token: native(&lp_denom),
                reward_token: info_a.clone(),
                asset_infos: [info_a.clone(), info_b.clone()],
                fee_recipient: None,
                fee_percentage: None,
                minimum_reward_to_compound: Uint128::new(1_000_000_000),
                compounder: compounder.address(),
                slippage_tolerance: Decimal::percent(1),
                reward_to_lp_token_route: vec![],
            },
            Some(&admin.address()),
            Some("Vault"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    // --- Admin ships seed LP to user (bank send — LP is native) ------------
    let user_lp = 100_000_000_000u128;
    Bank::new(&app)
        .send(
            MsgSend1 {
                from_address: admin.address(),
                to_address: user.address(),
                amount: vec![ProtoCoin1 {
                    denom: lp_denom.clone(),
                    amount: user_lp.to_string(),
                }],
            },
            &admin,
        )
        .unwrap();

    let env = VaultEnv {
        app,
        admin,
        user,
        compounder,
        factory_addr,
        pair_addr,
        lp_denom,
        farm_addr,
        vault_addr,
    };
    let wasm = Wasm::new(&env.app);

    deposit_lp(&env, &env.user, Uint128::new(user_lp));
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    env.app.increase_time(600);

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap();

    // Vault should not hoard either CW20 after compound — the swap + provide
    // drain the harvested reward into new LP. Some dust is OK.
    for (label, cw20_addr) in [("A", &cw20_a), ("B", &cw20_b)] {
        let resp: BalanceResponse = wasm
            .query(
                cw20_addr,
                &Cw20QueryMsg::Balance {
                    address: env.vault_addr.clone(),
                },
            )
            .unwrap();
        assert!(
            resp.balance < Uint128::new(10_000),
            "vault should not hoard CW20 {} after compound, got {}",
            label,
            resp.balance
        );
    }

    // Exit. WithdrawShares reply chain distributes the CW20 reward slice via
    // cw20.Transfer (not a bank send) since reward_token is Token.
    let shares = user_info(&env, &env.user.address()).shares;
    let user_cw20_a_before: BalanceResponse = wasm
        .query(
            &cw20_a,
            &Cw20QueryMsg::Balance {
                address: env.user.address(),
            },
        )
        .unwrap();

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: shares,
        },
        &[],
        &env.user,
    )
    .unwrap();

    // Compound gain reflected in LP back to user.
    let user_lp_after = bank_balance(&env, &env.user.address(), &env.lp_denom);
    assert!(
        user_lp_after > user_lp,
        "compound gain missing: post-exit LP {} vs deposited {}",
        user_lp_after,
        user_lp
    );

    // User's CW20 A balance should not have decreased (they never spent any);
    // if the exit happens to carry any unharvested farm credits, it grows.
    let user_cw20_a_after: BalanceResponse = wasm
        .query(
            &cw20_a,
            &Cw20QueryMsg::Balance {
                address: env.user.address(),
            },
        )
        .unwrap();
    assert!(
        user_cw20_a_after.balance >= user_cw20_a_before.balance,
        "CW20 reward slice should not go backwards on exit"
    );

    assert!(total_shares(&env).is_zero(), "vault should be fully unwound");
}

/// Farm owner triggers the timelocked `MigrateStaking` flow while the vault
/// is live. The old farm retains staker bonds and the reward tokens backing
/// already-credited rewards — only `undistributed_rewards` move — so vault
/// users can still exit with their LP plus the pre-migration reward slice.
#[test]
fn migrate_staking_does_not_strand_vault_users() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    deposit_lp(&env, &env.user, lp_amount);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    env.app.increase_time(600);

    // Farm owner proposes migration. Destination doesn't have to be a contract
    // — we're only testing the old farm's side of the contract, and the
    // destination receives a vanilla bank send for native reward tokens.
    wasm.execute(
        &env.farm_addr,
        &FarmExecuteMsg::ProposeMigrateStaking {
            new_staking_contract: env.admin.address(),
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // Applying before the 48h timelock elapses must fail.
    let early = wasm.execute(
        &env.farm_addr,
        &FarmExecuteMsg::ApplyMigrateStaking {},
        &[],
        &env.admin,
    );
    assert!(early.is_err(), "apply before timelock must reject");

    // Wait out the timelock (48h + 1s) and apply.
    env.app.increase_time(48 * 60 * 60 + 1);
    wasm.execute(
        &env.farm_addr,
        &FarmExecuteMsg::ApplyMigrateStaking {},
        &[],
        &env.admin,
    )
    .unwrap();

    // Bond is untouched — migrate_staking only forwards `undistributed_rewards`.
    assert_eq!(
        farm_bond(&env, &env.vault_addr),
        lp_amount,
        "vault bond must survive farm migration"
    );

    let user_lp_before = bank_balance(&env, &env.user.address(), &env.lp_denom);
    let user_atom_before = bank_balance(&env, &env.user.address(), DENOM_ATOM);

    let shares = user_info(&env, &env.user.address()).shares;
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::WithdrawShares {
            shares_to_burn: shares,
        },
        &[],
        &env.user,
    )
    .unwrap();

    // User receives LP + their proportional slice of the pre-migration reward
    // credit. The sole shareholder gets all of it.
    assert!(
        bank_balance(&env, &env.user.address(), &env.lp_denom) > user_lp_before,
        "LP not returned on post-migration exit"
    );
    assert!(
        bank_balance(&env, &env.user.address(), DENOM_ATOM) > user_atom_before,
        "pre-migration rewards not paid out on exit"
    );
    assert!(total_shares(&env).is_zero(), "vault should be fully unwound");
}

/// L-15: a deposit too small to mint any shares (because share price has
/// moved) triggers an auto-refund on `ActivateMyDeposit`. The dust LP comes
/// back to the user; their pending_deposit is cleared; total_shares is
/// unchanged.
#[test]
fn activate_my_deposit_refunds_dust() {
    let env = setup_native_native();
    let wasm = Wasm::new(&env.app);

    // First user establishes the share base.
    let lp_amount = provide_liquidity(&env, &env.user, 100_000_000_000, 100_000_000_000);
    deposit_lp(&env, &env.user, lp_amount);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivatePendingDeposits {
            users: vec![env.user.address()],
        },
        &[],
        &env.compounder,
    )
    .unwrap();

    // Compound bumps share price above 1 — now 1 LP rounds to 0 shares.
    env.app.increase_time(600);
    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::Compound {
            belief_prices: vec![Decimal::one()],
            minimum_lp_to_receive: Uint128::new(100_000_000),
        },
        &[],
        &env.user,
    )
    .unwrap();

    // Admin already holds LP from seeding the pair — use them as the dust
    // depositor. Snapshot balance, deposit 1 LP, activate, expect the same
    // balance back.
    let admin_lp_before = bank_balance(&env, &env.admin.address(), &env.lp_denom);
    let dust = Uint128::new(1);
    deposit_lp(&env, &env.admin, dust);

    let shares_before_activate = total_shares(&env);

    wasm.execute(
        &env.vault_addr,
        &VaultExecuteMsg::ActivateMyDeposit {},
        &[],
        &env.admin,
    )
    .unwrap();

    // Auto-refund path: admin's pending cleared, no new shares minted,
    // LP balance restored to the pre-deposit level.
    let admin_info = user_info(&env, &env.admin.address());
    assert!(
        admin_info.pending_deposit.is_zero(),
        "dust pending should clear"
    );
    assert!(admin_info.shares.is_zero(), "dust must not mint shares");
    assert_eq!(
        total_shares(&env),
        shares_before_activate,
        "total_shares must not move on dust refund"
    );
    assert_eq!(
        bank_balance(&env, &env.admin.address(), &env.lp_denom),
        admin_lp_before,
        "admin LP must be fully refunded"
    );
}

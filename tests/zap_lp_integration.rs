#![cfg(test)]
//! Integration tests for `choice_zap_lp` against a real `choice_pair`.
//!
//! Requires compiled WASM in `choice_exchange/artifacts/` — run
//! `./build_release.sh` (or per-contract `make build-zap-lp` + the workspace
//! optimizer for `cw20_base_build` / `cw20_adapter`) before
//! `cargo test --test zap_lp_integration`.
//!
//! Coverage matrix:
//!   * `Zap` happy path against a freshly-seeded native/native pair — LP
//!     minted, deltas swept to recipient.
//!   * Snapshot isolation: a user `Zap` does **not** drain pre-existing
//!     contract balances of either pair denom or of the LP token.
//!   * `ZapBalance` round-trip including the keeper `tip_bps` BankMsg.
//!   * Near-empty-pool zap where the swap delta lands at `1` wei — the
//!     M-01 fix skips ProvideLiquidity and sweeps the deltas back.
//!   * `SimulateZap` matches the pair's `Simulation` query wei-for-wei.
//!   * Native input → native/CW20 pair: CW20 dust returned via `Cw20::Transfer`.
//!   * CW20 input → native/CW20 pair via `cw20::Send` + `Receive`: allowance
//!     dance, LP minted, native dust returned via Bank.
//!   * CW20 input → CW20/CW20 pair: both sides go through allowance, both
//!     dusts returned via `Cw20::Transfer`.
//!   * `Receive` auth: a foreign CW20 contract whose token isn't in the pair
//!     is rejected with `InputAssetMismatch`.
//!   * `ZapBalance` for a CW20 royalty stream: tip via `Cw20::Transfer`, LP
//!     to `default_recipient`.

use cosmwasm_std::{Coin, Decimal, Uint128};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice::asset::{Asset, AssetInfo, PairInfo};
use choice::factory::{ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg};
use choice::pair::ExecuteMsg as PairExecuteMsg;
use cw20::{BalanceResponse, Cw20Coin, Cw20ExecuteMsg, Cw20QueryMsg};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;

use choice_zap_lp::msg::{
    ExecuteMsg as ZapExecuteMsg, InstantiateMsg as ZapInstantiateMsg, QueryMsg as ZapQueryMsg,
    SimulateZapResponse, ZapHookMsg,
};

const DENOM_INJ: &str = "inj";
const DENOM_ATOM: &str = "atom";
const DENOM_USDT: &str = "usdt";

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

#[allow(dead_code)]
struct ZapEnv {
    app: InjectiveTestApp,
    admin: SigningAccount,
    user: SigningAccount,
    keeper: SigningAccount,
    treasury: SigningAccount,
    pair_addr: String,
    pair_assets: [AssetInfo; 2],
    lp_denom: String,
    zap_addr: String,
}

/// Bootstrap atom/usdt native-only pair seeded with `seed_amt` per side, then
/// deploy the zap with `treasury` as the default recipient and a 25-bp tip.
fn setup_native_pair(seed_amt: u128) -> ZapEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let (admin, user, keeper, treasury) = init_four_accounts(&app);

    let pair_code_id = store(&wasm, &admin, "choice_pair.wasm");
    let factory_code_id = store(&wasm, &admin, "choice_factory.wasm");
    let auction_code_id = store(&wasm, &admin, "choice_send_to_auction.wasm");
    let zap_code_id = store(&wasm, &admin, "choice_zap_lp.wasm");

    let auction_addr = instantiate_native_only_auction(&wasm, &admin, auction_code_id);

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
    let pair_assets = pair_info.asset_infos.clone();

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

    let zap_addr = instantiate_zap(
        &wasm,
        &admin,
        zap_code_id,
        Some(treasury.address()),
        Some(25),
        Some(1_000_000),
        native(DENOM_ATOM),
        pair_addr.clone(),
    );
    wasm.execute(
        &zap_addr,
        &ZapExecuteMsg::AddKeeper {
            address: keeper.address(),
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
        pair_assets,
        lp_denom,
        zap_addr,
    }
}

/// atom/CW20 pair (a native side + a CW20 side). The CW20 is also given
/// to the user so they can drive the `Receive` path.
fn setup_native_cw20_pair(seed_amt: u128) -> (ZapEnv, String) {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let (admin, user, keeper, treasury) = init_four_accounts(&app);

    let pair_code_id = store(&wasm, &admin, "choice_pair.wasm");
    let factory_code_id = store(&wasm, &admin, "choice_factory.wasm");
    let auction_code_id = store(&wasm, &admin, "choice_send_to_auction.wasm");
    let zap_code_id = store(&wasm, &admin, "choice_zap_lp.wasm");
    let cw20_code_id = store(&wasm, &admin, "cw20_base_build.wasm");
    let adapter_code_id = store(&wasm, &admin, "cw20_adapter.wasm");

    let adapter_addr = instantiate_cw20_adapter(&app, &wasm, &admin, adapter_code_id, 1);
    let cw20_addr = instantiate_cw20(
        &wasm,
        &admin,
        cw20_code_id,
        "Choicoin",
        "CHOI",
        Uint128::new(100_000_000_000_000),
    );

    // Distribute some CW20 to user so they can call cw20.Send (Receive path).
    wasm.execute(
        &cw20_addr,
        &Cw20ExecuteMsg::Transfer {
            recipient: user.address(),
            amount: Uint128::new(10_000_000_000_000),
        },
        &[],
        &admin,
    )
    .unwrap();

    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: adapter_addr.clone(),
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

    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePair {
            assets: [
                Asset {
                    info: native(DENOM_ATOM),
                    amount: Uint128::zero(),
                },
                Asset {
                    info: cw20(&cw20_addr),
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
                asset_infos: [native(DENOM_ATOM), cw20(&cw20_addr)],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();
    let pair_assets = pair_info.asset_infos.clone();

    // Seed the pair: admin allows pair to TransferFrom the CW20 side.
    let seed = Uint128::new(seed_amt);
    wasm.execute(
        &cw20_addr,
        &Cw20ExecuteMsg::IncreaseAllowance {
            spender: pair_addr.clone(),
            amount: seed,
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
                    amount: seed,
                },
                Asset {
                    info: cw20(&cw20_addr),
                    amount: seed,
                },
            ],
            receiver: None,
            deadline: None,
            slippage_tolerance: None,
        },
        &[Coin::new(seed.u128(), DENOM_ATOM)],
        &admin,
    )
    .unwrap();

    // v2 is one-route-per-contract; this setup is consumed by the CW20-input
    // ZapBalance test and by user-facing tests that pass `pair` per-call.
    let zap_addr = instantiate_zap(
        &wasm,
        &admin,
        zap_code_id,
        Some(treasury.address()),
        Some(25),
        Some(1_000_000),
        cw20(&cw20_addr),
        pair_addr.clone(),
    );
    wasm.execute(
        &zap_addr,
        &ZapExecuteMsg::AddKeeper {
            address: keeper.address(),
        },
        &[],
        &admin,
    )
    .unwrap();

    let env = ZapEnv {
        app,
        admin,
        user,
        keeper,
        treasury,
        pair_addr,
        pair_assets,
        lp_denom,
        zap_addr,
    };
    (env, cw20_addr)
}

/// CW20/CW20 pair. Both sides are CW20 tokens; the user receives both for
/// the `Receive` path.
fn setup_cw20_cw20_pair(seed_amt: u128) -> (ZapEnv, String, String) {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let (admin, user, keeper, treasury) = init_four_accounts(&app);

    let pair_code_id = store(&wasm, &admin, "choice_pair.wasm");
    let factory_code_id = store(&wasm, &admin, "choice_factory.wasm");
    let auction_code_id = store(&wasm, &admin, "choice_send_to_auction.wasm");
    let zap_code_id = store(&wasm, &admin, "choice_zap_lp.wasm");
    let cw20_code_id = store(&wasm, &admin, "cw20_base_build.wasm");
    let adapter_code_id = store(&wasm, &admin, "cw20_adapter.wasm");

    // Two tokens → adapter has to register two denoms → 2 INJ creation fees.
    let adapter_addr = instantiate_cw20_adapter(&app, &wasm, &admin, adapter_code_id, 2);

    let cw20_a = instantiate_cw20(
        &wasm,
        &admin,
        cw20_code_id,
        "Choicoin A",
        "CHOIA",
        Uint128::new(100_000_000_000_000),
    );
    let cw20_b = instantiate_cw20(
        &wasm,
        &admin,
        cw20_code_id,
        "Choicoin B",
        "CHOIB",
        Uint128::new(100_000_000_000_000),
    );
    // Hand the user some of each so they can drive both legs of the Receive path.
    for cw20_addr in [&cw20_a, &cw20_b] {
        wasm.execute(
            cw20_addr,
            &Cw20ExecuteMsg::Transfer {
                recipient: user.address(),
                amount: Uint128::new(10_000_000_000_000),
            },
            &[],
            &admin,
        )
        .unwrap();
    }

    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: admin.address(),
                adapter_contract: adapter_addr.clone(),
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

    wasm.execute(
        &factory_addr,
        &FactoryExecuteMsg::CreatePair {
            assets: [
                Asset {
                    info: cw20(&cw20_a),
                    amount: Uint128::zero(),
                },
                Asset {
                    info: cw20(&cw20_b),
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
                asset_infos: [cw20(&cw20_a), cw20(&cw20_b)],
            },
        )
        .unwrap();
    let pair_addr = pair_info.contract_addr.clone();
    let lp_denom = pair_info.liquidity_token.clone();
    let pair_assets = pair_info.asset_infos.clone();

    let seed = Uint128::new(seed_amt);
    for cw20_addr in [&cw20_a, &cw20_b] {
        wasm.execute(
            cw20_addr,
            &Cw20ExecuteMsg::IncreaseAllowance {
                spender: pair_addr.clone(),
                amount: seed,
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
                    info: cw20(&cw20_a),
                    amount: seed,
                },
                Asset {
                    info: cw20(&cw20_b),
                    amount: seed,
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

    // No ZapBalance tests run against this setup; the pinned input is arbitrary.
    let zap_addr = instantiate_zap(
        &wasm,
        &admin,
        zap_code_id,
        Some(treasury.address()),
        Some(25),
        Some(1_000_000),
        cw20(&cw20_a),
        pair_addr.clone(),
    );
    wasm.execute(
        &zap_addr,
        &ZapExecuteMsg::AddKeeper {
            address: keeper.address(),
        },
        &[],
        &admin,
    )
    .unwrap();

    let env = ZapEnv {
        app,
        admin,
        user,
        keeper,
        treasury,
        pair_addr,
        pair_assets,
        lp_denom,
        zap_addr,
    };
    (env, cw20_a, cw20_b)
}

// ---------------------------------------------------------------------------
// Shared bootstrap helpers
// ---------------------------------------------------------------------------

fn init_four_accounts(
    app: &InjectiveTestApp,
) -> (SigningAccount, SigningAccount, SigningAccount, SigningAccount) {
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
    (admin, user, keeper, treasury)
}

fn store(wasm: &Wasm<InjectiveTestApp>, admin: &SigningAccount, filename: &str) -> u64 {
    wasm.store_code(&get_wasm_byte_code(filename), None, admin)
        .unwrap()
        .data
        .code_id
}

#[allow(clippy::too_many_arguments)]
fn instantiate_zap(
    wasm: &Wasm<InjectiveTestApp>,
    admin: &SigningAccount,
    code_id: u64,
    default_recipient: Option<String>,
    tip_bps: Option<u16>,
    min_zap_amount: Option<u128>,
    input: AssetInfo,
    pair: String,
) -> String {
    wasm.instantiate(
        code_id,
        &ZapInstantiateMsg {
            owner: Some(admin.address()),
            default_recipient,
            tip_bps,
            min_zap_amount: min_zap_amount.map(Uint128::new),
            input,
            pair,
        },
        Some(&admin.address()),
        Some("Zap"),
        &[],
        admin,
    )
    .unwrap()
    .data
    .address
}

fn instantiate_native_only_auction(
    wasm: &Wasm<InjectiveTestApp>,
    admin: &SigningAccount,
    code_id: u64,
) -> String {
    // Native-only pairs never call the adapter; passing the admin keeps the
    // schema validation happy.
    wasm.instantiate(
        code_id,
        &choice::send_to_auction::InstantiateMsg {
            owner: admin.address(),
            adapter_contract: admin.address(),
            burn_auction_subaccount:
                "0x1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        },
        Some(&admin.address()),
        Some("Auction"),
        &[],
        admin,
    )
    .unwrap()
    .data
    .address
}

fn instantiate_cw20_adapter(
    app: &InjectiveTestApp,
    wasm: &Wasm<InjectiveTestApp>,
    admin: &SigningAccount,
    code_id: u64,
    tokens_to_register: u8,
) -> String {
    #[derive(serde::Serialize)]
    struct EmptyInstantiate {}
    let addr = wasm
        .instantiate(
            code_id,
            &EmptyInstantiate {},
            Some(&admin.address()),
            Some("CW20 Adapter"),
            &[],
            admin,
        )
        .unwrap()
        .data
        .address;
    // Fund the adapter for the token-factory denom-create fee (1 INJ per
    // distinct CW20 the auction ever touches).
    use injective_test_tube::injective_std::types::cosmos::bank::v1beta1::MsgSend;
    use injective_test_tube::injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin;
    let amount = format!("{}", 100u128 * 10u128.pow(18) * tokens_to_register as u128);
    Bank::new(app)
        .send(
            MsgSend {
                from_address: admin.address(),
                to_address: addr.clone(),
                amount: vec![ProtoCoin {
                    denom: DENOM_INJ.to_string(),
                    amount,
                }],
            },
            admin,
        )
        .unwrap();
    addr
}

fn instantiate_cw20(
    wasm: &Wasm<InjectiveTestApp>,
    admin: &SigningAccount,
    code_id: u64,
    name: &str,
    symbol: &str,
    initial_balance: Uint128,
) -> String {
    wasm.instantiate(
        code_id,
        &Cw20InstantiateMsg {
            name: name.to_string(),
            symbol: symbol.to_string(),
            decimals: 6,
            initial_balances: vec![Cw20Coin {
                address: admin.address(),
                amount: initial_balance,
            }],
            mint: None,
            marketing: None,
        },
        Some(&admin.address()),
        Some(name),
        &[],
        admin,
    )
    .unwrap()
    .data
    .address
}

fn bank_balance(app: &InjectiveTestApp, addr: &str, denom: &str) -> u128 {
    Bank::new(app)
        .query_balance(&QueryBalanceRequest {
            address: addr.to_string(),
            denom: denom.to_string(),
        })
        .unwrap()
        .balance
        .map(|b| b.amount.parse::<u128>().unwrap())
        .unwrap_or(0)
}

fn bal(env: &ZapEnv, addr: &str, denom: &str) -> u128 {
    bank_balance(&env.app, addr, denom)
}

fn cw20_balance(env: &ZapEnv, token: &str, owner: &str) -> u128 {
    let wasm = Wasm::new(&env.app);
    let resp: BalanceResponse = wasm
        .query(
            token,
            &Cw20QueryMsg::Balance {
                address: owner.to_string(),
            },
        )
        .unwrap();
    resp.balance.u128()
}

// ---------------------------------------------------------------------------
// Native/native tests (the original suite — kept verbatim modulo new schema)
// ---------------------------------------------------------------------------

#[test]
fn zap_happy_path_user_receives_lp() {
    let env = setup_native_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let input_amt = 1_000_000_000u128;

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

    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(bal(&env, &env.zap_addr, DENOM_USDT) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

#[test]
fn zap_snapshot_isolation_does_not_drain_preexisting_balance() {
    let env = setup_native_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

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

#[test]
fn zap_balance_pays_tip_and_lps_to_default_recipient() {
    let env = setup_native_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let royalty = 1_000_000_000u128;
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

    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(bal(&env, &env.zap_addr, DENOM_USDT) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

#[test]
fn zap_skips_provide_when_swap_delta_too_small() {
    let env = setup_native_pair(10_000_000_000_000u128);
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
                max_spread: Some(Decimal::one()),
                slippage_tolerance: Some(Decimal::percent(99)),
                min_lp_out: None,
                deadline: None,
            },
            &[Coin::new(tiny, DENOM_ATOM)],
            &env.user,
        )
        .expect("zap should not panic on tiny input — M-01 skip should fire");

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

    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    let user_atom_after = bal(&env, &env.user.address(), DENOM_ATOM);
    let user_usdt_after = bal(&env, &env.user.address(), DENOM_USDT);
    assert_eq!(user_lp_after, user_lp_before, "no LP should have been minted");
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

#[test]
fn simulate_zap_matches_pair_simulation_query() {
    let env = setup_native_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let input_amt = Uint128::new(1_000_000_000);
    let sim: SimulateZapResponse = wasm
        .query(
            &env.zap_addr,
            &ZapQueryMsg::SimulateZap {
                pair: env.pair_addr.clone(),
                input: native(DENOM_ATOM),
                input_amount: input_amt,
            },
        )
        .unwrap();

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

// ---------------------------------------------------------------------------
// Native/CW20 pair
// ---------------------------------------------------------------------------

/// Native input into a native/CW20 pair: swap atom → CW20, provide both. CW20
/// dust must come back via `Cw20::Transfer` (LP + native dust via Bank).
#[test]
fn zap_native_input_into_native_cw20_pair() {
    let (env, cw20_addr) = setup_native_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let user_cw20_before = cw20_balance(&env, &cw20_addr, &env.user.address());

    let input_amt = 1_000_000_000u128;
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
    let user_cw20_after = cw20_balance(&env, &cw20_addr, &env.user.address());

    assert!(
        user_lp_after > user_lp_before,
        "user should have received LP"
    );
    // CW20 dust can be 0 (exact match) or a tiny positive value — never negative.
    assert!(
        user_cw20_after >= user_cw20_before,
        "user CW20 balance must not decrease (input was native)"
    );

    // Zap contract retains only the 1-wei haircut residuals.
    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(cw20_balance(&env, &cw20_addr, &env.zap_addr) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

/// CW20 input via `cw20::Send` → `Receive` hook. Swap CW20 → atom, then
/// provide both. Native dust + LP returns via Bank; the CW20 side feeds the
/// pair via `IncreaseAllowance` + the pair's `TransferFrom`.
#[test]
fn zap_cw20_input_via_receive_into_native_cw20_pair() {
    let (env, cw20_addr) = setup_native_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let user_atom_before = bal(&env, &env.user.address(), DENOM_ATOM);

    let input_amt = Uint128::new(1_000_000_000);
    let hook = cosmwasm_std::to_json_binary(&ZapHookMsg::Zap {
        pair: env.pair_addr.clone(),
        recipient: None,
        max_spread: Some(Decimal::permille(5)),
        slippage_tolerance: Some(Decimal::percent(1)),
        min_lp_out: Some(Uint128::new(1)),
        deadline: None,
    })
    .unwrap();
    wasm.execute(
        &cw20_addr,
        &Cw20ExecuteMsg::Send {
            contract: env.zap_addr.clone(),
            amount: input_amt,
            msg: hook,
        },
        &[],
        &env.user,
    )
    .unwrap();

    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    let user_atom_after = bal(&env, &env.user.address(), DENOM_ATOM);

    assert!(
        user_lp_after > user_lp_before,
        "user should have received LP"
    );
    // Native dust comes back — user atom balance must not decrease (they
    // didn't put any in).
    assert!(
        user_atom_after >= user_atom_before,
        "user atom balance must not decrease (input was CW20)"
    );

    // Contract holds at most 1 wei of either side and no LP.
    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert!(cw20_balance(&env, &cw20_addr, &env.zap_addr) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

/// A "rogue" CW20 contract (not in the target pair) tries to drive the
/// `Receive` hook by sending its tokens. The auth check in `orient_assets`
/// must reject because that contract's address isn't a side of the pair.
#[test]
fn receive_rejects_cw20_contract_not_in_pair() {
    let (env, _cw20_addr) = setup_native_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let rogue_code = store(&wasm, &env.admin, "cw20_base_build.wasm");
    let rogue = instantiate_cw20(
        &wasm,
        &env.admin,
        rogue_code,
        "Rogue",
        "ROG",
        Uint128::new(1_000_000_000_000),
    );
    // Give the user some rogue tokens.
    wasm.execute(
        &rogue,
        &Cw20ExecuteMsg::Transfer {
            recipient: env.user.address(),
            amount: Uint128::new(1_000_000_000),
        },
        &[],
        &env.admin,
    )
    .unwrap();

    let hook = cosmwasm_std::to_json_binary(&ZapHookMsg::Zap {
        pair: env.pair_addr.clone(),
        recipient: None,
        max_spread: Some(Decimal::permille(5)),
        slippage_tolerance: Some(Decimal::percent(1)),
        min_lp_out: None,
        deadline: None,
    })
    .unwrap();

    let err = wasm
        .execute(
            &rogue,
            &Cw20ExecuteMsg::Send {
                contract: env.zap_addr.clone(),
                amount: Uint128::new(1_000_000_000),
                msg: hook,
            },
            &[],
            &env.user,
        )
        .unwrap_err();
    let err_str = format!("{:?}", err);
    assert!(
        err_str.contains("InputAssetMismatch") || err_str.contains("does not match"),
        "expected InputAssetMismatch, got: {}",
        err_str
    );
}

/// `ZapBalance` for a CW20 royalty stream — same drain semantics as the
/// native path, but the tip routes through `Cw20::Transfer`.
#[test]
fn zap_balance_cw20_pays_tip_via_cw20_transfer() {
    let (env, cw20_addr) = setup_native_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let royalty = Uint128::new(1_000_000_000);
    // Royalty source: admin pushes via Cw20::Transfer (analogue of MsgSend).
    wasm.execute(
        &cw20_addr,
        &Cw20ExecuteMsg::Transfer {
            recipient: env.zap_addr.clone(),
            amount: royalty,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    let keeper_cw20_before = cw20_balance(&env, &cw20_addr, &env.keeper.address());
    let treasury_lp_before = bal(&env, &env.treasury.address(), &env.lp_denom);

    wasm.execute(
        &env.zap_addr,
        &ZapExecuteMsg::ZapBalance {
            max_spread: Some(Decimal::permille(5)),
            slippage_tolerance: Some(Decimal::percent(1)),
            min_lp_out: Some(Uint128::new(1)),
            deadline: None,
        },
        &[],
        &env.keeper,
    )
    .unwrap();

    let keeper_cw20_after = cw20_balance(&env, &cw20_addr, &env.keeper.address());
    let treasury_lp_after = bal(&env, &env.treasury.address(), &env.lp_denom);

    let expected_tip = royalty.u128() * 25 / 10_000;
    assert_eq!(
        keeper_cw20_after - keeper_cw20_before,
        expected_tip,
        "keeper tip should equal tip_bps of CW20 royalty"
    );
    assert!(
        treasury_lp_after > treasury_lp_before,
        "treasury should have received LP"
    );

    // Drain semantics on a CW20 royalty path.
    assert!(cw20_balance(&env, &cw20_addr, &env.zap_addr) <= 1);
    assert!(bal(&env, &env.zap_addr, DENOM_ATOM) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);
}

// ---------------------------------------------------------------------------
// CW20/CW20 pair
// ---------------------------------------------------------------------------

/// Both sides are CW20. User sends CW20-A in; contract swaps half to CW20-B
/// via `cw20::Send` (Swap hook), then provides both via dual `IncreaseAllowance`
/// + `ProvideLiquidity`. Both dusts return as `Cw20::Transfer`.
#[test]
fn zap_cw20_input_into_cw20_cw20_pair() {
    let (env, cw20_a, cw20_b) = setup_cw20_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let user_lp_before = bal(&env, &env.user.address(), &env.lp_denom);
    let user_a_before = cw20_balance(&env, &cw20_a, &env.user.address());
    let user_b_before = cw20_balance(&env, &cw20_b, &env.user.address());

    let input_amt = Uint128::new(1_000_000_000);
    let hook = cosmwasm_std::to_json_binary(&ZapHookMsg::Zap {
        pair: env.pair_addr.clone(),
        recipient: None,
        max_spread: Some(Decimal::permille(5)),
        slippage_tolerance: Some(Decimal::percent(1)),
        min_lp_out: Some(Uint128::new(1)),
        deadline: None,
    })
    .unwrap();
    wasm.execute(
        &cw20_a,
        &Cw20ExecuteMsg::Send {
            contract: env.zap_addr.clone(),
            amount: input_amt,
            msg: hook,
        },
        &[],
        &env.user,
    )
    .unwrap();

    let user_lp_after = bal(&env, &env.user.address(), &env.lp_denom);
    let user_a_after = cw20_balance(&env, &cw20_a, &env.user.address());
    let user_b_after = cw20_balance(&env, &cw20_b, &env.user.address());

    assert!(
        user_lp_after > user_lp_before,
        "user should have received LP"
    );
    // The user spent exactly `input_amt` of CW20-A; can get back at most
    // ~half of it net (the swap took its share). Their A balance must drop by
    // close to `input_amt`. Their B balance must stay flat or grow by a few wei.
    assert!(
        user_a_after < user_a_before,
        "user CW20-A should have decreased: before={}, after={}",
        user_a_before,
        user_a_after
    );
    assert!(
        (user_a_before - user_a_after) >= input_amt.u128() / 2,
        "user CW20-A drop should be at least half of input"
    );
    assert!(
        user_b_after >= user_b_before,
        "user CW20-B should not have decreased (input was A)"
    );

    // Pair LP minted; zap contract holds at most 1 wei of each side and no LP.
    assert!(cw20_balance(&env, &cw20_a, &env.zap_addr) <= 1);
    assert!(cw20_balance(&env, &cw20_b, &env.zap_addr) <= 1);
    assert_eq!(bal(&env, &env.zap_addr, &env.lp_denom), 0);

    // Symmetry: pair_assets reflects both CW20s.
    let asset_strs: Vec<String> = env.pair_assets.iter().map(|a| a.to_string()).collect();
    assert!(asset_strs.iter().any(|s| s == &cw20_a));
    assert!(asset_strs.iter().any(|s| s == &cw20_b));
}

/// Allowance hygiene: after a CW20-input zap, any residual allowance the
/// pair has on the zap contract must carry an `AtHeight(h+1)` expiration
/// stamped at the block of the zap. Residual amount can be a few wei (the
/// pair pulls `desired_amount` from the limiting side and leaves rounding
/// slop on the other) — what matters is that the record expires immediately
/// and cannot be reused on a later block.
#[test]
fn cw20_provide_allowance_expires_next_block() {
    let (env, cw20_a, _cw20_b) = setup_cw20_cw20_pair(10_000_000_000_000u128);
    let wasm = Wasm::new(&env.app);

    let zap_block = env.app.get_block_height() as u64;

    let input_amt = Uint128::new(1_000_000_000);
    let hook = cosmwasm_std::to_json_binary(&ZapHookMsg::Zap {
        pair: env.pair_addr.clone(),
        recipient: None,
        max_spread: Some(Decimal::permille(5)),
        slippage_tolerance: Some(Decimal::percent(1)),
        min_lp_out: Some(Uint128::new(1)),
        deadline: None,
    })
    .unwrap();
    wasm.execute(
        &cw20_a,
        &Cw20ExecuteMsg::Send {
            contract: env.zap_addr.clone(),
            amount: input_amt,
            msg: hook,
        },
        &[],
        &env.user,
    )
    .unwrap();
    let post_block = env.app.get_block_height() as u64;

    let resp: cw20::AllowanceResponse = wasm
        .query(
            &cw20_a,
            &cw20::Cw20QueryMsg::Allowance {
                owner: env.zap_addr.clone(),
                spender: env.pair_addr.clone(),
            },
        )
        .unwrap();

    // The expiration must be AtHeight at-or-before (post_block + 1) — i.e.
    // stamped at the block where the IncreaseAllowance ran, with the
    // contract's `+1` offset. cw20::Expiration is re-exported from cw_utils.
    match resp.expires {
        cw20::Expiration::AtHeight(h) => {
            assert!(
                h <= post_block + 1 && h >= zap_block,
                "allowance expiry h={} outside expected window [{}, {}]",
                h,
                zap_block,
                post_block + 1
            );
        }
        cw20::Expiration::Never {} if resp.allowance.is_zero() => {
            // cw20-base may report Never when the residual is zero — that's
            // fine, nothing to expire.
        }
        other => panic!(
            "unexpected expiration {:?} (allowance={})",
            other, resp.allowance
        ),
    }
}


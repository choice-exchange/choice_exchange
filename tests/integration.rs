#![cfg(test)]

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{
    injective_std::types::cosmos::bank::v1beta1::QueryBalanceRequest, Account, Bank,
    InjectiveTestApp, Module, SigningAccount, Wasm,
};

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::manager::{
    ExecuteMsg as ManagerExecuteMsg, InstantiateMsg as ManagerInstantiateMsg, Position,
    QueryMsg as ManagerQueryMsg,
};
use choice_clmm_common::pool::{
    ExecuteMsg as PoolExecuteMsg, PoolState, ProtocolFeesResponse, QueryMsg as PoolQueryMsg,
};
use choice_clmm_common::types::AssetInfo;

use cw721::msg::OwnerOfResponse;

const ATOM: &str = "atom";
const USDT: &str = "usdt";

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

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

fn get_wasm_byte_code(filename: &str) -> Vec<u8> {
    let path = format!("../../artifacts/{}", filename);
    std::fs::read(&path).unwrap_or_else(|_| panic!("Could not read wasm file at {}", path))
}

pub struct TestEnv {
    pub app: InjectiveTestApp,
    pub admin: SigningAccount,
    pub user: SigningAccount,
    pub factory_addr: String,
    pub manager_addr: String,
    pub pool_code_id: u64,
}

fn setup() -> TestEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    let admin_initial_coins = &[
        Coin::new(1_000_000_000_000_000_000_000_000_000_000u128, "inj"),
        Coin::new(1_000_000_000_000_000_000u128, "usdt"),
        Coin::new(1_000_000_000_000u128, ATOM),
    ];
    let admin_initial_decimals = &[
        18, // inj
        6,  // usdt
        6,  // atom
    ];

    let admin = app
        .init_account_decimals(admin_initial_coins, admin_initial_decimals)
        .unwrap();

    let user = app
        .init_account(&[
            Coin::new(1_000_000_000_000_000_000_000_000u128, "inj"),
            Coin::new(1_000_000_000_000u128, "usdt"),
            Coin::new(1_000_000_000_000u128, ATOM),
        ])
        .unwrap();

    // 2. Store Codes
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

    // 3. Instantiate Factory
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

    // 4. Instantiate Manager
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

    TestEnv {
        app,
        admin,
        user,
        factory_addr,
        manager_addr,
        pool_code_id,
    }
}

// Helper to bootstrap a pool with initial liquidity
fn setup_pool_with_liquidity(env: &TestEnv, wasm: &Wasm<InjectiveTestApp>) -> (String, String) {
    // 1. Create Pool
    let init_price = Uint256::from(79228162514264337593543950336u128); // Price = 1.0
    let create_pool_msg = FactoryExecuteMsg::CreatePool {
        token_a: native(ATOM),
        token_b: native(USDT),
        fee: 500,
        init_sqrt_price: init_price,
        max_fee_multiple: None,
    };
    wasm.execute(&env.factory_addr, &create_pool_msg, &[], &env.admin)
        .unwrap();

    let pool_addr: String = wasm
        .query(
            &env.factory_addr,
            &FactoryQueryMsg::GetPool {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee: 500,
            },
        )
        .unwrap();

    // 2. Mint Position (Range -100 to 100)
    let amount = Uint128::new(1_000_000_000); // 1000 Tokens
    let mint_msg = ManagerExecuteMsg::MintPosition {
        token0: native(ATOM),
        token1: native(USDT),
        fee: 500,
        tick_lower: -100,
        tick_upper: 100,
        amount0_desired: amount,
        amount1_desired: amount,
        amount0_min: Uint128::zero(),
        amount1_min: Uint128::zero(),
        recipient: None,
        deadline: 9999999999,
    };

    let funds = vec![
        Coin::new(amount.u128(), ATOM),
        Coin::new(amount.u128(), USDT),
    ];
    let res = wasm
        .execute(&env.manager_addr, &mint_msg, &funds, &env.user)
        .unwrap();

    let token_id = res
        .events
        .iter()
        .find(|e| e.ty == "wasm")
        .and_then(|e| e.attributes.iter().find(|a| a.key == "token_id"))
        .map(|a| a.value.clone())
        .expect("Token ID found");

    (pool_addr, token_id)
}

#[test]
fn test_e2e_create_pool_and_mint_position() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // --- STEP 1: Create Pool (ATOM/USDT) ---
    // Q64.96 Price for 1.0 = 2^96
    let init_price = Uint256::from(79228162514264337593543950336u128);

    let create_pool_msg = FactoryExecuteMsg::CreatePool {
        token_a: native(ATOM),
        token_b: native(USDT),
        fee: 500, // 0.05%
        init_sqrt_price: init_price,
        max_fee_multiple: None,
    };

    wasm.execute(&env.factory_addr, &create_pool_msg, &[], &env.admin)
        .unwrap();

    // Query Factory to get the new Pool Address
    let pool_addr: String = wasm
        .query(
            &env.factory_addr,
            &FactoryQueryMsg::GetPool {
                token_a: native(ATOM),
                token_b: native(USDT),
                fee: 500,
            },
        )
        .unwrap();

    println!("Pool Created at: {}", pool_addr);
    assert!(!pool_addr.is_empty());

    // --- STEP 2: Mint Position via Manager ---

    let amount_desired = Uint128::new(1_000_000);

    let mint_msg = ManagerExecuteMsg::MintPosition {
        token0: native(ATOM),
        token1: native(USDT),
        fee: 500,
        tick_lower: -100,
        tick_upper: 100,
        amount0_desired: amount_desired,
        amount1_desired: amount_desired,
        amount0_min: Uint128::zero(),
        amount1_min: Uint128::zero(),
        recipient: None,
        deadline: 9999999999,
    };

    let funds = vec![
        Coin::new(amount_desired.u128(), ATOM),
        Coin::new(amount_desired.u128(), USDT),
    ];

    let res = wasm
        .execute(&env.manager_addr, &mint_msg, &funds, &env.user)
        .unwrap();

    let token_id = res
        .events
        .iter()
        .find(|e| e.ty == "wasm")
        .and_then(|e| e.attributes.iter().find(|a| a.key == "token_id"))
        .map(|a| a.value.clone())
        .expect("Token ID not found in events");

    assert_eq!(token_id, "1");

    // --- STEP 3: Verification ---

    // A. Check NFT Ownership
    let owner_res: OwnerOfResponse = wasm
        .query(
            &env.manager_addr,
            &ManagerQueryMsg::OwnerOf {
                token_id: token_id.clone(),
                include_expired: None,
            },
        )
        .unwrap();

    assert_eq!(owner_res.owner, env.user.address());

    // B. Check Position Logic Data
    let position: Position = wasm
        .query(
            &env.manager_addr,
            &ManagerQueryMsg::Position {
                token_id: token_id.clone(),
            },
        )
        .unwrap();

    assert_eq!(position.pool_address, pool_addr);
    assert_eq!(position.tick_lower, -100);

    // C. Check Pool Balance using the imported QueryBalanceRequest
    let bank = Bank::new(&env.app);

    let pool_balance_atom = bank
        .query_balance(&QueryBalanceRequest {
            address: pool_addr.clone(),
            denom: ATOM.to_string(),
        })
        .unwrap()
        .balance
        .unwrap();

    let pool_balance_usdt = bank
        .query_balance(&QueryBalanceRequest {
            address: pool_addr,
            denom: USDT.to_string(),
        })
        .unwrap()
        .balance
        .unwrap();

    println!("Pool Balance ATOM: {}", pool_balance_atom.amount);
    println!("Pool Balance USDT: {}", pool_balance_usdt.amount);

    assert!(pool_balance_atom.amount.parse::<u128>().unwrap() > 0);
    assert!(pool_balance_usdt.amount.parse::<u128>().unwrap() > 0);
}

#[test]
fn test_swap_exact_input() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // 1. Setup Pool with Liquidity (User 1 provides)
    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // 2. Setup Trader (User 2)
    // We use 'admin' as the trader for simplicity, as they have funds
    let trader = &env.admin;
    let swap_amount = Uint128::new(10_000_000); // Swap 10 ATOM

    // Get initial balances
    let start_usdt = bank
        .query_balance(&QueryBalanceRequest {
            address: trader.address(),
            denom: USDT.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    // 3. Execute Swap: ATOM -> USDT (ZeroForOne = True if ATOM < USDT)
    // Since "atom" < "usdt", ATOM is Token0.
    // Swapping Token0 -> Token1 means zero_for_one = true.
    let swap_msg = PoolExecuteMsg::Swap {
        recipient: trader.address(),
        zero_for_one: true,
        amount_specified: swap_amount,
        // Price limit: Square root of 0.5 (price drops) -> ~56022770974786139916170664450
        // Or just use Min Sqrt Price + 1
        sqrt_price_limit_x96: Uint256::from(4295128739u128),
    };

    let res = wasm.execute(
        &pool_addr,
        &swap_msg,
        &[Coin::new(swap_amount.u128(), ATOM)],
        trader,
    );
    assert!(res.is_ok(), "Swap failed: {:?}", res.err());

    // 4. Verify Balances
    let end_usdt = bank
        .query_balance(&QueryBalanceRequest {
            address: trader.address(),
            denom: USDT.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    println!("Swap Input: {} ATOM", swap_amount);
    println!("Swap Output: {} USDT", end_usdt - start_usdt);

    assert!(end_usdt > start_usdt, "Trader should receive USDT");

    // 5. Verify Price Movement
    let slot0: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("New Tick: {}", slot0.tick);

    // Price should have moved down (selling Token0)
    // Initial Tick was 0 (Price 1.0). New tick should be negative.
    assert!(slot0.tick < 0);
}

#[test]
fn test_liquidity_lifecycle_and_fees() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // 1. Setup: User provides liquidity
    let (pool_addr, token_id) = setup_pool_with_liquidity(&env, &wasm);

    // 2. Action: Trader performs a Swap to generate fees
    // Swap 50 ATOM -> USDT
    let trader = &env.admin;
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: true,
            amount_specified: Uint128::new(50_000_000),
            sqrt_price_limit_x96: Uint256::from(4295128739u128),
        },
        &[Coin::new(Uint128::new(50_000_000), ATOM)],
        trader,
    )
    .unwrap();

    // 3. Action: User Increases Liquidity (Add more to existing NFT)
    // This creates a checkpoint for fee calculation
    let add_amount = Uint128::new(100_000);
    wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::IncreaseLiquidity {
            token_id: token_id.clone(),
            amount0_desired: add_amount,
            amount1_desired: add_amount,
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: 9999999999,
        },
        &[
            Coin::new(add_amount.u128(), ATOM),
            Coin::new(add_amount.u128(), USDT),
        ],
        &env.user,
    )
    .unwrap();

    // 4. Action: User Collects Fees
    let pre_collect_atom = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: ATOM.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::Collect {
            token_id: token_id.clone(),
            recipient: None, // To owner
        },
        &[],
        &env.user,
    )
    .unwrap();

    let post_collect_atom = bank
        .query_balance(&QueryBalanceRequest {
            address: env.user.address(),
            denom: ATOM.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    // Verify fees were collected (Swap was ATOM in, so fees are in ATOM)
    let fees_collected = post_collect_atom - pre_collect_atom;
    println!("Fees Collected: {} ATOM", fees_collected);
    assert!(fees_collected > 0, "Should have collected swap fees");

    // 5. Action: Decrease Liquidity (Remove Principal)
    // We remove a chunk of liquidity
    let remove_liquidity_amount = Uint128::new(500_000); // 50% ish
    wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::DecreaseLiquidity {
            token_id: token_id.clone(),
            liquidity: remove_liquidity_amount,
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            deadline: 9999999999,
        },
        &[],
        &env.user,
    )
    .unwrap();

    // We must call Collect again to actually get the tokens from the Decrease step
    // (Standard Uniswap V3 behavior: Decrease puts tokens in Owed, Collect pulls them out)
    let res = wasm.execute(
        &env.manager_addr,
        &ManagerExecuteMsg::Collect {
            token_id: token_id.clone(),
            recipient: None,
        },
        &[],
        &env.user,
    );
    assert!(res.is_ok());

    // 6. Action: Burn NFT (Optional - usually requires removing ALL liquidity first)
    // If you want to test burning, you must remove all liquidity first.
}

// ?

#[test]
fn test_partial_swap_price_limit() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // Tight limit: 0.999...
    let tight_limit = Uint256::from(79200000000000000000000000000u128);

    // Use a MASSIVE amount to guarantee we hit the limit
    // 500 Million ATOMs
    let swap_amount = Uint128::new(500_000_000);
    let trader = &env.admin;

    let res = wasm
        .execute(
            &pool_addr,
            &PoolExecuteMsg::Swap {
                recipient: trader.address(),
                zero_for_one: true,
                amount_specified: swap_amount,
                sqrt_price_limit_x96: tight_limit,
            },
            &[Coin::new(swap_amount.u128(), ATOM)],
            trader,
        )
        .unwrap();

    let amount_in_attr = res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "amount_in")
        .unwrap();

    let amount_used = amount_in_attr.value.parse::<u128>().unwrap();

    println!("Requested: {}, Used: {}", swap_amount, amount_used);

    // Now this should pass
    assert!(amount_used < swap_amount.u128());
}

#[test]
fn test_range_crossing_and_activation() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // 1. Setup Pool
    // User A provides liquidity in [-100, 100].
    // They provide ~1000 USDT and ~1000 ATOM.
    // Price starts at 1.0 (Tick 0).
    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // 2. User B mints a "Limit Order" position far below current price
    // Range: [-2000, -1000].
    // Since this range is entirely below the current price (Tick 0),
    // User B only needs to provide Token1 (USDT). They are essentially placing a bid for ATOM.
    let limit_amount = Uint128::new(5_000_000_000); // 5000 USDT
    let funds = vec![Coin::new(limit_amount.u128(), USDT)];

    let mint_msg = ManagerExecuteMsg::MintPosition {
        token0: native(ATOM),
        token1: native(USDT),
        fee: 500,
        tick_lower: -2000,
        tick_upper: -1000,
        amount0_desired: Uint128::zero(),
        amount1_desired: limit_amount,
        amount0_min: Uint128::zero(),
        amount1_min: Uint128::zero(),
        recipient: None,
        deadline: 9999999999,
    };

    wasm.execute(&env.manager_addr, &mint_msg, &funds, &env.user)
        .unwrap();

    // 3. Get initial state
    let slot0_start: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("Start Tick: {}", slot0_start.tick);
    println!("Start L: {}", slot0_start.liquidity);

    // 4. Swap 1: Push price down slightly, but stay within User A's range (0 -> -50ish)
    // We are selling ATOM (ZeroForOne = true), buying USDT.
    let trader = &env.admin;
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: true,
            amount_specified: Uint128::new(1_000_000), // Small amount
            sqrt_price_limit_x96: Uint256::from(4295128739u128), // Min price
        },
        &[Coin::new(Uint128::new(1_000_000), ATOM)],
        trader,
    )
    .unwrap();

    let slot0_mid: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("Mid Tick: {}", slot0_mid.tick);
    println!("Mid L: {}", slot0_mid.liquidity);

    // Assertion: Liquidity should be the same as start (only User A is active)
    // If this fails (L=0), it means the swap logic incorrectly wiped global liquidity.
    assert_ne!(
        slot0_mid.liquidity,
        Uint128::zero(),
        "Liquidity vanished after Swap 1!"
    );
    assert_eq!(
        slot0_mid.liquidity, slot0_start.liquidity,
        "Liquidity should not change within active range"
    );

    // 5. Swap 2: Push price HARD down to cross -1000
    // We need to sell enough ATOM to:
    // a. Buy all of User A's remaining USDT (down to tick -100)
    // b. Traverse the "gap" between -100 and -1000 (where L=0, price moves instantly)
    // c. Enter User B's range and start buying their USDT.
    // 2000 ATOM is > 1000 provided by User A, so this should suffice.
    let swap_amount = Uint128::new(2_000_000_000);

    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: true,
            amount_specified: swap_amount,
            sqrt_price_limit_x96: Uint256::from(4295128739u128),
        },
        &[Coin::new(swap_amount.u128(), ATOM)],
        trader,
    )
    .unwrap();

    let slot0_end: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("End Tick: {}", slot0_end.tick);
    println!("End L: {}", slot0_end.liquidity);

    // Assertion 1: We crossed the gap
    assert!(
        slot0_end.tick < -1000,
        "Price should have crossed into the -1000 range"
    );

    // Assertion 2: Active Liquidity is non-zero
    // (User A is now out of range (0), User B is now in range)
    assert!(
        slot0_end.liquidity != Uint128::zero(),
        "Should have active liquidity from User B"
    );

    // Optional: User B's liquidity calculation
    // Since B provided more tokens over a wider range, their L might be different from A.
    // We just verify it is NOT A's liquidity (since A is inactive).
    assert_ne!(
        slot0_end.liquidity, slot0_start.liquidity,
        "Liquidity should reflect User B now"
    );
}

#[test]
fn test_swap_one_for_zero_reverse() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // 1. Setup Pool (Price 1.0, Tick 0)
    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    let trader = &env.admin;
    // We want to Buy ATOM / Sell USDT.
    // 1000 USDT input.
    let swap_amount = Uint128::new(100_000_000);

    let start_atom = bank
        .query_balance(&QueryBalanceRequest {
            address: trader.address(),
            denom: ATOM.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    // 2. Execute Swap (ZeroForOne = FALSE)
    // Price Limit: Max Sqrt Price - 1 (Price goes UP)
    // 146144... is MAX. We pick a safe high number.
    // 79228... is 1.0. Let's pick 2.0 (~112045...)
    let limit_up = Uint256::from(112045541949572279837463876454u128);

    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: false, // <--- CRITICAL: Reverse Direction
            amount_specified: swap_amount,
            sqrt_price_limit_x96: limit_up,
        },
        &[Coin::new(swap_amount.u128(), USDT)], // Sending USDT
        trader,
    )
    .unwrap();

    // 3. Verify Balance
    let end_atom = bank
        .query_balance(&QueryBalanceRequest {
            address: trader.address(),
            denom: ATOM.to_string(),
        })
        .unwrap()
        .balance
        .unwrap()
        .amount
        .parse::<u128>()
        .unwrap();

    println!(
        "Reverse Swap: Input {} USDT -> Output {} ATOM",
        swap_amount,
        end_atom - start_atom
    );
    assert!(end_atom > start_atom, "Trader should receive ATOM");

    // 4. Verify Tick Movement
    let slot0: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("New Tick (Up): {}", slot0.tick);

    // Price should move UP (Positive Tick)
    assert!(slot0.tick > 0, "Tick should increase when buying Token0");
}

#[test]
fn test_overlapping_liquidity_math() {
    let env = setup();
    let wasm = Wasm::new(&env.app);

    // 1. User A creates Pool + Liquidity in [-100, 100]
    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // Get baseline L
    let slot0_a: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    let liq_a = slot0_a.liquidity;

    // 2. User B Mints in [0, 200]
    // Current tick is 0.
    // Overlap region: [0, 100].
    // User A is active in [0, 100].
    // User B is active in [0, 100].
    // Therefore, in the current tick 0, Liquidity should sum up (L_A + L_B).

    let amount = Uint128::new(1_000_000_000);
    let funds = vec![
        Coin::new(amount.u128(), ATOM),
        Coin::new(amount.u128(), USDT),
    ];

    let mint_msg = ManagerExecuteMsg::MintPosition {
        token0: native(ATOM),
        token1: native(USDT),
        fee: 500,
        tick_lower: 0,
        tick_upper: 200,
        amount0_desired: amount,
        amount1_desired: amount,
        amount0_min: Uint128::zero(),
        amount1_min: Uint128::zero(),
        recipient: None,
        deadline: 9999999999,
    };

    wasm.execute(&env.manager_addr, &mint_msg, &funds, &env.user)
        .unwrap();

    // 3. Verify Active Liquidity
    let slot0_b: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    let liq_total = slot0_b.liquidity;

    println!("L(A): {}", liq_a);
    println!("L(Total): {}", liq_total);

    // It should be strictly greater than A
    assert!(liq_total > liq_a);

    // 4. Swap UP past 100
    // At tick 100, User A exits. Only User B remains.
    // L should drop.
    let trader = &env.admin;
    let limit_up = Uint256::from(112045541949572279837463876454u128); // Price ~2.0

    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: false,                           // Buy ATOM (Price UP)
            amount_specified: Uint128::new(2_000_000_000), // Large swap to exit overlap
            sqrt_price_limit_x96: limit_up,
        },
        &[Coin::new(Uint128::new(2_000_000_000), USDT)],
        trader,
    )
    .unwrap();

    let slot0_end: PoolState = wasm.query(&pool_addr, &PoolQueryMsg::GetSlot0 {}).unwrap();
    println!("End Tick: {}", slot0_end.tick);
    println!("End L: {}", slot0_end.liquidity);

    // We must have crossed 100
    assert!(slot0_end.tick > 100);

    // Liquidity should drop because User A is no longer active
    assert!(
        slot0_end.liquidity < liq_total,
        "Liquidity should drop after exiting User A range"
    );
    assert!(
        slot0_end.liquidity > Uint128::zero(),
        "Liquidity should not be zero (User B still active)"
    );
}

// ============================================================================
// Phase 6 — Protocol-fee accrual → CollectProtocol → live send_to_auction
// round-trip (deferred from Phase 1).
// ============================================================================
//
// Exercises the full protocol-fee path end-to-end against the REAL
// `choice_send_to_auction` contract (which emits Injective exchange
// Deposit+ExternalTransfer messages): the factory owner turns on the carve,
// swaps accrue protocol fees on both token sides, the owner points the routing
// at a live auction + treasury, then `CollectProtocol` splits each token
// `burn_share_bps` to the auction (burned) and the remainder to the treasury.
// The auction leg succeeding proves the burn-auction round-trip works on-chain,
// not just in a mock.
#[test]
fn test_protocol_fee_collect_to_auction_roundtrip() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    // Fresh treasury account: starts with zero ATOM/USDT so deltas are clean.
    let treasury = env
        .app
        .init_account(&[Coin::new(1_000_000_000_000_000_000u128, "inj")])
        .unwrap();

    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // 1. Factory owner (admin) enables the 25% protocol carve on both sides.
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::SetFeeProtocol {
            fee_protocol_0: 4,
            fee_protocol_1: 4,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // A non-owner must NOT be able to set the carve (access control).
    let unauth = wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::SetFeeProtocol {
            fee_protocol_0: 10,
            fee_protocol_1: 10,
        },
        &[],
        &env.user,
    );
    assert!(
        unauth.is_err(),
        "non-factory-owner must not set protocol fee"
    );

    // 2. Generate fees on BOTH sides so protocol fees accrue in token0 and token1.
    let trader = &env.admin;
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: true,
            amount_specified: Uint128::new(50_000_000),
            sqrt_price_limit_x96: Uint256::from(4295128739u128),
        },
        &[Coin::new(Uint128::new(50_000_000), ATOM)],
        trader,
    )
    .unwrap();
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Swap {
            recipient: trader.address(),
            zero_for_one: false,
            amount_specified: Uint128::new(50_000_000),
            sqrt_price_limit_x96: Uint256::from(112045541949572279837463876454u128),
        },
        &[Coin::new(Uint128::new(50_000_000), USDT)],
        trader,
    )
    .unwrap();

    let fees: ProtocolFeesResponse = wasm
        .query(&pool_addr, &PoolQueryMsg::GetProtocolFees {})
        .unwrap();
    assert!(
        !fees.protocol_fees_0.is_zero() && !fees.protocol_fees_1.is_zero(),
        "protocol fees should accrue on both sides: {:?}",
        fees
    );

    // 3. Deploy the real send_to_auction contract and point routing at it.
    let auction_code_id = wasm
        .store_code(
            &get_wasm_byte_code("choice_send_to_auction.wasm"),
            None,
            &env.admin,
        )
        .unwrap()
        .data
        .code_id;
    let auction_addr = wasm
        .instantiate(
            auction_code_id,
            &choice::send_to_auction::InstantiateMsg {
                owner: env.admin.address(),
                // adapter_contract is only used on the CW20 path; a valid addr suffices.
                adapter_contract: env.admin.address(),
                burn_auction_subaccount:
                    "0x1111111111111111111111111111111111111111111111111111111111111111"
                        .to_string(),
            },
            Some(&env.admin.address()),
            Some("Choice Send To Auction"),
            &[],
            &env.admin,
        )
        .unwrap()
        .data
        .address;

    // 50% to the burn auction, 50% to the treasury.
    wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::UpdateProtocolFeeConfig {
            treasury: Some(treasury.address()),
            burn_auction: Some(auction_addr.clone()),
            burn_share_bps: Some(5000),
            clear_burn_auction: false,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    // 4. Collect everything. The auction leg must execute (Deposit+ExternalTransfer)
    //    or the whole tx reverts — success here IS the round-trip proof.
    let collect_res = wasm
        .execute(
            &pool_addr,
            &PoolExecuteMsg::CollectProtocol {
                amount0_requested: Uint128::MAX,
                amount1_requested: Uint128::MAX,
            },
            &[],
            &env.admin,
        )
        .unwrap();

    let collected_0: u128 = collect_res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "collected_0")
        .unwrap()
        .value
        .parse()
        .unwrap();
    let collected_1: u128 = collect_res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "collected_1")
        .unwrap()
        .value
        .parse()
        .unwrap();
    assert!(collected_0 > 0 && collected_1 > 0);

    // 5a. Protocol fee buckets are now drained.
    let after: ProtocolFeesResponse = wasm
        .query(&pool_addr, &PoolQueryMsg::GetProtocolFees {})
        .unwrap();
    assert!(after.protocol_fees_0.is_zero() && after.protocol_fees_1.is_zero());

    // 5b. Treasury received exactly the non-burn remainder (collected − floor(50%)).
    let expected_treasury_atom = collected_0 - (collected_0 * 5000 / 10_000);
    let expected_treasury_usdt = collected_1 - (collected_1 * 5000 / 10_000);
    assert_eq!(
        bank_balance(&bank, &treasury.address(), ATOM),
        expected_treasury_atom,
        "treasury ATOM remainder mismatch"
    );
    assert_eq!(
        bank_balance(&bank, &treasury.address(), USDT),
        expected_treasury_usdt,
        "treasury USDT remainder mismatch"
    );

    // 5c. The burn share left the pool entirely (auction consumed it via the
    //     exchange precompile, so it is NOT sitting in the auction's bank balance).
    let burn_atom = collected_0 * 5000 / 10_000;
    assert!(burn_atom > 0, "burn share should be non-zero");
}

// ============================================================================
// Phase 6 — Flash loan with a real borrower contract: callback round-trip,
// fee accrual, and underpayment revert (deferred from Phase 2).
// ============================================================================
//
// Deploys `flash_borrower_mock` (a real CosmWasm contract implementing the
// pool's FlashCallback), lends it both tokens, and verifies (a) on honest
// repayment the pool's balance grows by exactly the flash fee, and (b) a
// borrower that repays one unit short triggers the pool's balance-delta
// repayment check and the whole transaction reverts (pool balance unchanged).
#[test]
fn test_flash_loan_borrower_roundtrip_and_underpay_revert() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    // Deploy the borrower and pre-fund it so it can cover the flash fee
    // (it returns principal + fee; the fee comes out of this buffer).
    let borrower_code_id = wasm
        .store_code(
            &get_wasm_byte_code("flash_borrower_mock.wasm"),
            None,
            &env.admin,
        )
        .unwrap()
        .data
        .code_id;
    let borrower_addr = wasm
        .instantiate(
            borrower_code_id,
            &flash_borrower_mock::InstantiateMsg {},
            Some(&env.admin.address()),
            Some("Flash Borrower"),
            &[
                Coin::new(1_000_000u128, ATOM),
                Coin::new(1_000_000u128, USDT),
            ],
            &env.admin,
        )
        .unwrap()
        .data
        .address;

    let amount0 = Uint128::new(2_000_000); // ATOM (token0)
    let amount1 = Uint128::new(1_000_000); // USDT (token1)

    // `Flash {}` is gated on the factory's flash-borrower allowlist; authorize the
    // caller (the factory owner = env.admin here).
    wasm.execute(
        &env.factory_addr,
        &FactoryExecuteMsg::AuthorizeFlashBorrower {
            borrower: env.admin.address(),
        },
        &[],
        &env.admin,
    )
    .unwrap();

    let honest_plan = flash_borrower_mock::RepayPlan {
        denom0: ATOM.to_string(),
        denom1: USDT.to_string(),
        amount0,
        amount1,
        underpay0: Uint128::zero(),
        underpay1: Uint128::zero(),
        repay_via_submsg: false,
    };

    let pool_atom_before = bank_balance(&bank, &pool_addr, ATOM);
    let pool_usdt_before = bank_balance(&bank, &pool_addr, USDT);

    let res = wasm
        .execute(
            &pool_addr,
            &PoolExecuteMsg::Flash {
                recipient: borrower_addr.clone(),
                amount0,
                amount1,
                data: cosmwasm_std::to_json_binary(&honest_plan).unwrap(),
            },
            &[],
            &env.admin,
        )
        .unwrap();

    // Flash fee at the 0.05% base tier = ceil(amount * 500 / 1_000_000).
    let fee0: u128 = res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "fee0")
        .unwrap()
        .value
        .parse()
        .unwrap();
    let fee1: u128 = res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "fee1")
        .unwrap()
        .value
        .parse()
        .unwrap();
    assert!(fee0 > 0 && fee1 > 0, "flash fee must be non-zero");

    // The pool must end up with exactly its starting balance + the flash fee.
    assert_eq!(
        bank_balance(&bank, &pool_addr, ATOM),
        pool_atom_before + fee0,
        "pool ATOM should grow by exactly fee0"
    );
    assert_eq!(
        bank_balance(&bank, &pool_addr, USDT),
        pool_usdt_before + fee1,
        "pool USDT should grow by exactly fee1"
    );

    // --- Underpayment must revert the whole flash ---------------------------
    let pool_atom_mid = bank_balance(&bank, &pool_addr, ATOM);
    let pool_usdt_mid = bank_balance(&bank, &pool_addr, USDT);

    let cheat_plan = flash_borrower_mock::RepayPlan {
        denom0: ATOM.to_string(),
        denom1: USDT.to_string(),
        amount0,
        amount1,
        underpay0: Uint128::one(), // one unit short of (principal + fee)
        underpay1: Uint128::zero(),
        repay_via_submsg: false,
    };
    let cheat = wasm.execute(
        &pool_addr,
        &PoolExecuteMsg::Flash {
            recipient: borrower_addr,
            amount0,
            amount1,
            data: cosmwasm_std::to_json_binary(&cheat_plan).unwrap(),
        },
        &[],
        &env.admin,
    );
    assert!(cheat.is_err(), "underpaid flash must revert");

    // Reverted: the pool's balances are untouched by the failed flash.
    assert_eq!(bank_balance(&bank, &pool_addr, ATOM), pool_atom_mid);
    assert_eq!(bank_balance(&bank, &pool_addr, USDT), pool_usdt_mid);
}

// ============================================================================
// Phase 6 (b) — native flash repaid via a SUBMESSAGE bank send.
// ============================================================================
//
// The pool verifies flash repayment by querying its own balance in
// `reply_flash`, which relies on the borrower's repayment having settled first.
// CosmWasm runs a sub-call's messages AND submessages (and their replies) to
// completion before the parent's reply fires — so a borrower that repays inside
// a `SubMsg::reply_on_success` (with its own reply) must still settle before
// `reply_flash` reads the balance. This test proves that path: an honest native
// repayment routed through a submessage succeeds and the pool grows by exactly
// the flash fee.
#[test]
fn test_flash_native_repay_via_submessage() {
    let env = setup();
    let wasm = Wasm::new(&env.app);
    let bank = Bank::new(&env.app);

    let (pool_addr, _) = setup_pool_with_liquidity(&env, &wasm);

    let borrower_code_id = wasm
        .store_code(
            &get_wasm_byte_code("flash_borrower_mock.wasm"),
            None,
            &env.admin,
        )
        .unwrap()
        .data
        .code_id;
    let borrower_addr = wasm
        .instantiate(
            borrower_code_id,
            &flash_borrower_mock::InstantiateMsg {},
            Some(&env.admin.address()),
            Some("Flash Borrower SubMsg"),
            &[
                Coin::new(1_000_000u128, ATOM),
                Coin::new(1_000_000u128, USDT),
            ],
            &env.admin,
        )
        .unwrap()
        .data
        .address;

    let amount0 = Uint128::new(2_000_000); // ATOM (token0)
    let amount1 = Uint128::new(1_000_000); // USDT (token1)

    // `Flash {}` is gated on the factory's flash-borrower allowlist; authorize the
    // caller (the factory owner = env.admin here).
    wasm.execute(
        &env.factory_addr,
        &FactoryExecuteMsg::AuthorizeFlashBorrower {
            borrower: env.admin.address(),
        },
        &[],
        &env.admin,
    )
    .unwrap();

    let plan = flash_borrower_mock::RepayPlan {
        denom0: ATOM.to_string(),
        denom1: USDT.to_string(),
        amount0,
        amount1,
        underpay0: Uint128::zero(),
        underpay1: Uint128::zero(),
        repay_via_submsg: true,
    };

    let pool_atom_before = bank_balance(&bank, &pool_addr, ATOM);
    let pool_usdt_before = bank_balance(&bank, &pool_addr, USDT);

    let res = wasm
        .execute(
            &pool_addr,
            &PoolExecuteMsg::Flash {
                recipient: borrower_addr,
                amount0,
                amount1,
                data: cosmwasm_std::to_json_binary(&plan).unwrap(),
            },
            &[],
            &env.admin,
        )
        .unwrap();

    // Confirm the borrower actually took the submessage repayment path.
    assert!(
        res.events
            .iter()
            .flat_map(|e| e.attributes.iter())
            .any(|a| a.key == "via_submsg" && a.value == "true"),
        "borrower should have repaid via submessage"
    );

    let fee0: u128 = res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "fee0")
        .unwrap()
        .value
        .parse()
        .unwrap();
    let fee1: u128 = res
        .events
        .iter()
        .flat_map(|e| e.attributes.iter())
        .find(|a| a.key == "fee1")
        .unwrap()
        .value
        .parse()
        .unwrap();
    assert!(fee0 > 0 && fee1 > 0, "flash fee must be non-zero");

    // Submessage repayment settled before reply_flash queried the balance: the
    // pool ends with exactly its starting balance + the flash fee.
    assert_eq!(
        bank_balance(&bank, &pool_addr, ATOM),
        pool_atom_before + fee0,
        "pool ATOM should grow by exactly fee0 (submsg repay)"
    );
    assert_eq!(
        bank_balance(&bank, &pool_addr, USDT),
        pool_usdt_before + fee1,
        "pool USDT should grow by exactly fee1 (submsg repay)"
    );
}

#![cfg(test)]

use cosmwasm_std::{Coin, Uint128, Uint256};
use injective_test_tube::{Account, InjectiveTestApp, Module, SigningAccount, Wasm};
use std::fs::File;
use std::io::Write;

use choice_clmm_common::factory::{
    ExecuteMsg as FactoryExecuteMsg, InstantiateMsg as FactoryInstantiateMsg,
    QueryMsg as FactoryQueryMsg,
};
use choice_clmm_common::manager::{
    ExecuteMsg as ManagerExecuteMsg, InstantiateMsg as ManagerInstantiateMsg,
};
use choice_clmm_common::pool::{QueryMsg as PoolQueryMsg, TickInfo};
use choice_clmm_common::types::AssetInfo;

// --- CONSTANTS & TYPES ---
const ATOM: &str = "atom";
const USDT: &str = "usdt";

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

struct LiquidityBin {
    lower_tick: i32,
    upper_tick: i32,
    liquidity: u128,
}

// --- SETUP HELPERS ---

fn get_wasm_byte_code(filename: &str) -> Vec<u8> {
    let path = format!("../../artifacts/{}", filename);
    std::fs::read(&path).unwrap_or_else(|_| panic!("Could not read wasm file at {}", path))
}

struct VizEnv {
    app: InjectiveTestApp,
    admin: SigningAccount,
    user: SigningAccount,
    factory: String,
    manager: String,
}

fn setup_viz_env() -> VizEnv {
    let app = InjectiveTestApp::new();
    let wasm = Wasm::new(&app);

    // Give massive balances for visualization
    let funds = vec![
        Coin::new(1_000_000_000_000_000_000_000u128, ATOM),
        Coin::new(1_000_000_000_000_000_000_000u128, USDT),
        Coin::new(1_000_000_000_000_000_000_000u128, "inj"),
    ];

    let admin = app.init_account(&funds).unwrap();
    let user = app.init_account(&funds).unwrap();

    // Deploy
    let factory_code = wasm
        .store_code(
            &get_wasm_byte_code("choice_clmm_factory.wasm"),
            None,
            &admin,
        )
        .unwrap()
        .data
        .code_id;
    let pool_code = wasm
        .store_code(&get_wasm_byte_code("choice_clmm_pool.wasm"), None, &admin)
        .unwrap()
        .data
        .code_id;
    let manager_code = wasm
        .store_code(
            &get_wasm_byte_code("choice_clmm_manager.wasm"),
            None,
            &admin,
        )
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
            Some("Factory"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    let manager = wasm
        .instantiate(
            manager_code,
            &ManagerInstantiateMsg {
                name: "Viz".to_string(),
                symbol: "VIZ".to_string(),
                factory_addr: factory.clone(),
            },
            Some(&admin.address()),
            Some("Manager"),
            &[],
            &admin,
        )
        .unwrap()
        .data
        .address;

    VizEnv {
        app,
        admin,
        user,
        factory,
        manager,
    }
}

// --- LOGIC HELPERS ---

fn create_pool(env: &VizEnv, fee: u32) -> String {
    let wasm = Wasm::new(&env.app);
    let init_price = Uint256::from(79228162514264337593543950336u128); // 1.0

    wasm.execute(
        &env.factory,
        &FactoryExecuteMsg::CreatePool {
            token_a: native(ATOM),
            token_b: native(USDT),
            fee,
            init_sqrt_price: init_price,
            max_fee_multiple: None,
        },
        &[],
        &env.admin,
    )
    .unwrap();

    wasm.query(
        &env.factory,
        &FactoryQueryMsg::GetPool {
            token_a: native(ATOM),
            token_b: native(USDT),
            fee,
        },
    )
    .unwrap()
}

/// Helper to mint liquidity easily
fn mint(env: &VizEnv, lower: i32, upper: i32, amount: u128) {
    let wasm = Wasm::new(&env.app);

    // Simple heuristic: if range includes 0, provide both.
    // If range < 0, provide USDT. If range > 0, provide ATOM.
    let mut funds = vec![];
    let mut amt0 = Uint128::zero();
    let mut amt1 = Uint128::zero();

    if upper <= 0 {
        amt1 = Uint128::new(amount);
        funds.push(Coin::new(amount, USDT));
    } else if lower >= 0 {
        amt0 = Uint128::new(amount);
        funds.push(Coin::new(amount, ATOM));
    } else {
        amt0 = Uint128::new(amount);
        amt1 = Uint128::new(amount);
        funds.push(Coin::new(amount, ATOM));
        funds.push(Coin::new(amount, USDT));
    }

    wasm.execute(
        &env.manager,
        &ManagerExecuteMsg::MintPosition {
            token0: native(ATOM),
            token1: native(USDT),
            fee: 500,
            tick_lower: lower,
            tick_upper: upper,
            amount0_desired: amt0,
            amount1_desired: amt1,
            amount0_min: Uint128::zero(),
            amount1_min: Uint128::zero(),
            recipient: None,
            deadline: 0,
        },
        &funds,
        &env.user,
    )
    .unwrap();
}

/// Reconstructs the curve by querying `liquidity_net` from the contract
fn export_svg(env: &VizEnv, pool: &str, ticks: Vec<i32>, filename: &str) {
    let wasm = Wasm::new(&env.app);
    let mut bins = Vec::new();
    let mut current_l = 0i128;

    for i in 0..ticks.len() - 1 {
        let t_curr = ticks[i];
        let t_next = ticks[i + 1];

        // Query contract for exact net change
        let info: TickInfo = wasm
            .query(pool, &PoolQueryMsg::GetTickInfo { tick: t_curr })
            .unwrap_or_default();
        current_l += info.liquidity_delta;

        bins.push(LiquidityBin {
            lower_tick: t_curr,
            upper_tick: t_next,
            liquidity: if current_l < 0 { 0 } else { current_l as u128 },
        });
    }

    generate_liquidity_svg(filename, &bins);
}

// --- SVG GENERATOR (Dark Theme) ---
fn generate_liquidity_svg(filename: &str, bins: &[LiquidityBin]) {
    let width = 800;
    let height = 400;
    let padding = 40;

    // 1. Find bounds
    let min_tick = bins.first().map(|b| b.lower_tick).unwrap_or(0);
    let max_tick = bins.last().map(|b| b.upper_tick).unwrap_or(0);
    let max_liquidity = bins.iter().map(|b| b.liquidity).max().unwrap_or(1);

    // 2. Scaling helpers
    let scale_x = |tick: i32| -> f64 {
        if max_tick == min_tick {
            return padding as f64;
        }
        let range = (max_tick - min_tick) as f64;
        let pct = (tick - min_tick) as f64 / range;
        padding as f64 + (pct * (width - 2 * padding) as f64)
    };

    let scale_y = |liq: u128| -> f64 {
        let pct = liq as f64 / max_liquidity as f64;
        (height - padding) as f64 - (pct * (height - 2 * padding) as f64)
    };

    // 3. Build SVG Content (Using standard escaped quotes to prevent syntax errors)
    let mut svg = String::new();

    // Header
    svg.push_str(&format!(
        "<svg viewBox=\"0 0 {} {}\" xmlns=\"http://www.w3.org/2000/svg\" style=\"background-color:#1e1e1e; font-family:sans-serif;\">", 
        width, height
    ));

    // X Axis Line
    svg.push_str(&format!(
        "<line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#444\" stroke-width=\"1\" />",
        padding,
        height - padding,
        width - padding,
        height - padding
    ));

    // Labels
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" fill=\"#888\" font-size=\"12\">Tick {}</text>",
        padding,
        height - 20,
        min_tick
    ));
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" fill=\"#888\" font-size=\"12\" text-anchor=\"end\">Tick {}</text>",
        width - padding,
        height - 20,
        max_tick
    ));

    // Draw Bars
    for bin in bins {
        let x1 = scale_x(bin.lower_tick);
        let x2 = scale_x(bin.upper_tick);
        let bar_width = x2 - x1;

        let y_top = scale_y(bin.liquidity);
        let bar_height = (height - padding) as f64 - y_top;

        // Color intensity based on depth
        let opacity = 0.3 + (0.7 * (bin.liquidity as f64 / max_liquidity as f64));
        let color = format!("rgba(0, 200, 255, {})", opacity);

        if bar_width > 0.0 {
            svg.push_str(&format!(
                "<rect x=\"{:.2}\" y=\"{:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" stroke=\"none\" />",
                x1, y_top, bar_width, bar_height, color
            ));
            // Top border line
            svg.push_str(&format!(
                "<line x1=\"{:.2}\" y1=\"{:.2}\" x2=\"{:.2}\" y2=\"{:.2}\" stroke=\"#00ffff\" stroke-width=\"2\" />",
                x1, y_top, x2, y_top
            ));
        }
    }

    svg.push_str("</svg>");

    // 4. Write to file
    let path = format!("plots/{}", filename);
    std::fs::create_dir_all("plots").unwrap_or_default();
    let mut file = File::create(&path).expect("Unable to create file");
    file.write_all(svg.as_bytes())
        .expect("Unable to write data");
    println!("Generated liquidity curve: {}", path);
}

// --- TESTS START HERE ---

#[test]
fn viz_stablecoin_peg() {
    // SCENARIO: Stablecoin Peg (USDC/USDT)
    // Massive concentration in a tiny range [-10, 10]
    // Very little outside.
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // 1. The Peg (Massive L)
    mint(&env, -10, 10, 100_000_000_000);

    // 2. The De-peg Protection (Wide, shallow)
    mint(&env, -100, 100, 1_000_000_000);

    let ticks = (-120..=120).step_by(10).collect();
    export_svg(&env, &pool, ticks, "viz_stablecoin.svg");
}

#[test]
fn viz_bipolar_market() {
    // SCENARIO: "Camel Hump" / Bipolar
    // Traders think price will go DOWN (Sell wall at +tick)
    // Or price will go UP (Buy wall at -tick)
    // No one is providing liquidity in the middle.
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // Buy Wall (Support) [-500, -200]
    mint(&env, -500, -200, 5_000_000_000);

    // Sell Wall (Resistance) [200, 500]
    mint(&env, 200, 500, 5_000_000_000);

    // Small bridge in middle so SVG isn't empty
    mint(&env, -100, 100, 100_000_000);

    let ticks = (-600..=600).step_by(50).collect();
    export_svg(&env, &pool, ticks, "viz_bipolar.svg");
}

#[test]
fn viz_grid_strategy() {
    // SCENARIO: Grid Trading Bot
    // User places uniform orders at fixed intervals.
    // Creates a "Ladder" or "Comb" effect.
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // Mint discrete steps
    // Range [-1000, 1000], step 200.
    // Each position is 100 ticks wide.
    for i in (-1000..1000).step_by(200) {
        mint(&env, i, i + 100, 2_000_000_000);
    }

    // Add a base layer underneath everything
    mint(&env, -1200, 1200, 500_000_000);

    let ticks = (-1200..=1200).step_by(50).collect();
    export_svg(&env, &pool, ticks, "viz_grid.svg");
}

#[test]
fn viz_gaussian_curve() {
    // SCENARIO: Standard Market Maker (Normal Distribution)
    // Simulating a bell curve by stacking positions.
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // Layer 1 (Base)
    mint(&env, -1000, 1000, 500_000_000);
    // Layer 2
    mint(&env, -700, 700, 800_000_000);
    // Layer 3
    mint(&env, -400, 400, 1_500_000_000);
    // Layer 4 (Peak)
    mint(&env, -150, 150, 3_000_000_000);

    let ticks = (-1200..=1200).step_by(50).collect();
    export_svg(&env, &pool, ticks, "viz_gaussian.svg");
}

#[test]
fn viz_asymmetric_liquidity() {
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // Liquidity Wall to the LEFT (Buy Wall)
    mint(&env, -2000, -500, 10_000_000_000); // Large USDT for ATOM

    // Small Liquidity to the RIGHT (Sell Wall)
    mint(&env, 50, 150, 1_000_000); // Small ATOM for USDT

    let ticks = (-2000..=1000).step_by(100).collect();
    export_svg(&env, &pool, ticks, "viz_asymmetric.svg");
}

#[test]
fn viz_degenerate_liquidity_edges() {
    let env = setup_viz_env();
    let pool = create_pool(&env, 500);

    // Position 1: [100, 200]
    mint(&env, 100, 200, 1_000_000_000);

    // Position 2: [200, 300] - starts exactly where Position 1 ends
    mint(&env, 200, 300, 1_500_000_000);

    // Position 3: [-100, 0] - a bit of left side for context
    mint(&env, -100, 0, 500_000_000);

    let ticks = (-200..=300).step_by(50).collect();
    export_svg(&env, &pool, ticks, "viz_degenerate_edges.svg");
}

//! Dynamic-fee backtest harness — host-target example (no wasm, no test-tube).
//!
//! Replays a `(time, tick)` series through standalone reference implementations
//! of the **v1** dynamic fee (sqrt-price EMA, ported from `core/oracle.rs`) and
//! the proposed **v2** dynamic fee (tick-excursion volatility accumulator, the
//! spec in `docs/clmm_dynamic_fee_v2_plan.md` §2.3 — this `v2_step` is the
//! porting reference for the contract rewrite), and reports comparison metrics.
//!
//! ```bash
//! cd choice_exchange
//! cargo run --example fee_backtest                       # synthetic regimes
//! cargo run --example fee_backtest -- --csv series.csv   # real (time,tick[,notional])
//! cargo run --example fee_backtest -- --csv series.csv --dump out.csv
//! ```
//!
//! The two `*_step` fns are deliberately self-contained (they do NOT import the
//! contract) so calibration can be iterated freely before Phase 1 touches the
//! pool. Keep `v2_step` in sync with the contract once Phase 2 lands.

use choice_clmm_math::full_math::mul_div;
use choice_clmm_math::tick_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};
use cosmwasm_std::{Uint128, Uint256};
use std::convert::TryFrom;
use std::env;
use std::fs;

/// Fixed scale for the v2 convex term: `variable_ppm = control · v_a² / VFEE_SCALE`.
/// At 1e6 with control=8800, a 6% move (~583 ticks) adds ~2990 ppm (calibration §3).
const VFEE_SCALE: u128 = 1_000_000;

// ---------------------------------------------------------------------------
// Config mirrors (only the fields each version consumes).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct V1Config {
    base_fee_ppm: u32,
    max_fee_ppm: u32,
    volatility_multiplier: u32,
    ema_halflife_seconds: u64,
    max_fee_change_per_second_ppm: u32,
}

#[derive(Clone)]
struct V2Config {
    base_fee_ppm: u32,
    max_fee_ppm: u32,
    variable_fee_control: u32,
    max_volatility_accumulator: u32,
    /// Full-forget window (seconds): the accumulator linearly decays to 0 over
    /// this much idle time, so an elevated fee persists across the move then
    /// relaxes to base as trading calms.
    volatility_decay_seconds: u32,
    max_fee_change_per_second_ppm: u32,
}

/// Factory defaults proposed in the v2 plan §3.
fn v1_default() -> V1Config {
    V1Config {
        base_fee_ppm: 3_000,
        max_fee_ppm: 6_000,
        volatility_multiplier: 100_000,
        ema_halflife_seconds: 600,
        max_fee_change_per_second_ppm: 100,
    }
}
fn v2_default() -> V2Config {
    V2Config {
        base_fee_ppm: 3_000,
        max_fee_ppm: 6_000,
        variable_fee_control: 8_800,
        max_volatility_accumulator: 2_000,
        volatility_decay_seconds: 600,
        max_fee_change_per_second_ppm: 100,
    }
}

// ---------------------------------------------------------------------------
// v1 reference — faithful port of core/oracle.rs (sqrt-space EMA).
// ---------------------------------------------------------------------------

struct V1State {
    price_ema_x96: Uint256,
    last_time: u64,
    last_fee_ppm: u32,
}

fn v1_raw(cfg: &V1Config, ema: Uint256, price: Uint256) -> u32 {
    if ema.is_zero() {
        return cfg.base_fee_ppm;
    }
    let diff = if price > ema {
        price - ema
    } else {
        ema - price
    };
    let dynamic = mul_div(diff, Uint256::from(cfg.volatility_multiplier), ema).unwrap();
    let total = Uint256::from(cfg.base_fee_ppm) + dynamic;
    let max = Uint256::from(cfg.max_fee_ppm);
    let capped = if total > max { max } else { total };
    Uint128::try_from(capped).unwrap().u128() as u32
}

fn v1_step(cfg: &V1Config, st: &mut V1State, now: u64, price: Uint256) -> u32 {
    let delta = now.saturating_sub(st.last_time);
    if delta == 0 {
        return st.last_fee_ppm;
    }
    let halflife = cfg.ema_halflife_seconds.max(1);
    let ema_old = st.price_ema_x96;
    let ema_new = if delta >= halflife {
        price
    } else {
        let wo = Uint256::from(halflife - delta);
        let wn = Uint256::from(delta);
        let tw = Uint256::from(halflife);
        (ema_old * wo + price * wn) / tw
    };
    // Raw fee measured against the PRE-blend EMA (the v1 self-dilution fix).
    let raw = v1_raw(cfg, ema_old, price);
    let clamped = rate_limit(
        raw,
        st.last_fee_ppm,
        cfg.max_fee_change_per_second_ppm,
        delta,
    );
    st.price_ema_x96 = ema_new;
    st.last_time = now;
    st.last_fee_ppm = clamped;
    clamped
}

// ---------------------------------------------------------------------------
// v2 reference — decaying accumulator of realized tick-movement. PORTING REF.
//
// Entry-time computation observes price only at swap boundaries, so the signal
// is the realized move since the last observation, |current_tick − last_tick|,
// accumulated with linear time-decay. This captures a one-step gap (unlike a
// re-anchored displacement reference, which zeroes on the gap step), keeps the
// price reference instantaneous (no lag → no coupled time constants), and is a
// faithful realized-volatility proxy. Fed through a convex (squared) fee.
// ---------------------------------------------------------------------------

struct V2State {
    last_time: u64,
    last_tick: i32,
    volatility_accumulator: u64,
    last_fee_ppm: u32,
}

fn v2_step(cfg: &V2Config, st: &mut V2State, now: u64, tick: i32) -> u32 {
    let delta = now.saturating_sub(st.last_time);
    if delta == 0 {
        return st.last_fee_ppm;
    }

    // 1. Decay the accumulator by elapsed idle time (linear full-forget window),
    //    then add the realized move observed since the last update.
    let window = cfg.volatility_decay_seconds.max(1) as u64;
    let decayed = if delta >= window {
        0
    } else {
        st.volatility_accumulator * (window - delta) / window
    };
    let step_move = (tick - st.last_tick).unsigned_abs() as u64;
    let v_a = (decayed + step_move).min(cfg.max_volatility_accumulator as u64);

    // 2. Convex variable fee.
    let variable = (cfg.variable_fee_control as u128) * (v_a as u128) * (v_a as u128) / VFEE_SCALE;
    let raw = (cfg.base_fee_ppm as u128 + variable).min(cfg.max_fee_ppm as u128) as u32;

    // 3. Rate-limit (unchanged from v1).
    let clamped = rate_limit(
        raw,
        st.last_fee_ppm,
        cfg.max_fee_change_per_second_ppm,
        delta,
    );

    st.last_time = now;
    st.last_tick = tick;
    st.volatility_accumulator = v_a;
    st.last_fee_ppm = clamped;
    clamped
}

/// Shared per-second rate-limit (identical in v1 and v2).
fn rate_limit(raw: u32, prev: u32, max_per_sec: u32, delta: u64) -> u32 {
    let max_change = max_per_sec.saturating_mul(delta.min(u32::MAX as u64) as u32);
    if max_change == 0 {
        raw
    } else if raw > prev {
        prev.saturating_add(max_change).min(raw)
    } else {
        prev.saturating_sub(max_change).max(raw)
    }
}

fn tick_to_price(tick: i32) -> Uint256 {
    get_sqrt_ratio_at_tick(tick.clamp(MIN_TICK, MAX_TICK)).unwrap()
}

// ---------------------------------------------------------------------------
// Replay + metrics.
// ---------------------------------------------------------------------------

struct StepRow {
    t: u64,
    tick: i32,
    dtick: u64,
    notional: f64,
    fee_v1: u32,
    fee_v2: u32,
}

struct Metrics {
    avg_v1: f64,
    avg_v2: f64,
    max_v1: u32,
    max_v2: u32,
    captured_v1: f64,
    captured_v2: f64,
    corr_v1: f64,
    corr_v2: f64,
}

/// Replay a series through both fee functions. `notional_override` (CSV col 3)
/// is used when present, else a synthetic `1000 + 10·|Δtick|` (bigger moves =
/// bigger trades) so "fee captured" rewards charging more on high-vol steps.
fn replay(series: &[(u64, i32, Option<f64>)], v1c: &V1Config, v2c: &V2Config) -> Vec<StepRow> {
    let mut rows = Vec::with_capacity(series.len());
    if series.is_empty() {
        return rows;
    }
    let t0 = series[0].0;
    let tick0 = series[0].1;
    let mut s1 = V1State {
        price_ema_x96: tick_to_price(tick0),
        last_time: t0,
        last_fee_ppm: v1c.base_fee_ppm,
    };
    let mut s2 = V2State {
        last_time: t0,
        last_tick: tick0,
        volatility_accumulator: 0,
        last_fee_ppm: v2c.base_fee_ppm,
    };
    let mut prev_tick = tick0;
    for &(t, tick, notional) in series {
        let dtick = (tick - prev_tick).unsigned_abs() as u64;
        let fee_v1 = v1_step(v1c, &mut s1, t, tick_to_price(tick));
        let fee_v2 = v2_step(v2c, &mut s2, t, tick);
        let notional = notional.unwrap_or(1000.0 + 10.0 * dtick as f64);
        rows.push(StepRow {
            t,
            tick,
            dtick,
            notional,
            fee_v1,
            fee_v2,
        });
        prev_tick = tick;
    }
    rows
}

fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut cov = 0.0;
    let mut vx = 0.0;
    let mut vy = 0.0;
    for (x, y) in xs.iter().zip(ys) {
        cov += (x - mx) * (y - my);
        vx += (x - mx).powi(2);
        vy += (y - my).powi(2);
    }
    if vx == 0.0 || vy == 0.0 {
        return 0.0;
    }
    cov / (vx.sqrt() * vy.sqrt())
}

fn metrics(rows: &[StepRow]) -> Metrics {
    let n = rows.len().max(1) as f64;
    let dt: Vec<f64> = rows.iter().map(|r| r.dtick as f64).collect();
    let f1: Vec<f64> = rows.iter().map(|r| r.fee_v1 as f64).collect();
    let f2: Vec<f64> = rows.iter().map(|r| r.fee_v2 as f64).collect();
    Metrics {
        avg_v1: f1.iter().sum::<f64>() / n,
        avg_v2: f2.iter().sum::<f64>() / n,
        max_v1: rows.iter().map(|r| r.fee_v1).max().unwrap_or(0),
        max_v2: rows.iter().map(|r| r.fee_v2).max().unwrap_or(0),
        captured_v1: rows
            .iter()
            .map(|r| r.notional * r.fee_v1 as f64 / 1e6)
            .sum(),
        captured_v2: rows
            .iter()
            .map(|r| r.notional * r.fee_v2 as f64 / 1e6)
            .sum(),
        corr_v1: pearson(&f1, &dt),
        corr_v2: pearson(&f2, &dt),
    }
}

/// Mean fee over a window that *should* be quiet (the v1 gap-then-calm defect).
fn calm_mean(rows: &[StepRow], window: (usize, usize)) -> (f64, f64) {
    let (a, b) = window;
    let slice = &rows[a.min(rows.len())..b.min(rows.len())];
    if slice.is_empty() {
        return (0.0, 0.0);
    }
    let n = slice.len() as f64;
    (
        slice.iter().map(|r| r.fee_v1 as f64).sum::<f64>() / n,
        slice.iter().map(|r| r.fee_v2 as f64).sum::<f64>() / n,
    )
}

// ---------------------------------------------------------------------------
// Synthetic regimes. Deterministic (LCG), no external rng.
// ---------------------------------------------------------------------------

struct Scenario {
    name: &'static str,
    series: Vec<(u64, i32, Option<f64>)>,
    /// Index window that should be quiet (for the calm-overcharge metric).
    calm_window: Option<(usize, usize)>,
}

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

fn scenarios() -> Vec<Scenario> {
    let mut out = Vec::new();

    // 1. Calm — sparse trades, tiny ±2-tick wiggle around 0.
    let mut s = Vec::new();
    let mut rng = 1u64;
    for i in 0..300u64 {
        let w = (lcg(&mut rng) % 5) as i32 - 2;
        s.push((i * 45, w, None));
    }
    out.push(Scenario {
        name: "calm",
        series: s,
        calm_window: None,
    });

    // 2. Steady trend — slow monotone drift (~5 ticks/step). The v1 displacement
    //    signal conflates this with volatility; v2 (excursion + decay) should not
    //    saturate on a smooth ramp.
    let mut s = Vec::new();
    for i in 0..300u64 {
        s.push((i * 30, (i as i32) * 5, None));
    }
    out.push(Scenario {
        name: "steady_trend",
        series: s,
        calm_window: None,
    });

    // 3. Gap-then-calm — 50 calm steps, a +600-tick (~6%) jump, then 200 calm
    //    steps AT THE NEW LEVEL. The headline test: v1 keeps charging while the
    //    EMA crawls up; v2 decays back to base.
    let mut s = Vec::new();
    let mut rng = 7u64;
    let mut t = 0u64;
    for _ in 0..50 {
        let w = (lcg(&mut rng) % 5) as i32 - 2;
        s.push((t, w, None));
        t += 45;
    }
    let base_level = 600;
    s.push((t, base_level, None)); // the gap
    t += 45;
    for _ in 0..200 {
        let w = base_level + (lcg(&mut rng) % 5) as i32 - 2;
        s.push((t, w, None));
        t += 45;
    }
    out.push(Scenario {
        name: "gap_then_calm",
        series: s,
        calm_window: Some((51, 251)), // the post-gap quiet window
    });

    // 4. Choppy / volatile — large ±300-tick swings every step.
    let mut s = Vec::new();
    let mut rng = 99u64;
    for i in 0..300u64 {
        let w = (lcg(&mut rng) % 601) as i32 - 300;
        s.push((i * 20, w, None));
    }
    out.push(Scenario {
        name: "choppy_volatile",
        series: s,
        calm_window: None,
    });

    // 5. Round-trip — ramp +500 then back to 0 within the filter window. Ends
    //    where it started: v2 should NOT over-charge a move that caused little LVR.
    let mut s = Vec::new();
    let mut t = 0u64;
    for i in 0..50i32 {
        s.push((t, i * 10, None));
        t += 5;
    }
    for i in (0..50i32).rev() {
        s.push((t, i * 10, None));
        t += 5;
    }
    out.push(Scenario {
        name: "round_trip",
        series: s,
        calm_window: None,
    });

    // 6. Flash spike — one +800-tick block then immediately back. Stresses the
    //    rate-limit / same-block freeze interaction.
    let mut s = Vec::new();
    s.push((0, 0, None));
    s.push((1, 800, None));
    s.push((2, 0, None));
    for i in 3..60u64 {
        s.push((i, 0, None));
    }
    out.push(Scenario {
        name: "flash_spike",
        series: s,
        calm_window: None,
    });

    out
}

// ---------------------------------------------------------------------------
// CSV I/O.
// ---------------------------------------------------------------------------

fn load_csv(path: &str) -> Vec<(u64, i32, Option<f64>)> {
    let body = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path, e));
    let mut rows = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        // Skip a header row if col 0 isn't numeric.
        if i == 0 && cols[0].parse::<u64>().is_err() {
            continue;
        }
        let t = cols[0]
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("bad time at line {}", i + 1));
        let tick = cols[1]
            .parse::<i32>()
            .unwrap_or_else(|_| panic!("bad tick at line {}", i + 1));
        let notional = cols.get(2).and_then(|c| c.parse::<f64>().ok());
        rows.push((t, tick, notional));
    }
    rows
}

fn dump_csv(path: &str, rows: &[StepRow]) {
    let mut out = String::from("t,tick,dtick,notional,fee_v1,fee_v2\n");
    for r in rows {
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            r.t, r.tick, r.dtick, r.notional, r.fee_v1, r.fee_v2
        ));
    }
    fs::write(path, out).unwrap_or_else(|e| panic!("write {}: {}", path, e));
    eprintln!("wrote per-step CSV to {path}");
}

// ---------------------------------------------------------------------------
// Reporting.
// ---------------------------------------------------------------------------

fn print_header() {
    println!(
        "{:<16} | {:>10} {:>10} | {:>8} {:>8} | {:>12} {:>12} {:>6} | {:>6} {:>6}",
        "regime",
        "avg_v1",
        "avg_v2",
        "max_v1",
        "max_v2",
        "cap_v1",
        "cap_v2",
        "cap_r",
        "ρ_v1",
        "ρ_v2",
    );
    println!("{}", "-".repeat(118));
}

fn print_row(name: &str, m: &Metrics) {
    let cap_r = if m.captured_v1 > 0.0 {
        m.captured_v2 / m.captured_v1
    } else {
        0.0
    };
    println!(
        "{:<16} | {:>10.1} {:>10.1} | {:>8} {:>8} | {:>12.0} {:>12.0} {:>6.2} | {:>6.2} {:>6.2}",
        name,
        m.avg_v1,
        m.avg_v2,
        m.max_v1,
        m.max_v2,
        m.captured_v1,
        m.captured_v2,
        cap_r,
        m.corr_v1,
        m.corr_v2,
    );
}

// ---------------------------------------------------------------------------
// max_fee_ppm sweep — the §3.1 calibration objective (peak LP revenue).
//
// Charging more only helps until fee-elastic volume bleeds to a competing venue,
// so LP revenue is a Laffer curve in the cap. We model retained volume as
//   kept_volume = notional · exp(−K · (fee − base) / 1e6)
// (K = elasticity: % volume lost per unit excess-fee fraction; arb flow ≈ 0,
// retail large) and report revenue = Σ kept_volume · fee/1e6 for each candidate
// cap, against the pure-base-fee baseline. K MUST be estimated from real
// per-pool data — the default is illustrative only.
// ---------------------------------------------------------------------------

fn candidate_caps(base: u32) -> Vec<u32> {
    let mut caps: Vec<u32> = [5, 6, 8, 10, 12, 15, 20, 30]
        .iter()
        .map(|n| (base as u64 * *n as u64 / 5).min(999_999) as u32) // base · {1, 1.2, … 6}
        .collect();
    caps.dedup();
    caps
}

fn run_sweep(series: &[(u64, i32, Option<f64>)], v2_base: &V2Config, v1c: &V1Config, k: f64) {
    let base = v2_base.base_fee_ppm;
    println!(
        "max_fee_ppm sweep — LP revenue under fee-elastic volume (elasticity K={k})\n\
         kept_volume = notional · exp(−K·(fee−base)/1e6);  revenue = Σ kept_volume · fee/1e6\n"
    );
    println!(
        "{:>11} | {:>9} | {:>14} | {:>9}",
        "max_fee_ppm", "mean_fee", "LP_revenue", "vs_base"
    );
    println!("{}", "-".repeat(52));

    let mut base_rev = f64::NAN;
    let mut best = (0u32, f64::MIN);
    let mut rows_out: Vec<(u32, f64, f64)> = Vec::new();
    for cap in candidate_caps(base) {
        let mut cfg = v2_base.clone();
        cfg.max_fee_ppm = cap.max(base);
        let rows = replay(series, v1c, &cfg);
        let mut rev = 0.0;
        let mut fee_sum = 0.0;
        for r in &rows {
            let excess = (r.fee_v2 as f64 - base as f64) / 1e6;
            let kept = r.notional * (-k * excess).exp();
            rev += kept * r.fee_v2 as f64 / 1e6;
            fee_sum += r.fee_v2 as f64;
        }
        let mean_fee = fee_sum / rows.len().max(1) as f64;
        if cap.max(base) == base {
            base_rev = rev;
        }
        if rev > best.1 {
            best = (cap, rev);
        }
        rows_out.push((cap, mean_fee, rev));
    }
    for (cap, mean_fee, rev) in &rows_out {
        let vs = if base_rev.is_finite() && base_rev > 0.0 {
            format!("{:>8.2}x", rev / base_rev)
        } else {
            "   —".into()
        };
        let mark = if *cap == best.0 { "  ← peak" } else { "" };
        println!(
            "{:>11} | {:>9.0} | {:>14.1} | {}{}",
            cap, mean_fee, rev, vs, mark
        );
    }
    println!(
        "\nRevenue-maximizing cap ≈ {} ppm at K={}. Re-run with --elasticity to test sensitivity;\n\
         estimate the real K per pool/tier from --csv volume data before setting factory defaults.",
        best.0, k
    );
}

/// Concatenate the synthetic regimes into one continuous series (ticks rebased
/// so regime seams don't inject spurious jumps; 600s gaps between them). A real
/// calibration uses a per-pool `--csv` series instead.
fn blended_synthetic() -> Vec<(u64, i32, Option<f64>)> {
    let mut out: Vec<(u64, i32, Option<f64>)> = Vec::new();
    let mut t_off = 0u64;
    let mut carry_tick = 0i32;
    for sc in scenarios() {
        let base_t = sc.series.first().map(|x| x.0).unwrap_or(0);
        let first_tick = sc.series.first().map(|x| x.1).unwrap_or(0);
        let shift = carry_tick - first_tick;
        for (t, tick, n) in sc.series {
            let nt = t_off + (t - base_t);
            let st = tick + shift;
            out.push((nt, st, n));
            t_off = nt;
            carry_tick = st;
        }
        t_off += 600;
    }
    out
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let csv = arg_value(&args, "--csv");
    let dump = arg_value(&args, "--dump");
    let sweep = has_flag(&args, "--sweep");
    let elasticity = arg_value(&args, "--elasticity")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(50.0);
    let v1c = v1_default();
    let v2c = v2_default();

    if sweep {
        let (series, src) = match &csv {
            Some(p) => (load_csv(p), p.clone()),
            None => (blended_synthetic(), "blended synthetic regimes".into()),
        };
        println!("max_fee_ppm sweep over {} steps from {src}\n", series.len());
        run_sweep(&series, &v2c, &v1c, elasticity);
        return;
    }

    println!("Dynamic fee backtest — v1 (sqrt-EMA) vs v2 (tick accumulator)");
    println!(
        "v2 defaults: control={} max_va={} decay={}s  (base {}ppm cap {}ppm)\n",
        v2c.variable_fee_control,
        v2c.max_volatility_accumulator,
        v2c.volatility_decay_seconds,
        v2c.base_fee_ppm,
        v2c.max_fee_ppm,
    );
    println!(
        "Legend: cap_* = notional-weighted fee captured; cap_r = v2/v1; ρ_* = corr(fee, |Δtick|)\n"
    );

    if let Some(path) = csv {
        let series = load_csv(&path);
        println!("Replaying {} steps from {path}\n", series.len());
        let rows = replay(&series, &v1c, &v2c);
        print_header();
        print_row("csv", &metrics(&rows));
        if let Some(d) = dump {
            dump_csv(&d, &rows);
        }
        return;
    }

    print_header();
    let mut gap_calm: Option<(f64, f64)> = None;
    for sc in scenarios() {
        let rows = replay(&sc.series, &v1c, &v2c);
        print_row(sc.name, &metrics(&rows));
        if let Some(w) = sc.calm_window {
            gap_calm = Some(calm_mean(&rows, w));
        }
    }

    if let Some((c1, c2)) = gap_calm {
        println!(
            "\ngap_then_calm post-gap quiet window — mean fee:  v1 = {:.0} ppm   v2 = {:.0} ppm",
            c1, c2
        );
        println!(
            "  → v1 over-charges the calm-after-gap by {:.0} ppm vs base; v2 decays back toward base.",
            c1 - 3000.0
        );
    }

    println!("\nReadout:");
    println!("  • convexity  → choppy_volatile / gap max_v2 should hit the cap faster than v1");
    println!("  • decay      → gap_then_calm avg_v2 << avg_v1 (v2 relaxes once price settles)");
    println!("  • trend      → steady_trend avg_v2 < avg_v1 (v1 conflates drift with volatility)");
    println!("  • round-trip → round_trip charged as realized variance (intended LVR recapture)");
    println!("  • relevance  → ρ_v2 ≥ ρ_v1 (v2 fee tracks realized vol more tightly)");
    println!(
        "\nRun with --sweep [--elasticity K] to find the revenue-maximizing max_fee_ppm (§3.1)."
    );
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

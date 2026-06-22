# CLMM Dynamic Fee v2 — Design & Implementation Plan

Status: **proposed (pre-mainnet)**. Author pass: 2026-06-11.
Scope: `choice_clmm_pool` oracle/fee logic + `choice_clmm_factory` defaults.
Backtest harness: [`contracts/choice_clmm_pool/examples/fee_backtest.rs`](../contracts/choice_clmm_pool/examples/fee_backtest.rs).

---

## 1. Motivation

The v1 dynamic fee ([`core/oracle.rs`](../contracts/choice_clmm_pool/src/core/oracle.rs))
charges

```
raw_fee = base_fee_ppm + (|sqrt_p − EMA| / EMA) · volatility_multiplier      (clamped to max_fee_ppm)
```

against a **linear, full-forget EMA** of the sqrt price. It is *fund-safe* and
its quote/execution parity is correct, but as a volatility fee it is crude
compared to Meteora DLMM and Algebra Integral. Four substantive gaps, each
addressed by v2:

| # | v1 problem | Consequence | v2 fix |
|---|---|---|---|
| 1 | Measures **displacement from a lagging anchor** — a *level*, not a *rate* | A pool that gaps to a new price then trades calm keeps charging elevated fees for ~`halflife` while the EMA crawls up (overcharges the post-move calm) | **Decaying accumulator of realized tick-movement** — fee decays to base once trading calms, *regardless of price level* |
| 2 | **Linear** in deviation, then a hard cap | Gentle on the small/mid moves where most adverse selection happens, then flat | **Convex**: fee ∝ `v_a²` (Meteora squares its accumulator) |
| 3 | **One** `halflife` controls both anchor adaptation *and* fee persistence | Can't tune "anchor adapts slowly but fees relax quickly" | **Decoupled** `filter_period` / `decay_period` / `reduction_factor` |
| 4 | `volatility_multiplier` is a raw ppm-per-sqrt-deviation scalar, fragile across tiers (sqrt-space halves the signal) | Per-tier recalibration, easy to misconfigure | Signal is raw **ticks** (1 tick ≈ 1 bp); one tier-independent `variable_fee_control` |

Both safety innovations that v1 added over a textbook design are **kept
verbatim** in v2: the same-block fee freeze and the per-second rate-limit.

### Reference implementations compared

- **Meteora DLMM** — `variable_fee = variable_fee_control · (v_a · bin_step)²`, with a
  volatility accumulator `v_a` that counts bin crossings from a periodically
  re-anchored reference and decays via `filter_period` / `decay_period` /
  `reduction_factor`. v2 borrows the **convex (squared) accumulator + time-decay**
  but **not** the re-anchored-reference accumulation — see the note below.
- **Algebra Integral** — windowed tick-variance from a TWAP oracle fed through a
  double sigmoid. Rejected for v2: storing an observation array is heavier on
  CosmWasm gas/storage than the accumulator, and a hard `max_fee_ppm` cap + `v_a`
  cap already bound the output, so the sigmoid's smooth saturation buys little.

> **Why not Meteora's accumulator verbatim.** Meteora updates `v_a` *during* the
> swap as it crosses bins, measuring `|active_id − index_reference|`. Choice
> computes the fee **once at swap entry** (preserving the v1 quote/execution
> parity architecture), so it only observes price at swap boundaries. Under
> entry-time computation a re-anchored reference is actively wrong: the gapping
> swap re-anchors `index_reference` to the new tick *before* excursion is
> measured, so `|tick − i_r| = 0` and **the gap is never charged** (the Phase-0
> backtest confirmed `max_fee` never left base in `gap_then_calm`). v2 instead
> accumulates the **realized move since the last observation**,
> `|current_tick − last_tick|`, with time-decay — a true realized-volatility
> proxy that captures a one-step gap and needs no separate reference/anchor.

---

## 2. v2 design

### 2.1 `FeeConfig` (`packages/choice_clmm_common/src/pool.rs`)

```rust
pub struct FeeConfig {
    pub base_fee_ppm: u32,                 // KEEP — fee floor
    pub max_fee_ppm: u32,                  // KEEP — hard cap
    // --- volatility accumulator (replaces volatility_multiplier + ema_halflife_seconds) ---
    pub variable_fee_control: u32,         // ppm = control · v_a² / VFEE_SCALE
    pub max_volatility_accumulator: u32,   // cap on v_a, in ticks
    pub volatility_decay_seconds: u32,     // full-forget window: v_a decays to 0 over this idle time
    // --- anti-grief (KEEP, unchanged) ---
    pub max_fee_change_per_second_ppm: u32,
}
```

One time-constant replaces v1's `ema_halflife_seconds`. The coupling problem
(recommendation #3) is resolved *structurally*: the price reference is always
`last_tick` (no smoothing lag), so `volatility_decay_seconds` tunes only fee
persistence and nothing else.

`base_fee_ppm`, `max_fee_ppm`, and `max_fee_change_per_second_ppm` are retained
verbatim, so the factory's base/max logic, the backend `live_fee_ppm` column, and
the FE display keep working untouched.

### 2.2 `OracleData` (`contracts/choice_clmm_pool/src/state.rs`)

```rust
pub struct OracleData {
    pub last_update_time: u64,           // (was last_block_time)
    pub last_tick: i32,                  // tick observed at the last update
    pub volatility_accumulator: u64,     // v_a — decaying accumulator of realized tick-movement
    pub last_fee_ppm: u32,               // KEEP — rate-limit state + same-block freeze value
}
```

`price_ema_x96` is dropped. The oracle has **no consumer outside the fee logic**
(verified by grep — the only non-fee reference was a test), so this is safe.

### 2.3 Core math — `compute_fee` (single pure fn, shared by write + read paths)

```text
delta = now − last_update_time
if delta == 0: return (last_fee_ppm, oracle)              // same-block freeze — KEEP

// 1. Decay the accumulator by idle time (linear full-forget window), then add
//    the realized move observed since the last update.
window  = max(volatility_decay_seconds, 1)
decayed = (delta >= window) ? 0 : v_a · (window − delta) / window
v_a     = min(decayed + |current_tick − last_tick|, max_volatility_accumulator)

// 2. Convex variable fee (u128 intermediate)
variable_ppm = control · v_a² / VFEE_SCALE
raw_fee = min(base_fee_ppm + variable_ppm, max_fee_ppm)

// 3. Rate-limit (KEEP, unchanged)
clamped = clamp(raw_fee, prev_fee ± max_fee_change_per_second_ppm · delta)

persist {last_update_time=now, last_tick=current_tick, v_a, last_fee=clamped}
return clamped
```

`VFEE_SCALE = 1_000_000` (1e6), a fixed const (calibrated so `control = 8800`
adds ~2990 ppm at a 6% move — see §3).

The v1 architecture that makes quotes honest is **preserved exactly**: one
`compute_fee`, with `update_oracle_and_fee` (write) and `simulate_fee`
(read-only twin). The only signature change is `current_tick: i32` in place of
`current_price: Uint256` — every call site already holds `slot0.tick`.

### 2.4 Why this fixes the gap-then-calm defect (the headline win)

- **The move:** the gap is one large `|current_tick − last_tick|` increment →
  `v_a` spikes → convex fee spikes. ✓ captured.
- **Calm afterward:** at the new level each step's increment is tiny while the
  accumulator decays linearly toward 0 over `volatility_decay_seconds`, so the
  fee returns to base **regardless of the new price level**. v1 kept charging
  until the EMA crawled to the new level. ✓ fixed (Phase-0 `gap_then_calm`
  post-gap window: v1 3188 ppm vs v2 3103 ppm and falling).
- **Slow trend:** each step's realized increment is small, so a steady drift
  reads as low volatility → low fee. v1's displacement grew unboundedly with the
  trend. ✓ (Phase-0 `steady_trend`: avg v2 3053 vs v1 3466).
- **Round-trip:** both legs count as realized movement, so a fast round-trip *is*
  charged — a deliberate, **intended** divergence from Meteora's displacement
  model (which under-charges whipsaw). A round-trip is genuine realized variance
  that picks LPs off twice; charging it is LVR recapture, not a bug. It is
  bounded by `max_fee_ppm` and self-policing against wash-griefing (an attacker
  inflating `v_a` pays the inflated fee on their own trades, and the proceeds go
  to the LPs they're trying to grief). See §3.1 for why charging more is the
  right default here.

### 2.5 Why raw ticks (not bins) — tier independence

Meteora multiplies by `bin_step` inside the square so the fee tracks real price
movement regardless of bin granularity. A continuous-tick CLMM gets this for
free: 1 tick = `1.0001×` ≈ 1 bp of price, so accumulating **ticks** makes
`variable_fee_control` a single constant valid across every fee tier / spacing.
`tick_spacing` therefore does **not** appear in the fee math.

---

## 3. Calibration

`VFEE_SCALE = 1e6`; 0.30% tier (`base 3000`, `cap 6000`); 1 tick ≈ 1 bp so a P%
move ≈ `ln(1+P)/ln(1.0001)` ticks (6% ≈ 583 ticks). For a **single-step** move of
this size (`v_a ≈ ticks`):

| Price move | ticks (`v_a`) | `control = 8800` → variable ppm | total fee |
|---|---|---|---|
| 1% | ~100 | 88 | 3088 |
| 3% | ~291 | 745 | 3745 |
| 6% | ~583 | 2990 | ~6000 (cap) |
| 9% | ~862 | 6540 | cap |

Sustained volatility accumulates *above* a single step (each step adds its
increment on top of the decayed prior `v_a`), so choppy markets saturate the cap
faster than the single-step table suggests — exactly the intended convex
toxic-flow capture.

**Proposed starting defaults** (factory, all tiers):

| Param | Default | Rationale |
|---|---|---|
| `variable_fee_control` | `8800` | ~3000 ppm added at a single 6% move on the 0.30% tier |
| `max_volatility_accumulator` | `2000` (ticks ≈ 22% move) | bounds `v_a²` so `control·v_a²` stays well inside u128; far past saturation |
| `volatility_decay_seconds` | `600` | 10-min full-forget window; an elevated fee persists across a move then relaxes to base |
| `max_fee_change_per_second_ppm` | `100` | unchanged from v1 |

### 3.1 Calibration objective — maximize LP revenue, not minimize taker cost

The fee bump fires only during volatility, where the marginal flow is
disproportionately arbitrage/informed — the flow that was adversely selecting
LPs anyway. Taxing it is the LVR-recapture thesis, so **charging more is the
right default**; "more fees for LPs" is the goal, not a side effect to be
minimized. We therefore calibrate to **peak LP fee revenue**, not to minimal
taker cost.

The one real cost of charging more is **volume elasticity**: `revenue =
fee_rate × volume`, and volume bleeds to competing venues as the fee rises. So
revenue is a Laffer curve in the fee cap. The governing knob is **`max_fee_ppm`**
(per tier) — it is the single guardrail that decides how much recapture is too
much before order flow leaves. `variable_fee_control` and
`volatility_decay_seconds` shape *how fast* the fee climbs toward that cap;
`max_fee_ppm` sets *how far*.

Calibration procedure (per fee tier, on real `--csv` series):

1. Estimate the pool's **fee elasticity** of volume (how much volume leaves per
   bp of excess fee over the cheapest competing venue). Arb flow is nearly
   inelastic; retail is elastic — measure both if the data allows.
2. **Sweep `max_fee_ppm`** and pick the cap that maximizes
   `Σ kept_volume(fee) · fee_rate`. The harness `--sweep` mode does this against
   a tunable elasticity (§5.2).
3. Set `variable_fee_control` / `volatility_decay_seconds` so the climb to that
   cap matches the pool's volatility cadence (fast enough to catch real moves,
   not so fast that a single block saturates).

Bias LP-favoring throughout: keep the generous `volatility_decay_seconds`, do
**not** add round-trip dampening, and treat `max_fee_ppm` as the deliberate
revenue-maximizing ceiling rather than a safety afterthought.

These are **starting points**; Phase 0 (backtest) tunes `variable_fee_control`
and `volatility_decay_seconds` against real swap data before mainnet. The convex
shape deliberately charges *less* on sub-2% single moves and *more* on sustained
or large moves than v1 — that is the intent.

### 3.2 Real-data calibration — first pass (NONJA) + the K problem

Run against the one real Choice-listed series available on disk
(`choice_exchange_backend/backups/nonja_4h.csv`, 694 4h candles / 343 days,
close→tick, `v_quote` as volume weight; convert + sweep reproduced in the
session log):

**Fee-path side (estimable from price alone — no fee variation needed):**

| 4h close-to-close move | ticks | v2 fee @ control=8800, cap=6000 |
|---|---|---|
| median | 53 (~0.5%) | 3024 ppm (≈ base) |
| p90 | 202 (~2%) | ~3360 ppm |
| p99 | 1568 (~17%) | 6000 (cap) |
| max | 3745 (~45%) | 6000 (cap) |

4% of candles hit the cap; **volume-weighted average fee 4730 ppm vs simple
median 3024** — volume concentrates in volatile candles, and those pay more
(LVR-capture thesis, confirmed on real data). This validates that `control=8800`
keeps calm periods at base while large moves saturate — the magnitude side of the
calibration is sound. ⚠️ **Granularity caveat:** 4h candles ≫ the 600s decay, so
each step is charged as one big move; this validates `control`/`max_fee`
*magnitudes* but **not** the per-swap decay dynamics. Per-swap (or ≤1m) data is
needed for `volatility_decay_seconds`, and only exists in the prod DB
(`analytics_clmmswap`: `block_time`, `final_tick`, `fee_amount`, `live_fee_ppm`).

**Revenue side (`--sweep` on the real series, real volume weights):** the
optimal cap is **entirely K-bound** —

| elasticity K | revenue-max cap | revenue vs base |
|---|---|---|
| 0 (perfectly inelastic) | ≥18000 ppm | 2.98× |
| 20 | ≥18000 | 2.43× |
| 50 | ≥18000 | 1.84× |
| 100 | 9000 | 1.36× |
| 200 | 4800 | 1.08× |

**The K problem (unresolved — needs data not reachable from the workspace).** No
Choice DB is accessible locally (the running `:5432` is an unrelated stack; the
prod snapshot `:5434` is down), and a single fixed-fee token series has no fee
variation to regress against. K **cannot** be estimated from on-disk data.

**Structural prior (argues K is LOW for *this* fee):** the dynamic surcharge bites
only during volatility, when flow is arbitrage-dominated. Arbs are near-inelastic
in the revenue-relevant sense — raising the fee kills only the *marginal* arbs
(edge ≈ fee, extracting little LVR) while the large arbs (edge ≫ fee) keep firing
and now pay more. Elastic retail flow trades mostly in calm windows where the fee
is at base anyway, so its higher elasticity barely touches the dynamic component.
This argues the *effective* K for the dynamic fee sits low (≈20–50), i.e. toward
the high-cap end of the table — but weigh non-revenue factors (competitiveness,
optics) before adopting a 1.5%+ cap.

**To actually pin K (run on prod, in priority order):**

1. **Own arb-bot edge distribution (best, immediately available to us).** For the
   target pools, the CDF of observed arb edges *is* the retention curve: arb
   volume surviving a fee `f` = volume with edge > f. Fit
   `kept(f)/kept(base)` to the harness's `exp(−K·(f−base)/1e6)` → K directly, and
   it is the K for exactly the flow this fee taxes.
2. **Post-mainnet `live_fee_ppm` micro-regression.** Bin `analytics_clmmswap` by
   `live_fee_ppm` level, regress `ln(volume per unit time)` on the fee level
   controlling for realized volatility (the confound: fee and volume both rise
   with vol). Cleanest direct estimator once CLMM has real mainnet volume.
3. **Cross-venue volume share.** For pairs on both an AMM and Helix, regress
   `logit(amm_share)` on the effective cost gap (`fee_bps` − `taker_fee_rate`)
   per time bin across `analytics_choiceswap` / `analytics_orderbookmarkettrade`;
   slope ≈ −K. Available now but mixes venue-quality confounds.

---

## 4. Implementation phases

- **Phase 0 — Backtest harness (this commit).** Standalone host-target reference
  implementations of v1 and v2 replayed over synthetic regimes and (optionally)
  real `timestamp,tick` series, emitting comparison tables and per-step CSV.
  Locks in defaults. See §5.
- **Phase 1 — Types.** Rewrite `FeeConfig` (`pool.rs`) and `OracleData`
  (`state.rs`). Add `VFEE_SCALE`. `initialize_oracle` seeds `index_reference_tick`
  from the initial tick (signature takes tick, not price — `contract.rs` already
  computes it).
- **Phase 2 — Core logic.** Rewrite `oracle.rs` `compute_fee` per §2.3; keep the
  `update_oracle_and_fee` / `simulate_fee` twins and the `delta==0` freeze.
  All-new unit tests: convexity, decay-to-base, round-trip, filter-period
  accumulation, rate-limit interaction, quote/swap parity, overflow at extremes.
- **Phase 3 — Call sites.** Swap (`swap.rs`, 4 sites), flash (`flash.rs`, 1),
  `GetDynamicFee` query (`contract.rs`): pass `slot0.tick` / `state.tick`.
- **Phase 4 — Instantiate validation** (`contract.rs`): replace the
  `ema_halflife` / `volatility_multiplier` bounds with: `volatility_decay_seconds`
  in a sane window (e.g. `60..=86_400`, as the old halflife), `max_volatility_accumulator`
  bounded (overflow guard, e.g. `≤ 2·MAX_TICK`), `variable_fee_control` bounded.
  Keep `base ≤ max`, both `< 1e6`.
- **Phase 5 — Factory** (`factory/src/contract.rs` ~line 398): replace the one
  production `FeeConfig` literal with the new fields + Phase-0 defaults; rewrite
  the explanatory comment.
- **Phase 6 — Test fixtures.** Update the ~40 `FeeConfig { … }` literals across
  `tests.rs`, `regime_tests.rs`, `solvency_fuzz.rs`, `adversarial_fuzz.rs`,
  `exploit_regressions.rs`, `factory/test.rs` (shared helper where possible).
  Re-point EMA-specific regime/exploit tests to accumulator semantics.
- **Phase 7 — Frontend (cosmetic).** Rename the 3 optional fields in
  `ClmmAPI.ts`. `GetDynamicFee` / `live_fee_ppm` unchanged → no backend changes.
- **Phase 8 — Validation.** `cargo test` (host) → `./build_release.sh` (NOT
  `make build-all` for integration artifacts — bulk-memory gotcha) → integration
  + fuzz/mutation suites. Update `docs/choice_clmm.md` oracle section and the
  `choice_exchange/CLAUDE.md` "Dynamic fees" line.

### Blast radius (verified)

| Surface | Impact |
|---|---|
| `choice_clmm_pool` (oracle/state/contract/swap/flash) | rewritten — core of the change |
| `choice_clmm_factory` one `FeeConfig` literal | new fields/defaults |
| ~40 test `FeeConfig` literals | mechanical update |
| `ClmmAPI.ts` 3 optional fields | rename (non-breaking) |
| Backend, router/aggregator, `DynamicFeeResponse { fee_ppm }`, `GetDynamicFee` | **untouched** — same wire shape |

### Out of scope (v2.1)

Intra-swap progressive fee that taxes the **initiator** across crossed ticks
(Meteora-exact). v2 keeps entry-time computation — matches current behavior
(initiator cheap, followers taxed) and avoids a chicken-and-egg change to
`compute_swap_step`. The backtest quantifies whether it's worth pursuing.

---

## 5. Backtest harness

[`examples/fee_backtest.rs`](../contracts/choice_clmm_pool/examples/fee_backtest.rs)
is a host-target example (no test-tube, no wasm). It contains standalone
reference implementations of **both** the v1 fee (ported from `oracle.rs`,
sqrt-space EMA) and the v2 fee (the §2.3 spec — this becomes the porting
reference for Phase 2), and replays a `(time, tick)` series through both.

```bash
cd choice_exchange

# Synthetic regimes (calm / trend / gap-then-calm / choppy / round-trip / flash-spike):
cargo run --example fee_backtest

# Replay a real series (CSV: `timestamp_seconds,tick[,notional]`, header optional)
# and dump per-step fees for plotting:
cargo run --example fee_backtest -- --csv path/to/series.csv --dump out.csv
```

For each regime it reports, for v1 vs v2: average fee, max fee, fee captured
(notional-weighted), correlation of fee with per-step realized vol (`|Δtick|`),
and a **calm-after-gap overcharge** metric (mean fee in the quiet window
following a gap — the v1 defect v2 targets). Real per-pool `(time, tick)` series
come from the backend CLMM swap index; feed them via `--csv` to ground the
final defaults before mainnet.

### 5.1 Phase-0 results (synthetic, proposed defaults)

The harness already paid for itself: it caught (a) the re-anchored-reference flaw
that left the gap uncharged under entry-time computation, and (b) a `VFEE_SCALE`
calibration error (`1e9` → `1e6`). With the corrected v2 and the §3 defaults:

| regime | avg v1 | avg v2 | max v1 | max v2 | captured v2/v1 | reads |
|---|---|---|---|---|---|---|
| calm | 3006 | 3001 | 3018 | 3004 | 1.00 | both ≈ base ✓ |
| steady_trend | 3466 | 3053 | 3500 | 3057 | 0.88 | v2 ignores slow drift ✓ |
| gap_then_calm | 3163 | 3094 | **6000** | **6000** | 0.98 | v2 captures the gap, then decays ✓ |
| choppy_volatile | 3774 | 5982 | 5715 | 6000 | **1.55** | v2 captures sustained vol ✓ |
| round_trip | 3935 | 4400 | 5035 | 6000 | 1.12 | v2 charges realized round-trip variance |
| flash_spike | 3008 | 5225 | 3100 | 6000 | 1.59 | v2 reacts where v1 barely did ✓ |

Post-gap quiet window mean fee: **v1 3188 ppm vs v2 3103 ppm** (and falling) —
the gap-then-calm overcharge is materially reduced. These are *synthetic*
regimes; the numbers validate direction and shape, not final parameters — tune
`variable_fee_control` / `volatility_decay_seconds` on real `--csv` series before
locking factory defaults.

> ⚠️ **Keep `v2_step` in the harness in lockstep with the contract once Phase 2
> lands** — it is the executable spec. A divergence there silently invalidates
> every calibration run.

### 5.2 `--sweep` — the peak-LP-revenue objective (§3.1)

```bash
cargo run --example fee_backtest -- --sweep                      # blended synthetic
cargo run --example fee_backtest -- --sweep --elasticity 200     # test sensitivity
cargo run --example fee_backtest -- --sweep --csv pool.csv       # real per-pool series
```

`--sweep` evaluates a ladder of `max_fee_ppm` caps and reports LP revenue under a
fee-elastic volume model, `kept_volume = notional · exp(−K·(fee−base)/1e6)`, with
`K` the elasticity (`--elasticity`, default 50). It prints revenue vs the
pure-base-fee baseline per cap and marks the revenue-maximizing one. This makes
§3.1 measurable: the optimal cap depends entirely on `K`.

Phase-0 illustration (blended synthetic):

| flow type | K | revenue-max cap | revenue vs base |
|---|---|---|---|
| inelastic (arb-heavy) | 50 | ≥ 18000 ppm (highest tested) | **1.15×** — charge as much as the cap allows |
| elastic (retail) | 300 | ~3600 ppm | ~1.00× — extra fee just sheds volume |

So the cap is a per-pool revenue decision, not a safety number. `K` **must** be
estimated from real volume-vs-fee data per pool/tier before factory defaults are
set — the model is the framework, not the answer.

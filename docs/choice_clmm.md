# Choice CLMM — Concentrated Liquidity

Uniswap V3-style concentrated liquidity AMM for Injective. Three contracts plus two shared packages. Supports both native tokens and CW20 tokens via the `AssetInfo` enum.

## Contracts

### choice_clmm_pool (`contracts/choice_clmm_pool/`)

Core pool contract. Manages tick-based liquidity, swaps, fee accounting, and a dynamic fee oracle.

**Layout:** `contract.rs`, `state.rs`, `error.rs`, `actions/` (mint, swap, burn, collect), `core/` (positions, ticks, bitmap, oracle)

**Storage (`state.rs`):**

```rust
POOL_CONFIG: Item<PoolConfig>          // factory, token0: AssetInfo, token1: AssetInfo, tick_spacing, fee_config
POOL_STATE: Item<PoolState>            // sqrt_price (Q96), tick, liquidity (active L)
TICKS: Map<i32, TickInfo>              // per-tick: liquidity_delta, active_positions_count, fee_growth_outside_0/1, initialized
POSITIONS: Map<(&str, i32, i32), PositionInfo>  // (owner, lower, upper) -> liquidity, fee checkpoints, tokens_owed
TICK_BITMAP: Map<i16, Uint256>         // word_pos -> 256-bit bitmap of initialized ticks
FEE_GROWTH_GLOBAL_0: Item<Uint256>     // cumulative fees per unit liquidity for token0
FEE_GROWTH_GLOBAL_1: Item<Uint256>     // same for token1
ORACLE: Item<OracleData>              // last_update_time, last_tick, volatility_accumulator, last_fee_ppm
```

**Key types (from `packages/choice_clmm_common/`):**

```rust
AssetInfo::NativeToken { denom: String } | AssetInfo::Token { contract_addr: String }  // unified token type
PoolState { sqrt_price: Uint256, tick: i32, liquidity: Uint128 }
PoolConfig { factory: Addr, token0: AssetInfo, token1: AssetInfo, tick_spacing: u32, fee_config: FeeConfig }
FeeConfig { base_fee_ppm: u32, max_fee_ppm: u32, variable_fee_control: u32, max_volatility_accumulator: u32, volatility_decay_seconds: u32, max_fee_change_per_second_ppm: u32 }
PositionInfo { liquidity: u128, fee_growth_inside_0_last: Uint256, fee_growth_inside_1_last: Uint256, tokens_owed_0: Uint128, tokens_owed_1: Uint128 }
TickInfo { active_positions_count: u128, liquidity_delta: i128, fee_growth_outside_0: Uint256, fee_growth_outside_1: Uint256, initialized: bool }
```

**token0 is always ordered before token1** (NativeToken < Token; within same variant, lexicographic by key).

#### Actions

**Mint (`actions/mint.rs`)** — Add liquidity to a tick range

1. Validate tick range (divisible by spacing, lower < upper, within bounds)
2. Compute sqrt prices from ticks via `get_sqrt_ratio_at_tick()`
3. Initialize ticks if needed — set `fee_growth_outside` to global fees if tick <= current, else 0
4. Flip tick in bitmap on first use
5. Call `update_position()` to checkpoint accumulated fees
6. If range includes current price, add liquidity to pool state
7. Calculate required token amounts based on position vs current price:
   - Below current tick: only token0
   - Above current tick: only token1
   - Spanning current tick: both tokens
8. Pull funds: native tokens verified from `info.funds`; CW20 tokens pulled via `TransferFrom` (requires prior allowance)

**Swap (`actions/swap.rs`)** — Token exchange through tick ranges

1. Update oracle volatility accumulator (decay by idle time, add realized tick-move)
2. Compute dynamic fee (convex in realized volatility): `fee = base + control * v_a^2 / 1e6`, capped at max
3. Loop through initialized ticks:
   - Find next initialized tick via bitmap (`next_initialized_tick_in_chunk`)
   - Compute swap step with `compute_swap_step()` (input consumed, output, fees)
   - Accumulate global fee growth: `fee_amount * 2^128 / liquidity`
   - If crossing a tick: flip fee_growth_outside, apply liquidity_delta to active liquidity
4. Save updated price, tick, liquidity
5. Transfer tokens: native input verified from `info.funds` (excess refunded); CW20 input pulled via `TransferFrom` or already held via `Receive` hook. Output sent via `BankMsg::Send` (native) or `Cw20ExecuteMsg::Transfer` (CW20)

**Burn (`actions/burn.rs`)** — Remove liquidity

1. Call `update_position()` to collect accrued fees into tokens_owed
2. Compute principal token amounts from burned liquidity
3. Decrease tick counters; uninitialize and remove from bitmap if empty
4. Credit principal to tokens_owed on position

**Collect (`actions/collect.rs`)** — Claim tokens_owed (fees + burned principal)

1. Load position
2. Transfer requested amounts (capped at tokens_owed) via `BankMsg::Send` (native) or `Cw20ExecuteMsg::Transfer` (CW20)
3. Decrement tokens_owed

#### Core Algorithms

**Fee accounting (`core/positions.rs`, `core/ticks.rs`)** — "Outside model" from Uniswap V3

Fee growth inside a range [lower, upper] at current tick:

```text
fee_below = if current >= lower: tick_lower.fee_outside else: global - tick_lower.fee_outside
fee_above = if current < upper:  tick_upper.fee_outside else: global - tick_upper.fee_outside
fee_inside = global - fee_below - fee_above
```

Position's uncollected fees:

```text
tokens_owed += liquidity * (fee_inside_now - fee_inside_last) / 2^128
```

All fee growth uses wrapping U256 arithmetic (intentional overflow at MAX).

**Tick bitmap (`core/bitmap.rs`)** — Efficient next-tick lookup

- Each tick compressed by tick_spacing before bitmap ops
- `position(tick)` -> `(word_pos: i16, bit_pos: u8)` where `word_pos = tick >> 8`, `bit_pos = tick & 0xff`
- `flip_tick()` toggles bit, removes word if zeroed
- `next_initialized_tick_in_chunk()` finds next set bit (MSB for zero_for_one, LSB for one_for_zero)

**Dynamic fees (`core/oracle.rs`)** — convex realized-volatility pricing (v2)

The fee tracks a **decaying accumulator of realized tick-movement** fed through a
**convex (squared)** fee. The single pure `compute_fee` is shared by the write
path (`update_oracle_and_fee`) and the read-only quote twin (`simulate_fee`), so
a `Quote` returns exactly what a same-block swap charges:

```text
delta = now - last_update_time
if delta == 0: return last_fee_ppm                         # same-block freeze (anti-manipulation)

# 1. decay the accumulator by idle time (linear full-forget window), then add
#    the realized move observed since the last update:
window  = max(volatility_decay_seconds, 1)
decayed = (delta >= window) ? 0 : v_a * (window - delta) / window
v_a     = min(decayed + |current_tick - last_tick|, max_volatility_accumulator)

# 2. convex variable fee (u128 intermediate for the square; VFEE_SCALE = 1e6):
variable = control * v_a^2 / VFEE_SCALE
raw_fee  = min(base_fee + variable, max_fee)

# 3. rate-limit the change per elapsed second, then persist:
fee = clamp(raw_fee, prev_fee +/- max_fee_change_per_second_ppm * delta)
persist { last_update_time = now, last_tick = current_tick, v_a, last_fee = fee }
```

The signal is raw **ticks** (1 tick ≈ 1 bp of price), so the calibration is
**tier-independent** — no sqrt-space halving and no per-tier recalibration. The
factory's default calibration (`variable_fee_control = 8800`,
`max_volatility_accumulator = 2000` ticks, `volatility_decay_seconds = 600`,
`max_fee = base * max_fee_multiple` (default 2x, up to 10x for launchpad
graduations), rate limit 100 ppm/s) means a single ~6% move (~583 ticks) adds
~2990 ppm — enough to saturate the 0.30% tier to its default 0.60% cap; sustained
chop accumulates *above* a single step and saturates faster, while a slow trend
reads as small per-step increments and stays near base.

Why this shape (vs the v1 sqrt-EMA-displacement fee it replaces): the accumulator
measures a *rate* of realized movement, not displacement from a lagging anchor, so
the fee decays to base once trading calms **regardless of the new price level**
(v1 kept charging while the EMA crawled up after a gap); the convex square charges
proportionally more on large/sustained moves; and the instantaneous `last_tick`
reference means `volatility_decay_seconds` is the *only* time constant (it tunes
fee persistence alone). A fast round-trip is charged on both legs — deliberate
LVR recapture, bounded by `max_fee` and self-policing against wash-griefing.
Design-of-record: [`docs/clmm_dynamic_fee_v2_plan.md`](clmm_dynamic_fee_v2_plan.md);
executable spec / backtest: `contracts/choice_clmm_pool/examples/fee_backtest.rs`.

Safety: every step is checked/saturating arithmetic, the square uses a `u128`
intermediate, the used `v_a` is capped before squaring, and `raw_fee` is provably
`< 1_000_000` (it is `min(_, max_fee)` with `max_fee` constructor-bounded
`< 1_000_000`) — which keeps the downstream `as u32` cast and `compute_swap_step`'s
fee-denominator check safe.

Protocol fees default **ON** per the Uniswap v3 deployment table — divisor 4
(25% of swap fees) for tiers <= 0.05%, divisor 6 (~16.7%) above — with the
treasury defaulting to the factory owner and the burn-auction split off. The
factory owner retunes per pool via `SetFeeProtocol` (0 | 4..=10) and
`UpdateProtocolFeeConfig`.

**Messages (`packages/choice_clmm_common/src/pool.rs`):**

- `Mint { recipient, lower_tick, upper_tick, amount, data }` — add liquidity (native funds attached; CW20 pulled via TransferFrom)
- `Swap { recipient, zero_for_one, amount_specified, sqrt_price_limit_x96 }` — low-level swap with explicit direction
- `SwapExactInput { minimum_amount_out, recipient, deadline }` — user-friendly swap; direction inferred from attached native funds
- `Receive(Cw20ReceiveMsg)` — CW20 hook entry point; inner msg is `Cw20HookMsg::SwapExactInput { minimum_amount_out, recipient, deadline }`
- `Burn { lower_tick, upper_tick, amount }` — remove liquidity
- `Collect { recipient, lower_tick, upper_tick, amount0_requested, amount1_requested }` — claim fees/principal
- Query `GetSlot0` — returns current sqrt_price, tick, liquidity
- Query `GetConfig` — returns PoolConfig
- Query `Quote { token_in: AssetInfo, amount_in }` — simulate swap, returns `{ amount_out, amount_in_consumed, fee_amount }`

---

### choice_clmm_factory (`contracts/choice_clmm_factory/`)

Creates CLMM pools with deterministic addresses.

**Storage (`state.rs`):**

```rust
CONFIG: Item<Config>                          // owner, pool_code_id
FEE_TIERS: Map<u32, u32>                     // fee_pips -> tick_spacing
POOLS: Map<(String, String, u32), String>     // (token0, token1, fee) -> pool_address
TMP_INSTANTIATE_INFO: Item<TmpInstantiateInfo> // transient state for reply handler
```

**Default fee tiers (set on instantiate):**

- 100 pips (0.01%) -> tick spacing 1
- 500 pips (0.05%) -> tick spacing 10
- 3000 pips (0.30%) -> tick spacing 60
- 10000 pips (1.00%) -> tick spacing 200

**Pool creation:** Uses `Instantiate2` with `SHA256(token0 + token1 + fee)` as salt for deterministic addresses. Pool address stored via reply handler.

**Messages (`packages/choice_clmm_common/src/factory.rs`):**

- `CreatePool { token_a: AssetInfo, token_b: AssetInfo, fee, init_sqrt_price, max_fee_multiple }` — create a new pool (tokens auto-sorted; CW20 addresses validated). `max_fee_multiple` (`Option`, 2..=10, default 2) sets the dynamic-fee ceiling to `fee * multiple`; launchpad graduations pass up to 10.
- `EnableFeeAmount { fee, tick_spacing }` — add new fee tier (owner only)
- `UpdateConfig { owner, pool_code_id }` — update factory config (owner only)
- Query `GetPool { token_a: AssetInfo, token_b: AssetInfo, fee }` — returns pool address

---

### choice_clmm_manager (`contracts/choice_clmm_manager/`)

NFT wrapper for CLMM positions. Built on `cw721-base`.

**Storage (`state.rs`):**

```rust
CONFIG: Item<Config>                    // factory address
TOKEN_ID_COUNTER: Item<u64>            // auto-incrementing NFT ID
POSITIONS: Map<u64, Position>          // token_id -> Position { token0: AssetInfo, token1: AssetInfo, fee, tick_lower, tick_upper, pool_address }
```

**Messages (`packages/choice_clmm_common/src/manager.rs`):**

- `MintPosition { token0: AssetInfo, token1: AssetInfo, fee, tick_lower, tick_upper, amount0_desired, amount1_desired, amount0_min, amount1_min, recipient, deadline }` — create position NFT + mint liquidity. Native funds attached; CW20 tokens require prior approval of the manager contract
- `IncreaseLiquidity { token_id, amount0_desired, amount1_desired, amount0_min, amount1_min, deadline }` — add more liquidity to existing position (same funding rules)
- `DecreaseLiquidity { token_id, liquidity, amount0_min, amount1_min, deadline }` — remove liquidity (requires NFT ownership)
- `Collect { token_id, recipient }` — claim accumulated fees
- `Burn { token_id }` — burn position NFT (only when liquidity = 0)

**CW20 flow in manager:** For CW20 tokens, the manager uses a two-step approach: (1) `TransferFrom` to pull tokens from user to manager, (2) `IncreaseAllowance` to approve the pool, then sends `Mint` to pool. This avoids requiring users to approve both manager and pool.

**Flow:** Manager queries factory for pool address, calculates liquidity from amounts using `get_liquidity_for_amounts()`, sends Mint/Burn/Collect to pool, and mints/manages the CW721 NFT.

---

## Math Library: `packages/choice_clmm_math/`

Pure math — no storage, no CosmWasm deps beyond Uint types.

**Tick math (`tick_math.rs`):**

- `MIN_TICK = -887272`, `MAX_TICK = 887272`
- `MIN_SQRT_RATIO = 4295128739`, `MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970341`
- `get_sqrt_ratio_at_tick(tick) -> Uint256` — computes `sqrt(1.0001^tick) * 2^96` using bit-shift decomposition with precomputed constants
- `get_tick_at_sqrt_ratio(sqrt_price) -> i32` — binary search for largest tick where `sqrt_ratio(tick) <= input`

**Sqrt price math (`sqrt_price_math.rs`):**

- `get_amount0_delta(sqrt_a, sqrt_b, liquidity, round_up)` — token0 amount for a liquidity+range: `L * (sqrt_upper - sqrt_lower) / (sqrt_upper * sqrt_lower)`
- `get_amount1_delta(sqrt_a, sqrt_b, liquidity, round_up)` — token1 amount: `L * (sqrt_upper - sqrt_lower) / 2^96`
- `get_next_sqrt_price_from_input(...)` — new price after adding input tokens
- `get_next_sqrt_price_from_output(...)` — new price after removing output tokens

**Liquidity math (`liquidity_math.rs`):**

- `get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount0)` — `L = amount0 * sqrt_lower * sqrt_upper / (sqrt_upper - sqrt_lower)`
- `get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount1)` — `L = amount1 * 2^96 / (sqrt_upper - sqrt_lower)`
- `get_liquidity_for_amounts(sqrt_current, sqrt_lower, sqrt_upper, amount0, amount1)` — takes min of the two constraints based on where current price falls

**Swap math (`swap_math.rs`):**

- `compute_swap_step(sqrt_current, sqrt_target, liquidity, amount_remaining, fee_pips, zero_for_one)` — returns `SwapStepResult { sqrt_ratio_next, amount_in, amount_out, fee_amount }`
- Full step to target if enough input; partial step otherwise. Fee deducted from input.

**Full math (`full_math.rs`):**

- `mul_div(a, b, denom)` — `(a * b) / denom` via Uint512 intermediate to prevent overflow
- `mul_div_round_up(a, b, denom)` — same with ceiling rounding

## Common Types: `packages/choice_clmm_common/`

Shared message and state types across all three CLMM contracts.

- `types.rs` — `AssetInfo` enum (`NativeToken { denom }` | `Token { contract_addr }`) with `transfer_msg()`, `transfer_from_msg()`, `increase_allowance_msg()` helpers. Ordered: NativeToken < Token, lexicographic within variant
- `pool.rs` — ExecuteMsg (incl. `Receive`, `SwapExactInput`), `Cw20HookMsg`, QueryMsg, PoolState, PoolConfig, FeeConfig, TickInfo, QuoteResponse
- `factory.rs` — ExecuteMsg, QueryMsg, Config
- `manager.rs` — ExecuteMsg, QueryMsg, Position (with `AssetInfo` token fields)

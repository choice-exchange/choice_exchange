# CLMM Graduation Plan — Seed CLMM Pools from the Launchpad + Locked-Liquidity Fee Collector

**Status:** Contracts implemented + unit-tested (60 unit tests green, clippy clean) · integration test stubbed pending a CLMM test-tube harness · keeper wiring (Phase 6) not done · still gated on the CLMM stack reaching mainnet (see [clmm_extensions_plan.md](./clmm_extensions_plan.md))
**Goal:** Let a launchpad-style launch *graduate to a CLMM pool* instead of (or in addition to) the legacy XYK pair, and lock the seeded liquidity permanently behind a thin wrapper that can still collect trading fees forever.
**Scope:** Concentrated in `choice_pool_seeder` (new CLMM seeding mode + a `Locker` role). `choice_mts_issuer` is unchanged — it relays the seeder payload opaquely. No changes to the CLMM contracts themselves.

---

## Design decisions (locked)

| Decision | Choice | Rationale |
|---|---|---|
| Wrapper shape | **`Role::Locker` in the `choice_pool_seeder` binary** | Seeder is already a single-crate, role-via-instantiate binary (Factory/Sink). A third role reuses the deploy + audit pipeline and inherits the factory's Instantiate2 addressing, so the locker address is **derivable from `internal_id` before the sink exists** — which is the binding constraint (the sink must mint the NFT to a known address). Cleaner than a standalone code-id, less muddy than welding fee-collection onto the one-shot sink. |
| Fee routing | **Single configurable beneficiary** | `manager.Collect` sends fees straight to one address (e.g. project treasury). Splitting/burning is left to an off-contract step or a later iteration. |
| XYK vs CLMM | **Keep both, selectable** via `pool_kind` enum on `SinkInit` | Existing XYK launches unaffected; new launches pick their graduation venue. |
| LP range | **Full-range only** (`MIN_TICK..MAX_TICK` aligned to the fee tier's tick spacing) | Mimics XYK constant-product, never goes out of range, needs no rebalancing, and consumes both seed balances modulo rounding dust. |

Standalone-locker fallback: if a generic "lock any CLMM position" service is ever wanted outside the launchpad, split `Role::Locker` into its own crate/code-id. The ABI is identical, so this is a packaging change only.

---

## Background — what exists today

**Current XYK seeding** (`choice_pool_seeder` sink `Settle`, [contract.rs](../contracts/choice_pool_seeder/src/contract.rs)):
1. Reads its two bank balances (`token_denom`, `pair_denom`); requires both non-zero.
2. Requires exact tokenfactory **create-denom fee** in `info.funds` (the XYK pair mints an LP bank denom).
3. Deducts `tip_bps` from the pair balance → tip to caller.
4. `choice_factory.CreatePair { assets: [token, pair] }` → self-callback `ProvideLiquidity` → self-callback `DistributeLp` routes the minted LP bank denom per `lp_destination` (`Burn` | `SendTo`).

**CLMM stack** (pre-mainnet):
- `choice_clmm_factory.CreatePool { token_a, token_b, fee, init_sqrt_price }` — **no funds/fee**, deterministic Instantiate2, registers `POOLS[(t0,t1,fee)]`, internal reply writes the registry. **Requires an explicit `init_sqrt_price` (Q64.96).**
- `choice_clmm_manager.MintPosition { token0, token1, fee, tick_lower, tick_upper, amount{0,1}_desired, amount{0,1}_min, recipient, deadline }` — resolves the pool from the factory itself (`GetPool`), reads slot0, computes liquidity via `get_liquidity_for_amounts`, accepts **native funds and refunds surplus to the sender**, mints a **cw721 position NFT to `recipient`**. Does almost all the seeding work.
- `manager.Collect { token_id, recipient }` — NFT owner/approved collects accrued fees pro-rata. **A contract may own the NFT and collect**, and there is no obligation to ever decrease liquidity ⇒ "locked liquidity that still earns fees."
- Fee tiers / tick spacing (factory defaults): `100→1`, `500→10`, `3000→60`, `10000→200`. Bounds: `MIN_TICK=-887272`, `MAX_TICK=887272`.

**Issuer** (`choice_mts_issuer`): `RegisterLaunch` forwards an opaque, pre-serialized `create_sink_payload` to the seeder factory and is otherwise DEX-agnostic; its only XYK touch is the *optional* `AddNativeTokenDecimals(choice_factory)` dust call. CLMM launches pass `choice_factory: None`.

---

## Phase plan

| # | Phase | Scope | Status |
|---|-------|-------|--------|
| 1 | `pool_kind` plumbing | `SinkInit` enum (`Xyk`/`Clmm`), stored config, role/query updates | ☑ Done |
| 2 | CLMM `Settle` | sort + on-chain `init_sqrt_price` + CreatePool + MintPosition + dust sweep | ☑ Done |
| 3 | `Role::Locker` | new role + `CreateLocker` on factory + `CollectFees` + beneficiary rotation | ☑ Done |
| 4 | sqrt-price + tick math | `init_sqrt_price` from seed ratio; full-range tick alignment helpers + unit tests | ☑ Done |
| 5 | Tests | unit tests for math + CLMM settle chain + locker; integration test stubbed (`#[ignore]`) pending a CLMM test-tube harness | ◐ Unit done, integ stubbed |
| 6 | Docs / deploy notes | wiring for keeper (derive locker addr, build CLMM `create_sink_payload`) | ☐ |

Legend: ☐ not started · ◐ in progress · ☑ done

## As-built notes (deviations from the original plan)

- **`SinkInit` restructured, not extended.** `choice_factory` + `lp_destination` moved *into* a new `PoolKind::Xyk { choice_factory, lp_destination }` variant; `PoolKind::Clmm { clmm_factory, clmm_manager, fee_tier, position_recipient }` is the new path. Clean break (pre-mainnet), so old serialized payloads don't deserialize — acceptable.
- **Factory pins both DEXes.** `FactoryInit` gained `clmm_factory: Option<String>` + `clmm_manager: Option<String>` (all-or-nothing — `ClmmHalfConfigured` otherwise). `CreateSink`/`CreateLocker` validate the sink/locker addresses against these pins, preserving the "factory pins the DEX deployment" audit story.
- **Locker reuses `sink_code_id`.** The seeder binary serves all three roles, so `CreateLocker` instantiate2's against the same code-id. `CreateLocker` also pins `locker_init.manager` to the factory's `clmm_manager`.
- **No sink-side reply IDs.** `Settle` enqueues `CreatePool` → `MintPosition` → `SweepDust` as sequential top-level messages; the factory's internal create reply registers the pool before `MintPosition` runs, and the manager re-resolves the pool by `(token0,token1,fee)`, so the sink never needs the pool address.
- **`MintPosition` uses `deadline: 0`** (manager treats 0 as "no deadline") and `amount*_min: 0` (safe — pool is created at our price in the same atomic tx).
- **Dust sweep → `refund_receiver`.** The manager refunds one-sided surplus to the sink; `CallbackMsg::SweepDust` forwards leftover `token_denom`/`pair_denom` there (no-op if zero).
- **Pre-existing-pool guard** = query `GetPool`; any non-error response ⇒ `ClmmPoolAlreadyExists` (refuse to seed into a pre-priced pool). The factory's own "Pool already exists" revert is a second backstop.
- **`init_sqrt_price` lives in `src/clmm.rs`** (not `choice_clmm_math`) to avoid a math-crate dep; `isqrt` via `Uint512` (blanket `Isqrt` impl, confirmed available). Constants (`MIN/MAX_TICK`, `MIN_SQRT_RATIO`, max-sqrt) reproduced locally.
- **Locker `CollectFees { token_id: Option }`** — `Some` collects one; `None` enumerates owned NFTs (`manager.Tokens`, capped at 30) and collects each. Fees route pool → manager → `beneficiary` via `Collect { recipient }`, never touching the locker.
- **Issuer unchanged**, as planned (relays the opaque `create_sink_payload`).

---

## Phase 1 — `pool_kind` plumbing

### Design
Add a `pool_kind` discriminator to `SinkInit` so the same sink binary seeds either venue. Keep `Xyk` byte-identical to today so existing serialized payloads still deserialize (use `#[serde(...)]` defaulting or an explicit tagged enum — verify wire-compat against any in-flight payloads; pre-mainnet so a clean break is acceptable if simpler).

```rust
// msg.rs
pub enum PoolKind {
    Xyk { lp_destination: LpDestination },                 // existing behaviour
    Clmm {
        clmm_factory: String,
        clmm_manager: String,
        fee_tier: u32,                                     // must be an enabled tier
        position_recipient: String,                        // the locker (or any addr)
        // optional: init_sqrt_price_override: Option<Uint256>
    },
}
```

`SinkConfig`/`SinkState` (state.rs) gain a stored, address-validated mirror (`PoolKindStored`), analogous to the existing `LpDestinationStored`. `Settle` branches on it. `Refund` is venue-agnostic and unchanged (it just routes the two bank balances back).

### Touch list
- [ ] `msg.rs`: `PoolKind` enum; embed in `SinkInit`; extend `RoleResponse::Sink`/`SinkConfig` query output.
- [ ] `state.rs`: `PoolKindStored` (validated addrs), store in `SinkConfig`.
- [ ] `contract.rs`: `instantiate` validates the `Clmm` variant (addrs resolve, `fee_tier` non-zero); query handlers surface it.

---

## Phase 2 — CLMM `Settle`

### Design
New branch in `Settle` for `PoolKind::Clmm`. Unlike XYK, **no create-denom fee is attached** (CLMM `CreatePool` is free). Flow:

1. Read balances `token_denom`, `pair_denom`; require both > 0. Deduct `tip_bps` from the pair balance → tip to caller (unchanged).
2. **Sort** `(token_denom, pair_denom)` into `(token0, token1)` using the factory's exact ordering rule. Both are `AssetInfo::NativeToken` ⇒ lexicographic by denom string. Map the post-tip amounts to `(amount0, amount1)`.
3. **Compute `init_sqrt_price`** on-chain from the seed ratio (Phase 4): `sqrt_price_x96 = isqrt( amount1 · 2¹⁹² / amount0 )`, clamped to `[MIN_SQRT_RATIO, MAX_SQRT_RATIO]`. (If an override field is added, prefer it when `Some`.)
4. **Pre-existing-pool guard:** query `factory.GetPool { token0, token1, fee }`. If it already exists, either (a) bail with a clear error, or (b) read slot0 and proceed only if the live price is within a tolerance of our computed price — otherwise the seed ratio mismatches and the mint is lopsided. Default to **bail** unless the keeper opts into reuse.
5. Enqueue **sequential** messages (no sink-side reply IDs needed — the factory's *internal* create reply that writes `POOLS` completes before the next parent message runs, so the manager's `GetPool` resolves in-tx):
   - `factory.CreatePool { token_a: token0, token_b: token1, fee: fee_tier, init_sqrt_price }`.
   - `manager.MintPosition { token0, token1, fee, tick_lower, tick_upper, amount0_desired: amount0, amount1_desired: amount1, amount0_min, amount1_min, recipient: position_recipient, deadline }` with **both denoms attached as funds**. `tick_lower/upper` = full-range aligned to the fee tier's spacing (Phase 4). Since we set the price ourselves, slippage is nil — set `amount*_min` to the amounts minus a tiny dust tolerance, or 0.
   - **Dust sweep** self-callback: the manager refunds surplus native to the sink (`info.sender`); sweep any remaining `token_denom`/`pair_denom` to `refund_receiver` (or `issuer`) so nothing strands.
6. Mark status `Settled`. The position NFT now lives at `position_recipient` (the locker).

### Notes / risks
- The NFT recipient does **not** need to be instantiated at mint time (cw721 ownership is just an address); the locker contract only needs to exist before `CollectFees` is first called.
- `amount*_min`: because the pool is created at exactly our ratio, `MintPosition`'s internal `get_liquidity_for_amounts` consumes both sides nearly fully; remaining dust is one-sided and small. Keep `min` permissive to avoid a self-inflicted slippage revert on rounding.

### Touch list
- [ ] `contract.rs`: `settle_clmm` branch; token sort helper; CreatePool + MintPosition message builders; dust-sweep callback (`CallbackMsg::SweepDust`).
- [ ] `msg.rs`: extend `CallbackMsg` with `SweepDust {}` (or fold into existing callback enum).
- [ ] Import `choice_clmm_common` factory/manager/pool query+execute msg types as a dep of the seeder crate.

---

## Phase 3 — `Role::Locker`

### Design
A third role in the same binary. Holds the position NFT permanently and exposes fee collection only — **no `DecreaseLiquidity`, no `Burn`, no `TransferNft`** ⇒ principal is locked forever; only fees flow out.

```rust
// state.rs — Role enum gains:
Locker

// LockerConfig
pub struct LockerConfig {
    pub manager: Addr,            // choice_clmm_manager
    pub token_id: Option<String>, // set on ReceiveNft, or fixed at instantiate
    pub beneficiary: Addr,        // single fee destination
    pub admin: Option<Addr>,      // may rotate beneficiary; None ⇒ fully immutable
}
```

Execute:
- `CollectFees {}` — **permissionless**; `manager.Collect { token_id, recipient: beneficiary }`.
- `ReceiveNft(Cw721ReceiveMsg)` — cw721 hook; records `token_id` + asserts sender == configured `manager`. (Alternative: fix `token_id` at instantiate if the keeper knows it; the hook is more robust.)
- `UpdateBeneficiary { new }` — `admin`-only; omit/lock if `admin` is `None`.

Factory gains `CreateLocker { salt, locker_init }` — Instantiate2, **same pattern as `CreateSink`**, so the locker address is deterministically derivable from a salt keyed on `internal_id`. This is what lets the keeper compute `position_recipient` before creating the sink.

### Touch list
- [ ] `state.rs`: `Role::Locker`, `LockerConfig`, storage item.
- [ ] `msg.rs`: `LockerInit`, `CreateLocker { salt, locker_init }` on factory; `CollectFees {}`, `ReceiveNft`, `UpdateBeneficiary`; `Role`/`LockerConfig` queries.
- [ ] `contract.rs`: instantiate-role dispatch for `Locker`; `CreateLocker` (Instantiate2, mirror `CreateSink`); `collect_fees`, `receive_nft`, `update_beneficiary` handlers.

---

## Phase 4 — sqrt-price + tick math

### Design
Two pure helpers (unit-tested in isolation), placed in the seeder crate (or `choice_clmm_math` if reusable):

```rust
/// Q64.96 sqrt price that makes a full-range mint consume both balances.
/// sqrt_price_x96 = isqrt( amount1 << 192 / amount0 ), clamped to valid range.
fn init_sqrt_price_from_amounts(amount0: Uint128, amount1: Uint128) -> StdResult<Uint256>;

/// Full-range ticks aligned (toward zero) to the fee tier's tick spacing.
/// lower = (MIN_TICK / spacing) * spacing ; upper = (MAX_TICK / spacing) * spacing
fn full_range_ticks(tick_spacing: i32) -> (i32, i32);
```

`init_sqrt_price_from_amounts`: compute `amount1 << 192` in `Uint512` to avoid overflow, divide by `amount0`, `isqrt`, narrow to `Uint256`, then clamp to `[MIN_SQRT_RATIO+1, MAX_SQRT_RATIO-1]`. **Verify `Uint512` implements `Isqrt`**; if not, drop in a manual 256-bit integer-sqrt (Newton's method on `Uint256`).

`tick_spacing` per fee tier is a known constant (`100→1, 500→10, 3000→60, 10000→200`), so full-range ticks need no chain query. Integer division truncates toward zero, keeping the result inside `[MIN_TICK, MAX_TICK]` (e.g. spacing 60 → `-887220..887220`).

### Checklist
- [ ] `init_sqrt_price_from_amounts` + tests: round-trip `get_tick_at_sqrt_ratio` is consistent; extreme ratios clamp; equal amounts → ~`2^96`.
- [ ] `full_range_ticks` + tests: each tier stays within bounds and is divisible by spacing.

---

## Phase 5 — Tests

- [ ] **CLMM integration test** mirroring the existing XYK `tests/integration.rs`: instantiate factory(seeder) + CLMM factory + manager; create a sink with `PoolKind::Clmm`; fund it; `Settle`; assert a pool exists at the expected price, the NFT is owned by the locker, and both balances were consumed (dust swept).
- [ ] **Locker test**: `CollectFees` routes fees to `beneficiary`; no path decreases liquidity; `UpdateBeneficiary` gated on `admin`.
- [ ] **Pre-existing-pool guard**: `Settle` bails (or validates) when the pool already exists at a different price.
- [ ] Unit tests from Phase 4.

> Build note: integration artifacts need `build_release.sh`, not `make build-all`, due to bulk-memory (see [clmm_extensions_plan.md](./clmm_extensions_plan.md)).

---

## Phase 6 — Keeper / deploy wiring (docs only)

The keeper graduation flow, end to end:
1. Derive the locker address: Instantiate2(seeder_factory, locker_code_id, salt = encode(issuer, internal_id, "locker")).
2. `factory.CreateLocker { salt, locker_init: { manager, beneficiary, admin } }` (idempotent — derived address).
3. Build the CLMM `create_sink_payload` = `CreateSink { salt, sink_init: { …, pool_kind: Clmm { clmm_factory, clmm_manager, fee_tier, position_recipient: <locker addr> } } }`.
4. `issuer.RegisterLaunch { …, seeder_factory, create_sink_payload, choice_factory: None }` — unchanged issuer.
5. On graduation, fund the sink (Leg B token + Leg C pair) and call `Settle` (permissionless).
6. Anytime after, anyone calls `locker.CollectFees {}` to sweep trading fees to the beneficiary.

Open question for wiring: whether `RegisterLaunch` should `CreateLocker` atomically (one fewer keeper step, larger issuer blast radius) or the keeper creates it out-of-band (recommended — keeps the issuer untouched).

---

## Risks / open items

- **CLMM not on mainnet** — graduation can't ship until [clmm_extensions_plan.md](./clmm_extensions_plan.md) lands on mainnet.
- **Front-run pool creation** at a bad price before `Settle` → lopsided seed. Mitigated by the Phase-2 `GetPool` guard.
- **`Uint512::isqrt`** may not be impl'd — fallback to manual integer sqrt.
- **Token sort parity** — the seeder's sort MUST match the CLMM factory's ordering exactly, or `CreatePool`/`MintPosition` token0/token1 disagree. Reuse the factory's sort helper if exported.
- **Dust** — manager refunds one-sided surplus to the sink; the sweep step must run or value strands in a terminal sink.
- **Locker immutability** — with `admin: None` the beneficiary is permanent; confirm that's the desired trust model per launch (vs. a rotatable treasury).

---

## Build & test commands
```bash
cd choice_exchange
cargo build && cargo test                 # unit
./build_release.sh                         # WASM artifacts (bulk-memory; NOT make build-all)
cargo test --test integration              # injective_test_tube
```

# Prompt: Add CLMM Pool Integration to dex_aggregator

You are adding support for a new pool type — **CLMM (Concentrated Liquidity Market Maker)** — to the `dex_aggregator` contract. Below is every piece of information you need. Do not guess or hallucinate any message formats; use exactly what is specified here.

---

## 1. Swap Execute Message

The aggregator should use `SwapExactInput`, the user-friendly variant. The low-level `Swap` variant exists but requires a `sqrt_price_limit_x96: Uint256` that the aggregator shouldn't need to compute.

### For native token input (sent as `funds` on `WasmMsg::Execute`):

```rust
#[cw_serde]
pub enum ClmmPoolExecuteMsg {
    SwapExactInput {
        /// Minimum tokens to receive — slippage protection
        minimum_amount_out: Uint128,
        /// Defaults to msg sender if None
        recipient: Option<String>,
        /// Unix timestamp deadline; ignored if None
        deadline: Option<u64>,
    },
}
```

- `minimum_amount_out` is **required**. The aggregator should compute this from a `Quote` query with slippage applied.
- `recipient` should be set to the aggregator's own contract address (`env.contract.address`) so it can chain multi-hop swaps.
- `deadline` can be `None`.
- The swap **direction is inferred from the native token attached in `funds`**. No `zero_for_one` or `offer_asset` field is needed.

### For CW20 token input (sent via `Cw20ExecuteMsg::Send`):

```rust
// The inner hook message serialized into the `msg` field of Cw20ExecuteMsg::Send
#[cw_serde]
pub enum Cw20HookMsg {
    SwapExactInput {
        minimum_amount_out: Uint128,
        recipient: Option<String>,
        deadline: Option<u64>,
    },
}
```

Usage: `Cw20ExecuteMsg::Send { contract: pool_address, amount, msg: to_json_binary(&Cw20HookMsg::SwapExactInput { ... })? }`

The pool identifies which token is being offered by checking which CW20 contract called it.

---

## 2. Token Input Handling

| Input type | Supported? | How |
|-----------|-----------|-----|
| **Native tokens** | Yes | Attach as `funds` on `WasmMsg::Execute` with the `SwapExactInput` message |
| **CW20 tokens** | Yes | Use `Cw20ExecuteMsg::Send` to the pool address with `Cw20HookMsg::SwapExactInput` as the encoded `msg` |

This is the same pattern as the existing AMM integration — both native and CW20 are supported. The CW20 hook message format is shown above.

---

## 3. Simulation / Quote Query

```rust
// Query message
#[cw_serde]
pub enum ClmmPoolQueryMsg {
    Quote {
        token_in: AssetInfo,
        amount_in: Uint128,
    },
}

// Response
#[cw_serde]
pub struct QuoteResponse {
    /// The expected output amount — THIS IS THE FIELD YOU WANT
    pub amount_out: Uint128,
    /// How much input is actually consumed
    pub amount_in_consumed: Uint128,
    /// Fee charged
    pub fee_amount: Uint128,
}
```

- **Output field:** `amount_out: Uint128`
- Uses standard `Uint128` (no `FPDecimal` conversion needed).
- `token_in` uses the same `AssetInfo` type as the legacy AMM (`NativeToken { denom }` / `Token { contract_addr }`).

---

## 4. Reply Event Format

After a swap submessage succeeds, the CLMM pool emits attributes on the default `wasm` event:

| Event type | Attribute key | Value format |
|-----------|--------------|-------------|
| `wasm` | `action` | `"swap"` (literal string) |
| `wasm` | `amount_in` | Integer string (Uint128) |
| `wasm` | **`amount_out`** | **Integer string (Uint128)** — this is the output amount |
| `wasm` | `final_price` | Uint256 string (Q64.96 sqrt price) |
| `wasm` | `final_tick` | i32 string |

**The output amount is in attribute key `"amount_out"` on the `"wasm"` event, as a plain integer string (no decimals, no truncation needed).**

To parse it in `parse_amount_from_swap_reply`, look for:
- Event type: `"wasm"`
- Attribute key: `"amount_out"`

This is simpler than the orderbook case — no decimal parsing needed.

---

## 5. Operation Struct

```rust
#[cw_serde]
pub struct ClmmSwapOp {
    /// The CLMM pool contract address
    pub pool_address: String,
    /// Which token is being offered
    pub offer_asset_info: AssetInfo,  // Use the aggregator's existing AssetInfo type
    /// Which token is being received
    pub ask_asset_info: AssetInfo,    // Use the aggregator's existing AssetInfo type
}
```

**No additional fields are needed.** Unlike the orderbook (which needs `min_quantity_tick_size`), the CLMM pool handles everything internally:
- Direction is inferred from the input token
- Price limits are auto-set to full range in `SwapExactInput`
- No tick spacing or fee tier needed per-operation (those are pool config, not per-swap)

---

## 6. Pre-Execution Logic

### Rounding
**None required.** The CLMM pool accepts any non-zero `Uint128` amount. If the pool doesn't consume the full input (which can happen near price boundaries), it refunds the excess automatically.

### Pre-execution simulation query
**Yes — query `Quote` to compute `minimum_amount_out`.** Before executing:

1. Query the pool: `ClmmPoolQueryMsg::Quote { token_in, amount_in }`
2. Get `QuoteResponse.amount_out`
3. Apply slippage tolerance (e.g., 0.5%): `minimum_amount_out = amount_out * 995 / 1000`
4. Execute `SwapExactInput { minimum_amount_out, recipient, deadline: None }`

If the quote returns `amount_out = 0`, the swap would fail. You may want to emit a no-op or error in that case.

---

## 7. Output Delivery

**The pool automatically sends output tokens to the `recipient` address.** No separate claim step is needed.

- For native token output: uses `BankMsg::Send`
- For CW20 token output: uses `Cw20ExecuteMsg::Transfer`

The aggregator should set `recipient` to its own contract address (`env.contract.address`) for intermediate hops, and to the user's address for the final hop (same pattern as AMM).

---

## 8. Pool Identifier

**Single contract address**, same as AMM and orderbook. Each CLMM pool is its own contract instance, created by the `choice_clmm_factory`. The pool address is used for both execution and queries.

Pools can be looked up from the factory via:
```rust
FactoryQueryMsg::GetPool {
    token_a: AssetInfo,
    token_b: AssetInfo,
    fee: u32,  // e.g., 500 = 0.05%, 3000 = 0.3%, 10000 = 1%
}
```
But the aggregator doesn't need to query the factory — the route planner provides the pool address in the operation.

---

## AssetInfo Type

The CLMM contracts use the **same `AssetInfo` enum** as the legacy AMM:

```rust
#[cw_serde]
pub enum AssetInfo {
    NativeToken { denom: String },
    Token { contract_addr: String },
}
```

**No conversion function is needed.** The aggregator can use its existing `AssetInfo` type directly in CLMM `Quote` queries and operations.

---

## Summary Table

| # | Item | Answer |
|---|------|--------|
| 1 | Execute message | `SwapExactInput { minimum_amount_out, recipient, deadline }` — direction inferred from input token |
| 2 | Token input | Both native (`funds`) and CW20 (`Send` hook with `Cw20HookMsg::SwapExactInput`) |
| 3 | Simulation query | `Quote { token_in, amount_in }` → `QuoteResponse { amount_out, amount_in_consumed, fee_amount }` |
| 4 | Reply events | Event type `wasm`, attribute key `amount_out`, integer string format |
| 5 | Op struct fields | Just `pool_address`, `offer_asset_info`, `ask_asset_info` — no extra fields |
| 6 | Pre-execution | Query `Quote` to compute `minimum_amount_out` with slippage; no rounding needed |
| 7 | Output delivery | Auto-sent to `recipient` via `BankMsg::Send` or `Cw20ExecuteMsg::Transfer` |
| 8 | Pool identifier | Single contract address |

---

## Files to Modify

| File | What changes |
|------|-------------|
| `contracts/dex_aggregator/src/msg.rs` | Add `ClmmSwapOp` struct, add `Clmm(ClmmSwapOp)` variant to `Operation` enum, add `clmm` submodule with `ClmmPoolExecuteMsg`, `Cw20HookMsg`, `ClmmPoolQueryMsg`, `QuoteResponse` |
| `contracts/dex_aggregator/src/execute.rs` | Add `Operation::Clmm` arm in `create_swap_cosmos_msg`: query `Quote` for `minimum_amount_out`, build `SwapExactInput` msg. Handle both native (attach `funds`) and CW20 (`Cw20ExecuteMsg::Send`) |
| `contracts/dex_aggregator/src/reply.rs` | Add `Operation::Clmm` arm in `get_operation_output`, `get_operation_input`, `get_operation_address`. In `parse_amount_from_swap_reply`, add pattern: event type `"wasm"`, attribute `"amount_out"` |
| `contracts/dex_aggregator/src/query.rs` | Add `Operation::Clmm` arm in `simulate_single_operation` (query `Quote`, return `amount_out`) and `get_path_start_info` |
| `contracts/mock_swap/src/lib.rs` | Add `ProtocolType::Clmm` variant with matching event emission (`action=swap`, `amount_out=...`) and `Quote` query handler |
| `tests/integration.rs` | Add integration tests: deploy mock CLMM pool, test single-hop and multi-hop routes through CLMM, test mixed AMM+CLMM routes |

---

## Implementation Notes

1. **The CLMM pattern is closer to AMM than orderbook.** It supports both native and CW20, has a simple query interface with `Uint128` types, and uses the standard `wasm` event type. The main new element is the `minimum_amount_out` slippage field computed from a pre-execution `Quote` query.

2. **Slippage computation for `minimum_amount_out`:** Before executing the swap, query the pool with `Quote { token_in, amount_in }`. Apply a slippage tolerance to `amount_out` (e.g., multiply by `0.995` for 0.5% slippage). Use this as `minimum_amount_out`. The pool will revert the swap if the actual output is less than this.

3. **No rounding or tick size concerns.** Unlike the orderbook integration, there's no `min_quantity_tick_size` to worry about. Any non-zero `Uint128` amount is valid input.

4. **The `amount_out` event attribute key** differs from both AMM (`return_amount`) and orderbook (`swap_final_amount`). Make sure the reply parser handles this new key.

5. **AssetInfo types are shared.** The CLMM uses the same `AssetInfo` enum as the legacy AMM (`NativeToken`/`Token`), so no conversion is needed. The `SwapExactInput` message itself doesn't contain an asset info field — the pool infers direction from the input token.

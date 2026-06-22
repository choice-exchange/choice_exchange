# CLMM Extensions Plan — Protocol Fees, Flash, Exact-Output, Hook Seam

**Status:** In progress · pre-mainnet · CLMM not yet deployed to mainnet (free to change storage layout / ABI)
**Goal:** Close the competitive gaps vs. latest DEX tech (Uniswap v4-era) without a singleton/hooks rewrite. Keep the v3 multi-instance architecture; add the features that actually pay off on CosmWasm/Injective.
**Audit:** Single security pass at the end, across all phases.

Architecture decision (from the review): **do NOT** refactor to a v4 singleton or build a permissionless hooks engine. v4's singleton + transient-storage flash-accounting solves EVM gas/CALL costs that don't bind on CosmWasm, and a hooks *market* has no ecosystem of third-party authors here. Instead we steal the genuinely good, transferable ideas: protocol fees, per-pool flash swaps, exact-output, and a cheap reserved hook seam for future optionality.

---

## Progress tracker

| # | Phase | Scope | Status |
|---|-------|-------|--------|
| 1 | Protocol fees | carve + accumulators + `SetFeeProtocol` + `CollectProtocol` + burn-auction split | ☑ Done (unit-tested) |
| 2 | Flash swaps | `Flash` entry + callback/repay reply + reentrancy `LOCK` over all mutating entrypoints | ☑ Done (unit-tested) |
| 3 | Exact-output | exact-out loop branch + `SwapExactOutput` + `QuoteExactOutput` | ☑ Done (unit-tested) |
| 4 | Hook seam | reserve `Option<Addr> hook` + permission bits in `PoolConfig` (no engine) | ☑ Done (unit-tested) |
| 5 | Aggregator refinement | exact-out routing, caller-supplied slippage, protocol-fee awareness in `dex_aggregator` | ☑ Done (unit-tested) |
| 6 | Security audit | math + reentrancy + fee-accounting + new entrypoints; full test pass | ☑ Done (no crit/high; 2 integ tests + 4 regressions added) |

Legend: ☐ not started · ◐ in progress · ☑ done

Update this table and the per-phase checklists as work lands. Record any design deviations inline under the relevant phase.

---

## Phase 1 — Protocol fees (+ INJ burn-auction split)

### Design
v3-style carve, accrue-then-collect (NOT per-swap cross-contract calls).

- **Rate** lives in slot0 (`PoolState`): `fee_protocol_0: u8`, `fee_protocol_1: u8`. Each is a divisor `n` meaning "protocol takes `fee_amount / n` of the LP fee" (v3 convention: `0` = off, valid range e.g. `4..=10` → 25%..10%). One per direction so token0 and token1 fees can differ.
- **Accumulators**: `PROTOCOL_FEES_0: Item<Uint128>`, `PROTOCOL_FEES_1: Item<Uint128>`. These are *protocol-owned*, not part of pool liquidity, so they're never withdrawable by LPs.
- **Hot-path carve** in `compute_swap` at [swap.rs:94-104](../contracts/choice_clmm_pool/src/actions/swap.rs#L94-L104):
  - Compute `step.fee_amount` as today.
  - If `fee_protocol_{dir} != 0`: `protocol_delta = step.fee_amount / fee_protocol_{dir}`; subtract it from the amount that flows into `fee_growth_global` (so LPs only earn the remainder). Accumulate `protocol_delta` into a new `protocol_fee_{0,1}` field on `SwapComputation`.
  - `apply_swap` adds those to `PROTOCOL_FEES_{0,1}`. Keep the carve out of `state_fee_total`'s LP attribution but track total for the swap event.
- **Setting the rate**: pool `ExecuteMsg::SetFeeProtocol { fee_protocol_0, fee_protocol_1 }`, gated to the **factory owner** (query `factory.GetConfig` for `owner`, like v3's `factory.setFeeProtocol`). Validate `0 || 4..=10`.
- **Collection + burn split**: pool `ExecuteMsg::CollectProtocol { amount0_requested, amount1_requested }`, gated to factory owner. Routing config (below) decides the split:
  - `burn_share_bps` of each collected token → `choice_send_to_auction` via `ExecuteMsg::SendNative { asset }` (native) or `Cw20ExecuteMsg::Send` (CW20), matching the existing [choice_pair pattern](../contracts/choice_pair/src/contract.rs#L549). Mirrors the documented fee flow: *swap fees → choice_send_to_auction → Injective burn auction*.
  - remainder → `treasury` recipient.
- **Routing config** — new `Item<ProtocolFeeConfig>`:
  ```rust
  pub struct ProtocolFeeConfig {
      pub treasury: Addr,                 // gets the non-burn remainder
      pub burn_auction: Option<Addr>,     // choice_send_to_auction; None = no burn split
      pub burn_share_bps: u16,            // 0..=10000 of protocol fees routed to burn auction
  }
  ```
  Factory passes defaults at pool instantiate; factory owner can update via a pool `UpdateProtocolFeeConfig`.

### Why accrue-then-collect, not per-swap
A cross-contract `SendNative` on every swap would add a submessage + reentrancy surface to the hot path and wreck swap gas. CLMM fees are tiny per-swap and dust-prone; accrue in storage, sweep periodically. This is a deliberate divergence from `choice_pair` (XYK, simpler accounting) and is the v3-correct approach.

### Touch list
- `packages/choice_clmm_common/src/pool.rs` — `PoolState` (add `fee_protocol_0/1`), new `ExecuteMsg` variants (`SetFeeProtocol`, `CollectProtocol`, `UpdateProtocolFeeConfig`), `ProtocolFeeConfig`, query `GetProtocolFees` / `GetProtocolFeeConfig`.
- `contracts/choice_clmm_pool/src/state.rs` — `PROTOCOL_FEES_0/1`, `PROTOCOL_FEE_CONFIG`.
- `contracts/choice_clmm_pool/src/actions/swap.rs` — carve in `compute_swap`, accrue in `apply_swap`, extend `SwapComputation`.
- `contracts/choice_clmm_pool/src/actions/` — new `protocol.rs` (`set_fee_protocol`, `collect_protocol`, `update_protocol_fee_config`).
- `contracts/choice_clmm_pool/src/contract.rs` — instantiate defaults + dispatch.
- `contracts/choice_clmm_factory/` — pass `ProtocolFeeConfig` defaults on `CreatePool`; optionally a factory-level default.
- Tests: carve math correctness, off-by-default, owner gating, collect split to auction + treasury, Quote unaffected.

### Deviations from original design (as built)
- **Dedicated `ProtocolFeeConfig` Item, not slot0 fields.** Rates + routing live in one `Item("protocol_fee_config")` rather than on `PoolState`. Keeps the manager-facing `PoolState`/`GetSlot0` surface untouched; costs one cheap `may_load` in the swap path (helper `load_protocol_fee_rates`).
- **Default-off, configured post-create.** `InstantiateMsg` is unchanged (avoids touching its many call sites). Pool instantiates with rates `0/0`, `burn_auction: None`, `burn_share_bps: 0`, and `treasury` defaulted to the **factory owner** (queried live, falls back to the factory address). Owner turns it on via `SetFeeProtocol` + `UpdateProtocolFeeConfig`.
- **Burn-auction message is a local mirror.** `protocol.rs` defines a minimal `SendNative` enum rather than depending on the legacy `choice` package; the CLMM `AssetInfo` serializes identically to the auction's expected shape.

### Checklist
- [x] State + message types (`ProtocolFeeConfig`, `ProtocolFeesResponse`, exec/query variants)
- [x] Hot-path carve + accrual (`compute_swap` carve, `apply_swap` accrual, `protocol_fee` event attr)
- [x] `SetFeeProtocol` (factory-owner gated, validates `0 || 4..=10`)
- [x] `CollectProtocol` with burn/treasury split
- [x] `UpdateProtocolFeeConfig`
- [x] Instantiate defaults (off; treasury = factory owner)
- [x] Unit tests — 6 new (off-by-default, 25% carve math, owner gating ×2, invalid divisor, collect split)
- [x] Integration test: live `send_to_auction` round-trip via `injective_test_tube` — `test_protocol_fee_collect_to_auction_roundtrip` (Phase 6)

**Files touched:** `packages/choice_clmm_common/src/pool.rs`, `contracts/choice_clmm_pool/src/{state.rs, contract.rs, actions/mod.rs, actions/swap.rs, actions/protocol.rs (new), tests.rs}`. Build + clippy clean; 32 unit + 6 regression tests green.

---

## Phase 2 — Flash swaps (+ reentrancy lock)

### Design
Classic v3 `flash()`, per-pool (no v4 singleton net-settlement — without a singleton there's nothing to net across).

- `ExecuteMsg::Flash { recipient, amount0, amount1, data: Binary }`.
- Flow: compute `fee0 = ceil(amount0 * fee_pips / 1e6)`, `fee1` likewise (use current dynamic fee). Snapshot pool's pre-balances. Send `amount0/amount1` to `recipient`, then `SubMsg::reply_on_success` calling the recipient's flash-callback with `data` so the borrower can act.
- Reply: assert pool balance is back to `pre + fee` for each token (query bank/cw20 balances). On success, split each `fee` between `fee_growth_global` (LPs) and `PROTOCOL_FEES` per the same `fee_protocol` carve as swaps. Emit event.
- **Reentrancy `LOCK: Item<bool>`**: set at the top of every mutating entrypoint (`swap*`, `mint`, `burn`, `collect`, `flash`, `collect_protocol`), clear at the end. Flash introduces the first real reentrancy vector (callback to arbitrary contract), so the lock is mandatory and must guard *all* state mutators, not just flash.
- Liquidity invariant: flash must not change `liquidity`, `sqrt_price`, or `tick` — only move balances and accrue fees.

### Notes / risks
- CosmWasm reply-based callback means the borrower's action runs as a submessage chain; the repayment check happens in the pool's reply after the callback returns. Ensure the lock spans the whole flash→callback→reply window (lock set before dispatch, cleared in reply).
- Decide repayment detection: balance-delta check (robust, supports native + CW20) vs. requiring borrower to `Send`/`TransferFrom` exactly. Prefer **balance-delta assertion** to stay token-type agnostic.

### Touch list
- `packages/choice_clmm_common/src/pool.rs` — `Flash` variant, flash callback message shape, `Get... ` if needed.
- `contracts/choice_clmm_pool/src/state.rs` — `LOCK`, pending-flash context Item.
- `contracts/choice_clmm_pool/src/actions/flash.rs` — new.
- `contracts/choice_clmm_pool/src/contract.rs` — dispatch + reply handler + lock guards on all mutators.
- Tests: successful flash + repay, insufficient-repay revert, reentrancy attempt (flash → swap mid-callback) reverts via lock, fee accrual to LPs + protocol.

### Deviations / decisions (as built)
- **Repayment model: balance-delta + lock** (decided with user). Native tokens can't be pulled (`TransferFrom` is CW20-only), so a "pool pulls exact" model is impossible for the native side anyway; balance-delta is one uniform path. The lock is what makes it safe — during the callback the only way pool balance can rise is a direct transfer back = genuine repayment. Mirrors Uniswap V3 `flash()`.
- **Lock is check-only on normal mutators, set/clear only by flash.** The central guard in `execute()` rejects every fund-affecting variant while `REENTRANCY_LOCK` is held; only `execute_flash` sets it and `reply_flash` clears it. Rationale: normal mutators never hand control to untrusted code across an open invariant (bank Send executes no code; CW20 payouts use `Transfer`, which has no recipient hook), so they only need to be *blocked during* a flash, not to set the lock themselves. Pure config setters (`SetFeeProtocol`, `UpdateProtocolFeeConfig`) are exempt.
- **Repayment is passive, so it isn't blocked by the lock.** Borrower repays via native bank Send or CW20 `Transfer` (NOT `Send`) — neither invokes the pool's `execute`, so they don't trip the guard. Documented on `FlashCallbackMsg`.
- **Flash fee follows the Phase 1 carve.** LP share → `fee_growth_global` (needs `liquidity > 0`); protocol share → `PROTOCOL_FEES_*`. When `liquidity == 0` the LP share has no recipient, so it's routed to the protocol bucket (never lost, never divide-by-zero) instead of reverting.
- Added the pool's first `reply` entrypoint (`REPLY_FLASH = 100`); new `PendingFlash` state + `is_locked` helper; fee rounded UP via `mul_div_round_up`.

### Checklist
- [x] `is_locked` guard on all fund-affecting entrypoints (central guard in `execute`)
- [x] `Flash` entry + fee computation (rounded up, current dynamic fee)
- [x] Callback submessage (`FlashCallbackMsg`) + `reply` repayment assertion (balance-delta per token)
- [x] Fee accrual (LP `fee_growth` + protocol carve; L==0 → protocol)
- [x] Unit tests — 6 (lends+locks, blocks reentrant swap + nested flash, reply accrues+unlocks, underpay revert, protocol carve, L==0 routing)
- [x] Integration test: real borrower-contract callback round-trip via `injective_test_tube` — `test_flash_loan_borrower_roundtrip_and_underpay_revert` + `contracts/flash_borrower_mock` (Phase 6)

**Files touched:** `packages/choice_clmm_common/src/pool.rs`, `contracts/choice_clmm_pool/src/{error.rs, state.rs, contract.rs, actions/mod.rs, actions/flash.rs (new), tests.rs}`. Build + clippy clean; 38 unit + 6 regression tests green.

---

## Phase 3 — Exact-output swaps

### Design
Extend the math, then the pool surface. `compute_swap_step` currently documents exact-out as unsupported ([swap_math.rs:34-36](../packages/choice_clmm_math/src/swap_math.rs#L34-L36)); the `get_next_sqrt_price_from_output` primitive already exists ([swap_math.rs:2-5](../packages/choice_clmm_math/src/swap_math.rs#L2-L5)).

- Add exact-out branch to `compute_swap_step` (v3 parity: `amountRemaining < 0` semantics — here pass an `exact_input: bool`). In exact-out, `amount_remaining` is the desired output; `amount_out` is capped by it and `amount_in`/`fee` derive from the price move to reach the target.
- Extend `compute_swap` to thread `exact_input` and terminate on output target.
- Pool surface:
  - Low-level: add `exact_input: bool` (or a sign convention) to `Swap`.
  - High-level: `ExecuteMsg::SwapExactOutput { amount_out, maximum_amount_in, recipient, deadline }` — pull/refund the difference vs. `maximum_amount_in`, slippage check on input.
- Reuse the existing input-source plumbing (`SwapInputSource`) — for native exact-out, attach `maximum_amount_in` and refund the unused remainder.

### Touch list
- `packages/choice_clmm_math/src/swap_math.rs` — exact-out branch + `v3_swap_math_vectors` exact-out vectors.
- `contracts/choice_clmm_pool/src/actions/swap.rs` — thread `exact_input`, `SwapExactOutput` handler, `query_quote` exact-out variant.
- `packages/choice_clmm_common/src/pool.rs` — `SwapExactOutput`, `Swap.exact_input`, `Quote` exact-out.
- Tests: v3 exact-out vectors, refund correctness, slippage (`maximum_amount_in`).

### Deviations / decisions (as built)
- **Math step helper already existed.** `compute_swap_step_exact_out` was already in `swap_math.rs` (marked `#[allow(dead_code)]`); wired it in and removed the dead-code allow. Added exact-out vectors.
- **`exact_input: bool` threaded through `compute_swap`, shared loop.** Kept the audited tick-crossing + protocol-carve logic shared (it depends only on direction, not in/out); branched only the 4 localized spots: step call, remaining/calculated updates, and final `amount_in`/`amount_out` derivation.
- **L==0 *and* `target == current` handled as a free no-op step in exact-out.** The exact-out helper rejects both (exact-in's helper tolerates them). Without the `target == current` case, a swap starting exactly on a tick/price boundary (e.g. price 1.0) errored. Both now advance to target for free, and the crossing logic advances the tick — matching exact-in.
- **High-level only; low-level `Swap` left exact-in.** Did not add `exact_input` to the low-level `Swap` message (would churn its signature + existing tests for no benefit). Exact-out is exposed via `SwapExactOutput` + `QuoteExactOutput`, which is what the aggregator needs.
- **CW20 exact-out via allowance only** (pull exactly the cost); no `Receive`-hook exact-out variant (the borrower can't know the cost up-front). Native attaches `maximum_amount_in` and `apply_swap` refunds the surplus.
- **Full-fill required.** `SwapExactOutput` reverts (`InsufficientOutput`) if the pool can't deliver the whole requested output, and reverts (`ExcessiveInput`) if the cost exceeds `maximum_amount_in`.

### Checklist
- [x] `compute_swap_step_exact_out` wired + 3 new math vectors
- [x] `compute_swap` exact-out threading (shared loop, `exact_input` flag)
- [x] `SwapExactOutput` handler (native + CW20-allowance, refund, slippage)
- [x] `QuoteExactOutput` query + handler
- [x] Unit tests — 5 (delivers+refunds, quote==exec, excessive-input revert, insufficient-liquidity revert, reverse direction)

**Files touched:** `packages/choice_clmm_math/src/swap_math.rs`, `packages/choice_clmm_common/src/pool.rs`, `contracts/choice_clmm_pool/src/{error.rs, contract.rs, actions/swap.rs, tests.rs}`. Build + clippy clean; 43 unit + 6 regression + 29 math tests green.

---

## Phase 4 — Hook seam (reserve only)

### Design
Capture v4's *optionality* at near-zero cost without building the engine.

- Add to `PoolConfig`: `hook: Option<Addr>` (default `None`) and `hook_permissions: u16` (bitflags for before/after swap/mint/burn — reserved, unused).
- No execution machinery now. This makes a future before/after-swap hook a non-breaking addition (the field already exists; pre-mainnet so even the layout is free).
- Document the intended call sites (`before_swap`/`after_swap` around `compute_swap`/`apply_swap`) as comments so a later implementer has the contract.

### Deviations / decisions (as built)
- **Reserve only — field + flags + docs, no setter, no engine.** Added `hook: Option<Addr>` and `hook_permissions: u16` to the pool's `PoolConfig` (default `None`/`0`), plus `HOOK_*` flag constants and documented call sites in `apply_swap`. Deliberately *no* `SetHook` and *no* invocation: a setter would imply the hook does something, which it doesn't yet. A future phase adds the setter + before/after invocation together. The field's presence is the reservation (and pre-mainnet, even that's free — it's about establishing the shape).
- **Defaulted at instantiate, not via `InstantiateMsg`.** Consistent with Phases 1/2 — kept `InstantiateMsg` untouched; `hook` is `None` at creation. Wiring a hook through the factory is deferred to the engine phase.
- Adding fields to `PoolConfig` is backward-compatible for external deserializers (cw_serde doesn't deny unknown fields), so the manager/aggregator are unaffected.

### Checklist
- [x] `PoolConfig.hook` + `hook_permissions` fields (default None/0) + `HOOK_*` flag constants
- [x] Defaulted at instantiate (None/0)
- [x] Doc comment marking intended hook call sites (`apply_swap`, incl. reentrancy note)
- [x] Unit test — `hook_seam_defaults_to_none`

**Files touched:** `contracts/choice_clmm_pool/src/{state.rs, contract.rs, actions/swap.rs, tests.rs}`. Build + clippy clean; 44 unit + 6 regression green.

---

## Decision: full hook engine — DEFERRED (not before audit)

Considered building hook *execution* (before/after swap engine) before the security audit. **Decision: no — keep the seam reserved (Phase 4) only.** Rationale: full hooks reverse the project thesis (skip the EVM-specific complexity that buys nothing on CosmWasm with an ecosystem of one), and adding the most exploit-prone v4 subsystem right before an audit maximizes audit scope/cost speculatively. The two things hooks usually deliver (dynamic fees, oracle) already exist built-in. No concrete near-term hook use case. The reserved field means hooks can still be added later via a contract upgrade without a `PoolConfig` migration. If a concrete use case emerges (MEV internalization / limit orders / Helix integration), prefer a **minimal owner-set before/after hook**, not full v4 permissionless delta-returning hooks. Do not re-open without a concrete use case.

## Session checkpoint (end of session 1)

Phases **1–4 complete** (protocol fees + burn split, flash + reentrancy lock, exact-output, hook seam). Pool crate: **44 unit + 6 regression + 29 math tests green; build + clippy clean.**

**Resume here — remaining work for the next session:**
- **Phase 5 — Aggregator refinement** (`aggregation_contract/contracts/dex_aggregator`): caller-supplied slippage on CLMM ops (replace hardcoded 0.5%), add an exact-output CLMM op leg using the new `SwapExactOutput`/`QuoteExactOutput`, verify `Quote` semantics are unchanged post protocol-fee carve (carve is output-neutral, so should be fine — confirm).
- **Phase 6 — Security audit**: the dedicated pass (see its section), plus the two deferred integration tests via `injective_test_tube`: (a) protocol-fee `CollectProtocol` → live `send_to_auction` round-trip; (b) flash borrower-contract callback round-trip. Run `make build-all` first.

---

## Phase 5 — Aggregator refinement (`dex_aggregator`)

Current state: `dex_aggregator` **already routes CLMM** via `Operation::ClmmSwap(ClmmSwapOp)` using `Quote` + `SwapExactInput` ([aggregation_contract/.../execute.rs](../../aggregation_contract/contracts/dex_aggregator/src/execute.rs)). The legacy `choice_router` does NOT (XYK only) — leave it or add CLMM later, low priority.

Refinements:
- Replace the hardcoded 0.5% slippage with caller-supplied per-op min-out.
- Add a `ClmmSwapExactOutput` op once Phase 3 lands (needed for exact-out route legs).
- Protocol-fee awareness: ensure quotes/sims account for the LP-fee-minus-protocol-carve (the pool's `Quote` already returns net `amount_out`, so verify no double counting after Phase 1).

### Deviations / decisions (as built)
- **Caller-supplied slippage = `max_slippage_bps: Option<u16>` per op, on BOTH `ClmmSwapOp` and `OrderbookSwapOp`.** `None` defaults to `DEFAULT_SLIPPAGE_BPS` (50 = 0.5%), preserving the old hardcoded behavior. Extended to the orderbook leg too (it had the same hardcoded `0.005`); same code smell, one fix. Backward-compatible on the wire — serde reads a missing `Option` field as `None`, so the mainnet `Operation` enum keeps deserializing old payloads. Shared helper `min_out_after_slippage` (rejects `bps > 10_000`).
- **Exact-output = native-input only, refund the unspent budget.** New `Operation::ClmmSwapExactOutput(ClmmSwapExactOutputOp { pool_address, offer_asset_info, ask_asset_info, amount_out })`. The struct is CW20-capable but the runtime rejects CW20 input (`ExactOutputNativeInputOnly`); the pool's CW20 exact-out is allowance-only (no `Receive` hook) and would bypass the tax-token path, so it's deferred — addable later with **no message migration** (just the runtime branch). Chosen over "Full" to minimize audit surface before Phase 6; native covers ~all real exact-out routes (Injective spend assets are native denoms).
- **The forward router can't pre-size an exact-out leg, so unspent input is refunded.** Mechanics in `create_swap_cosmos_msg`: query `GetConfig` (deserializing only `token0`/`token1` into a partial `ConfigResponse` — serde drops the rest) to resolve `zero_for_one` = paying `token0`; query `QuoteExactOutput` for the cost; attach **exactly the quoted cost** as funds with `maximum_amount_in = cost` (tightest bound — same-block deterministic, so the pool refunds nothing); send the leg's budget surplus (`budget − cost`) straight back to the **route initiator** as the offer asset. The refund is the user's own money (not route output), so it bypasses the aggregator fee and `minimum_receive`. `create_swap_cosmos_msg` now returns `(primary, extra)` and takes `initiator`; the three call sites add `extra` as fire-and-forget messages. Edge handling: CW20 input / `amount_out == 0` / `cost > budget` (`InsufficientInputBudget`) / `deliverable < amount_out` (`ExactOutputNotFillable`) / offer ≠ either pool token (`OperationAssetMismatch`) all revert. Known caveat: a second split hitting the *same pool in the same stage* can move the price between quote and execution → pool reverts `ExcessiveInput` (safe, rare; documented).
- **`SimulateRoute` exact-out arm reports the deliverable `amount_out`** (via `QuoteExactOutput`), independent of the supplied input — matching that the unspent input is refunded, not routed onward.
- **Quote semantics verified output-neutral (no change needed).** The protocol carve at [swap.rs:152-163](../contracts/choice_clmm_pool/src/actions/swap.rs#L152-L163) only splits `step.fee_amount` between LP fee-growth and the protocol bucket; `amount_out` derives from the price move and is untouched (code comment: *"The swapper's cost is unchanged — only the split differs."*). The aggregator already consumes `Quote.amount_out` as the net received — no double-counting.

### Checklist
- [x] Caller-supplied slippage on CLMM ops (+ orderbook ops, same fix)
- [x] Exact-output CLMM op — native-input, refund unspent budget to initiator (CW20 deferred, no-migration)
- [x] Verify Quote semantics post protocol-fee change (output-neutral, confirmed)
- [x] Unit tests — 11 new (slippage default/custom/over-100%/reject; exact-out attaches cost + refunds, no-refund-when-exact, zero_for_one both directions, CW20/budget/unfillable reverts; caller-slippage threads to min-out; simulate exact-out)
- [x] Integration test refresh + the pre-existing cosmwasm-std 3.0 migration fix. The `tests/integration.rs` target was **non-compiling pre-Phase-5** (in-flight cosmwasm-std 3.0 migration); fixed by: (a) dropping the `cw20`/`cw20-base` 2.0.0 dev-dep (it pulled a *second* cosmwasm-std 2.x — the conflict source) and talking to `cw20_base.wasm` via local 3.0 `cw20_compat` types (JSON ABI is version-stable); (b) `Uint256::u128()` → `Uint128::try_from`, `Coin.amount` now `Uint256` (`.into()` / `try_from`). Added `mock_swap` `SwapExactOutput`/`QuoteExactOutput`/`GetConfig` (inverse-rate cost; token0=input/token1=output) + a Phase 5 exact-out integration test (`test_clmm_exact_output_delivers_exact_and_refunds_surplus`: exactly 150 USDT out, 2-INJ surplus refunded, aggregator holds zero leftover). Rebuilt WASM via `./build_release.sh` (Docker workspace-optimizer 0.17.0).

**Files touched:** `aggregation_contract/contracts/dex_aggregator/src/{msg.rs, execute.rs, reply.rs, query.rs, error.rs}`, `contracts/mock_swap/src/lib.rs`, `contracts/dex_aggregator/Cargo.toml` (drop cw20-base dev-dep), `tests/integration.rs` (cw-std 3.0 fixes + cw20_compat + Phase 5 test). **26 lib unit + 31 integration tests green; clippy all-targets clean; artifacts rebuilt.**

---

## Phase 6 — Security audit

Single pass after Phases 1–5. Focus areas:
- Fee-accounting: protocol carve never lets LP + protocol exceed `fee_amount`; rounding direction always favors the pool; wrapping accumulators unaffected.
- Reentrancy: `LOCK` covers every mutator; flash callback cannot re-enter swap/mint/burn/collect; lock cleared on all paths incl. error.
- Flash: repayment assertion is balance-based and unforgeable; no liquidity/price mutation; fee split correct.
- Exact-out: no under/overflow at tick boundaries; refund math exact; can't extract value via dust rounding.
- Access control: `SetFeeProtocol`/`CollectProtocol`/`UpdateProtocolFeeConfig` strictly factory-owner; burn-auction routing can't be redirected by non-owner.
- Re-run `tests/exploit_regressions.rs` + add new regressions for each phase.
- Run `/security-review` and `/code-review high` over the full diff.

### Audit results (session 2, 2026-06-05)

**Outcome: no critical or high findings.** The P1–P5 diff was reviewed line-by-line
against each focus area. The carve/flash/exact-out logic is V3-faithful and the
arithmetic is panic-safe (typed errors, never `.unwrap()` on attacker-reachable
paths). Findings below are observations / by-design notes, not vulnerabilities.

- **Fee-accounting — OK.** Carve is `protocol_delta = floor(step.fee / n)`,
  `lp_fee = step.fee − protocol_delta` ([swap.rs:156-163](../contracts/choice_clmm_pool/src/actions/swap.rs#L156-L163)),
  so `lp + protocol == fee` exactly and the protocol can never exceed the fee
  (a `checked_sub` guards the invariant explicitly). `amount_out` derives only
  from the price move, so the carve is **output-neutral** — quotes stay correct
  and pass `(0,0)`. Fee-growth accumulators stay `wrapping_*` (intentional U256
  overflow). Dust below the divisor (`fee < n`) rounds the protocol share to 0,
  i.e. entirely to LPs — favors LPs, not the pool, but it is the swapper's
  already-paid fee, so no value is lost. Regression: `ext_p1_…`.
- **Reentrancy — OK.** The central guard in `execute` ([contract.rs:185-191](../contracts/choice_clmm_pool/src/contract.rs#L185-L191))
  rejects **every** fund-affecting variant while `REENTRANCY_LOCK` is held;
  only the two pure (fund-less) config setters are exempt, and they do not clear
  the lock. The lock is set by `execute_flash` and cleared by `reply_flash`; a
  failing callback uses `reply_on_success`, so any borrower/reply error reverts
  the whole tx and rolls the lock back to unset — it can never stick. Nested
  flash and CW20-`Receive` re-entry are both blocked (Receive is guarded).
  Regression: `ext_p2_…` exhaustively asserts all nine mutators are blocked and
  both setters are exempt.
- **Flash — OK.** Repayment is a pure balance-delta assertion
  (`bal ≥ snapshot + fee`, [flash.rs:146-169](../contracts/choice_clmm_pool/src/actions/flash.rs#L146-L169));
  unforgeable because the lock means the only way a balance can rise mid-callback
  is a direct transfer back. `execute_flash` never writes `POOL_STATE`, so
  liquidity/price/tick are immutable across a flash. Fee follows the P1 carve;
  `L==0` routes the LP share to the protocol bucket (no divide-by-zero, never
  lost). Overpayment is donated to the pool's free balance (matches V3) — benign.
- **Exact-out — OK.** No over/underflow at tick boundaries: `state_amount_remaining
  -= step.amount_out` is guarded by `max_reached` (full step) or an explicit clamp
  to the remaining budget (partial step, [swap_math.rs:261-266](../packages/choice_clmm_math/src/swap_math.rs#L261-L266));
  `amount_in` is always rounded UP and `fee` `mul_div_round_up` — every rounding
  favors the pool, so a swapper can never extract value via dust. Native refund is
  exactly `attached − cost`; the full-fill + `maximum_amount_in` checks revert
  rather than partially fill. Regression: `ext_p3_…` (exact delivery + exact
  refund + quote==exec).
- **Access control — OK.** `SetFeeProtocol` / `CollectProtocol` /
  `UpdateProtocolFeeConfig` all call `assert_factory_owner`, which resolves the
  owner **live** from `factory.GetConfig` ([protocol.rs:35-47](../contracts/choice_clmm_pool/src/actions/protocol.rs#L35-L47))
  so an ownership transfer takes effect immediately and a stale stored owner can't
  be exploited. Burn-auction routing lives only in the owner-gated
  `ProtocolFeeConfig`; there is no non-owner path to redirect it. Hook seam stays
  inert (default `None`/`0`, no setter) — regression `ext_p4_…`.

**Deliverables landed:**
- Two `injective_test_tube` integration tests in `tests/integration.rs`:
  `test_protocol_fee_collect_to_auction_roundtrip` (real `choice_send_to_auction`
  Deposit+ExternalTransfer round-trip + treasury-remainder + owner gating) and
  `test_flash_loan_borrower_roundtrip_and_underpay_revert` (real borrower contract
  `contracts/flash_borrower_mock`, honest repay grows pool by exactly the fee,
  one-unit underpay reverts the whole flash).
- Four per-phase regressions in `tests/exploit_regressions.rs` (`ext_p1`..`ext_p4`)
  plus the extended audit→test table; P5's regressions live in the aggregator crate.
- New test-only contract `contracts/flash_borrower_mock` (+ `make build-flash-borrower-mock`,
  added to `build-all`). `build_release.sh` (Docker, rustc 1.86) rebuilt all artifacts —
  `make build-all`'s local cargo build emits **bulk-memory** wasm the test-tube VM
  rejects ("bulk memory support is not enabled"); integration artifacts MUST come
  from `build_release.sh`.
- **`/security-review` + `/code-review high` were NOT run by the tooling**: this
  checkout is not a git repository, so the diff-based review skills have no diff to
  operate on. The review above was done manually over the same surface. Re-run the
  skills once the tree is under git (`git init` + commit, or run from the upstream
  repo checkout).

**Test totals (session 2):** pool 44 unit + **10** regression (+4 new) + 29 math;
CLMM integration **9** passing (+2 new); `cargo clippy --all-targets` clean.

---

## Build & test commands
```bash
cd choice_exchange
cargo build && cargo test                 # unit
make build-all                            # WASM artifacts (needed for integration)
cargo test --test integration             # injective_test_tube
```

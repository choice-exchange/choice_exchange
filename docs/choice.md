# Choice Exchange — Legacy AMM

Constant-product (x*y=k) AMM forked from TerraSwap, updated to CosmWasm v2 for Injective.

## Contracts

### choice_pair (`contracts/choice_pair/`)

Core trading pool. Each pair holds two assets and mints native LP tokens via Injective's token factory.

**Files:** `contract.rs` (entry points + swap math), `state.rs`, `error.rs`

**Storage:**
```rust
PAIR_INFO: Item<PairInfoRaw>  // asset_infos, contract_addr, liquidity_token, decimals, burn/fee addresses
```

**Swap math:**
```
return_amount = (ask_pool * offer_amount) / (offer_pool + offer_amount)
spread_amount = (offer_amount * ask_pool / offer_pool) - return_amount
commission    = return_amount * 0.003  (0.3%)
```

Fee split (of the 0.3% commission):
- 4/6 (0.20%) — stays in pool (LPs)
- 1/6 (0.05%) — sent to fee_wallet_address
- 1/6 (0.05%) — sent to burn_address (send_to_auction contract)

**Decimal normalization:** All math upscales both assets to `10^max(decimal_0, decimal_1)`, computes, then downscales the result.

**Liquidity provision:**
- Initial: `LP = sqrt(deposit_0 * deposit_1) - 1000` (1000 locked forever as minimum liquidity)
- Subsequent: `LP = min(deposit_0 * total_share / pool_0, deposit_1 * total_share / pool_1)`. Excess refunded.

**LP token denom:** `factory/{pair_contract_address}/lp` (native Injective token factory denom, not CW20)

**Messages (`packages/choice/src/pair.rs`):**

| Execute | Description |
|---------|-------------|
| `ProvideLiquidity { assets, receiver, deadline, slippage_tolerance }` | Deposit both assets, receive LP tokens |
| `WithdrawLiquidity { amount, min_assets, deadline }` | Burn LP tokens, receive proportional assets |
| `Swap { offer_asset, belief_price, max_spread, to, deadline }` | Swap one asset for the other |

| Query | Returns |
|-------|---------|
| `Pair {}` | PairInfo (asset_infos, contract_addr, lp denom, decimals) |
| `Pool {}` | PoolResponse (assets, total_share) |
| `Simulation { offer_asset }` | return_amount, spread_amount, commission_amount |
| `ReverseSimulation { ask_asset }` | offer_amount, spread_amount, commission_amount |

**Errors (`error.rs`):** InvalidZeroAmount, MaxSpreadAssertion, MaxSlippageAssertion, AssetMismatch, ExpiredDeadline, MinAmountAssertion, MinimumLiquidityAmountError, InvalidLiquidityFunds

---

### choice_factory (`contracts/choice_factory/`)

Creates and registers pair contracts. Stores native token decimal mappings.

**Files:** `contract.rs`, `state.rs`

**Storage:**
```rust
CONFIG: Item<Config>                     // owner, pair_code_id, burn_address, fee_wallet_address, proposed_owner
TMP_PAIR_INFO: Item<TmpPairInfo>         // transient state during pair creation (reply pattern)
PAIRS: Map<&[u8], PairInfoRaw>           // pair_key -> pair info
ALLOW_NATIVE_TOKENS: Map<&[u8], u8>     // denom bytes -> decimals
```

**Pair key:** Sorted asset_info byte concatenation.

**Messages (`packages/choice/src/factory.rs`):**

| Execute | Description |
|---------|-------------|
| `CreatePair { assets }` | Instantiate a new pair contract |
| `AddNativeTokenDecimals { denom, decimals }` | Register native/IBC token decimals (must hold balance > 0) |
| `UpdateConfig { params }` | Update pair_code_id, burn_address, fee_wallet_address |
| `MigratePair { contract, code_id }` | Upgrade a pair contract |
| `ProposeNewOwner / AcceptOwnership / CancelOwnershipProposal` | Ownership transfer |

| Query | Returns |
|-------|---------|
| `Config {}` | owner, pair_code_id, burn_address, fee_wallet_address |
| `Pair { asset_infos }` | PairInfo for the given pair |
| `Pairs { start_after, limit }` | Paginated pair list (max 30) |
| `NativeTokenDecimals { denom }` | Registered decimals for a native token |

---

### choice_router (`contracts/choice_router/`)

Multi-hop swap execution and simulation.

**Files:** `contract.rs`, `testing/`

**Messages (`packages/choice/src/router.rs`):**

| Execute | Description |
|---------|-------------|
| `ExecuteSwapOperations { operations, minimum_receive, to, deadline }` | Execute a sequence of swaps |
| `SimulateSwapOperations { offer_amount, operations }` | Simulate multi-hop output |
| `ReverseSimulateSwapOperations { ask_amount, operations }` | Simulate required input |

Each operation: `SwapOperation::Choice { offer_asset_info, ask_asset_info }`

Validation: The chain of operations must have exactly one final output token. Each operation's output is the next operation's input.

---

### choice_send_to_auction (`contracts/choice_send_to_auction/`)

Routes the 0.05% burn fee to Injective's burn auction basket.

**Flow:**
1. Receives native or CW20 tokens
2. For CW20: converts via Injective CW20 adapter to `factory/{adapter}/{cw20_address}` denom
3. Deposits to contract's Injective subaccount
4. Transfers to burn_auction_subaccount via ExternalTransfer

---

## Shared Package: `packages/choice/`

Common types used across legacy contracts.

| File | Key Types |
|------|-----------|
| `asset.rs` | `Asset`, `AssetInfo` (Token/NativeToken), `AssetRaw`, `PairInfo`, `PairInfoRaw` |
| `pair.rs` | `InstantiateMsg`, `ExecuteMsg`, `QueryMsg`, `Cw20HookMsg`, response types |
| `factory.rs` | Factory messages and query responses |
| `router.rs` | Router messages, `SwapOperation` |
| `querier.rs` | Query helpers for pairs, pools, simulations |
| `testing.rs` | Mock querier for unit tests |

## Deployment Order

1. Upload all contract binaries, record code IDs
2. Instantiate send_to_auction
3. Instantiate factory (with send_to_auction as `burn_address`)
4. Instantiate router (with factory address)
5. Register native token decimals on factory (`AddNativeTokenDecimals`)
6. Create pairs via factory (`CreatePair`)

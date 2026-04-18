# Choice Vault — Auto-Compounding

Accepts LP token deposits, stakes them in a Farm contract, and auto-compounds harvested rewards back into LP tokens.

**Location:** `contracts/choice_vault/`
**Files:** `contract.rs` (entry points + compound logic), `state.rs`, `msg.rs`, `error.rs`

## Storage (`state.rs`)

```rust
CONFIG: Item<Config>
// Config {
//   owner, compounder, pair_contract, farm_contract,
//   lp_token: AssetInfo, reward_token: AssetInfo, asset_infos: [AssetInfo; 2],
//   slippage_tolerance: Decimal,
//   fee_recipient: Option<Addr>, fee_percentage: Option<Decimal>,
//   minimum_reward_to_compound: Uint128,
//   proposed_owner: Option<Addr>,
//   reward_to_lp_token_route: Vec<SwapHop>
// }

TOTAL_SHARES: Item<Uint128>
TOTAL_PENDING_DEPOSITS: Item<Uint128>
USERS: Map<&Addr, UserInfo>             // UserInfo { shares: Uint128, pending_deposit: Uint128 }
COMPOUNDING_INFO: Item<CompoundingInfo> // last_compound_time, last_reward_amount_compounded, total_lp_staked_at_last_compound
```

`SwapHop { pair_contract: Addr, to_asset_info: AssetInfo }` — defines multi-hop reward routing.

## Two-Phase Deposit Model

Deposits go through two phases to prevent share price manipulation:

1. **Deposit** — LP tokens added to user's `pending_deposit`, immediately bonded to farm (start earning rewards), but no shares minted yet
2. **ActivatePendingDeposits** — Compounder-only batch operation (max 30 users per call) converts pending deposits to shares at fair rate

Share minting formula:

```text
if total_shares == 0:
    shares = pending_amount
else:
    shares = pending_amount * total_shares / (total_lp_staked - total_pending_deposits)
```

This separates deposit timing from share pricing, preventing sandwich attacks.

## Auto-Compound Flow

Triggered by `Compound {}` (compounder-only). Uses SubMsg reply chain (4 steps):

1. **Harvest** (reply ID 1) — withdraw rewards from farm. Deduct fee if configured. Start reward routing.
2. **Route swaps** (reply ID 2) — execute intermediate hops in `reward_to_lp_token_route`. If no route, skip to final swap.
3. **Final swap** (reply ID 3) — split reward into 50/50 of both LP pair assets. Call pair's `ProvideLiquidity`.
4. **Provide liquidity** (reply ID 4) — receive new LP tokens, bond them back to farm. Save CompoundingInfo metrics.

## Messages

**Execute:**

- `Deposit {}` / `Receive(Cw20ReceiveMsg)` — deposit native or CW20 LP tokens. Added to pending_deposit, bonded to farm immediately.
- `WithdrawPending { amount }` — withdraw LP tokens that haven't been activated as shares yet. Unbonds from farm first.
- `WithdrawShares { shares_to_burn }` — redeem shares for LP tokens: `lp = shares * (total_staked - pending) / total_shares`. When the farm has unharvested rewards, the exit runs through a reply chain: farm.Withdraw → on reply, unbond + transfer LP + transfer the exiter's proportional slice of the reward_token balance. This prevents exiters from forfeiting unharvested rewards.
- `Compound { belief_prices, minimum_lp_to_receive }` — compounder only. Initiates the 4-step auto-compound flow.
- `ActivatePendingDeposits { users: Vec<String> }` — compounder only. Batch convert pending deposits to shares (max 30 users). Refuses to run while farm's `pending_reward >= max(1, minimum_reward_to_compound)` to prevent dilution of existing shareholders.
- `ActivateMyDeposit {}` — any user, activates their own pending deposit. Same dilution guard as the batch variant.
- `UpdateConfig { ... }` — owner only. Update slippage, fees, minimum_reward_to_compound. Compounder rotation is intentionally excluded — use the timelocked propose/apply flow below.
- `ProposeCompounder { new_compounder }` — owner only. Stages a compounder rotation that cannot take effect for `COMPOUNDER_ROTATION_DELAY_SECONDS` (48h).
- `ApplyCompounderRotation` — owner only. Finalizes the rotation once the timelock has elapsed.
- `CancelCompounderProposal` — owner only. Clears a pending rotation.
- `ProposeNewOwner / AcceptOwnership / CancelOwnershipProposal` — ownership transfer.

**Instantiate validation:** the compound path must terminate on one of the pair's two assets — either `reward_token` itself is a pair asset and `reward_to_lp_token_route` is empty, or the last hop's `to_asset_info` equals a pair asset. Otherwise instantiate rejects with `CompoundPathMustEndOnPairAsset`, since the final 50/50 swap would offer a token `pair_contract` doesn't trade.

**Query:**

- `Config {}` — returns Config
- `TotalShares {}` — total shares issued
- `UserInfo { user }` — user's shares and pending_deposit
- `CompoundingInfo {}` — last compound time and metrics (useful for APR calculation)
- `PendingDeposits { start_after, limit }` — paginated list of users with pending deposits
- `TotalPendingDeposits {}` — sum of all pending LP deposits
- `PendingCompounderRotation {}` — `{ pending_compounder, effective_at }` for a staged rotation

## Errors (`error.rs`)

- `Unauthorized` — caller not owner/compounder
- `InsufficientShares` — burning more shares than owned
- `InvalidCw20HookMsg` — bad CW20 hook payload
- `InvalidFeePercentage` — fee > 100%
- `BatchTooLarge` — ActivatePendingDeposits with > 30 users

## Key Behaviors

- Pending deposits earn farm rewards immediately (bonded on deposit), but don't receive shares until activation
- Compounder is a separate privileged address (typically a bot) that calls `Compound` and `ActivatePendingDeposits`
- Fee deducted from harvested rewards before compounding, not from deposits or withdrawals
- Multi-hop reward routing supports converting any reward token to LP pair assets
- `minimum_reward_to_compound` prevents wasteful compounds when rewards are too small

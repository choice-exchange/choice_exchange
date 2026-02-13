# Choice Farm — Staking & Rewards

LP staking contract that distributes reward tokens over configurable time-based schedules.

**Location:** `contracts/choice_farm/`
**Files:** `contract.rs` (entry points + reward math), `state.rs`, `msg.rs`
**No custom error type** — uses `cosmwasm_std::StdError` directly.

## Storage (`state.rs`)

```rust
CONFIG: Item<Config>
// Config { owner, reward_token: AssetInfo, staking_token: AssetInfo, distribution_schedule: Vec<(u64, u64, Uint128)> }
// distribution_schedule entries: (start_time, end_time, total_amount)

STATE: Item<State>
// State { last_distributed: u64, total_bond_amount: Uint128, global_reward_index: Decimal }

STAKER_INFO: Map<&[u8], StakerInfo>   // canonical address -> staker info
// StakerInfo { reward_index: Decimal, bond_amount: Uint128, pending_reward: Uint128 }
```

## Reward Distribution Math

Uses a **global reward index** pattern for gas-efficient per-user accounting.

**compute_reward()** — called before any state change:

1. For each schedule slot `(start, end, amount)`:
   - Overlap with `[last_distributed, now]`: `min(end, now) - max(start, last_distributed)`
   - Per-second rate: `amount / (end - start)`
   - Distributed in window: `overlap * rate`
2. Sum all distributed amounts across slots
3. Increment global index: `global_reward_index += distributed / total_bond_amount`
4. Update `last_distributed = now`

**compute_staker_reward()** — per-user:

```text
reward = bond_amount * (global_reward_index - staker.reward_index)
staker.pending_reward += reward
staker.reward_index = global_reward_index
```

## Messages

**Execute:**

- `Bond {}` — stake tokens (native via funds, CW20 via Receive hook). Computes rewards first, then increases bond.
- `Unbond { amount }` — unstake tokens. Computes rewards, decreases bond, sends staking tokens back. Cleans up storage if user has zero bonds and rewards.
- `Withdraw {}` — claim pending rewards. Sends reward tokens, zeros pending_reward. Removes user from storage if no bonds remain.
- `UpdateConfig { distribution_schedule }` — owner only. Validates new schedules don't remove already-started slots.
- `MigrateStaking { new_staking_contract }` — owner only. Computes remaining undistributed rewards, transfers them to a new contract. Used for contract upgrades.

**Query:**

- `Config {}` — returns Config
- `State { block_time }` — returns State (optionally computed at a specific time)
- `StakerInfo { staker, block_time }` — returns StakerInfo with pending rewards computed up to block_time

## Key Behaviors

- Rewards computed lazily on every bond/unbond/withdraw — no cron needed
- Multiple overlapping schedule slots supported (e.g., phase 1 and phase 2 running concurrently)
- If `total_bond_amount == 0`, distributed rewards are effectively lost (no one to receive them)
- Staker storage cleaned up when both `bond_amount` and `pending_reward` are zero
- Supports both native and CW20 as staking and reward tokens

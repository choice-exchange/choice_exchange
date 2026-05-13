# Choice Farm — Staking & Rewards

LP staking contract that distributes reward tokens over configurable
time-based schedules. Spawned by `choice_farm_factory`; the factory's wasm
admin (and every spawned farm's wasm admin) is held by a separate
`choice_admin_timelock` contract that delays `MsgMigrateContract` by the
same window users have to exit.

**Location:** `contracts/choice_farm/`
**Files:** `contract.rs` (entry points + reward math), `state.rs`,
`mock_querier.rs` (test fixture)
**No custom error type** — uses `cosmwasm_std::StdError` directly.

## Storage (`state.rs`)

```rust
CONFIG: Item<Config>
// Config {
//     owner, reward_token: AssetInfo, staking_token: AssetInfo,
//     distribution_schedule: Vec<(u64, u64, Uint128)>,
//     pending_owner, pending_owner_effective_at,
// }
// distribution_schedule entries: (start_time, end_time, total_amount)
//   - validated at instantiate AND at every apply: start < end, amount > 0,
//     slot duration <= MAX_SCHEDULE_SLOT_DURATION_SECONDS (4 years), at
//     most MAX_SCHEDULE_SLOTS (20) slots.

STATE: Item<State>
// State {
//     last_distributed, total_bond_amount, global_reward_index,
//     undistributed_rewards,
//     unclaimed_pending,            // L-1: total rewards already credited
//                                   // to the global index but not yet
//                                   // withdrawn by stakers. Mirrors the
//                                   // contract's outstanding obligation.
// }

STAKER_INFO: Map<&[u8], StakerInfo>   // canonical address -> staker info
// StakerInfo { reward_index, bond_amount, pending_reward }

PENDING_MIGRATION:     Item<PendingMigration>     // queued migrate_staking
PENDING_CONFIG_UPDATE: Item<PendingConfigUpdate>  // H-2: queued schedule update
PENDING_FARM:          (factory-side; see choice_farm_factory)

LAST_SEEN_CW20_BALANCE: Map<&[u8], Uint128>
// M-1: cached cw20 balance the farm last reconciled. Keyed by the canonical
// address of the CW20 token contract (reward and staking may differ). On
// every Receive hook the farm queries its actual balance, compares the
// delta to the claimed amount, and rejects if the CW20 lied about a
// transfer. Decremented on outbound Cw20::Transfer (Withdraw / Unbond /
// apply_migrate_staking) before the message dispatches.
```

Constants:

- `TIMELOCK_DELAY_SECONDS = 48h` — propose/apply delay for migration,
  schedule update, and owner rotation.
- `MAX_SCHEDULE_SLOTS = 20` — bounds compute_reward gas.
- `MAX_SCHEDULE_SLOT_DURATION_SECONDS = 4 years` — bounds how far a
  compromised owner can stretch a single emission slot.

## Reward Distribution Math

Uses a **global reward index** pattern for gas-efficient per-user accounting.

**compute_reward()** — called before any state change. Overflow-protected
since security review (H-1, L-1, M-2):

1. If `block_time <= state.last_distributed`, no-op.
2. **M-2:** if `state.total_bond_amount == 0`, return *without advancing*
   `last_distributed`. The schedule effectively pauses while there are no
   stakers; the next bonder sweeps up the empty window's emission. This
   preserves the full advertised budget. Documented operator-visible
   behavior change vs. pre-hardening designs that bumped `last_distributed`
   and stranded the empty-window emission.
3. For each schedule slot `(start, end, amount)`:
   - Overlap with `[last_distributed, now]`:
     `min(end, now) - max(start, last_distributed)`.
   - Distributed in window: `amount.multiply_ratio(overlap, end - start)`
     (single Uint128 floor, not double-floor).
4. `distributed = min(theoretical, state.undistributed_rewards)` — solvency
   cap.
5. Compute index increment with overflow protection:
   - `raw_increment = Decimal::checked_from_ratio(distributed, total_bond)`
     — capped at `Decimal::MAX` if the ratio is unrepresentable.
   - `headroom = Decimal::MAX - state.global_reward_index`.
   - `applied_increment = min(raw_increment, headroom)`.
6. Compute the corresponding raw-unit amount actually credited:
   `credited = total_bond.checked_mul_floor(applied_increment)`. Bounded at
   `distributed` and at `undistributed_rewards`.
7. `state.undistributed_rewards -= credited`,
   `state.unclaimed_pending    += credited`,
   `state.global_reward_index  += applied_increment`,
   `state.last_distributed     = block_time`.

When `raw_increment` overflows or `global_reward_index` would exceed
`Decimal::MAX`, the surplus stays in `undistributed_rewards` — the next
call (with healthier ratio or a Withdraw that lowers index_delta) picks it
up. **H-1 regression test** exercises this: a single bonder of 1 raw unit
on a huge-emission schedule no longer panics in `compute_reward`.

**compute_staker_reward()** — per-user, saturating (L-3):

```text
index_delta   = global_reward_index.checked_sub(staker.reward_index)
pending       = bond_amount.checked_mul_floor(index_delta)
                  .unwrap_or(Uint128::MAX)            # L-3: saturate
staker.reward_index   = global_reward_index
staker.pending_reward = staker.pending_reward.saturating_add(pending)
```

No state mutation: `unclaimed_pending` is incremented in `compute_reward`
when credit moves from `undistributed_rewards` into the implicit
"owed-via-schedule" bucket; `compute_staker_reward` just shuffles credit
from the implicit bucket into the user's explicit `pending_reward` —
total owed is unchanged.

## Messages

**Execute:**

- `Bond { amount }` — stake tokens.
  - Native staking: send `amount` of the configured denom in `info.funds`.
  - CW20 staking: invoked via `Cw20::Send(amount, farm, HookMsg::Bond)`.
    The farm reconciles its self-balance (M-1) before crediting.
- `Unbond { amount }` — unstake. Sweeps user rewards, decrements bond,
  sends staking tokens back. Decrements `LAST_SEEN_CW20_BALANCE` for CW20
  staking before dispatching the outbound `Cw20::Transfer`. Cleans up
  `STAKER_INFO` when both `bond_amount` and `pending_reward` are zero.
- `Withdraw {}` — claim pending rewards. Sends reward tokens, zeros
  `staker.pending_reward`, subtracts the paid amount from
  `state.unclaimed_pending`. Decrements CW20 last-seen for CW20 rewards.
- `Fund {}` (native) / `Receive(HookMsg::Fund {})` (CW20) — anyone can
  top up `state.undistributed_rewards`. CW20 reconcile applies.
- **H-2 schedule update (timelocked):**
  - `ProposeUpdateConfig { distribution_schedule }` — owner; queues.
  - `ApplyUpdateConfig {}` — owner; installs after timelock. Re-runs
    `validate_distribution_schedule` and `assert_new_schedules` against
    the *now* state so a started slot in the queued schedule is rejected.
  - `CancelUpdateConfigProposal {}` — owner; clears the queue.
- **Migration (timelocked):**
  - `ProposeMigrateStaking { new_staking_contract }` — owner; queues.
  - `ApplyMigrateStaking {}` — owner; after timelock. Sweeps the
    contract's actual reward-token balance (via Bank or `Cw20::Balance`
    query), subtracts `state.total_bond_amount` (if reward == staking
    denom) and `state.unclaimed_pending`, forwards the rest to
    `new_staking_contract`. Emits a `migration_notice` event attribute
    telling stakers to `Unbond` + `Withdraw` from the old farm.
  - `CancelMigrateStakingProposal {}` — owner.
- **Owner rotation (timelocked):**
  - `ProposeNewOwner { new_owner }` / `ApplyOwnerRotation {}` /
    `CancelOwnerProposal {}`.

`UpdateConfig` (instant variant) was removed in the security pass — schedule
mutation must go through the propose/apply path.

**Query:**

- `Config {}`
- `State { block_time }`
- `StakerInfo { staker, block_time }` — pending computed up to block_time.
- `PendingOwnerRotation {}`
- `PendingMigration {}`
- `PendingConfigUpdate {}` — H-2 surface for indexers / front-ends.

## Key Behaviors

- Rewards computed lazily on every bond/unbond/withdraw — no cron needed.
- Multiple overlapping schedule slots supported.
- **M-2:** if `total_bond_amount` drops to 0, `last_distributed` does
  *not* advance. Operators must understand that the schedule's wall-clock
  end can effectively stretch beyond the originally-published end while
  the farm sits empty. The full advertised budget is honoured to whichever
  staker is bonded when emissions resume.
- **L-1:** `apply_migrate_staking` forwards every reward-token unit that is
  not bonded (when staking and reward share a denom) and not earmarked for
  a credited-but-unwithdrawn user claim. Floor-truncation dust is swept
  out by this path.
- **M-1:** the contract trusts the CW20 token to be honest only as far as
  the on-chain balance it actually holds. A reward or staking CW20 that
  invokes `farm.Receive` without first moving tokens is rejected; the
  exploit of "inflating bond_amount via fake Receive calls" is closed.
- **C-1:** the wasm admin is the `choice_admin_timelock` contract. Any
  `MsgMigrateContract` is delayed by the admin-timelock's configured
  window so users have the same 48 h exit they get from
  `ProposeMigrateStaking`.

## Sibling contracts

- `choice_farm_factory` ([../contracts/choice_farm_factory/](../contracts/choice_farm_factory/)) — spawns farms, holds the registry, enforces a `ContractInfo`-based admin check at `CreateFarm` (refuses to spawn unless the factory's own wasm admin matches `config.farm_owner`).
- `choice_admin_timelock` ([../contracts/choice_admin_timelock/](../contracts/choice_admin_timelock/)) — holds wasm-admin powers over the factory and every farm. Delays `MsgMigrateContract` for `timelock_seconds` (≥ 1 h floor, 48 h in production).

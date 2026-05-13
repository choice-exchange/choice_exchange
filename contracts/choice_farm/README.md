# choice_farm — Staking & Rewards

LP staking contract that distributes a reward token to bonded stakers over a
time-based `distribution_schedule`. Forked from the Anchor protocol staking
contract; hardened in the v1.1.2 security pass.

This README is a quick orientation. For the deep dive — reward-index math,
overflow protection (H-1), the empty-window pause behavior (M-2), CW20
self-balance reconciliation (M-1), apply_migrate_staking dust sweep (L-1) —
see [`docs/farm.md`](../../docs/farm.md).

## Role in the system

```text
                  ┌────────────────────────┐
                  │ choice_admin_timelock  │  ← wasm admin (factory + farms)
                  │  (48 h delay on        │
                  │   MsgMigrateContract)  │
                  └────────────┬───────────┘
                               │ admin =
                               │
                  ┌────────────▼───────────┐
                  │  choice_farm_factory   │  ← spawns farms, holds registry
                  │  CreateFarm fee 1 INJ  │
                  └────────────┬───────────┘
                               │ instantiates with
                               │ admin = farm_owner
                               │ owner = farm_owner
                  ┌────────────▼───────────┐
                  │      choice_farm       │  ← this contract
                  │  bond/unbond/withdraw  │
                  └────────────────────────┘
```

A `choice_farm` is **always** spawned by the factory in production —
instantiating it standalone is supported for testing only. Both the
factory's wasm admin and each farm's wasm admin are the
`choice_admin_timelock` contract, so any code migration is delayed by the
same window users get from `ProposeMigrateStaking`.

## Lifecycle

1. **Spawn** (via factory): `CreateFarm` on the factory pays a 1 INJ launch
   fee + the full reward budget, then instantiates this contract with
   `admin = farm_owner` and `Config.owner = farm_owner`. The factory
   forwards the reward in the reply via `ExecuteMsg::Fund {}` so
   `undistributed_rewards` is credited atomically.

2. **Operation**:
   - Stakers call `Bond { amount }` (native) or `Cw20::Send + HookMsg::Bond`
     (CW20). The farm reconciles its self-balance against
     `LAST_SEEN_CW20_BALANCE` before crediting — a CW20 that fires
     `Receive` without actually moving tokens is rejected.
   - `Withdraw {}` claims pending rewards; `Unbond { amount }` decrements
     stake and sweeps rewards first.
   - Rewards accrue lazily on every state-changing call. No cron.

3. **Schedule update** (timelocked, 48 h):
   - `ProposeUpdateConfig { distribution_schedule }` → owner queues.
   - `ApplyUpdateConfig {}` → owner installs after the timelock elapses.
     Re-validates the schedule against current block time so a slot that
     started during the wait window is rejected.
   - `CancelUpdateConfigProposal {}` clears the queue.
   - There is no instant `UpdateConfig` — it was removed in the security
     pass.

4. **Migration to a new staking contract** (timelocked, 48 h):
   - `ProposeMigrateStaking { new_staking_contract }` → owner queues.
   - `ApplyMigrateStaking {}` → owner sweeps the farm's actual reward-token
     balance (Bank or `Cw20::Balance` query), subtracts
     `total_bond_amount` (when reward denom == staking denom) and
     `unclaimed_pending`, forwards the rest to `new_staking_contract`.
     Emits a `migration_notice` event attribute telling stakers to
     `Unbond` + `Withdraw` from the old farm.
   - `CancelMigrateStakingProposal {}` clears the queue.

5. **Owner rotation** (timelocked, 48 h):
   `ProposeNewOwner` → `ApplyOwnerRotation` → `CancelOwnerProposal`.

6. **Code migration** (timelocked at the admin-timelock contract):
   The farm's wasm admin is the `choice_admin_timelock`. Any
   `MsgMigrateContract` against this farm must be proposed at the timelock
   contract, wait `timelock_seconds` (48 h in production), and then be
   applied. The same window applies to the factory and to every other farm
   spawned under the same timelock.

## Messages — quick reference

### Execute

| Message | Auth | Timelock | Notes |
| --- | --- | --- | --- |
| `Bond { amount }` | anyone | — | Native: include `amount` of `staking_token.denom` in funds. |
| `Receive(HookMsg::Bond {})` | CW20 staking token | — | CW20 path; self-balance reconciled (M-1). |
| `Unbond { amount }` | staker | — | Sweeps pending reward first; cleans `STAKER_INFO` row when both fields hit zero. |
| `Withdraw {}` | staker | — | Sends `pending_reward` of `reward_token`; zeros `staker.pending_reward`; decrements `unclaimed_pending`. |
| `Fund {}` / `Receive(HookMsg::Fund {})` | anyone | — | Top up `undistributed_rewards`. |
| `ProposeUpdateConfig { distribution_schedule }` | owner | queues | Re-validated at `Apply`. |
| `ApplyUpdateConfig {}` | owner | 48 h | Rejects schedules whose earliest slot has already started. |
| `CancelUpdateConfigProposal {}` | owner | — | |
| `ProposeMigrateStaking { new_staking_contract }` | owner | queues | |
| `ApplyMigrateStaking {}` | owner | 48 h | Sweeps reward balance minus bonded + unclaimed. |
| `CancelMigrateStakingProposal {}` | owner | — | |
| `ProposeNewOwner { new_owner }` | owner | queues | |
| `ApplyOwnerRotation {}` | owner | 48 h | |
| `CancelOwnerProposal {}` | owner | — | |

### Query

| Query | Returns |
| --- | --- |
| `Config {}` | owner, reward/staking token, distribution_schedule |
| `State { block_time }` | last_distributed, total_bond_amount, global_reward_index, undistributed_rewards, unclaimed_pending |
| `StakerInfo { staker, block_time }` | bond_amount, pending_reward (projected to `block_time`), reward_index |
| `PendingOwnerRotation {}` | pending_owner, effective_at |
| `PendingMigration {}` | new_staking_contract, effective_at |
| `PendingConfigUpdate {}` | distribution_schedule, effective_at |

## Constants

- `TIMELOCK_DELAY_SECONDS = 48 h` — Propose → Apply window for **every** of
  the farm's three timelocked paths (config update, migrate staking, owner
  rotation). Hard-coded; not a config field.
- `MAX_SCHEDULE_SLOTS = 20` — Bounds `compute_reward` gas.
- `MAX_SCHEDULE_SLOT_DURATION_SECONDS = 4 years` — Bounds how far a
  compromised owner can stretch a single emission slot.

## Behavioral surprises worth knowing

- **Empty-window pause (M-2):** if `total_bond_amount` drops to zero,
  `last_distributed` does *not* advance. The schedule pauses; the next
  staker to bond sweeps up the missed window. The full advertised budget
  is honoured, but the schedule's effective end can extend past its
  published `end_time` while the farm sits empty.
- **Lazy accrual:** rewards are only computed on bond/unbond/withdraw/fund.
  A passive farm with no calls will show stale `last_distributed`;
  `Query::State` and `Query::StakerInfo` accept a `block_time` arg and
  project forward for read-only callers.
- **CW20 honesty:** `LAST_SEEN_CW20_BALANCE` is the contract's source of
  truth for what a CW20 reward/staking token has actually delivered.
  A `Receive` whose claimed amount doesn't match the on-chain balance
  delta is rejected (M-1). Outbound CW20 transfers (Withdraw, Unbond,
  apply_migrate_staking) decrement this cache *before* dispatching.
- **Unclaimed pending obligations:** `state.unclaimed_pending` mirrors how
  much credit has been moved from `undistributed_rewards` into staker
  `pending_reward` but not yet withdrawn (L-1). `apply_migrate_staking`
  forwards `reward_balance - bonded - unclaimed_pending`, so dust from
  floor-truncation is swept out by this path; users still see the rewards
  the index already credited them.

## Build

```bash
cargo build -p choice-farm
cargo test  -p choice-farm
```

## See also

- [`docs/farm.md`](../../docs/farm.md) — deep dive on reward math + security
  notes.
- [`contracts/choice_farm_factory/`](../choice_farm_factory/) — factory
  README.
- [`contracts/choice_admin_timelock/`](../choice_admin_timelock/) — admin
  timelock README.
- [`deploy/guide.md`](../../deploy/guide.md) — operator flow for deploying
  the timelock + factory + farms together.

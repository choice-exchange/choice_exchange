# choice_farm_factory — Farm Spawner & Registry

Single gateway through which all production `choice_farm` instances are
created. Collects a 1 INJ anti-spam launch fee, pulls the full reward
budget from the caller, instantiates the farm with the canonical
`farm_owner` (the admin timelock) as both `Config.owner` and wasm admin,
and records the result in an on-chain registry indexers and the front end
read from.

## Role in the system

```text
   user (operator) ──CreateFarm(fee + reward)──► choice_farm_factory
                                                       │
                                                       │ instantiate (admin = farm_owner)
                                                       ▼
                                                  choice_farm
                                                       │
                                                       │ admin
                                                       ▼
                                            choice_admin_timelock
                                              (48 h migration delay)
```

- `farm_owner` is configured at factory-instantiate time and **must** be a
  `choice_admin_timelock` address. It becomes:
  - the wasm admin of every spawned farm,
  - the wasm admin of the factory itself (enforced — see C-1 below),
  - the `Config.owner` of every spawned farm.
- The **operator** is whoever pays the launch fee and chooses the
  schedule. They have **no on-chain role** on the spawned farm; their
  address is recorded in `FarmRecord.operator` for off-chain attribution.

## CreateFarm flow

1. Caller (operator) prepares funds:
   - Native-reward farm: send `instantiate_fee_inj` + `total_reward` in
     `info.funds` (combined when the reward denom is `inj`).
   - CW20-reward farm: send `instantiate_fee_inj` in `info.funds` and run
     `cw20::IncreaseAllowance(spender = factory, amount = total_reward)`
     first. The factory pulls via `TransferFrom`.
2. **C-1 admin assertion**: the factory queries its own `ContractInfo`
   and refuses to spawn unless `ContractInfo.admin == config.farm_owner`.
   This prevents a deployment-hygiene mistake (e.g. `MsgUpdateAdmin` ran
   on the factory but `farm_owner` not updated to match) from producing a
   farm whose admin diverges from what `FarmRecord.farm_owner` reports.
3. The schedule is validated (`MAX_SCHEDULE_SLOTS = 20`, every slot's
   `end` in the future, every slot non-empty).
4. The factory sends `instantiate_fee_inj` of `inj` to `fee_collector` via
   `BankMsg`, instantiates the farm with empty `funds`, and in the
   reply-on-success handler funds the new farm via `ExecuteMsg::Fund {}`
   (native) or `Cw20HookMsg::Fund {}` (CW20) — so
   `state.undistributed_rewards` is credited atomically with farm creation.
5. A `FarmRecord` is written under both the auto-incrementing id (`FARMS`
   map) and the farm bech32 (`FARM_BY_ADDR` map).

If any step fails the whole tx reverts — there is no half-spawned state.
The transient `PENDING_FARM` singleton is consumed by the reply or unwound
on revert.

## Lifecycle / governance

The factory's owner is separate from `farm_owner` — both default to the
governance multisig in production, but they can be different:

- **`owner`** (factory) — calls `UpdateConfig`, `ProposeUpdateFarmCodeId`,
  owner rotation. Typically the multisig directly so policy changes don't
  pay the 48 h delay.
- **`farm_owner`** (configured into every spawned farm) — owns the
  spawned farms. Must be the admin-timelock contract so farm migrations
  are user-visible 48 h in advance.

### Instant updates (owner-only, no timelock)

`UpdateConfig { fee_collector?, instantiate_fee_inj?, farm_owner? }` —
only affects **future** `CreateFarm` calls. Existing farms keep the
`farm_owner` captured at their creation time (it's stored on the
`FarmRecord` and inside the farm itself).

### Timelocked updates (48 h, `TIMELOCK_DELAY_SECONDS`)

- **`farm_code_id` swap** (H-2): changing the code that future farms are
  spawned with is gated so users have a 48 h heads-up before a new code
  starts shipping.
  - `ProposeUpdateFarmCodeId { farm_code_id }`
  - `ApplyUpdateFarmCodeId {}` — owner; only after `effective_at`.
  - `CancelUpdateFarmCodeIdProposal {}` — clears the queue.
- **Owner rotation**: `ProposeNewOwner` → `ApplyOwnerRotation` →
  `CancelOwnerProposal`. Mirror of the farm's path.

`farm_code_id` is the only field the factory swaps under timelock —
fee_collector, fee amount, and `farm_owner` for *future* farms are
instant by design (they don't retroactively change a deployed farm's
behavior).

### Factory-contract migration

The factory's own wasm migration is also delayed, but the delay is
enforced by the **admin timelock contract**, not the factory:

- Migration is proposed at `choice_admin_timelock.Propose { contract:
  <factory addr>, code_id, msg }`.
- After `timelock.timelock_seconds` (48 h prod) anyone can call
  `Apply {}` on the timelock; the timelock dispatches the
  `MsgMigrateContract` against the factory.

This is the same mechanism that gates farm migrations — see
[`../choice_admin_timelock/`](../choice_admin_timelock/).

## Messages — quick reference

### Execute

| Message | Auth | Timelock | Notes |
| --- | --- | --- | --- |
| `CreateFarm { reward_token, staking_token, distribution_schedule }` | anyone | — | Pays 1 INJ + reward budget; requires C-1 admin match. |
| `UpdateConfig { fee_collector?, instantiate_fee_inj?, farm_owner? }` | owner | instant | Only affects future farms. |
| `ProposeUpdateFarmCodeId { farm_code_id }` | owner | queues | |
| `ApplyUpdateFarmCodeId {}` | owner | 48 h | |
| `CancelUpdateFarmCodeIdProposal {}` | owner | — | |
| `ProposeNewOwner { new_owner }` | owner | queues | |
| `ApplyOwnerRotation {}` | owner | 48 h | |
| `CancelOwnerProposal {}` | owner | — | |

### Query

| Query | Returns |
| --- | --- |
| `Config {}` | owner, fee_collector, instantiate_fee_inj, farm_code_id, farm_owner |
| `PendingOwnerRotation {}` | pending_owner, effective_at |
| `PendingFarmCodeIdUpdate {}` | farm_code_id, effective_at |
| `Farm { id }` | one `FarmRecord` |
| `FarmByAddr { addr }` | one `FarmRecord` |
| `Farms { start_after?, limit? }` | paginated registry scan; limit ≤ 100, default 30 |
| `FarmCount {}` | total farms ever created (== next id) |

## Constants

- `TIMELOCK_DELAY_SECONDS = 48 h` — `farm_code_id` swap + owner rotation.
- `MAX_SCHEDULE_SLOTS = 20` — mirrors `choice_farm` so the factory rejects
  cleanly before burning gas on a sub-call.
- `INJ_DENOM = "inj"` — launch fee denom.
- `INSTANTIATE_FARM_REPLY_ID = 1` — reply id for the spawn submessage.

## Operational surprises worth knowing

- **The factory's own wasm admin matters** — `CreateFarm` queries
  `ContractInfo.admin` on every call. If somebody runs `MsgUpdateAdmin`
  on the factory (e.g. rotating off a deploy key) and forgets to
  re-point it at the timelock, every subsequent `CreateFarm` fails with
  `factory admin mismatch`. The deploy script
  [`deploy/instantiate_farm_factory.sh`](../../deploy/instantiate_farm_factory.sh)
  warns up-front when `ADMIN != FARM_OWNER`.
- **`farm_owner` is frozen per-farm at creation.** Updating the global
  `farm_owner` via `UpdateConfig` only affects farms created afterward.
  Existing farms still report (and obey) the value captured into their
  `FarmRecord`.
- **No partial spawn state.** `PENDING_FARM` is a transient singleton
  written by `execute_create_farm` and consumed by the reply. A failing
  submsg reverts the whole tx (clearing the write). The reentrancy guard
  rejects nested `CreateFarm` from a CW20 reward token's `TransferFrom`
  handler.
- **CW20 vs native reward funding.** Native rewards must arrive in
  `info.funds`; CW20 rewards must be pre-approved. Either way, the farm
  is instantiated with empty `funds` and gets the reward via a follow-up
  `Fund {}` in the reply — so the farm's `state.undistributed_rewards`
  always matches what was actually delivered.

## Build

```bash
cargo build -p choice-farm-factory
cargo test  -p choice-farm-factory
```

## See also

- [`../choice_farm/`](../choice_farm/) — the contract the factory spawns.
- [`../choice_admin_timelock/`](../choice_admin_timelock/) — the wasm-admin
  holder; gates factory + farm migrations.
- [`../../docs/farm.md`](../../docs/farm.md) — farm reward math + security
  notes.
- [`../../deploy/guide.md`](../../deploy/guide.md) — operator-facing
  deploy flow (timelock → factory → CreateFarm).

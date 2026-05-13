# choice_admin_timelock — Wasm-Admin Timelock

Standalone contract that holds wasm-admin powers over the
`choice_farm_factory` and every spawned `choice_farm`. Its sole job is to
delay `MsgMigrateContract` by a configurable `timelock_seconds` (1 h
minimum, 48 h in production) so users observe a pending migration before
it lands, giving them the same exit window they get from the farm's own
`ProposeMigrateStaking` flow.

The contract is intentionally minimal: it has no business logic and no
control over funds — it just queues, holds, and dispatches `WasmMsg::Migrate`
messages on behalf of an owner (typically the Choice governance multisig).

## Role in the system

```text
                  governance multisig
                          │
                          │ owner of
                          ▼
              ┌─────────────────────────┐
              │  choice_admin_timelock  │  ← THIS contract
              │  timelock_seconds:48 h  │
              └────────────┬────────────┘
                           │ wasm admin of:
            ┌──────────────┼──────────────┐
            ▼              ▼              ▼
       factory          farm A          farm B   …
```

The multisig (or any owner the contract is configured with) cannot migrate
the factory or a farm directly — it must `Propose` against the timelock,
wait `timelock_seconds`, then anyone may `Apply` to settle the migration.

## Why "anyone may apply"

Both `Apply {}` (migration) and `ApplyOwnerRotation {}` are **permissionless**
on purpose. The owner queues the action; the timelock enforces the delay;
once the delay has elapsed, any wallet can complete it. This means:

- If the owner multisig is unreachable when the timelock expires, users
  watching the queued migration can still settle it.
- An adversary that learns the owner's keys cannot indefinitely stall a
  rotation they had already queued.

## Lifecycle

### Instantiate

```json
{
  "owner": "inj1...multisig...",
  "timelock_seconds": 172800
}
```

- `timelock_seconds` ≥ `MIN_TIMELOCK_SECONDS` (3600 = 1 h). Anything below
  is rejected at instantiate. The deploy script
  [`deploy/instantiate_admin_timelock.sh`](../../deploy/instantiate_admin_timelock.sh)
  bails early before broadcasting.
- `timelock_seconds` is **immutable** once instantiated. To change it,
  ship a new timelock contract and rotate the wasm admins of the factory
  and existing farms over to it (which itself goes through the *old*
  timelock — a one-way ratchet by design).
- The contract refuses any attached funds (`info.funds.is_empty()`).
- The contract's own wasm admin (the `--admin` you pass at instantiate
  time) is whoever may later migrate the timelock itself. In early life
  this is typically the deployer key; ops rotates it to clear-admin or a
  separate higher-tier multisig once the deploy is stable.

### Queue a migration

Owner calls:

```json
{
  "propose": {
    "contract": "inj1...factory_or_farm...",
    "code_id": 1234,
    "msg": "<base64-encoded MigrateMsg JSON>"
  }
}
```

A subsequent `Propose` overwrites the pending one (and resets the timer).
A pending migration is observable via `QueryMsg::PendingMigration {}` so
the front end and indexers can surface it.

### Apply

Anyone calls `{ "apply": {} }` once `effective_at <= block_time`. The
timelock dispatches `WasmMsg::Migrate { contract_addr, new_code_id, msg }`
as itself — which is why the timelock must be the target contract's wasm
admin for the migration to succeed.

### Cancel

Owner-only `{ "cancel": {} }` clears the queue with no delay. Useful when
the proposal was a mistake or when a new proposal is being prepared.

### Owner rotation

`ProposeNewOwner { new_owner }` → `ApplyOwnerRotation {}` (anyone, after
delay) → `CancelOwnerProposal {}` (owner). Same 48 h delay applies. Use
this when rotating the governance multisig itself.

## Messages — quick reference

### Execute

| Message | Auth | Timelock | Notes |
| --- | --- | --- | --- |
| `Propose { contract, code_id, msg }` | owner | queues | Overwrites any existing proposal. |
| `Apply {}` | **anyone** | after delay | Dispatches `WasmMsg::Migrate`. |
| `Cancel {}` | owner | — | Clears the pending migration. |
| `ProposeNewOwner { new_owner }` | owner | queues | |
| `ApplyOwnerRotation {}` | **anyone** | after delay | |
| `CancelOwnerProposal {}` | owner | — | |

### Query

| Query | Returns |
| --- | --- |
| `Config {}` | owner, timelock_seconds |
| `PendingMigration {}` | contract, code_id, msg, effective_at — all `Option`, all `None` when nothing is queued |
| `PendingOwnerRotation {}` | pending_owner, effective_at |

## Constants

- `MIN_TIMELOCK_SECONDS = 3600` (1 h) — enforced at instantiate; rejects
  fat-fingered values close to zero.
- `timelock_seconds` — configured per-instance; recommended values:
  - **mainnet**: `172800` (48 h)
  - **testnet (fast)**: `3600` (1 h)
  - **testnet (mainnet rehearsal)**: `172800` (48 h)

## Operational notes

- **Migrate the targets, not the timelock.** Routine code updates run
  `Propose` on this contract against the target (factory / farm / etc.).
  Migrating the timelock contract itself is unusual — `MigrateMsg {}` is
  defined (with a `cw2::get_contract_version` guard) but you generally
  ship a new timelock and rotate admins instead.
- **One pending migration at a time.** The contract holds at most one
  `PendingMigration`. Queueing a second `Propose` overwrites the first
  and resets the timer. To migrate multiple contracts in sequence, apply
  the first, then propose the next.
- **`msg` is opaque to the timelock.** The base64-encoded `MigrateMsg` is
  whatever the *target* contract's `migrate` entry point expects (e.g.
  `{}` for the farm and factory's current empty `MigrateMsg`). The
  timelock does not validate the payload.
- **Public-apply implication.** Don't assume the owner is the one who
  applies. Off-chain monitoring should treat the `apply` event as "the
  delay elapsed and someone settled it," not "the owner acted."

## Build

```bash
cargo build -p choice-admin-timelock
cargo test  -p choice-admin-timelock
```

## See also

- [`../choice_farm_factory/`](../choice_farm_factory/) — primary controlled
  contract; owns and migrates each spawned farm.
- [`../choice_farm/`](../choice_farm/) — every spawned farm's wasm admin
  is this timelock.
- [`../../deploy/guide.md`](../../deploy/guide.md) — operator-facing flow
  for the timelock → factory → farms deploy.
- [`../../deploy/upload_admin_timelock.sh`](../../deploy/upload_admin_timelock.sh)
  and
  [`../../deploy/instantiate_admin_timelock.sh`](../../deploy/instantiate_admin_timelock.sh).

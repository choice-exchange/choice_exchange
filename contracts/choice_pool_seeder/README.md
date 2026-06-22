# `choice_pool_seeder`

Generic per-launch liquidity-bootstrap **factory + sink** for legacy XYK
Choice pools. The second of two reusable Choice code-ids that back the
token-launchpad graduation on-ramp; the first is
[`choice_mts_issuer`](../choice_mts_issuer/). Both are dApp-agnostic
infrastructure — a launchpad is the first consumer, not the owner.

## Topology — single code-id, role-via-instantiate

This crate compiles to **one** WASM artifact. The role is fixed at
instantiate via [`InstantiateMsg::Factory`](src/msg.rs) or
[`InstantiateMsg::Sink`](src/msg.rs). Every handler in
[`contract::execute`](src/contract.rs) dispatches off the `ROLE` storage
key and rejects cross-role calls with `ContractError::WrongRole`.

| Instance | Spawned by | Purpose |
|---|---|---|
| **Factory** | The consumer dApp, once. Pinned to the target DEX: `choice_factory` (XYK) and/or `clmm_factory` + `clmm_manager` (CLMM). | Receives `CreateSink { salt, sink_init }` from `choice_mts_issuer.RegisterLaunch`. Spawns the sink at `instantiate2(this_factory, sink_code_id, salt)`. Carries no funds. |
| **Sink** | The factory, once per launch via Instantiate2. Immutable post-instantiate (no admin). | Holds the launch + pair denoms between Leg B/C arrival and `Settle`. Single-shot: terminates as `Settled` or `Refunded`. |

The factory's `sink_code_id` is typically equal to its own code-id (the
single-binary deploy). It's kept as a mutable config field so an
audit-rotated v2 sink build can be swapped in by the admin without
rebuilding the factory.

## Lifecycle

```text
                                          [issuer]                   [keeper /          [anyone]
                                        RegisterLaunch                 forwarder]        Settle{}
                                              │                            │                │
        ┌─────────┐  CreateSink {salt, init} ┌─────────┐                    │                │
        │ ISSUER  │────────────────────────▶ │ FACTORY │── Instantiate2 ──▶ ┌──────┐         │
        └─────────┘                          └─────────┘                    │ SINK │         │
              │                                                             └──────┘         │
              │  Leg B: BankMsg::Send (cw_held)              ┌──────────────────┘            │
              └────────────────────────────────────────────▶ │                               │
                                                             │                               │
                                Leg C (EVM-side):            │                               │
                                pair-asset → forwarder ─────▶│                               │
                                                             │                               │
                                            (token_denom + pair_denom now in sink) ──────────┘
                                                             │
                                                             ▼ atomic chain
                                          ┌─── factory.CreatePair (with creation fee)
                                          ├─── self.Callback::ProvideLiquidity
                                          │      └─ pair.ProvideLiquidity { full deposits }
                                          └─── self.Callback::DistributeLp
                                                 └─ BankMsg::Burn OR BankMsg::Send (LP)
```

## Salt convention

Per the design doc:

```text
salt = encode(issuer_addr, internal_id)
```

The factory itself does NOT interpret the salt — it just hands it to
`WasmMsg::Instantiate2`. The convention is chosen by `choice_mts_issuer`,
where the off-chain caller of `RegisterLaunch` precomputes the sink
address with `instantiate2_address(checksum, factory_addr, salt)` and
passes it into `RegisterLaunch.seeder_addr` so the issuer can ship Leg B
to a known address.

## `Settle` prerequisites (caller-enforced)

`Settle` is permissionless and takes **no tip** — the entire seed balance
goes into the pool. What the caller must get right depends on the venue:

1. **(XYK only) both denoms pre-registered** on the target `choice_factory`
   via `AddNativeTokenDecimals`. For the launch denom, `choice_mts_issuer`
   handles this in its own `RegisterLaunch` flow when called with
   `choice_factory: Some(...)` — the issuer is the denom owner and the
   only entity authorized to sign that registration. For the pair denom
   (the pair asset, e.g. INJ) this is the consumer dApp's responsibility;
   usually a no-op because the pair denom is already registered from existing
   pools. (CLMM pools don't go through `AddNativeTokenDecimals`.)
2. **The create-pair fee in `info.funds` on the `Settle` tx itself** — NOT
   pre-funded into the sink. The amount is venue-specific:
   - **XYK** — attach **exactly** the live tokenfactory denom-creation fee
     (the chain debits it to mint the `factory/<pair>/lp` denom). `Settle`
     queries the live fee via `query_token_factory_denom_create_fee` and
     rejects anything that isn't an exact match (denom set + amounts) — no
     over-pay refund path, no chance of folding the fee into the pool. If
     the chain fee changed since the keeper's preflight quote, just retry
     with the new value.
   - **CLMM** — attach **nothing**. CLMM pool creation is free, and `Settle`
     rejects any attached funds (`UnexpectedFundsForClmmSettle`) so a stray
     coin can't be silently folded into the deposit.

If a prerequisite is missing, `Settle` fails atomically (no partial state
mutation).

## `Refund`

Permissionless after `instantiated_at + deadline_seconds`. Routes the
sink's bank balance:

- `token_denom` → `issuer` (which has `RefundFailedLaunch` to burn it
  cleanly, leaving no zombie supply on the CW side).
- `pair_denom` → `refund_receiver` (the consumer dApp's EVM-side refund
  authority, which typically runs proportional refunds back to curve
  participants).

If both balances are zero, `Refund` errors with `NothingToRefund` — no-op
on already-drained sinks.

## `lp_destination`

Immutable per-sink. v1 set:

- `Burn` — `BankMsg::Burn` the freshly-minted LP. Once burned, the pool's
  initial liquidity is permanently unwithdrawable. The common v1 default.
- `SendTo(Addr)` — `BankMsg::Send` LP to an address. Useful for
  treasuries, future locker contracts, or LP-incentive farms.

`Lock { until, beneficiary }` is **deferred to v2** (design §10 item 12).

## Migration

`MigrateMsg::FromV1 {}` / `MigrateMsg::Patch {}` are the standard
two-variant migration shape — same discipline as
[`choice_zap_lp`](../choice_zap_lp/) and
[`choice_mts_issuer`](../choice_mts_issuer/), so a v2 → v2 patch
deployment can't smuggle a `choice_factory` rewrite past
`MsgMigrateContract`. No v1 → v2 schema delta exists yet; the variant is
wired now so callers can compile against a stable migrate-msg shape.

## Build

```bash
make build-pool-seeder        # from choice_exchange/
# or
./build_release.sh            # workspace optimiser; produces all artifacts
```

Output: `artifacts/choice_pool_seeder.wasm`.

## Test

```bash
cargo test -p choice-pool-seeder --lib              # unit (34 tests)
cargo test -p choice-pool-seeder --test integration # integration; needs WASM artifact
```

Unit tests use `choice::mock_querier` to populate bank balances + the
tokenfactory create-fee handler, and exercise the full message chain
emitted by `Settle` (CreatePair + ProvideLiquidity callback +
DistributeLp callback) end-to-end against mocked storage. The
post-`CreatePair` `factory.Pair { asset_infos }` lookup that runs
on-chain between the callbacks is exercised by directly invoking the
callback with a pre-populated mock factory, since the chain-only message
ordering can't be observed from unit tests.

Integration tests (`tests/integration.rs`, `injective_test_tube`) cover
`Role` / `FactoryConfig` query round-trips and admin rotation, plus two full
on-chain lifecycles against real DEX stacks:

- **XYK** — `CreateSink` → fund → permissionless `Settle` → `choice_factory`
  creates the pair, liquidity is provided, the LP is burned, and the seed
  balances are fully drained.
- **CLMM** — `CreateLocker` + `CreateSink` → fund → `Settle` → the CLMM
  factory creates the pool at the seed ratio, a full-range position NFT is
  minted to the locker, dust is swept, and `locker.CollectFees` routes swap
  fees to the beneficiary.

Both need `make build-all` artifacts (the legacy + CLMM stack wasm). The
launchpad-side E2E (phase-3 step 11) additionally exercises the round trip
end-to-end with a real keeper.

## Consumer integration

Any EVM dApp that has already deployed a [`choice_mts_issuer`](../choice_mts_issuer/)
instance becomes a consumer by:

1. Deploying its own `choice_pool_seeder` factory instance against the
   same code-id, pinned to the target DEX deployment — `choice_factory`
   (XYK) and/or `clmm_factory` + `clmm_manager` (CLMM; both or neither).
   The factory addr is then wired into the issuer's
   `RegisterLaunch.seeder_factory` field per launch.
2. Pre-computing each per-launch sink address off-chain via
   `instantiate2_address(checksum_of_seeder_code, factory_addr,
   salt=encode(issuer_addr, internal_id))`. The result feeds the
   issuer's `RegisterLaunch.seeder_addr`.
3. Constructing each `SinkInit` with `choice_factory` set to the
   factory's pinned value (the factory rejects mismatches at
   `CreateSink` time).
4. Cranking `Settle` with the correct `info.funds`: for an **XYK** sink,
   attach exactly the live tokenfactory create-pair fee; for a **CLMM**
   sink, attach nothing. The fee rides on the `Settle` tx — the sink is
   never pre-funded with it.

Genericity is validated on-chain by
`tests/integration.rs::second_consumer_sendto_lp_and_committed_seed_xyk`
— a second consumer with a different economic model (treasury-owned LP via
`SendTo` + committed seed amounts) driven through a full XYK graduation.

The full cross-contract walkthrough (3-leg value flow, lifecycle, keeper
duties, constraints) lives in
[`docs/launchpad_integration.md`](../../docs/launchpad_integration.md).

# `choice_pool_seeder`

Generic per-launch liquidity-bootstrap **factory + sink** for legacy XYK
Choice pools. The second of two reusable Choice code-ids that ship under
the SHROOM launchpad's rev-3 design ([`trippy_inj/shroom_launchpad/design_brainstorm.md`
§5](../../../trippy_inj/shroom_launchpad/design_brainstorm.md)); the first
is [`choice_mts_issuer`](../choice_mts_issuer/). Both are dApp-agnostic —
SHROOM is the first consumer, not the owner.

## Topology — single code-id, role-via-instantiate

This crate compiles to **one** WASM artifact. The role is fixed at
instantiate via [`InstantiateMsg::Factory`](src/msg.rs) or
[`InstantiateMsg::Sink`](src/msg.rs). Every handler in
[`contract::execute`](src/contract.rs) dispatches off the `ROLE` storage
key and rejects cross-role calls with `ContractError::WrongRole`.

| Instance | Spawned by | Purpose |
|---|---|---|
| **Factory** | The consumer dApp, once. Pinned to a `choice_factory` address + a `max_tip_bps`. | Receives `CreateSink { salt, sink_init }` from `choice_mts_issuer.RegisterLaunch`. Spawns the sink at `instantiate2(this_factory, sink_code_id, salt)`. Carries no funds. |
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
                                          ┌─── tip → caller (pair_denom * tip_bps)
                                          ├─── factory.CreatePair (with creation fee)
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

`Settle` is permissionless and tip-incentivized, but the caller MUST
ensure two things before it can succeed:

1. **Both denoms are pre-registered** on the target `choice_factory` via
   `AddNativeTokenDecimals`. For the launch denom, `choice_mts_issuer`
   handles this in its own `RegisterLaunch` flow when called with
   `choice_factory: Some(...)` — the issuer is the denom owner and the
   only entity authorized to sign that registration. For the pair denom
   (SHROOM, INJ, …) this is the consumer dApp's responsibility; usually a
   no-op because the pair denom is already registered from existing pools.
2. **Attach the tokenfactory create-pair fee in `info.funds`** (currently
   0.1 INJ on Injective mainnet). `Settle` queries the live fee from
   `query_token_factory_denom_create_fee`, validates the attached funds
   cover it, forwards it as `funds:` on the `factory.CreatePair` exec, and
   refunds any over-payment back to the caller at the end of the message
   chain. No pre-funding is needed; the sink stays free of stranded INJ
   regardless of fee changes between the keeper's off-chain quote and
   on-chain execution.

If either prerequisite is missing, `Settle` fails atomically (no partial
state mutation).

## `Refund`

Permissionless after `instantiated_at + deadline_seconds`. Routes the
sink's bank balance:

- `token_denom` → `issuer` (which has `RefundFailedLaunch` to burn it
  cleanly, leaving no zombie supply on the CW side).
- `pair_denom` → `refund_receiver` (the consumer dApp's EVM-side refund
  authority; for SHROOM, `LaunchpadCore` runs proportional refunds to
  curve participants).

If both balances are zero, `Refund` errors with `NothingToRefund` — no-op
on already-drained sinks.

## `lp_destination`

Immutable per-sink. v1 set:

- `Burn` — `BankMsg::Burn` the freshly-minted LP. Once burned, the pool's
  initial liquidity is permanently unwithdrawable. SHROOM's v1 default.
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
emitted by `Settle` (tip + CreatePair + ProvideLiquidity callback +
DistributeLp callback) end-to-end against mocked storage. The
post-`CreatePair` `factory.Pair { asset_infos }` lookup that runs
on-chain between the callbacks is exercised by directly invoking the
callback with a pre-populated mock factory, since the chain-only message
ordering can't be observed from unit tests.

Integration tests cover `Role` / `FactoryConfig` query round-trips and
admin rotation in `injective_test_tube`. The full `CreateSink` → `Settle`
lifecycle is `#[ignore]`'d until a wired `choice_factory` deployment fits
inside one integration test file; in the meantime the launchpad-side
E2E (phase-3 step 11) exercises that round trip with a real deployment.

## Consumer integration

Any EVM dApp that has already deployed a [`choice_mts_issuer`](../choice_mts_issuer/)
instance becomes a consumer by:

1. Deploying its own `choice_pool_seeder` factory instance against the
   same code-id, pointing at the desired `choice_factory` deployment
   with a `max_tip_bps` of choice. The factory addr is then wired into
   the issuer's `RegisterLaunch.seeder_factory` field per launch.
2. Pre-computing each per-launch sink address off-chain via
   `instantiate2_address(checksum_of_seeder_code, factory_addr,
   salt=encode(issuer_addr, internal_id))`. The result feeds the
   issuer's `RegisterLaunch.seeder_addr`.
3. Constructing each `SinkInit` with `choice_factory` set to the
   factory's pinned value (the factory rejects mismatches at
   `CreateSink` time).
4. Funding the sink with INJ for the create-pair fee before any keeper
   calls `Settle` — typically batched with the Leg C pair-asset forward.

The SHROOM launchpad is the first consumer; a second toy consumer for
genericity validation is on the phase-3 to-do list (design §11 step 11).

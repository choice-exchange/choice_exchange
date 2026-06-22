# Launchpad integration guide — `choice_mts_issuer` + `choice_pool_seeder`

This is the cross-contract guide for building a **token launchpad** on top of
two reusable Choice CosmWasm code-ids:

- [`choice_mts_issuer`](../contracts/choice_mts_issuer/) — mints the launch
  token (a tokenfactory denom auto-paired to an EVM ERC20), holds the
  graduation reserve, and routes it to the per-launch sink at graduation.
- [`choice_pool_seeder`](../contracts/choice_pool_seeder/) — a single code-id
  with three roles (Factory / Sink / Locker) that turns the launch + pair
  reserves into a seeded Choice pool (XYK or CLMM) with optional permanently
  locked liquidity.

**Both are dApp-agnostic infrastructure.** They contain no launchpad-specific
addresses or logic — a launchpad is just the first *consumer*, not the owner. A
second consumer just deploys its own instances and points them at its own launch
controller. The issuer even keys launches by `(evm_authority, internal_id)`
specifically so **one issuer instance can serve several consumers** at once.

For the wire-level message/field reference, generate typed clients from the
checked-in JSON Schemas (`contracts/<crate>/schema/*.json`, via
`@cosmwasm/ts-codegen`) — this guide covers the *protocol* between the pieces,
not every field.

---

## 1. Actors

| Actor | VM | Who supplies it | Role |
|---|---|---|---|
| **Issuer** (`choice_mts_issuer`) | CW | you (one instance) | Creates the launch denom + paired ERC20, mints supply, holds the graduation reserve (`cw_held`), delivers it to the sink. |
| **Seeder factory** (`choice_pool_seeder::Factory`) | CW | you (one instance) | Spawns a per-launch **Sink** (and, for CLMM, a **Locker**) at deterministic `instantiate2` addresses. Pinned to your target DEX. |
| **Sink** (`choice_pool_seeder::Sink`) | CW | factory, per launch | Holds the launch + pair denoms, then `Settle`s them into a pool. Single-shot: `Settled` or `Refunded`. |
| **Locker** (`choice_pool_seeder::Locker`) | CW | factory, per CLMM launch | Holds the CLMM position NFT forever; only collects + splits fees. |
| **Authority contract** | EVM (or any) | **you** | Owns the launch lifecycle (the sale/curve), receives `evm_supply`, forwards the raised pair-asset at graduation, and is the address `leftover` is burned from. |
| **Keeper** | off-chain | **you** | The only actor the issuer trusts. Translates your launch lifecycle into the three CW calls; computes `instantiate2` addresses; cranks `Settle`. |
| **Forwarder** | bech32 hot key | **you** | Receives pair-asset from the EVM bank precompile and forwards it to the sink (Leg C). Holds ~1–2 blocks of in-flight value. |

The issuer/seeder **never observe EVM events**. Everything the EVM side "tells"
the contracts goes through keeper-relayed CW messages. The authority contract +
keeper are entirely your domain: a typical implementation is an EVM launch/curve
contract that emits a "launch created" and a "bootstrap-ready" event, plus an
off-chain keeper that maps those onto the issuer/seeder calls in §4.

---

## 2. The three-leg value flow

A launch token's `total_supply` is split into two halves at mint, and the pool
is seeded from two legs that converge on the sink:

```text
  RegisterLaunch (Leg A — mint):
    issuer mints total_supply of factory/<issuer>/<prefix>_<id>
      ├── evm_supply  ──▶ authority contract   (the sale / bonding curve runs here)
      └── cw_held     ──▶ retained by issuer    (the graduation reserve)

  Graduation:
    Leg B (CW):   issuer ── BankMsg::Send(cw_held) ──▶ SINK     (at DeliverToSeeder)
    Leg C (EVM):  authority ── bank precompile ──▶ forwarder ── send ──▶ SINK

    once both denoms are in the sink:
      keeper ── Settle{} ──▶ sink creates the pool at the committed ratio
```

- **Leg A** happens atomically inside `RegisterLaunch`.
- **Leg B** is the issuer shipping the retained reserve to the sink at
  `DeliverToSeeder`.
- **Leg C** is your EVM side moving the raised pair-asset to the sink. Because a
  CosmWasm contract can't sign an EVM tx, the pair-asset is bank-precompiled to
  the **forwarder** bech32, which forwards it to the sink's address.
- `Settle` seeds **exactly** the committed `expected_token` / `expected_pair`
  amounts (see §5) and sweeps any surplus, so the opening pool price is pinned
  regardless of donations or rounding.

---

## 3. Lifecycle state machines

**Issuer** (`LaunchStatus`, keyed by `(evm_authority, internal_id)`):

```text
  RegisterLaunch          DeliverToSeeder
  ───────────────▶ Registered ───────────────▶ Delivered        (happy path)
       (keeper)        │          (keeper)
                       │ RefundFailedLaunch
                       └───────────────────────▶ Refunded        (failure path)
                          (keeper; or admin after refund_deadline_seconds)
```

`RenounceDenomAdmin` is the post-`Delivered` cleanup that drops the issuer's
tokenfactory admin over the denom (so it can no longer mint/admin-burn).

**Sink** (`SinkStatus`):

```text
  CreateSink            Settle{}
  ──────────▶ Pending ───────────▶ Settled                       (happy path)
                 │
                 │ Refund{}  (permissionless after deadline_seconds)
                 │ ForceRefund{}  (factory admin, any time)
                 └─────────▶ Refunded                            (failure path)
```

---

## 4. What a consumer must do — step by step

### 4.1 One-time deploy

1. **Build the optimized wasm** (`./build_release.sh` — the Docker
   workspace-optimizer, not a raw `cargo build`) and store both code-ids.
2. **Instantiate an issuer** (`InstantiateMsg`): `admin` (timelock/multisig),
   `keeper`, `forwarder`, `subdenom_prefix` (≤ 12 chars — see §6), `decimals`
   (18), `refund_deadline_seconds`.
3. **Instantiate a seeder factory** (`InstantiateMsg::Factory`): `admin`,
   `sink_code_id` (usually the seeder's own code-id), and your target DEX —
   `choice_factory` (XYK) and/or `clmm_factory` + `clmm_manager` (CLMM; both or
   neither). These are immutable per-instance so the factory audits as "targets
   this DEX, full stop."
4. **Deploy your authority contract** and **stand up your keeper** (§4.3).

### 4.2 Per launch — `RegisterLaunch` (keeper-gated)

The keeper computes, off-chain, then submits one atomic `RegisterLaunch`:

1. Pick the launch's `internal_id` (unique per `evm_authority`).
2. Derive the **sink address**:
   `instantiate2_address(seeder_code_checksum, factory_addr, salt)` where
   `salt = encode(issuer_addr, internal_id)`. → `seeder_addr`.
3. (CLMM) derive the **locker address** the same way with a launch-specific
   salt, and pre-build a `CreateLocker { salt, locker_init }` — `locker_init`
   carries `creator` + `creator_fee_share_bps` (mirror your curve's fee split).
4. Build `sink_init` (`SinkInit`): denoms, decimals, `pool_kind`
   (`Xyk{...}` or `Clmm{ position_recipient = locker_addr, ... }`),
   `deadline_seconds`, the committed `expected_token` / `expected_pair` (§5),
   and `refund_receiver` — which **must equal `evm_authority`** (`RegisterLaunch`
   rejects a mismatch with `RefundReceiverMismatch`; see §6.7). The authority
   contract is the failure-path refund recipient.
5. Serialize `create_sink_payload = to_json_binary(CreateSink { salt, sink_init })`.
   It is **opaque to the issuer** — the whole sink config surface stays inside
   the seeder code-id.
6. Submit `RegisterLaunch { internal_id, evm_authority, total_supply,
   evm_supply, pair_denom, seeder_factory, seeder_addr, create_sink_payload,
   choice_factory: Some(...) for XYK, salt_suffix, clmm_pool_auth }`.

`RegisterLaunch` is one atomic tx: it creates the denom, mints, ships
`evm_supply` to `evm_authority`, auto-deploys the paired ERC20
(`MsgCreateTokenPair`), forwards `create_sink_payload` to your factory (which
`instantiate2`s the sink), and — for CLMM — reserves the pool slot. Any failure
reverts the whole launch. With `verify_seeder_derivation` on (the default), the
issuer re-derives `seeder_addr` on-chain and rejects a mismatch, so a
compromised keeper can't point the reserve at a look-alike sink.

### 4.3 Graduation & failure — the keeper's job

| Your launch transitions… | Keeper calls… | Effect |
|---|---|---|
| created | `issuer.RegisterLaunch` | §4.2 |
| bootstrap-ready (curve filled, `leftover` known) | `issuer.DeliverToSeeder { evm_authority, internal_id, leftover }` | Burns `leftover` from `evm_authority`; ships `cw_held` (Leg B) to the sink. |
| both legs in the sink | `sink.Settle {}` | Creates + seeds the pool. **Funds: XYK → attach exactly the live create-pair fee; CLMM → attach nothing** (see [seeder README](../contracts/choice_pool_seeder/README.md#settle-prerequisites-caller-enforced)). |
| delivered, optional cleanup | `issuer.RenounceDenomAdmin` | Drops the issuer's tokenfactory admin. |
| failed / stuck | `issuer.RefundFailedLaunch` + `sink.Refund` (or `ForceRefund`) | Burns `cw_held`, routes pair-asset to `refund_receiver`. |

Before `Settle`, the keeper must ensure **both denoms are registered on the XYK
`choice_factory`** (`AddNativeTokenDecimals`) — the issuer handles the launch
denom when `RegisterLaunch` is called with `choice_factory: Some(...)`; the pair
denom is usually already registered. (CLMM skips this.)

---

## 5. Committed-amount seeding (price-pinning)

`SinkInit.expected_token` / `expected_pair` are the **exact** amounts `Settle`
will seed, at the exact ratio — computed by the keeper from the launch's
deterministic graduation amounts. `Settle` seeds these and sweeps any surplus
to the refund/issuer legs. This:

- pins the opening price against a "donate to the sink, then settle" reprice
  attack, and
- rejects a premature `Settle` on a still-partially-funded sink.

Both must be set together. Omitting them (the debug-only path) falls back to
seeding the live balance — **don't ship that to production.**

---

## 6. The contract you're signing up for (deliberate couplings)

These aren't bugs — they're the infrastructure's opinions. Document them for
your users:

1. **Injective MultiVM token model is mandatory.** Every launch token is an
   issuer-owned tokenfactory denom `factory/<issuer>/<prefix>_<id>` auto-paired
   to a chain-deployed `MintBurnBankERC20` (`MsgCreateTokenPair`,
   `allow_admin_burn=true`). You cannot bring a pre-existing token or a pure
   CW20.
2. **18 decimals only** in v1 (`UpdateDecimals` can retune the default for
   future launches within `0..=18`, but mainnet sizing assumes 18).
3. **Subdenom prefix ≤ 12 chars** (`MAX_SUBDENOM_PREFIX_LEN`) — leaves room for
   a `u64` id suffix inside the 44-char tokenfactory subdenom cap.
4. **Graduation targets Choice AMMs only** — XYK `choice_factory` or CLMM
   `clmm_factory` + `clmm_manager`. Not Astroport/Helix/etc.
5. **`RegisterLaunch` is keeper-gated**, and the failure path is never fully
   permissionless (even post-deadline only the admin joins the keeper) — a
   wide-open refund would let anyone terminally refund a slow-but-valid launch.
6. **You run the off-chain layer.** The keeper and authority contract are
   yours; the contracts only define the CW message contract above. There is no
   shared keeper service — each consumer operates its own.
7. **`refund_receiver` is pinned to `evm_authority`.** `RegisterLaunch` rejects
   any launch whose `sink_init.refund_receiver != evm_authority`
   (`RefundReceiverMismatch`) — otherwise a compromised keeper could route
   failure-path pair-asset to an attacker. So your authority contract must be
   able to receive and redistribute the refunded pair-asset. (Driving the
   seeder's `CreateSink` directly, bypassing the issuer, leaves it
   unconstrained — the invariant is issuer-level.)

---

## 7. Security defaults worth keeping

- `verify_seeder_derivation = true` (issuer) — on-chain `instantiate2`
  re-derivation of the sink address. Only an admin escape-hatch can disable it.
- Committed `expected_*` amounts on every production sink (§5).
- `admin` = timelock/multisig on **both** the issuer and the seeder factory;
  two-step rotation (`UpdateAdmin` → `AcceptAdmin`) on both.
- `SetPaused` circuit breakers on both — pausing blocks new launches/sinks while
  letting in-flight ones complete or wind down.

---

## 8. Worked example

The end-to-end flow described in §4 — `RegisterLaunch` → `DeliverToSeeder` →
`Settle`, plus the failure/refund path — is exercised against a real on-chain
DEX stack in the integration suites:

- `contracts/choice_mts_issuer/tests/integration.rs`
- `contracts/choice_pool_seeder/tests/integration.rs`

These also prove the cross-contract genericity: `choice_pool_seeder`'s
`second_consumer_sendto_lp_and_committed_seed_xyk` drives a second consumer with
a different economic model (treasury-owned LP) through a full XYK graduation,
confirming the contracts carry no consumer-specific assumptions. Read them as the
canonical reference for a consumer integration.

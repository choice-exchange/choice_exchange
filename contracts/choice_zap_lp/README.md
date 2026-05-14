# choice_zap_lp

A CosmWasm contract for zapping a single asset (native coin **or** CW20) into
the legacy XYK LP (`factory/{pair}/lp`) of a `choice_pair` pool. One contract
code id serves two distinct use cases:

1. **User zap** ([`Zap`](#zap) / [`Receive`](#receive)) — permissionless,
   multi-pair. The UI passes `pair` per-call. The contract handles the
   optimal-split, swap, LP provision, and forwards everything (LP + dust)
   back to the caller. Works against any `choice_pair` pool.
2. **Royalty zap** ([`ZapBalance`](#zapbalance)) — keeper-managed,
   single-pair. The contract's `(input, pair)` route is **immutable, set at
   instantiate**. The contract address receives royalty payouts via
   `MsgSend` (native) or `cw20::Transfer` (CW20); an allowlisted keeper
   periodically pokes `ZapBalance {}`. LP + dust go to the configured
   `default_recipient`.

Both paths share the same atomic three-message chain and the same closed-form
optimal-split math.

> **v2.0.0 — one route per contract instantiation.** Prior versions held an
> owner-managed map of input → pair routes. As of v2, every contract instance
> is dedicated to one royalty stream. Deploy N contracts to serve N streams.
> See [Migration](#migration-from-v1) below.

## Architecture

A `Zap` (or `ZapBalance`) call emits three top-level messages, executed in
order in the same transaction:

1. `pair.Swap` — sells the optimal-split portion of A for B.
2. `Callback::ProvideLiquidity` — deposits the freshly-minted A + B deltas
   into the pair with `receiver = self`.
3. `Callback::Sweep` — forwards the minted LP + any rounding-dust A/B to
   `recipient`, asserting `min_lp_out` if set.

The optimal-split formula uses the closed-form derivation against the pair's
0.3% commission rate, so the post-swap A and B balances match the pool's
ratio modulo 1–2 wei of integer rounding. A defensive 1-wei haircut in
`Callback::ProvideLiquidity` guards against the pair's `desired > deposit`
underflow on the non-limiting side.

### Snapshot mechanism

The callbacks operate on **deltas** (`current_balance − pre_*`), not raw
balances. `Zap` and `Receive` snapshot the contract's A/B/LP balances at
entry; the callbacks then only forward what *this call* produced.
Pre-existing balances (queued royalties, dust from an earlier keeper run,
etc.) are untouchable by the user zap. `ZapBalance` passes zero snapshots
— that path is drain mode and forwards everything.

This is what lets `Zap` be safely permissionless while `ZapBalance` stays
keeper-only.

## Roles

| Role | Powers |
|---|---|
| **Owner** | `UpdateConfig`, `AddKeeper`/`RemoveKeeper`, `Sweep` (rescue). Implicitly allowed to call `ZapBalance`. **Cannot** change `pair` or `input` — those are immutable post-instantiate. |
| **Keeper** | `ZapBalance` only. A compromised keeper key cannot redirect funds (route is in immutable Config), change slippage caps, or otherwise alter contract behavior. Worst case: fires the zap at a bad moment. |
| **Recipient** | `default_recipient` for `ZapBalance`; `info.sender` (or per-call override) for `Zap`. Receives LP + dust. |

## Messages

### Instantiate

```jsonc
{
  "owner": "inj1...",                       // optional, defaults to instantiator
  "default_recipient": "inj1...",           // optional; ZapBalance errors until set
  "tip_bps": 25,                            // optional, default 0, hard-capped at 100 (1%)
  "min_zap_amount": "1000000",              // optional, default 0
  "input": { "native_token": { "denom": "inj" } },   // immutable royalty input
  "pair":  "inj1...<choice_pair address>"            // immutable royalty target
}
```

`input` is a `choice::asset::AssetInfo`:

- Native: `{ "native_token": { "denom": "inj" } }`
- CW20:   `{ "token": { "contract_addr": "inj1..." } }`

### Execute

#### Zap

```jsonc
// Permissionless. Caller sends one native coin in info.funds; that denom
// must match one side of `pair`. LP + dust go to `recipient` (defaults to
// info.sender). `pair` is per-call — independent of the royalty route.
{ "zap": {
    "pair": "inj1...",
    "recipient": "inj1...",        // optional, defaults to info.sender
    "max_spread": "0.005",         // optional, default 0.5%
    "slippage_tolerance": "0.01",  // optional, default 1%
    "min_lp_out": "1000",          // optional, asserted on the delta
    "deadline": 1734567890         // optional, unix seconds
} }
```

#### Receive

CW20 entry point. Triggered when a user calls `cw20.Send(zap, amount, msg)`
on the CW20 token contract:

```jsonc
// `msg` payload (base64 in cw20.Send):
{ "zap": {
    "pair": "inj1...",
    "recipient": "inj1...",
    "max_spread": "0.005",
    "slippage_tolerance": "0.01",
    "min_lp_out": "1000",
    "deadline": 1734567890
} }
```

The CW20 contract becomes `info.sender`; the original caller is preserved as
the recipient fallback. Native funds attached to the same call are rejected
(`ReceiveWithFunds`).

#### ZapBalance

```jsonc
// Owner or allowlisted keeper only. Reads the contract's current balance of
// Config.input, pays the caller `tip_bps` as a tip, zaps the remainder
// into Config.pair, and forwards LP + dust to `default_recipient`.
//
// Note: no `input` field — the route is pinned in Config at instantiate.
{ "zap_balance": {
    "max_spread": "0.005",         // optional
    "slippage_tolerance": "0.01",  // optional, default 1%
    "min_lp_out": "1000",          // optional
    "deadline": 1734567890         // optional
} }
```

#### Admin (owner-only)

```jsonc
{ "update_config": {
    "owner": "inj1...",                // optional
    "default_recipient": "inj1...",    // optional; empty string clears
    "tip_bps": 25,                     // optional; capped at 100 (1%)
    "min_zap_amount": "1000000"        // optional; ZapBalance no-ops below this
} }
{ "add_keeper":     { "address": "inj1..." } }
{ "remove_keeper":  { "address": "inj1..." } }
{ "sweep":          { "recipient": "inj1...", "assets": [ { "native_token": { "denom": "inj" } } ] } }
```

`pair` and `input` are **not** mutable. To change them, instantiate a fresh
contract.

### Query

```jsonc
{ "config":       {} }                                                      // ConfigResponse (includes input + pair)
{ "simulate_zap": { "pair": "inj1...", "input": <AssetInfo>, "input_amount": "1000" } }
{ "keepers":      {} }                                                      // KeepersResponse
{ "is_keeper":    { "address": "inj1..." } }                                // IsKeeperResponse
```

### Migrate

v1 → v2 migration takes the new immutable route and re-writes Config:

```jsonc
{
  "input": { "token": { "contract_addr": "inj1..." } },
  "pair":  "inj1..."
}
```

The owner, default_recipient, tip_bps, and min_zap_amount of the v1 Config
are preserved. The v1 `ROUTES` map entries are orphaned (unreachable from v2
code, harmless).

## Royalty pipeline setup

For each `(input_asset, pair)` royalty stream:

1. **Instantiate** with `input` + `pair` baked in, plus `default_recipient
   = treasury`. `ZapBalance` refuses to run until `default_recipient` is
   set.
2. **`AddKeeper`** for each address you'll run the keeper from. Hot key,
   no other authority — funded with a few INJ for gas.
3. **Point royalty payouts at this contract's address**:
   - Native input: plain `MsgSend`.
   - CW20 input: `cw20.Transfer { recipient: zap, amount }` on the CW20
     contract.
4. **Deploy the [`zap_keeper_bot`](../../../choice-zap-keeper/README.md)**
   pointed at this contract address.

Run one zap contract instance + one keeper bot per royalty stream. Per-stream
isolation means a compromised keeper key, a misconfigured `tip_bps`, or a
pair migration affects exactly one stream.

## User-zap (UI) integration

The user-facing path is **independent of the contract's pinned route**. The
UI just needs any v2 zap contract address — it passes `pair` per-call. A
single contract instance can serve UI traffic for every pair on the DEX.

For a native input, build a single `MsgExecuteContract`:

- `sender`: end user
- `contract`: zap contract address
- `funds`: `[Coin(denom, amount)]` — the user's deposit (denom must match
  one side of the chosen `pair`)
- `msg`: `{ "zap": { "pair": "inj1...", "max_spread": "0.005",
  "slippage_tolerance": "0.01" } }`

For a CW20 input, call `cw20.Send` on the CW20 token:

- `sender`: end user
- `contract`: CW20 contract
- `msg`: `{ "send": { "contract": "<zap>", "amount": "<wei>", "msg": "<base64>" } }`
  where the inner base64 decodes to `{ "zap": { "pair": "...", ... } }`

The minted LP plus any dust returns to the user in the same tx. No `recipient`
is needed unless the user wants to LP into someone else's wallet.

## Security model

- `Zap` / `Receive` are permissionless but **snapshot-isolated** — the caller
  can only reach balance deltas this call produced.
- `ZapBalance` is gated on owner ∪ keeper allowlist. It snapshots the
  non-input asset and LP balances at entry, so any pre-existing dust or
  accidental transfer stays untouched (rescuable via `Sweep`). The pair and
  input are in immutable Config. A compromised keeper key cannot redirect
  funds into a malicious pool, change the input asset, or alter the recipient.
- LP is minted to the contract (`receiver = self`), then forwarded in
  `Callback::Sweep`. `min_lp_out` checks against the freshly-minted delta,
  not the recipient's total LP balance.
- `tip_bps` is hard-capped at 100 bps so a slip on `UpdateConfig` cannot
  drain royalties to keepers.
- `max_spread` and `slippage_tolerance` are capped at 50% per call so a UI
  bug, fat-fingered keeper config, or compromised hot key cannot disable
  MEV protection on the swap leg.
- `Callback` requires `info.sender == env.contract.address` — external
  callers cannot invoke it.
- `MsgMigrateContract` cannot rewrite the immutable `(input, pair)` route.
  The migrate entrypoint dispatches by cw2 version: only `FromV1` (against
  a 1.x contract) accepts a route, and `Patch` (against a 2.x contract)
  carries no fields. The wasm-admin can therefore only roll forward to a
  newer v2 code id, never re-pin the route.

## Migration from v1

v1 (1.1.x) held an owner-managed `RegisterRoute` map. v2 removes that map
and pins one immutable route per instance. Either path is supported:

- **Migrate in place.** Call `MsgMigrateContract` with the new code id and
  `MigrateMsg::FromV1 { input, pair }`. The owner / default_recipient /
  tip_bps / min_zap_amount stay; old `ROUTES` entries become orphaned
  storage. Any additional routes that lived on the v1 contract become
  unreachable — instantiate fresh contracts for them. The migrate handler
  rejects this variant on a 2.x contract, so it can only be used once.
- **Instantiate fresh per stream** (recommended). Easier to reason about,
  zero migration risk. The v1 contract can be neutralized by clearing
  `default_recipient` and removing all keepers.

For a v2 → v2 patch (e.g. picking up a bugfix without changing the route),
use `MsgMigrateContract` with `MigrateMsg::Patch {}`.

## Build & test

```bash
# Workspace-relative commands (run from choice_exchange/).
cargo build -p choice-zap-lp
cargo test  -p choice-zap-lp --lib
cargo test --test zap_lp_integration   # requires artifacts/ wasm

# Production wasm artifact (~450K):
RUSTFLAGS="-C link-arg=-s -C target-feature=-bulk-memory" \
  cargo build --release --lib --target wasm32-unknown-unknown -p choice-zap-lp

# Or via the workspace optimizer (single-contract build):
docker run --rm -v "$(pwd)":/code \
  --mount type=volume,source="$(basename "$(pwd)")_cache",target=/code/target \
  --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
  cosmwasm/workspace-optimizer:0.17.0 ./contracts/choice_zap_lp
```

Integration coverage in [`tests/zap_lp_integration.rs`](../../tests/zap_lp_integration.rs)
exercises the three-message chain end-to-end against a real `choice_pair`,
snapshot semantics with a pre-existing balance, and `ZapBalance` round-trips
for both native and CW20 inputs.

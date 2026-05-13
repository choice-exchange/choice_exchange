# choice_zap_lp

A CosmWasm contract for zapping a single native coin into the legacy XYK LP
(`factory/{pair}/lp`) of a `choice_pair` pool. One contract serves two
distinct use cases:

1. **User zap** ([`Zap`](#zap)) — permissionless. UI hooks in with
   `info.funds` and a pair address; the contract handles the optimal-split,
   swap, LP provision, and forwards everything (LP + dust) back to the
   caller.
2. **Royalty zap** ([`ZapBalance`](#zapbalance)) — keeper-managed. The
   contract's address receives NFT royalty payouts via plain `MsgSend`; an
   allowlisted keeper periodically pokes `ZapBalance { input_denom }`. The
   contract resolves the target pair from its owner-managed route map,
   optionally pays the keeper a tip in the input denom, and forwards LP +
   dust to the configured `default_recipient`.

Both paths share the same atomic three-message chain and the same closed-form
optimal-split math.

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
balances. `Zap` snapshots the contract's A/B/LP balances at entry; the
callbacks then only forward what *this call* produced. Pre-existing balances
(queued royalties, dust from an earlier keeper run, etc.) are untouchable by
the user zap. `ZapBalance` passes zero snapshots — that path is drain mode
and forwards everything.

This is what lets `Zap` be safely permissionless while `ZapBalance` stays
keeper-only.

## Roles

| Role | Powers |
|---|---|
| **Owner** | `UpdateConfig`, `RegisterRoute`/`UnregisterRoute`, `AddKeeper`/`RemoveKeeper`, `Sweep` (rescue). Implicitly allowed to call `ZapBalance`. |
| **Keeper** | `ZapBalance` only. A compromised keeper key cannot redirect funds, change routes, or alter slippage — its only authority is to trigger the zap into an owner-set route. |
| **Recipient** | `default_recipient` for `ZapBalance`; `info.sender` (or per-call override) for `Zap`. Receives LP + dust. |

## Messages

### Execute

#### Zap

```jsonc
// Permissionless. Caller sends one native coin in info.funds; that denom
// must match one side of `pair`. LP + dust go to `recipient` (defaults to
// info.sender).
{ "zap": {
    "pair": "inj1...",
    "recipient": "inj1...",        // optional, defaults to info.sender
    "max_spread": "0.005",         // optional, default 0.5%
    "slippage_tolerance": "0.01",  // optional, default 1%
    "min_lp_out": "1000",          // optional, asserted on the delta
    "deadline": 1734567890         // optional, unix seconds
} }
```

#### ZapBalance

```jsonc
// Owner or allowlisted keeper only. Reads the contract's current balance of
// `input_denom`, pays the caller `tip_bps` as a tip, zaps the remainder
// into the registered route, and forwards LP + dust to `default_recipient`.
{ "zap_balance": {
    "input_denom": "inj",
    "max_spread": "0.005",         // optional
    "slippage_tolerance": "0.01",  // optional
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
{ "register_route":   { "input_denom": "inj", "pair": "inj1..." } }
{ "unregister_route": { "input_denom": "inj" } }
{ "add_keeper":       { "address": "inj1..." } }
{ "remove_keeper":    { "address": "inj1..." } }
{ "sweep":            { "recipient": "inj1...", "denoms": ["inj"] } }
```

### Query

```jsonc
{ "config":       {} }                                                  // ConfigResponse
{ "simulate_zap": { "pair": "inj1...", "input_denom": "inj", "input_amount": "1000" } }
{ "route":        { "input_denom": "inj" } }                            // RouteResponse
{ "routes":       {} }                                                  // RoutesResponse
{ "keepers":      {} }                                                  // KeepersResponse
{ "is_keeper":    { "address": "inj1..." } }                            // IsKeeperResponse
```

## Royalty pipeline setup

1. **Instantiate** with `default_recipient = treasury` (or set it later via
   `UpdateConfig`). `ZapBalance` refuses to run until this is set.
2. **`RegisterRoute`** per royalty denom — owner pre-registers
   `(input_denom → pair)`. The keeper cannot redirect.
3. **`AddKeeper`** for each address you'll run the keeper from.
   Hot key, no other authority — funded with a few INJ for gas.
4. **Point NFT royalties at the zap contract address** via plain `MsgSend`.
5. **Deploy [`choice-zap-keeper`](../../../choice-zap-keeper/)** on a host
   you control (pm2 spec at
   [`deploy/choice.config.cjs`](../../../choice-zap-keeper/deploy/choice.config.cjs)).
   Each keeper instance polls one `input_denom` — run one per royalty
   stream.

## User-zap (UI) integration

For the Choice UI: build a single `MsgExecuteContract`:

- `sender`: end user
- `contract`: zap contract address
- `funds`: `[Coin(input_denom, amount)]` — the user's deposit
- `msg`: `{ "zap": { "pair": "inj1...<chosen pair>", "max_spread": "0.005", "slippage_tolerance": "0.01" } }`

The minted LP plus any dust returns to the user in the same tx. No `recipient`
is needed unless the user wants to LP into someone else's wallet.

## Security model

- `Zap` is permissionless but **snapshot-isolated** — the caller can only
  reach balance deltas this call produced.
- `ZapBalance` is gated on owner ∪ keeper allowlist. The pair is resolved
  from owner-controlled storage. A compromised keeper key cannot redirect
  funds into a malicious pool.
- LP is minted to the contract (`receiver = self`), then forwarded in
  `Callback::Sweep`. `min_lp_out` checks against the freshly-minted delta,
  not the recipient's total LP balance.
- `tip_bps` is hard-capped at 100 bps so a slip on `UpdateConfig` cannot
  drain royalties to keepers.
- `Callback` requires `info.sender == env.contract.address` — external
  callers cannot invoke it.

## Build & test

```bash
# Workspace-relative commands (run from choice_exchange/).
cargo build -p choice-zap-lp
cargo test  -p choice-zap-lp --lib

# Production wasm artifact (~485K):
RUSTFLAGS="-C link-arg=-s -C target-feature=-bulk-memory" \
  cargo build --release --lib --target wasm32-unknown-unknown -p choice-zap-lp

# Or via the workspace optimizer:
./build_release.sh
```

## Untested

Integration coverage against a live `choice_pair` via `injective_test_tube`
is not yet in [`tests/integration.rs`](../../tests/integration.rs). Worth
adding before mainnet — particularly:

- the three-message chain end-to-end against a real pair,
- the snapshot semantics with a pre-existing balance in the contract,
- a `ZapBalance` round-trip including the keeper tip.

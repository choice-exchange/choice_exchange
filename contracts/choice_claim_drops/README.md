# choice-claim-drops

Multi-campaign cumulative merkle claim-drop hub. One deployed contract hosts
any number of independent claim campaigns — one-shot community airdrops
(trippytools "Claim Drop"), high-value single drops, and keeper-driven reward
streams (Trippy Terminal trading rewards) — all as the same object with
different lifecycles.

## Two kinds of campaign — the safety model

Publishing a merkle root is the power to direct a campaign's unclaimed float
(the contract can't see leaves, so it trusts the publisher on the tree). The
contract splits campaigns so that power is only ever held where it's safe:

- **One-shot (`streaming: false`)** — permissionless. Auto-frozen on its first
  published root, so its already-funded float can **never** be reassigned by a
  later republish. This is the mode for community airdrops and high-value single
  drops. Create + fund + publish + freeze can happen in a single tx (see
  `initial`), so the claim link is never shareable before the root is immutable.
- **Streaming (`streaming: true`)** — **owner-only.** Accepts repeated
  `UpdateRoot`s and an optional keeper bot, for daily reward epochs. The mutable
  root is a trusted-updater surface: whoever holds the creator or keeper key can
  reassign the campaign's unclaimed float. Keep the keeper key hot-but-poor
  (≤ one epoch's emission) and cap the contract's streaming float accordingly.
  A streaming campaign can be `Freeze`d by the owner to end it.

### Solvency invariant

`UpdateRoot { root, total, leaves_uri }` (and `CreateCampaign { initial }`) must
attach **exactly `total − previous_total`** of the campaign denom, plus the
platform fee on that delta (charged on top, ceil-rounded) if `fee_bps` is set.
`total` can never decrease, and every payout is hard-capped at
`total − claimed_total`, which confines a dishonest tree to its own campaign's
balance — campaigns are fully isolated.

Each funding delta is booked into a per-denom **liabilities** ledger; the
owner-only `Rescue` can sweep only `bank_balance − liabilities`, so tokens
mis-sent to the contract are recoverable while claimant funds never are.

### Dual-root claim window

The previous root stays claimable after an update, so proofs fetched just before
a keeper publish don't race to failure. Safe under cumulative semantics: an old
leaf only ever claims less.

### Freeze / expiry / clawback

`expiry: None` = perpetual. While unexpired, `SetExpiry` may only *extend*;
winding down a perpetual campaign requires ≥ 7 days notice. **A frozen perpetual
campaign's expiry is locked** — it can never be given an expiry, so an
"immutable" drop can never be clawed back. After expiry, claims stop and
`Clawback` returns the unclaimed remainder to the creator (once). Use the
two-step `TransferCreator` to rotate a lost/compromised creator key before it
strands that remainder.

## Merkle tree spec (cross-language)

Any tree builder (TS keeper, trippytools frontend) must reproduce this
byte-for-byte — golden vectors are pinned in `src/merkle.rs` tests:

```text
leaf   = sha256(utf8("{bech32_address}:{cumulative_amount_base_units}"))
parent = sha256(concat(min(a, b), max(a, b)))      // sorted-pair, no L/R flags
```

- Address: canonical lowercase bech32 (`inj1…`), exactly as the chain returns it.
- Amount: decimal string of the lifetime cumulative allocation in base units,
  no separators (`"1000000"`).
- Odd node at any level promotes unchanged (its proof omits that level).
- Single-leaf tree: the leaf is the root; the proof is empty.
- **Deduplicate addresses** before building; set `total == Σ leaf amounts`
  exactly (over-declaring strands funds until clawback; under-declaring makes
  some leaves unclaimable).

Publishers upload the full `(address, amount)` leaves file to `leaves_uri`
(stored on-chain per campaign): clients rebuild the tree locally and derive
their own proofs, and the file doubles as the public audit record.

## Messages

| Message | Who | Notes |
|---|---|---|
| `CreateCampaign { denom, meta, keeper?, expiry?, streaming, initial? }` | anyone (streaming: owner) | `initial` publishes+funds the first root in-tx; one-shots auto-freeze |
| `UpdateRoot { id, root, total, leaves_uri }` | creator / keeper | attach exactly `delta (+ fee)`; one-shot auto-freezes after |
| `Freeze { id }` | creator | one-way; also locks expiry when perpetual |
| `Claim { id, amount, proof }` | claimant | pays `amount − claimed`; current or previous root |
| `ClaimMany { claims, allow_partial? }` | claimant | `allow_partial` skips closed/exhausted claims instead of reverting |
| `Clawback { id }` | creator | after expiry, once |
| `Rescue { denom, amount?, recipient? }` | owner | sweeps only `balance − liabilities` |
| `TransferOwnership`/`AcceptOwnership` | owner → new | two-step |
| `TransferCreator { id }`/`AcceptCreator { id }` | creator → new | two-step; moves the creator index |
| `SetKeeper` (streaming only) / `SetExpiry` / `SetCampaignPaused` / `UpdateMeta` | creator | expiry extend-only; frozen perpetual locked |
| `UpdateConfig { fee_bps?, fee_collector?, paused? }` | owner | fee_bps ≤ 1000; ownership moves via the two-step flow |

Queries: `Config`, `Campaign`, `Campaigns`, `CampaignsByCreator`, `Claimed`,
`Claims { id, start_after, limit }` (paginated claimants for the manage view),
`Claimable { id, address, amount, proof }` (dry-run → validity + payable now),
`FundingRequired { id, new_total }` (exact `delta`/`fee`/`required` to attach),
and `Liabilities { denom }` (owed vs. bank balance reconciliation).

Events: `create_campaign` (id, creator, denom, streaming, keeper, expiry),
`update_root` (id, root, total, delta, fee), `claim` (id, claimant, cumulative,
payout, denom — emitted per-claim from both `Claim` and `ClaimMany`),
`claim_skipped`, `clawback`, `rescue`, plus the admin actions.

## Build & test

```bash
cargo test -p choice-claim-drops
cargo build --release --target wasm32-unknown-unknown -p choice-claim-drops
cd contracts/choice_claim_drops && cargo run --example schema   # run from the contract dir
./build_release.sh                                              # optimized artifact
```

## Deferred (future work)

Root-update timelock for streaming campaigns (URD-style veto window — migrate to
add if a streaming campaign ever holds enough float to justify it; the contract
sits behind `choice_admin_timelock`), claim-on-behalf, batch `Claimable`.

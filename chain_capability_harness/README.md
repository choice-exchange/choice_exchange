# chain_capability_harness

A **workspace-excluded** test crate that runs the *real* Injective chain
(`injective-core v1.19.0`, bundled by `injective-test-tube 1.19.0`) to validate
chain capabilities the fresh CLMM / MTS-issuer / pool-seeder contracts depend
on but that the main workspace can't test directly.

## Why it's separate

`injective-test-tube 1.19` depends on `cosmwasm-std 3` + `injective-cosmwasm
0.3.6`. The main `choice_exchange` workspace is pinned at `cosmwasm-std 2.2.2` /
`injective-cosmwasm 0.3.4-1` (the legacy XYK contracts). Cargo would unify
`injective-cosmwasm 0.3.6` onto the whole workspace and force cw-std 3 onto the
2.2.2 contracts, breaking their compile.

The empty `[workspace]` table in this crate's `Cargo.toml` makes it its own
workspace root, so it has an independent lockfile and never perturbs the legacy
tree. It does **not** depend on any workspace contract crate at compile time —
it drives the chain via raw protobuf messages (and, where extended, via
compiled `.wasm` artifacts loaded from disk), so there's no version coupling.

## What it proves

`tests/erc20_create_token_pair.rs` — minimal capability probe: executes
`injective.erc20.v1beta1.MsgCreateTokenPair` with an empty `erc20_address`
against the bundled chain and asserts it **auto-deploys a `MintBurnBankERC20`**
and persists the pair. Disproves the old assumption that the erc20 module needed
an Injective v1.20+ image; v1.19.0 already ships and executes it.

`tests/issuer_lifecycle.rs` — full `choice_mts_issuer` lifecycle, driving the
**actual compiled wasm** of `choice_mts_issuer` + `choice_pool_seeder` (from
`../artifacts/`, via JSON messages — no contract-crate dep):

  1. instantiate the seeder as a Factory and the issuer.
  2. keeper `RegisterLaunch` (XYK path, `choice_factory: None`): one atomic tx
     that creates the denom, mints `total_supply`, auto-deploys the ERC20,
     forwards `CreateSink` to the seeder (Instantiate2 → real sink), and ships
     `evm_supply` to the EVM authority. Asserts the launch record, bank
     balances, erc20 pair, and that the **sink exists at the address the
     harness derived** via the keeper's Injective instantiate2 algorithm.
  3. keeper `DeliverToSeeder`: bank-sends `cw_held` to the sink (issuer drained
     to 0) and **admin-burns `leftover` from the EVM authority**.

`tests/clmm_graduation.rs` — the CLMM variant of the lifecycle. Same flow, but
the launch is a CLMM graduation: the seeder Factory is configured with
`clmm_factory` + `clmm_manager`, the sink's `pool_kind` is `Clmm`, and
`RegisterLaunch` carries a `clmm_pool_auth` so the issuer emits
`AuthorizeCreation` at a **real deployed `choice_clmm_factory`**. Asserts the
anti-squat reservation landed (`GetCreationAuth.creator == sink`). The CLMM
manager + position recipient are dummy addresses because `Settle` (which needs a
real pool + funded pair leg) isn't exercised — the factory is the only extra
real contract required.

`tests/clmm_settle.rs` — the **complete CLMM graduation including `Settle`**,
driving the whole stack (issuer + seeder + `choice_clmm_factory`/`manager`/`pool`):
RegisterLaunch → DeliverToSeeder → fund the sink's pair leg → permissionless
`Settle` creates the CLMM pool at the seed ratio and mints a full-range position
NFT into a `Locker` → a swap accrues fees → `Locker::CollectFees` routes them to
the beneficiary (locker never holds them). The whole launch→graduation→
locked-liquidity-earns-fees path on a real chain.

`tests/xyk_settle.rs` — the **complete XYK graduation including `Settle`**,
driving issuer + seeder + `choice_factory`/`choice_pair` (+ a native auction as
the factory's burn address): RegisterLaunch (`choice_factory: Some`, so the
issuer registers the launch denom) → DeliverToSeeder → fund the pair leg →
`Settle` runs `CreatePair` + `ProvideLiquidity` and burns the LP. Asserts the
pair reserves equal the seed and the sink is fully drained.

`tests/failure_paths.rs` — the CW-side **failure terminals** a launch hits when
it never graduates:
  * `issuer_refund_failed_launch` — a launch stuck in `Registered` (curve never
    filled / bootstrap aborted). Asserts the deadline gate (non-keeper before
    `refund_deadline_seconds` is rejected), then keeper `RefundFailedLaunch`
    burns `cw_held` (status → Refunded) while `evm_supply` stays untouched
    (EVM-side participant refunds are LaunchpadCore's job), and the state is
    terminal (no later `DeliverToSeeder`).
  * `sink_refund_returns_legs` — a funded sink that never settled. Asserts the
    deadline gate, then (after `increase_time`) permissionless `Refund` routes
    the token side back to the issuer and the pair side to `refund_receiver`,
    draining the sink.

Findings established by the lifecycle runs:
  * The keeper's **instantiate2 derivation is correct** (32-byte hash truncated
    to 20 bytes) — the sink `Role` query at the computed address succeeds.
  * **admin burn-from works on injective-core v1.19.0.** `proto.rs` calls the
    `allow_admin_burn` field "v1.20+"; the burn-from path nonetheless executes
    on the v1.19.0 bundle.
  * The genesis **tokenfactory denom-creation fee is 10 INJ** — `RegisterLaunch`
    requires the keeper attach it exactly; the harness reads it from
    tokenfactory params and forwards it.

## Running

```bash
cd chain_capability_harness
cargo test -- --nocapture
```

First build is slow (2-3 min): test-tube compiles all of `injective-core` into a
c-shared library via cgo. Requires the Go toolchain — the bundled `go.mod`
pins `go 1.26.2`, so build with `GOTOOLCHAIN=auto` (the default) to let Go
auto-fetch it.

## Three gotchas baked into the test (read before extending)

1. **Mint before pairing.** The erc20 module rejects pairing a zero-supply
   bank denom (`unknown bank denom or zero supply`). Create the denom, then
   `MsgMint` non-zero supply, *then* `MsgCreateTokenPair`.
2. **High custom gas.** The empty-`erc20_address` path runs an internal
   `MsgEthereumTx` to deploy the ERC20. Auto fee-simulation under-provisions
   it (`MsgEthereumTx GasLimit is higher than remaining tx GasLimit`). Send the
   pair tx with `FeeSetting::Custom { gas_limit: ~60_000_000, .. }`.
3. **EVM-event UTF-8 decode panic.** `test-tube-inj 2.0.10` panics decoding
   `FinalizeBlock` event attributes whose values are non-UTF-8 — which the EVM
   emits on the deploy. The block is **already committed** when this fires, so
   `catch_unwind` the `execute()` and assert via a follow-up **query** (query
   responses are protobuf, not raw block events, so they're unaffected).

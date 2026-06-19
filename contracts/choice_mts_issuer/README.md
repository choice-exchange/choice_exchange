# `choice_mts_issuer`

Generic MultiVM Token Standard (MTS) issuer + reserve custodian for Injective
dApps that need to launch a paired EVM/CW token and later seed it into a
Choice pool. Per-dApp instantiated against a single Choice-published code-id;
no shared state, no protocol fees.

This is one of two reusable Choice code-ids that ship under the SHROOM
launchpad's rev-3 design ([`trippy_inj/shroom_launchpad/design_brainstorm.md`
§5](../../../trippy_inj/shroom_launchpad/design_brainstorm.md)). The other is
`choice_pool_seeder` (factory + per-launch sink), built separately.

## What it does

`RegisterLaunch` (permissionless, atomic). **Caller must attach the
tokenfactory create-denom fee** (currently 0.1 INJ on Injective mainnet) in
`info.funds`; the issuer queries the chain for the live fee, validates the
attached funds cover it, and refunds any over-payment back to `info.sender`
at the end of the message chain. Dispatch order matters — the chain runs
the messages in this exact sequence:

1. **`MsgCreateDenom`** (Stargate) — creates `factory/<this>/<prefix>_<id>`
   with `allow_admin_burn=true`. This contract is the tokenfactory admin.
   The chain debits the create-denom fee from the issuer's bank balance
   (already pre-credited by the `info.funds` attached on the entry call).
2. **`SetTokenMetadata`** — sets decimals/name/symbol so chain-level
   indexers / explorers see the right metadata. The `choice_factory` keeps
   its own `NATIVE_TOKEN_DECIMALS` map separately — wired in step 5 below.
3. **`Mint`** total_supply to self.
4. **`MsgCreateTokenPair`** (Stargate, SubMsg with `ReplyOn::Success`) —
   pairs the bank denom to an auto-deployed `MintBurnBankERC20` whose owner
   is the lower-20-byte form of this contract's bech32. The reply handler
   captures the ERC20 address into per-launch state.
5. **(Optional) `factory.AddNativeTokenDecimals`** — when `RegisterLaunch`
   is called with `choice_factory: Some(addr)`, the issuer chains a
   `WasmMsg::Execute` to that address with `1` wei of the new denom attached
   as funds. The `choice_factory`'s per-denom verification reads its own
   bank balance, so the dust both satisfies the check and registers the
   denom in one tx. This step is skipped when `choice_factory: None` — the
   consumer dApp then takes responsibility for registering the denom
   out-of-band (only the denom owner — i.e. this contract — can sign that
   tx, so the consumer must route it back through `RegisterLaunch` or a
   future dedicated exec). Setting `choice_factory: Some(...)` requires
   `cw_held >= 1` (`total_supply - evm_supply >= 1`); the dust comes out of
   `cw_held`, so the stored `LaunchRecord.cw_held` is reduced by 1 wei.
6. **`WasmMsg::Execute`** to the consumer dApp's `seeder_factory` —
   forwards an opaque `CreateSink` payload. The sink lives at a
   caller-precomputed `instantiate2` address; the issuer doesn't compute the
   salt itself.
7. **`BankMsg::Send`** `evm_supply` of the new denom to `evm_authority`
   (the consumer dApp's EVM contract bech32). After this the EVM curve / fair
   launch / vault funds itself off the bank ledger via the auto-deployed
   `MintBurnBankERC20`'s `transfer` precompile.
8. **(Conditional) refund** — if `info.funds` exceeded the chain's
   create-denom fee, the excess `BankMsg::Send`s back to `info.sender`.
   Pre-existing issuer balance is untouched (only the over-pay deltas
   refund), so the issuer leaves no stranded INJ in the success case.

`DeliverToSeeder { internal_id, leftover }` (keeper-relayed after the EVM
authority emits `BootstrapReady(internal_id, leftover)`):

* **`MsgBurn`** (Stargate) burning `leftover` from `evm_authority.bech32` —
  works because the denom was created with `allow_admin_burn=true` and no
  permissions namespace, so the issuer (as tokenfactory admin) can burn-from
  any holder. See [`feedback_inj_tokenfactory_admin_burn`] memory; confirmed
  on testnet 2026-05-26.
* **`BankMsg::Send`** `cw_held` of the new denom to the per-launch sink. The
  sink later runs `factory.CreatePair` + `provide_liquidity` against its own
  bank balance once the pair-asset leg also lands (forwarded EVM-side, out
  of scope here).

`RefundFailedLaunch { internal_id, reason }` (keeper before the per-launch
deadline, anyone after) burns `cw_held` from self. EVM-side circulating
supply cleanup is the consumer dApp's job — the issuer doesn't reach into
EVM here.

Admin can rotate `admin`, `keeper`, and `forwarder`. Migration is split
`FromV1 {}` / `Patch {}` so a `MsgMigrateContract` with the v1-shaped
payload can't accidentally rewrite a v2 contract (same discipline as
[`choice_zap_lp`](../choice_zap_lp/)).

## Build

```bash
make build-mts-issuer        # from choice_exchange/
# or
./build_release.sh           # workspace optimiser; produces all artifacts
```

Output: `artifacts/choice_mts_issuer.wasm`.

## Test

```bash
cargo test -p choice-mts-issuer --lib     # unit (23 tests)
cargo test -p choice-mts-issuer --test integration   # integration; needs WASM artifact
```

Integration tests use `injective_test_tube 1.16.3-1`, which bundles a
pre-v1.20 chain image. As a result, the full `RegisterLaunch` lifecycle is
`#[ignore]`d in `tests/integration.rs` — `MsgCreateTokenPair`'s
`/injective.erc20.v1beta1.MsgCreateTokenPair` type-url isn't registered on
the test image. Unit tests (`src/tests.rs`) cover the message-wiring,
reply-decode, and state-transition paths end-to-end with mocked storage.

## Consumer integration

Any EVM dApp can become a consumer by:

1. Deploying its own `choice_mts_issuer` instance (admin/timelock + keeper
   key pair + forwarder bech32).
2. Deploying its own `choice_pool_seeder` factory (per the seeder's docs).
3. Implementing an "authority contract" (EVM or otherwise) that owns the
   launch lifecycle and signals three transitions to its keeper: launch
   created, bootstrap-ready (carrying the unsold `leftover` to burn), and
   failure. *How* it signals is the consumer's choice — the issuer never
   sees EVM events, only the keeper-relayed CW messages below. SHROOM does
   it with `LaunchpadCore.LaunchCreated` + `Phase3Settler.BootstrapReady`
   (see those contracts for the exact, richer signatures); the failure path
   is keeper- or deadline-driven, not a dedicated event.
4. Running a keeper that maps those transitions onto this issuer:
   `RegisterLaunch` (created) → `DeliverToSeeder` (bootstrap-ready) →
   `RenounceDenomAdmin`, with `RefundFailedLaunch` on the failure path.

That is the entire surface the issuer cares about: keeper-relayed CW
messages. The full cross-contract walkthrough (3-leg value flow, lifecycle,
keeper duties, constraints) lives in
[`docs/launchpad_integration.md`](../../docs/launchpad_integration.md).
SHROOM is the first consumer, not the owner.

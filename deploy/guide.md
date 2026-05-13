# Deployment Guide

Scripts in this directory deploy and configure the Choice contracts on
Injective. Network-specific config lives in `network/{testnet,mainnet}.env`;
each script picks one via the `NETWORK` env var (default: `testnet`).

## Prerequisites

- `injectived` CLI installed and configured.
- The signer key in the network env file exists in `injectived keys list`.
- Sufficient INJ in the signer for gas + fees (and 1 INJ + reward amounts if
  testing `CreateFarm`).
- Optimised wasm artifacts in `../artifacts/`. Produce them with
  `./build_release.sh` from the workspace root.

## File overview

| Script | Purpose | Network-aware |
| --- | --- | --- |
| `lib.sh` | Shared bash helpers (`store_contract`, `instantiate_contract`, `wait_for_tx`, `banner`). Sourced by every other script. | n/a |
| `network/testnet.env`, `network/mainnet.env` | Per-network config: node URL, chain id, gas, fees, signer. | n/a |
| `upload_admin_timelock.sh` | Uploads `choice_admin_timelock.wasm`. Prerequisite for the farm flow. | yes |
| `instantiate_admin_timelock.sh` | Instantiates the admin timelock with `owner` (governance multisig) and `timelock_seconds` (48 h in production). Prints the timelock address — feed it to the farm factory as `FARM_OWNER` and use it as the `--admin` on the factory itself. | yes |
| `upload_farm_code.sh` | Uploads `choice_farm.wasm` + `choice_farm_factory.wasm` in one go. Prints both code ids. | yes |
| `instantiate_farm_factory.sh` | Instantiates the farm factory with `farm_code_id`, fee collector, fee amount. | yes |
| `upload_vault_code.sh` | Uploads `choice_vault.wasm`. | yes |
| `deploy_testnet.sh` | Full legacy DEX deploy on testnet (pair, factory, burn manager, CW20 adapter, router). | testnet-only |
| `deploy_mainnet.sh` | Full legacy DEX deploy on mainnet (uses pre-uploaded code ids and the dev multisig). | mainnet-only |
| `deploy_clmm_testnet.sh` | CLMM deploy on testnet (pool + factory + manager). | testnet-only |
| `add_native_token_decimals.sh` | Registers native-token decimals against an already-deployed factory. | testnet-only |
| `add_decimals_mainnet.sh` | Same, mainnet. | mainnet-only |

The dex / clmm / add-decimals scripts encode distinct deployment plans per
network (different signer/admin policies, pre-existing code ids on mainnet,
different token lists). They are kept separate by design; only the
upload + factory-instantiate flow is collapsed to a single network-aware
script.

## Farm factory flow (full sequence)

The factory now enforces (C-1) that its own wasm admin matches
`config.farm_owner` at every `CreateFarm`. Both must be the address of a
`choice_admin_timelock` contract — otherwise `CreateFarm` errors with
`factory admin mismatch` or `factory has no wasm admin set`. Deploy the
admin timelock **first**.

```bash
# 1. Build the wasm artifacts.
./build_release.sh

# 2. Upload + instantiate the admin timelock. Prints TIMELOCK_ADDR.
#    Production: TIMELOCK_SECONDS=172800 (48 h). Owner = governance multisig.
NETWORK=mainnet \
    OWNER="inj1...multisig..." \
    TIMELOCK_SECONDS=172800 \
    ./deploy/upload_admin_timelock.sh
NETWORK=mainnet \
    OWNER="inj1...multisig..." \
    TIMELOCK_SECONDS=172800 \
    TIMELOCK_CODE_ID=<id from upload> \
    ./deploy/instantiate_admin_timelock.sh
# → record TIMELOCK_ADDR from DEPLOY_CAPTURE_TIMELOCK_ADDR=…

# 3. Upload farm + factory wasms. Prints FARM_CODE_ID + FACTORY_CODE_ID.
NETWORK=mainnet ./deploy/upload_farm_code.sh

# 4. Instantiate the factory.
#    - FARM_OWNER must be the timelock address (will be installed as every
#      spawned farm's owner AND wasm admin).
#    - The factory's own wasm admin must ALSO be the timelock — pass it via
#      ADMIN to instantiate_farm_factory.sh. The factory rejects CreateFarm
#      if its own ContractInfo.admin doesn't match FARM_OWNER.
FARM_CODE_ID=<id1> FACTORY_CODE_ID=<id2> \
    NETWORK=mainnet \
    OWNER="inj1...multisig..." \
    FEE_COLLECTOR="inj1...treasury..." \
    FARM_OWNER="$TIMELOCK_ADDR" \
    ADMIN="$TIMELOCK_ADDR" \
    FEE_INJ_BASE="1000000000000000000" \
    ./deploy/instantiate_farm_factory.sh
```

The instantiate script prints the new factory address and the exact env-var
lines the frontend needs (`VITE_FARM_FACTORY_ADDRESS`,
`VITE_FARM_CREATION_FEE_INJ_BASE`).

**Testnet shortcut:** if you don't want to wait 48 h between code-id
proposals on a dev chain, instantiate the timelock with
`TIMELOCK_SECONDS=3600` (1 h, the minimum allowed by `MIN_TIMELOCK_SECONDS`)
and rotate it to 48 h before mainnet.

## Make targets

The same flows are reachable via the workspace Makefile:

```bash
make deploy-help                                              # list deploy targets
make upload-farm                                              # NETWORK=testnet by default
make upload-farm NETWORK=mainnet
make instantiate-farm-factory FARM_CODE_ID=123 FACTORY_CODE_ID=124
make instantiate-farm-factory NETWORK=mainnet \
    FARM_CODE_ID=123 FACTORY_CODE_ID=124 \
    OWNER=inj1...multisig... FEE_COLLECTOR=inj1...treasury...
make deploy-farm                                              # upload + instantiate
make upload-vault
```

## Mainnet safety checklist

- `OWNER` (factory config) is the governance multisig — kept separate
  from the wasm-admin for clean separation: config rotations require a
  multisig sig; code migrations require multisig + 48 h timelock.
- `FARM_OWNER` is the `choice_admin_timelock` address (not the multisig
  directly). Every spawned farm inherits this as its `Config.owner` and
  wasm admin.
- The factory's wasm `admin` is **also** the admin-timelock address (NOT
  `$SIGNER_ADDRESS`). The factory's `CreateFarm` queries its own
  `ContractInfo` and refuses to spawn if `admin != farm_owner`. Verify
  with `injectived query wasm contract <factory-addr>` before the first
  `CreateFarm`.
- The admin-timelock's `timelock_seconds` is `172800` (48 h) on mainnet.
  `instantiate_admin_timelock.sh` rejects anything < `3600` so a
  fat-fingered low value can't slip through.
- `FEE_COLLECTOR` is Choice's treasury (not the deployer key).
- `FEE_INJ_BASE` matches `VITE_FARM_CREATION_FEE_INJ_BASE` in the
  frontend env.
- Post-deploy verification: query `factory.PendingFarmCodeIdUpdate` and
  `farm.PendingConfigUpdate` / `PendingMigration` on a representative
  farm — they should all be empty. Front-end indexers should surface a
  banner if any of these become non-empty (a 48 h heads-up to stakers).

## Legacy DEX deploy (testnet)

```bash
./deploy/deploy_testnet.sh
# → prints factory address; record it.

# Edit add_native_token_decimals.sh's FACTORY_CONTRACT to the new address,
# then:
./deploy/add_native_token_decimals.sh
```

This path predates the farm factory and is unrelated to it — running it does
not affect farm deploys and vice versa.

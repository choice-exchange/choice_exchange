#!/bin/bash
set -euo pipefail

# Phase-3 SHROOM launchpad MAINNET deploy. Stores + instantiates:
#   - choice_mts_issuer   (per-launch MTS denom issuance + cross-VM seed)
#   - choice_pool_seeder  (factory + sink for initial-liquidity seeding)
#
# THE difference from the testnet script (deploy_phase3_testnet.sh) is the
# admin model. On testnet, ADMIN defaults to the signer hot key. On mainnet
# that is a launch blocker: the deployer EOA would hold BOTH
#   (a) the CosmWasm wasm-admin (MsgMigrateContract authority), and
#   (b) the contract-level Config.admin (keeper/forwarder/pause/param control),
# for the issuer AND the seeder factory — a single hot key able to migrate or
# re-point the whole graduation pipeline in one tx.
#
# This script REQUIRES `ADMIN` to be the choice_admin_timelock address and
# wires it as BOTH the wasm-admin (4th arg to instantiate_contract) and the
# Config.admin (init JSON). It refuses to default ADMIN to the signer and warns
# loudly if ADMIN == the signer. Deploy the timelock first:
#   TIMELOCK_CODE_ID=<id> ./deploy/instantiate_admin_timelock.sh
# then pass its address as ADMIN here.
#
# Required env (no defaults — fail closed):
#   ADMIN            choice_admin_timelock bech32 (wasm-admin + Config.admin)
#   KEEPER           keeper relay key bech32
#   FORWARDER        pair-asset forwarder hot key bech32
#   CHOICE_FACTORY   mainnet legacy XYK factory pinned by the seeder
#   CLMM_FACTORY     mainnet CLMM factory (set together with CLMM_MANAGER)
#   CLMM_MANAGER     mainnet CLMM NFT position manager
# Optional:
#   SUBDENOM_PREFIX (default "shroom"), DECIMALS (18),
#   REFUND_DEADLINE_SECONDS (86400)
#
# Output: deploy/results/phase3_mainnet_<timestamp>.json

DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NETWORK="${NETWORK:-mainnet}"
export NETWORK
source "$DEPLOY_DIR/lib.sh"

if [ "$NETWORK" != "mainnet" ]; then
    echo "ERROR: this script is mainnet-only (NETWORK=$NETWORK). Use deploy_phase3_testnet.sh for testnet." >&2
    exit 1
fi

ARTIFACTS_DIR="${ARTIFACTS_DIR:-$DEPLOY_DIR/../artifacts}"
ISSUER_WASM="${ISSUER_WASM:-$ARTIFACTS_DIR/choice_mts_issuer.wasm}"
SEEDER_WASM="${SEEDER_WASM:-$ARTIFACTS_DIR/choice_pool_seeder.wasm}"

# Fail-closed on the governance-critical inputs. NO defaulting to the signer.
: "${ADMIN:?set ADMIN to the choice_admin_timelock address (wasm-admin + Config.admin)}"
: "${KEEPER:?set KEEPER to the keeper relay bech32}"
: "${FORWARDER:?set FORWARDER to the pair-asset forwarder bech32}"
: "${CHOICE_FACTORY:?set CHOICE_FACTORY to the mainnet legacy XYK factory}"
: "${CLMM_FACTORY:?set CLMM_FACTORY to the mainnet CLMM factory}"
: "${CLMM_MANAGER:?set CLMM_MANAGER to the mainnet CLMM manager}"

SUBDENOM_PREFIX="${SUBDENOM_PREFIX:-shroom}"
DECIMALS="${DECIMALS:-18}"
REFUND_DEADLINE_SECONDS="${REFUND_DEADLINE_SECONDS:-86400}"

# Loud guard: the whole point of this script is that ADMIN is NOT a hot EOA.
if [ "$ADMIN" = "$SIGNER_ADDRESS" ]; then
    echo "ERROR: ADMIN ($ADMIN) equals the signer. On mainnet the issuer + seeder admin" >&2
    echo "       (wasm-admin AND Config.admin) MUST be the choice_admin_timelock, not the" >&2
    echo "       deployer hot key. Deploy the timelock and pass its address as ADMIN." >&2
    exit 1
fi

# Best-effort preflight: confirm ADMIN is actually a contract (the timelock),
# not an EOA typo. A non-contract address answers with an error here.
echo "  Verifying ADMIN is a contract (timelock)..."
if injectived query wasm contract "$ADMIN" --node "$NODE" >/dev/null 2>&1; then
    echo "    ok: $ADMIN holds contract code"
else
    echo "WARNING: ADMIN ($ADMIN) does not resolve as a contract. If this is an" >&2
    echo "         intentional multisig EOA, continue; otherwise abort (Ctrl-C)." >&2
    echo "         Sleeping 10s so you can bail..." >&2
    sleep 10
fi

banner "SHROOM Phase-3 CW deploy (MAINNET)"
echo "  issuer wasm:        $ISSUER_WASM"
echo "  seeder wasm:        $SEEDER_WASM"
echo "  admin (timelock):   $ADMIN   <- wasm-admin AND Config.admin for BOTH contracts"
echo "  keeper:             $KEEPER"
echo "  forwarder:          $FORWARDER"
echo "  choice_factory:     $CHOICE_FACTORY"
echo "  clmm_factory:       $CLMM_FACTORY"
echo "  clmm_manager:       $CLMM_MANAGER"
echo "  subdenom_prefix:    $SUBDENOM_PREFIX"
echo " "

for w in "$ISSUER_WASM" "$SEEDER_WASM"; do
    if [ ! -f "$w" ]; then
        echo "ERROR: missing $w — build with './build_release.sh' first" >&2
        exit 1
    fi
done

# ---------------------------------------------------------------------
# 1. Store both wasm artifacts.
# ---------------------------------------------------------------------
echo " "
echo "----- 1/4 Store choice_mts_issuer ----"
ISSUER_CODE_ID=$(store_contract "$ISSUER_WASM")
echo "  > issuer code-id: $ISSUER_CODE_ID"

echo " "
echo "----- 2/4 Store choice_pool_seeder ----"
SEEDER_CODE_ID=$(store_contract "$SEEDER_WASM")
echo "  > seeder code-id: $SEEDER_CODE_ID"

# ---------------------------------------------------------------------
# 2. Instantiate choice_mts_issuer — wasm-admin = ADMIN (timelock).
# ---------------------------------------------------------------------
echo " "
echo "----- 3/4 Instantiate choice_mts_issuer ----"
ISSUER_INIT=$(cat <<EOF
{
  "admin": "$ADMIN",
  "subdenom_prefix": "$SUBDENOM_PREFIX",
  "decimals": $DECIMALS,
  "keeper": "$KEEPER",
  "forwarder": "$FORWARDER",
  "refund_deadline_seconds": $REFUND_DEADLINE_SECONDS
}
EOF
)
ISSUER_ADDR=$(instantiate_contract "$ISSUER_CODE_ID" "$ISSUER_INIT" "choice_mts_issuer-shroom-mainnet" "$ADMIN")
echo "  > issuer addr: $ISSUER_ADDR"

# ---------------------------------------------------------------------
# 3. Instantiate choice_pool_seeder (Factory) — wasm-admin = ADMIN (timelock).
# ---------------------------------------------------------------------
echo " "
echo "----- 4/4 Instantiate choice_pool_seeder (Factory) ----"
SEEDER_INIT=$(cat <<EOF
{
  "factory": {
    "admin": "$ADMIN",
    "sink_code_id": $SEEDER_CODE_ID,
    "choice_factory": "$CHOICE_FACTORY",
    "clmm_factory": "$CLMM_FACTORY",
    "clmm_manager": "$CLMM_MANAGER"
  }
}
EOF
)
SEEDER_ADDR=$(instantiate_contract "$SEEDER_CODE_ID" "$SEEDER_INIT" "choice_pool_seeder-shroom-mainnet" "$ADMIN")
echo "  > seeder factory addr: $SEEDER_ADDR"

# ---------------------------------------------------------------------
# Write results.
# ---------------------------------------------------------------------
mkdir -p "$DEPLOY_DIR/results"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_PATH="$DEPLOY_DIR/results/phase3_mainnet_${STAMP}.json"
cat > "$RESULTS_PATH" <<EOF
{
  "network": "$NETWORK",
  "chain_id": "$CHAIN_ID",
  "deployed_at": "$STAMP",
  "signer": "$SIGNER_ADDRESS",
  "code_ids": {
    "choice_mts_issuer": "$ISSUER_CODE_ID",
    "choice_pool_seeder": "$SEEDER_CODE_ID"
  },
  "addresses": {
    "issuer": "$ISSUER_ADDR",
    "seeder_factory": "$SEEDER_ADDR",
    "choice_factory": "$CHOICE_FACTORY",
    "clmm_factory": "$CLMM_FACTORY",
    "clmm_manager": "$CLMM_MANAGER"
  },
  "roles": {
    "admin_timelock": "$ADMIN",
    "wasm_admin": "$ADMIN",
    "keeper": "$KEEPER",
    "forwarder": "$FORWARDER"
  },
  "config": {
    "subdenom_prefix": "$SUBDENOM_PREFIX",
    "decimals": $DECIMALS,
    "refund_deadline_seconds": $REFUND_DEADLINE_SECONDS
  }
}
EOF

echo " "
echo "================================================="
echo "  Phase-3 MAINNET CW deploy complete"
echo "================================================="
echo "  issuer addr:           $ISSUER_ADDR"
echo "  seeder factory addr:   $SEEDER_ADDR"
echo "  admin (timelock):      $ADMIN  (wasm-admin + Config.admin on BOTH)"
echo " "
echo "  Results: $RESULTS_PATH"
echo " "
echo "  Post-deploy verification (run these):"
echo "    # Both must report admin = the timelock and a non-empty wasm admin:"
echo "    injectived query wasm contract $ISSUER_ADDR --node $NODE | grep admin"
echo "    injectived query wasm contract $SEEDER_ADDR --node $NODE | grep admin"
echo "    injectived query wasm contract-state smart $ISSUER_ADDR '{\"config\":{}}' --node $NODE"
echo "    injectived query wasm contract-state smart $SEEDER_ADDR '{\"factory_config\":{}}' --node $NODE"
echo " "
echo "  EVM side (separate): set LaunchpadCore admin to the multisig/timelock —"
echo "    transferAdmin(multisig) then acceptAdmin(); confirm admin() == multisig."

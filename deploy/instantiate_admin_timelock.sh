#!/bin/bash
set -euo pipefail

# Instantiate the choice_admin_timelock contract.
#
# Required env var (or positional arg 1):
#   TIMELOCK_CODE_ID   code id of choice_admin_timelock.wasm
#
# Optional env vars (with defaults):
#   NETWORK            testnet | mainnet (default: testnet)
#   OWNER              owner of the timelock — controls Propose / Cancel /
#                      ProposeNewOwner. On mainnet, override to the
#                      governance multisig. Default: $SIGNER_ADDRESS.
#   TIMELOCK_SECONDS   delay between propose and apply for migrations and
#                      owner rotations. Default: 172800 (48 h). The contract
#                      enforces a 3600 (1 h) floor; this script bails early
#                      if you go below it.
#   LABEL              wasm contract label (default: "Choice Admin Timelock v1.1.2")
#   ADMIN              wasm admin for THIS contract. Default: $SIGNER_ADDRESS
#                      to keep the deploy mutable while ops shake it out.
#                      Rotate to clear-admin or a separate higher-tier
#                      multisig once the deploy is stable.
#
# Usage:
#   NETWORK=testnet TIMELOCK_CODE_ID=125 ./deploy/instantiate_admin_timelock.sh
#   ./deploy/instantiate_admin_timelock.sh 125          # positional, testnet

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TIMELOCK_CODE_ID="${TIMELOCK_CODE_ID:-${1:-}}"
OWNER="${OWNER:-$SIGNER_ADDRESS}"
TIMELOCK_SECONDS="${TIMELOCK_SECONDS:-172800}"
LABEL="${LABEL:-Choice Admin Timelock v1.1.2}"
ADMIN="${ADMIN:-$SIGNER_ADDRESS}"

if [ -z "$TIMELOCK_CODE_ID" ]; then
    echo "ERROR: TIMELOCK_CODE_ID is required."
    echo "       Pass via env var or positional arg 1."
    echo "       Run ./deploy/upload_admin_timelock.sh first to get it."
    exit 1
fi
if ! [[ "$TIMELOCK_CODE_ID" =~ ^[0-9]+$ ]]; then
    echo "ERROR: TIMELOCK_CODE_ID '$TIMELOCK_CODE_ID' is not an integer."; exit 1
fi
if ! [[ "$TIMELOCK_SECONDS" =~ ^[0-9]+$ ]]; then
    echo "ERROR: TIMELOCK_SECONDS '$TIMELOCK_SECONDS' is not an integer."; exit 1
fi
if [ "$TIMELOCK_SECONDS" -lt 3600 ]; then
    echo "ERROR: TIMELOCK_SECONDS=$TIMELOCK_SECONDS is below the contract floor (3600 = 1 h)."
    echo "       The contract's MIN_TIMELOCK_SECONDS would reject this at instantiate."
    exit 1
fi

banner "⏳ INSTANTIATE CHOICE ADMIN TIMELOCK"
echo "  Timelock code id: $TIMELOCK_CODE_ID"
echo "  Owner:            $OWNER"
echo "  Timelock seconds: $TIMELOCK_SECONDS"
echo "  Wasm admin:       $ADMIN"
echo " "

# timelock_seconds is u64 → JSON number, not string.
INIT_MSG=$(cat <<EOF
{
  "owner": "$OWNER",
  "timelock_seconds": $TIMELOCK_SECONDS
}
EOF
)

echo "-------------------------------------------------"
echo "  InstantiateMsg:"
echo "-------------------------------------------------"
echo "$INIT_MSG"
echo " "

echo "-------------------------------------------------"
echo "  Broadcasting instantiate..."
echo "-------------------------------------------------"
TIMELOCK_ADDR=$(instantiate_contract "$TIMELOCK_CODE_ID" "$INIT_MSG" "$LABEL" "$ADMIN")

echo " "
echo "================================================="
echo "  ✅ TIMELOCK INSTANTIATED"
echo "================================================="
echo "  Address:          $TIMELOCK_ADDR"
echo "  Owner:            $OWNER"
echo "  Timelock seconds: $TIMELOCK_SECONDS"
echo "  Wasm admin:       $ADMIN"
echo " "

# Machine-readable marker.
echo "DEPLOY_CAPTURE_TIMELOCK_ADDR=$TIMELOCK_ADDR"
echo " "

echo "  Next: instantiate the farm factory with the timelock as BOTH"
echo "  FARM_OWNER (config) and ADMIN (factory's own wasm admin)."
echo "  The factory's CreateFarm refuses to spawn unless those match."
echo " "
echo "      NETWORK=$NETWORK \\"
echo "      FARM_CODE_ID=<id> \\"
echo "      FACTORY_CODE_ID=<id> \\"
echo "      FARM_OWNER=$TIMELOCK_ADDR \\"
echo "      ADMIN=$TIMELOCK_ADDR \\"
echo "      ./deploy/instantiate_farm_factory.sh"
echo " "

#!/bin/bash
set -euo pipefail

# Instantiate the choice_farm_factory contract.
#
# Required env vars (or positional args, in this order):
#   FARM_CODE_ID       code id of choice_farm.wasm
#   FACTORY_CODE_ID    code id of choice_farm_factory.wasm
#
# Optional env vars (with defaults):
#   NETWORK            testnet | mainnet (default: testnet)
#   OWNER              factory admin/owner — should be a timelocked multisig
#                      in production (default: $SIGNER_ADDRESS).
#   FEE_COLLECTOR      address that receives every farm's launch fee
#                      (default: $OWNER). Override to Choice's treasury on
#                      mainnet.
#   FARM_OWNER         address installed as Config.owner AND wasm admin on
#                      every spawned farm. SHOULD be the Choice governance
#                      multisig — users trust that no individual launch
#                      operator can change a farm's schedule or drain
#                      undistributed rewards (default: $OWNER).
#   FEE_INJ_BASE       launch fee in inj base units (default: 1 INJ = 1e18).
#                      Must match VITE_FARM_CREATION_FEE_INJ_BASE on the FE
#                      or users see a stale preflight number.
#   LABEL              wasm contract label (default: "Choice Farm Factory v1.1.2")
#   ADMIN              wasm admin for the factory contract (default:
#                      $SIGNER_ADDRESS). On mainnet this MUST be set to the
#                      same choice_admin_timelock address as FARM_OWNER —
#                      execute_create_farm queries the factory's own
#                      ContractInfo and refuses to spawn farms when
#                      admin != farm_owner.
#
# Usage:
#   NETWORK=testnet FARM_CODE_ID=123 FACTORY_CODE_ID=124 ./deploy/instantiate_farm_factory.sh
#   ./deploy/instantiate_farm_factory.sh 123 124        # positional, defaults to testnet

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

FARM_CODE_ID="${FARM_CODE_ID:-${1:-}}"
FACTORY_CODE_ID="${FACTORY_CODE_ID:-${2:-}}"
OWNER="${OWNER:-$SIGNER_ADDRESS}"
FEE_COLLECTOR="${FEE_COLLECTOR:-$OWNER}"
FARM_OWNER="${FARM_OWNER:-$OWNER}"
FEE_INJ_BASE="${FEE_INJ_BASE:-1000000000000000000}"
LABEL="${LABEL:-Choice Farm Factory v1.1.2}"
ADMIN="${ADMIN:-$SIGNER_ADDRESS}"

if [ -z "$FARM_CODE_ID" ] || [ -z "$FACTORY_CODE_ID" ]; then
    echo "ERROR: FARM_CODE_ID and FACTORY_CODE_ID are required."
    echo "       Pass via env vars or positional args 1 and 2."
    echo "       Run ./deploy/upload_farm_code.sh first to get both."
    exit 1
fi
if ! [[ "$FARM_CODE_ID" =~ ^[0-9]+$ ]]; then
    echo "ERROR: FARM_CODE_ID '$FARM_CODE_ID' is not an integer."; exit 1
fi
if ! [[ "$FACTORY_CODE_ID" =~ ^[0-9]+$ ]]; then
    echo "ERROR: FACTORY_CODE_ID '$FACTORY_CODE_ID' is not an integer."; exit 1
fi

banner "🏭 INSTANTIATE CHOICE FARM FACTORY"
echo "  Factory code id: $FACTORY_CODE_ID"
echo "  Farm code id:    $FARM_CODE_ID"
echo "  Owner:           $OWNER"
echo "  Fee collector:   $FEE_COLLECTOR"
echo "  Farm owner:      $FARM_OWNER"
echo "  Wasm admin:      $ADMIN"
echo "  Fee (inj base):  $FEE_INJ_BASE"
echo " "

# C-1: the factory's CreateFarm queries its own ContractInfo and refuses to
# spawn farms unless its wasm admin equals farm_owner. Surface the mismatch
# loudly but don't auto-correct — the operator should choose explicitly.
if [ "$ADMIN" != "$FARM_OWNER" ]; then
    echo "  ⚠️  WARNING: ADMIN ($ADMIN) != FARM_OWNER ($FARM_OWNER)."
    echo "      The factory's CreateFarm will FAIL with 'factory admin mismatch'"
    echo "      until you run MsgUpdateAdmin to align them."
    echo "      Pass ADMIN=\$FARM_OWNER (i.e. the choice_admin_timelock address)"
    echo "      to instantiate with the correct admin from the start."
    echo " "
fi

# instantiate_fee_inj is Uint128 → JSON string. farm_code_id is u64 → number.
INIT_MSG=$(cat <<EOF
{
  "owner": "$OWNER",
  "fee_collector": "$FEE_COLLECTOR",
  "instantiate_fee_inj": "$FEE_INJ_BASE",
  "farm_code_id": $FARM_CODE_ID,
  "farm_owner": "$FARM_OWNER"
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
# Admin defaults to $SIGNER_ADDRESS but in production should be the
# choice_admin_timelock (same as FARM_OWNER) — execute_create_farm queries
# its own ContractInfo and refuses to spawn farms otherwise. Each spawned
# farm uses admin = FARM_OWNER. The factory contract has no special role
# on the farms post-launch — they are owned and migrated by FARM_OWNER.
FACTORY_ADDR=$(instantiate_contract "$FACTORY_CODE_ID" "$INIT_MSG" "$LABEL" "$ADMIN")

echo " "
echo "================================================="
echo "  ✅ FACTORY INSTANTIATED"
echo "================================================="
echo "  Address:         $FACTORY_ADDR"
echo "  Owner:           $OWNER"
echo "  Fee collector:   $FEE_COLLECTOR"
echo "  Farm owner:      $FARM_OWNER"
echo "  Wasm admin:      $ADMIN"
echo "  Fee:             $FEE_INJ_BASE inj"
echo "  Spawning farms with code id: $FARM_CODE_ID"
echo " "
echo "  Set in the frontend .env:"
echo "      VITE_FARM_FACTORY_ADDRESS=\"$FACTORY_ADDR\""
echo "      VITE_FARM_CREATION_FEE_INJ_BASE=\"$FEE_INJ_BASE\""
if [ -n "$FARM_OWNER" ]; then
    echo "      VITE_FARM_ADMIN_TIMELOCK_ADDRESS=\"$FARM_OWNER\""
fi
echo " "

#!/bin/bash
set -euo pipefail

# Upload choice_admin_timelock.wasm. The admin timelock holds wasm-admin
# powers over the farm factory and every spawned farm; deploy it FIRST so
# its address is available to pass as FARM_OWNER + ADMIN when instantiating
# the factory.
#
# Usage:
#   NETWORK=testnet ./deploy/upload_admin_timelock.sh   # default
#   NETWORK=mainnet ./deploy/upload_admin_timelock.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

banner "🚀 UPLOAD ADMIN TIMELOCK CODE"

ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"

echo "-------------------------------------------------"
echo "  Storing choice_admin_timelock.wasm"
echo "-------------------------------------------------"
TIMELOCK_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_admin_timelock.wasm")
echo "  ✅ choice_admin_timelock code id: $TIMELOCK_CODE_ID"
echo " "

echo "================================================="
echo "  ✅ UPLOAD COMPLETE"
echo "================================================="
echo "  choice_admin_timelock  code id: $TIMELOCK_CODE_ID"
echo " "

# Machine-readable marker for Makefile capture.
echo "DEPLOY_CAPTURE_TIMELOCK_CODE_ID=$TIMELOCK_CODE_ID"
echo " "

echo "  Next:"
echo "      NETWORK=$NETWORK \\"
echo "      TIMELOCK_CODE_ID=$TIMELOCK_CODE_ID \\"
echo "      ./deploy/instantiate_admin_timelock.sh"
echo " "

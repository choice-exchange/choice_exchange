#!/bin/bash
set -euo pipefail

# Upload both wasms required for the farm factory flow:
#   1. choice_farm.wasm           (the child contract the factory spawns)
#   2. choice_farm_factory.wasm   (the gateway that collects the fee + registers farms)
#
# Order matters: the factory's InstantiateMsg.farm_code_id references the
# farm code id, so we upload the farm first and print both at the end so the
# operator can plug them into instantiate_farm_factory.sh.
#
# Usage:
#   NETWORK=testnet ./deploy/upload_farm_code.sh   # default
#   NETWORK=mainnet ./deploy/upload_farm_code.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

banner "🚀 UPLOAD FARM + FACTORY CODE"

# Resolve artifact paths relative to the deploy/ dir's parent so the script
# works whether invoked from the repo root or from inside deploy/.
ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"

echo "-------------------------------------------------"
echo "  Storing choice_farm.wasm"
echo "-------------------------------------------------"
FARM_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_farm.wasm")
echo "  ✅ choice_farm code id: $FARM_CODE_ID"
echo " "

echo "-------------------------------------------------"
echo "  Storing choice_farm_factory.wasm"
echo "-------------------------------------------------"
FACTORY_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_farm_factory.wasm")
echo "  ✅ choice_farm_factory code id: $FACTORY_CODE_ID"
echo " "

echo "================================================="
echo "  ✅ UPLOAD COMPLETE"
echo "================================================="
echo "  choice_farm            code id: $FARM_CODE_ID"
echo "  choice_farm_factory    code id: $FACTORY_CODE_ID"
echo " "

# Machine-readable markers for the Makefile `deploy-farm` target to parse.
# Keep these on their own lines and at the very bottom of stdout so the
# parser can grep with a simple anchored regex.
echo "DEPLOY_CAPTURE_FARM_CODE_ID=$FARM_CODE_ID"
echo "DEPLOY_CAPTURE_FACTORY_CODE_ID=$FACTORY_CODE_ID"
echo " "

echo "  Next:"
echo "      NETWORK=$NETWORK \\"
echo "      FARM_CODE_ID=$FARM_CODE_ID \\"
echo "      FACTORY_CODE_ID=$FACTORY_CODE_ID \\"
echo "      ./deploy/instantiate_farm_factory.sh"
echo " "

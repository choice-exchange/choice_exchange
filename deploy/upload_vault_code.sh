#!/bin/bash
set -euo pipefail

# Upload the choice_vault wasm and print the resulting code id.
#
# Usage:
#   NETWORK=testnet ./deploy/upload_vault_code.sh   # default
#   NETWORK=mainnet ./deploy/upload_vault_code.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"

banner "🚀 UPLOAD CHOICE VAULT CODE"

echo "-------------------------------------------------"
echo "  Storing choice_vault.wasm"
echo "-------------------------------------------------"
VAULT_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_vault.wasm")
echo "  ✅ choice_vault code id: $VAULT_CODE_ID"
echo " "

#!/bin/bash
set -euo pipefail

# Upload choice_claim_drops.wasm with InstantiatePermission: Everybody.
#
# The permission is the point, not a detail. choice_claim_drops uses an
# own-instance deployment model: `owner` is per-instance and all state
# (campaigns, liabilities, fee config) is isolated per instance, so anyone who
# wants owner-only streaming campaigns deploys their OWN instance from this
# code id rather than being added to an allowlist on ours. That only works if
# the code is instantiable permissionlessly — hence --instantiate-everybody.
#
# Uploading the code still needs whatever permission the chain grants for
# MsgStoreCode (on Injective mainnet: Choice's upload permission). Instantiate
# is a separate check, which is what we're opening here.
#
# Usage:
#   NETWORK=testnet ./deploy/upload_claim_drops_code.sh   # default
#   NETWORK=mainnet ./deploy/upload_claim_drops_code.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

banner "🚀 UPLOAD CLAIM DROPS CODE"

ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"
WASM="$ARTIFACTS_DIR/choice_claim_drops.wasm"

echo "-------------------------------------------------"
echo "  Storing choice_claim_drops.wasm"
echo "  Instantiate permission: Everybody"
echo "-------------------------------------------------"

STORE_EXTRA_ARGS="--instantiate-everybody=true"
export STORE_EXTRA_ARGS

CLAIM_DROPS_CODE_ID=$(store_contract "$WASM")
echo "  ✅ choice_claim_drops code id: $CLAIM_DROPS_CODE_ID"
echo " "

echo "-------------------------------------------------"
echo "  Verifying instantiate permission on chain"
echo "-------------------------------------------------"
injectived query wasm code-info "$CLAIM_DROPS_CODE_ID" --node="$NODE" --output json \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print("  data_hash:  ", d.get("data_hash")); print("  creator:    ", d.get("creator")); print("  permission: ", json.dumps(d.get("instantiate_permission")))'
echo " "

echo "================================================="
echo "  ✅ UPLOAD COMPLETE"
echo "================================================="
echo "  choice_claim_drops  code id: $CLAIM_DROPS_CODE_ID"
echo " "

# Machine-readable marker for Makefile / script capture.
echo "DEPLOY_CAPTURE_CLAIM_DROPS_CODE_ID=$CLAIM_DROPS_CODE_ID"
echo " "

echo "  Next:"
echo "      NETWORK=$NETWORK \\"
echo "      CLAIM_DROPS_CODE_ID=$CLAIM_DROPS_CODE_ID \\"
echo "      ./deploy/instantiate_claim_drops.sh"
echo " "

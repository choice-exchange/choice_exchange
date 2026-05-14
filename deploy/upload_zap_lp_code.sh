#!/bin/bash
set -euo pipefail

# Upload choice_zap_lp.wasm. The zap contract serves two roles from one
# code id — permissionless user `Zap` and owner-managed royalty
# `ZapBalance`. See contracts/choice_zap_lp/README.md.
#
# Usage:
#   NETWORK=testnet ./deploy/upload_zap_lp_code.sh   # default
#   NETWORK=mainnet ./deploy/upload_zap_lp_code.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

banner "🚀 UPLOAD ZAP LP CODE"

ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"

echo "-------------------------------------------------"
echo "  Storing choice_zap_lp.wasm"
echo "-------------------------------------------------"
ZAP_LP_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_zap_lp.wasm")
echo "  ✅ choice_zap_lp code id: $ZAP_LP_CODE_ID"
echo " "

echo "================================================="
echo "  ✅ UPLOAD COMPLETE"
echo "================================================="
echo "  choice_zap_lp  code id: $ZAP_LP_CODE_ID"
echo " "

# Machine-readable marker for Makefile / CI capture.
echo "DEPLOY_CAPTURE_ZAP_LP_CODE_ID=$ZAP_LP_CODE_ID"
echo " "

echo "  Next:"
echo "      NETWORK=$NETWORK \\"
echo "      ZAP_LP_CODE_ID=$ZAP_LP_CODE_ID \\"
echo "      ./deploy/instantiate_zap_lp.sh"
echo " "
